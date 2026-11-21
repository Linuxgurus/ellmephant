use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use ellmephant::config::{Config, Paths};
use ellmephant::embed::HashEmbedder;
use ellmephant::mcp::{MemoryServer, http_router};
use ellmephant::model::Spec;
use ellmephant::store::session::Session;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::{Peer, RoleClient, ServiceExt};
use tower::ServiceExt as _;

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

const DIM: usize = 64;
const HASH_MODEL: &str = "fake-hash-v1";

fn settle(dir: &std::path::Path) {
    ellmephant::index::vec::register_extension();
    let conn = ellmephant::db::open(&Paths::from_root(dir)).expect("open");
    let embedder = HashEmbedder::new(DIM);
    let _spec = ellmephant::embed::VecSpec::of(&embedder);
    while ellmephant::store::encode::drain(&conn, &embedder, 64).expect("drain") > 0 {}
}

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
    assert!(text(&got).contains("\"visits\":1"), "{}", text(&got));

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

    let again = call(
        &client,
        "memory_get",
        serde_json::json!({ "key": "infra-logs" }),
    )
    .await;
    let again: serde_json::Value = serde_json::from_str(&text(&again)).expect("memory json");
    assert_eq!(again["visits"], serde_json::json!(2));
    assert_eq!(again["times_useful"], serde_json::json!(1));
    assert!(
        again["_nudge"]
            .as_str()
            .unwrap_or_default()
            .contains("memory_upvote"),
        "get result missing the upvote nudge: {again}"
    );

    let bogus = call(
        &client,
        "memory_upvote",
        serde_json::json!({ "keys": ["ghost"] }),
    )
    .await;
    assert_eq!(bogus.is_error, Some(true));

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
    assert_eq!(found["cols"], serde_json::json!(["key", "via"]));
    assert_eq!(found["hits"][0][0], serde_json::json!("infra-logs"));

    let named = call(
        &client,
        "memory_search",
        serde_json::json!({ "query": "where do logs go", "fields": ["title", "distance"] }),
    )
    .await;
    let named: serde_json::Value = serde_json::from_str(&text(&named)).expect("search json");
    assert_eq!(
        named["cols"],
        serde_json::json!(["key", "via", "title", "distance"])
    );
    assert_eq!(named["hits"][0][0], serde_json::json!("infra-logs"));

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
    assert_eq!(back["cols"], serde_json::json!(["key", "title"]));
    assert_eq!(
        back["backlinks"][0][0],
        serde_json::json!("infra-observability")
    );
    settle(dir.path());

    let upserted = call(
        &client,
        "memory_upsert",
        serde_json::json!({ "key": "infra-logs", "title": "Where logs live", "body": "Logs ship to ns1 via vector (updated)." }),
    )
    .await;
    assert!(!upserted.is_error.unwrap_or(false), "{}", text(&upserted));

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
    assert!(
        miss.contains("writer-agrees-absent"),
        "the miss must report what the writer connection sees: {miss}"
    );
    assert!(
        !miss.contains("writer-sees-it"),
        "a deleted row must not be reported as visible to the writer: {miss}"
    );

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

    let stats = call(&client, "memory_stats", serde_json::json!({})).await;
    assert!(!stats.is_error.unwrap_or(false), "{}", text(&stats));
    let raw = text(&stats);
    let stats: serde_json::Value = serde_json::from_str(&raw).expect("stats json");
    assert_eq!(stats["memories"], serde_json::json!(1));
    assert_eq!(stats["tag_assignments"], serde_json::json!(0));
    assert_eq!(stats["visits_total"], serde_json::json!(0));
    assert!(
        !raw.contains("\"body\""),
        "stats must not carry bodies: {raw}"
    );

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

    for (key, body) in [
        ("fold-source", "line from the source"),
        ("fold-keeper", "line the keeper had"),
    ] {
        let r = call(
            &client,
            "memory_upsert",
            serde_json::json!({ "key": key, "title": key, "body": body }),
        )
        .await;
        assert!(!r.is_error.unwrap_or(false), "{}", text(&r));
    }
    let folded = call(
        &client,
        "memory_fold",
        serde_json::json!({ "from": "fold-source", "into": "fold-keeper" }),
    )
    .await;
    assert!(!folded.is_error.unwrap_or(false), "{}", text(&folded));
    let out: serde_json::Value = serde_json::from_str(&text(&folded)).expect("fold json");
    assert_eq!(out["from"], serde_json::json!("fold-source"));
    assert_eq!(out["into"], serde_json::json!("fold-keeper"));
    assert_eq!(out["changed"], serde_json::json!(true));
    assert_eq!(
        out["integrated"],
        serde_json::json!(["line from the source"]),
        "the missing line is appended verbatim"
    );
    let keeper = call(
        &client,
        "memory_get",
        serde_json::json!({ "key": "fold-keeper" }),
    )
    .await;
    assert!(
        text(&keeper).contains("line the keeper had\\nline from the source"),
        "the keeper grew: {}",
        text(&keeper)
    );
    let source = call(
        &client,
        "memory_get",
        serde_json::json!({ "key": "fold-source" }),
    )
    .await;
    assert!(
        !source.is_error.unwrap_or(false),
        "a fold must NOT delete the source: {}",
        text(&source)
    );
    let bad = call(
        &client,
        "memory_fold",
        serde_json::json!({
            "from": "fold-source", "into": "fold-keeper", "title": "rewritten"
        }),
    )
    .await;
    assert_eq!(
        bad.is_error,
        Some(true),
        "a fold cannot change a title, so it cannot take one: {}",
        text(&bad)
    );

    client.cancel().await.expect("cancel");
    handle.await.expect("join");
}

async fn post(
    app: &axum::Router,
    session_id: Option<&str>,
    body: serde_json::Value,
) -> (StatusCode, Option<String>, String) {
    post_to(app, "/mcp", session_id, body).await
}

async fn post_to(
    app: &axum::Router,
    uri: &str,
    session_id: Option<&str>,
    body: serde_json::Value,
) -> (StatusCode, Option<String>, String) {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
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

#[tokio::test]
async fn tools_round_trip_over_http() {
    let dir = tempfile::tempdir().unwrap();
    let state = ellmephant::http::HttpState::new(
        Paths::from_root(dir.path()),
        Config::default(),
        std::sync::Arc::new(ellmephant::http::LockWatch::new()),
        std::time::Instant::now(),
    );
    let app = http_router(
        MemoryServer::new(session(dir.path())).with_identity("tester"),
        "127.0.0.1",
        state,
    );

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

    let (status, _, _) = post(
        &app,
        Some(&session),
        serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .await;
    assert!(status.is_success(), "{status}");

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

    let filtered = call(
        &client,
        "memory_search",
        serde_json::json!({ "tags": ["infra"], "min_chars": 100 }),
    )
    .await;
    let filtered: serde_json::Value = serde_json::from_str(&text(&filtered)).expect("search json");
    assert_eq!(filtered["cols"], serde_json::json!(["key", "via"]));
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

    let listed = client.list_tools(Default::default()).await.expect("list");
    for name in ["project_upsert", "project_get", "project_list"] {
        assert!(
            listed.tools.iter().any(|t| t.name == name),
            "{name} not advertised"
        );
    }

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

    let full = call(
        &client,
        "project_get",
        serde_json::json!({ "key": "chronos", "hydrate": "full" }),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&text(&full)).expect("json");
    assert_eq!(v["bodies"]["cols"], serde_json::json!(["key", "body"]));
    assert_eq!(v["bodies"]["rows"][0][0], serde_json::json!("boot-note"));

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

    call(
        &client,
        "memory_upsert",
        serde_json::json!({ "key": "note", "tags": [] }),
    )
    .await;
    let got = call(&client, "memory_get", serde_json::json!({ "key": "note" })).await;
    let g: serde_json::Value = serde_json::from_str(&text(&got)).expect("json");
    assert_eq!(g["tags"], serde_json::json!([]));

    let bad = call(
        &client,
        "memory_upsert",
        serde_json::json!({ "key": "ghost", "source": "x" }),
    )
    .await;
    assert!(bad.is_error.unwrap_or(false), "{}", text(&bad));

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
        serde_json::json!(["key_a", "title_a", "key_b", "title_b", "distance", "class"]),
        "the positional table is self-describing"
    );
    assert!(
        d["count"].as_u64().expect("count") >= 1,
        "near-identical rows must pair: {d}"
    );
    assert!(
        d["pairs"]
            .as_array()
            .expect("pairs")
            .iter()
            .any(|p| p[5] == serde_json::json!("duplicate")),
        "each pair carries its class: {d}"
    );

    let plan = call(
        &client,
        "memory_dupes",
        serde_json::json!({ "max_distance": 1.0, "plan": true }),
    )
    .await;
    assert!(!plan.is_error.unwrap_or(false), "{}", text(&plan));
    let p: serde_json::Value = serde_json::from_str(&text(&plan)).expect("json");
    assert_eq!(
        p["cols"],
        serde_json::json!(["keep", "absorb", "reason"]),
        "the plan table is self-describing"
    );
    assert_eq!(
        p["refused_cols"],
        serde_json::json!(["key_a", "key_b", "class", "reason"]),
    );
    let steps = p["plan"].as_array().expect("plan");
    assert_eq!(steps.len(), 1, "one duplicate pair, one step: {p}");
    assert_eq!(steps[0][0], serde_json::json!("dup-a"), "the keeper: {p}");
    assert_eq!(steps[0][1], serde_json::json!("dup-b"), "the absorbed: {p}");

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

    let alias = call(
        &client,
        "semantic_search",
        serde_json::json!({ "query": "velero", "kind": "project", "k": 10, "fields": ["key"] }),
    )
    .await;
    assert_eq!(alias.is_error, Some(true), "`kind` is an alias of `type`");
}

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

    let again = call(
        &client,
        "mail_inbox",
        serde_json::json!({ "recipient": "tester" }),
    )
    .await;
    let second: serde_json::Value = serde_json::from_str(&text(&again)).expect("json");
    assert_eq!(second["cursor"], page_top);
    assert_eq!(second["letters"].as_array().map(Vec::len), Some(0));

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

#[tokio::test]
async fn adopt_takes_responsibility_and_refuses_what_it_should() {
    let dir = tempfile::tempdir().unwrap();
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

    let ok = call(
        &client,
        "memory_adopt",
        serde_json::json!({ "key": "claimable" }),
    )
    .await;
    assert!(!ok.is_error.unwrap_or(false), "{}", text(&ok));
    let body: serde_json::Value = serde_json::from_str(&text(&ok)).expect("json");
    assert_eq!(body["owner"], "tester");

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

    let set = call(
        &client,
        "config_set",
        serde_json::json!({ "key": "recess_daily_time", "value": "90m" }),
    )
    .await;
    assert!(!set.is_error.unwrap_or(false), "{}", text(&set));

    let got = call(
        &client,
        "config_get",
        serde_json::json!({ "key": "recess_daily_time" }),
    )
    .await;
    let row: serde_json::Value = serde_json::from_str(&text(&got)).expect("json");
    assert_eq!(row["value"], "90m");

    let listed = call(
        &client,
        "config_list",
        serde_json::json!({ "prefix": "recess_daily_time" }),
    )
    .await;
    let rows: serde_json::Value = serde_json::from_str(&text(&listed)).expect("json");
    assert_eq!(rows["config"].as_array().unwrap().len(), 1);
    assert_eq!(rows["config"][0]["key"], "recess_daily_time");

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

#[tokio::test]
async fn the_core_door_advertises_only_the_hot_path() {
    let dir = tempfile::tempdir().unwrap();
    let state = ellmephant::http::HttpState::new(
        Paths::from_root(dir.path()),
        Config::default(),
        std::sync::Arc::new(ellmephant::http::LockWatch::new()),
        std::time::Instant::now(),
    );
    let app = http_router(
        MemoryServer::new(session(dir.path())).with_identity("tester"),
        "127.0.0.1",
        state,
    );

    let (status, session, body) = post_to(
        &app,
        "/mcp/core",
        None,
        serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
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
    assert!(
        payload(&body)["result"]["instructions"]
            .as_str()
            .unwrap_or("")
            .contains("CORE SURFACE"),
        "the core door names what it is not carrying: {body}"
    );
    let _ = post_to(
        &app,
        "/mcp/core",
        Some(&session),
        serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .await;

    let (status, _, body) = post_to(
        &app,
        "/mcp/core",
        Some(&session),
        serde_json::json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let listed = payload(&body);
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names.len(), ellmephant::mcp::CORE_TOOLS.len());
    assert!(!names.contains(&"memory_dupes"), "hygiene is not core");
    assert!(
        !names.contains(&"channel_prune"),
        "channel admin is not core"
    );

    let (status, _, body) = post_to(
        &app,
        "/mcp/core",
        Some(&session),
        serde_json::json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": { "name": "memory_dupes", "arguments": {} }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        payload(&body)["result"]["isError"] != serde_json::json!(true),
        "a named non-core tool still runs on the core door: {body}"
    );
}
