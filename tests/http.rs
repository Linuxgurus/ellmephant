use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode, header};
use ellmephant::config::{Config, Paths};
use ellmephant::embed::HashEmbedder;
use ellmephant::http::{HttpState, LockWatch};
use ellmephant::mcp::{MemoryServer, http_router};
use ellmephant::model::Spec;
use ellmephant::store::session::Session;
use rmcp::model::CallToolRequestParams;
use rmcp::{Peer, RoleClient, ServiceExt};
use tower::ServiceExt as _;

const TEST_PEN: &str = "tester";
const TEST_SECRET: &str = "test-secret-proves-nothing";
const TEST_FROM: &str = "X-Stet-From";
const TEST_AUTH: &str = "X-Stet-Auth";

fn session(dir: &std::path::Path) -> Session {
    let paths = Paths::from_root(dir);
    paths.ensure().expect("create store root");
    provision_write_identity(&paths);
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

fn provision_write_identity(paths: &Paths) {
    let conn = ellmephant::db::open(paths).expect("open");
    conn.execute(
        "INSERT OR IGNORE INTO pen (name, kind, parent_id)
         VALUES (?1, 'self', (SELECT id FROM pen WHERE name = 'estate'))",
        [TEST_PEN],
    )
    .expect("seed pen");
    conn.execute(
        "INSERT OR REPLACE INTO pen_credential (pen_id, hash, issued_by)
         SELECT id, ?2, NULL FROM pen WHERE name = ?1",
        rusqlite::params![TEST_PEN, ellmephant::store::credential::digest(TEST_SECRET)],
    )
    .expect("seed credential");
}

fn router(server: MemoryServer, state: HttpState) -> axum::Router {
    http_router(server, "127.0.0.1", state)
}

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

#[tokio::test]
async fn a_queued_document_is_load_and_not_a_fault() {
    let dir = tempfile::tempdir().unwrap();
    let app = app_with_token(dir.path(), "tok");
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

async fn request(
    app: &axum::Router,
    method: Method,
    uri: &str,
    token: Option<&str>,
    content_type: Option<&str>,
    body: &str,
) -> (StatusCode, String, Option<String>) {
    let is_write = matches!(
        method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    );
    let mut builder = Request::builder().method(method).uri(uri);
    if is_write {
        builder = builder
            .header(TEST_FROM, TEST_PEN)
            .header(TEST_AUTH, TEST_SECRET);
    }
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

async fn mcp_peer(server: MemoryServer) -> rmcp::service::RunningService<RoleClient, ()> {
    mcp_peer_as(server, "tester").await
}

async fn mcp_peer_as(
    server: MemoryServer,
    pen: &str,
) -> rmcp::service::RunningService<RoleClient, ()> {
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

const DIM: usize = 64;
const HASH_MODEL: &str = "fake-hash-v1";

fn settle(dir: &std::path::Path) {
    ellmephant::index::vec::register_extension();
    let conn = ellmephant::db::open(&Paths::from_root(dir)).expect("open");
    let embedder = HashEmbedder::new(DIM);
    let _spec = ellmephant::embed::VecSpec::of(&embedder);
    while ellmephant::store::encode::drain(&conn, &embedder, 64).expect("drain") > 0 {}
}

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

#[tokio::test]
async fn the_dupes_plan_flag_is_a_door() {
    let dir = tempfile::tempdir().unwrap();
    let app = app_with_token(dir.path(), "tok");
    let body = "the turn boundary is offered once per wake, and a miss is not a failure.";
    seed(&app, "tok", "aaa-plan", "Same thing twice", body).await;
    seed(&app, "tok", "bbb-plan", "Same thing twice", body).await;
    settle(dir.path());

    let (status, body, _) = get(&app, "/dupes?plan=1&max_distance=1&format=json").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let plan: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(
        plan["cols"],
        serde_json::json!(["keep", "absorb", "reason"]),
        "the plan table is self-describing: {plan}"
    );
    let steps = plan["plan"].as_array().expect("plan");
    assert_eq!(steps.len(), 1, "one duplicate pair, one step: {plan}");
    assert_eq!(steps[0][0], serde_json::json!("aaa-plan"), "{plan}");
    assert_eq!(steps[0][1], serde_json::json!("bbb-plan"), "{plan}");

    let (status, body, _) = get(&app, "/dupes?max_distance=1&format=json").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("\"class\""), "{body}");
}

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
                .header(TEST_FROM, TEST_PEN)
                .header(TEST_AUTH, TEST_SECRET)
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

    let gated = app_with_token(dir.path(), "tok");
    let (_, boot_open, _) = get(&app, "/bootstrap").await;
    let (_, boot_gated, _) = get(&gated, "/bootstrap").await;
    assert_eq!(
        without_queue_age(&boot_open),
        without_queue_age(&boot_gated),
        "a boot must not depend on the auth config"
    );
}

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

    assert!(
        doc.lines().next().unwrap().contains("your memory"),
        "the first line must say what this endpoint is: {doc}"
    );
    assert!(
        doc.contains("HOW TO GET AT IT") && doc.contains("GET /search?q="),
        "the reader is told how to fetch memories: {doc}"
    );

    assert!(
        doc.contains("BOOT: read me first, then go"),
        "the boot note's body must be resident: {doc}"
    );
    assert!(
        doc.contains("memory `bootstrap`"),
        "and named, so it can be fetched again: {doc}"
    );

    assert!(
        doc.contains("no self set") && doc.contains("proved no pen"),
        "an unproven caller is told it has no self set rather than being given one: {doc}"
    );
    assert!(
        !doc.contains(secret) && !doc.contains("STET ONLY BODY"),
        "another agent's identity must never be served: {doc}"
    );

    assert!(
        !doc.contains("args:") && !doc.contains("inputSchema"),
        "the default boot carries no tool schemas: {doc}"
    );
    assert!(
        doc.contains("?tools=true"),
        "and it says where they are: {doc}"
    );
}

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

#[tokio::test]
async fn bootstrap_stays_inside_its_budget() {
    let dir = tempfile::tempdir().unwrap();
    let app = app_with_token(dir.path(), "tok");
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
    assert!(doc.contains("resident prose."), "{doc}");
    assert!(
        doc.contains("no self set"),
        "an unproven caller is told so: {doc}"
    );
}

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

    let (_, body, _) = get(&app, "/bootstrap?format=json&tools=true").await;
    let doc: serde_json::Value = serde_json::from_str(&body).unwrap();
    let tools = doc["tools"].as_array().expect("tools with ?tools=true");
    assert_eq!(tools.len(), ellmephant::mcp::TOOLS.len());

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

#[tokio::test]
async fn boot_text_renders_arguments_from_the_schema() {
    let dir = tempfile::tempdir().unwrap();
    let app = app_with_token(dir.path(), "tok");
    let (status, doc, _) = get(&app, "/bootstrap?tools=true").await;
    assert_eq!(status, StatusCode::OK, "{doc}");

    assert!(doc.contains("k: integer (optional)"), "{doc}");
    assert!(
        doc.contains("mode: \"literal\"|\"regex\" (optional)"),
        "{doc}"
    );
    assert!(
        doc.contains("(\"key\"|\"via\"|\"title\"|\"tags\"|\"chars\"|\"distance\"|\"score\")[]"),
        "{doc}"
    );
    assert!(doc.contains("key: string (required)"), "{doc}");
    assert!(doc.contains("tags: boolean (optional)"), "{doc}");
}

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

#[tokio::test]
async fn write_routes_accept_the_mcp_arguments_as_a_body() {
    let dir = tempfile::tempdir().unwrap();
    let app = app_with_token(dir.path(), "tok");
    seed(&app, "tok", "mb", "a body", "text").await;

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

#[tokio::test]
async fn upvote_route_honours_the_mcp_batch_body() {
    let dir = tempfile::tempdir().unwrap();
    let app = app_with_token(dir.path(), "tok");
    seed(&app, "tok", "uv-a", "a", "a").await;
    seed(&app, "tok", "uv-b", "b", "b").await;

    let times_useful = |body: &str| -> serde_json::Value {
        serde_json::from_str::<serde_json::Value>(body).unwrap()["times_useful"].clone()
    };

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

    let (status, body, _) = get(&app, "/search?q=words%20worth%20finding&format=json").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("idx-a"), "{body}");

    {
        ellmephant::index::vec::register_extension();
        let conn = rusqlite::Connection::open(Paths::from_root(dir.path()).db()).unwrap();
        ellmephant::index::vec::clear(&conn).unwrap();
    }

    let (status, body, _) = get(&app, "/search?q=words%20worth%20finding&format=json").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(body.contains("index unavailable"), "{body}");

    let empty = tempfile::tempdir().unwrap();
    let app = app_with_token(empty.path(), "tok");
    let (status, body, _) = get(&app, "/search?q=anything&format=json").await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

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

    let (status, body, _) = get(&app, "/search?q=words%20worth%20finding&format=json").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("waited"), "{body}");
}

#[tokio::test]
async fn wait_true_reports_a_write_that_could_not_be_indexed() {
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

    let dir = tempfile::tempdir().unwrap();
    let watch = Arc::new(LockWatch::new());
    let paths = Paths::from_root(dir.path());
    let cfg = Config::default();
    let spec = Spec::from_config(&cfg, &paths);
    provision_write_identity(&paths);
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

    let (status, _, _) = get(&app, "/memory/unindexable?format=json").await;
    assert_eq!(status, StatusCode::OK, "the row is written regardless");
}

#[tokio::test]
async fn wait_true_holds_the_lock_and_health_names_the_holder() {
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
    provision_write_identity(&paths);
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

#[tokio::test]
async fn history_is_reachable_by_address_after_the_row_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let app = app_open(dir.path());
    seed(&app, "", "kept", "Kept", "first state").await;
    seed(&app, "", "kept", "Kept", "second state").await;

    let peer = mcp_peer(MemoryServer::with_watch(
        Arc::new(Mutex::new(session(dir.path()))),
        Arc::new(LockWatch::new()),
    ))
    .await;

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

    let (status, body, _) = request(&app, Method::GET, "/history?key=kept", None, None, "").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("\"count\":1"), "{body}");
    let (status, body, _) =
        request(&app, Method::GET, "/memory/kept?version=1", None, None, "").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("first state"), "{body}");

    let receipt: serde_json::Value = serde_json::from_str(
        &tool(&peer, "memory_delete", serde_json::json!({ "key": "kept" })).await,
    )
    .expect("delete json");
    assert_eq!(receipt["status"], "removed");
    assert_eq!(receipt["versions_remaining"], 2);

    let (status, _, _) = request(&app, Method::GET, "/memory/kept", None, None, "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, body, _) =
        request(&app, Method::GET, "/memory/kept?version=1", None, None, "").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("first state"), "{body}");
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

    let (status, body, _) = get(&app, "/claim?format=json").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["claims"][0]["name"], serde_json::json!("bank"));

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

#[tokio::test]
async fn a_refused_cas_write_answers_conflict_not_updated() {
    let dir = tempfile::tempdir().unwrap();
    let app = app_with_token(dir.path(), "tok");
    seed(&app, "tok", "cas-probe", "one sharp line", "v1").await;
    settle(dir.path());

    let (_, body, _) = get(&app, "/memory/cas-probe?format=json").await;
    let row: serde_json::Value = serde_json::from_str(&body).unwrap();
    let version = row["version"].as_i64().expect("the row has a version");

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

    let (_, body, _) = get(&app, "/memory/cas-probe?format=json").await;
    let row: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(row["version"], version, "nothing was written");
    assert_eq!(row["body"], "v1");

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

    let (status, _, _) = get(&app, "/mail/1").await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "an anonymous letter read must be refused too"
    );

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

    let got = tool(service.peer(), "memory_get", json!({ "key": "mine" })).await;
    let doc: serde_json::Value = serde_json::from_str(&got).unwrap();
    assert_eq!(doc["owner"], json!("tester"), "{got}");
    assert_eq!(doc["owner_relation"], json!("ME"), "{got}");

    let (status, body, _) = get(&app, "/memory/mine").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("owner   tester"), "{body}");
    assert!(
        !body.contains("ME"),
        "an anonymous reader has no walk: {body}"
    );
}

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

    let (status, body, _) = get(&app, "/whois/tester?format=json").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let over_http: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(over_http["owns"], json!(1), "{body}");
    assert!(
        over_http["relation"].is_null(),
        "an unproven reader gets no walk: {body}"
    );

    let (status, _, _) = get(&app, "/whois/nobody-at-all").await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "an unknown pen is a refusal"
    );
}

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
    assert_eq!(
        doc["name"],
        json!("tester/finger-9f3a1c"),
        "the minted name"
    );
    assert_eq!(doc["kind"], json!("finger"), "the minted kind");
    assert_eq!(doc["parent"], json!("tester"), "the caller is the parent");
    assert_eq!(
        doc["secret"].as_str().map(str::len),
        Some(64),
        "the child's secret is returned ONCE"
    );

    let seen = tool(
        service.peer(),
        "whois",
        json!({ "pen": "tester/finger-9f3a1c" }),
    )
    .await;
    let seen: serde_json::Value = serde_json::from_str(&seen).unwrap();
    assert_eq!(seen["relation"], json!("C \"child\""), "{seen}");

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

    let refused = tool(
        service.peer(),
        "memory_upsert",
        json!({ "key": "finger-work", "title": "Finger work", "body": "b", "as": "tester/f-1" }),
    )
    .await;
    assert!(refused.contains("LIVE lend"), "{refused}");

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

fn identity_fixture(paths: &Paths) {
    let conn = ellmephant::db::open(paths).unwrap();
    for (name, secret) in [
        ("http-as-tester", "as-secret-1"),
        ("http-as-gated", "as-secret-g"),
    ] {
        conn.execute(
            "INSERT OR IGNORE INTO pen (name, kind) VALUES (?1, 'self')",
            [name],
        )
        .unwrap();
        if ellmephant::store::credential::verify(&conn, name, secret)
            .unwrap()
            .is_none()
        {
            let hash = ellmephant::store::credential::digest(secret);
            conn.execute(
                "INSERT OR REPLACE INTO pen_credential (pen_id, hash)
                 VALUES ((SELECT id FROM pen WHERE name = ?1), ?2)",
                [name, hash.as_str()],
            )
            .unwrap();
        }
    }
}

#[tokio::test]
async fn the_http_write_accepts_an_as_claim_but_only_with_a_live_lend() {
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let app = app_with_token(dir.path(), "tok");

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

    identity_fixture(&paths);

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

    let refused = tool(
        service.peer(),
        "sudo",
        json!({ "powerful": "estate", "secs": 60 }),
    )
    .await;
    assert!(refused.contains("no sudo policy"), "{refused}");

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

    let too_long = tool(
        service.peer(),
        "sudo",
        json!({ "powerful": "estate", "secs": 601 }),
    )
    .await;
    assert!(too_long.contains("at most 600s"), "{too_long}");

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

#[tokio::test]
async fn a_pen_behind_a_gated_channel_cannot_change_the_record() {
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::from_root(dir.path());
    identity_fixture(&paths);
    let app = app_with_token(dir.path(), "tok");

    let seq;
    {
        let conn = ellmephant::db::open(&paths).unwrap();
        conn.execute_batch(
            "INSERT OR IGNORE INTO pen (name, kind) VALUES ('http-as-gated', 'self');
             INSERT OR IGNORE INTO pen (name, kind) VALUES ('james', 'self');",
        )
        .unwrap();
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
        ellmephant::store::channel::present(&conn, "http-as-gated", None).unwrap();
        ellmephant::store::channel::present(&conn, "http-as-gated", None).unwrap();
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

#[tokio::test]
async fn the_gate_gives_the_same_verdict_on_both_doors() {
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::from_root(dir.path());
    identity_fixture(&paths);
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

    {
        let paths = Paths::from_root(dir.path());
        let conn = ellmephant::db::open(&paths).unwrap();
        conn.execute_batch(
            "INSERT OR IGNORE INTO pen (name, kind) VALUES ('http-as-gated', 'self');
             INSERT OR IGNORE INTO pen (name, kind) VALUES ('james', 'self');",
        )
        .unwrap();
        use ellmephant::store::channel as ch;
        ch::invite(&conn, "james", "http-as-gated", "ops_team").unwrap();
        ch::invite(&conn, "james", "james", "ops_team").unwrap();
        let m = ch::send(&conn, "james", "ops_team", "the cluster is on fire", None).unwrap();
        ch::present(&conn, "http-as-gated", None).unwrap();
        ch::present(&conn, "http-as-gated", None).unwrap();
        conn.execute(
            "UPDATE channel_delivery SET presented_at = datetime('now','-5 minutes')
              WHERE seq = ?1 AND pen_id = (SELECT id FROM pen WHERE name='http-as-gated')",
            [m.seq],
        )
        .unwrap();
        let b = ch::breach(&conn, "http-as-gated").unwrap();
        assert!(b.is_some(), "the fixture must actually gate the pen: {b:?}");
    }

    let mcp_verdict = tool(
        service.peer(),
        "memory_upsert",
        json!({ "key": "through-mcp", "title": "t", "body": "b" }),
    )
    .await;

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

#[tokio::test]
async fn a_locked_pen_may_rebuild_but_may_not_prune() {
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::from_root(dir.path());
    identity_fixture(&paths);
    let app = app_with_token(dir.path(), "tok");

    {
        let conn = ellmephant::db::open(&paths).unwrap();
        conn.execute_batch(
            "INSERT OR IGNORE INTO pen (name, kind) VALUES ('http-as-gated', 'self');
             INSERT OR IGNORE INTO pen (name, kind) VALUES ('james', 'self');",
        )
        .unwrap();
        ellmephant::store::channel::invite(&conn, "james", "http-as-gated", "ops_team").unwrap();
        ellmephant::store::channel::invite(&conn, "james", "james", "ops_team").unwrap();
        ellmephant::store::channel::send(
            &conn,
            "james",
            "ops_team",
            "the cluster is on fire",
            None,
        )
        .expect("send");
        ellmephant::store::channel::present(&conn, "http-as-gated", None).unwrap();
        ellmephant::store::channel::present(&conn, "http-as-gated", None).unwrap();
        conn.execute(
            "UPDATE channel_delivery SET presented_at = datetime('now','-5 minutes')
              WHERE pen_id = (SELECT id FROM pen WHERE name = 'http-as-gated')",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE channel SET keep_secs = 3600 WHERE key = 'ops_team'",
            [],
        )
        .unwrap();
        conn.execute("UPDATE channel SET gate = 'all' WHERE key = 'ops_team'", [])
            .unwrap();
    }

    let post = |path: &'static str| {
        let app = app.clone();
        async move {
            let req = Request::builder()
                .method(Method::POST)
                .uri(path)
                .header(header::AUTHORIZATION, "Bearer tok")
                .header(header::CONTENT_TYPE, "application/json")
                .header("X-Stet-From", "http-as-gated")
                .header("X-Stet-Auth", "as-secret-g")
                .body(Body::from(json!({}).to_string()))
                .unwrap();
            let res = app.oneshot(req).await.unwrap();
            let status = res.status();
            let bytes = to_bytes(res.into_body(), 64 * 1024).await.unwrap();
            (status, String::from_utf8_lossy(&bytes).to_string())
        }
    };

    let (status, body) = post("/memory").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "the pen is locked: {body}");
    assert!(
        body.contains("not been fetched"),
        "and for the reason we think: {body}"
    );

    for path in ["/reparse", "/reembed"] {
        let (status, body) = post(path).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{path} is maintenance, not an act: {status} {body}"
        );
    }

    let (status, body) = post("/channels/prune").await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "prune is acting, and the pen is locked: {status} {body}"
    );
    assert!(
        body.contains("ops_team") && body.contains("not been fetched"),
        "the refusal names the channel and the reason: {body}"
    );
}

#[tokio::test]
async fn pen_rotate_returns_a_new_secret_and_the_old_one_dies() {
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let app = app_with_token(dir.path(), "tok");
    let paths = Paths::from_root(dir.path());
    let old = {
        let conn = ellmephant::db::open(&paths).unwrap();
        conn.execute(
            "INSERT INTO pen (name, kind, parent_id)
             VALUES ('rotate-tester', 'self', (SELECT id FROM pen WHERE name = 'estate'))",
            [],
        )
        .unwrap();
        ellmephant::store::credential::issue(&conn, "rotate-tester", None).unwrap()
    };

    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/pen/rotate?format=json")
                .header(header::AUTHORIZATION, "Bearer tok")
                .header("X-Stet-From", "rotate-tester")
                .header("X-Stet-Auth", &old)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let bytes = to_bytes(res.into_body(), 64 * 1024).await.unwrap();
    let body = String::from_utf8_lossy(&bytes).to_string();
    assert_eq!(status, StatusCode::OK, "{status} {body}");
    let doc: serde_json::Value = serde_json::from_str(&body).unwrap();
    let fresh = doc["secret"].as_str().unwrap().to_string();
    assert_ne!(fresh, old, "a rotation is a new secret");
    assert_eq!(doc["pen"], json!("rotate-tester"));

    let whoami = |secret: String| {
        let app = app.clone();
        async move {
            let res = app
                .oneshot(
                    Request::builder()
                        .uri("/whoami")
                        .header("X-Stet-From", "rotate-tester")
                        .header("X-Stet-Auth", secret)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let bytes = to_bytes(res.into_body(), 64 * 1024).await.unwrap();
            String::from_utf8_lossy(&bytes).to_string()
        }
    };
    assert!(
        whoami(fresh).await.contains("\"proven\":true"),
        "the new secret proves"
    );
    assert!(
        whoami(old).await.contains("\"proven\":false"),
        "the old secret is dead on the NEXT call"
    );
}

#[tokio::test]
async fn api_index_names_the_rest_of_the_surface() {
    let dir = tempfile::tempdir().unwrap();
    let app = app_open(dir.path());

    let (status, body, _) = get(&app, "/api?format=json").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let doc: serde_json::Value = serde_json::from_str(&body).unwrap();
    let ops = doc["ops"].as_array().expect("ops");
    assert_eq!(
        ops.len(),
        ellmephant::http_surface::ROUTES.len(),
        "every REST route is in the index"
    );
    assert!(
        ops.iter().all(|op| op.get("schema").is_none()),
        "the default index is lean"
    );
    assert!(
        ops.iter().any(|op| op["group"] == "hygiene"),
        "hygiene is the group the core door leaves out"
    );

    let (status, body, _) = get(&app, "/api?format=json&full=true").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let full: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(
        full["ops"]
            .as_array()
            .unwrap()
            .iter()
            .all(|op| op.get("schema").is_some()),
        "?full=true carries the schemas"
    );

    let (status, body, _) = get(&app, "/api/hygiene?format=json").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let group: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(
        group["ops"]
            .as_array()
            .unwrap()
            .iter()
            .all(|op| op["group"] == "hygiene")
    );

    let (status, _, _) = get(&app, "/api/nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
