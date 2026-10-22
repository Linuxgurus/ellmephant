//! MCP server (P7) on `rmcp`: Streamable HTTP (default, the networked transport) or stdio.
//!
//! Tools: `memory_upsert`, `memory_delete`, `memory_search`,
//! `memory_get`, `memory_backlinks`, `memory_upvote`, `memory_stats`,
//! `memory_dupes` — with read-only annotations on the read tools and destructive on `delete`.
//! The tool descriptions carry the SPEC §9 write policy; that is where memory quality is
//! actually tuned. `memory_get` counts a `visits` read and carries the upvote cue — both in its
//! description *and* in the returned payload (`_nudge`), where the model's attention sits after
//! consuming a body; `memory_upvote` bumps the `times_useful` feedback counter.
//!
//! The store is one SQLite file behind a single connection (SPEC §15 #10), so the
//! server holds the [`Session`] behind a mutex and serves requests serially. SQLite and
//! the encoder are synchronous and can hold the lock for a while, so each tool runs its
//! store work on a blocking thread via [`tokio::task::spawn_blocking`] rather than
//! occupying a tokio worker.
//!
//! Two transports, both standard MCP: [`Transport::Http`] (the default) mounts the tool
//! surface at `/mcp` on a listening port via `rmcp`'s `StreamableHttpService` + `axum`;
//! [`Transport::Stdio`] keeps the client-launched pipe.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use rmcp::{ServerHandler, ServiceExt, schemars, tool, tool_handler, tool_router};
use serde::Deserialize;

use crate::cli::session::Session;
use crate::config::{Config, Paths};
// NOTE: do *not* import `crate::error::Result` here — `#[tool_handler]` expands to code that
// uses the unqualified `Result` from `rmcp`, and our single-parameter alias would shadow it.
use crate::error::Error;
use crate::model;
use crate::search::{Bundle, Field, Fields, MatchMode, Query, to_wire_json};
use crate::store::links::backlinks_to_wire_json;
use crate::store::{self, project};

/// Names of the tools this server exposes.
pub const TOOLS: &[&str] = &[
    "whoami",
    "memory_upsert",
    "memory_delete",
    "memory_merge",
    "memory_search",
    "semantic_search",
    "memory_get",
    "memory_history",
    "memory_backlinks",
    "memory_upvote",
    "memory_stats",
    "memory_dupes",
    "memory_reembed",
    "project_upsert",
    "project_get",
    "project_list",
    "claim_acquire",
    "claim_release",
    "claim_list",
    "mail_send",
    "mail_inbox",
    "mail_read",
    "mail_prune",
];

/// The server's name, as `serverInfo` reports it — one source for both doors, so a boot and an
/// `initialize` cannot disagree about who is answering.
pub(crate) const SERVER_NAME: &str = "ellmephant";

/// One tool exactly as `tools/list` presents it, for a surface that is not MCP.
#[derive(Debug, Clone)]
pub(crate) struct ToolCard {
    /// Tool name.
    pub name: String,
    /// The law: what it does, when to use it, what it returns.
    pub description: String,
    /// The JSON Schema the model is handed as `inputSchema`.
    pub input_schema: serde_json::Value,
    /// The tool's annotation hints, as `tools/list` serializes them. The human-readable title lives
    /// in here (`annotations.title`), not at the top level — which is why a card has no `title`.
    pub annotations: serde_json::Value,
}

/// The server-level instruction — the same text an MCP `initialize` carries.
pub(crate) fn instructions() -> &'static str {
    INSTRUCTIONS
}

/// The tool surface as `tools/list` presents it.
///
/// Read from the router the `#[tool]` attributes build, so a door that is not MCP describes the
/// same toolkit from the same source instead of a hand-kept copy: a tool added below appears
/// here for free. `TOOLS` is the name contract; this is the content.
pub(crate) fn tool_catalog() -> Vec<ToolCard> {
    MemoryServer::tool_router()
        .list_all()
        .into_iter()
        .map(|tool| ToolCard {
            name: tool.name.to_string(),
            description: tool
                .description
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default(),
            input_schema: serde_json::Value::Object((*tool.input_schema).clone()),
            annotations: tool
                .annotations
                .as_ref()
                .and_then(|a| serde_json::to_value(a).ok())
                .unwrap_or(serde_json::Value::Null),
        })
        .collect()
}

/// Server-level instruction: the identity, the smart-caveman style, and the one duty that spans
/// every tool (curation), defined once here. Every other law lives in the tool descriptions — each
/// states its own, so nothing is written twice (SPEC §9).
const INSTRUCTIONS: &str = "ellmephant = Memory storage in smart-caveman style — big words in, \
small words out; drop filler, articles, glue. List-shaped reads return a positional table: a \
`cols` header names the fields, rows are bare arrays. Curation is part of the job, not an extra: \
when a memory shapes your task, `memory_upvote` it — the store's own rankings ride on your votes. \
A project is an activity over time — a container you can stand inside: \
`project_upsert` writes one, `project_get` reads one with its `preload`, `project_list` \
shows them all; a memory JOINS a project by carrying the `prj:<key>` TAG (search it — intersect \
with a topic tag to narrow to one facet) and POINTS at one \
from its body with `[[prj:key]]` (a bare `[[key]]` is a memory; keys are plain names, and the two \
tables may share one). \
Tool descriptions are the spec.";

/// `memory_reembed` description: the only path out of a model or dimension change.
const REEMBED_DESCRIPTION: &str = "\
Queue every stored document that needs (re-)embedding, and rebuild the derived index. Needed when \
the encoder changes — a different model, projection or dimension — because the vector table's width \
is fixed when it is created, and the store refuses to reinterpret stored vectors rather than hand \
back wrong neighbours. It also re-parses every body to rebuild the `[[link]]` graph, whose edges are \
computed at write time and so do not follow a parser change on their own. It repairs a torn index \
too: a document with no vector is queued, and the index is what says so — nothing on the row claims \
it was indexed. The rows \
(`memory` — keys, tags, titles, bodies; `project` — a project's own fields) are the source of truth; \
everything this touches is derived, so nothing written can be lost and re-running it is harmless. \
It is a PRODUCER, not an embedder: it returns immediately with `{queued: N}` and a background worker \
embeds the documents one at a time, so a rebuild is no longer an hour-long write outage. Only a \
dimension change still blocks, and only for the table's drop and recreate.";

const UPSERT_DESCRIPTION: &str = "\
Upsert one memory. WRITE in smart-caveman. One topic per memory — closely clustered facts, one \
per line; never a misc/gotchas bucket. Before saving, check: body hold ≥2 distinct topics? Split — \
one call each, each atom stands alone (≥2-3 facts). Trim junk on edit — deepen the topic, never add \
a second. Merge redundant memories by hand: weaker is folded into stronger and `memory_delete` \
Skip facts cheap to derive, live state, or one-off. PATCHING: every field is optional — omit one and \
the stored value is left alone, so a single field can change without resending the row. Empty is an \
instruction: `source: \"\"` clears it and `tags: []` clears them, but `title`/`body` may never be \
empty. A new key needs both a `title` and a `body`. The reply's \
`queued` says whether this write scheduled an embed: a written document is indexed a moment later by \
the store's encode worker, so a semantic search straight after a write may not see it yet. Nothing on \
the ROW records encoding state — the queue holds the work and the index holds the result, by design.";

/// `memory_upvote` description. The feedback nudge, carried here only.
const UPVOTE_DESCRIPTION: &str = "Upvote the memories that shaped this task — one that changed a \
decision, dodged a bug, or you followed its law. A bare fetch is a visit, not this. Pass every key \
that earned its keep in one call; under-voting starves the store's own rankings.";

/// The firing cue that rides the `memory_get` *result* payload, not just its static description.
/// The model's attention is on the body it just consumed — a trigger fires where attention sits at
/// the moment of use (cf. the telemetry lesson).
pub(crate) const GET_NUDGE: &str = "Shaped your task? memory_upvote this key.";

/// `memory_history` description — the read law for history: metadata only, two shapes, and the fact
/// that a key does not have to exist to have one.
const HISTORY_DESCRIPTION: &str = "\
List a memory's versions: the states its DOCUMENT (`title` + `body`) held before each change, oldest \
first — so `n = 1` is the first state that was replaced, and the state you see in the live row is the \
one that is NOT here. Bodies are not returned: pick an `n` from this list, then read it with \
`memory_get version=N`. Omit `key` for the INDEX of every key holding versions, where `orphan` marks \
keys whose memory is GONE — that history is then the only copy of its content, which is why a delete \
leaves something findable. A version is reached by ADDRESS and is never embedded: `memory_history KEY` \
answers for a deleted key exactly as it does for a live one.";

/// Cap on rows returned by a bare `memory_history` call; the reply always states the true total, so a
/// truncated list is a visible fact rather than a silent one.
pub(crate) const HISTORY_LIMIT: usize = 100;

/// `memory_dupes` description. A finder for the merge rule, not a merger.
const DUPES_DESCRIPTION: &str = "\
Find duplicate *candidates*: memory pairs whose documents sit closer than a tight `max_distance` \
(default 0.05). Nearest first, titles shown. Candidates, not verdicts — `memory_get` both before \
merging. The finder for the merge rule.";

/// Cap on returned dupe pairs for a bare `memory_dupes` call.
pub(crate) const DUPES_LIMIT: usize = 50;

/// `memory_search` description. The read law lives here — the semantic phrase, the literal/regex
/// leg, the structured filters, the lean `key`-only default, and the narrow-to-wide widen trigger.
const SEARCH_DESCRIPTION: &str = "\
Hunt memory by meaning (`query`) or by literal (`pattern`) — semantics reach sense; `pattern` reaches \
a half-remembered string (a key, a filename, an error). `mode` reads `pattern` as `literal` (a \
substring, the default) or `regex`. Both legs and the filters intersect (all must hold); omit all to \
filter the whole store. `created_after`/`created_before` and `updated_after`/`updated_before` are ISO \
windows over the timestamps (`updated_after` is the \"what changed since I last woke\" filter). Hits \
are `key`-only by default; `fields` adds more, body never. `memory_get` \
the hit(s) you'll use; a known `key` goes straight there. Nothing fits? Widen — raise `k`, loosen \
`max_distance`.";

/// `memory_merge` description.
const MERGE_DESCRIPTION: &str = "\
Integrate one memory into another and delete it: every line of `from`'s body that `into` does not \
already carry is appended to `into` VERBATIM — nothing is lost, nothing is paraphrased — then every \
`[[from]]` reference is repointed at `into` (bodies, the `[[…]]` graph, and any project `preload` \
naming it) and `from` is deleted. `into` SURVIVES: name the keeper as `into`, because you are judging \
which of the two is the stronger memory. Lines `into` already carries are not appended, so a merge \
REMOVES redundancy — that is what it is for, and a merge that only ever appends is the thing this \
replaces. Fold the prose yourself first (an ordinary upsert) when you want the survivor to read as \
one thing rather than as an append: nothing is then missing, `integrated` comes back empty, and the \
call has nothing to integrate. Returns `integrated` — the lines it appended on your behalf — so the fold it \
made for you is visible. Both keys must exist: a merge into a typo would destroy the source. To \
discard prose on purpose use `memory_delete`: a merge integrates, a delete discards. `[[prj:from]]` \
is not rewritten (that names a project). Reports what moved.";

/// `project_upsert` description. The project write law lives here and *only* here.
const PROJECT_UPSERT_DESCRIPTION: &str = "\
Create or patch a project — an activity completed over time: identity, status, prose, and an \
ordered `preload`. A project is the structure half of the store and a container you can stand \
inside; facts go in memories, where the store matches them by meaning. `key` is a plain slug — the \
name is the identity, `prj:`/`prj-` are accepted and stripped, and a memory may share it. \
`status` is todo|active|paused|done|aborted — `done` and `aborted` both end a project, and there is no delete; \
`owner` is a set of names, e.g. [\"Cline\",\"James\"] — free text, since the store keeps no list of \
who exists (and `me` would mean whatever reader asks). `preload` is the ORDERED list of memory keys to load as \
context to work \
in here; it replaces the previous list, and it may name memories this project did not author \
(conventions, boot notes). `start_date`/`end_date` are ISO dates (`YYYY-MM-DD`): `start_date` is \
yours if you state one, else it is stamped the moment the project stops being `todo` — a `todo` \
project has NOT started — and it is never cleared, so a start on a `todo` project means it was \
worked on once and parked. `end_date` is stamped when `status` becomes done|aborted — `status` says \
HOW it ended, the date only says WHEN, and reopening clears it. PATCHING: every field is optional — \
omit one and the stored value is left alone, so `project_get` is NOT required to edit a single \
field. Empty is an instruction: `goal: \"\"`/`state: \"\"` clear, `owner: []` unassigns, and \
`tags: []`/`preload: []` clear — but `title`/`status` may never be empty (an empty `status` is \
refused, not quietly read as `todo`). A new key needs a `title`. A memory joins a project by \
carrying the `prj:<key>` TAG; nothing links it. List a project's memories with `memory_search`'s \
`tags` filter.";

/// `project_get` description.
const PROJECT_GET_DESCRIPTION: &str = "\
Read one project whole, with its `preload` resolved — the ordered context to load (project -> \
memory), titles hydrated. A referenced key whose title is null resolves to nothing: a forward \
reference or rot. The read reports it rather than hiding it. `hydrate` (`none` default, `snippet`, \
`full`) optionally carries each preload memory's BODY, so a project can be a warm start rather than \
a reading list; the bodies ride in a sister `bodies` table, in preload order. Membership is a TAG, \
not a relation: a project's memories are the ones carrying `prj:<key>`, found with `memory_search`'s \
`tags` filter (intersect with a topic tag to narrow to one facet).";

/// `project_list` description.
const PROJECT_LIST_DESCRIPTION: &str = "\
List projects — the cheap index over them (`key`, `status`, `owner`, `tags`, `title`, `goal`, \
`start_date`, `end_date`; never `state`, which is a `project_get` away). Filters intersect: `status`, `tags` (every listed \
tag must be present) and a literal/regex `pattern` over `key`/`title`/`goal`/`state`. Omit them \
all to list every project.";

/// Parameters for [`MemoryServer::memory_upsert`]. Every field is optional: an omitted field keeps
/// the value already stored under `key`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpsertParams {
    /// Stable slug.
    #[serde(default)]
    #[schemars(description = "Stable slug. Reuse patches that memory; a new key is created.")]
    pub key: Option<String>,
    /// Salient one-liner.
    #[serde(default)]
    #[schemars(description = "One sharp line. Omit to keep the stored one; never empty.")]
    pub title: Option<String>,
    /// The facts.
    #[serde(default)]
    #[schemars(
        description = "Facts, one per line. Link memory with `[[other-key]]`. Omit to keep the stored body; never empty."
    )]
    pub body: Option<String>,
    /// Provenance.
    #[serde(default)]
    #[schemars(
        description = "Proof stone: command/path/repo that derive this. Never live state. Omit to keep it; an empty string clears it."
    )]
    pub source: Option<String>,
    /// Where the information came from, when it is not the speaker.
    #[serde(default)]
    #[schemars(
        description = "Where this came from, when it is NOT you: a URL, a file, a command, `my inference`. Recorded BESIDE the store-stamped `asserted_by` (who spoke), so relaying a fact never makes you its source. Omit when you are the origin."
    )]
    pub origin: Option<String>,
    /// Aspect tags.
    #[serde(default)]
    #[schemars(
        description = "Aspects of the subject, e.g. storage, gotchas. Omit to keep the stored tags; `[]` clears them."
    )]
    pub tags: Option<Vec<String>>,
    /// Keep this row to the writing pen alone.
    #[serde(default)]
    #[schemars(
        description = "Make this row PRIVATE to the writing pen (`true`), or public (`false`). Omit to keep the stored audience. Private is the READ tier: the row is invisible to every other pen — not as a hit, a title, a count or a backlink — and readable by you alone, plus the owner's recorded forensic path. The audience is derived from the PROVEN identity, so this flag cannot name someone else. Unencrypted: the file, the snapshot and the off-cluster copy hold it in plaintext, so it protects against another agent using the tools and against nothing else."
    )]
    pub private: Option<bool>,
    /// Why this change is being made, for the version it supersedes.
    #[serde(default)]
    #[schemars(
        description = "Why THIS change — recorded beside the version this write creates, so a later reader (`memory_history`) learns the intent instead of reconstructing it. Only a change to `title`/`body` creates a version; a note alone does not."
    )]
    pub version_note: Option<String>,
    /// The CAS token: the `version` a `memory_get` returned.
    #[serde(default)]
    #[schemars(
        description = "Write only if the memory is STILL at this `version` (the field `memory_get` returns). A mismatch writes nothing and returns `status: conflict` with the current version — the safe way to read-modify-write (a bank balance, a counter). `0` means 'this key does not exist yet'. Omit for last-write-wins."
    )]
    pub expected_version: Option<i64>,
}

impl UpsertParams {
    /// Convert the request into a partial write — `None` means "leave this field alone".
    fn into_patch(self) -> store::Patch {
        store::Patch {
            key: self.key,
            title: self.title,
            body: self.body,
            source: self.source,
            // Stamped by the caller of `into_patch` from the connection's PROVEN identity — the
            // request payload cannot set it, because `asserted_by` is not a param.
            asserted_by: None,
            origin: self.origin,
            tags: self.tags,
            private: self.private,
            version_note: self.version_note,
            expected_version: self.expected_version,
        }
    }
}

/// `semantic_search` description: the store-wide search, and the graveyard behind a flag.
const SEMANTIC_SEARCH_DESCRIPTION: &str = "\
Search the whole store by meaning: every document type that has vectors, one `type` if you name it, \
or the GRAVEYARD of deleted documents when `deleted` is true. Live and deleted results are never \
merged — a tombstone hit gives `type`/`key`/`distance`/`deleted_at` and no body, because a deleted \
document is a name to look up rather than text to read (`memory_history KEY` holds what it said). \
`memory_search` remains the memory-only alias.";

/// Parameters for `semantic_search` — the store-wide search, with the two knobs `memory_search`
/// deliberately does not have.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SemanticSearchParams {
    /// The phrase to match.
    #[serde(default)]
    #[schemars(description = "Phrase to hunt with, matched against each document's embedding.")]
    pub query: Option<String>,
    /// Restrict to one document type. ★ The NAME IS `type`, because that is what the tool description
    /// promised and what callers sent: the field was `kind` here, so `type` was silently discarded by
    /// serde and every filtered search returned the whole store (soma/ellmephant#87). `kind` remains
    /// an alias for compatibility; the advertised name is the one that works.
    #[serde(rename = "type", alias = "kind", default)]
    #[schemars(
        description = "Restrict to one document type (`memory`, `project`). Absent = every type that has vectors."
    )]
    pub doc_type: Option<String>,
    /// Search deleted documents instead of live ones.
    #[serde(default)]
    #[schemars(
        description = "Search the GRAVEYARD: documents that were deleted. Never merged with live hits — it is a different index. Returns keys and `deleted_at`, never bodies."
    )]
    pub deleted: Option<bool>,
    /// Result count.
    #[serde(default)]
    #[schemars(description = "Most hits to bring back (default = configured k).")]
    pub k: Option<usize>,
    /// Cosine-distance ceiling.
    #[serde(default)]
    #[schemars(description = "Cosine-distance ceiling (0-2). Smaller = closer.")]
    pub max_distance: Option<f32>,
    /// Extra hit fields.
    #[serde(default)]
    #[schemars(
        description = "Extra hit fields for live hits (tombstones always return `type`, `key`, `distance`, `deleted_at`)."
    )]
    pub fields: Vec<FieldParam>,
}

/// Parameters for [`MemoryServer::memory_search`].
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchParams {
    /// The phrase to match.
    #[serde(default)]
    #[schemars(description = "Phrase to hunt with. Matched against embedded document.")]
    pub query: Option<String>,
    /// The literal/regex pattern.
    #[serde(default)]
    #[schemars(
        description = "Literal/regex pattern over `key`/`title`/`body` — for a half-remembered literal (a key, a filename, an error string). Intersects with `query` and the filters. Blank = no lexical leg."
    )]
    pub pattern: Option<String>,
    /// How `pattern` is read.
    #[serde(default)]
    #[schemars(description = "How `pattern` is read: `literal` (substring, default) or `regex`.")]
    pub mode: Option<MatchModeParam>,
    /// Result count.
    #[serde(default)]
    #[schemars(description = "Most hits bring back (default = configured k).")]
    pub k: Option<usize>,
    /// Cosine-distance ceiling.
    #[serde(default)]
    #[schemars(description = "Cosine-distance ceiling (0-2). Smaller = closer.")]
    pub max_distance: Option<f32>,
    /// Link-neighbourhood hops.
    #[serde(default)]
    #[schemars(description = "Link-neighbour hops drag in (0 = off).")]
    pub expand: Option<u8>,
    /// Tag filter (AND).
    #[serde(default)]
    #[schemars(description = "Tag filter; a memory must carry EVERY listed tag.")]
    pub tags: Vec<String>,
    /// Minimum embedded-document size.
    #[serde(default)]
    #[schemars(description = "Minimum embedded-document size, in chars.")]
    pub min_chars: Option<usize>,
    /// Maximum embedded-document size.
    #[serde(default)]
    #[schemars(description = "Maximum embedded-document size, in chars.")]
    pub max_chars: Option<usize>,
    /// Lower bound on `created`.
    #[serde(default)]
    #[schemars(
        description = "Only memories created at or after this ISO timestamp (`YYYY-MM-DD` or `YYYY-MM-DD HH:MM:SS`). Compared lexicographically, so a date bounds the whole day."
    )]
    pub created_after: Option<String>,
    /// Upper bound on `created`.
    #[serde(default)]
    #[schemars(description = "Only memories created at or before this ISO timestamp.")]
    pub created_before: Option<String>,
    /// Lower bound on `updated`.
    #[serde(default)]
    #[schemars(
        description = "Only memories last written at or after this ISO timestamp — the \"what changed since I last woke\" filter."
    )]
    pub updated_after: Option<String>,
    /// Upper bound on `updated`.
    #[serde(default)]
    #[schemars(description = "Only memories last written at or before this ISO timestamp.")]
    pub updated_before: Option<String>,
    /// Hit fields to return beyond `key`.
    #[serde(default)]
    #[schemars(
        description = "Extra hit fields to return, alongside the always-present `key`. Omit for `key` only (lean). E.g. `[\"title\",\"tags\",\"distance\"]`."
    )]
    pub fields: Vec<FieldParam>,
}

/// A hit field a caller can request on `memory_search` (the `fields` selection). `key` is always
/// returned, so naming it here is a harmless no-op.
#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FieldParam {
    /// Slug.
    Key,
    /// Salient line.
    Title,
    /// Attached tags.
    Tags,
    /// Embedded-document length (chars).
    Chars,
    /// Cosine distance (semantic ranking only).
    Distance,
}

impl FieldParam {
    /// Map the wire enum onto the search-layer [`Field`].
    fn to_field(self) -> Field {
        match self {
            FieldParam::Key => Field::Key,
            FieldParam::Title => Field::Title,
            FieldParam::Tags => Field::Tags,
            FieldParam::Chars => Field::Chars,
            FieldParam::Distance => Field::Distance,
        }
    }
}

/// How a `pattern` is read on `memory_search` (the lexical leg).
#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MatchModeParam {
    /// A plain substring, case-insensitive; regex metacharacters are literal text.
    Literal,
    /// A regex, case-insensitive by default (opt out with a leading `(?-i)`).
    Regex,
}

impl MatchModeParam {
    /// Map the wire enum onto the search-layer mode.
    fn to_mode(self) -> MatchMode {
        match self {
            MatchModeParam::Literal => MatchMode::Literal,
            MatchModeParam::Regex => MatchMode::Regex,
        }
    }
}

/// A single-key request.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KeyParams {
    /// The memory's key.
    #[schemars(description = "Memory key. Slug.")]
    pub key: String,
}

/// Parameters for [`MemoryServer::memory_get`].
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetParams {
    /// The memory's key.
    #[schemars(description = "Memory key. Slug.")]
    pub key: String,
    /// Read a PAST state instead of the live one.
    #[schemars(
        description = "Read version `n` instead of the live row — the state that change replaced. `memory_history KEY` lists them. Counts no `visits`: a past state is not the memory."
    )]
    #[serde(default)]
    pub version: Option<i64>,
}

/// Parameters for [`MemoryServer::memory_history`].
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HistoryParams {
    /// The key whose versions to list; omitted, the reply is the index of every key that has one.
    #[schemars(
        description = "Memory key. Slug. OMIT for the index of every key holding versions — which is how a key whose memory is GONE is found at all (`orphan`)."
    )]
    #[serde(default)]
    pub key: Option<String>,
    /// Index mode only: just the orphans.
    #[schemars(
        description = "Index mode only (`key` omitted): return just the keys whose memory is GONE — history that is now the only copy of its content."
    )]
    #[serde(default)]
    pub orphans_only: bool,
    /// Cap the rows returned.
    #[schemars(description = "Most rows to return. The reply states the true total regardless.")]
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Parameters for [`MemoryServer::memory_merge`].
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MergeParams {
    /// The key to delete.
    #[schemars(
        description = "The key to delete — the weaker memory. Its lines are integrated into `into`, its `[[…]]` references are repointed at `into`, then it is deleted."
    )]
    pub from: String,
    /// The key that survives.
    #[schemars(
        description = "The key that survives — the stronger memory. You are judging which is which by naming it here. Any line of `from` this does not already carry is appended to it verbatim, so its information is in it when the call returns."
    )]
    pub into: String,
}

/// A many-key request (feedback sweep).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KeysParams {
    /// The memories' keys.
    #[schemars(description = "Memory keys (one or more). Slugs.")]
    pub keys: Vec<String>,
}

/// Parameters for [`MemoryServer::memory_stats`].
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StatsParams {
    /// Include the full tag vocabulary.
    #[serde(default)]
    #[schemars(
        description = "Also return `all_tags` — the FULL tag vocabulary with counts (count-desc, then name). Off by default so the digest stays lean; on, it is how you see the whole tag set (and its sprawl) instead of inventing a synonym."
    )]
    pub tags: Option<bool>,
}

/// Parameters for a no-argument tool (e.g. `memory_reembed`).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListParams {}

/// Parameters for [`MemoryServer::claim_acquire`].
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClaimAcquireParams {
    /// The name to lease.
    #[schemars(
        description = "The name to lease — free text, a handle two writers share (a `hands-bank` key, a handoff topic). Trimmed; must not be empty."
    )]
    pub name: String,
    /// Who is taking it.
    #[schemars(
        description = "Who is taking it — your own identifier. The SAME owner re-acquiring RENEWS rather than conflicts; a DIFFERENT owner is refused while the lease is live."
    )]
    pub owner: String,
    /// Lease length in seconds.
    #[serde(default)]
    #[schemars(
        description = "Lease length in seconds (default 900, max 86400). The lease EXPIRES on its own, so a crashed holder frees the name without help."
    )]
    pub ttl_seconds: Option<i64>,
}

/// Parameters for [`MemoryServer::claim_release`].
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClaimReleaseParams {
    /// The name to release.
    #[schemars(description = "The name the lease is over.")]
    pub name: String,
    /// The owner that must match.
    #[schemars(
        description = "Who is releasing — must be the current holder. A non-owner is refused and told who holds it; release is not a way to steal a lease."
    )]
    pub owner: String,
}

/// Parameters for [`MemoryServer::claim_list`].
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClaimListParams {
    /// Limit to one name.
    #[serde(default)]
    #[schemars(
        description = "Optional: one name to inspect. Omit for every claim, EXPIRED rows included — the audit view, so an unexplained stale claim is visible rather than hidden."
    )]
    pub name: Option<String>,
}

/// Parameters for [`MemoryServer::mail_send`].
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MailSendParams {
    /// Who is writing.
    #[schemars(
        description = "Who is writing: `stet` for the self, or `stet/finger-<id>` for one of its fingers. ASSERTED by the caller, not authenticated by the store."
    )]
    pub sender: String,
    /// The self the letter is addressed to.
    #[schemars(
        description = "The SELF the letter is addressed to (`stet`, `twine`). Never a finger: a finger is how a self ANSWERS mail, not who receives it."
    )]
    pub recipient: String,
    /// One line.
    #[serde(default)]
    #[schemars(description = "One line, so an inbox can be scanned without fetching bodies.")]
    pub subject: Option<String>,
    /// The letter itself.
    #[schemars(
        description = "The letter. Free text — no verbs, no priority, no task fields. If you want something done, ASK, and let the reader decide."
    )]
    pub body: String,
    /// How to read the body.
    #[serde(default)]
    #[schemars(
        description = "How the body is encoded: `text` (default), `json`, `gzip+base64`. Declared so the reader does not spend a finger decoding gibberish."
    )]
    pub encoding: Option<String>,
}

/// Parameters for [`MemoryServer::mail_inbox`].
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MailInboxParams {
    /// Whose box to read.
    #[schemars(description = "Whose box: the self whose mail this is (`stet`, `twine`).")]
    pub recipient: String,
    /// The reader's cursor.
    #[serde(default)]
    #[schemars(
        description = "Your cursor: the highest `seq` you have already dealt with (0 for everything). Keep it in YOUR store as `mail-last-read` and advance it only AFTER handling the letters — the advance is the commit."
    )]
    pub since: Option<i64>,
    /// Long-poll.
    #[serde(default)]
    #[schemars(
        description = "Block up to this many seconds (max 25) and return the moment a letter lands. This is how a hand waits for mail without polling; omit it to check and return immediately."
    )]
    pub wait_seconds: Option<u64>,
    /// Page size.
    #[serde(default)]
    #[schemars(description = "Most envelopes to return (default 50, max 500).")]
    pub limit: Option<usize>,
}

/// Parameters for [`MemoryServer::mail_read`].
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MailReadParams {
    /// Which letter.
    #[schemars(description = "The `seq` from `mail_inbox`.")]
    pub seq: i64,
}

/// Parameters for [`MemoryServer::mail_prune`].
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MailPruneParams {
    /// Retention window.
    #[schemars(
        description = "Delete letters older than this many days. ★ Letters expire because they are kept for a reader, not for the truth; memories have no such path."
    )]
    pub keep_days: i64,
}

/// Parameters for [`MemoryServer::memory_dupes`].
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DupesParams {
    /// Cosine-distance ceiling (tight by default).
    #[serde(default)]
    #[schemars(description = "Cosine-distance ceiling; tight by default. Smaller = stricter.")]
    pub max_distance: Option<f32>,
    /// Cap on returned pairs.
    #[serde(default)]
    #[schemars(description = "Most pairs to return, nearest first.")]
    pub limit: Option<usize>,
}

/// Parameters for [`MemoryServer::project_get`].
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectKeyParams {
    /// The project's key.
    #[schemars(description = "Project key — a plain slug (`prj:`/`prj-` accepted and stripped).")]
    pub key: String,
    /// How much of each preload memory's body to carry.
    #[serde(default)]
    #[schemars(
        description = "How much of each `preload` memory's BODY to carry: `none` (default — keys and titles only, a reading list), `snippet` (first ~280 chars), `full` (whole body). Order is preserved; a key that resolves to nothing is shown with a null title, not dropped."
    )]
    pub hydrate: Option<String>,
}

/// Parameters for [`MemoryServer::project_upsert`].
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectUpsertParams {
    /// Stable slug.
    #[serde(default)]
    #[schemars(description = "Stable slug — a plain name; reuse replaces that project.")]
    pub key: Option<String>,
    /// Salient one-liner.
    #[serde(default)]
    #[schemars(description = "One sharp line. Omit to keep the stored one; never empty.")]
    pub title: Option<String>,
    /// What this is for.
    #[serde(default)]
    #[schemars(description = "What this is for. One line.")]
    pub goal: Option<String>,
    /// Where it stands.
    #[serde(default)]
    #[schemars(description = "Mutable current-state prose: where it stands, what's next.")]
    pub state: Option<String>,
    /// Lifecycle.
    #[serde(default)]
    #[schemars(
        description = "todo | active | paused | done | aborted. Omit to keep the stored status (a fresh project is `todo`); never empty."
    )]
    pub status: Option<String>,
    /// Whose it is.
    #[serde(default)]
    #[schemars(
        description = "Whose it is: a set of names, e.g. [\"Cline\",\"James\"]. Free text; empty = unassigned."
    )]
    pub owner: Option<Vec<String>>,
    /// Aspect tags.
    #[serde(default)]
    #[schemars(
        description = "Aspects of the subject, e.g. continuity, storage. Omit to keep them; `[]` clears them."
    )]
    pub tags: Option<Vec<String>>,
    /// The ordered context to load.
    #[serde(default)]
    #[schemars(
        description = "ORDERED memory keys to load to work in here; replaces the previous list. May name memories this project did not author."
    )]
    pub preload: Option<Vec<String>>,
    /// When the work started.
    #[serde(default)]
    #[schemars(
        description = "When the work started, ISO `YYYY-MM-DD`. Leave it out and it is stamped the moment the project stops being `todo` (a `todo` project has not started); it is never cleared. State one to backfill a late entry."
    )]
    pub start_date: Option<String>,
    /// When it ended.
    #[serde(default)]
    #[schemars(
        description = "When it ended, ISO `YYYY-MM-DD` — only with status done|aborted (both END a project; `status` says how, this says when). Stamped when omitted, cleared if reopened."
    )]
    pub end_date: Option<String>,
}

impl ProjectUpsertParams {
    /// Convert the request into a partial write — `None` means "leave this field alone".
    fn into_patch(self) -> project::Patch {
        project::Patch {
            key: self.key,
            title: self.title,
            goal: self.goal,
            state: self.state,
            status: self.status,
            owner: self.owner,
            tags: self.tags,
            preload: self.preload,
            start_date: self.start_date,
            end_date: self.end_date,
        }
    }
}

/// Parameters for [`MemoryServer::project_list`].
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectListParams {
    /// Lifecycle filter.
    #[serde(default)]
    #[schemars(description = "Only this status.")]
    pub status: Option<String>,
    /// Tag filter (AND).
    #[serde(default)]
    #[schemars(description = "Tag filter; a project must carry EVERY listed tag.")]
    pub tags: Vec<String>,
    /// Owner filter (AND).
    #[serde(default)]
    #[schemars(
        description = "Owner filter; a project must list EVERY given name (case-insensitive)."
    )]
    pub owner: Vec<String>,
    /// The literal/regex pattern.
    #[serde(default)]
    #[schemars(
        description = "Literal/regex pattern over `key`/`title`/`goal`/`state`. Blank = no lexical leg."
    )]
    pub pattern: Option<String>,
    /// How `pattern` is read.
    #[serde(default)]
    #[schemars(description = "How `pattern` is read: `literal` (substring, default) or `regex`.")]
    pub mode: Option<MatchModeParam>,
}

/// The caller's PROVEN identity, read from the HTTP request that carried this call.
///
/// `Ok(None)` means nothing was claimed — a stdio transport has no HTTP request, and a client that
/// sends no headers has claimed nothing. `Err` means a name was claimed and could not be proved, and
/// it is REFUSED rather than downgraded to `anonymous`: an unproven name is a lie with a signature.
fn identity_of(
    ctx: &rmcp::service::RequestContext<rmcp::RoleServer>,
) -> Result<Option<String>, crate::identity::Forged> {
    let Some(parts) = ctx.extensions.get::<axum::http::request::Parts>() else {
        return Ok(None);
    };
    crate::identity::resolve(&parts.headers)
}

/// ★ A WRITE REQUIRES A PEN. Reads are open to anyone; a write with NO proven identity is REFUSED,
/// not stamped `anonymous`. Anonymous is an honest answer to "who spoke", but the store has no
/// reason to accept an unsigned hand: an anonymous write is indistinguishable from one nobody meant,
/// and it cannot be held to anything. Every refusal costs one header pair to fix; `whoami` says what
/// this connection currently is.
fn pen_name(
    ctx: &rmcp::service::RequestContext<rmcp::RoleServer>,
    declared: Option<&str>,
) -> std::result::Result<String, String> {
    match identity_of(ctx) {
        Ok(Some(name)) => Ok(name),
        // ★ A TRANSPORT WITH NO REQUEST MAY DECLARE ITS PEN ONCE, at construction: a stdio or
        // in-process server is started BY a process and is as trusted as that process. HTTP
        // transports ignore this — their headers decide, and an unproven header is still refused.
        Ok(None) => match declared {
            Some(name) => Ok(name.to_string()),
            None => Err("refused: a write requires a proven identity — send `X-Stet-From` and `X-Stet-Auth` (reads need neither). `whoami` reports what this connection is."
                .to_string()),
        },
        Err(forged) => Err(forged.to_string()),
    }
}

/// The MCP server. Holds the single store [`Session`] behind a mutex; `#[tool_handler]`
/// reaches the tool table through the generated `Self::tool_router()`, so there is no field.
#[derive(Clone)]
pub struct MemoryServer {
    session: Arc<Mutex<Session>>,
    watch: Arc<crate::http::LockWatch>,
    /// Read-side handholds, taken once at construction so that a READ never has to touch `session`:
    /// the writer holds that for the whole of a write, embedding included, and a reader queueing on
    /// it is precisely the stall this split removes.
    paths: crate::config::Paths,
    cfg: crate::config::Config,
    encoder: Arc<super::cli::session::Encoder>,
    /// ★ A DECLARED PEN, for transports that carry no HTTP request (stdio, in-process). A server
    /// started BY a process is as trusted as that process, so it may say who it is once, at
    /// construction; every write it makes is stamped with that name instead of being refused as
    /// anonymous. HTTP transports ignore this — their headers decide.
    declared: Option<String>,
    /// ★ WHO MAY READ WHOSE MAIL, stated out loud: `(from, to)` pairs from
    /// `ELLMEPHANT_MAIL_SURROGATES=hands=stet`. The hand that serves `stet` authenticates as `hands`,
    /// so without a declared surrogate the recipient-only rule would lock my own hand out of the
    /// mail it exists to read. Every exception is written here instead of being implied.
    surrogates: Vec<(String, String)>,
}

/// Parse `a=b,c=d` into pairs. Blank or malformed entries are dropped rather than guessed at.
fn parse_surrogates(raw: &str) -> Vec<(String, String)> {
    raw.split(',')
        .filter_map(|pair| {
            let (from, to) = pair.split_once('=')?;
            let (from, to) = (from.trim(), to.trim());
            (!from.is_empty() && !to.is_empty()).then(|| (from.to_string(), to.to_string()))
        })
        .collect()
}

impl MemoryServer {
    /// Build a server over an open session — the `ellmephant mcp` path.
    pub fn new(session: Session) -> Self {
        Self::from_shared(Arc::new(Mutex::new(session)))
    }

    /// Build a server over a session already behind a shared lock (tests).
    pub fn from_shared(session: Arc<Mutex<Session>>) -> Self {
        Self::with_watch(session, Arc::new(crate::http::LockWatch::new()))
    }

    /// Build a server sharing a writer witness with the plain-HTTP surface.
    ///
    /// The read-side handholds are read out of the session here, once, while nothing else holds it.
    pub fn with_watch(session: Arc<Mutex<Session>>, watch: Arc<crate::http::LockWatch>) -> Self {
        let (paths, cfg, encoder) = {
            let guard = session.lock().unwrap_or_else(|e| e.into_inner());
            (guard.paths().clone(), guard.cfg().clone(), guard.encoder())
        };
        Self {
            session,
            watch,
            paths,
            cfg,
            encoder,
            declared: None,
            surrogates: parse_surrogates(
                &std::env::var("ELLMEPHANT_MAIL_SURROGATES").unwrap_or_default(),
            ),
        }
    }

    /// ★ ONLY THE RECIPIENT READS ITS MAIL. `mail_inbox` takes a recipient as an argument, so without
    /// this any caller could read any box. A proven identity reads its own box, or a box an explicit
    /// surrogate map hands it; anonymous is refused before the box is even considered.
    fn mail_box_for(
        &self,
        ctx: &rmcp::service::RequestContext<rmcp::RoleServer>,
        requested: &str,
    ) -> std::result::Result<String, String> {
        let who = pen_name(ctx, self.declared.as_deref())?;
        if who.eq_ignore_ascii_case(requested) {
            return Ok(who);
        }
        let allowed = self.surrogates.iter().any(|(from, to)| {
            from.eq_ignore_ascii_case(&who) && to.eq_ignore_ascii_case(requested)
        });
        if allowed {
            return Ok(who);
        }
        Err(format!(
            "refused: `{who}` may not read `{requested}`'s mail — only the recipient, or a declared surrogate, may"
        ))
    }

    /// Declare the pen for a transport with no HTTP request (stdio / in-process). Writes are then
    /// stamped with this name; without it they are REFUSED, because anonymous is read-only.
    pub fn with_identity(mut self, name: impl Into<String>) -> Self {
        self.declared = Some(name.into());
        self
    }

    /// The writer witness this server marks while a tool holds the store lock.
    pub fn watch(&self) -> Arc<crate::http::LockWatch> {
        Arc::clone(&self.watch)
    }

    /// Run `f` against the store on a blocking thread, turning an [`Error`] into a
    /// tool-error string. SQLite and the encoder can hold the store lock for a while, so
    /// this must not occupy a tokio worker.
    ///
    /// `tool` labels the call in logs: a success is a `debug!` with its latency, a store
    /// error a `warn!`, and a panic/join failure an `error!`. That is the one place every
    /// tool failure surfaces, so `docker logs` never loses it to a client-side error string.
    pub(crate) async fn with_store<T, F>(
        &self,
        tool: &'static str,
        f: F,
    ) -> std::result::Result<T, String>
    where
        T: Send + 'static,
        F: FnOnce(&mut Session) -> crate::error::Result<T> + Send + 'static,
    {
        let session = Arc::clone(&self.session);
        let watch = Arc::clone(&self.watch);
        let started = std::time::Instant::now();
        let outcome = tokio::task::spawn_blocking(move || {
            // The single connection is a mutex; a long re-embed or model load queues here.
            let wait = std::time::Instant::now();
            let mut guard = session
                .lock()
                .map_err(|_| "memory store lock poisoned".to_string())?;
            tracing::debug!(
                tool,
                wait_ms = wait.elapsed().as_millis() as u64,
                "store lock acquired"
            );
            // Witness the holder for exactly as long as the lock is held.
            let _held = watch.held(tool);
            f(&mut guard).map_err(|e| e.to_string())
        })
        .await;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        match outcome {
            Ok(Ok(value)) => {
                tracing::debug!(tool, elapsed_ms, "tool ok");
                Ok(value)
            }
            Ok(Err(err)) => {
                tracing::warn!(tool, elapsed_ms, error = %err, "tool error");
                Err(err)
            }
            Err(join) => {
                let err = format!("memory store task failed: {join}");
                tracing::error!(tool, elapsed_ms, error = %err, "tool task failed");
                Err(err)
            }
        }
    }

    /// Run `f` against the store on a READ-ONLY session of its own: its own SQLite connection, the
    /// shared encoder, and **no store lock at all**.
    ///
    /// This is the difference between a read that waits and a read that answers. A write holds
    /// `session` for its whole duration — the embedding included, and the embedding is the slow part
    /// (~1.3-8.5 s per KB of body) — so a reader that queued on that lock waited out the write. In
    /// WAL mode a reader alongside a writer is exactly what SQLite does well: it sees the last
    /// committed state, which `/health` has always demonstrated by answering in 0.02s during a 51s
    /// write, purely because it never touched the session.
    ///
    /// The cost is one `open` per read (tens of microseconds), which buys reads that do not care how
    /// long a write takes.
    pub(crate) async fn with_read<T, F>(
        &self,
        tool: &'static str,
        f: F,
    ) -> std::result::Result<T, String>
    where
        T: Send + 'static,
        F: FnOnce(&Session) -> crate::error::Result<T> + Send + 'static,
    {
        let paths = self.paths.clone();
        let cfg = self.cfg.clone();
        let encoder = Arc::clone(&self.encoder);
        let started = std::time::Instant::now();
        let outcome = tokio::task::spawn_blocking(move || {
            let session = Session::read_only(&paths, cfg, encoder).map_err(|e| e.to_string())?;
            f(&session).map_err(|e| e.to_string())
        })
        .await;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        match outcome {
            Ok(Ok(value)) => {
                tracing::debug!(tool, elapsed_ms, "read ok");
                Ok(value)
            }
            Ok(Err(err)) => {
                tracing::warn!(tool, elapsed_ms, error = %err, "read error");
                Err(err)
            }
            Err(join) => {
                let err = format!("memory read task failed: {join}");
                tracing::error!(tool, elapsed_ms, error = %err, "read task failed");
                Err(err)
            }
        }
    }

    /// Count a read (`visits`) without ever waiting for the store.
    ///
    /// `visits` is telemetry, not a fact, and a read must not queue behind a write to bump a counter.
    /// So this takes the writer only if it is free and gives up otherwise: a missed visit count costs
    /// nothing, while a read that waits out a rebuild is the whole bug this split exists to remove.
    /// Returns the new count when it landed, so the caller can report it exactly as before.
    pub(crate) fn bump_visit(&self, key: &str) -> Option<i64> {
        let session = self.session.try_lock().ok()?;
        session.record_visit(key).ok()
    }
}

/// One JSON object as the tool's text result.
pub(crate) fn json_ok(value: serde_json::Value) -> String {
    serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string())
}

/// Log one `memory_search` round trip: the request, every returned hit (key/tags/distance) and
/// the size of the serialized response. Visible with `RUST_LOG=ellmephant=info`; the Docker image
/// sets that by default.
#[allow(clippy::too_many_arguments)] // one structured log line; splitting it would hurt grepping
fn log_search(
    query: Option<&str>,
    expand: u8,
    bundle: &Bundle,
    response_bytes: usize,
    elapsed_ms: u64,
) {
    tracing::info!(
        query = query.unwrap_or("-"),
        expand,
        hits = bundle.hits.len(),
        neighbors = bundle.neighbors.len(),
        response_bytes,
        elapsed_ms,
        "memory_search"
    );
    for hit in &bundle.hits {
        tracing::info!(
            key = %hit.key,
            tags = ?hit.tags,
            distance = ?hit.distance,
            "  hit"
        );
    }
}

/// The store lock, with a poisoned mutex a plain error rather than a panic.
///
/// `with_store` reaches the same lock through `spawn_blocking`; the encode worker takes it directly,
/// because it must hold it for two short pieces with a model call *between* them — which one blocking
/// closure cannot express.
#[cfg(feature = "encoder")]
fn lock(session: &Arc<Mutex<Session>>) -> crate::error::Result<std::sync::MutexGuard<'_, Session>> {
    session
        .lock()
        .map_err(|_| Error::Config("memory store lock poisoned".into()))
}

/// One pass: the head of the queue, embedded, then indexed. Returns whether it did a job.
///
/// The lock is taken twice and held across *neither* half of the model call — that is the entire
/// design; everything else here is bookkeeping around it. It also **says what it did**: a worker that
/// only logs its failures is indistinguishable from one that is not running, which is the difference
/// between a quiet store and a stalled one.
#[cfg(feature = "encoder")]
fn pass(
    session: &Arc<Mutex<Session>>,
    watch: &crate::http::LockWatch,
    encoder: &super::cli::session::Encoder,
    idle: &rusqlite::Connection,
    skipped: &mut std::collections::HashMap<String, std::time::Instant>,
) -> crate::error::Result<bool> {
    // 0. Forget skips that have expired: a document that failed once must be tried again, or a
    //    transient model error would cost it its freshness forever.
    skipped.retain(|_, at| at.elapsed().as_secs() < SKIP_SECS);
    // 1. The head, on a read-only connection: no store lock, so an empty queue costs one indexed
    //    SELECT and contends with no one.
    let skip_now: std::collections::HashSet<String> = skipped.keys().cloned().collect();
    let entry = match crate::store::encode::next(idle, &skip_now)? {
        Some(entry) => entry,
        None => {
            // The refill rule, and the only producer this queue has: nothing is queued, so ask what
            // has no vector and queue it at bulk priority. A rebuild is therefore "delete the
            // vectors" plus this; a crash, a dropped entry and a hand-deleted vector are the same
            // question with the same answer, and the answer is empty when there is nothing to do.
            let skip: std::collections::HashSet<String> = skipped.keys().cloned().collect();
            let queued = {
                let _held = watch.held("memory_encode_refill");
                let session = lock(session)?;
                session.refill(&skip)?
            };
            if queued > 0 {
                tracing::info!(
                    queued,
                    "encode worker: refilled the queue from what has no vector"
                );
            }
            match crate::store::encode::next(idle, &skip_now)? {
                Some(entry) => entry,
                None => return Ok(false),
            }
        }
    };
    let started = std::time::Instant::now();
    // 2. Claim under a short lock — witnessed, so `/health` can say who holds the writer. The TEXT
    //    crosses in the claim itself: the queue carries it, so nothing is re-rendered here.
    let claim = {
        let _held = watch.held("memory_encode");
        let session = lock(session)?;
        session.claim_encode(&entry.keyslug)?
    };
    let Some(claim) = claim else {
        return Ok(false);
    };
    // 3. The model call, with the store lock held NOWHERE. This is where the 1.2 GB cold load lands:
    //    on this worker, not on whoever is waiting to write.
    let captured = claim.doc().to_string();
    let vector = match encoder.with(|e| e.embed_one(&captured)) {
        Ok(vector) => vector,
        Err(err) => {
            // KEEP the entry. It is the record that this document's vector is owed: deleting it
            // would leave a row whose vector describes text that is no longer there, with nothing
            // left to say so — `missing()` could not see it either, because a vector exists. So the
            // cost of a failure is a backlog that does not drain, which is visible; the skip list
            // only stops the retry from becoming a hot loop, and it lives in memory so a restart
            // tries again immediately.
            skipped.insert(entry.keyslug.clone(), std::time::Instant::now());
            tracing::warn!(
                keyslug = %entry.keyslug,
                error = %err,
                skip_secs = SKIP_SECS,
                "encode worker: embed failed — entry dropped, retried after the skip window"
            );
            return Ok(true);
        }
    };
    // 4. Settle under a second short lock, and read the queue's new shape while we are in there — the
    //    log line then costs no second lock, and the number it prints is the one the settle left.
    let counts = {
        let _held = watch.held("memory_encode_write");
        let session = lock(session)?;
        session.settle_encode(&claim, &vector)?;
        session.encode_counts()?
    };
    skipped.remove(&entry.keyslug);
    tracing::info!(
        keyslug = %entry.keyslug,
        elapsed_ms = started.elapsed().as_millis() as u64,
        pending = counts.pending,
        "encode worker: encoded"
    );
    Ok(true)
}

/// How long a document that failed to embed is left out of the refill rule. Long enough that a broken
/// document cannot spin (a pass is ~one embed), short enough that a transient failure recovers.
#[cfg(feature = "encoder")]
const SKIP_SECS: u64 = 600;

/// The encode worker: the queue's one consumer, and the reason a write does not embed.
///
/// A plain OS thread rather than a tokio task, deliberately. A pass is blocking SQLite calls with a
/// model call between them, and the whole point is that the model call holds the store lock nowhere —
/// which a thread says by dropping a guard, where `spawn_blocking`'s `'static` closures would make it
/// awkward to keep the connection across the call.
#[cfg(feature = "encoder")]
pub struct EncodeWorker {
    /// Set to ask the thread to finish its pass and exit.
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(feature = "encoder")]
impl EncodeWorker {
    /// Ask the worker to finish its current pass and exit.
    pub fn stop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(feature = "encoder")]
impl Drop for EncodeWorker {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(feature = "encoder")]
impl MemoryServer {
    /// Spawn the one encode worker for this store.
    ///
    /// **Exactly one per process**: call it from `serve`, never from a clone — `http_router` clones
    /// the server per MCP session, so spawning in the constructor would mean one worker per client,
    /// all fighting for the single writer.
    pub fn spawn_encode_worker(&self, interval: std::time::Duration) -> EncodeWorker {
        use std::sync::atomic::{AtomicBool, Ordering};
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let session = Arc::clone(&self.session);
        let watch = Arc::clone(&self.watch);
        let encoder = Arc::clone(&self.encoder);
        let paths = self.paths.clone();
        let thread = std::thread::Builder::new()
            .name("encode-worker".into())
            .spawn(move || {
                // One read-only connection for the worker's life: the idle check must not take the
                // store lock, or an idle store would contend with its own readers for nothing.
                let idle = match crate::db::open_read_only(&paths) {
                    Ok(conn) => conn,
                    Err(err) => {
                        tracing::error!(error = %err, "encode worker: cannot read the store");
                        return;
                    }
                };
                tracing::info!(
                    interval_ms = interval.as_millis() as u64,
                    "encode worker up"
                );
                let mut skipped: std::collections::HashMap<String, std::time::Instant> =
                    std::collections::HashMap::new();
                while !flag.load(Ordering::SeqCst) {
                    match pass(&session, &watch, &encoder, &idle, &mut skipped) {
                        Ok(_) => {}
                        Err(err) => tracing::warn!(error = %err, "encode worker: pass failed"),
                    }
                    std::thread::sleep(interval);
                }
            })
            .map_err(|err| tracing::error!(error = %err, "encode worker: cannot spawn"))
            .ok();
        EncodeWorker { stop, thread }
    }
}

/// Deliberately **not** gated on the encoder: a store-only build has no worker to leave the work to,
/// so this is the only way it could index anything — and it should fail with the stub embedder's own
/// error, not fail to compile.
impl MemoryServer {
    /// Embed one queued document now, instead of leaving it to the worker: the `?wait=true` door.
    ///
    /// The worker's job, run on the caller's thread and under the caller's lock — which is the whole
    /// difference, and the reason this exists in exactly one place. See `ENCODE-WORKER.md`.
    pub async fn encode_now(&self, key: &str) -> std::result::Result<bool, String> {
        let key = key.to_string();
        self.with_store("memory_encode_now", move |s| s.encode_document(&key))
            .await
    }
}

/// The `memory_upvote` sweep, implemented ONCE for both doors.
///
/// The MCP tool and `POST /memory/{key}/upvote` share this, so they cannot drift in either the error
/// they raise or the JSON they return. That is the whole point of a second door: not a second
/// implementation, a second *way in*.
pub(crate) async fn upvote_sweep(
    server: &MemoryServer,
    keys: Vec<String>,
) -> std::result::Result<String, String> {
    if keys.is_empty() {
        return Err("memory_upvote needs at least one key".to_string());
    }
    let sweep = keys.clone();
    let (upvoted, missing) = server
        .with_store("memory_upvote", move |s| s.record_useful_many(&sweep))
        .await
        .map_err(|e| e.to_string())?;
    // A sweep that hit nothing is a mistake worth surfacing, not a silent zero.
    if upvoted.is_empty() {
        return Err(Error::NotFound(keys.join(", ")).to_string());
    }
    tracing::info!(
        tool = "memory_upvote",
        upvoted = upvoted.len(),
        missing = missing.len(),
        "marked useful"
    );
    let upvoted: Vec<serde_json::Value> = upvoted
        .into_iter()
        .map(|(key, times_useful)| serde_json::json!({ "key": key, "times_useful": times_useful }))
        .collect();
    let mut result = serde_json::json!({ "status": "useful", "upvoted": upvoted });
    if !missing.is_empty() {
        result["missing"] = serde_json::json!(missing);
    }
    Ok(json_ok(result))
}

#[tool_router]
impl MemoryServer {
    /// WHO this connection speaks for — the name a write would be stamped with, and whether it was
    /// PROVEN. A read: it changes nothing, and it never returns the secret.
    #[tool(
        name = "whoami",
        description = "Report the identity this connection writes as: `asserted_by` (the `sender`/author a write is stamped with) and `proven` (true when `X-Stet-From` was matched against `X-Stet-Auth`). `proven: false` with `asserted_by: anonymous` means the connection presents no identity, so every write will be REFUSED (reads need no identity). Use this instead of guessing, and never to test credentials — it does not echo them.",
        annotations(
            title = "Who am I",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn whoami(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> std::result::Result<String, String> {
        let body = match identity_of(&ctx) {
            Ok(Some(name)) => serde_json::json!({ "asserted_by": name, "proven": true }),
            Ok(None) => {
                serde_json::json!({ "asserted_by": crate::identity::ANONYMOUS, "proven": false })
            }
            // A claim that did not verify still deserves an answer: the caller learns the name it
            // sent is not the name it has, which is the whole confusion this tool exists to end.
            Err(forged) => serde_json::json!({
                "asserted_by": crate::identity::ANONYMOUS,
                "proven": false,
                "claimed": forged.0,
                "problem": "the X-Stet-Auth proof is missing or wrong",
            }),
        };
        Ok(json_ok(body))
    }

    /// Store one atomic memory; upserts by key. See the description for the write law.
    #[tool(
        name = "memory_upsert",
        description = UPSERT_DESCRIPTION,
        annotations(
            title = "Upsert memory",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn memory_upsert(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<UpsertParams>,
    ) -> std::result::Result<String, String> {
        let mut patch = p.into_patch();
        // The name travels on the CONNECTION, not the payload: a forged claim refuses the write
        // rather than silently becoming `anonymous`, because a lie with a signature is worse than
        // no signature at all.
        patch.asserted_by = Some(pen_name(&ctx, self.declared.as_deref())?);
        let outcome = self
            .with_store("memory_upsert", move |s| s.memory_merge(&patch))
            .await?;
        tracing::info!(
            tool = "memory_upsert",
            key = %outcome.key,
            created = outcome.created,
            queued = outcome.queued,
            conflict = ?outcome.conflict,
            "memory stored"
        );
        // ★ A CONFLICT IS A RESULT, NOT AN ERROR: the caller asked "write if unchanged", the answer is
        // "it changed", and the current version is what it needs to retry. Reporting it as a tool
        // error would make a retry look like a failure of the store.
        if let Some(current) = outcome.conflict {
            return Ok(json_ok(serde_json::json!({
                "status": "conflict",
                "key": outcome.key,
                "current_version": current,
            })));
        }
        Ok(json_ok(serde_json::json!({
            "status": if outcome.created { "created" } else { "updated" },
            "key": outcome.key,
            "queued": outcome.queued,
        })))
    }

    /// Delete one memory by key.
    #[tool(
        name = "memory_delete",
        description = "Kill one memory by `key` — gone from search and from `memory_get`, no undo. Nothing is LOST: the delete snapshots the state it removes first, so its VERSIONS remain and the NEWEST of them is the content it held (`memory_history KEY`). `memory_history_erase` is the act that truly destroys. Use `memory_merge` when another memory should absorb it: a bare delete leaves every `[[key]]` pointing at it dangling, and `memory_stats` will report the store as damaged. Unsure rather than wrong? Reword it instead.",
        annotations(
            title = "Delete memory",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn memory_delete(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<KeyParams>,
    ) -> std::result::Result<String, String> {
        pen_name(&ctx, self.declared.as_deref())?; // writes are signed; anonymous is read-only
        let key = p.key;
        let rm_key = key.clone();
        // The receipt says what the delete did NOT remove. History outlives the row by design, and a
        // delete that stays silent about that reads as "gone" when it means "not findable as current".
        if let Some(remaining) = self
            .with_store("memory_delete", move |s| {
                if !s.rm(&rm_key)? {
                    return Ok(None);
                }
                Ok(Some(s.version_count(&rm_key)?))
            })
            .await?
        {
            tracing::info!(tool = "memory_delete", key = %key, versions = remaining, "memory removed");
            Ok(json_ok(serde_json::json!({
                "status": "removed",
                "key": key,
                // The newest of these IS the state the memory was deleted in, because the delete
                // snapshots what it removes: the count is a count of recoverable states.
                "versions_remaining": remaining,
            })))
        } else {
            tracing::warn!(tool = "memory_delete", key = %key, "no such memory");
            Err(format!("no such memory: {key}"))
        }
    }

    /// Integrate one memory into another and delete it — the safe way to merge.
    #[tool(
        name = "memory_merge",
        description = MERGE_DESCRIPTION,
        annotations(
            title = "Merge memory",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn memory_merge(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<MergeParams>,
    ) -> std::result::Result<String, String> {
        pen_name(&ctx, self.declared.as_deref())?; // writes are signed; anonymous is read-only
        let from = p.from;
        let into = p.into;
        let log_from = from.clone();
        let log_into = into.clone();
        let out = self
            .with_store("memory_merge", move |s| s.merge_into(&from, &into))
            .await?;
        tracing::info!(
            tool = "memory_merge",
            from = %log_from,
            into = %log_into,
            bodies = out.bodies,
            preloads = out.preloads,
            "memories merged"
        );
        serde_json::to_string(&out).map_err(|e| e.to_string())
    }

    /// Semantic phrase search across every document type, with the graveyard behind a flag.
    #[tool(
        name = "semantic_search",
        description = SEMANTIC_SEARCH_DESCRIPTION,
        annotations(
            title = "Semantic search (all types)",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn semantic_search(
        &self,
        Parameters(p): Parameters<SemanticSearchParams>,
    ) -> std::result::Result<String, String> {
        self.run_semantic(p).await
    }

    /// The `semantic_search` body, shared by the tool and `GET /semantic` so the two doors cannot drift.
    pub(crate) async fn run_semantic(
        &self,
        p: SemanticSearchParams,
    ) -> std::result::Result<String, String> {
        let query = p.query.unwrap_or_default();
        if query.trim().is_empty() {
            return Err("semantic_search needs a `query`".to_string());
        }
        let k = p.k.unwrap_or(5).max(1);
        let log_query = query.clone();
        if p.deleted.unwrap_or(false) {
            let md = p.max_distance;
            let hits = self
                .with_read("semantic_search", move |s| s.search_deleted(&query, k, md))
                .await?;
            let hits: Vec<serde_json::Value> = hits
                .into_iter()
                .map(|t| {
                    serde_json::json!({
                        "type": t.kind,
                        "key": t.key,
                        "distance": t.distance,
                        "deleted_at": t.deleted_at,
                    })
                })
                .collect();
            tracing::info!(
                tool = "semantic_search",
                deleted = true,
                hits = hits.len(),
                "graveyard searched"
            );
            return serde_json::to_string(&serde_json::json!({ "deleted": true, "hits": hits }))
                .map_err(|e| e.to_string());
        }
        // A live search: one type if asked, otherwise the whole store. Today only memories are
        // enqueued, so a request for another type must SAY that rather than return a quiet empty.
        if let Some(kind) = p.doc_type.as_deref() {
            let kind = kind.trim().to_string();
            if kind != "memory" {
                let probe = kind.clone();
                let n = self
                    .with_read("semantic_search", move |s| s.vectors_of(&probe))
                    .await?;
                if n == 0 {
                    return Err(format!(
                        "no `{kind}` documents are indexed yet — this store can only search by \
meaning over the kinds that have vectors (memory does). Ask for one, or omit `type`."
                    ));
                }
            }
        }
        let _ = log_query;
        let params = SearchParams {
            query: Some(query),
            pattern: None,
            mode: None,
            k: Some(k),
            max_distance: p.max_distance,
            expand: None,
            tags: Vec::new(),
            min_chars: None,
            max_chars: None,
            created_after: None,
            created_before: None,
            updated_after: None,
            updated_before: None,
            fields: p.fields,
        };
        self.run_search(params).await
    }

    /// Semantic phrase search: KNN over the document embeddings.
    #[tool(
        name = "memory_search",
        description = SEARCH_DESCRIPTION,
        annotations(
            title = "Search memory",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn memory_search(
        &self,
        Parameters(p): Parameters<SearchParams>,
    ) -> std::result::Result<String, String> {
        self.run_search(p).await
    }

    /// Run a search and render its wire JSON. Shared by the MCP tool and `GET /search`, so the two
    /// doors emit byte-identical bodies (HTTP.md § "the wire grammar").
    pub(crate) async fn run_search(&self, p: SearchParams) -> std::result::Result<String, String> {
        let query = p.query;
        let pattern = p.pattern;
        let mode = p.mode.map(MatchModeParam::to_mode).unwrap_or_default();
        let k = p.k;
        let max_distance = p.max_distance;
        let expand = p.expand.unwrap_or(0);
        let tags = p.tags;
        let min_chars = p.min_chars;
        let max_chars = p.max_chars;
        let created_after = p.created_after;
        let created_before = p.created_before;
        let updated_after = p.updated_after;
        let updated_before = p.updated_before;
        // Default: `key` only (lean). Named fields are added, in canonical order.
        let fields = Fields::selection(p.fields.into_iter().map(FieldParam::to_field));
        // Keep a copy for the log line: the original moves into the store closure.
        let log_query = query.clone();
        let started = std::time::Instant::now();
        let bundle = self
            .with_read("memory_search", move |s| {
                let q = Query {
                    text: query,
                    pattern,
                    mode,
                    k: k.unwrap_or_else(|| s.default_k()),
                    // Fall back to the configured ceiling so a bare semantic search is capped.
                    max_distance: max_distance.or_else(|| s.default_max_distance()),
                    expand,
                    tags,
                    min_chars,
                    max_chars,
                    created_after,
                    created_before,
                    updated_after,
                    updated_before,
                };
                s.search(&q)
            })
            .await?;
        let json = to_wire_json(&bundle, &fields).map_err(|e| e.to_string())?;
        log_search(
            log_query.as_deref(),
            expand,
            &bundle,
            json.len(),
            started.elapsed().as_millis() as u64,
        );
        Ok(json)
    }

    /// Fetch one memory in full by key.
    #[tool(
        name = "memory_get",
        description = "Pull one memory whole by `key` — the only read that returns a body. If it shaped your task, `memory_upvote` it.",
        annotations(
            title = "Get memory",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn memory_get(
        &self,
        Parameters(p): Parameters<GetParams>,
    ) -> std::result::Result<String, String> {
        let key = p.key;
        // A VERSION is reached by address, never resolved through the live row (which need not exist
        // — its history outlives it), and it counts no visit: `visits` measures the memory, not a
        // state the memory held.
        if let Some(n) = p.version {
            let lookup = key.clone();
            let past = self
                .with_read("memory_get", move |s| s.version(&lookup, n))
                .await?
                .ok_or_else(|| format!("no version {n} for {key}"))?;
            let mut v = serde_json::to_value(&past).map_err(|e| e.to_string())?;
            if let Some(obj) = v.as_object_mut() {
                obj.insert("version".to_string(), serde_json::json!(n));
                obj.insert("_nudge".to_string(), serde_json::json!(GET_NUDGE));
            }
            return serde_json::to_string(&v).map_err(|e| e.to_string());
        }
        let lookup = key.clone();
        let mut mem = match self.with_read("memory_get", move |s| s.show(&lookup)).await {
            Ok(m) => m,
            Err(e) => return Err(self.explain_miss(&key, e).await),
        };
        // Count the read best-effort: `visits` is telemetry, so a read must never wait for it. If a
        // write holds the store this visit simply goes uncounted — which is nothing, next to a read
        // that queued behind the entire write.
        if let Some(seen) = self.bump_visit(&key) {
            mem.visits = seen;
        }
        tracing::debug!(tool = "memory_get", key = %key, "memory fetched");
        // Carry the firing cue in the payload, not only the static description: the model reads
        // the returned body here, so this is where the upvote ask must live.
        let mut v = serde_json::to_value(&mem).map_err(|e| e.to_string())?;
        if let Some(obj) = v.as_object_mut() {
            obj.insert("_nudge".to_string(), serde_json::json!(GET_NUDGE));
        }
        serde_json::to_string(&v).map_err(|e| e.to_string())
    }

    /// List a memory's versions, or the index of every key that holds one.
    #[tool(
        name = "memory_history",
        description = HISTORY_DESCRIPTION,
        annotations(
            title = "Memory history",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn memory_history(
        &self,
        Parameters(p): Parameters<HistoryParams>,
    ) -> std::result::Result<String, String> {
        let cap = p.limit.unwrap_or(HISTORY_LIMIT as u32) as usize;
        match p.key {
            Some(key) => {
                let lookup = key.clone();
                let rows = self
                    .with_read("memory_history", move |s| s.versions(&lookup))
                    .await?;
                let total = rows.len();
                tracing::debug!(tool = "memory_history", key = %key, total, "history listed");
                Ok(json_ok(serde_json::json!({
                    "key": key,
                    "count": total,
                    "versions": rows.into_iter().take(cap).collect::<Vec<_>>(),
                })))
            }
            None => {
                let orphans = p.orphans_only;
                let rows = self
                    .with_read("memory_history", move |s| s.version_index(orphans))
                    .await?;
                let total = rows.len();
                Ok(json_ok(serde_json::json!({
                    "count": total,
                    "keys": rows.into_iter().take(cap).collect::<Vec<_>>(),
                })))
            }
        }
    }

    /// Bump the `times_useful` counter on a memory that helped the task.
    #[tool(
        name = "memory_upvote",
        description = UPVOTE_DESCRIPTION,
        annotations(
            title = "Upvote memory",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn memory_upvote(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<KeysParams>,
    ) -> std::result::Result<String, String> {
        pen_name(&ctx, self.declared.as_deref())?; // writes are signed; anonymous is read-only
        upvote_sweep(self, p.keys).await
    }

    /// List the memories whose bodies link to `key`.
    #[tool(
        name = "memory_backlinks",
        description = "List memories whose body point at `key` with `[[…]]`.",
        annotations(
            title = "Backlinks",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn memory_backlinks(
        &self,
        Parameters(p): Parameters<KeyParams>,
    ) -> std::result::Result<String, String> {
        let key = p.key;
        let lookup = key.clone();
        let links = self
            .with_read("memory_backlinks", move |s| s.backlinks(&lookup))
            .await?;
        tracing::debug!(tool = "memory_backlinks", key = %key, count = links.len(), "backlinks");
        backlinks_to_wire_json(&links).map_err(|e| e.to_string())
    }

    /// Read-only digest: how much is stored, how it is tagged and linked, and what earned its keep.
    #[tool(
        name = "memory_stats",
        description = "Read-only digest of the whole store — counts, size, tag/link shape, top tags, usage (`visits`/`times_useful`), top useful, age, and the ENCODE QUEUE (`encode_pending` documents still owing a vector, `encode_failed` those the encoder gave up on, `encode_oldest_s` how long the oldest has waited). A non-empty `encode_pending` is normal — a write schedules its document and returns; a growing `encode_oldest_s` means the worker is not keeping up.",
        annotations(
            title = "Store stats",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn memory_stats(
        &self,
        Parameters(p): Parameters<StatsParams>,
    ) -> std::result::Result<String, String> {
        let with_tags = p.tags.unwrap_or(false);
        let stats = self
            .with_read("memory_stats", move |s| s.stats(with_tags))
            .await?;
        tracing::info!(tool = "memory_stats", memories = stats.memories, "stats");
        serde_json::to_string(&stats).map_err(|e| e.to_string())
    }

    /// Find near-duplicate candidates: memory pairs closer than a tight distance.
    #[tool(
        name = "memory_dupes",
        description = DUPES_DESCRIPTION,
        annotations(
            title = "Find duplicates",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn memory_dupes(
        &self,
        Parameters(p): Parameters<DupesParams>,
    ) -> std::result::Result<String, String> {
        let max_distance = p.max_distance.unwrap_or(store::dupes::DEFAULT_MAX_DISTANCE);
        let limit = p.limit.unwrap_or(DUPES_LIMIT);
        let pairs = self
            .with_read("memory_dupes", move |s| s.dupes(max_distance, limit))
            .await?;
        tracing::info!(
            tool = "memory_dupes",
            pairs = pairs.len(),
            max_distance,
            "dupes"
        );
        store::dupes::to_wire_json(&pairs).map_err(|e| e.to_string())
    }

    /// Recompute every vector and rebuild the derived index — the only path out of a model or
    /// dimension change, which [`crate::index::vec::ensure`] deliberately refuses to absorb.
    #[tool(
        name = "memory_reembed",
        description = REEMBED_DESCRIPTION,
        annotations(
            title = "Re-embed store",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn memory_reembed(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(_): Parameters<ListParams>,
    ) -> std::result::Result<String, String> {
        pen_name(&ctx, self.declared.as_deref())?; // writes are signed; anonymous is read-only
        let report = self
            .with_store("memory_reembed", move |s| s.reembed())
            .await?;
        tracing::info!(
            tool = "memory_reembed",
            queued = report.queued,
            dim = report.dim,
            rebuilt = report.rebuilt,
            model = %report.model,
            "queued a re-embed of the store"
        );
        // `Report` is not Serialize; spell the fields out so the response stays explicit.
        Ok(json_ok(serde_json::json!({
            "status": "queued",
            "queued": report.queued,
            "dim": report.dim,
            "rebuilt": report.rebuilt,
            "model": report.model,
        })))
    }

    /// Create or replace a project — the structures half of the store.
    #[tool(
        name = "project_upsert",
        description = PROJECT_UPSERT_DESCRIPTION,
        annotations(
            title = "Upsert project",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn project_upsert(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<ProjectUpsertParams>,
    ) -> std::result::Result<String, String> {
        pen_name(&ctx, self.declared.as_deref())?; // writes are signed; anonymous is read-only
        let patch = p.into_patch();
        let outcome = self
            .with_store("project_upsert", move |s| s.project_merge(&patch))
            .await?;
        tracing::info!(
            tool = "project_upsert",
            key = %outcome.key,
            created = outcome.created,
            "project stored"
        );
        Ok(json_ok(serde_json::json!({
            "status": if outcome.created { "created" } else { "updated" },
            "key": outcome.key,
        })))
    }

    /// Read one project whole: its fields, its ordered preload, and the memories it owns.
    #[tool(
        name = "project_get",
        description = PROJECT_GET_DESCRIPTION,
        annotations(
            title = "Get project",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn project_get(
        &self,
        Parameters(p): Parameters<ProjectKeyParams>,
    ) -> std::result::Result<String, String> {
        let key = p.key;
        let hydrate = project::Hydrate::parse(p.hydrate.as_deref().unwrap_or("none"))
            .map_err(|e| e.to_string())?;
        let lookup = key.clone();
        let view = self
            .with_read("project_get", move |s| s.project_view(&lookup, hydrate))
            .await?;
        tracing::debug!(
            tool = "project_get",
            key = %key,
            preload = view.preload.len(),
            "project read"
        );
        project::view_to_wire_json(&view).map_err(|e| e.to_string())
    }

    /// List the projects: the cheap index (no `state`), with intersecting filters.
    #[tool(
        name = "project_list",
        description = PROJECT_LIST_DESCRIPTION,
        annotations(
            title = "List projects",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn project_list(
        &self,
        Parameters(p): Parameters<ProjectListParams>,
    ) -> std::result::Result<String, String> {
        let filter = project::Filter {
            status: p.status,
            tags: p.tags,
            owner: p.owner,
            pattern: p.pattern,
            mode: p.mode.map(MatchModeParam::to_mode).unwrap_or_default(),
        };
        let rows = self
            .with_read("project_list", move |s| s.project_list(&filter))
            .await?;
        tracing::info!(tool = "project_list", projects = rows.len(), "projects");
        project::list_to_wire_json(&rows).map_err(|e| e.to_string())
    }

    /// Take or renew an atomic lease over a name — the cross-machine claim.
    #[tool(
        name = "claim_acquire",
        description = "Take (or renew) an atomic LEASE over a `name`. This is coordination between TWO writers to one store — the store versions but does not transact, so a bank `debit` (or any read-modify-write) can be lost when two writers interleave. The store owns this primitive because acquiring a client-side lease is itself a read-modify-write, and clocks cannot order the tie-break (two hands skew hours). ONE statement decides and writes: it succeeds when the name is free or the lease has EXPIRED, and is REFUSED while a DIFFERENT owner holds a live lease. The SAME owner re-acquiring is a renewal, not a conflict — names and owners are case-insensitive (`Hand-A` renews `hand-a`), and a renewal keeps the FIRST `acquired_at`. Returns `won` and the holder NOW — on a loss, `claim` names who holds it and until when. Hold it only around the critical section (claim → act and debit → `claim_release`), and pass a `ttl_seconds` that outlives the work but not the buyer: a crashed holder EXPIRES out of the way on its own. Not security — both writers are the same self; coordination only.",
        annotations(
            title = "Acquire claim",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn claim_acquire(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<ClaimAcquireParams>,
    ) -> std::result::Result<String, String> {
        let ClaimAcquireParams {
            name,
            owner,
            ttl_seconds,
        } = p;
        // ★ WHO MAY LEASE. With `ELLMEPHANT_LEASERS` set, a lease is a privilege: the hands and the
        // terminal, never a browser chat. Without it, this stays open exactly as it was.
        let proven = pen_name(&ctx, self.declared.as_deref())?;
        crate::identity::may_lease(Some(&proven))?;
        // A PROVEN identity IS the owner of its lease, so the audit row cannot be signed with
        // someone else's name; an unproven caller's assertion stands, as before.
        let _asserted_owner = owner; // the pen decides; the argument is history
        let owner = proven;
        let out = self
            .with_store("claim_acquire", move |s| {
                s.claim_acquire(&name, &owner, ttl_seconds)
            })
            .await?;
        tracing::info!(
            tool = "claim_acquire",
            name = %out.claim.name,
            owner = %out.claim.owner,
            won = out.won,
            "claim attempted"
        );
        serde_json::to_string(&out).map_err(|e| e.to_string())
    }

    /// Release a lease — only your own.
    #[tool(
        name = "claim_release",
        description = "Release a lease you hold, so a waiting writer can take it now instead of waiting out the TTL. Only the CURRENT HOLDER may release: a release by anyone else is refused and the reply names the holder, because a lease is not a lock you can steal. A lease that lapses is harmless — this is the tidy path, not a duty. Returns `released` and, when the lease survives, the holder still on it.",
        annotations(
            title = "Release claim",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn claim_release(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<ClaimReleaseParams>,
    ) -> std::result::Result<String, String> {
        pen_name(&ctx, self.declared.as_deref())?; // writes are signed; anonymous is read-only
        let ClaimReleaseParams { name, owner } = p;
        let log_name = name.clone();
        let log_owner = owner.clone();
        let out = self
            .with_store("claim_release", move |s| s.claim_release(&name, &owner))
            .await?;
        tracing::info!(
            tool = "claim_release",
            name = %log_name,
            owner = %log_owner,
            released = out.released,
            "claim released"
        );
        serde_json::to_string(&out).map_err(|e| e.to_string())
    }

    /// List the claims — one by name, or every claim, expired rows included.
    #[tool(
        name = "claim_list",
        description = "Read the claims: one by `name`, or EVERY claim when `name` is omitted. Each row carries its `owner`, when it was `acquired_at`, when it `expires_at`, and a derived `live` flag. EXPIRED rows are kept, not swept — an unexplained stale claim is a bug report, and this view is the only place to read it. Use this before blaming a writer for not proceeding: the answer is usually that someone else holds the lease, and the row says who and until when.",
        annotations(
            title = "List claims",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn claim_list(
        &self,
        Parameters(p): Parameters<ClaimListParams>,
    ) -> std::result::Result<String, String> {
        let name = p.name;
        let lookup = name.clone();
        let rows = self
            .with_read("claim_list", move |s| s.claims(lookup.as_deref()))
            .await?;
        tracing::info!(tool = "claim_list", claims = rows.len(), "claims listed");
        serde_json::to_string(&serde_json::json!({ "claims": rows })).map_err(|e| e.to_string())
    }

    /// Write one letter into a self's box.
    #[tool(
        name = "mail_send",
        description = "Write ONE letter into another self's box. This is a MAILBOX, not a message bus: the row is the whole event — nothing is delivered, no door is rung, and the sender NEVER learns whether it was read. ★ A letter is a CLAIM, NOT AN ORDER: it may be ignored, and there is deliberately no priority field and no verb in the schema — if you want something done, ASK in prose and let the reader decide. Address a SELF (`stet`, `twine`); a finger answers mail, it never receives it. Bodies may be large but stay under 1 MiB — put a real artifact in object storage and send a link plus a checksum. Returns the letter's `seq`, which the reader uses as its cursor.",
        annotations(
            title = "Send mail",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn mail_send(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<MailSendParams>,
    ) -> std::result::Result<String, String> {
        let MailSendParams {
            sender,
            recipient,
            subject,
            body,
            encoding,
        } = p;
        // A PROVEN identity outranks the argument. `sender` is asserted text — the one field a chat
        // could use to speak as someone else — so the header pair, when it verifies, decides it.
        let _asserted_sender = sender; // the argument is history; the pen decides
        let sender = pen_name(&ctx, self.declared.as_deref())?;
        let (log_sender, log_recipient) = (sender.clone(), recipient.clone());
        let out = self
            .with_store("mail_send", move |s| {
                s.mail_send(
                    &sender,
                    &recipient,
                    subject.as_deref().unwrap_or(""),
                    &body,
                    encoding.as_deref().unwrap_or("text"),
                )
            })
            .await?;
        tracing::info!(
            tool = "mail_send",
            sender = %log_sender,
            recipient = %log_recipient,
            seq = out.seq,
            bytes = out.bytes,
            "letter written"
        );
        serde_json::to_string(&out).map_err(|e| e.to_string())
    }

    /// What is in a box after a cursor. Bodies omitted; `wait_seconds` long-polls.
    #[tool(
        name = "mail_inbox",
        description = "What is in `recipient`'s box AFTER `since` — envelopes only (seq, at, sender, subject, encoding, bytes), oldest first, bodies omitted so you can decide whether a body is worth fetching (`mail_read`). ★ `since` is your own cursor: keep it in YOUR store as `mail-last-read` and advance it ONLY after you have dealt with the letters — the advance is the commit, so a processor that dies first leaves them unread and sees them again. `wait_seconds` LONG-POLLS: the call blocks up to 25s and returns the moment a letter lands, which is how a hand waits for mail without polling. No receipt is ever sent to the sender.",
        annotations(
            title = "Read inbox",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn mail_inbox(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<MailInboxParams>,
    ) -> std::result::Result<String, String> {
        let MailInboxParams {
            recipient,
            since,
            wait_seconds,
            limit,
        } = p;
        // ★ THE BOX IS CHECKED BEFORE THE LONG POLL: waiting 25 seconds to be told you may not read
        // it is a worse answer than being told now.
        self.mail_box_for(&ctx, &recipient)?;
        let since = since.unwrap_or(0);
        let limit = limit.unwrap_or(50).clamp(1, 500);
        let wait = wait_seconds
            .unwrap_or(0)
            .min(crate::store::mail::MAX_WAIT_SECONDS);

        // ★ THE LONG POLL IS THE RECIPIENT'S OWN: the reader holds the connection open and nobody
        // pushes into its room. That keeps "a letter is a claim" true at the transport level too.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(wait);
        loop {
            let who = recipient.clone();
            let (rows, newest) = self
                .with_read("mail_inbox", move |s| {
                    // Two facts, and they were being conflated: the top of THIS PAGE and the top of
                    // the BOX. `max_seq` was read as "newest letter that exists" while it meant
                    // "highest seq returned" — which cost two readers an hour of theorising about a
                    // 273-letter discrepancy that never existed (measured 2026-10-21).
                    Ok((s.mail_inbox(&who, since, limit)?, s.mail_head(&who)?))
                })
                .await?;
            if !rows.is_empty() || std::time::Instant::now() >= deadline {
                let max_seq = rows.iter().map(|r| r.seq).max().unwrap_or(since);
                return serde_json::to_string(&serde_json::json!({
                    "letters": rows,
                    "max_seq": max_seq,
                    "newest_available": newest,
                    "waited": wait > 0 && rows.is_empty(),
                }))
                .map_err(|e| e.to_string());
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    }

    /// Turn a bare "no such memory" into a claim with its evidence (soma/ellmephant#57).
    ///
    /// A miss says something about the WHOLE store, and on 2026-10-21 it said the wrong thing for 42
    /// minutes while nobody could test which of four hypotheses was true. Nothing here changes the
    /// answer — a missing row is still an error — but it names the store, its size, its schema and the
    /// key's history, so the next reader can tell an absent key from a wrong file, a stale reader or a
    /// delete. Refusals that are NOT misses pass through untouched.
    async fn explain_miss(&self, key: &str, err: String) -> String {
        if !err.contains("no such memory") {
            return err;
        }
        let lookup = key.to_string();
        let facts = match self
            .with_read("memory_get", move |s| s.miss_facts(&lookup))
            .await
        {
            Ok(f) => f,
            // The diagnostic itself failed — say so rather than replacing one silent answer with
            // another; the original error is still the truth the caller asked for.
            Err(e) => {
                tracing::warn!(tool = "memory_get", key = %key, diagnostic = %e, "memory_get miss, and the diagnostic failed too");
                return err;
            }
        };
        tracing::warn!(
            tool = "memory_get",
            key = %key,
            store = %facts.store,
            memories = facts.memories,
            schema = facts.schema,
            versions = facts.versions,
            newest = ?facts.newest,
            "memory_get miss"
        );
        // ★ THE ONE BIT THAT HALVES THE SEARCH (#57): does the process's own WRITER connection see the
        // row? Reader-blind-only means a WAL/snapshot/path divergence; both blind means the store
        // genuinely does not hold it. Timed, because a miss must not queue behind a long encode.
        let writer_sees = {
            let k = key.to_string();
            match tokio::time::timeout(
                std::time::Duration::from_secs(2),
                self.with_store("memory_get", move |s| s.exists(&k)),
            )
            .await
            {
                Ok(Ok(true)) => "writer-sees-it",
                Ok(Ok(false)) => "writer-agrees-absent",
                Ok(Err(_)) => "writer-check-failed",
                Err(_) => "writer-check-timed-out",
            }
        };
        tracing::warn!(tool = "memory_get", key = %key, writer_sees, "memory_get miss: writer check");
        let divergence = if writer_sees == "writer-sees-it" {
            " ★ THIS PROCESS'S WRITER CONNECTION CAN SEE THIS ROW, so the read is lying about a row the process itself can prove: report the store and this sentence, do not re-write the memory."
        } else {
            ""
        };
        if facts.versions > 0 {
            format!(
                "no such memory: {key} — but {} holds {} memories (schema {}) and {} version(s) for this key, newest {}. So this is a DELETED row with history, or a reader looking at the wrong store; `memory_history` lists the versions and `memory_get` with `version=N` fetches one. Do not treat this as 'never existed'. [{}]{divergence}",
                facts.store,
                facts.memories,
                facts.schema,
                facts.versions,
                facts.newest.as_deref().unwrap_or("unknown"),
                writer_sees
            )
        } else {
            format!(
                "no such memory: {key} — {} holds {} memories (schema {}) and has no history for this key, so it has never existed THERE. If you believe it does, the store that holds it is not this one. [{}]{divergence}",
                facts.store, facts.memories, facts.schema, writer_sees
            )
        }
    }

    /// One letter, body included.
    #[tool(
        name = "mail_read",
        description = "Fetch ONE letter whole, body included, by `seq`. Read the envelope in `mail_inbox` first: `bytes` and `encoding` are there so you can decide whether the body is worth the context, and how to interpret it (`text`, `json`, `gzip+base64`). `none` means that seq is gone — pruned, or never written; letters expire, memories do not.",
        annotations(
            title = "Read letter",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn mail_read(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<MailReadParams>,
    ) -> std::result::Result<String, String> {
        let seq = p.seq;
        let found = self
            .with_read("mail_read", move |s| s.mail_read(seq))
            .await?;
        // ★ A LETTER IS PRIVATE CORRESPONDENCE: the box it is addressed to decides who may read it,
        // and the check happens on the LETTER, not on an argument the caller chose.
        if let Some(letter) = &found {
            self.mail_box_for(&ctx, &letter.recipient)?;
        }
        match found {
            Some(letter) => serde_json::to_string(&letter).map_err(|e| e.to_string()),
            None => Ok(serde_json::json!({ "letter": null, "seq": seq }).to_string()),
        }
    }

    /// Drop letters nobody ever read.
    #[tool(
        name = "mail_prune",
        description = "Delete letters older than `keep_days`, returning how many went. ★ THIS IS WHAT MAKES A MAILBOX NOT A MEMORY STORE: a memory is kept because it is TRUE, a letter is kept because NOBODY HAS READ IT YET — so once nobody plausibly will, it should go. Memories have no such path. Run it from a bell; there is no automatic expiry.",
        annotations(
            title = "Prune mail",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn mail_prune(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<MailPruneParams>,
    ) -> std::result::Result<String, String> {
        pen_name(&ctx, self.declared.as_deref())?; // writes are signed; anonymous is read-only
        let keep_days = p.keep_days;
        let removed = self
            .with_store("mail_prune", move |s| s.mail_prune(keep_days))
            .await?;
        tracing::info!(tool = "mail_prune", keep_days, removed, "letters pruned");
        Ok(serde_json::json!({ "removed": removed, "keep_days": keep_days }).to_string())
    }
}

#[tool_handler]
impl ServerHandler for MemoryServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(SERVER_NAME, crate::VERSION))
            .with_instructions(INSTRUCTIONS)
    }
}

/// How `ellmephant mcp` reaches its client.
pub enum Transport {
    /// Newline-delimited JSON-RPC over stdin/stdout (a client launches the process).
    Stdio,
    /// MCP Streamable HTTP — the default networked transport. The tool surface is mounted at
    /// `/mcp` on `host:port`.
    Http { host: String, port: u16 },
}

/// Serve MCP on the chosen [`Transport`] until it is stopped (the `ellmephant mcp` subcommand).
/// HTTP is the default; stdio stays available for client-launched setups.
pub fn serve(
    paths: Paths,
    cfg: Config,
    spec: model::Spec,
    transport: Transport,
) -> crate::error::Result<()> {
    tracing::info!(
        version = crate::VERSION,
        db = %paths.db().display(),
        model = %cfg.model,
        dim = cfg.dim,
        device = %cfg.device,
        precision = %cfg.precision,
        offline = spec.offline,
        "starting ellmephant mcp"
    );
    let watch = Arc::new(crate::http::LockWatch::new());
    // `Session::open` moves `paths`/`cfg`, so the health state takes its own copies first.
    let state = crate::http::HttpState::new(
        paths.clone(),
        cfg.clone(),
        Arc::clone(&watch),
        std::time::Instant::now(),
    );
    // Before `cfg` moves into the session: the interval the worker polls at.
    let encode_interval = std::time::Duration::from_millis(cfg.encode_interval_ms);
    let server = MemoryServer::with_watch(
        Arc::new(Mutex::new(Session::open(paths, cfg, spec)?)),
        watch,
    );
    // ONE worker, started here because `serve` builds the one server the whole process shares — see
    // `spawn_encode_worker`, which must never be called from a clone. A plain thread, so it does not
    // need the runtime below and a wedged transport cannot take the encoder down with it. Held for the
    // life of this function and stopped on the way out.
    #[cfg(feature = "encoder")]
    let _worker = server.spawn_encode_worker(encode_interval);
    // rmcp is async; the rest of ellmephant is not, so own the runtime here.
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        match transport {
            Transport::Stdio => {
                // `waiting()` keeps the process alive until the client closes the pipe.
                let running = server
                    .serve(rmcp::transport::stdio())
                    .await
                    .map_err(|e| Error::Config(format!("mcp initialize failed: {e}")))?;
                running
                    .waiting()
                    .await
                    .map_err(|e| Error::Config(format!("mcp server task failed: {e}")))?;
                Ok(())
            }
            Transport::Http { host, port } => serve_http(server, &host, port, state).await,
        }
    })
}

/// Mount the tool surface at `/mcp` as an `axum` router. Each MCP session gets its own
/// `MemoryServer` clone, all sharing the one [`Session`] behind the mutex (SPEC §15 #10).
///
/// `bind_host` decides the DNS-rebinding guard: on a loopback bind the built-in allow-list
/// (localhost / 127.0.0.1 / ::1) stands; any other bind (e.g. `0.0.0.0` in Docker) disables
/// it, since remote clients arrive with a Host header we cannot enumerate. There is no auth —
/// bind wide only behind a trusted network.
pub fn http_router(
    server: MemoryServer,
    bind_host: &str,
    state: crate::http::HttpState,
) -> axum::Router {
    let config = if is_loopback(bind_host) {
        StreamableHttpServerConfig::default()
    } else {
        tracing::warn!(
            host = bind_host,
            "mcp http bound off-loopback; host-header checks off"
        );
        StreamableHttpServerConfig::default().disable_allowed_hosts()
    };
    // rmcp reaps an idle session after 5 minutes (`SessionConfig::DEFAULT_KEEP_ALIVE`). A client
    // that pauses longer than that — a slow human, a long local build — then gets
    // "Session not found" on its next call and must re-handshake. Hold sessions for a day instead.
    // (`None` disables reaping outright: never stuck, but a client that silently vanishes keeps
    // its slot until the process restarts.)
    const SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);
    let mut sessions = LocalSessionManager::default();
    sessions.session_config.keep_alive = Some(SESSION_IDLE_TIMEOUT);
    let service_server = server.clone();
    let service: StreamableHttpService<MemoryServer, LocalSessionManager> =
        StreamableHttpService::new(move || Ok(server.clone()), sessions.into(), config);
    // The plain-HTTP reads reuse this very session, so the two doors cannot drift.
    let state = state.with_server(service_server);
    use crate::http_surface as surface;
    axum::Router::new()
        .route("/health", axum::routing::get(crate::http::health))
        .route("/whoami", axum::routing::get(surface::whoami))
        .route("/bootstrap", axum::routing::get(surface::bootstrap))
        .route("/stats", axum::routing::get(surface::stats))
        .route("/history", axum::routing::get(surface::history))
        .route(
            "/memory/{key}",
            axum::routing::get(surface::memory_get).delete(surface::memory_delete),
        )
        .route(
            "/memory/{key}/backlinks",
            axum::routing::get(surface::backlinks),
        )
        .route("/memory/{key}/upvote", axum::routing::post(surface::upvote))
        .route("/memory/{key}/merge", axum::routing::post(surface::merge))
        .route("/memory", axum::routing::post(surface::upsert))
        .route("/search", axum::routing::get(surface::search))
        .route("/dupes", axum::routing::get(surface::dupes))
        .route("/projects", axum::routing::get(surface::projects))
        .route("/project/{key}", axum::routing::get(surface::project_get))
        .route("/project", axum::routing::post(surface::project_upsert))
        .route(
            "/claim",
            axum::routing::get(surface::claims).post(surface::claim_acquire),
        )
        .route(
            "/claim/{key}/release",
            axum::routing::post(surface::claim_release),
        )
        // The mailbox. Same rule as the tools: a letter is a claim, not an order.
        .route(
            "/mail",
            axum::routing::get(surface::mail_inbox).post(surface::mail_send),
        )
        .route("/mail/prune", axum::routing::post(surface::mail_prune))
        .route("/mail/{seq}", axum::routing::get(surface::mail_read))
        .route("/reembed", axum::routing::post(surface::reembed))
        // Our own 404/405 bodies, so a gateway cannot substitute an empty one for the truth.
        .fallback(surface::not_found)
        .method_not_allowed_fallback(surface::method_not_allowed)
        .nest_service("/mcp", service)
        .with_state(state)
}

/// True for `localhost`, `127.0.0.0/8` and `::1`.
fn is_loopback(host: &str) -> bool {
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

/// Bind `host:port`, serve HTTP until Ctrl-C, and drain in-flight requests (SPEC §13).
async fn serve_http(
    server: MemoryServer,
    host: &str,
    port: u16,
    state: crate::http::HttpState,
) -> crate::error::Result<()> {
    spawn_snapshotter(state.paths.clone());
    let listener = tokio::net::TcpListener::bind((host, port)).await?;
    let addr = listener.local_addr()?;
    tracing::info!(%addr, "ellmephant mcp listening on http://{addr}/mcp");
    let app = http_router(server, host, state);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// Write a consistent snapshot on a timer — the QUIESCE that makes the backup trustworthy.
///
/// A known place to pause for a moment is worth more than a clever one: this loop is deliberately
/// dull — open a reader, `VACUUM INTO`, rename, sleep. Its failure is logged and never fatal, because
/// a store that cannot snapshot is still a store that serves.
fn spawn_snapshotter(paths: crate::config::Paths) {
    let Some(every) = crate::store::snapshot::every() else {
        tracing::info!("snapshotter disabled (ELLMEPHANT_SNAPSHOT_HOURS=0)");
        return;
    };
    tokio::spawn(async move {
        loop {
            let root = paths.clone();
            let done =
                tokio::task::spawn_blocking(move || -> crate::error::Result<std::path::PathBuf> {
                    // ★ A READER, not the writing open: a snapshot must never migrate or take the writer
                    // lock. VACUUM INTO only READS the store and writes a new file.
                    let conn = crate::db::open_read_only(&root)?;
                    crate::store::snapshot::write(&conn, root.root())
                })
                .await;
            match done {
                Ok(Ok(path)) => {
                    tracing::info!(path = %path.display(), "consistent snapshot written")
                }
                Ok(Err(err)) => tracing::warn!(error = %err, "snapshot failed"),
                Err(err) => tracing::warn!(error = %err, "snapshot task failed"),
            }
            tokio::time::sleep(every).await;
        }
    });
}

/// Resolves when the process is asked to stop, so `axum` can finish open requests.
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("mcp shutting down");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn router_exposes_every_tool_with_annotations() {
        let tools = MemoryServer::tool_router().list_all();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        for expected in TOOLS {
            assert!(names.contains(expected), "missing {expected:?}: {names:?}");
        }

        let find = |name: &str| {
            tools
                .iter()
                .find(|t| t.name.as_ref() == name)
                .unwrap_or_else(|| panic!("tool {name:?} not found"))
        };
        let ann = |name: &str| find(name).annotations.clone().expect("annotations");

        // Read tools declare themselves read-only.
        for name in [
            "memory_search",
            "memory_get",
            "memory_backlinks",
            "memory_stats",
            "memory_dupes",
        ] {
            assert_eq!(
                ann(name).read_only_hint,
                Some(true),
                "{name} must be read-only"
            );
        }
        // Delete is the destructive one; upsert is an idempotent write.
        assert_eq!(ann("memory_delete").destructive_hint, Some(true));
        assert_eq!(ann("memory_delete").read_only_hint, Some(false));
        // Feedback writes but is neither read-only nor destructive.
        assert_eq!(ann("memory_upvote").read_only_hint, Some(false));
        assert_eq!(ann("memory_upvote").destructive_hint, Some(false));
        assert_eq!(ann("memory_upsert").idempotent_hint, Some(true));
        // Re-embed writes, but only to the derived index -- nothing it touches is source data.
        assert_eq!(ann("memory_reembed").read_only_hint, Some(false));
        assert_eq!(ann("memory_reembed").destructive_hint, Some(false));
        assert_eq!(ann("memory_reembed").idempotent_hint, Some(true));

        // The write *policy* rides on the upsert description (SPEC §9)...
        let upsert = find("memory_upsert")
            .description
            .clone()
            .expect("description");
        assert!(upsert.to_lowercase().contains("one topic"), "{upsert}");
        assert!(upsert.to_lowercase().contains("merge"), "{upsert}");
        // The pre-write split nudge: *evaluate* ("before saving"), then *action* (split, one call each).
        assert!(upsert.to_lowercase().contains("before saving"), "{upsert}");
        assert!(upsert.to_lowercase().contains("split"), "{upsert}");
        // Growth law is *coherence, not size*: deepen a topic, never add a second; no "never grow".
        assert!(
            upsert.to_lowercase().contains("deepen the topic"),
            "{upsert}"
        );
        assert!(!upsert.to_lowercase().contains("never grow"), "{upsert}");
        // The feedback nudge must carry a *trigger*, not just a quality bar.
        let upvote = find("memory_upvote")
            .description
            .clone()
            .expect("description");
        assert!(
            upvote.to_lowercase().contains("shaped this task"),
            "{upvote}"
        );
        assert!(upvote.to_lowercase().contains("visit"), "{upvote}");
        // The old "no flattery" clause was a *brake* that starved the counter; it must stay gone,
        // replaced by a default-on push to vote.
        assert!(
            !upvote.to_lowercase().contains("no flattery"),
            "the withholding brake is back: {upvote}"
        );
        assert!(upvote.to_lowercase().contains("under-voting"), "{upvote}");
        // The sweep takes *many* keys: the schema exposes `keys`, never a single `key`.
        let upvote_props = find("memory_upvote")
            .input_schema
            .get("properties")
            .and_then(|v| v.as_object())
            .cloned()
            .expect("upvote schema has properties");
        assert!(upvote_props.contains_key("keys"), "{upvote_props:?}");
        assert!(!upvote_props.contains_key("key"), "{upvote_props:?}");
        // ...and the firing cue must ride the *consuming* read, not the write tool —
        // the model reads `memory_get`, uses the body, and there learns to upvote.
        let get = find("memory_get").description.clone().expect("description");
        assert!(get.contains("memory_upvote"), "{get}");
        // ...and the cue must also ride `memory_get`'s *result* payload, where the model's
        // attention actually sits after consuming the body (pinned end-to-end in tests/mcp.rs).
        assert!(GET_NUDGE.contains("memory_upvote"), "{GET_NUDGE}");
        // Curation is a cross-tool duty, so it is announced once in the server instructions too.
        assert!(
            INSTRUCTIONS.contains("memory_upvote") && INSTRUCTIONS.contains("Curation"),
            "{INSTRUCTIONS}"
        );
        // ...while field definitions live on the field schemas, not the description.
        let upsert_props = find("memory_upsert")
            .input_schema
            .get("properties")
            .and_then(|v| v.as_object())
            .cloned()
            .expect("upsert schema has properties");
        let source_desc = upsert_props
            .get("source")
            .and_then(|v| v.get("description"))
            .and_then(|v| v.as_str())
            .expect("source description");
        assert!(
            source_desc.to_lowercase().contains("proof"),
            "{source_desc}"
        );
        assert!(
            !upsert.contains("`source` ="),
            "field defs belong on the fields: {upsert}"
        );

        // The `start_date` field description must not promise the OLD behaviour. It has now been
        // wrong twice — once stamping at creation, once claiming it was never stamped — and the
        // description is read as law (SPEC §9), so a stale sentence there is a lie the model acts on.
        // Pin the two halves of the current rule: derive on leaving `todo`, and never clear.
        let start_desc = find("project_upsert")
            .input_schema
            .get("properties")
            .and_then(|v| v.get("start_date"))
            .and_then(|v| v.get("description"))
            .and_then(|v| v.as_str())
            .expect("start_date description")
            .to_lowercase();
        assert!(
            start_desc.contains("todo"),
            "the start_date description must state the derivation: {start_desc}"
        );
        assert!(
            start_desc.contains("never cleared"),
            "the start_date description must say it is never cleared: {start_desc}"
        );
        assert!(
            !start_desc.contains("stamped on first write"),
            "start_date is not stamped at creation time: {start_desc}"
        );

        // Search carries the semantic phrase (`query`), the lexical leg (`pattern` + `mode`) and the
        // structured filters — but no `key` leg (a known key is `memory_get`).
        let props = find("memory_search")
            .input_schema
            .get("properties")
            .and_then(|v| v.as_object())
            .cloned()
            .expect("search schema has properties");
        for gone in ["key", "tag_match", "field", "match"] {
            assert!(
                !props.contains_key(gone),
                "memory_search must not expose {gone}: {props:?}"
            );
        }
        let mut searchable: Vec<&str> = props.keys().map(String::as_str).collect();
        searchable.sort();
        assert_eq!(
            searchable,
            [
                "created_after",
                "created_before",
                "expand",
                "fields",
                "k",
                "max_chars",
                "max_distance",
                "min_chars",
                "mode",
                "pattern",
                "query",
                "tags",
                "updated_after",
                "updated_before"
            ]
        );
        // The `mode` values live in the schema (a `MatchModeParam` enum), never in the description.
        let mode_oneof = find("memory_search")
            .input_schema
            .get("$defs")
            .and_then(|d| d.get("MatchModeParam"))
            .and_then(|f| f.get("oneOf"))
            .and_then(|v| v.as_array())
            .expect("MatchModeParam enum in the schema");
        let mode_vals: Vec<&str> = mode_oneof
            .iter()
            .filter_map(|v| v.get("const"))
            .filter_map(|v| v.as_str())
            .collect();
        for want in ["literal", "regex"] {
            assert!(
                mode_vals.contains(&want),
                "missing mode {want}: {mode_vals:?}"
            );
        }
        // The projection selection rides on `fields` (a hit returns `key` alone by default).
        assert!(props.contains_key("fields"), "search must expose `fields`");
        // The `fields` *values* live in the schema (a `FieldParam` enum), never in the description —
        // that is how the model learns them; the prose stays policy-only.
        let field_oneof = find("memory_search")
            .input_schema
            .get("$defs")
            .and_then(|d| d.get("FieldParam"))
            .and_then(|f| f.get("oneOf"))
            .and_then(|v| v.as_array())
            .expect("FieldParam enum in the schema");
        let field_vals: Vec<&str> = field_oneof
            .iter()
            .filter_map(|v| v.get("const"))
            .filter_map(|v| v.as_str())
            .collect();
        for want in ["key", "title", "tags", "chars", "distance"] {
            assert!(
                field_vals.contains(&want),
                "missing field {want}: {field_vals:?}"
            );
        }

        // The read law rides on the search description, not the server instructions.
        let search = find("memory_search")
            .description
            .clone()
            .expect("description");
        assert!(search.to_lowercase().contains("widen"), "{search}");
        // The filter/projection *definitions* are the schema's job — the desc must not restate them.
        assert!(
            !search.contains("min_chars") && !search.contains("max_chars"),
            "search desc must not restate field defs: {search}"
        );

        // The server instruction carries the identity + the smart-caveman style (defined
        // once); it must not restate the tool law (that rides on the tools).
        let info = MemoryServer::new(memory_session()).get_info();
        let instructions = info.instructions.expect("instructions");
        assert!(
            instructions.to_lowercase().contains("tool descriptions"),
            "{instructions}"
        );
        assert!(
            instructions.to_lowercase().contains("smart-caveman"),
            "{instructions}"
        );
        assert!(
            !instructions.contains("one topic")
                && !instructions.contains("merge")
                && !instructions.contains("widen"),
            "instructions must not repeat tool law: {instructions}"
        );
    }

    /// A throwaway session for inspecting server info (no store work is done).
    fn memory_session() -> Session {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = Paths::from_root(dir.path());
        paths.ensure().expect("create store root");
        let cfg = Config::default();
        let spec = crate::model::Spec::from_config(&cfg, &paths);
        Session::open(paths, cfg, spec).expect("open store")
    }
}
