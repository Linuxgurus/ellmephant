//! `GET /health` tests — the read-only promise and the writer witness. In-process over
//! `oneshot`, with the deterministic `HashEmbedder`, so no weights and no network.

use std::sync::{Arc, Mutex};
use std::time::Instant;

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
    let cfg = Config::default();
    let spec = Spec::from_config(&cfg, &paths);
    Session::open(paths, cfg, spec)
        .expect("open store")
        .with_embedder(Box::new(HashEmbedder::new(64)))
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

/// Read purity: `GET /memory/{key}` never bumps `visits`; the counter is a separate write.
#[tokio::test]
async fn reading_a_memory_does_not_count_a_visit() {
    let dir = tempfile::tempdir().unwrap();
    let app = app_with_token(dir.path(), "tok");
    seed(&app, "tok", "pure", "one sharp line", "the body").await;

    let (_, first, _) = get(&app, "/memory/pure?format=json").await;
    let first: serde_json::Value = serde_json::from_str(&first).unwrap();
    assert_eq!(first["visits"], 0, "a read must not count");

    let (_, second, _) = get(&app, "/memory/pure?format=json").await;
    let second: serde_json::Value = serde_json::from_str(&second).unwrap();
    assert_eq!(second["visits"], 0, "reading twice still counts nothing");

    // The write, where it belongs, is the only thing that moves it.
    let (status, body, _) = request(
        &app,
        Method::POST,
        "/memory/pure/visit?format=json",
        Some("tok"),
        None,
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let counted: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(counted["visits"], 1, "{body}");
}

/// Auth before writes: every write route refuses a bare token and a wrong one, with a bare challenge.
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

/// `/bootstrap` shape: exactly one BOOT NOTE body, a non-resident memory shows its title and not
/// its body, and both `writer` and `ROUTES` are on the first screen.
#[tokio::test]
async fn bootstrap_is_a_view_not_a_dump() {
    let dir = tempfile::tempdir().unwrap();
    let app = app_with_token(dir.path(), "tok");
    let note_body = "BOOT: read me first, then go";
    let (status, body, _) = request(
        &app,
        Method::POST,
        "/memory",
        Some("tok"),
        Some("text/plain"),
        &format!("Key: bootstrap-self\nTitle: boot me\nTags: cline-self\n---\n{note_body}\n"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let secret = "this body must not be resident";
    let (status, body, _) = request(
        &app,
        Method::POST,
        "/memory",
        Some("tok"),
        Some("text/plain"),
        &format!(
            "Key: other-self\nTitle: another cline-self key\nTags: cline-self\n---\n{secret}\n"
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, doc, content_type) = get(&app, "/bootstrap").await;
    assert_eq!(status, StatusCode::OK, "{doc}");
    assert!(content_type.unwrap().starts_with("text/plain"));
    assert_eq!(doc.matches("BOOT NOTE").count(), 1, "one body resident");
    assert!(doc.contains(note_body), "the boot note body is resident");
    assert!(doc.contains("writer"), "{doc}");
    assert!(doc.contains("ROUTES"), "{doc}");
    assert!(
        doc.contains("another cline-self key"),
        "the non-resident key shows its title"
    );
    assert!(!doc.contains(secret), "and not its body");
}

/// `/bootstrap` is read-only: it changes no `visits` and no counts.
#[tokio::test]
async fn bootstrap_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let app = app_with_token(dir.path(), "tok");
    seed(&app, "tok", "still", "unchanged", "no visits here").await;

    let (_, before, _) = get(&app, "/memory/still?format=json").await;
    let (status, _, _) = get(&app, "/bootstrap").await;
    assert_eq!(status, StatusCode::OK);
    let (_, after, _) = get(&app, "/memory/still?format=json").await;
    assert_eq!(before, after, "/bootstrap changed the store");
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

    // `memory_get` is the one read that differs, and deliberately: MCP counts the visit, HTTP must
    // not. Everything else is identical.
    let (_, http_get, _) = get(&app, "/memory/alpha?format=json").await;
    let http_get: serde_json::Value = serde_json::from_str(&http_get).unwrap();
    let mcp_get: serde_json::Value =
        serde_json::from_str(&tool(service.peer(), "memory_get", json!({ "key": "alpha" })).await)
            .unwrap();
    assert_eq!(http_get["visits"], 0, "HTTP stays pure");
    assert_eq!(mcp_get["visits"], 1, "MCP counts the read");
    assert_eq!(http_get["key"], mcp_get["key"]);
    assert_eq!(http_get["body"], mcp_get["body"]);
    assert_eq!(http_get["title"], mcp_get["title"]);
}
