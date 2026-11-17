//! THE GATE BITES BOTH DOORS (soma/ellmephant#231, #232, #234).
//!
//! The epic names three ways the two adapters had drifted. Two are pinned by tests now: who may
//! write (`tests/authority_contract.rs`) and who may open a box (`tests/mail_contract.rs`). This is
//! the third: `channel_gate` was hand-placed in seven write verbs *per door*, and the only thing
//! keeping coverage correct was that nobody had added an eighth.
//!
//! Since #234 the gate lives in `ops::`, which every write verb on both doors goes through, and it
//! runs unconditionally — so a `gate: writes` channel bites a caller who named no pen at all (the
//! old HTTP door skipped the gate entirely for those). This test proves the sentence, on both
//! doors, and proves that LISTENING is what releases it: the gate is a seatbelt, not a lock.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode, header};
use ellmephant::config::{Config, Paths};
use ellmephant::db;
use ellmephant::embed::HashEmbedder;
use ellmephant::http::{HttpState, LockWatch};
use ellmephant::mcp::{MemoryServer, http_router};
use ellmephant::model::Spec;
use ellmephant::store::channel;
use ellmephant::store::session::Session;
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use tower::ServiceExt as _;

const DIM: usize = 64;
const HASH_MODEL: &str = "fake-hash-v1";
const TOKEN: &str = "door";
const PEN: &str = "tester";
const SENDER: &str = "james";
const CHANNEL: &str = "ops_team";

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

/// Two pens, both members of `ops_team`; the channel gated on writes, one missed delivery.
///
/// Returns the caller's secret.
fn gated_channel(dir: &std::path::Path) -> String {
    let paths = Paths::from_root(dir);
    let conn = db::open(&paths).expect("open");
    for name in [PEN, SENDER] {
        conn.execute(
            "INSERT INTO pen (name, kind, parent_id)
             VALUES (?1, 'self', (SELECT id FROM pen WHERE name = 'estate'))",
            [name],
        )
        .expect("seed pen");
    }
    let secret = ellmephant::store::credential::issue(&conn, PEN, None).expect("issue credential");

    // ★ ONE MISSED DELIVERY IS ENOUGH (misses_to_gate = 1), so the test reads as one rule rather
    // than as an arithmetic exercise.
    conn.execute(
        "UPDATE channel
            SET gate = 'writes', grace_secs = 0, clock = 'delivery',
                misses_to_gate = 1, lock_max_secs = 0
          WHERE key = ?1",
        [CHANNEL],
    )
    .expect("set policy");
    channel::invite(&conn, SENDER, SENDER, CHANNEL).expect("sender is a member");
    channel::invite(&conn, SENDER, PEN, CHANNEL).expect("invite the caller");
    channel::send(&conn, SENDER, CHANNEL, "we are rolling", None).expect("send");
    // The turn boundary passes without a fetch: that is the breach.
    channel::present(&conn, PEN, None).expect("present");
    channel::gate(&conn, PEN, channel::Class::Record).expect_err("the gate is closed");
    secret
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

async fn post_memory(
    app: &axum::Router,
    identity: Option<(&str, &str)>,
    key: &str,
) -> (StatusCode, String) {
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri("/memory")
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header(header::CONTENT_TYPE, "text/plain");
    if let Some((from, auth)) = identity {
        builder = builder
            .header("X-Stet-From", from)
            .header("X-Stet-Auth", auth);
    }
    let body = format!("Key: {key}\nTitle: {key}\n---\nthe body\n");
    let response = app
        .clone()
        .oneshot(builder.body(Body::from(body)).unwrap())
        .await
        .expect("http request");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn mcp_upsert(
    server: MemoryServer,
    declared: Option<&str>,
    key: &str,
) -> Result<String, String> {
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
    let client = ().serve(client_io).await.expect("client handshake");
    let result = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("memory_upsert").with_arguments(
                serde_json::json!({ "key": key, "title": key, "body": "the body" })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .expect("tool call");
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap_or_default();
    if result.is_error.unwrap_or(false) {
        Err(text)
    } else {
        Ok(text)
    }
}

/// The clause the gate refuses with. It must name the channel and say what to do about it.
const GATE: &str = "has not been fetched";

/// ★ A BREACHED PEN IS REFUSED ON BOTH DOORS, in the same words.
#[tokio::test]
async fn a_gated_pen_is_refused_on_both_doors_in_the_same_words() {
    let dir = tempfile::tempdir().unwrap();
    let secret = gated_channel(dir.path());
    let (app, mcp) = doors(dir.path());

    let (status, http_body) = post_memory(&app, Some((PEN, &secret)), "probe").await;
    assert!(
        !status.is_success(),
        "HTTP accepted a write from a pen the gate holds shut ({status}): {http_body}"
    );
    assert!(
        http_body.contains(GATE),
        "HTTP refused, but not at the gate: {http_body}"
    );

    let mcp_body = mcp_upsert(mcp, Some(PEN), "probe")
        .await
        .expect_err("MCP accepted a write from a pen the gate holds shut");
    assert!(
        mcp_body.contains(GATE),
        "MCP refused, but not at the gate: {mcp_body}"
    );
    assert_eq!(
        http_body.trim(),
        mcp_body.trim(),
        "the two doors refused the gate in different words — a refusal is part of the law"
    );
}

/// ★ DROPPING THE PEN IS NOT A WAY PAST IT (soma/ellmephant#231). The old HTTP door ran the gate
/// only `if let Some(caller)`, so an unidentified caller skipped a `gate: writes` channel entirely.
/// They are refused at the identity gate now — which is earlier, and the point is that neither
/// answer is a write.
#[tokio::test]
async fn an_unidentified_caller_cannot_skip_the_gate() {
    let dir = tempfile::tempdir().unwrap();
    let _secret = gated_channel(dir.path());
    let (app, mcp) = doors(dir.path());

    let (status, body) = post_memory(&app, None, "probe").await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "an unidentified writer must be refused before the gate is even consulted: {body}"
    );

    let mcp_body = mcp_upsert(mcp, None, "probe")
        .await
        .expect_err("MCP accepted an unidentified writer");
    assert!(
        mcp_body.contains("proven identity"),
        "MCP refused, but not at the identity gate: {mcp_body}"
    );
}

/// ★ LISTENING RELEASES IT — the gate is a seatbelt, not a lock. After a fetch, the same pen writes
/// on both doors.
#[tokio::test]
async fn fetching_releases_the_gate_on_both_doors() {
    let dir = tempfile::tempdir().unwrap();
    let secret = gated_channel(dir.path());

    {
        // Listening: the one act the gate is asking for.
        let paths = Paths::from_root(dir.path());
        let conn = db::open(&paths).expect("open");
        let got = channel::fetch(&conn, PEN, Some(CHANNEL), 50).expect("fetch");
        assert!(!got.is_empty(), "there was a message to read");
        channel::gate(&conn, PEN, channel::Class::Record).expect("reading releases it");
    }

    let (app, mcp) = doors(dir.path());
    let (status, body) = post_memory(&app, Some((PEN, &secret)), "after-fetch").await;
    assert!(
        status.is_success(),
        "HTTP still refuses after the channel was fetched ({status}): {body}"
    );
    mcp_upsert(mcp, Some(PEN), "after-fetch-mcp")
        .await
        .expect("MCP still refuses after the channel was fetched");
}
