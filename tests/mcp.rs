//! MCP round-trip tests (P7). In-process: the server runs over a `tokio::io::duplex`
//! transport with the deterministic `HashEmbedder`, so this needs no weights and no
//! network, yet still exercises the real `rmcp` handshake, tool routing and schemas.

use llmphant::cli::session::Session;
use llmphant::config::{Config, Paths};
use llmphant::embed::HashEmbedder;
use llmphant::mcp::MemoryServer;
use llmphant::model::Spec;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::{Peer, RoleClient, ServiceExt};

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
            "scope": "infra",
            "title": "Where logs live",
            "body": "Logs ship to ns1 via vector.",
            "type": "signpost",
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

    let found = call(
        &client,
        "memory_search",
        serde_json::json!({ "query": "where do logs go" }),
    )
    .await;
    assert!(text(&found).contains("infra-logs"), "{}", text(&found));

    // A second memory links back; `memory_backlinks` finds it.
    call(
        &client,
        "memory_add",
        serde_json::json!({
            "key": "infra-observability",
            "scope": "infra",
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
    assert!(
        text(&back).contains("infra-observability"),
        "{}",
        text(&back)
    );

    // update requires an existing key; a missing one is a tool error.
    let updated = call(
        &client,
        "memory_update",
        serde_json::json!({ "key": "infra-logs", "scope": "infra", "title": "Where logs live", "body": "Logs ship to ns1 via vector (updated)." }),
    )
    .await;
    assert!(!updated.is_error.unwrap_or(false), "{}", text(&updated));
    let missing = call(
        &client,
        "memory_update",
        serde_json::json!({ "key": "ghost", "scope": "infra", "title": "x", "body": "y" }),
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

    client.cancel().await.expect("cancel");
    handle.await.expect("join");
}

#[tokio::test]
async fn export_and_import_tools_round_trip() {
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
                "key": key,
                "scope": "infra",
                "title": key,
                "body": format!("body for {key}"),
            }),
        )
        .await;
        assert!(!added.is_error.unwrap_or(false), "{}", text(&added));
    }

    let dump = dir.path().join("dump.jsonl");
    let exported = call(
        &client,
        "memory_export",
        serde_json::json!({ "path": dump.to_str().unwrap() }),
    )
    .await;
    assert!(!exported.is_error.unwrap_or(false), "{}", text(&exported));
    assert!(
        text(&exported).contains("\"count\":2"),
        "{}",
        text(&exported)
    );
    assert!(dump.is_file(), "export wrote no file");

    // Importing the just-written dump is idempotent, so a dry-run reports zero changes.
    let dry = call(
        &client,
        "memory_import",
        serde_json::json!({ "path": dump.to_str().unwrap(), "dry_run": true }),
    )
    .await;
    assert!(!dry.is_error.unwrap_or(false), "{}", text(&dry));
    assert!(text(&dry).contains("\"count\":0"), "{}", text(&dry));

    // A bad retrieval mode is a tool error, not a panic.
    let bad = call(
        &client,
        "memory_search",
        serde_json::json!({ "query": "anything", "mode": "knn" }),
    )
    .await;
    assert_eq!(bad.is_error, Some(true));

    client.cancel().await.expect("cancel");
    handle.await.expect("join");
}
