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

/// A read counts a visit — on **both** doors. Telemetry is the one write a `GET` may make: a counter
/// is not data, so counting does not make a read unsafely repeatable, and `memory_get` has always done
/// it. There is no separate counter route to call: MCP has no such tool, and `project_get` counts
/// nothing on either door.
#[tokio::test]
async fn reading_a_memory_counts_a_visit() {
    let dir = tempfile::tempdir().unwrap();
    let app = app_with_token(dir.path(), "tok");
    seed(&app, "tok", "counted", "one sharp line", "the body").await;

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

/// `/bootstrap` is the MCP boot and **nothing more**: the server identity, the instruction text, and
/// every tool with an HTTP verb in place of its MCP verb. No resident set, no counts, no route
/// index — a boot is the *API surface*, not a view of the store's contents.
#[tokio::test]
async fn bootstrap_is_the_mcp_boot_and_nothing_more() {
    let dir = tempfile::tempdir().unwrap();
    let app = app_with_token(dir.path(), "tok");
    // Seed exactly the content the OLD boot inlined, to prove the boot no longer reads the store.
    seed(
        &app,
        "tok",
        "bootstrap-self",
        "boot me",
        "BOOT: read me first, then go",
    )
    .await;
    let secret = "this body must not be resident";
    seed(&app, "tok", "other-self", "another cline-self key", secret).await;

    let (status, doc, content_type) = get(&app, "/bootstrap").await;
    assert_eq!(status, StatusCode::OK, "{doc}");
    assert!(content_type.unwrap().starts_with("text/plain"));

    // The handshake, and only the handshake.
    assert!(
        doc.contains("INSTRUCTIONS"),
        "the standing law rides the boot"
    );
    assert!(doc.contains("smart-caveman"), "and it is the MCP text");
    assert!(
        doc.contains(&format!("TOOLS — {}", ellmephant::mcp::TOOLS.len())),
        "the tool list is complete: {doc}"
    );

    // Every tool, named by the HTTP verb that reaches it rather than by its MCP tool name.
    for route in ellmephant::http_surface::ROUTES {
        assert!(
            doc.contains(&format!("{} {}", route.method, route.path)),
            "`{} {}` is missing from the boot",
            route.method,
            route.path
        );
    }

    // NOTHING MORE. Every section the boot used to compose is gone.
    for gone in [
        "SELF (tag",
        "BOOT NOTE",
        "OPEN (tag",
        "ACTIVE PROJECTS",
        "HOT —",
        "ROUTES",
    ] {
        assert!(
            !doc.contains(gone),
            "`{gone}` is not part of an MCP boot: {doc}"
        );
    }
    assert!(
        !doc.contains("bootstrap-self") && !doc.contains(secret),
        "a boot is built from code, not read from the store: {doc}"
    );
}

/// `/bootstrap?format=json` is the MCP handshake in MCP's own shape: `initialize`'s fields plus
/// `tools/list`'s, with `method`/`path` where MCP carries `name` and every other field untouched.
#[tokio::test]
async fn bootstrap_json_is_the_mcp_handshake() {
    let dir = tempfile::tempdir().unwrap();
    let app = app_with_token(dir.path(), "tok");

    let (status, body, _) = get(&app, "/bootstrap?format=json").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let doc: serde_json::Value = serde_json::from_str(&body).unwrap();

    // Exactly the handshake's fields, and no others — this is the "nothing more" assertion.
    let mut keys: Vec<&String> = doc.as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(
        keys,
        ["capabilities", "instructions", "serverInfo", "tools"],
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

    let tools = doc["tools"].as_array().expect("tools");
    assert_eq!(tools.len(), ellmephant::mcp::TOOLS.len());

    // Each card is the MCP card with the verb swapped in: same title, description, schema, hints.
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
/// call from the boot alone: a nullable property reports the type it carries, an enum reports its
/// values, and a required argument says so.
#[tokio::test]
async fn boot_text_renders_arguments_from_the_schema() {
    let dir = tempfile::tempdir().unwrap();
    let app = app_with_token(dir.path(), "tok");
    let (status, doc, _) = get(&app, "/bootstrap").await;
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
