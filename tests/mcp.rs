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
    // The config is the WRITE path's view of the vector space — a write has no embedder to ask — so it
    // has to agree with the fake injected below. Disagree, and `encode_hash` describes a different space
    // than the stored vectors do, which makes every later write look like a document change.
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

/// The embedder every session in this file is built with (see `session`).
const DIM: usize = 64;
/// Its model id, which the config has to echo (see `session`).
const HASH_MODEL: &str = "fake-hash-v1";

/// Settle every queued document, the way the worker would.
///
/// A write only *queues* its embed now (`ENCODE-WORKER.md`) and a test process has no worker, so a
/// test that asserts on a search, a duplicate pair or a vector count has to settle first. The store is
/// one file, so a second connection to it is how a test drives the queue — the one-writer rule is the
/// *service's*, not the file's.
fn settle(dir: &std::path::Path) {
    ellmephant::index::vec::register_extension();
    let conn = ellmephant::db::open(&Paths::from_root(dir)).expect("open");
    let embedder = HashEmbedder::new(DIM);
    let _spec = ellmephant::embed::VecSpec::of(&embedder);
    while ellmephant::store::encode::drain(&conn, &embedder, 64).expect("drain") > 0 {}
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
    let server = MemoryServer::new(session(dir.path())).with_identity("tester");

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

    // ★ ONE KEY AS A BARE STRING: the common case is one memory, and the caller should not have to
    // wrap it in an array. Same tool, same reply, one code path.
    let single = call(
        &client,
        "memory_upvote",
        serde_json::json!({ "keys": "infra-logs" }),
    )
    .await;
    assert!(!single.is_error.unwrap_or(false), "{}", text(&single));
    assert!(
        text(&single).contains("\"times_useful\":3"),
        "a string key upvotes exactly like a one-element array: {}",
        text(&single)
    );
    settle(dir.path());

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
    // Settle the row just written, so the index is non-empty for the searches below. The later
    // re-upsert of `infra-logs` and its delete remove that row's vector on purpose.
    settle(dir.path());

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
    // ★ A MISS IS A CLAIM, AND IT MUST CARRY ITS EVIDENCE (soma/ellmephant#57): a deleted key is not
    // an absent one, and a reader that cannot tell them apart spends a session on the wrong theory.
    let miss = text(&gone);
    assert!(
        miss.contains("version(s) for this key"),
        "a deleted-but-versioned key must say so: {miss}"
    );
    assert!(
        miss.contains("DELETED row with history"),
        "and name which of the two states it is in: {miss}"
    );
    assert!(
        miss.contains("schema"),
        "the store's identity is the first thing to rule out: {miss}"
    );
    // ★ AND THE BIT THAT HALVES THE SEARCH (soma/ellmephant#57): with the row deleted, this process's
    // writer must AGREE it is absent. If it ever disagrees, the read is blind to a row the writer can
    // prove — which is the only shape the original incident can have had.
    assert!(
        miss.contains("writer-agrees-absent"),
        "the miss must report what the writer connection sees: {miss}"
    );
    assert!(
        !miss.contains("writer-sees-it"),
        "a deleted row must not be reported as visible to the writer: {miss}"
    );

    // A key that never existed says the other thing, and does not pretend to know otherwise.
    let never = call(
        &client,
        "memory_get",
        serde_json::json!({ "key": "never-existed-at-all" }),
    )
    .await;
    let never_text = text(&never);
    assert!(
        never_text.contains("has never existed THERE"),
        "an absent key must not be described as deleted: {never_text}"
    );

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

    // ★ AN UNKNOWN ARGUMENT IS REFUSED (soma/ellmephant#87). This test used to assert the opposite —
    // that `match: "fuzzy"` was silently ignored and the search "still succeeded". That is precisely
    // how `type` was dropped for a day: serde's default is to ignore what it does not know, so the
    // server answered a question nobody asked and looked confident doing it.
    let extra = call(
        &client,
        "memory_search",
        serde_json::json!({ "query": "where do logs go", "match": "fuzzy" }),
    )
    .await;
    assert_eq!(
        extra.is_error,
        Some(true),
        "an argument the tool does not understand must be refused, not discarded: {}",
        text(&extra)
    );
    // `memory_merge` integrates one key into another and deletes it, repointing its references (the
    // safe merge). The survivor already carries the deleted body's prose, so there is nothing to
    // integrate here and the call has nothing to integrate.
    for (key, body) in [
        ("merge-old", "deleted"),
        ("merge-new", "survivor, now carrying: deleted"),
    ] {
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
    assert_eq!(
        out["integrated"],
        serde_json::json!([]),
        "the caller folded first, so nothing was left to integrate"
    );

    let gone = call(
        &client,
        "memory_get",
        serde_json::json!({ "key": "merge-old" }),
    )
    .await;
    assert_eq!(gone.is_error, Some(true), "the source is deleted");
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
    let state = ellmephant::http::HttpState::new(
        Paths::from_root(dir.path()),
        Config::default(),
        std::sync::Arc::new(ellmephant::http::LockWatch::new()),
        std::time::Instant::now(),
    );
    // This router is driven in-process with no headers, so the server declares its pen (anonymous
    // writes are refused by design). The header -> identity path is covered by the provenance tests.
    let app = http_router(
        MemoryServer::new(session(dir.path())).with_identity("tester"),
        "127.0.0.1",
        state,
    );

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
async fn search_filters_round_trip() {
    // `memory_search` filters on tags and size, intersecting; a lean reply is still key-only.
    let dir = tempfile::tempdir().unwrap();
    let server = MemoryServer::new(session(dir.path())).with_identity("tester");
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

    client.cancel().await.expect("cancel");
    handle.await.expect("join");
}

#[tokio::test]
async fn projects_round_trip_over_the_transport() {
    let dir = tempfile::tempdir().unwrap();
    let server = MemoryServer::new(session(dir.path())).with_identity("tester");

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

    // The tag vocabulary is opt-in (`tags: true`) so the digest stays lean — and a memory now
    // carries tags (`chronos-design` just above), so the full set is real here.
    let lean = call(&client, "memory_stats", serde_json::json!({})).await;
    assert!(
        !text(&lean).contains("all_tags"),
        "the default digest carries no vocabulary: {}",
        text(&lean)
    );
    let vocab = call(&client, "memory_stats", serde_json::json!({ "tags": true })).await;
    let v: serde_json::Value = serde_json::from_str(&text(&vocab)).expect("json");
    let all = v["all_tags"]
        .as_array()
        .expect("`tags: true` adds all_tags");
    assert!(
        all.iter().any(|t| t[0] == serde_json::json!("design")),
        "the vocabulary lists the tags in use: {}",
        text(&vocab)
    );

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
    let server = MemoryServer::new(session(dir.path())).with_identity("tester");

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

    // Patch ONE field: the tags are NOT resent, so they must survive. The row is settled first, so
    // that `queued: false` is a fact about the document and not about an un-encoded row.
    settle(dir.path());
    let patched = call(
        &client,
        "memory_upsert",
        serde_json::json!({ "key": "note", "source": "cmd: prove" }),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&text(&patched)).expect("json");
    assert_eq!(v["status"], serde_json::json!("updated"));
    assert_eq!(
        v["queued"],
        serde_json::json!(false),
        "`source` is not part of the document"
    );

    let got = call(&client, "memory_get", serde_json::json!({ "key": "note" })).await;
    let g: serde_json::Value = serde_json::from_str(&text(&got)).expect("json");
    assert_eq!(g["body"], serde_json::json!("the facts"));
    assert_eq!(g["source"], serde_json::json!("cmd: prove"));
    assert_eq!(
        g["tags"],
        serde_json::json!(["alpha"]),
        "an omitted field keeps its stored value -- tags are not wiped"
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
    let server = MemoryServer::new(session(dir.path())).with_identity("tester");

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
    settle(dir.path());

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

    // Tear the index, the way a killed rebuild would: drop one row's vector and leave its `encode_hash`
    // still saying "indexed". That is precisely the case the hash alone cannot see.
    {
        ellmephant::index::vec::register_extension();
        let conn = ellmephant::db::open(&Paths::from_root(dir.path())).expect("open");
        let id: i64 = conn
            .query_row("SELECT id FROM memory WHERE key = 'dup-a'", [], |r| {
                r.get(0)
            })
            .expect("row id");
        ellmephant::index::vec::delete(&conn, id).expect("tear the index");
    }

    // `memory_reembed` takes no arguments: it QUEUES every document (`queued` is therefore the row
    // count, not "the ones that look owed" — there is no stamp to judge that with) and rebuilds the
    // parts that take no model. It embeds nothing itself — the worker does that.
    let re = call(&client, "memory_reembed", serde_json::json!({})).await;
    assert!(!re.is_error.unwrap_or(false), "{}", text(&re));
    let r: serde_json::Value = serde_json::from_str(&text(&re)).expect("json");
    assert_eq!(r["status"], serde_json::json!("queued"));
    assert_eq!(
        r["queued"],
        serde_json::json!(2),
        "a rebuild is always-full: the torn row is repaired, the intact one is re-embedded"
    );
    assert!(r["dim"].as_u64().expect("dim") > 0);

    client.cancel().await.expect("cancel");
    handle.await.expect("join");
}

#[tokio::test]
async fn project_get_on_a_missing_key_is_a_tool_error() {
    let dir = tempfile::tempdir().unwrap();
    let server = MemoryServer::new(session(dir.path())).with_identity("tester");

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

/// ★ `semantic_search(type=…)` MUST FILTER (soma/ellmephant#87). The parameter was named `kind` in
/// Rust while the description promised `type`, and serde discarded the argument it did not know — so
/// every "kind-filtered" search returned the whole store, and one of my own verifications passed
/// because the hits *looked* like projects.
#[tokio::test]
async fn semantic_search_honours_the_kind_parameter() {
    let dir = tempfile::tempdir().unwrap();
    let server = MemoryServer::new(session(dir.path())).with_identity("tester");

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

    let m = call(
        &client,
        "memory_upsert",
        serde_json::json!({ "key": "velero-notes-memory", "title": "Velero notes", "body": "velero restores the cluster from the bucket" }),
    )
    .await;
    assert!(!m.is_error.unwrap_or(false), "{}", text(&m));
    settle(dir.path());

    // ★ THE PROOF IS THE REFUSAL, NOT A HIT. Before the fix, `type: "project"` was silently dropped
    // and this call answered with MEMORIES — confidently, and with no sign the filter was discarded
    // (soma/ellmephant#87). Projects carry no vectors yet (that is its own open item), so an honest
    // filtered search must SAY SO rather than quietly widen to every kind.
    let projects = call(
        &client,
        "semantic_search",
        serde_json::json!({ "query": "velero", "type": "project", "k": 10, "fields": ["key"] }),
    )
    .await;
    assert_eq!(
        projects.is_error,
        Some(true),
        "a kind with no vectors must be refused, not silently widened: {}",
        text(&projects)
    );
    assert!(
        text(&projects).contains("project"),
        "and the refusal must name the kind: {}",
        text(&projects)
    );

    // The kind that DOES have vectors still answers.
    let memories = call(
        &client,
        "semantic_search",
        serde_json::json!({ "query": "velero", "type": "memory", "k": 10, "fields": ["key"] }),
    )
    .await;
    assert!(!memories.is_error.unwrap_or(false), "{}", text(&memories));
    assert!(
        text(&memories).contains("velero-notes-memory"),
        "the memory must still be found: {}",
        text(&memories)
    );

    // `kind` remains an alias for the name the schema advertises.
    let alias = call(
        &client,
        "semantic_search",
        serde_json::json!({ "query": "velero", "kind": "project", "k": 10, "fields": ["key"] }),
    )
    .await;
    assert_eq!(alias.is_error, Some(true), "`kind` is an alias of `type`");
}

/// ★ soma/ellmephant#79 over the real transport: the cursor is the PROVEN pen's, stored in the store,
/// so `mail_inbox` needs no `since` and the caller cannot forget, re-spell or advance someone else's.
#[tokio::test]
async fn the_mail_cursor_is_stored_for_the_proven_pen_over_the_transport() {
    let dir = tempfile::tempdir().unwrap();
    let server = MemoryServer::new(session(dir.path())).with_identity("tester");

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

    for n in 0..2 {
        let sent = call(
            &client,
            "mail_send",
            serde_json::json!({
                "sender": "tester",
                "recipient": "tester",
                "subject": format!("letter {n}"),
                "body": "body",
            }),
        )
        .await;
        assert!(!sent.is_error.unwrap_or(false), "{}", text(&sent));
    }

    // No `since`: the store knows where this pen is (nowhere yet) and says what is left.
    let inbox = call(
        &client,
        "mail_inbox",
        serde_json::json!({ "recipient": "tester" }),
    )
    .await;
    assert!(!inbox.is_error.unwrap_or(false), "{}", text(&inbox));
    let first: serde_json::Value = serde_json::from_str(&text(&inbox)).expect("json");
    assert_eq!(first["reader"], "tester", "the cursor names its reader");
    assert_eq!(first["cursor"], 0, "never advanced");
    assert_eq!(first["unread"], 2, "one call answers 'how much mail'");
    let page_top = first["page_top"].as_i64().expect("a full page");

    let ack = call(
        &client,
        "mail_ack",
        serde_json::json!({ "recipient": "tester", "seq": page_top }),
    )
    .await;
    assert!(!ack.is_error.unwrap_or(false), "{}", text(&ack));
    let acked: serde_json::Value = serde_json::from_str(&text(&ack)).expect("json");
    assert_eq!(acked["seq"], page_top);
    assert_eq!(acked["moved"], 2, "what the advance covered");
    assert_eq!(acked["unread"], 0);

    // The stored cursor is the default now: no `since` means "after where I am".
    let again = call(
        &client,
        "mail_inbox",
        serde_json::json!({ "recipient": "tester" }),
    )
    .await;
    let second: serde_json::Value = serde_json::from_str(&text(&again)).expect("json");
    assert_eq!(second["cursor"], page_top);
    assert_eq!(second["letters"].as_array().map(Vec::len), Some(0));

    // A cursor only moves forward, and the refusal says where it actually is.
    let back = call(
        &client,
        "mail_ack",
        serde_json::json!({ "recipient": "tester", "seq": 0 }),
    )
    .await;
    assert!(back.is_error.unwrap_or(false), "{}", text(&back));
    assert!(
        text(&back).contains("only moves forward"),
        "{}",
        text(&back)
    );

    client.cancel().await.expect("cancel");
    handle.await.expect("join");
}

/// `memory_adopt` — responsibility, not authorship, and not a land-grab.
///
/// Three rules, all at the door: an ownerless row is claimable; an owned row needs `force` AND an
/// ancestor; a row the pen cannot read cannot be adopted at all.
#[tokio::test]
async fn adopt_takes_responsibility_and_refuses_what_it_should() {
    let dir = tempfile::tempdir().unwrap();
    // Build the schema, then seed pens and rows on a second connection: the door has no way to
    // create an ownerless row (an upsert always sets the writer as owner), and a fixture that
    // cannot produce the state under test would be testing itself.
    drop(session(dir.path()));
    {
        let conn = ellmephant::db::open(&Paths::from_root(dir.path())).expect("open");
        for pen in ["tester", "other"] {
            conn.execute(
                "INSERT OR IGNORE INTO pen (name, kind) VALUES (?1, 'self')",
                rusqlite::params![pen],
            )
            .expect("seed pen");
        }
        let mem = |key: &str| ellmephant::store::Memory {
            key: key.to_string(),
            title: key.to_string(),
            body: "a fact".to_string(),
            asserted_by: Some("other".to_string()),
            ..Default::default()
        };
        ellmephant::store::add(&conn, &mem("claimable")).expect("add");
        ellmephant::store::disclaim(&conn, "claimable", "other", false).expect("disclaim");
        ellmephant::store::add(&conn, &mem("owned")).expect("add");
        ellmephant::store::add(&conn, &mem("secret")).expect("add");
        conn.execute(
            "UPDATE memory SET audience = 'pen:other' WHERE key = 'secret'",
            [],
        )
        .expect("private");
        // `add` settles nothing, but adopt reads only the row and the pen table.
        ellmephant::store::encode::drain(&conn, &HashEmbedder::new(DIM), 64).expect("drain");
    }

    let server = MemoryServer::new(session(dir.path())).with_identity("tester");
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

    // An ownerless row: no force needed.
    let ok = call(
        &client,
        "memory_adopt",
        serde_json::json!({ "key": "claimable" }),
    )
    .await;
    assert!(!ok.is_error.unwrap_or(false), "{}", text(&ok));
    let body: serde_json::Value = serde_json::from_str(&text(&ok)).expect("json");
    assert_eq!(body["owner"], "tester");

    // An owned row: refused, with or without force, because `tester` is not an ancestor of `other`.
    // The ancestor check comes first on purpose — whether the caller passed `force` is irrelevant to
    // a pen that had no right to ask.
    let no_force = call(
        &client,
        "memory_adopt",
        serde_json::json!({ "key": "owned" }),
    )
    .await;
    assert!(no_force.is_error.unwrap_or(false), "{}", text(&no_force));
    assert!(text(&no_force).contains("ancestor"), "{}", text(&no_force));
    let forced = call(
        &client,
        "memory_adopt",
        serde_json::json!({ "key": "owned", "force": true }),
    )
    .await;
    assert!(forced.is_error.unwrap_or(false), "{}", text(&forced));
    assert!(text(&forced).contains("ancestor"), "{}", text(&forced));

    // A row this pen cannot read: refused, and the refusal must not leak the content.
    let secret = call(
        &client,
        "memory_adopt",
        serde_json::json!({ "key": "secret" }),
    )
    .await;
    assert!(secret.is_error.unwrap_or(false), "{}", text(&secret));
    assert!(text(&secret).contains("cannot read"), "{}", text(&secret));

    client.cancel().await.expect("cancel");
    handle.await.expect("join");
}

#[tokio::test]
async fn config_round_trips_with_its_written_acl() {
    let dir = tempfile::tempdir().unwrap();
    // The registry is provisioning, not a tool: seed the tree the test speaks from.
    {
        let conn = ellmephant::db::open(&Paths::from_root(dir.path())).expect("open");
        conn.execute_batch(
            "DELETE FROM pen;
             INSERT INTO pen (name, kind) VALUES ('estate', 'self');
             INSERT INTO pen (name, kind, parent_id) VALUES ('james', 'self', (SELECT id FROM pen WHERE name='estate'));
             INSERT INTO pen (name, kind, parent_id) VALUES ('stet', 'self', (SELECT id FROM pen WHERE name='james'));",
        )
        .expect("pen tree");
    }

    let server = MemoryServer::new(session(dir.path())).with_identity("james");
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

    // Define: the ACL is written on the row, and the caller is recorded as owner.
    let defined = call(
        &client,
        "config_define",
        serde_json::json!({
            "key": "recess_daily_time",
            "value": "2h",
            "set_audience": "pen:james",
            "read_audience": "pen:james,pen:stet",
            "source": "James raised the daily recess",
        }),
    )
    .await;
    assert!(!defined.is_error.unwrap_or(false), "{}", text(&defined));
    let row: serde_json::Value = serde_json::from_str(&text(&defined)).expect("json");
    assert_eq!(row["value"], "2h");
    assert_eq!(row["owner"], "james");

    // Set: allowed for the written setter.
    let set = call(
        &client,
        "config_set",
        serde_json::json!({ "key": "recess_daily_time", "value": "90m" }),
    )
    .await;
    assert!(!set.is_error.unwrap_or(false), "{}", text(&set));

    // Get + list agree.
    let got = call(
        &client,
        "config_get",
        serde_json::json!({ "key": "recess_daily_time" }),
    )
    .await;
    let row: serde_json::Value = serde_json::from_str(&text(&got)).expect("json");
    assert_eq!(row["value"], "90m");

    let listed = call(&client, "config_list", serde_json::json!({})).await;
    let rows: serde_json::Value = serde_json::from_str(&text(&listed)).expect("json");
    assert_eq!(rows["config"].as_array().unwrap().len(), 1);

    // `set` cannot invent an item: the ACL is the point.
    let invented = call(
        &client,
        "config_set",
        serde_json::json!({ "key": "never-defined", "value": "x" }),
    )
    .await;
    assert!(invented.is_error.unwrap_or(false));
    assert!(
        text(&invented).contains("define it first"),
        "{}",
        text(&invented)
    );

    client.cancel().await.expect("cancel");
    handle.await.expect("join");
}
