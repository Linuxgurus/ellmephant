//! THE TWO DOORS AGREE ON WHO MAY OPEN A BOX (soma/ellmephant#231, #235).
//!
//! The authority contract (`tests/authority_contract.rs`) covers who may WRITE. This is its twin for
//! the other place a rule was decided twice: mail. When this test was written the doors disagreed in
//! two ways, and both were live defects rather than tidiness:
//!
//! 1. **The grant was wrong on one door.** MCP checked `mail_may_ack` for `mail_ack`; HTTP checked
//!    `mail_may_read`, so a pen holding only an `ack` grant was refused by one door and admitted by
//!    the other. A rule that is asked twice will always eventually be asked differently.
//! 2. **The sentence had rotted.** The MCP refusal carried a run of blanks where a word had been
//!    cut ("or a live              `{scope}` grant") — the corrupted string this epic was filed
//!    over, in the very function that was supposed to be the one rule.
//!
//! So the contract is behavioural, and it compares the WORDS, not just the status: a refusal a
//! caller can read is part of the law. Drive both doors with the same caller and the same box, and
//! require the same sentence.

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
const OTHER: &str = "other";

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

/// Two pens with credentials: `tester` (the caller) and `other` (the box owner).
fn provision(dir: &std::path::Path) -> (String, String) {
    let paths = Paths::from_root(dir);
    let conn = ellmephant::db::open(&paths).expect("open");
    let mut secrets = Vec::new();
    for name in [PEN, OTHER] {
        conn.execute(
            "INSERT INTO pen (name, kind, parent_id)
             VALUES (?1, 'self', (SELECT id FROM pen WHERE name = 'estate'))",
            [name],
        )
        .expect("seed pen");
        secrets.push(ellmephant::store::credential::issue(&conn, name, None).expect("issue"));
    }
    (secrets[0].clone(), secrets[1].clone())
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

/// One HTTP request, with the door's bearer and an optional identity pair. Returns status and body.
async fn request(
    app: &axum::Router,
    method: Method,
    uri: &str,
    identity: Option<(&str, &str)>,
    content_type: Option<&str>,
    body: &str,
) -> (StatusCode, String) {
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
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
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

/// Call one mail tool; `Err` carries the refusal sentence the caller would read.
async fn mcp_call(
    server: MemoryServer,
    declared: Option<&str>,
    tool: &str,
    args: serde_json::Value,
) -> Result<String, String> {
    let service = peer(server, declared).await;
    let result = service
        .peer()
        .call_tool(
            CallToolRequestParams::new(tool.to_string())
                .with_arguments(args.as_object().expect("object args").clone()),
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

/// A letter in `other`'s box, written by `other`, so `tester` has something to be refused.
async fn seed_letter(app: &axum::Router, secret: &str) -> i64 {
    let body = serde_json::json!({
        "sender": OTHER, "recipient": OTHER, "subject": "s", "body": "private words",
    })
    .to_string();
    let (status, out) = request(
        app,
        Method::POST,
        "/mail?format=json",
        Some((OTHER, secret)),
        Some("application/json"),
        &body,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "seed letter: {out}");
    let parsed: serde_json::Value = serde_json::from_str(&out).expect("letter json");
    parsed["seq"].as_i64().expect("seq")
}

/// One mail verb, spelled both ways.
struct Case {
    tool: &'static str,
    args: fn(&str) -> serde_json::Value,
    method: fn() -> Method,
    uri: fn(&str) -> String,
    body: &'static str,
}

fn cases() -> Vec<Case> {
    vec![
        Case {
            tool: "mail_inbox",
            args: |_| serde_json::json!({ "recipient": OTHER, "limit": 10 }),
            method: || Method::GET,
            uri: |_| format!("/mail?recipient={OTHER}"),
            body: "",
        },
        Case {
            tool: "mail_ack",
            args: |_| serde_json::json!({ "recipient": OTHER, "seq": 1 }),
            method: || Method::POST,
            uri: |_| format!("/mail/ack?recipient={OTHER}&seq=1"),
            body: "",
        },
        Case {
            tool: "mail_read",
            args: |_| serde_json::json!({ "seq": 1 }),
            method: || Method::GET,
            uri: |_| "/mail/1".to_string(),
            body: "",
        },
    ]
}

/// The clause both doors must say, with the VERB that was denied. `ack` is the one that was wrong.
fn refusal_clause(verb: &str) -> String {
    format!("may not {verb} `{OTHER}`'s mail")
}

/// ★ THE CONTRACT: a stranger is refused by both doors, in the same words, with no blank runs.
#[tokio::test]
async fn a_stranger_is_refused_on_both_doors_in_the_same_words() {
    for case in cases() {
        let dir = tempfile::tempdir().unwrap();
        let (secret, other_secret) = provision(dir.path());
        let (app, mcp) = doors(dir.path());
        seed_letter(&app, &other_secret).await;

        let (status, http_body) = request(
            &app,
            (case.method)(),
            &(case.uri)(""),
            Some((PEN, &secret)),
            None,
            case.body,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "HTTP {} must refuse a stranger with 403, got {status}: {http_body}",
            case.tool
        );

        let mcp_body = mcp_call(mcp, Some(PEN), case.tool, (case.args)(PEN))
            .await
            .expect_err(&format!("MCP {} accepted a stranger", case.tool));

        // The verb a door names must be the verb the caller asked for: `mail_ack` judged by the
        // `read` grant was the defect this test exists for.
        let verb = if case.tool == "mail_ack" {
            "ack"
        } else {
            "read"
        };
        let clause = refusal_clause(verb);
        assert!(
            http_body.contains(&clause),
            "HTTP {} refused, but not with the box rule: {http_body}",
            case.tool
        );
        assert!(
            mcp_body.contains(&clause),
            "MCP {} refused, but not with the box rule: {mcp_body}",
            case.tool
        );
        assert_eq!(
            http_body.trim(),
            mcp_body.trim(),
            "the two doors refused {} in different words — a refusal is part of the law",
            case.tool
        );
        // ★ The corruption this epic was filed over: a run of four spaces means a word was cut out
        // of the sentence by a copy-edit, and it shipped for a week because nothing compared it.
        assert!(
            !http_body.contains("    "),
            "the refusal contains a run of blanks — a word was cut out of it: {http_body}"
        );
    }
}

/// ★ AN `ack` GRANT IS AN `ack` GRANT (soma/ellmephant#235). This is the divergence the two
/// doors carried: HTTP asked the `read` grant for `mail_ack`, so a pen holding only `ack` was
/// refused here and admitted on MCP. Both doors must now say yes.
#[tokio::test]
async fn an_ack_grant_opens_ack_on_both_doors() {
    let dir = tempfile::tempdir().unwrap();
    let (secret, other_secret) = provision(dir.path());
    let (app, mcp) = doors(dir.path());
    seed_letter(&app, &other_secret).await;

    // `other` lends `tester` an `ack` grant — and NOT a `read` one.
    let ask = serde_json::json!({
        "grantor": OTHER, "surrogate": PEN, "scopes": "ack",
    })
    .to_string();
    let (status, body) = request(
        &app,
        Method::POST,
        "/mail/surrogate/grant?format=json",
        Some((OTHER, &other_secret)),
        Some("application/json"),
        &ask,
    )
    .await;
    assert!(status.is_success(), "grant: {status} {body}");

    let (status, body) = request(
        &app,
        Method::POST,
        &format!("/mail/ack?recipient={OTHER}&seq=1"),
        Some((PEN, &secret)),
        None,
        "",
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "HTTP refused an ack the grant allows (the read-grant bug): {body}"
    );

    mcp_call(
        mcp,
        Some(PEN),
        "mail_ack",
        serde_json::json!({ "recipient": OTHER, "seq": 1 }),
    )
    .await
    .expect("MCP refused an ack the grant allows");
}

/// ★ A pen may open its OWN box on both doors — the positive control, so the refusals above are a
/// decision rather than a broken route.
#[tokio::test]
async fn a_pen_may_open_its_own_box_on_both_doors() {
    let dir = tempfile::tempdir().unwrap();
    let (secret, _other_secret) = provision(dir.path());
    let (app, mcp) = doors(dir.path());

    // inbox: `tester` asks for `tester`'s box.
    let (status, body) = request(
        &app,
        Method::GET,
        &format!("/mail?recipient={PEN}&limit=10"),
        Some((PEN, &secret)),
        None,
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "own box over HTTP: {body}");
    mcp_call(
        mcp,
        Some(PEN),
        "mail_inbox",
        serde_json::json!({ "recipient": PEN, "limit": 10 }),
    )
    .await
    .expect("own box over MCP");
}

/// ★ NO PEN IS REFUSED ON BOTH DOORS — before the box is even consulted.
#[tokio::test]
async fn an_unproven_reader_is_refused_on_both_doors() {
    let dir = tempfile::tempdir().unwrap();
    let (_secret, other_secret) = provision(dir.path());
    let (app, mcp) = doors(dir.path());
    seed_letter(&app, &other_secret).await;

    let (status, body) = request(
        &app,
        Method::GET,
        &format!("/mail?recipient={PEN}"),
        None,
        None,
        "",
    )
    .await;
    assert!(
        status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN,
        "HTTP refused an unproven mail reader with {status}: {body}"
    );

    let mcp_body = mcp_call(
        mcp,
        None,
        "mail_inbox",
        serde_json::json!({ "recipient": PEN }),
    )
    .await
    .expect_err("MCP accepted an unproven mail reader");
    assert!(
        mcp_body.contains("proven identity"),
        "MCP refused, but not at the identity gate: {mcp_body}"
    );
}
