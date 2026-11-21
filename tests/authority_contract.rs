use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode, header};
use ellmephant::config::{Config, Paths};
use ellmephant::embed::HashEmbedder;
use ellmephant::http::{HttpState, LockWatch};
use ellmephant::mcp::{MemoryServer, http_router};
use ellmephant::model::Spec;
use ellmephant::store::session::Session;
use rmcp::model::CallToolRequestParams;
use rmcp::{RoleClient, ServiceExt};
use tower::ServiceExt as _;

const DIM: usize = 64;
const HASH_MODEL: &str = "fake-hash-v1";
const TOKEN: &str = "door";
const PEN: &str = "tester";

fn session(dir: &std::path::Path) -> Session {
    let paths = Paths::from_root(dir);
    paths.ensure().expect("create store root");
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

fn provision(dir: &std::path::Path) -> String {
    let paths = Paths::from_root(dir);
    let conn = ellmephant::db::open(&paths).expect("open");
    conn.execute(
        "INSERT INTO pen (name, kind, parent_id)
         VALUES (?1, 'self', (SELECT id FROM pen WHERE name = 'estate'))",
        [PEN],
    )
    .expect("seed pen");
    ellmephant::store::credential::issue(&conn, PEN, None).expect("issue credential")
}

fn doors(dir: &std::path::Path) -> (axum::Router, MemoryServer) {
    let watch = Arc::new(LockWatch::new());
    let shared = Arc::new(Mutex::new(session(dir)));
    let mcp = MemoryServer::with_watch(Arc::clone(&shared), Arc::clone(&watch));
    let state = HttpState::new(
        Paths::from_root(dir),
        Config::default(),
        Arc::clone(&watch),
        Instant::now(),
    )
    .with_write_token(Some(TOKEN.to_string()));
    let app = http_router(
        MemoryServer::with_watch(Arc::clone(&shared), Arc::clone(&watch)),
        "127.0.0.1",
        state,
    );
    (app, mcp)
}

async fn request(
    app: &axum::Router,
    method: Method,
    uri: &str,
    identity: Option<(&str, &str)>,
    content_type: Option<&str>,
    body: &str,
) -> StatusCode {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"));
    if let Some((from, auth)) = identity {
        builder = builder
            .header("X-Stet-From", from)
            .header("X-Stet-Auth", auth);
    }
    if let Some(content_type) = content_type {
        builder = builder.header(header::CONTENT_TYPE, content_type);
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::from(body.to_string())).unwrap())
        .await
        .expect("http request");
    let status = response.status();
    let _ = to_bytes(response.into_body(), usize::MAX).await;
    status
}

async fn peer(
    server: MemoryServer,
    declared: Option<&str>,
) -> rmcp::service::RunningService<RoleClient, ()> {
    let server = match declared {
        Some(pen) => server.with_identity(pen),
        None => server,
    };
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

async fn mcp_answer(
    server: MemoryServer,
    declared: Option<&str>,
    tool: &'static str,
    args: serde_json::Value,
) -> (bool, String) {
    let service = peer(server, declared).await;
    let result = service
        .peer()
        .call_tool(
            CallToolRequestParams::new(tool)
                .with_arguments(args.as_object().expect("object args").clone()),
        )
        .await
        .expect("tool call");
    let refused = result.is_error.unwrap_or(false);
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap_or_default();
    (refused, text)
}

async fn mcp_post(
    app: &axum::Router,
    session: Option<&str>,
    identity: Option<(&str, &str)>,
    body: serde_json::Value,
) -> (StatusCode, Option<String>, String) {
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri("/mcp")
        .header(header::HOST, "127.0.0.1")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json, text/event-stream");
    if let Some(id) = session {
        builder = builder.header("mcp-session-id", id);
    }
    if let Some((from, auth)) = identity {
        builder = builder
            .header("X-Stet-From", from)
            .header("X-Stet-Auth", auth);
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::from(body.to_string())).unwrap())
        .await
        .expect("http request");
    let status = response.status();
    let session = response
        .headers()
        .get("mcp-session-id")
        .map(|v| v.to_str().unwrap().to_string());
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (
        status,
        session,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

fn payload(body: &str) -> serde_json::Value {
    for frame in body
        .lines()
        .filter_map(|line| line.strip_prefix("data:").map(str::trim))
        .filter(|frame| !frame.is_empty())
    {
        if let Ok(value) = serde_json::from_str(frame) {
            return value;
        }
    }
    serde_json::from_str(body).unwrap_or_else(|e| panic!("not JSON-RPC: {e}: {body}"))
}

async fn mcp_session(app: &axum::Router) -> String {
    let (status, session, body) = mcp_post(
        app,
        None,
        None,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": { "name": "authority-contract", "version": "1.0" }
            }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let session = session.expect("Mcp-Session-Id header");
    let (status, _, _) = mcp_post(
        app,
        Some(&session),
        None,
        serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .await;
    assert!(status.is_success(), "{status}");
    session
}

async fn mcp_over_http(
    app: &axum::Router,
    session: &str,
    identity: Option<(&str, &str)>,
    tool: &str,
    args: &serde_json::Value,
) -> (bool, String) {
    let (status, _, body) = mcp_post(
        app,
        Some(session),
        identity,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": { "name": tool, "arguments": args },
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "tools/call is a 200: {body}");
    let payload = payload(&body);
    let result = &payload["result"];
    let refused = result["isError"].as_bool().unwrap_or(false);
    let text = result["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    (refused, text)
}

const MISSING: &str = "a write requires a proven identity";

struct Case {
    tool: &'static str,
    args: serde_json::Value,
    method: fn() -> Method,
    uri: &'static str,
    content_type: Option<&'static str>,
    body: &'static str,
}

fn cases() -> Vec<Case> {
    let upsert_body = "Key: probe\nTitle: Probe\n---\nthe probe body\n";
    vec![
        Case {
            tool: "memory_upsert",
            args: serde_json::json!({ "key": "probe", "title": "Probe", "body": "the probe body" }),
            method: || Method::POST,
            uri: "/memory",
            content_type: Some("text/plain"),
            body: upsert_body,
        },
        Case {
            tool: "memory_delete",
            args: serde_json::json!({ "key": "alpha" }),
            method: || Method::DELETE,
            uri: "/memory/alpha",
            content_type: None,
            body: "",
        },
        Case {
            tool: "memory_append",
            args: serde_json::json!({ "key": "alpha", "line": "one more fact" }),
            method: || Method::POST,
            uri: "/memory/alpha/append?line=one%20more%20fact",
            content_type: None,
            body: "",
        },
        Case {
            tool: "memory_merge",
            args: serde_json::json!({ "from": "alpha", "into": "beta" }),
            method: || Method::POST,
            uri: "/memory/alpha/merge?into=beta",
            content_type: None,
            body: "",
        },
        Case {
            tool: "memory_fold",
            args: serde_json::json!({ "from": "alpha", "into": "beta" }),
            method: || Method::POST,
            uri: "/memory/alpha/fold?into=beta",
            content_type: None,
            body: "",
        },
        Case {
            tool: "project_upsert",
            args: serde_json::json!({ "key": "probe-project", "title": "Probe project", "status": "active" }),
            method: || Method::POST,
            uri: "/project",
            content_type: Some("application/json"),
            body: r#"{"key":"probe-project","title":"Probe project","status":"active"}"#,
        },
    ]
}

async fn seed(app: &axum::Router, secret: &str) {
    for (key, title) in [("alpha", "Alpha"), ("beta", "Beta")] {
        let body = format!("Key: {key}\nTitle: {title}\n---\nthe body of {key}\n");
        let status = request(
            app,
            Method::POST,
            "/memory",
            Some((PEN, secret)),
            Some("text/plain"),
            &body,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "seed {key}");
    }
}

#[tokio::test]
async fn a_valid_pen_writes_on_both_doors() {
    for case in cases() {
        let http_dir = tempfile::tempdir().unwrap();
        let secret = provision(http_dir.path());
        let (app, _mcp) = doors(http_dir.path());
        seed(&app, &secret).await;
        let http = request(
            &app,
            (case.method)(),
            case.uri,
            Some((PEN, &secret)),
            case.content_type,
            case.body,
        )
        .await;
        assert!(
            http.is_success(),
            "HTTP {} as a proven pen should be accepted, got {http}",
            case.tool
        );

        let mcp_dir = tempfile::tempdir().unwrap();
        let secret = provision(mcp_dir.path());
        let (app, mcp) = doors(mcp_dir.path());
        seed(&app, &secret).await;
        let (refused, text) = mcp_answer(mcp, Some(PEN), case.tool, case.args.clone()).await;
        assert!(
            !refused,
            "MCP {} as a proven pen should be accepted: {text}",
            case.tool
        );

        let http_dir = tempfile::tempdir().unwrap();
        let secret = provision(http_dir.path());
        let (app, _mcp) = doors(http_dir.path());
        seed(&app, &secret).await;
        let session = mcp_session(&app).await;
        let (refused, text) =
            mcp_over_http(&app, &session, Some((PEN, &secret)), case.tool, &case.args).await;
        assert!(
            !refused,
            "MCP-over-HTTP {} as a proven pen should be accepted: {text}",
            case.tool
        );
    }
}

#[tokio::test]
async fn http_refuses_an_unproven_writer_with_401() {
    for case in cases() {
        let dir = tempfile::tempdir().unwrap();
        let secret = provision(dir.path());
        let (app, _mcp) = doors(dir.path());
        seed(&app, &secret).await;

        let status = request(
            &app,
            (case.method)(),
            case.uri,
            None,
            case.content_type,
            case.body,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "HTTP {} must refuse a caller with NO identity at the authority gate (401), not later \
             and not by accident — the store would otherwise stamp `anonymous`, own nothing, and \
             skip the channel gate",
            case.tool
        );
    }
}

#[tokio::test]
async fn mcp_refuses_an_unproven_writer_by_name() {
    for case in cases() {
        let dir = tempfile::tempdir().unwrap();
        let secret = provision(dir.path());
        let (app, mcp) = doors(dir.path());
        seed(&app, &secret).await;

        let (refused, text) = mcp_answer(mcp, None, case.tool, case.args).await;
        assert!(
            refused,
            "MCP {} accepted a caller with NO identity: {text}",
            case.tool
        );
        assert!(
            text.contains(MISSING),
            "MCP {} refused, but not at the authority gate: {text}",
            case.tool
        );
    }
}

#[tokio::test]
async fn a_forged_writer_is_refused_on_both_doors() {
    for case in cases() {
        let dir = tempfile::tempdir().unwrap();
        let secret = provision(dir.path());
        let (app, _mcp) = doors(dir.path());
        seed(&app, &secret).await;

        let status = request(
            &app,
            (case.method)(),
            case.uri,
            Some((PEN, "not-the-secret")),
            case.content_type,
            case.body,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "HTTP {} accepted a FORGED identity — `.ok().flatten()` turned the lie into \
             `anonymous` and wrote the commons",
            case.tool
        );

        let session = mcp_session(&app).await;
        let (refused, text) = mcp_over_http(
            &app,
            &session,
            Some((PEN, "not-the-secret")),
            case.tool,
            &case.args,
        )
        .await;
        assert!(
            refused,
            "MCP {} accepted a caller whose proof is WRONG: {text}",
            case.tool
        );
        assert!(
            text.contains("x-stet-auth"),
            "MCP {} refused, but not by naming the failed proof: {text}",
            case.tool
        );
    }
}
