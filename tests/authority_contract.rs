//! THE TWO DOORS AGREE ON WHO MAY WRITE (soma/ellmephant#231, #232).
//!
//! The defect this exists to remove is not a bug in the store: it is that a write's *authority*
//! was decided twice, once per door, and the copies disagreed. On HTTP a pen was optional — four
//! (then six) handlers did `identity::resolve(...).ok().flatten()`, which collapses FORGED and
//! MISSING into `None`, so an unidentified caller wrote as `anonymous` (owner NULL, the commons,
//! and `channel_gate` skipped). On MCP the same verb refused both.
//!
//! So the contract is a table, and it is behaviour-level on purpose: whatever refactor moves the
//! code underneath, this test must still pass. For every write verb, and for each caller
//! `{no proof, forged proof, valid proof}`, the two doors must answer the same way.

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

/// Give the store a pen that can prove itself, and return its secret.
///
/// `tester` hangs from the root `estate` because a parentless pen cannot hold a credential — a
/// fixture that makes its pen a root is testing a state the code refuses.
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

/// The HTTP door and an MCP server over the SAME store, as `serve` wires them.
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

/// One request with the door's bearer, an optional identity pair, and an optional body.
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
    // Drain the body so the connection finishes whatever it queued.
    let status = response.status();
    let _ = to_bytes(response.into_body(), usize::MAX).await;
    status
}

/// An in-process MCP peer. `None` is a transport that declared no pen — the MCP shape of "no
/// headers". (A forged claim can only arrive over an HTTP transport, so it has no in-process twin.)
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

/// Call one write tool; `true` means the door accepted it.
async fn mcp_accepted(
    server: MemoryServer,
    declared: Option<&str>,
    tool: &'static str,
    args: serde_json::Value,
) -> bool {
    let service = peer(server, declared).await;
    let result = service
        .peer()
        .call_tool(
            CallToolRequestParams::new(tool)
                .with_arguments(args.as_object().expect("object args").clone()),
        )
        .await
        .expect("tool call");
    !result.is_error.unwrap_or(false)
}

/// One write verb, spelled both ways.
struct Case {
    /// The MCP tool name.
    tool: &'static str,
    /// The MCP arguments.
    args: serde_json::Value,
    /// The HTTP method.
    method: fn() -> Method,
    /// The HTTP path, with any query string.
    uri: &'static str,
    /// The HTTP content type, when the route wants a body.
    content_type: Option<&'static str>,
    /// The HTTP body (text front matter or JSON).
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

/// Seed the two rows the destructive verbs need, as a proven pen.
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
        // One store per door: the destructive verbs consume their target, and comparing "the same
        // answer" must not mean "the second door found the first door's leftovers".
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
        assert!(
            mcp_accepted(mcp, Some(PEN), case.tool, case.args).await,
            "MCP {} as a proven pen should be accepted",
            case.tool
        );
    }
}

/// ★ THE REGRESSION. Today this fails on six rows of the HTTP column: `upsert`, `memory_delete`,
/// `memory_append`, `memory_merge`, `memory_fold` and `project_upsert` reach the store with no pen
/// when the caller proves nothing, while MCP refuses. A forged pair is the loud instance of the
/// same law — it must be refused too, not silently downgraded to `anonymous`.
#[tokio::test]
async fn an_unproven_or_forged_writer_is_refused_on_both_doors() {
    for case in cases() {
        // --- nothing claimed
        let dir = tempfile::tempdir().unwrap();
        let secret = provision(dir.path());
        let (app, mcp) = doors(dir.path());
        seed(&app, &secret).await;

        let http = request(
            &app,
            (case.method)(),
            case.uri,
            None,
            case.content_type,
            case.body,
        )
        .await;
        let mcp_ok = mcp_accepted(mcp.clone(), None, case.tool, case.args.clone()).await;

        assert!(
            !http.is_success(),
            "HTTP {} accepted a caller with NO identity (got {http}) — the store would stamp \
             `anonymous`, own nothing, and skip the channel gate",
            case.tool
        );
        assert!(
            !mcp_ok,
            "MCP {} accepted a caller with NO identity",
            case.tool
        );
        assert_eq!(
            http.is_success(),
            mcp_ok,
            "the two doors disagree on {} with no identity",
            case.tool
        );

        // --- a claimed name with the wrong proof
        let dir = tempfile::tempdir().unwrap();
        let secret = provision(dir.path());
        let (app, mcp) = doors(dir.path());
        seed(&app, &secret).await;

        let http = request(
            &app,
            (case.method)(),
            case.uri,
            Some((PEN, "not-the-secret")),
            case.content_type,
            case.body,
        )
        .await;
        assert!(
            !http.is_success(),
            "HTTP {} accepted a FORGED identity (got {http}) — `.ok().flatten()` turned the lie \
             into `anonymous` and wrote the commons",
            case.tool
        );
        assert!(
            !mcp_accepted(mcp.clone(), None, case.tool, case.args.clone()).await,
            "MCP {} accepted a caller with no identity in the forged case",
            case.tool
        );
    }
}
