//! MCP round-trip tests (P7). In-process: the server runs over a `tokio::io::duplex`
//! transport with the deterministic `HashEmbedder`, so this needs no weights and no
//! network, yet still exercises the real `rmcp` handshake, tool routing and schemas.

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use ellmephant::cli::session::Session;
use ellmephant::config::{Config, Paths};
use ellmephant::embed::HashEmbedder;
use ellmephant::mcp::{MemoryServer, http_router};
use ellmephant::model::Spec;
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
        "memory_upsert",
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

    // Feedback is independent of a fetch (a bare title can earn it) and bumps `times_useful`.
    // A sweep takes one or more keys.
    let upvote = call(
        &client,
        "memory_upvote",
        serde_json::json!({ "keys": ["infra-logs"] }),
    )
    .await;
    assert!(!upvote.is_error.unwrap_or(false), "{}", text(&upvote));
    assert!(
        text(&upvote).contains("\"times_useful\":1"),
        "{}",
        text(&upvote)
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
    assert_eq!(again["times_useful"], serde_json::json!(1));
    // The upvote cue rides the fetched *payload* itself, not just the tool's static description:
    // the ask must sit where the model's attention lands after consuming the body.
    assert!(
        again["_nudge"]
            .as_str()
            .unwrap_or_default()
            .contains("memory_upvote"),
        "get result missing the upvote nudge: {again}"
    );

    // An unknown key is a tool error (not a silent zero): a sweep that hits nothing fails.
    let bogus = call(
        &client,
        "memory_upvote",
        serde_json::json!({ "keys": ["ghost"] }),
    )
    .await;
    assert_eq!(bogus.is_error, Some(true));

    // A mixed sweep upvotes the keys that exist and reports the rest in `missing` (not fatal).
    let mixed = call(
        &client,
        "memory_upvote",
        serde_json::json!({ "keys": ["infra-logs", "ghost"] }),
    )
    .await;
    assert!(!mixed.is_error.unwrap_or(false), "{}", text(&mixed));
    let mixed: serde_json::Value = serde_json::from_str(&text(&mixed)).expect("upvote json");
    assert_eq!(mixed["upvoted"][0]["key"], serde_json::json!("infra-logs"));
    assert_eq!(mixed["upvoted"][0]["times_useful"], serde_json::json!(2));
    assert_eq!(mixed["missing"], serde_json::json!(["ghost"]));

    let found = call(
        &client,
        "memory_search",
        serde_json::json!({ "query": "where do logs go" }),
    )
    .await;
    let found: serde_json::Value = serde_json::from_str(&text(&found)).expect("search json");
    // Positional wire, lean default: no `fields` named, so a hit is just its key (SPEC §10).
    assert_eq!(found["cols"], serde_json::json!(["key"]));
    assert_eq!(found["hits"][0][0], serde_json::json!("infra-logs"));

    // Naming `fields` adds those columns (canonical order), `key` always leading.
    let named = call(
        &client,
        "memory_search",
        serde_json::json!({ "query": "where do logs go", "fields": ["title", "distance"] }),
    )
    .await;
    let named: serde_json::Value = serde_json::from_str(&text(&named)).expect("search json");
    assert_eq!(
        named["cols"],
        serde_json::json!(["key", "title", "distance"])
    );
    assert_eq!(named["hits"][0][0], serde_json::json!("infra-logs"));

    // A second memory links back; `memory_backlinks` finds it.
    call(
        &client,
        "memory_upsert",
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

    // `memory_upsert` upserts: re-storing an existing key overwrites it, no twin row.
    let upserted = call(
        &client,
        "memory_upsert",
        serde_json::json!({ "key": "infra-logs", "title": "Where logs live", "body": "Logs ship to ns1 via vector (updated)." }),
    )
    .await;
    assert!(!upserted.is_error.unwrap_or(false), "{}", text(&upserted));

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

    // `memory_stats` is the read-only digest: one keyed JSON object, no bodies.
    let stats = call(&client, "memory_stats", serde_json::json!({})).await;
    assert!(!stats.is_error.unwrap_or(false), "{}", text(&stats));
    let raw = text(&stats);
    let stats: serde_json::Value = serde_json::from_str(&raw).expect("stats json");
    // Only `infra-observability` survives (it carried no tags; `infra-logs` was deleted).
    assert_eq!(stats["memories"], serde_json::json!(1));
    assert_eq!(stats["tag_assignments"], serde_json::json!(0));
    assert_eq!(stats["visits_total"], serde_json::json!(0));
    assert!(
        !raw.contains("\"body\""),
        "stats must not carry bodies: {raw}"
    );

    // An unknown argument is ignored (serde default), so the search still succeeds.
    let extra = call(
        &client,
        "memory_search",
        serde_json::json!({ "query": "where do logs go", "match": "fuzzy" }),
    )
    .await;
    assert!(!extra.is_error.unwrap_or(false), "{}", text(&extra));
    // `memory_merge` retires one key into another, repointing its references (the safe merge).
    for (key, body) in [("merge-old", "retired"), ("merge-new", "survivor")] {
        let r = call(
            &client,
            "memory_upsert",
            serde_json::json!({ "key": key, "title": key, "body": body }),
        )
        .await;
        assert!(!r.is_error.unwrap_or(false), "{}", text(&r));
    }
    let r = call(
        &client,
        "memory_upsert",
        serde_json::json!({ "key": "merge-ref", "title": "ref", "body": "see [[merge-old]]" }),
    )
    .await;
    assert!(!r.is_error.unwrap_or(false), "{}", text(&r));

    let merged = call(
        &client,
        "memory_merge",
        serde_json::json!({ "from": "merge-old", "into": "merge-new" }),
    )
    .await;
    assert!(!merged.is_error.unwrap_or(false), "{}", text(&merged));
    let out: serde_json::Value = serde_json::from_str(&text(&merged)).expect("merge json");
    assert_eq!(out["from"], serde_json::json!("merge-old"));
    assert_eq!(out["into"], serde_json::json!("merge-new"));
    assert_eq!(
        out["bodies"],
        serde_json::json!(1),
        "the referrer was rewritten"
    );

    let gone = call(
        &client,
        "memory_get",
        serde_json::json!({ "key": "merge-old" }),
    )
    .await;
    assert_eq!(gone.is_error, Some(true), "the source is retired");
    let referrer = call(
        &client,
        "memory_get",
        serde_json::json!({ "key": "merge-ref" }),
    )
    .await;
    assert!(
        text(&referrer).contains("[[merge-new]]"),
        "the reference followed the merge: {}",
        text(&referrer)
    );

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
    assert_eq!(payload(&body)["result"]["serverInfo"]["name"], "ellmephant");

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
        ellmephant::mcp::TOOLS.len()
    );

    // A real write + read round-trips through the shared store.
    let (status, _, body) = post(
        &app,
        Some(&session),
        serde_json::json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": { "name": "memory_upsert", "arguments": {
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
async fn search_filters_and_coherence_mark_round_trip() {
    // `memory_search` filters on tags/size/coherence; `memory_coherent` records the verdict,
    // which an upsert resets, and a `coherent` filter then selects.
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

    async fn hits(client: &Peer<RoleClient>, args: serde_json::Value) -> usize {
        let r = call(client, "memory_search", args).await;
        assert!(!r.is_error.unwrap_or(false), "search failed: {}", text(&r));
        let v: serde_json::Value = serde_json::from_str(&text(&r)).expect("search json");
        v["hits"].as_array().unwrap().len()
    }

    for (key, body) in [
        ("note-long", "alpha ".repeat(50)),
        ("note-short", "beta".to_string()),
    ] {
        let added = call(
            &client,
            "memory_upsert",
            serde_json::json!({ "key": key, "title": key, "body": body, "tags": ["infra"] }),
        )
        .await;
        assert!(!added.is_error.unwrap_or(false), "{}", text(&added));
    }

    // Filters intersect: the tag keeps both, the size floor keeps the long one.
    let filtered = call(
        &client,
        "memory_search",
        serde_json::json!({ "tags": ["infra"], "min_chars": 100 }),
    )
    .await;
    let filtered: serde_json::Value = serde_json::from_str(&text(&filtered)).expect("search json");
    // A filter with no `fields` named is still key-only.
    assert_eq!(filtered["cols"], serde_json::json!(["key"]));
    assert_eq!(filtered["hits"].as_array().unwrap().len(), 1);
    assert_eq!(filtered["hits"][0][0], serde_json::json!("note-long"));

    // Both start unjudged (null).
    assert_eq!(
        hits(&client, serde_json::json!({ "coherent": "null" })).await,
        2
    );

    // Mark the long note a grab-bag: `false` finds it, `null` finds the other, `!true` both.
    let marked = call(
        &client,
        "memory_coherent",
        serde_json::json!({ "key": "note-long", "coherent": false }),
    )
    .await;
    assert!(!marked.is_error.unwrap_or(false), "{}", text(&marked));
    assert_eq!(
        hits(&client, serde_json::json!({ "coherent": "false" })).await,
        1
    );
    assert_eq!(
        hits(&client, serde_json::json!({ "coherent": "null" })).await,
        1
    );
    assert_eq!(
        hits(&client, serde_json::json!({ "coherent": "!true" })).await,
        2
    );

    // An upsert resets the verdict to unjudged (the row changed, so re-judge it).
    call(
        &client,
        "memory_upsert",
        serde_json::json!({ "key": "note-long", "title": "note-long", "body": "changed", "tags": ["infra"] }),
    )
    .await;
    assert_eq!(
        hits(&client, serde_json::json!({ "coherent": "null" })).await,
        2
    );
    assert_eq!(
        hits(&client, serde_json::json!({ "coherent": "false" })).await,
        0
    );

    // An unknown key is a tool error.
    let bogus = call(
        &client,
        "memory_coherent",
        serde_json::json!({ "key": "ghost", "coherent": true }),
    )
    .await;
    assert_eq!(bogus.is_error, Some(true));

    client.cancel().await.expect("cancel");
    handle.await.expect("join");
}

#[tokio::test]
async fn projects_round_trip_over_the_transport() {
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

    // The project tools are advertised alongside the memory ones.
    let listed = client.list_tools(Default::default()).await.expect("list");
    for name in ["project_upsert", "project_get", "project_list"] {
        assert!(
            listed.tools.iter().any(|t| t.name == name),
            "{name} not advertised"
        );
    }

    // A memory to preload, then the project itself.
    call(
        &client,
        "memory_upsert",
        serde_json::json!({ "key": "boot-note", "title": "Read me first", "body": "boot" }),
    )
    .await;
    let stored = call(
        &client,
        "project_upsert",
        serde_json::json!({
            "key": "chronos",
            "title": "Chronos — the timing subsystem",
            "goal": "Place my own intent into a future I won't occupy",
            "state": "Design only",
            "status": "active",
            "owner": ["Cline", "James"],
            "start_date": "2026-09-01",
            "tags": ["continuity"],
            "preload": ["boot-note"]
        }),
    )
    .await;
    assert!(!stored.is_error.unwrap_or(false), "{}", text(&stored));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&text(&stored)).unwrap()["key"],
        serde_json::json!("chronos"),
        "the key comes back as a plain name"
    );

    // `project_get` returns the project with its ordered preload.
    let got = call(
        &client,
        "project_get",
        serde_json::json!({ "key": "chronos" }),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&text(&got)).expect("json");
    assert_eq!(v["project"]["status"], serde_json::json!("active"));
    assert_eq!(
        v["project"]["start_date"],
        serde_json::json!("2026-09-01"),
        "a backfilled start survives the wire"
    );
    assert_eq!(
        v["project"]["end_date"],
        serde_json::json!(""),
        "a running project has no end"
    );
    assert_eq!(
        v["project"]["owner"],
        serde_json::json!(["Cline", "James"]),
        "the owner set crosses the wire as an array, sorted"
    );
    assert_eq!(v["preload"]["rows"][0][0], serde_json::json!("boot-note"));
    assert_eq!(
        v["preload"]["rows"][0][1],
        serde_json::json!("Read me first")
    );
    assert!(
        v.get("contains").is_none(),
        "membership is a tag now: there is no `contains` table on the wire"
    );
    assert!(
        v.get("bodies").is_none(),
        "hydration is opt-in: the default read carries no bodies"
    );

    // ...and `hydrate: full` carries each preload memory's body in a sister table, in order.
    let full = call(
        &client,
        "project_get",
        serde_json::json!({ "key": "chronos", "hydrate": "full" }),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&text(&full)).expect("json");
    assert_eq!(v["bodies"]["cols"], serde_json::json!(["key", "body"]));
    assert_eq!(v["bodies"]["rows"][0][0], serde_json::json!("boot-note"));

    // A memory JOINS the project by carrying its tag — set on the memory write, never a relation,
    // and read back with the ordinary `tags` filter.
    let tagged = call(
        &client,
        "memory_upsert",
        serde_json::json!({
            "key": "chronos-design",
            "title": "Chronos design v0",
            "body": "bells + periods",
            "tags": ["prj:chronos", "design"]
        }),
    )
    .await;
    assert!(!tagged.is_error.unwrap_or(false), "{}", text(&tagged));

    let members = call(
        &client,
        "memory_search",
        serde_json::json!({ "tags": ["prj:chronos", "design"] }),
    )
    .await;
    let found: serde_json::Value = serde_json::from_str(&text(&members)).expect("json");
    assert_eq!(
        found["hits"][0][0],
        serde_json::json!("chronos-design"),
        "the project tag, intersected with a topic tag, finds its members"
    );

    // A second project is not a second home: a memory may carry as many project tags as it likes.
    call(
        &client,
        "project_upsert",
        serde_json::json!({ "key": "sessions", "title": "Sessions" }),
    )
    .await;
    let both = call(
        &client,
        "memory_upsert",
        serde_json::json!({
            "key": "chronos-design",
            "title": "Chronos design v0",
            "body": "bells + periods",
            "tags": ["prj:chronos", "prj:sessions", "design"]
        }),
    )
    .await;
    assert!(
        !both.is_error.unwrap_or(false),
        "two project tags are fine — nothing enforces one home: {}",
        text(&both)
    );

    // `project_list` is the cheap index, and its filters intersect.
    let all = call(&client, "project_list", serde_json::json!({})).await;
    let v: serde_json::Value = serde_json::from_str(&text(&all)).expect("json");
    assert_eq!(v["projects"].as_array().unwrap().len(), 2);
    let active = call(
        &client,
        "project_list",
        serde_json::json!({ "status": "active", "pattern": "timing" }),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&text(&active)).expect("json");
    assert_eq!(v["projects"].as_array().unwrap().len(), 1);
    assert_eq!(v["projects"][0][0], serde_json::json!("chronos"));

    // An unknown project is a clean tool error.
    let missing = call(&client, "project_get", serde_json::json!({ "key": "nope" })).await;
    assert_eq!(missing.is_error, Some(true));
    assert!(
        text(&missing).contains("no such project"),
        "{}",
        text(&missing)
    );

    client.cancel().await.expect("cancel");
    handle.await.expect("join");
}

#[tokio::test]
async fn upsert_patches_one_field_over_the_transport() {
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

    // Create, with tags.
    let created = call(
        &client,
        "memory_upsert",
        serde_json::json!({
            "key": "note", "title": "A note", "body": "the facts", "tags": ["alpha"]
        }),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&text(&created)).expect("json");
    assert_eq!(v["status"], serde_json::json!("created"));
    assert_eq!(v["coherent_reset"], serde_json::json!(false));

    // A verdict, so we can prove a patch does not un-judge the row.
    call(
        &client,
        "memory_coherent",
        serde_json::json!({ "key": "note", "coherent": true }),
    )
    .await;

    // Patch ONE field: the tags are NOT resent, so they must survive.
    let patched = call(
        &client,
        "memory_upsert",
        serde_json::json!({ "key": "note", "source": "cmd: prove" }),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&text(&patched)).expect("json");
    assert_eq!(v["status"], serde_json::json!("updated"));
    assert_eq!(
        v["reembedded"],
        serde_json::json!(false),
        "`source` is not part of the document"
    );
    assert_eq!(v["coherent_reset"], serde_json::json!(false));

    let got = call(&client, "memory_get", serde_json::json!({ "key": "note" })).await;
    let g: serde_json::Value = serde_json::from_str(&text(&got)).expect("json");
    assert_eq!(g["body"], serde_json::json!("the facts"));
    assert_eq!(g["source"], serde_json::json!("cmd: prove"));
    assert_eq!(
        g["tags"],
        serde_json::json!(["alpha"]),
        "an omitted field keeps its stored value -- tags are not wiped"
    );
    assert_eq!(
        g["coherent"],
        serde_json::json!(true),
        "a field write must not re-open the split review"
    );

    // Clearing IS expressible: `[]` empties the tags.
    call(
        &client,
        "memory_upsert",
        serde_json::json!({ "key": "note", "tags": [] }),
    )
    .await;
    let got = call(&client, "memory_get", serde_json::json!({ "key": "note" })).await;
    let g: serde_json::Value = serde_json::from_str(&text(&got)).expect("json");
    assert_eq!(g["tags"], serde_json::json!([]));

    // A new key with no title is refused rather than written empty.
    let bad = call(
        &client,
        "memory_upsert",
        serde_json::json!({ "key": "ghost", "source": "x" }),
    )
    .await;
    assert!(bad.is_error.unwrap_or(false), "{}", text(&bad));

    // Projects: patch one field, and an omitted `status` is NOT reset to `todo`.
    let made = call(
        &client,
        "project_upsert",
        serde_json::json!({ "key": "chronos", "title": "Chronos", "status": "active" }),
    )
    .await;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&text(&made)).expect("json")["status"],
        serde_json::json!("created"),
        "a new key reports itself: this is what makes a mistyped key LOUD instead of a duplicate"
    );
    let set = call(
        &client,
        "project_upsert",
        serde_json::json!({ "key": "chronos", "state": "Design only" }),
    )
    .await;
    assert!(!set.is_error.unwrap_or(false), "{}", text(&set));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&text(&set)).expect("json")["status"],
        serde_json::json!("updated")
    );
    let got = call(
        &client,
        "project_get",
        serde_json::json!({ "key": "chronos" }),
    )
    .await;
    let g: serde_json::Value = serde_json::from_str(&text(&got)).expect("json");
    assert_eq!(g["project"]["state"], serde_json::json!("Design only"));
    assert_eq!(
        g["project"]["status"],
        serde_json::json!("active"),
        "an omitted status keeps its stored value"
    );

    client.cancel().await.expect("cancel");
    handle.await.expect("join");
}

#[tokio::test]
async fn dupes_and_reembed_round_trip_over_the_transport() {
    // The two tools that were wired but never called by a test.
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

    // Two rows that differ only in their key, so the finder has a pair to report.
    for key in ["dup-a", "dup-b"] {
        call(
            &client,
            "memory_upsert",
            serde_json::json!({
                "key": key, "title": "Same thing twice", "body": "identical facts"
            }),
        )
        .await;
    }

    let dupes = call(
        &client,
        "memory_dupes",
        serde_json::json!({ "max_distance": 1.0 }),
    )
    .await;
    assert!(!dupes.is_error.unwrap_or(false), "{}", text(&dupes));
    let d: serde_json::Value = serde_json::from_str(&text(&dupes)).expect("json");
    assert_eq!(
        d["cols"],
        serde_json::json!(["key_a", "title_a", "key_b", "title_b", "distance"]),
        "the positional table is self-describing"
    );
    assert!(
        d["count"].as_u64().expect("count") >= 1,
        "near-identical rows must pair: {d}"
    );

    // `memory_reembed` takes no arguments: it recomputes every vector and rebuilds the index.
    let re = call(&client, "memory_reembed", serde_json::json!({})).await;
    assert!(!re.is_error.unwrap_or(false), "{}", text(&re));
    let r: serde_json::Value = serde_json::from_str(&text(&re)).expect("json");
    assert_eq!(r["status"], serde_json::json!("re-embedded"));
    assert!(r["rows"].as_u64().expect("rows") >= 2);
    assert!(r["dim"].as_u64().expect("dim") > 0);

    client.cancel().await.expect("cancel");
    handle.await.expect("join");
}

#[tokio::test]
async fn project_get_on_a_missing_key_is_a_tool_error() {
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

    let missing = call(
        &client,
        "project_get",
        serde_json::json!({ "key": "ghost" }),
    )
    .await;
    assert!(missing.is_error.unwrap_or(false), "{}", text(&missing));
    assert!(
        text(&missing).contains("no such project"),
        "got: {}",
        text(&missing)
    );

    client.cancel().await.expect("cancel");
    handle.await.expect("join");
}
