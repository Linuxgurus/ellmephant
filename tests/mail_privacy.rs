//! The mailbox on the PLAIN HTTP DOOR — the same rule as the MCP tools.
//!
//! A gate on one door is theatre: `bin/mail` is a REST client, so a caller who is refused over MCP
//! would simply walk around it. These tests hold both doors to `recipient-only`, and to "the pen
//! decides the sender, not the body".
//!
//! The identity table and the surrogate map are ENV, so every test here takes one process-wide lock
//! (the same trick `src/identity.rs` uses) — a parallel test would otherwise see another's table.
//!
//! The std lock IS held across awaits on purpose: `#[tokio::test]` is a current-thread runtime, so a
//! non-`Send` future is fine, and an async lock would need a tokio feature the crate does not carry
//! just for a test. Silence the lint globally here rather than on three functions.
#![allow(clippy::await_holding_lock)]

use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex, MutexGuard};
use std::time::Instant;

use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode, header};
use ellmephant::cli::session::Session;
use ellmephant::config::{Config, Paths};
use ellmephant::embed::HashEmbedder;
use ellmephant::http::{HttpState, LockWatch};
use ellmephant::mcp::{MemoryServer, http_router};
use ellmephant::model::Spec;
use tower::ServiceExt as _;

const DIM: usize = 64;
const HASH_MODEL: &str = "fake-hash-v1";

/// An open, offline session backed by the fake embedder (mirrors `tests/http.rs`).
fn session(dir: &Path) -> Session {
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

fn app(dir: &Path) -> axum::Router {
    let watch = Arc::new(LockWatch::new());
    let state = HttpState::new(
        Paths::from_root(dir),
        Config::default(),
        Arc::clone(&watch),
        Instant::now(),
    );
    let server = MemoryServer::with_watch(Arc::new(StdMutex::new(session(dir))), watch);
    http_router(server, "127.0.0.1", state)
}

/// The env is process-wide, so every test here takes the same lock for its whole body.
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: StdMutex<()> = StdMutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Install the process-wide table (and surrogate map) for the duration of this test.
///
/// SAFETY: every test in this binary holds [`env_lock`] first, so there is one writer at a time.
fn install(identities: &str, surrogates: &str) {
    unsafe {
        std::env::set_var("ELLMEPHANT_IDENTITIES", identities);
        std::env::set_var("ELLMEPHANT_MAIL_SURROGATES", surrogates);
    }
}

/// One request, with the pen headers the caller chose (or none at all).
async fn call(
    app: &axum::Router,
    method: Method,
    uri: &str,
    pen: Option<(&str, &str)>,
    body: Option<&str>,
) -> (StatusCode, String) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some((who, proof)) = pen {
        req = req
            .header("X-Stet-From", who)
            .header("X-Stet-Auth", proof);
    }
    let req = if let Some(body) = body {
        req.header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    } else {
        req.body(Body::empty()).unwrap()
    };
    let response = app.clone().oneshot(req).await.expect("http request");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

const TABLE: &str = "stet:s1,hands:h1,james:j1";

/// Send one letter to `stet` with a proven pen, and return its seq.
async fn seed(app: &axum::Router) -> i64 {
    let (status, body) = call(
        app,
        Method::POST,
        "/mail?format=json",
        Some(("james", "j1")),
        Some(r#"{"recipient":"stet","subject":"s","body":"b"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    serde_json::from_str::<serde_json::Value>(&body).unwrap()["seq"]
        .as_i64()
        .expect("seq")
}

#[tokio::test]
async fn the_pen_decides_the_sender_not_the_body() {
    let _guard = env_lock();
    install(TABLE, "");
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path());

    // A proven pen outranks the `sender` argument, exactly as it does over MCP.
    let (status, body) = call(
        &app,
        Method::POST,
        "/mail?format=json",
        Some(("stet", "s1")),
        Some(r#"{"sender":"Mallory","recipient":"james","body":"hi"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let seq = serde_json::from_str::<serde_json::Value>(&body).unwrap()["seq"]
        .as_i64()
        .unwrap();

    let (status, body) = call(
        &app,
        Method::GET,
        &format!("/mail/{seq}?format=json"),
        Some(("james", "j1")),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["sender"],
        "stet",
        "the letter is stamped with the proof, not with the body"
    );
}

#[tokio::test]
async fn anonymous_neither_sends_nor_reads() {
    let _guard = env_lock();
    install(TABLE, "");
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path());
    let seq = seed(&app).await;

    let (status, body) = call(
        &app,
        Method::POST,
        "/mail?format=json",
        None,
        Some(r#"{"recipient":"stet","body":"unsigned"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    let (status, body) = call(&app, Method::GET, "/mail?recipient=stet", None, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    let (status, body) = call(&app, Method::GET, &format!("/mail/{seq}"), None, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(body.contains("PROVEN identity"), "{body}");
}

#[tokio::test]
async fn a_proven_stranger_is_refused_and_a_declared_surrogate_is_not() {
    let _guard = env_lock();
    install(TABLE, "hands=stet");
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path());
    let seq = seed(&app).await;

    // A proven pen is not a permission: `james` may read `james`, and only that.
    let (status, body) = call(
        &app,
        Method::GET,
        "/mail?recipient=stet",
        Some(("james", "j1")),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(body.contains("may not read"), "{body}");

    let (status, body) =
        call(&app, Method::GET, &format!("/mail/{seq}"), Some(("james", "j1")), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // The hand is the declared surrogate for the box it serves.
    let (status, body) = call(
        &app,
        Method::GET,
        "/mail?recipient=stet&format=json",
        Some(("hands", "h1")),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let body = serde_json::from_str::<serde_json::Value>(&body).unwrap();
    assert_eq!(body["letters"][0]["seq"], seq);

    // A forged claim is refused even for its own box: a name with no proof is a lie.
    let (status, body) = call(
        &app,
        Method::GET,
        "/mail?recipient=stet",
        Some(("stet", "wrong")),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
}

/// ★ A LETTER TO YOUR OWN BOX IS NEVER DELIVERY (seq 26): the guard lives in the store, so the REST
/// door refuses it exactly as the MCP tool does. A proven `James` pen addressing `james` is the
/// misfiling that sat unread in every boot; the hand's answer path (`hands` -> `stet`) stays open.
#[tokio::test]
async fn a_pen_cannot_send_to_its_own_box() {
    let _guard = env_lock();
    install(TABLE, "hands=stet");
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path());

    let (status, body) = call(
        &app,
        Method::POST,
        "/mail?format=json",
        Some(("james", "j1")),
        Some(r#"{"recipient":"james","subject":"handoff","body":"for the next me"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("never delivery"), "{body}");

    // Nothing was written: the box is still empty.
    let (status, body) = call(
        &app,
        Method::GET,
        "/mail?recipient=james&format=json",
        Some(("james", "j1")),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let body: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["letters"].as_array().map(Vec::len), Some(0), "{body}");

    // A self's own pen is refused too — the rule is about the box, not about being human.
    let (status, body) = call(
        &app,
        Method::POST,
        "/mail?format=json",
        Some(("stet", "s1")),
        Some(r#"{"recipient":"stet","body":"a note to myself"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // The hand serves `stet` without BEING `stet`: that answer is delivery, not a self-send.
    let (status, body) = call(
        &app,
        Method::POST,
        "/mail?format=json",
        Some(("hands", "h1")),
        Some(r#"{"recipient":"stet","subject":"result","body":"the guard is in"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}
