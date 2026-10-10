//! MCP round-trip tests (P7). In-process: the server runs over a `tokio::io::duplex`
//! transport with the deterministic `HashEmbedder`, so this needs no weights and no
//! network, yet still exercises the real `rmcp` handshake, tool routing and schemas.

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use llmphant::cli::session::Session;
use llmphant::config::{Config, Paths};
use llmphant::embed::HashEmbedder;
use llmphant::mcp::{MemoryServer, http_router};
use llmphant::model::Spec;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::{Peer, RoleClient, ServiceExt};
use tower::ServiceExt as _;

/// An open, offline session backed by the fake embedder.
fn session(dir: &std::path::Path) -> Session {
    let paths = Paths::from_root(dir);
    paths.ensure().expect("create store root");
    let cfg = Config::default();
    let spec = Spec::from_config(&cfg, &paths);
    Session::open(paths, cfg, spec)
        .expect("open store")
        .with_embedder(Box::new(HashEmbedder::new(64)))
}

/// Call one tool and return its result (tool *errors* come back as `is_error`, not `Err`).
async fn call(
    client: &Peer<RoleClient>,
    name: &'static str,
    args: serde_json::Value,
) -> CallToolResult {
    client
        .call_tool(
            CallToolRequestParams::new(name)
                .with_arguments(args.as_object().expect("object args").clone()),
        )
        .await
        .expect("tool call")
}

/// The first text block of a result, or "".
fn text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .find_map(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap_or_default()
}

#[tokio::test]
async fn tools_round_trip_over_the_transport() {
    let dir = tempfile::tempdir().unwrap();
    let server = MemoryServer::new(session(dir.path()));

    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let handle = tokio::spawn(async move {
        server
            .serve(server_io)
            .await
            .expect("serve")
            .waiting()
            .await
            .expect("wait");
    });
    let client = ().serve(client_io).await.expect("client handshake");

    // tools/list advertises the surface with annotations.
    let listed = client.list_tools(Default::default()).await.expect("list");
    let search = listed
        .tools
        .iter()
        .find(|t| t.name == "memory_search")
        .expect("memory_search advertised");
    assert_eq!(
        search.annotations.as_ref().unwrap().read_only_hint,
        Some(true)
    );

    // add -> get -> search round-trip.
    let added = call(
        &client,
        "memory_add",
        serde_json::json!({
            "key": "infra-logs",
            "title": "Where logs live",
            "body": "Logs ship to ns1 via vector.",
            "source": "docs/observability.md",
            "tags": ["logs", "observability"]
        }),
    )
    .await;
    assert!(
        !added.is_error.unwrap_or(false),
        "add failed: {}",
        text(&added)
    );

    let got = call(
        &client,
        "memory_get",
        serde_json::json!({ "key": "infra-logs" }),
    )
    .await;
    assert!(text(&got).contains("Logs ship to ns1"), "{}", text(&got));
    // A fetch counted exactly one visit.
    assert!(text(&got).contains("\"visits\":1"), "{}", text(&got));

    // Feedback is independent of a fetch (a bare title can earn it) and bumps `was_useful`.
    let useful = call(
        &client,
        "memory_useful",
        serde_json::json!({ "key": "infra-logs" }),
    )
    .await;
    assert!(!useful.is_error.unwrap_or(false), "{}", text(&useful));
    assert!(
        text(&useful).contains("\"was_useful\":1"),
        "{}",
        text(&useful)
    );

    // A second fetch shows both counters riding on the row.
    let again = call(
        &client,
        "memory_get",
        serde_json::json!({ "key": "infra-logs" }),
    )
    .await;
    let again: serde_json::Value = serde_json::from_str(&text(&again)).expect("memory json");
    assert_eq!(again["visits"], serde_json::json!(2));
    assert_eq!(again["was_useful"], serde_json::json!(1));

    // An unknown key is a tool error (not a silent zero).
    let bogus = call(
        &client,
        "memory_useful",
        serde_json::json!({ "key": "ghost" }),
    )
    .await;
    assert_eq!(bogus.is_error, Some(true));

    let found = call(
        &client,
        "memory_search",
        serde_json::json!({ "query": "where do logs go" }),
    )
    .await;
    let found: serde_json::Value = serde_json::from_str(&text(&found)).expect("search json");
    // Positional wire: a `cols` header names the fields, rows are bare arrays.
    assert_eq!(
        found["cols"],
        serde_json::json!(["key", "title", "tags", "distance"])
    );
    assert_eq!(found["hits"][0][0], serde_json::json!("infra-logs"));

    // A second memory links back; `memory_backlinks` finds it.
    call(
        &client,
        "memory_add",
        serde_json::json!({
            "key": "infra-observability",
            "title": "Observability",
            "body": "Dashboards are in Grafana; see [[infra-logs]] for the pipeline."
        }),
    )
    .await;
    let back = call(
        &client,
        "memory_backlinks",
        serde_json::json!({ "key": "infra-logs" }),
    )
    .await;
    let back: serde_json::Value = serde_json::from_str(&text(&back)).expect("backlinks json");
    // Positional wire: a `cols` header names the fields, rows are bare arrays.
    assert_eq!(back["cols"], serde_json::json!(["key", "title"]));
    assert_eq!(
        back["backlinks"][0][0],
        serde_json::json!("infra-observability")
    );

    // update requires an existing key; a missing one is a tool error.
    let updated = call(
        &client,
        "memory_update",
        serde_json::json!({ "key": "infra-logs", "title": "Where logs live", "body": "Logs ship to ns1 via vector (updated)." }),
    )
    .await;
    assert!(!updated.is_error.unwrap_or(false), "{}", text(&updated));
    let missing = call(
        &client,
        "memory_update",
        serde_json::json!({ "key": "ghost", "title": "x", "body": "y" }),
    )
    .await;
    assert_eq!(missing.is_error, Some(true));

    // delete removes it; a second get is a tool error.
    let removed = call(
        &client,
        "memory_delete",
        serde_json::json!({ "key": "infra-logs" }),
    )
    .await;
    assert!(!removed.is_error.unwrap_or(false), "{}", text(&removed));
    let gone = call(
        &client,
        "memory_get",
        serde_json::json!({ "key": "infra-logs" }),
    )
    .await;
    assert_eq!(gone.is_error, Some(true));

    // An unknown argument is ignored (serde default), so the search still succeeds.
    let extra = call(
        &client,
        "memory_search",
        serde_json::json!({ "query": "where do logs go", "match": "fuzzy" }),
    )
    .await;
    assert!(!extra.is_error.unwrap_or(false), "{}", text(&extra));

    client.cancel().await.expect("cancel");
    handle.await.expect("join");
}

/// One MCP-over-HTTP POST to `/mcp`, returning status, `Mcp-Session-Id` (if any) and body.
async fn post(
    app: &axum::Router,
    session_id: Option<&str>,
    body: serde_json::Value,
) -> (StatusCode, Option<String>, String) {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("host", "127.0.0.1")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream");
    if let Some(id) = session_id {
        builder = builder.header("mcp-session-id", id);
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

/// The server answers a POST with JSON or an SSE stream; pull the JSON-RPC payload out of
/// whichever `data:` frame actually carries it (the stream opens with an empty one).
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

/// The default `mcp` transport is Streamable HTTP; drive the real router (no socket, no client).
#[tokio::test]
async fn tools_round_trip_over_http() {
    let dir = tempfile::tempdir().unwrap();
    let app = http_router(MemoryServer::new(session(dir.path())), "127.0.0.1");

    // initialize issues a session id.
    let (status, session, body) = post(
        &app,
        None,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "1.0" }
            }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let session = session.expect("Mcp-Session-Id header");
    assert_eq!(payload(&body)["result"]["serverInfo"]["name"], "llmphant");

    // The initialized notification is accepted (202, empty body).
    let (status, _, _) = post(
        &app,
        Some(&session),
        serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .await;
    assert!(status.is_success(), "{status}");

    // tools/list advertises the whole surface.
    let (status, _, body) = post(
        &app,
        Some(&session),
        serde_json::json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        payload(&body)["result"]["tools"].as_array().unwrap().len(),
        llmphant::mcp::TOOLS.len()
    );

    // A real write + read round-trips through the shared store.
    let (status, _, body) = post(
        &app,
        Some(&session),
        serde_json::json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": { "name": "memory_add", "arguments": {
                "key": "http-note",
                "title": "HTTP note", "body": "reached over http"
            }}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        payload(&body)["result"]["isError"],
        serde_json::json!(false)
    );
}

#[tokio::test]
async fn list_tool_reports_the_index_without_bodies() {
    // `memory_list` is the read-out surface: the whole store index, no bodies.
    let dir = tempfile::tempdir().unwrap();
    let server = MemoryServer::new(session(dir.path()));
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let handle = tokio::spawn(async move {
        server
            .serve(server_io)
            .await
            .expect("serve")
            .waiting()
            .await
            .expect("wait");
    });
    let client = ().serve(client_io).await.expect("client handshake");

    for key in ["note-one", "note-two"] {
        let added = call(
            &client,
            "memory_add",
            serde_json::json!({
                "key": key, "title": key, "body": format!("body for {key}"),
                "tags": ["infra"],
            }),
        )
        .await;
        assert!(!added.is_error.unwrap_or(false), "{}", text(&added));
    }

    let listed = call(&client, "memory_list", serde_json::json!({})).await;
    assert!(!listed.is_error.unwrap_or(false), "{}", text(&listed));
    let raw = text(&listed);
    let listed: serde_json::Value = serde_json::from_str(&raw).expect("list json");
    // Positional wire: a `cols` header names the fields, rows are bare arrays.
    assert_eq!(listed["cols"], serde_json::json!(["key", "title", "tags"]));
    assert_eq!(listed["count"], serde_json::json!(2));
    assert_eq!(listed["memories"][0][0], serde_json::json!("note-one"));
    assert_eq!(listed["memories"][0][2], serde_json::json!(["infra"]));
    assert!(
        !raw.contains("\"body\""),
        "list must not carry bodies: {raw}"
    );

    client.cancel().await.expect("cancel");
    handle.await.expect("join");
}
