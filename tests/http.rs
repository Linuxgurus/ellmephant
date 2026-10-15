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
    // has to agree with the fake injected below. Disagree, and `doc_hash` describes a different space
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
        body.contains("not in schema v8"),
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
    assert!(text.contains("0 failed"), "{text}");
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
    let spec = ellmephant::embed::VecSpec::of(&embedder);
    while ellmephant::store::encode::drain(&conn, &embedder, &spec, 64).expect("drain") > 0 {}
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
    assert!(checked >= 7, "expected every write route, saw {checked}");
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
        boot_open, boot_gated,
        "a boot must not depend on the auth config"
    );
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
        "bootstrap-self",
        "boot me",
        "BOOT: read me first, then go",
    )
    .await;
    let secret = "a body that is NOT resident, only indexed by key";
    seed_tagged(
        &app,
        "tok",
        "other-self",
        "another cline-self key",
        "cline-self",
        secret,
    )
    .await;

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
        doc.contains("bootstrap-self"),
        "and named, so it can be fetched again: {doc}"
    );

    // Everything else in the self set is an INDEX: key and title, never the body.
    assert!(
        doc.contains("other-self") && doc.contains("another cline-self key"),
        "the self set is indexed by key and title: {doc}"
    );
    assert!(
        !doc.contains(secret),
        "a non-resident body must not be smuggled in: {doc}"
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
        "bootstrap-self",
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
            "cline-self",
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
    assert!(doc.contains("self-00") && doc.contains("self-19"), "{doc}");
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
        "bootstrap-self",
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
    assert_eq!(
        doc["youResident"]["key"],
        serde_json::json!("bootstrap-self")
    );
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
    // A six-valued filter enum, spelled out.
    assert!(
        doc.contains("coherent: \"true\"|\"false\"|\"null\"|\"!true\"|\"!false\"|\"!null\""),
        "{doc}"
    );
    // An array of an enum is parenthesised, so the `[]` cannot be misread.
    assert!(
        doc.contains("(\"key\"|\"title\"|\"tags\"|\"coherent\"|\"chars\"|\"distance\")[]"),
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
    let service = mcp_peer(mcp).await;

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
    assert_eq!(
        http_rest, mcp_rest,
        "every field but the counter must match"
    );
}

/// The MCP tool's OWN arguments, sent as a request body, must work on the write routes that take an
/// argument: a caller that knows `memory_coherent` and `memory_merge` should not have to learn a
/// second spelling for the same thing. The query forms stay, for a caller that only has a URL.
#[tokio::test]
async fn write_routes_accept_the_mcp_arguments_as_a_body() {
    let dir = tempfile::tempdir().unwrap();
    let app = app_with_token(dir.path(), "tok");
    seed(&app, "tok", "mb", "a body", "text").await;

    let coherent_of = |body: &str| -> serde_json::Value {
        serde_json::from_str::<serde_json::Value>(body).unwrap()["coherent"].clone()
    };

    // `memory_coherent` takes {key, coherent}; an absent field is `null` (unjudged), which is what
    // the tool's optional field means.
    let (status, body, _) = request(
        &app,
        Method::PUT,
        "/memory/mb/coherent?format=json",
        Some("tok"),
        Some("application/json"),
        r#"{"key":"mb","coherent":true}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, got, _) = get(&app, "/memory/mb?format=json").await;
    assert_eq!(coherent_of(&got), serde_json::json!(true), "{got}");

    let (status, body, _) = request(
        &app,
        Method::PUT,
        "/memory/mb/coherent?format=json",
        Some("tok"),
        Some("application/json"),
        r#"{"key":"mb"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, got, _) = get(&app, "/memory/mb?format=json").await;
    assert_eq!(coherent_of(&got), serde_json::Value::Null, "{got}");

    // A body `key` that disagrees with the path is refused, not silently preferred.
    let (status, body, _) = request(
        &app,
        Method::PUT,
        "/memory/mb/coherent?format=json",
        Some("tok"),
        Some("application/json"),
        r#"{"key":"other","coherent":true}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // `memory_merge` takes {from, into} — and a merge refuses to silently drop prose, so the target
    // must already carry the source's. That is the documented order: write the facts into `into`
    // first, then retire `from`.
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
    assert_eq!(status, StatusCode::NOT_FOUND, "the source is retired");

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

    // Both ways in, one meaning: the query spellings still work.
    seed(&app, "tok", "mb3", "third", "y").await;
    let (status, body, _) = request(
        &app,
        Method::PUT,
        "/memory/mb3/coherent?value=false&format=json",
        Some("tok"),
        None,
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, got, _) = get(&app, "/memory/mb3?format=json").await;
    assert_eq!(coherent_of(&got), serde_json::json!(false), "{got}");
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

/// A merge that would silently drop prose is refused. `into`'s body is never edited, so retiring a
/// memory carrying a fact the target lacks deletes the fact and reports only link counts — which
/// made the one operation called "tidying up" the one that could quietly lose the detail.
#[tokio::test]
async fn a_merge_that_would_drop_prose_is_refused() {
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
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("refusing to merge"), "{body}");
    assert!(
        body.contains("DELETE"),
        "it says how to discard on purpose: {body}"
    );

    // A refused merge retires nothing.
    let (status, _, _) = get(&app, "/memory/dup-from?format=json").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a refused merge must not delete the source"
    );

    // Fold the fact in first, and the same merge goes through.
    seed(
        &app,
        "tok",
        "dup-into",
        "the survivor",
        "only the survivor's words\na fact only the duplicate carries",
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
    let (status, _, _) = get(&app, "/memory/dup-from?format=json").await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "the source retires once the fold is in"
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
