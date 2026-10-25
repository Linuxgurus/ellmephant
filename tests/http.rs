//! `GET /health` tests — the read-only promise and the writer witness. In-process over
//! `oneshot`, with the deterministic `HashEmbedder`, so no weights and no network.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode, header};
use ellmephant::cli::session::Session;
use ellmephant::config::{Config, Paths};
use ellmephant::embed::HashEmbedder;
use ellmephant::http::{HttpState, LockWatch};
use ellmephant::mcp::{MemoryServer, http_router};
use ellmephant::model::Spec;
use rmcp::model::CallToolRequestParams;
use rmcp::{Peer, RoleClient, ServiceExt};
use tower::ServiceExt as _;

/// An open, offline session backed by the fake embedder (mirrors `tests/mcp.rs`).
fn session(dir: &std::path::Path) -> Session {
    let paths = Paths::from_root(dir);
    paths.ensure().expect("create store root");
    // The config is the WRITE path's view of the vector space — a write has no embedder to ask — so it
    // has to agree with the fake injected below. Disagree, and `encode_hash` describes a different space
    // than the stored vectors do, which makes every later write look like a document change.
    let cfg = Config {
        model: HASH_MODEL.to_string(),
        dim: DIM,
        ..Config::default()
    };
    cfg.validate().expect("valid config");
    let spec = Spec::from_config(&cfg, &paths);
    Session::open(paths, cfg, spec)
        .expect("open store")
        .with_embedder(Box::new(HashEmbedder::new(DIM)))
}

/// A router over a server and a health state that share one witness.
fn router(server: MemoryServer, state: HttpState) -> axum::Router {
    http_router(server, "127.0.0.1", state)
}

/// One `GET`, returning status, body and content-type.
async fn get(app: &axum::Router, uri: &str) -> (StatusCode, String, Option<String>) {
    let response = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .expect("http request");
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_string());
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (
        status,
        String::from_utf8_lossy(&bytes).into_owned(),
        content_type,
    )
}

#[tokio::test]
async fn health_reports_a_ready_store() {
    let dir = tempfile::tempdir().unwrap();
    let watch = Arc::new(LockWatch::new());
    let state = HttpState::new(
        Paths::from_root(dir.path()),
        Config::default(),
        Arc::clone(&watch),
        Instant::now(),
    );
    let server = MemoryServer::with_watch(Arc::new(Mutex::new(session(dir.path()))), watch);

    let (status, body, content_type) = get(&router(server, state), "/health").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        content_type.unwrap().starts_with("text/plain"),
        "the default is prose"
    );
    assert!(body.contains("ready"), "{body}");
    assert!(body.contains("schema      v"), "{body}");
    assert!(body.contains("writer      free"), "{body}");
}

#[tokio::test]
async fn health_json_carries_the_fields() {
    let dir = tempfile::tempdir().unwrap();
    let watch = Arc::new(LockWatch::new());
    let state = HttpState::new(
        Paths::from_root(dir.path()),
        Config::default(),
        Arc::clone(&watch),
        Instant::now(),
    );
    let server = MemoryServer::with_watch(Arc::new(Mutex::new(session(dir.path()))), watch);

    let (status, body, content_type) = get(&router(server, state), "/health?format=json").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(content_type.unwrap().starts_with("application/json"));
    let value: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
    for key in [
        "version",
        "ready",
        "schema",
        "memories",
        "vectors",
        "projection",
        "links",
        "projects",
        "writer",
        "encode_pending",
        "encode_failed",
        "encode_oldest_s",
    ] {
        assert!(value.get(key).is_some(), "missing {key}: {body}");
    }
    assert_eq!(value["ready"], serde_json::Value::Bool(true), "{body}");
}

/// The promise: a probe cannot create or migrate a store — it must leave no file behind.
#[tokio::test]
async fn health_cannot_create_a_store() {
    let server_dir = tempfile::tempdir().unwrap();
    let missing_dir = tempfile::tempdir().unwrap();
    let paths = Paths::from_root(missing_dir.path());
    assert!(!paths.db().exists(), "precondition: no store");

    let state = HttpState::new(
        paths.clone(),
        Config::default(),
        Arc::new(LockWatch::new()),
        Instant::now(),
    );
    let server = MemoryServer::new(session(server_dir.path()));

    let (status, body, _) = get(&router(server, state), "/health").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(body.contains("store missing"), "{body}");
    assert!(
        body.contains(&format!(
            "not in schema v{}",
            ellmephant::store::encode::SCHEMA
        )),
        "and the queue line admits it cannot be read, rather than reporting zeroes: {body}"
    );
    assert!(!paths.db().exists(), "the probe must not create the store");
}

#[tokio::test]
async fn health_witnesses_the_writer() {
    let dir = tempfile::tempdir().unwrap();
    let watch = Arc::new(LockWatch::new());
    let state = HttpState::new(
        Paths::from_root(dir.path()),
        Config::default(),
        Arc::clone(&watch),
        Instant::now(),
    );
    let server = MemoryServer::with_watch(
        Arc::new(Mutex::new(session(dir.path()))),
        Arc::clone(&watch),
    );
    let app = router(server, state);

    let guard = watch.held("memory_reembed");
    let (status, body, _) = get(&app, "/health").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("busy"), "{body}");
    assert!(body.contains("memory_reembed"), "{body}");
    drop(guard);

    let (_, body, _) = get(&app, "/health").await;
    assert!(body.contains("writer      free"), "{body}");
}

/// **The regression this test exists for.** A write schedules its document and returns, so between the
/// write and the worker's pass the store holds N memories and N-1 vectors — the *normal* state of a
/// healthy store. Gating readiness on "every row has a vector" made that a `NOT READY` 503 saying
/// "run `reembed`", i.e. reporting load as a fault, and flapping on every single write.
#[tokio::test]
async fn a_queued_document_is_load_and_not_a_fault() {
    let dir = tempfile::tempdir().unwrap();
    let app = app_with_token(dir.path(), "tok");
    // No worker runs here, so nothing drains it: the queue is pinned at one.
    seed(
        &app,
        "tok",
        "zz-queued",
        "queued",
        "this body was scheduled for embedding, not embedded",
    )
    .await;

    let (status, body, _) = get(&app, "/health?format=json").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a pending document must not 503: {body}"
    );
    let value: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
    assert_eq!(value["ready"], serde_json::Value::Bool(true), "{body}");
    assert_eq!(value["encode_pending"], 1, "{body}");
    assert_eq!(value["encode_failed"], 0, "{body}");
    assert!(
        value["encode_oldest_s"].is_i64(),
        "a waiting document has an age: {body}"
    );

    let (_, text, _) = get(&app, "/health").await;
    assert!(text.contains("1 pending"), "{text}");
    assert!(
        !text.contains("failed"),
        "the queue keeps no failure history any more, so the line must not claim one: {text}"
    );
    assert!(
        !text.contains("without a vector"),
        "the old fault wording is gone, because a queue is not one: {text}"
    );
}

/// And once the worker has caught up, the queue reads empty — with `null` for the age, so "nothing
/// queued" is not confused with "nothing reported". The drain here stands in for the worker; the
/// worker itself is `encode_worker.rs` and `serve_wiring.rs`.
#[tokio::test]
async fn a_drained_queue_reports_empty() {
    let dir = tempfile::tempdir().unwrap();
    let app = app_with_token(dir.path(), "tok");
    seed(
        &app,
        "tok",
        "zz-drained",
        "drained",
        "the worker will come for this",
    )
    .await;

    let stand_in = session(dir.path());
    assert!(
        stand_in.encode_document("zz-drained").expect("embed"),
        "there was a job to do"
    );
    drop(stand_in);

    let (status, body, _) = get(&app, "/health?format=json").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let value: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
    assert_eq!(value["encode_pending"], 0, "{body}");
    assert_eq!(
        value["encode_oldest_s"],
        serde_json::Value::Null,
        "an empty queue has no oldest wait: {body}"
    );
    assert_eq!(value["vectors"], value["memories"], "{body}");
}

/// The other half of readiness: a writer that will not come back. The line is 2 s, so this holds the
/// lock past it and asks. Everything else about the store is perfect — which is exactly the state the
/// incident reported as `ready: true` beside `writer: busy 1239.1s`.
#[tokio::test]
async fn a_wedged_writer_is_not_ready() {
    let dir = tempfile::tempdir().unwrap();
    let watch = Arc::new(LockWatch::new());
    let state = HttpState::new(
        Paths::from_root(dir.path()),
        Config::default(),
        Arc::clone(&watch),
        Instant::now(),
    );
    let server = MemoryServer::with_watch(
        Arc::new(Mutex::new(session(dir.path()))),
        Arc::clone(&watch),
    );
    let app = router(server, state);

    let guard = watch.held("memory_reembed");
    std::thread::sleep(Duration::from_millis(2_100));
    let (status, body, _) = get(&app, "/health").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(body.contains("writer held too long"), "{body}");
    assert!(
        body.contains("memory_reembed"),
        "and it names the holder: {body}"
    );

    drop(guard);
    let (status, body, _) = get(&app, "/health").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "and it recovers with no help: {body}"
    );
}

/// Parity is enforced, not asserted by hand: every MCP tool has exactly one route (HTTP.md slice 2).
#[test]
fn every_tool_has_a_route() {
    use ellmephant::http_surface::ROUTES;
    use ellmephant::mcp::TOOLS;
    assert_eq!(
        ROUTES.len(),
        TOOLS.len(),
        "the route table must have one entry per tool"
    );
    for tool in TOOLS {
        assert!(
            ROUTES.iter().any(|route| route.tool == *tool),
            "no HTTP route maps to `{tool}`"
        );
    }
}

/// A router with a write token configured, so the auth gate is live.
fn app_with_token(dir: &std::path::Path, token: &str) -> axum::Router {
    let watch = Arc::new(LockWatch::new());
    let state = HttpState::new(
        Paths::from_root(dir),
        Config::default(),
        Arc::clone(&watch),
        Instant::now(),
    )
    .with_write_token(Some(token.to_string()));
    let server = MemoryServer::with_watch(Arc::new(Mutex::new(session(dir))), watch);
    http_router(server, "127.0.0.1", state)
}

/// A router with NO write token configured, so the write routes are open (see `resolve`).
fn app_open(dir: &std::path::Path) -> axum::Router {
    let watch = Arc::new(LockWatch::new());
    let state = HttpState::new(
        Paths::from_root(dir),
        Config::default(),
        Arc::clone(&watch),
        Instant::now(),
    );
    let server = MemoryServer::with_watch(Arc::new(Mutex::new(session(dir))), watch);
    http_router(server, "127.0.0.1", state)
}

/// Read a response into `(status, body, content-type)`.
async fn read(response: axum::response::Response) -> (StatusCode, String, Option<String>) {
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_string());
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (
        status,
        String::from_utf8_lossy(&bytes).into_owned(),
        content_type,
    )
}

/// One request with an optional bearer and body.
async fn request(
    app: &axum::Router,
    method: Method,
    uri: &str,
    token: Option<&str>,
    content_type: Option<&str>,
    body: &str,
) -> (StatusCode, String, Option<String>) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    if let Some(content_type) = content_type {
        builder = builder.header(header::CONTENT_TYPE, content_type);
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::from(body.to_string())).unwrap())
        .await
        .expect("http request");
    read(response).await
}

/// An in-process MCP peer over the same session — the other door, for parity. The service must be
/// kept alive by the caller (dropping it closes the pipe).
async fn mcp_peer(server: MemoryServer) -> rmcp::service::RunningService<RoleClient, ()> {
    mcp_peer_as(server, "tester").await
}

/// The same peer, declaring a DIFFERENT pen. Needed whenever a test compares the two doors for the
/// same identity: `mcp_peer` hard-codes `tester`, which silently made an MCP call look ungated.
async fn mcp_peer_as(
    server: MemoryServer,
    pen: &str,
) -> rmcp::service::RunningService<RoleClient, ()> {
    // This peer is IN-PROCESS: there is no HTTP request, so no headers, so no proven identity.
    // A transport with no request declares its pen once, at construction — which is exactly what
    // `with_identity` is for (anonymous writes are refused by design).
    let server = server.with_identity(pen);
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        server
            .serve(server_io)
            .await
            .expect("serve")
            .waiting()
            .await
            .expect("wait");
    });
    ().serve(client_io).await.expect("client handshake")
}

/// Call one MCP tool and return its text result.
async fn tool(peer: &Peer<RoleClient>, name: &'static str, args: serde_json::Value) -> String {
    let result = peer
        .call_tool(
            CallToolRequestParams::new(name)
                .with_arguments(args.as_object().expect("object args").clone()),
        )
        .await
        .expect("tool call");
    result
        .content
        .iter()
        .find_map(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap_or_default()
}

/// The embedder every app in this file is built with (`app_with_token`).
const DIM: usize = 64;
/// Its model id, which the config has to echo (see `session`).
const HASH_MODEL: &str = "fake-hash-v1";

/// Settle every queued document, the way the worker would.
///
/// A write only *queues* its embed now (`ENCODE-WORKER.md`) and a test process has no worker, so
/// anything that asserts on search results or vector counts has to settle first. The store is one
/// file, so a second connection to it is how a test drives the queue — the one-writer rule is the
/// *service's*, not the file's.
fn settle(dir: &std::path::Path) {
    ellmephant::index::vec::register_extension();
    let conn = ellmephant::db::open(&Paths::from_root(dir)).expect("open");
    let embedder = HashEmbedder::new(DIM);
    let _spec = ellmephant::embed::VecSpec::of(&embedder);
    while ellmephant::store::encode::drain(&conn, &embedder, 64).expect("drain") > 0 {}
}

/// Seed one memory over HTTP.
async fn seed(app: &axum::Router, token: &str, key: &str, title: &str, body: &str) {
    let front = format!("Key: {key}\nTitle: {title}\n---\n{body}\n");
    let (status, response, _) = request(
        app,
        Method::POST,
        "/memory",
        Some(token),
        Some("text/plain"),
        &front,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "seed failed: {response}");
}

/// [`seed`] with tags, so the memory lands inside a tag's set — which is what the boot's self-set
/// index reads. (The front-matter carries `Tags:` as a comma list.)
async fn seed_tagged(
    app: &axum::Router,
    token: &str,
    key: &str,
    title: &str,
    tags: &str,
    body: &str,
) {
    let front = format!("Key: {key}\nTitle: {title}\nTags: {tags}\n---\n{body}\n");
    let (status, response, _) = request(
        app,
        Method::POST,
        "/memory",
        Some(token),
        Some("text/plain"),
        &front,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "seed failed: {response}");
}

/// A read counts a visit — on **both** doors. Telemetry is the one write a `GET` may make: a counter
/// is not data, so counting does not make a read unsafely repeatable, and `memory_get` has always done
/// it. There is no separate counter route to call: MCP has no such tool, and `project_get` counts
/// nothing on either door.
#[tokio::test]
async fn reading_a_memory_counts_a_visit() {
    let dir = tempfile::tempdir().unwrap();
    let app = app_with_token(dir.path(), "tok");
    seed(&app, "tok", "counted", "one sharp line", "the body").await;
    settle(dir.path());

    let (_, first, _) = get(&app, "/memory/counted?format=json").await;
    let first: serde_json::Value = serde_json::from_str(&first).unwrap();
    assert_eq!(first["visits"], 1, "the first read counts");

    let (_, second, _) = get(&app, "/memory/counted?format=json").await;
    let second: serde_json::Value = serde_json::from_str(&second).unwrap();
    assert_eq!(second["visits"], 2, "and the second");

    // There is no explicit counter to call. A route with no counterpart in the MCP boot would be one
    // an agent could never discover, and `project_get` does not count hydrated bodies on either door.
    let (status, _, _) = request(
        &app,
        Method::POST,
        "/memory/counted/visit?format=json",
        Some("tok"),
        None,
        "",
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "the split-out counter is gone"
    );

    // And no OTHER read may touch a counter — only the one that returns a body.
    let total = |body: &str| -> i64 {
        serde_json::from_str::<serde_json::Value>(body).unwrap()["visits_total"]
            .as_i64()
            .expect("visits_total")
    };
    let (_, before, _) = get(&app, "/stats?format=json").await;
    for uri in [
        "/memory/counted/backlinks?format=json",
        "/search?q=counted&format=json",
        "/dupes?format=json",
        "/projects?format=json",
    ] {
        let (status, _, _) = get(&app, uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
    }
    let (_, after, _) = get(&app, "/stats?format=json").await;
    assert_eq!(total(&before), total(&after), "no other read counts");
}

/// Auth before writes: with a token configured, every write route refuses a bare credential and a
/// wrong one, and the challenge is a bare `Bearer`.
#[tokio::test]
async fn every_write_route_is_gated() {
    use ellmephant::http_surface::ROUTES;
    let dir = tempfile::tempdir().unwrap();
    let app = app_with_token(dir.path(), "tok");

    let mut checked = 0;
    for route in ROUTES.iter().filter(|r| r.method != "GET") {
        let path = route.path.replace("{key}", "probe");
        let method = Method::from_bytes(route.method.as_bytes()).unwrap();

        let (status, body, _) = request(&app, method.clone(), &path, None, None, "").await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{method} {path} with no token: {body}"
        );

        let (status, _, _) = request(&app, method.clone(), &path, Some("wrong"), None, "").await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{method} {path} wrong token"
        );

        // The challenge is exactly `Bearer` — a `resource_metadata` one would start an OAuth flow.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method.clone())
                    .uri(&path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response
                .headers()
                .get(header::WWW_AUTHENTICATE)
                .map(|v| v.to_str().unwrap()),
            Some("Bearer"),
            "{method} {path} challenge"
        );
        checked += 1;
    }
    assert!(checked >= 6, "expected every write route, saw {checked}");
}

/// The other half of the gate: with NO token configured the write routes are **open**, and
/// `/bootstrap` says so rather than implying a gate that is not there. Deliberate and temporary —
/// see `resolve`.
#[tokio::test]
async fn writes_are_open_when_no_token_is_configured() {
    let dir = tempfile::tempdir().unwrap();
    let app = app_open(dir.path());

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/memory")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"key":"open-note","title":"open","body":"written with no token"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers().get(header::WWW_AUTHENTICATE).is_none(),
        "an open door must not hand out a challenge to invent an auth flow from"
    );

    // A boot is built from code, so it may not vary with the door's state or the store's contents:
    // the same document, whether writes are open or gated.
    let gated = app_with_token(dir.path(), "tok");
    let (_, boot_open, _) = get(&app, "/bootstrap").await;
    let (_, boot_gated, _) = get(&gated, "/bootstrap").await;
    assert_eq!(
        without_queue_age(&boot_open),
        without_queue_age(&boot_gated),
        "a boot must not depend on the auth config"
    );
}

/// The boot's encode-queue readout carries a CLOCK — `1 pending (oldest 12s)`. Two renders a moment
/// apart can report `0s` and `1s` while agreeing about every decision this test asks about, so the
/// age is stripped before the documents are compared. Found the loud way: green locally, red in CI,
/// where the render is slow enough to cross a tick.
fn without_queue_age(boot: &str) -> String {
    const MARK: &str = "(oldest ";
    let mut out = String::with_capacity(boot.len());
    let mut rest = boot;
    while let Some(at) = rest.find(MARK) {
        let (head, tail) = rest.split_at(at + MARK.len());
        out.push_str(head);
        rest = tail.trim_start_matches(|c: char| c.is_ascii_digit());
    }
    out.push_str(rest);
    out
}

/// The same code with a token configured is a gate — so auth stays a config flip, not a rewrite.
#[tokio::test]
async fn a_configured_token_closes_the_door_again() {
    let dir = tempfile::tempdir().unwrap();
    let app = app_with_token(dir.path(), "tok");
    let body = r#"{"key":"x","title":"t","body":"b"}"#;

    let (status, response, _) = request(
        &app,
        Method::POST,
        "/memory",
        None,
        Some("application/json"),
        body,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{response}");

    let (status, response, _) = request(
        &app,
        Method::POST,
        "/memory",
        Some("tok"),
        Some("application/json"),
        body,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
}

/// The text round trip — the reason the format exists: a body with quotes, colons, braces and a
/// line of `---` survives byte-for-byte.
#[tokio::test]
async fn a_prose_body_round_trips_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    let app = app_with_token(dir.path(), "tok");
    let body = "line \"quotes\"\ncolons: here\nbraces {}\n---\nstill body";
    seed(&app, "tok", "zz-probe", "one sharp line", body).await;

    let (_, got, _) = get(&app, "/memory/zz-probe?format=json").await;
    let value: serde_json::Value = serde_json::from_str(&got).unwrap();
    assert_eq!(value["body"], body, "{got}");
}

/// Front-matter strictness: an unknown field is a 400 naming it, and nothing is written.
#[tokio::test]
async fn an_unknown_front_matter_field_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let app = app_with_token(dir.path(), "tok");
    let text = "Key: zz-typo\nTitle: t\nKeyy: oops\n---\nbody\n";
    let (status, body, _) = request(
        &app,
        Method::POST,
        "/memory",
        Some("tok"),
        Some("text/plain"),
        text,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("Keyy"), "the 400 must name the field: {body}");

    let (status, _, _) = get(&app, "/memory/zz-typo").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "nothing must be written");
}

/// Failures: 404 on an unknown key, 405 on a known path with the wrong method, and our own text
/// 404 on an unknown path (never an empty body a gateway could substitute).
#[tokio::test]
async fn failures_are_ours_not_the_gateways() {
    let dir = tempfile::tempdir().unwrap();
    let app = app_with_token(dir.path(), "tok");

    let (status, body, _) = get(&app, "/memory/nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(!body.is_empty(), "our 404 carries a body");

    let (status, body, _) = request(&app, Method::POST, "/health", None, None, "").await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{body}");
    assert!(body.contains("method not allowed"), "{body}");

    let (status, body, content_type) = get(&app, "/not/a/route").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(body.contains("not found"), "{body}");
    assert!(
        content_type.unwrap().starts_with("text/plain"),
        "the fallback is text"
    );
}

/// `/bootstrap` is the door to the store, not a handshake dump: it says what this is, how to fetch a
/// memory, and hands over the resident note in full — all inside the byte budget a fetching client
/// will actually deliver intact.
#[tokio::test]
async fn bootstrap_is_the_door_to_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let app = app_with_token(dir.path(), "tok");
    seed(
        &app,
        "tok",
        "bootstrap",
        "boot me",
        "BOOT: read me first, then go",
    )
    .await;
    // Someone else's identity, and a self tag that is NOT this caller's. An anonymous boot must show
    // neither: the door used to inline `stet-self` for every caller, which is how a cold Cline was
    // handed Stet's identity (2026-10-22).
    let secret = "a body that is NOT resident, only indexed by key";
    seed_tagged(
        &app,
        "tok",
        "other-self",
        "another self key",
        "self:cline",
        secret,
    )
    .await;
    seed(&app, "tok", "bootstrap-stet", "stet boot", "STET ONLY BODY").await;

    let (status, doc, content_type) = get(&app, "/bootstrap").await;
    assert_eq!(status, StatusCode::OK, "{doc}");
    assert!(content_type.unwrap().starts_with("text/plain"));

    // It says what it IS, in the first line, and how to get at what is in it.
    assert!(
        doc.lines().next().unwrap().contains("your memory"),
        "the first line must say what this endpoint is: {doc}"
    );
    assert!(
        doc.contains("HOW TO GET AT IT") && doc.contains("GET /search?q="),
        "the reader is told how to fetch memories: {doc}"
    );

    // The resident note is INLINE — the one body a cold client needs before it can decide anything.
    assert!(
        doc.contains("BOOT: read me first, then go"),
        "the boot note's body must be resident: {doc}"
    );
    assert!(
        doc.contains("memory `bootstrap`"),
        "and named, so it can be fetched again: {doc}"
    );

    // NO SELF SET for an anonymous caller — and no other body smuggled in by any route.
    assert!(
        doc.contains("no self set") && doc.contains("proved no pen"),
        "an unproven caller is told it has no self set rather than being given one: {doc}"
    );
    assert!(
        !doc.contains(secret) && !doc.contains("STET ONLY BODY"),
        "another agent's identity must never be served: {doc}"
    );

    // The catalogue is NOT inline: no argument schemas, and the pointer says how much it would cost.
    assert!(
        !doc.contains("args:") && !doc.contains("inputSchema"),
        "the default boot carries no tool schemas: {doc}"
    );
    assert!(
        doc.contains("?tools=true"),
        "and it says where they are: {doc}"
    );
}

/// ★ THE DOOR MUST NOT NAME A CALLER IT CANNOT NAME (2026-10-22). The boot once inlined `stet-self`
/// for everyone and indexed `cline-self`, so an anonymous caller was handed one agent's first-person
/// identity. The address is built from the PROVEN pen now; with no pen there is no self set, and the
/// legacy keys are not a back door to somebody else's note.
#[tokio::test]
async fn bootstrap_never_serves_another_agents_identity() {
    let dir = tempfile::tempdir().unwrap();
    let app = app_with_token(dir.path(), "tok");
    seed(&app, "tok", "bootstrap", "floor", "FLOOR BODY").await;
    seed(&app, "tok", "bootstrap-stet", "stet boot", "STET BODY").await;
    seed(&app, "tok", "bootstrap-self", "old note", "OLD BODY").await;
    seed_tagged(
        &app,
        "tok",
        "stet-self",
        "stet self",
        "stet-self",
        "STET SELF BODY",
    )
    .await;

    let (status, doc, _) = get(&app, "/bootstrap").await;
    assert_eq!(status, StatusCode::OK, "{doc}");
    assert!(
        doc.contains("FLOOR BODY") && doc.contains("memory `bootstrap`"),
        "an anonymous caller gets the shared floor: {doc}"
    );
    assert!(
        !doc.contains("STET BODY") && !doc.contains("OLD BODY") && !doc.contains("STET SELF BODY"),
        "no other agent's note or self set may be served: {doc}"
    );
}

/// **The boot has to survive being fetched.** A client that truncates on size delivered this endpoint's
/// old body minus 7 KB of its middle — a document that reads as complete and is not. So the default
/// view is held under a budget, and this test is what holds it there: it fails on the day the boot
/// regrows past what a fetcher will hand over, which is a decision, not an accident.
#[tokio::test]
async fn bootstrap_stays_inside_its_budget() {
    let dir = tempfile::tempdir().unwrap();
    let app = app_with_token(dir.path(), "tok");
    // A store shaped like the real one: a ~1.6 KB resident note, a self set of 20 tagged keys with
    // titles long enough to exercise the clip, and open projects. A stress fixture would only prove
    // the budget is smaller than a stress fixture.
    seed(
        &app,
        "tok",
        "bootstrap",
        "boot me",
        &"resident prose. ".repeat(100),
    )
    .await;
    for i in 0..20 {
        seed_tagged(
            &app,
            "tok",
            &format!("self-{i:02}"),
            &"a title that is long enough to be clipped somewhere sensible".repeat(2),
            "self:cline",
            "body",
        )
        .await;
    }
    for i in 0..13 {
        let body = format!(
            r#"{{"key":"prj-{i:02}","title":"project {i}","status":"active","goal":"a goal line long enough to be clipped by the boot, as the real ones are"}}"#
        );
        let (status, response, _) = request(
            &app,
            Method::POST,
            "/project",
            Some("tok"),
            Some("application/json"),
            &body,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "project seed failed: {response}");
    }

    let (_, doc, _) = get(&app, "/bootstrap").await;
    assert!(
        doc.len() < ellmephant::http_surface::BOOT_BUDGET_BYTES,
        "the boot is {} bytes, over the {} it must fit in — trim a section or raise the budget \
         deliberately",
        doc.len(),
        ellmephant::http_surface::BOOT_BUDGET_BYTES
    );
    // The size is not bought by dropping the content the boot exists for.
    assert!(doc.contains("resident prose."), "{doc}");
    assert!(
        doc.contains("no self set"),
        "an unproven caller is told so: {doc}"
    );
}

/// A boot still becomes a boot with no store to read: the API part survives, and the missing facts are
/// NAMED rather than omitted — silence would read as "there is nothing in here".
#[tokio::test]
async fn bootstrap_survives_an_unreadable_store() {
    let server_dir = tempfile::tempdir().unwrap();
    let missing = tempfile::tempdir().unwrap();
    let state = HttpState::new(
        Paths::from_root(missing.path()),
        Config::default(),
        Arc::new(LockWatch::new()),
        Instant::now(),
    );
    let server = MemoryServer::new(session(server_dir.path()));

    let (status, doc, _) = get(&router(server, state), "/bootstrap").await;
    assert_eq!(status, StatusCode::OK, "a boot is still useful: {doc}");
    assert!(doc.contains("STORE UNREADABLE"), "{doc}");
    assert!(
        doc.contains("HOW TO GET AT IT"),
        "the API part does not depend on the store: {doc}"
    );
    assert!(
        !missing.path().join("memory.db").exists(),
        "the boot must not create a store either"
    );
}

/// `/bootstrap?format=json` is the MCP handshake in MCP's own shape: `initialize`'s fields plus
/// `tools/list`'s, with `method`/`path` where MCP carries `name` and every other field untouched.
#[tokio::test]
async fn bootstrap_json_is_the_door_in_json() {
    let dir = tempfile::tempdir().unwrap();
    let app = app_with_token(dir.path(), "tok");
    seed(
        &app,
        "tok",
        "bootstrap",
        "boot me",
        "BOOT: read me first, then go",
    )
    .await;

    let (status, body, _) = get(&app, "/bootstrap?format=json").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let doc: serde_json::Value = serde_json::from_str(&body).unwrap();

    // The MCP names a client already knows how to read, plus the door and its content. `tools` is
    // NOT here by default — that is the whole point of the split.
    let mut keys: Vec<&String> = doc.as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "capabilities",
            "howToUse",
            "instructions",
            "openProjects",
            "selfSet",
            "serverInfo",
            "state",
            "unreadable",
            "youResident",
        ],
        "{doc}"
    );
    assert_eq!(doc["serverInfo"]["name"], serde_json::json!("ellmephant"));
    assert_eq!(
        doc["serverInfo"]["version"],
        serde_json::json!(ellmephant::VERSION)
    );
    assert_eq!(doc["capabilities"], serde_json::json!({ "tools": {} }));
    assert!(
        doc["instructions"]
            .as_str()
            .unwrap()
            .contains("smart-caveman")
    );

    // The calls are stated as calls, so a machine can act without parsing prose.
    let calls: Vec<&str> = doc["howToUse"]["calls"]
        .as_array()
        .expect("calls")
        .iter()
        .map(|c| c["call"].as_str().unwrap())
        .collect();
    assert!(
        calls.iter().any(|c| c.starts_with("GET /search")),
        "{calls:?}"
    );
    assert!(
        calls.iter().any(|c| c.starts_with("GET /memory/")),
        "{calls:?}"
    );
    assert!(
        calls.iter().any(|c| c.starts_with("POST /memory")),
        "{calls:?}"
    );

    // The resident note is carried, body and all; the rest of the set is keys and titles.
    assert_eq!(doc["youResident"]["key"], serde_json::json!("bootstrap"));
    assert_eq!(
        doc["youResident"]["body"],
        serde_json::json!("BOOT: read me first, then go")
    );
    assert!(doc["selfSet"]["keys"].is_array(), "{doc}");
    assert!(
        doc["state"].as_str().unwrap().contains("ready"),
        "the state line rides the JSON too: {doc}"
    );

    // And with `?tools=true` the catalogue is exactly what it used to be.
    let (_, body, _) = get(&app, "/bootstrap?format=json&tools=true").await;
    let doc: serde_json::Value = serde_json::from_str(&body).unwrap();
    let tools = doc["tools"].as_array().expect("tools with ?tools=true");
    assert_eq!(tools.len(), ellmephant::mcp::TOOLS.len());

    // Each card is the MCP card with the verb swapped in: same description, schema, hints.
    for route in ellmephant::http_surface::ROUTES {
        let card = tools
            .iter()
            .find(|card| card["path"] == route.path && card["method"] == route.method)
            .unwrap_or_else(|| panic!("no card for {} {}", route.method, route.path));
        assert!(
            card.get("name").is_none(),
            "the MCP verb is replaced, not carried alongside: {card}"
        );
        assert!(
            card.get("title").is_none(),
            "MCP omits an absent title rather than sending \"\": {card}"
        );
        assert!(card["description"].as_str().unwrap().len() > 40, "{card}");
        assert!(card["inputSchema"].is_object(), "{card}");
        assert!(card["annotations"].is_object(), "{card}");
    }

    // The schema is MCP's, verbatim.
    let search = tools.iter().find(|c| c["path"] == "/search").unwrap();
    assert!(search["inputSchema"]["properties"]["query"].is_object());
    assert_eq!(
        search["annotations"]["readOnlyHint"],
        serde_json::json!(true)
    );
    assert_eq!(
        search["annotations"]["title"],
        serde_json::json!("Search memory"),
        "the human title rides `annotations`, as it does over MCP"
    );
}

/// The text form renders each argument from the tool's own schema, so a caller can construct the
/// call from the catalogue alone: a nullable property reports the type it carries, an enum reports
/// its values, and a required argument says so. (With `?tools=true` — the boot itself carries no
/// schemas.)
#[tokio::test]
async fn boot_text_renders_arguments_from_the_schema() {
    let dir = tempfile::tempdir().unwrap();
    let app = app_with_token(dir.path(), "tok");
    let (status, doc, _) = get(&app, "/bootstrap?tools=true").await;
    assert_eq!(status, StatusCode::OK, "{doc}");

    // `k` is `["integer", "null"]` — an optional integer, not "any".
    assert!(doc.contains("k: integer (optional)"), "{doc}");
    // A `$ref` to a string enum shows the values a caller may send.
    assert!(
        doc.contains("mode: \"literal\"|\"regex\" (optional)"),
        "{doc}"
    );
    // An array of an enum is parenthesised, so the `[]` cannot be misread.
    assert!(
        doc.contains("(\"key\"|\"title\"|\"tags\"|\"chars\"|\"distance\")[]"),
        "{doc}"
    );
    // A required argument says so; a plain boolean resolves without a ref.
    assert!(doc.contains("key: string (required)"), "{doc}");
    assert!(doc.contains("tags: boolean (optional)"), "{doc}");
}

/// `/bootstrap` is read-only: it changes no counter and no count. (It cannot: the document is built
/// from code and reads no database. The counters are compared through `/stats`, because reading a
/// memory would itself count — the very thing this asserts.)
#[tokio::test]
async fn bootstrap_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let app = app_with_token(dir.path(), "tok");
    seed(&app, "tok", "still", "unchanged", "no visits here").await;

    let digest = |body: &str| -> serde_json::Value {
        let doc: serde_json::Value = serde_json::from_str(body).unwrap();
        serde_json::json!({
            "memories": doc["memories"],
            "visits_total": doc["visits_total"],
            "useful_total": doc["useful_total"],
        })
    };
    let (_, before, _) = get(&app, "/stats?format=json").await;
    let (status, _, _) = get(&app, "/bootstrap").await;
    assert_eq!(status, StatusCode::OK);
    let (_, after, _) = get(&app, "/stats?format=json").await;
    assert_eq!(
        digest(&before),
        digest(&after),
        "/bootstrap changed the store"
    );
}

/// One core, two doors: for every read route the JSON body is byte-identical to the MCP tool's
/// output for the same call (`to_wire_json`/`json_ok` reused, not reimplemented).
#[tokio::test]
async fn the_two_doors_are_one_core() {
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let watch = Arc::new(LockWatch::new());
    let shared = Arc::new(Mutex::new(session(dir.path())));
    let mcp = MemoryServer::with_watch(Arc::clone(&shared), Arc::clone(&watch));
    let state = HttpState::new(
        Paths::from_root(dir.path()),
        Config::default(),
        Arc::clone(&watch),
        Instant::now(),
    )
    .with_write_token(Some("tok".into()));
    let app = http_router(
        MemoryServer::with_watch(Arc::clone(&shared), Arc::clone(&watch)),
        "127.0.0.1",
        state,
    );
    let service = mcp_peer_as(mcp, "http-as-gated").await;

    let seeded = tool(
        service.peer(),
        "memory_upsert",
        json!({ "key": "alpha", "title": "Alpha", "body": "alpha body links [[beta]]", "tags": ["t"] }),
    )
    .await;
    assert!(seeded.contains("\"status\""), "{seeded}");
    tool(
        service.peer(),
        "memory_upsert",
        json!({ "key": "beta", "title": "Beta", "body": "a different body", "tags": ["t"] }),
    )
    .await;
    tool(
        service.peer(),
        "project_upsert",
        json!({ "key": "proj", "title": "Project P", "status": "active", "tags": ["work"] }),
    )
    .await;

    // The two writes queued their embeds; settle them so the semantic read below has an index to
    // read, and the MCP and HTTP answers are comparable.
    settle(dir.path());

    let cases: [(&str, &str, serde_json::Value); 6] = [
        ("/stats?format=json", "memory_stats", json!({})),
        (
            "/memory/alpha/backlinks?format=json",
            "memory_backlinks",
            json!({ "key": "alpha" }),
        ),
        (
            "/search?q=alpha&k=5&fields=key,title&format=json",
            "memory_search",
            json!({ "query": "alpha", "k": 5, "fields": ["key", "title"] }),
        ),
        ("/dupes?format=json", "memory_dupes", json!({})),
        ("/projects?format=json", "project_list", json!({})),
        (
            "/project/proj?format=json",
            "project_get",
            json!({ "key": "proj" }),
        ),
    ];
    for (uri, name, args) in cases {
        let (status, http_body, _) = get(&app, uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {http_body}");
        let mcp_body = tool(service.peer(), name, args).await;
        assert_eq!(http_body, mcp_body, "{uri} diverged from {name}");
    }

    // `memory_get` is the same operation on both doors, telemetry included: each call advances
    // `visits` by one, and the payloads agree on every field but the counter (two successive reads
    // must differ there — that is the point of a counter).
    let (_, http_body, _) = get(&app, "/memory/alpha?format=json").await;
    let http_get: serde_json::Value = serde_json::from_str(&http_body).unwrap();
    let mcp_body = tool(service.peer(), "memory_get", json!({ "key": "alpha" })).await;
    let mcp_get: serde_json::Value = serde_json::from_str(&mcp_body).unwrap();

    let http_visits = http_get["visits"]
        .as_i64()
        .expect("visits in the http payload");
    let mcp_visits = mcp_get["visits"]
        .as_i64()
        .expect("visits in the mcp payload");
    assert_eq!(
        mcp_visits,
        http_visits + 1,
        "both doors count a visit, in turn: {http_body} vs {mcp_body}"
    );

    let mut http_rest = http_get.clone();
    let mut mcp_rest = mcp_get.clone();
    if let Some(obj) = http_rest.as_object_mut() {
        obj.remove("visits");
    }
    if let Some(obj) = mcp_rest.as_object_mut() {
        obj.remove("visits");
    }

    // ★ ONE FIELD IS ABOUT THE ASKER, NOT THE ROW (2026-10-23). `owner_relation` is the walk from the
    // reader's proven pen to the row's owner, and in THIS test the two doors are different pens: the
    // MCP peer declares `tester`, the HTTP call proves nobody. So it is asserted separately — present
    // and `ME` where the pen asks about its own row, absent where nobody asked as anyone — and then
    // dropped, because "the same core answers the same" is a claim about the ROW and it still holds.
    assert_eq!(
        mcp_rest.get("owner_relation").and_then(|v| v.as_str()),
        Some("ME"),
        "the door that proved a pen gets the walk: {mcp_body}"
    );
    assert!(
        http_rest.get("owner_relation").is_none(),
        "a door with no proven pen gets no walk, not a guess: {http_body}"
    );
    if let Some(obj) = http_rest.as_object_mut() {
        obj.remove("owner_relation");
    }
    if let Some(obj) = mcp_rest.as_object_mut() {
        obj.remove("owner_relation");
    }
    assert_eq!(
        http_rest, mcp_rest,
        "every field but the counter and the reader-relative walk must match"
    );
}

/// The MCP tool's OWN arguments, sent as a request body, must work on the write routes that take an
/// argument: a caller that knows `memory_merge` should not have to learn a second spelling for the
/// same thing. The query forms stay, for a caller that only has a URL.
#[tokio::test]
async fn write_routes_accept_the_mcp_arguments_as_a_body() {
    let dir = tempfile::tempdir().unwrap();
    let app = app_with_token(dir.path(), "tok");
    seed(&app, "tok", "mb", "a body", "text").await;

    // `memory_merge` takes {from, into}. The target already carries the source's prose here, so there
    // is nothing to integrate; had it not, the merge would append it rather than refuse.
    seed(
        &app,
        "tok",
        "mb-into",
        "target",
        "the target's own body, now carrying: text",
    )
    .await;
    let (status, body, _) = request(
        &app,
        Method::POST,
        "/memory/mb/merge?format=json",
        Some("tok"),
        Some("application/json"),
        r#"{"from":"mb","into":"mb-into"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _, _) = get(&app, "/memory/mb?format=json").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "the source is deleted");

    // ...and a `from` that disagrees with the path, likewise.
    seed(&app, "tok", "mb2", "another", "x").await;
    let (status, body, _) = request(
        &app,
        Method::POST,
        "/memory/mb2/merge?format=json",
        Some("tok"),
        Some("application/json"),
        r#"{"from":"mb","into":"mb-into"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

/// `memory_upvote` takes `{"keys": [...]}` — a BATCH — so the route must honour that body rather than
/// silently upvoting the key in its own path, which is what it used to do. And the reply is the tool's
/// reply, because both doors call one `upvote_sweep`.
#[tokio::test]
async fn upvote_route_honours_the_mcp_batch_body() {
    let dir = tempfile::tempdir().unwrap();
    let app = app_with_token(dir.path(), "tok");
    seed(&app, "tok", "uv-a", "a", "a").await;
    seed(&app, "tok", "uv-b", "b", "b").await;

    let times_useful = |body: &str| -> serde_json::Value {
        serde_json::from_str::<serde_json::Value>(body).unwrap()["times_useful"].clone()
    };

    // A body naming OTHER keys: the path key must be left alone.
    let (status, body, _) = request(
        &app,
        Method::POST,
        "/memory/uv-a/upvote?format=json",
        Some("tok"),
        Some("application/json"),
        r#"{"keys":["uv-b"]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let doc: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(doc["upvoted"][0]["key"], "uv-b", "the body wins: {body}");
    let (_, a, _) = get(&app, "/memory/uv-a?format=json").await;
    assert_eq!(
        times_useful(&a),
        serde_json::json!(0),
        "the path key is untouched"
    );
    let (_, b, _) = get(&app, "/memory/uv-b?format=json").await;
    assert_eq!(times_useful(&b), serde_json::json!(1), "{b}");

    // ★ A SINGLE KEY AS A BARE STRING: the same request as `{"keys":["uv-b"]}`, and the same reply.
    let (status, body, _) = request(
        &app,
        Method::POST,
        "/memory/uv-a/upvote?format=json",
        Some("tok"),
        Some("application/json"),
        r#"{"keys":"uv-b"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let doc: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        doc["upvoted"][0]["key"], "uv-b",
        "a string key works: {body}"
    );

    // A batch reports the keys it could NOT find, exactly as the tool does.
    let (status, body, _) = request(
        &app,
        Method::POST,
        "/memory/uv-a/upvote?format=json",
        Some("tok"),
        Some("application/json"),
        r#"{"keys":["uv-a","nope"]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let doc: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(doc["upvoted"][0]["key"], "uv-a", "{body}");
    assert_eq!(doc["missing"][0], "nope", "{body}");

    // No body: the path key is the one-key shorthand (unchanged behaviour).
    let (status, body, _) = request(
        &app,
        Method::POST,
        "/memory/uv-b/upvote?format=json",
        Some("tok"),
        None,
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // A sweep that hits nothing is a 404, as on the tool.
    let (status, body, _) = request(
        &app,
        Method::POST,
        "/memory/uv-a/upvote?format=json",
        Some("tok"),
        Some("application/json"),
        r#"{"keys":["nope"]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

/// A store with rows but no vectors must SAY so rather than answer an empty list: a caller reads
/// `hits: []` as "nothing matched" and stops asking, and that is the one answer that is a lie. This
/// is the state an interrupted `reembed` leaves behind.
#[tokio::test]
async fn search_says_so_when_the_index_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    let app = app_with_token(dir.path(), "tok");
    seed(
        &app,
        "tok",
        "idx-a",
        "a memory",
        "a body with words worth finding",
    )
    .await;
    settle(dir.path());

    // The truth first: with vectors, the phrase finds it.
    let (status, body, _) = get(&app, "/search?q=words%20worth%20finding&format=json").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("idx-a"), "{body}");

    // Now strip every vector, as a killed rebuild would.
    {
        ellmephant::index::vec::register_extension();
        let conn = rusqlite::Connection::open(Paths::from_root(dir.path()).db()).unwrap();
        ellmephant::index::vec::clear(&conn).unwrap();
    }

    let (status, body, _) = get(&app, "/search?q=words%20worth%20finding&format=json").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(body.contains("index unavailable"), "{body}");

    // A store with no rows at all is empty, not broken: that search is still a plain 200.
    let empty = tempfile::tempdir().unwrap();
    let app = app_with_token(empty.path(), "tok");
    let (status, body, _) = get(&app, "/search?q=anything&format=json").await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// `backlinks` on a key with no row is a 404, not an empty list: *no backlinks* and *no such memory*
/// are different answers, and it is the second that decides whether a delete is safe.
#[tokio::test]
async fn backlinks_of_a_missing_memory_is_a_404() {
    let dir = tempfile::tempdir().unwrap();
    let app = app_with_token(dir.path(), "tok");
    let (status, body, _) = get(&app, "/memory/no-such-memory/backlinks?format=json").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    seed(&app, "tok", "bl-target", "a target", "a body").await;
    let (status, body, _) = get(&app, "/memory/bl-target/backlinks?format=json").await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// A body past the store's cap is refused with 413, rather than accepted and then ground through the
/// encoder for minutes while the whole session waits behind it.
#[tokio::test]
async fn a_body_past_the_cap_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let app = app_with_token(dir.path(), "tok");
    let cap = ellmephant::store::MAX_BODY_CHARS;

    let too_big =
        serde_json::json!({"key": "too-big", "title": "too big", "body": "x".repeat(cap + 1)})
            .to_string();
    let (status, body, _) = request(
        &app,
        Method::POST,
        "/memory?format=json",
        Some("tok"),
        Some("application/json"),
        &too_big,
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert!(body.contains("body too long"), "{body}");
    let (status, _, _) = get(&app, "/memory/too-big?format=json").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "nothing was stored");

    // Exactly at the cap is allowed: a limit, not an off-by-one.
    let at_cap = serde_json::json!({"key": "at-cap", "title": "at cap", "body": "y".repeat(cap)})
        .to_string();
    let (status, body, _) = request(
        &app,
        Method::POST,
        "/memory?format=json",
        Some("tok"),
        Some("application/json"),
        &at_cap,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// A merge no longer needs the caller to fold first: retiring a memory whose fact the target lacks
/// APPENDS that fact, verbatim, and the target keeps it. The old gate made the one operation called
/// "tidying up" the one that could quietly lose the detail — or refuse and stall the workflow.
#[tokio::test]
async fn a_merge_integrates_the_fact_instead_of_losing_or_refusing_it() {
    let dir = tempfile::tempdir().unwrap();
    let app = app_with_token(dir.path(), "tok");
    seed(
        &app,
        "tok",
        "dup-into",
        "the survivor",
        "only the survivor's words",
    )
    .await;
    seed(
        &app,
        "tok",
        "dup-from",
        "the duplicate",
        "a fact only the duplicate carries",
    )
    .await;

    let (status, body, _) = request(
        &app,
        Method::POST,
        "/memory/dup-from/merge?format=json",
        Some("tok"),
        Some("application/json"),
        r#"{"from":"dup-from","into":"dup-into"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.contains("a fact only the duplicate carries"),
        "the integrated line is reported, not just a link count: {body}"
    );

    // The source is deleted, and its fact SURVIVED in the target: not lost, not paraphrased.
    let (status, _, _) = get(&app, "/memory/dup-from?format=json").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "the source is deleted");
    let (status, body, _) = get(&app, "/memory/dup-into?format=json").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.contains("a fact only the duplicate carries"),
        "the fact was carried into the survivor: {body}"
    );
    assert!(
        body.contains("only the survivor's words"),
        "and the survivor's own words are untouched: {body}"
    );
}

/// `?wait=true` settles the document before answering, so the row is searchable the moment the write
/// returns — the one path that deliberately holds the store lock across the model call.
#[tokio::test]
async fn wait_true_indexes_before_it_answers() {
    let dir = tempfile::tempdir().unwrap();
    let app = app_with_token(dir.path(), "tok");
    let front = "Key: waited\nTitle: Waited\n---\na body with words worth finding\n";

    let (status, body, _) = request(
        &app,
        Method::POST,
        "/memory?wait=true&format=json",
        Some("tok"),
        Some("text/plain"),
        front,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.contains("\"indexed\":true"),
        "the write should report it indexed: {body}"
    );

    // Deliberately NO `settle(dir.path())` here: the request drained the queue itself, and that is the
    // whole promise of `?wait=true`.
    let (status, body, _) = get(&app, "/search?q=words%20worth%20finding&format=json").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("waited"), "{body}");
}

/// `?wait=true` when the encoder **cannot** embed: the write still stands, and the reply says so rather
/// than claiming a guarantee that was missed. This is the path a vanished weights cache would take.
#[tokio::test]
async fn wait_true_reports_a_write_that_could_not_be_indexed() {
    /// An encoder that always fails.
    struct Broken;

    impl ellmephant::embed::Embedder for Broken {
        fn dim(&self) -> usize {
            64
        }
        fn model_id(&self) -> &str {
            "broken"
        }
        fn embed(&self, _texts: &[String]) -> ellmephant::error::Result<Vec<Vec<f32>>> {
            Err(ellmephant::error::Error::Config("no vector for you".into()))
        }
    }

    // Built WITH the failing encoder: `Encoder::inject` sets a `OnceLock`, so it cannot be replaced
    // after the fact — a test that tried would silently keep the working one.
    let dir = tempfile::tempdir().unwrap();
    let watch = Arc::new(LockWatch::new());
    let paths = Paths::from_root(dir.path());
    let cfg = Config::default();
    let spec = Spec::from_config(&cfg, &paths);
    let session = Session::open(paths.clone(), cfg.clone(), spec)
        .expect("open store")
        .with_embedder(Box::new(Broken));
    let state = HttpState::new(paths, cfg, Arc::clone(&watch), Instant::now())
        .with_write_token(Some("tok".into()));
    let app = http_router(
        MemoryServer::with_watch(Arc::new(Mutex::new(session)), watch),
        "127.0.0.1",
        state,
    );

    let front = "Key: unindexable\nTitle: Unindexable\n---\na body the encoder will refuse\n";
    let (status, body, _) = request(
        &app,
        Method::POST,
        "/memory?wait=true&format=json",
        Some("tok"),
        Some("text/plain"),
        front,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a failed index must not fail the write: {body}"
    );
    assert!(body.contains("\"indexed\":false"), "{body}");
    assert!(
        body.contains("encode_error"),
        "and it must say why, not just that it did not: {body}"
    );

    // The row is there: the queue is derived state, so the write was never at risk.
    let (status, _, _) = get(&app, "/memory/unindexable?format=json").await;
    assert_eq!(status, StatusCode::OK, "the row is written regardless");
}

/// `?wait=true` costs exactly what its docs say it costs — it holds the store lock across the model
/// call — and **`/health` says so while it does**.
///
/// That witness is not decoration: `writer: busy 1239.1s (memory_reembed)` is what made the 45-minute
/// incident legible in the first place. A writer nobody can see is how a store comes to report `ready`
/// while it cannot write.
#[tokio::test]
async fn wait_true_holds_the_lock_and_health_names_the_holder() {
    /// Slow enough that a poller cannot miss the window.
    struct Slow;

    impl ellmephant::embed::Embedder for Slow {
        fn dim(&self) -> usize {
            64
        }
        fn model_id(&self) -> &str {
            "slow"
        }
        fn embed(&self, texts: &[String]) -> ellmephant::error::Result<Vec<Vec<f32>>> {
            std::thread::sleep(std::time::Duration::from_millis(600));
            Ok(vec![vec![0.0; 64]; texts.len()])
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let watch = Arc::new(LockWatch::new());
    let paths = Paths::from_root(dir.path());
    let cfg = Config::default();
    let spec = Spec::from_config(&cfg, &paths);
    let session = Session::open(paths.clone(), cfg.clone(), spec)
        .expect("open store")
        .with_embedder(Box::new(Slow));
    let state = HttpState::new(paths, cfg, Arc::clone(&watch), Instant::now())
        .with_write_token(Some("tok".into()));
    let app = http_router(
        MemoryServer::with_watch(Arc::new(Mutex::new(session)), watch),
        "127.0.0.1",
        state,
    );

    // The synchronous write runs in the background; the poller watches the witness in the foreground.
    let writing = {
        let app = app.clone();
        tokio::spawn(async move {
            let front = "Key: witnessed\nTitle: Witnessed\n---\na body embedded slowly\n";
            request(
                &app,
                Method::POST,
                "/memory?wait=true&format=json",
                Some("tok"),
                Some("text/plain"),
                front,
            )
            .await
        })
    };

    let deadline = Instant::now() + Duration::from_secs(10);
    let seen = loop {
        let (_, body, _) = get(&app, "/health?format=json").await;
        if body.contains("memory_encode_now") {
            break body;
        }
        assert!(
            Instant::now() < deadline,
            "the witness never named the lock holder: {body}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert!(
        seen.contains("\"writer\":\"busy"),
        "and it should read as busy while held: {seen}"
    );

    let (status, body, _) = writing.await.expect("the write task");
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.contains("\"indexed\":true"),
        "the synchronous door settles it before answering: {body}"
    );
}

/// History is reachable by ADDRESS from both doors — and a delete says what it did NOT remove.
///
/// This is the test the missing-FK decision needs: the row goes, the history stays, and the index
/// is the only thing in the store that can still name it.
#[tokio::test]
async fn history_is_reachable_by_address_after_the_row_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let app = app_open(dir.path());
    seed(&app, "", "kept", "Kept", "first state").await;
    // A change to the DOCUMENT earns a version (a tag or source write would not).
    seed(&app, "", "kept", "Kept", "second state").await;

    let peer = mcp_peer(MemoryServer::with_watch(
        Arc::new(Mutex::new(session(dir.path()))),
        Arc::new(LockWatch::new()),
    ))
    .await;

    // The list is METADATA: enough to choose an `n`, and no bodies.
    let listed: serde_json::Value = serde_json::from_str(
        &tool(
            &peer,
            "memory_history",
            serde_json::json!({ "key": "kept" }),
        )
        .await,
    )
    .expect("history json");
    assert_eq!(listed["count"], 1);
    assert_eq!(listed["versions"][0]["n"], 1);
    assert!(
        listed["versions"][0].get("body").is_none(),
        "the list is metadata; a body is a separate call"
    );

    // ...and one version reads in the same shape a live memory does.
    let past: serde_json::Value = serde_json::from_str(
        &tool(
            &peer,
            "memory_get",
            serde_json::json!({ "key": "kept", "version": 1 }),
        )
        .await,
    )
    .expect("version json");
    assert_eq!(past["body"], "first state");
    assert_eq!(past["version"], 1);
    assert_eq!(past["title"], "Kept");

    // HTTP answers from a TOP-LEVEL path: history is not a sub-resource of a live memory.
    let (status, body, _) = request(&app, Method::GET, "/history?key=kept", None, None, "").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("\"count\":1"), "{body}");
    let (status, body, _) =
        request(&app, Method::GET, "/memory/kept?version=1", None, None, "").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("first state"), "{body}");

    // The delete's receipt states what remains: history outlives the row by design.
    let receipt: serde_json::Value = serde_json::from_str(
        &tool(&peer, "memory_delete", serde_json::json!({ "key": "kept" })).await,
    )
    .expect("delete json");
    assert_eq!(receipt["status"], "removed");
    // The delete snapshotted the state it removed, so what remains is COMPLETE: both states.
    assert_eq!(receipt["versions_remaining"], 2);

    // The row is gone from the door...
    let (status, _, _) = request(&app, Method::GET, "/memory/kept", None, None, "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // ...but the address still answers, and the INDEX is how it is found at all.
    let (status, body, _) =
        request(&app, Method::GET, "/memory/kept?version=1", None, None, "").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("first state"), "{body}");
    // A delete loses NOTHING: the newest version IS the state it was deleted in, so the content it
    // held survives the row. That is why `erase` is the only real destroyer left.
    let (status, body, _) =
        request(&app, Method::GET, "/memory/kept?version=2", None, None, "").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("second state"), "{body}");
    let listed: serde_json::Value = serde_json::from_str(
        &tool(
            &peer,
            "memory_history",
            serde_json::json!({ "key": "kept" }),
        )
        .await,
    )
    .expect("history json");
    assert_eq!(
        listed["count"], 2,
        "the delete added a version, not removed one"
    );
    let idx: serde_json::Value = serde_json::from_str(
        &tool(
            &peer,
            "memory_history",
            serde_json::json!({ "orphans_only": true }),
        )
        .await,
    )
    .expect("index json");
    assert_eq!(idx["count"], 1);
    assert_eq!(idx["keys"][0]["key"], "kept");
    assert_eq!(idx["keys"][0]["orphan"], true);
}

/// The claim routes work end to end: acquire, see it, be refused by a second owner, release, and let
/// the second owner in. The store-level race is `tests/claims.rs`; this proves the HTTP door.
#[tokio::test]
async fn claims_round_trip_over_http() {
    let dir = tempfile::tempdir().unwrap();
    let watch = Arc::new(LockWatch::new());
    let state = HttpState::new(
        Paths::from_root(dir.path()),
        Config::default(),
        Arc::clone(&watch),
        Instant::now(),
    );
    let server = MemoryServer::with_watch(Arc::new(Mutex::new(session(dir.path()))), watch);
    let app = router(server, state);
    let json_body = Some("application/json");

    // A takes the lease.
    let (status, body, _) = request(
        &app,
        Method::POST,
        "/claim?format=json",
        None,
        json_body,
        r#"{"name":"bank","owner":"hand-a","ttl_seconds":300}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["won"], serde_json::json!(true), "{body}");
    assert_eq!(v["claim"]["owner"], serde_json::json!("hand-a"));

    // B is refused, and the reply names the holder.
    let (status, body, _) = request(
        &app,
        Method::POST,
        "/claim?format=json",
        None,
        json_body,
        r#"{"name":"bank","owner":"hand-b","ttl_seconds":300}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["won"], serde_json::json!(false), "{body}");
    assert_eq!(v["claim"]["owner"], serde_json::json!("hand-a"));

    // The audit view shows it.
    let (status, body, _) = get(&app, "/claim?format=json").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["claims"][0]["name"], serde_json::json!("bank"));

    // A releases it.
    let (status, body, _) = request(
        &app,
        Method::POST,
        "/claim/bank/release?format=json",
        None,
        json_body,
        r#"{"owner":"hand-a"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["released"], serde_json::json!(true), "{body}");

    // And B can now take it.
    let (status, body, _) = request(
        &app,
        Method::POST,
        "/claim?format=json",
        None,
        json_body,
        r#"{"name":"bank","owner":"hand-b","ttl_seconds":300}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["won"], serde_json::json!(true), "{body}");
}

/// A REFUSED compare-and-swap write must never be reported as success (#101).
///
/// `expected_version` is the documented way to do read-modify-write. Over the plain-HTTP door the
/// reply was built from `created` alone, so a refusal — nothing written — answered `200 updated`.
/// That is a silent failure for the one caller who is trying to be careful: an editor that fetched
/// a row, changed it, and posted back would believe it reconciled while another writer stood.
#[tokio::test]
async fn a_refused_cas_write_answers_conflict_not_updated() {
    let dir = tempfile::tempdir().unwrap();
    let app = app_with_token(dir.path(), "tok");
    seed(&app, "tok", "cas-probe", "one sharp line", "v1").await;
    settle(dir.path());

    let (_, body, _) = get(&app, "/memory/cas-probe?format=json").await;
    let row: serde_json::Value = serde_json::from_str(&body).unwrap();
    let version = row["version"].as_i64().expect("the row has a version");

    // A stale expectation: the condition is false, so the store writes nothing.
    let stale = serde_json::json!({
        "key": "cas-probe",
        "body": "stale write",
        "expected_version": version - 1,
    })
    .to_string();
    let (status, response, _) = request(
        &app,
        Method::POST,
        "/memory?format=json",
        Some("tok"),
        Some("application/json"),
        &stale,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a refused write must not answer 200: {response}"
    );
    let refused: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert_eq!(
        refused["status"], "conflict",
        "the MCP shape, on both doors"
    );
    assert_eq!(
        refused["current_version"], version,
        "the version to retry against"
    );
    assert_eq!(refused["key"], "cas-probe");

    // And the refusal is honest: the row is untouched.
    let (_, body, _) = get(&app, "/memory/cas-probe?format=json").await;
    let row: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(row["version"], version, "nothing was written");
    assert_eq!(row["body"], "v1");

    // The text door agrees with the JSON door rather than saying "stored".
    let (status, text, _) = request(
        &app,
        Method::POST,
        "/memory",
        Some("tok"),
        Some("application/json"),
        &stale,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(text.contains("conflict"), "text says conflict: {text}");
    assert!(
        !text.contains("stored"),
        "text must not claim a store: {text}"
    );

    // A write with the RIGHT expectation still lands.
    let fresh = serde_json::json!({
        "key": "cas-probe",
        "body": "v2",
        "expected_version": version,
    })
    .to_string();
    let (status, response, _) = request(
        &app,
        Method::POST,
        "/memory?format=json",
        Some("tok"),
        Some("application/json"),
        &fresh,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a valid CAS write still writes: {response}"
    );
    let written: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert_eq!(written["status"], "updated");
}

/// ★ THE BOX IS THE BOUNDARY ON EVERY DOOR (soma/ellmephant#92).
///
/// Measured 2026-10-22: an anonymous `GET /mail?recipient=stet` answered **200 with 49 envelopes** —
/// the MCP tool had refused non-recipients since 2026-10-18, and this door had no check at all.
#[tokio::test]
async fn an_anonymous_caller_cannot_read_a_mailbox() {
    let dir = tempfile::tempdir().unwrap();
    let watch = Arc::new(LockWatch::new());
    let state = HttpState::new(
        Paths::from_root(dir.path()),
        Config::default(),
        Arc::clone(&watch),
        Instant::now(),
    );
    let server = MemoryServer::with_watch(Arc::new(Mutex::new(session(dir.path()))), watch);
    let app = router(server, state);

    // The inbox: envelopes leak senders, subjects and counts even without bodies.
    let (status, body, _) = get(&app, "/mail?recipient=stet").await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "an anonymous inbox read must be refused, got {status}: {body}"
    );
    assert!(
        body.contains("proven pen"),
        "the refusal must say what is missing: {body}"
    );

    // The body, by seq, is the same act one step in.
    let (status, _, _) = get(&app, "/mail/1").await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "an anonymous letter read must be refused too"
    );

    // A cursor is the reader's own; an anonymous caller has no reader to name.
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/mail/ack?recipient=stet&seq=1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("http request");
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "an anonymous cursor advance must be refused"
    );
}

/// A row says who answers for it, and the walk says what that pen is to the reader — `memory_get`
/// carries both, computed at READ time from the asker's seat (James, 2026-10-23).
#[tokio::test]
async fn memory_get_names_the_owner_and_what_they_are_to_you() {
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let watch = Arc::new(LockWatch::new());
    let shared = Arc::new(Mutex::new(session(dir.path())));
    let mcp = MemoryServer::with_watch(Arc::clone(&shared), Arc::clone(&watch));
    let state = HttpState::new(
        Paths::from_root(dir.path()),
        Config::default(),
        Arc::clone(&watch),
        Instant::now(),
    )
    .with_write_token(Some("tok".into()));
    let app = http_router(
        MemoryServer::with_watch(Arc::clone(&shared), Arc::clone(&watch)),
        "127.0.0.1",
        state,
    );
    let service = mcp_peer(mcp).await;

    tool(
        service.peer(),
        "memory_upsert",
        json!({ "key": "mine", "title": "Mine", "body": "the facts" }),
    )
    .await;

    // The asker IS the owner here, so the walk is `ME` — the one answer that needs no gloss.
    let got = tool(service.peer(), "memory_get", json!({ "key": "mine" })).await;
    let doc: serde_json::Value = serde_json::from_str(&got).unwrap();
    assert_eq!(doc["owner"], json!("tester"), "{got}");
    assert_eq!(doc["owner_relation"], json!("ME"), "{got}");

    // The HTTP door reached without an identity pair is NOBODY: it still learns who answers for the
    // row, and learns nothing about what that pen is to it. Absence, never a guess.
    let (status, body, _) = get(&app, "/memory/mine").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("owner   tester"), "{body}");
    assert!(
        !body.contains("ME"),
        "an anonymous reader has no walk: {body}"
    );
}

/// `whois` — the tree is configuration, not content, so an unproven reader may ask WHO a pen is;
/// what it does not get is a walk, because it has not proved to be anybody.
#[tokio::test]
async fn whois_answers_on_both_doors() {
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let watch = Arc::new(LockWatch::new());
    let shared = Arc::new(Mutex::new(session(dir.path())));
    let mcp = MemoryServer::with_watch(Arc::clone(&shared), Arc::clone(&watch));
    let state = HttpState::new(
        Paths::from_root(dir.path()),
        Config::default(),
        Arc::clone(&watch),
        Instant::now(),
    )
    .with_write_token(Some("tok".into()));
    let app = http_router(
        MemoryServer::with_watch(Arc::clone(&shared), Arc::clone(&watch)),
        "127.0.0.1",
        state,
    );
    let service = mcp_peer(mcp).await;

    // A write registers the pen (`ensure_pen`), so the peer's own pen exists to be looked up.
    tool(
        service.peer(),
        "memory_upsert",
        json!({ "key": "mine", "title": "Mine", "body": "the facts" }),
    )
    .await;

    let got = tool(service.peer(), "whois", json!({ "pen": "tester" })).await;
    let doc: serde_json::Value = serde_json::from_str(&got).unwrap();
    assert_eq!(doc["name"], json!("tester"), "{got}");
    assert_eq!(doc["kind"], json!("self"), "{got}");
    assert_eq!(doc["owns"], json!(1), "{got}");
    assert_eq!(doc["relation"], json!("ME"), "{got}");
    assert!(
        doc["ceased"].is_null(),
        "a live pen has no tombstone: {got}"
    );

    // The same pen over HTTP, asked by nobody: same facts, and no walk.
    let (status, body, _) = get(&app, "/whois/tester?format=json").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let over_http: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(over_http["owns"], json!(1), "{body}");
    assert!(
        over_http["relation"].is_null(),
        "an unproven reader gets no walk: {body}"
    );

    // A name nobody minted is refused, not invented.
    let (status, _, _) = get(&app, "/whois/nobody-at-all").await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "an unknown pen is a refusal"
    );
}

/// `pen_mint` — the runtime door, on both doors. The parent is the CALLER, and a name is spent.
#[tokio::test]
async fn pen_mint_creates_a_child_under_the_caller() {
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let watch = Arc::new(LockWatch::new());
    let shared = Arc::new(Mutex::new(session(dir.path())));
    let mcp = MemoryServer::with_watch(Arc::clone(&shared), Arc::clone(&watch));
    let state = HttpState::new(
        Paths::from_root(dir.path()),
        Config::default(),
        Arc::clone(&watch),
        Instant::now(),
    )
    .with_write_token(Some("tok".into()));
    let _app = http_router(
        MemoryServer::with_watch(Arc::clone(&shared), Arc::clone(&watch)),
        "127.0.0.1",
        state,
    );
    let service = mcp_peer(mcp).await;

    // A write first, so the caller exists as a pen (`ensure_pen`) before it mints.
    tool(
        service.peer(),
        "memory_upsert",
        json!({ "key": "seed", "title": "Seed", "body": "first" }),
    )
    .await;

    let minted = tool(
        service.peer(),
        "pen_mint",
        json!({ "name": "tester/finger-9f3a1c", "kind": "finger" }),
    )
    .await;
    let doc: serde_json::Value = serde_json::from_str(&minted).unwrap();
    assert_eq!(doc["name"], json!("tester/finger-9f3a1c"), "{minted}");
    assert_eq!(doc["kind"], json!("finger"), "{minted}");
    assert_eq!(
        doc["parent"],
        json!("tester"),
        "the caller is the parent: {minted}"
    );

    // The new pen is in the tree, and the walk says what it is to the minting hand.
    let seen = tool(
        service.peer(),
        "whois",
        json!({ "pen": "tester/finger-9f3a1c" }),
    )
    .await;
    let seen: serde_json::Value = serde_json::from_str(&seen).unwrap();
    assert_eq!(seen["relation"], json!("C \"child\""), "{seen}");

    // A name is spent: minting it again is refused, and so is a kind nobody defined.
    let again = tool(
        service.peer(),
        "pen_mint",
        json!({ "name": "tester/finger-9f3a1c", "kind": "finger" }),
    )
    .await;
    assert!(again.contains("already a pen"), "{again}");
    let bad = tool(
        service.peer(),
        "pen_mint",
        json!({ "name": "tester/odd", "kind": "hand" }),
    )
    .await;
    assert!(bad.contains("`self` or `finger`"), "{bad}");
}

/// `as:` — a write credited to a descendant, permitted by a LIVE LEND and not by tree shape.
#[tokio::test]
async fn a_write_may_be_credited_to_a_finger_it_holds_a_lend_for() {
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let watch = Arc::new(LockWatch::new());
    let shared = Arc::new(Mutex::new(session(dir.path())));
    let mcp = MemoryServer::with_watch(Arc::clone(&shared), Arc::clone(&watch));
    let state = HttpState::new(
        Paths::from_root(dir.path()),
        Config::default(),
        Arc::clone(&watch),
        Instant::now(),
    )
    .with_write_token(Some("tok".into()));
    let _app = http_router(
        MemoryServer::with_watch(Arc::clone(&shared), Arc::clone(&watch)),
        "127.0.0.1",
        state,
    );
    let service = mcp_peer(mcp).await;

    tool(
        service.peer(),
        "memory_upsert",
        json!({ "key": "seed", "title": "Seed", "body": "s" }),
    )
    .await;
    tool(
        service.peer(),
        "pen_mint",
        json!({ "name": "tester/f-1", "kind": "finger" }),
    )
    .await;

    // ★ PARENTAGE IS NOT ENOUGH: without a lend the credit is refused, and the refusal says why.
    let refused = tool(
        service.peer(),
        "memory_upsert",
        json!({ "key": "finger-work", "title": "Finger work", "body": "b", "as": "tester/f-1" }),
    )
    .await;
    assert!(refused.contains("LIVE lend"), "{refused}");

    // The lend is what permits it, and it is a session with an expiry.
    let lent = tool(
        service.peer(),
        "pen_lend",
        json!({ "pen": "tester/f-1", "secs": 60 }),
    )
    .await;
    let session: serde_json::Value = serde_json::from_str(&lent).unwrap();
    assert_eq!(session["powerful"], json!("tester"), "{lent}");
    assert_eq!(session["weak"], json!("tester/f-1"), "{lent}");
    assert!(session["expires_at"].is_string(), "a lend expires: {lent}");

    let done = tool(
        service.peer(),
        "memory_upsert",
        json!({ "key": "finger-work", "title": "Finger work", "body": "b", "as": "tester/f-1" }),
    )
    .await;
    assert!(done.contains("\"created\""), "{done}");

    // The row says WHO did it and WHO answers for it — and they are different pens.
    let got = tool(
        service.peer(),
        "memory_get",
        json!({ "key": "finger-work" }),
    )
    .await;
    let doc: serde_json::Value = serde_json::from_str(&got).unwrap();
    assert_eq!(doc["asserted_by"], json!("tester/f-1"), "who did it: {got}");
    assert_eq!(doc["owner"], json!("tester"), "who answers for it: {got}");
    assert_eq!(doc["owner_relation"], json!("ME"), "{got}");
}

/// The identity fixture for HTTP writes.
///
/// ★ ONE TABLE, SET ONCE, AS A SUPERSET: cargo runs the tests in this binary in parallel threads, and
/// `ELLMEPHANT_IDENTITIES` is process-global. A per-test table that each test removes would make
/// whichever test finished last decide the identity of the one still running. Every HTTP test that
/// needs a proven pen calls this and none of them removes it.
fn identity_fixture() {
    const TABLE: &str = "ELLMEPHANT_IDENTITIES";
    unsafe {
        std::env::set_var(
            TABLE,
            "http-as-tester:as-secret-1,http-as-gated:as-secret-g",
        )
    };
}

/// `as:` on the BODY of `POST /memory` — the same claim as the MCP argument, and the same gate: a
/// live lend, not tree shape. (The identity fixture is local to this test: it is the only one that
/// needs a proven pen over HTTP, and it removes the table when it is done.)
#[tokio::test]
async fn the_http_write_accepts_an_as_claim_but_only_with_a_live_lend() {
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let app = app_with_token(dir.path(), "tok");

    // Provisioning, outside every door: two pens and (later) a lend.
    let paths = Paths::from_root(dir.path());
    {
        let conn = ellmephant::db::open(&paths).unwrap();
        conn.execute_batch(
            "INSERT OR IGNORE INTO pen (name, kind) VALUES ('http-as-tester', 'self');
             INSERT OR IGNORE INTO pen (name, kind, parent_id)
                 VALUES ('http-as-tester/f-1', 'finger', (SELECT id FROM pen WHERE name='http-as-tester'));",
        )
        .unwrap();
    }

    identity_fixture();

    let post = |body: serde_json::Value| {
        let app = app.clone();
        async move {
            let req = Request::builder()
                .method(Method::POST)
                .uri("/memory")
                .header(header::AUTHORIZATION, "Bearer tok")
                .header(header::CONTENT_TYPE, "application/json")
                .header("X-Stet-From", "http-as-tester")
                .header("X-Stet-Auth", "as-secret-1")
                .body(Body::from(body.to_string()))
                .unwrap();
            let res = app.oneshot(req).await.unwrap();
            let status = res.status();
            let bytes = to_bytes(res.into_body(), 64 * 1024).await.unwrap();
            (status, String::from_utf8_lossy(&bytes).to_string())
        }
    };

    // No lend: the claim is refused, and the refusal is the same one the MCP door gives.
    let (status, body) = post(json!({
        "key": "finger-note", "title": "Finger note", "body": "b",
        "as": "http-as-tester/f-1"
    }))
    .await;
    assert_ne!(
        status,
        StatusCode::OK,
        "no lend, no credit: {status} {body}"
    );
    assert!(body.contains("LIVE lend"), "{status} {body}");

    // With a lend it lands, credited to the finger.
    {
        let conn = ellmephant::db::open(&paths).unwrap();
        ellmephant::store::sudo::lend(&conn, "http-as-tester", "http-as-tester/f-1", 60).unwrap();
    }
    let (status, body) = post(json!({
        "key": "finger-note", "title": "Finger note", "body": "b",
        "as": "http-as-tester/f-1"
    }))
    .await;
    assert!(status.is_success(), "{status} {body}");
    assert!(body.contains("finger-note"), "{status} {body}");

    let conn = ellmephant::db::open(&paths).unwrap();
    let mem = ellmephant::store::get(&conn, "finger-note")
        .unwrap()
        .unwrap();
    assert_eq!(
        mem.asserted_by.as_deref(),
        Some("http-as-tester/f-1"),
        "who did it"
    );
    assert_eq!(
        mem.owner.as_deref(),
        Some("http-as-tester"),
        "who answers for it"
    );
}

/// The three sudo verbs on the door: a borrow needs POLICY, a pass goes DOWN, a release CASCADES.
/// The policy is written outside the doors — that is what provisioning is, and it is the point.
#[tokio::test]
async fn the_sudo_verbs_are_on_the_door() {
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let watch = Arc::new(LockWatch::new());
    let shared = Arc::new(Mutex::new(session(dir.path())));
    let mcp = MemoryServer::with_watch(Arc::clone(&shared), Arc::clone(&watch));
    let state = HttpState::new(
        Paths::from_root(dir.path()),
        Config::default(),
        Arc::clone(&watch),
        Instant::now(),
    )
    .with_write_token(Some("tok".into()));
    let _app = http_router(
        MemoryServer::with_watch(Arc::clone(&shared), Arc::clone(&watch)),
        "127.0.0.1",
        state,
    );
    let service = mcp_peer(mcp).await;

    tool(
        service.peer(),
        "memory_upsert",
        json!({ "key": "seed", "title": "Seed", "body": "s" }),
    )
    .await;

    // No policy, no borrow — authority is granted, never assumed.
    let refused = tool(
        service.peer(),
        "sudo",
        json!({ "powerful": "estate", "secs": 60 }),
    )
    .await;
    assert!(refused.contains("no sudo policy"), "{refused}");

    // Provisioning writes the policy, the way terraform would: outside every door.
    {
        let conn = ellmephant::db::open(&Paths::from_root(dir.path())).unwrap();
        ellmephant::store::sudo::grant(&conn, "estate", "tester", 600, "test").unwrap();
    }

    let borrowed = tool(
        service.peer(),
        "sudo",
        json!({ "powerful": "estate", "secs": 300 }),
    )
    .await;
    let session: serde_json::Value = serde_json::from_str(&borrowed).unwrap();
    assert_eq!(session["weak"], json!("tester"), "{borrowed}");
    assert_eq!(session["powerful"], json!("estate"), "{borrowed}");

    // A borrow cannot outlive its policy.
    let too_long = tool(
        service.peer(),
        "sudo",
        json!({ "powerful": "estate", "secs": 601 }),
    )
    .await;
    assert!(too_long.contains("at most 600s"), "{too_long}");

    // Passing goes DOWN, and the receiver's session names where it came from.
    tool(
        service.peer(),
        "pen_mint",
        json!({ "name": "tester/hand", "kind": "self" }),
    )
    .await;
    let passed = tool(
        service.peer(),
        "sudo_pass",
        json!({ "powerful": "estate", "pen": "tester/hand", "secs": 60 }),
    )
    .await;
    let passed: serde_json::Value = serde_json::from_str(&passed).unwrap();
    assert_eq!(passed["weak"], json!("tester/hand"), "{passed}");
    assert!(
        passed["via"].is_i64(),
        "a passed session names its source: {passed}"
    );

    // Releasing cascades: the hand's session and the finger's, in one act.
    let released = tool(
        service.peer(),
        "sudo_release",
        json!({ "powerful": "estate" }),
    )
    .await;
    let released: serde_json::Value = serde_json::from_str(&released).unwrap();
    assert_eq!(
        released["sessions"],
        json!(2),
        "both sessions died: {released}"
    );
}

/// The audit, END TO END: an ancestor reading a descendant's private row leaves a `private_read`
/// record naming the pen, the row and the reason. This is #120's missing half (#129).
#[tokio::test]
async fn reading_into_the_subtree_is_recorded_on_the_door() {
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let watch = Arc::new(LockWatch::new());
    let shared = Arc::new(Mutex::new(session(dir.path())));
    let mcp = MemoryServer::with_watch(Arc::clone(&shared), Arc::clone(&watch));
    let state = HttpState::new(
        Paths::from_root(dir.path()),
        Config::default(),
        Arc::clone(&watch),
        Instant::now(),
    )
    .with_write_token(Some("tok".into()));
    let _app = http_router(
        MemoryServer::with_watch(Arc::clone(&shared), Arc::clone(&watch)),
        "127.0.0.1",
        state,
    );
    let service = mcp_peer(mcp).await;

    // A hand, one finger under it, and a lend so the finger can be credited for its own work.
    tool(
        service.peer(),
        "memory_upsert",
        json!({ "key": "seed", "title": "Seed", "body": "s" }),
    )
    .await;
    tool(
        service.peer(),
        "pen_mint",
        json!({ "name": "tester/f-1", "kind": "finger" }),
    )
    .await;
    tool(
        service.peer(),
        "pen_lend",
        json!({ "pen": "tester/f-1", "secs": 60 }),
    )
    .await;
    let wrote = tool(
        service.peer(),
        "memory_upsert",
        json!({ "key": "finger-note", "title": "Finger note", "body": "private work",
                "audience": "subtree:tester", "as": "tester/f-1" }),
    )
    .await;
    assert!(
        wrote.contains("stored") || wrote.contains("created"),
        "{wrote}"
    );

    // The ancestor reads it — allowed, and now RECORDED.
    let got = tool(
        service.peer(),
        "memory_get",
        json!({ "key": "finger-note" }),
    )
    .await;
    assert!(
        got.contains("private work"),
        "an ancestor reads down its subtree: {got}"
    );

    let conn = ellmephant::db::open(&Paths::from_root(dir.path())).unwrap();
    let rows = ellmephant::store::audit::recent(&conn, 10).unwrap();
    let (_, pen, key, reason, _) = rows
        .iter()
        .find(|r| r.2.as_deref() == Some("finger-note"))
        .unwrap_or_else(|| panic!("no audit row for the subtree read: {rows:?}"));
    assert_eq!(pen, "tester", "who looked");
    assert_eq!(key.as_deref(), Some("finger-note"), "which row");
    assert_eq!(reason, "subtree", "why");
}

/// The audit covers PAST STATES too: `memory_get?version=N` hands an ancestor the contents of a
/// descendant's private row, so that boundary is recorded on the same table (#129, found reviewing it).
#[tokio::test]
async fn reading_a_past_state_of_a_descendants_private_row_is_recorded() {
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let watch = Arc::new(LockWatch::new());
    let shared = Arc::new(Mutex::new(session(dir.path())));
    let mcp = MemoryServer::with_watch(Arc::clone(&shared), Arc::clone(&watch));
    let state = HttpState::new(
        Paths::from_root(dir.path()),
        Config::default(),
        Arc::clone(&watch),
        Instant::now(),
    )
    .with_write_token(Some("tok".into()));
    let _app = http_router(
        MemoryServer::with_watch(Arc::clone(&shared), Arc::clone(&watch)),
        "127.0.0.1",
        state,
    );
    let service = mcp_peer(mcp).await;

    tool(
        service.peer(),
        "memory_upsert",
        json!({ "key": "seed", "title": "Seed", "body": "s" }),
    )
    .await;
    tool(
        service.peer(),
        "pen_mint",
        json!({ "name": "tester/f-1", "kind": "finger" }),
    )
    .await;
    tool(
        service.peer(),
        "pen_lend",
        json!({ "pen": "tester/f-1", "secs": 60 }),
    )
    .await;
    tool(
        service.peer(),
        "memory_upsert",
        json!({ "key": "f-note", "title": "v1", "body": "first private",
                "audience": "subtree:tester", "as": "tester/f-1" }),
    )
    .await;
    // A second write supersedes the first, so v1 is now history.
    tool(
        service.peer(),
        "memory_upsert",
        json!({ "key": "f-note", "title": "v2", "body": "second private",
                "audience": "subtree:tester", "as": "tester/f-1" }),
    )
    .await;

    let past = tool(
        service.peer(),
        "memory_get",
        json!({ "key": "f-note", "version": 1 }),
    )
    .await;
    assert!(
        past.contains("first private"),
        "an ancestor reads a past state: {past}"
    );

    let conn = ellmephant::db::open(&Paths::from_root(dir.path())).unwrap();
    let rows = ellmephant::store::audit::recent(&conn, 20).unwrap();
    assert!(
        rows.iter()
            .any(|r| r.2.as_deref() == Some("f-note") && r.4.as_deref() == Some("version/history")),
        "no audit row for the version read: {rows:?}"
    );
}

/// ★ THE GATE, THROUGH THE DOOR (#151): a pen that was TOLD something in a gated channel and did not
/// listen cannot change the record — and fetching is what reopens it.
///
/// This is the half that makes ellmechat worth building: the schema and the ledger were already
/// tested; what was untested is that a *write* is actually refused.
#[tokio::test]
async fn a_pen_behind_a_gated_channel_cannot_change_the_record() {
    use serde_json::json;
    identity_fixture();

    let dir = tempfile::tempdir().unwrap();
    let app = app_with_token(dir.path(), "tok");
    let paths = Paths::from_root(dir.path());

    // Provisioning, outside every door: the pen, its membership, and an unread incident.
    let seq;
    {
        let conn = ellmephant::db::open(&paths).unwrap();
        conn.execute_batch(
            "INSERT OR IGNORE INTO pen (name, kind) VALUES ('http-as-gated', 'self');
             INSERT OR IGNORE INTO pen (name, kind) VALUES ('james', 'self');",
        )
        .unwrap();
        // `ops_team` is invite-only: a pen cannot add itself, so the keeper invites both the pen
        // under test and the pen that raises the incident.
        ellmephant::store::channel::invite(&conn, "james", "http-as-gated", "ops_team").unwrap();
        ellmephant::store::channel::invite(&conn, "james", "james", "ops_team").unwrap();
        let m = ellmephant::store::channel::send(
            &conn,
            "james",
            "ops_team",
            "the cluster is on fire",
            None,
        )
        .expect("send");
        seq = m.seq;
        // Two boundaries offered it and the pen did not fetch: `ops_team` tolerates one, not two.
        ellmephant::store::channel::present(&conn, "http-as-gated", None).unwrap();
        ellmephant::store::channel::present(&conn, "http-as-gated", None).unwrap();
        // The grace is 60s from the FIRST OPPORTUNITY; backdate the opportunity, not the message.
        conn.execute(
            "UPDATE channel_delivery SET presented_at = datetime('now','-5 minutes')
              WHERE seq = ?1 AND pen_id = (SELECT id FROM pen WHERE name = 'http-as-gated')",
            [seq],
        )
        .unwrap();
    }

    let post = |app: axum::Router| async move {
        let req = Request::builder()
            .method(Method::POST)
            .uri("/memory")
            .header(header::AUTHORIZATION, "Bearer tok")
            .header(header::CONTENT_TYPE, "application/json")
            .header("X-Stet-From", "http-as-gated")
            .header("X-Stet-Auth", "as-secret-g")
            .body(Body::from(
                json!({ "key": "after-incident", "title": "t", "body": "b" }).to_string(),
            ))
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = to_bytes(res.into_body(), 64 * 1024).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    };

    let (status, body) = post(app.clone()).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a behind pen must not write: {status} {body}"
    );
    assert!(
        body.contains("ops_team") && body.contains("not been fetched"),
        "and the refusal names the channel and the reason: {body}"
    );

    // LISTEN. Fetching is the only thing that clears it, and it clears it for the whole ledger.
    {
        let conn = ellmephant::db::open(&paths).unwrap();
        let heard = ellmephant::store::channel::fetch(&conn, "http-as-gated", None, 50).unwrap();
        assert_eq!(heard.len(), 1, "one message was waiting");
        assert!(heard[0].body.contains("on fire"));
    }

    let (status, body) = post(app).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "after fetching, the record opens: {status} {body}"
    );
}

/// ★ THE TWO DOORS MUST AGREE (#159, Stet's refusal): not "both paths work" — **the same verdict on
/// the same inputs, compared.** One shared store, one gated pen, one refusal text.
#[tokio::test]
async fn the_gate_gives_the_same_verdict_on_both_doors() {
    use serde_json::json;
    identity_fixture();
    let dir = tempfile::tempdir().unwrap();
    let watch = Arc::new(LockWatch::new());
    let shared = Arc::new(Mutex::new(session(dir.path())));
    let mcp = MemoryServer::with_watch(Arc::clone(&shared), Arc::clone(&watch));
    let state = HttpState::new(
        Paths::from_root(dir.path()),
        Config::default(),
        Arc::clone(&watch),
        Instant::now(),
    )
    .with_write_token(Some("tok".into()));
    let app = http_router(
        MemoryServer::with_watch(Arc::clone(&shared), Arc::clone(&watch)),
        "127.0.0.1",
        state,
    );
    let service = mcp_peer_as(mcp, "http-as-gated").await;

    // Provisioning outside both doors: a pen on the ops team, and an incident it has not fetched.
    {
        let paths = Paths::from_root(dir.path());
        let conn = ellmephant::db::open(&paths).unwrap();
        conn.execute_batch(
            "INSERT OR IGNORE INTO pen (name, kind) VALUES ('http-as-gated', 'self');
             INSERT OR IGNORE INTO pen (name, kind) VALUES ('james', 'self');",
        )
        .unwrap();
        use ellmephant::store::channel as ch;
        ch::join(&conn, "http-as-gated", "ops_team").unwrap();
        ch::join(&conn, "james", "ops_team").unwrap();
        let m = ch::send(&conn, "james", "ops_team", "the cluster is on fire", None).unwrap();
        ch::present(&conn, "http-as-gated").unwrap();
        ch::present(&conn, "http-as-gated").unwrap();
        conn.execute(
            "UPDATE channel_delivery SET presented_at = datetime('now','-5 minutes')
              WHERE seq = ?1 AND pen_id = (SELECT id FROM pen WHERE name='http-as-gated')",
            [m.seq],
        )
        .unwrap();
        let b = ch::breach(&conn, "http-as-gated").unwrap();
        assert!(b.is_some(), "the fixture must actually gate the pen: {b:?}");
    }

    // Door one: MCP.
    let mcp_verdict = tool(
        service.peer(),
        "memory_upsert",
        json!({ "key": "through-mcp", "title": "t", "body": "b" }),
    )
    .await;

    // Door two: HTTP.
    let req = Request::builder()
        .method(Method::POST)
        .uri("/memory")
        .header(header::AUTHORIZATION, "Bearer tok")
        .header(header::CONTENT_TYPE, "application/json")
        .header("X-Stet-From", "http-as-gated")
        .header("X-Stet-Auth", "as-secret-g")
        .body(Body::from(
            json!({ "key": "through-http", "title": "t", "body": "b" }).to_string(),
        ))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = to_bytes(res.into_body(), 64 * 1024).await.unwrap();
    let http_verdict = String::from_utf8_lossy(&bytes).to_string();

    assert_eq!(status, StatusCode::BAD_REQUEST, "{http_verdict}");
    assert!(
        mcp_verdict.contains("ops_team") && mcp_verdict.contains("not been fetched"),
        "MCP must refuse by name: {mcp_verdict}"
    );
    assert!(
        http_verdict.contains("ops_team") && http_verdict.contains("not been fetched"),
        "HTTP must refuse by name: {http_verdict}"
    );
    // The SAME verdict, compared — the channel, the reason, and the count, not just "both failed".
    let mcp_core: Vec<&str> = mcp_verdict
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|w| !w.is_empty())
        .collect();
    for word in ["ops_team", "offered", "fetched"] {
        assert!(
            mcp_core.contains(&word),
            "MCP verdict lost {word}: {mcp_verdict}"
        );
        assert!(
            http_verdict.contains(word),
            "HTTP verdict lost {word}: {http_verdict}"
        );
    }

    // And the same inputs STOP being refused once the pen listens — on both doors, again compared.
    {
        let paths = Paths::from_root(dir.path());
        let conn = ellmephant::db::open(&paths).unwrap();
        ellmephant::store::channel::fetch(&conn, "http-as-gated", None, 50).unwrap();
    }
    let mcp_after = tool(
        service.peer(),
        "memory_upsert",
        json!({ "key": "through-mcp", "title": "t", "body": "b" }),
    )
    .await;
    assert!(
        mcp_after.contains("\"status\""),
        "MCP opens after fetch: {mcp_after}"
    );

    let req = Request::builder()
        .method(Method::POST)
        .uri("/memory")
        .header(header::AUTHORIZATION, "Bearer tok")
        .header(header::CONTENT_TYPE, "application/json")
        .header("X-Stet-From", "http-as-gated")
        .header("X-Stet-Auth", "as-secret-g")
        .body(Body::from(
            json!({ "key": "through-http", "title": "t", "body": "b" }).to_string(),
        ))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK, "HTTP opens after fetch");
}
