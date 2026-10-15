//! MCP server (P7) on `rmcp`: Streamable HTTP (default, the networked transport) or stdio.
//!
//! Tools: `memory_upsert`, `memory_delete`, `memory_search`,
//! `memory_get`, `memory_backlinks`, `memory_coherent`, `memory_upvote`, `memory_stats`,
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
    "memory_upsert",
    "memory_delete",
    "memory_merge",
    "memory_search",
    "memory_get",
    "memory_backlinks",
    "memory_coherent",
    "memory_upvote",
    "memory_stats",
    "memory_dupes",
    "memory_reembed",
    "project_upsert",
    "project_get",
    "project_list",
];

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
Recompute every stored vector and rebuild the derived vector index. Needed when the encoder \
changes — a different model, projection or dimension — because the vector table's width is fixed \
when it is created, and the store refuses to reinterpret stored vectors rather than hand back \
wrong neighbours. It also re-parses every body to rebuild the `[[link]]` graph, whose edges are \
computed at write time and so do not follow a parser change on their own. The rows (`memory` — \
keys, tags, titles, bodies; `project` — a project's own \
fields) are the source of truth; everything \
this touches is derived, so nothing written can be lost and re-running it is harmless. Heavy: it \
loads the encoder and re-embeds the whole store in one call, so call it once and wait, rather \
than retrying it.";

const UPSERT_DESCRIPTION: &str = "\
Upsert one memory. WRITE in smart-caveman. One topic per memory — closely clustered facts, one \
per line; never a misc/gotchas bucket. Before saving, check: body hold ≥2 distinct topics? Split — \
one call each, each atom stands alone (≥2-3 facts). Trim junk on edit — deepen the topic, never add \
a second. Merge redundant memories by hand: weaker is folded into stronger and `memory_delete` \
Skip facts cheap to derive, live state, or one-off. PATCHING: every field is optional — omit one and \
the stored value is left alone, so a single field can change without resending the row. Empty is an \
instruction: `source: \"\"` clears it and `tags: []` clears them, but `title`/`body` may never be \
empty. A new key needs both a `title` and a `body`. Rewriting `title` or `body` re-opens the split \
review (`coherent` resets); a tags or source change does not — filing is not meaning.";

/// `memory_upvote` description. The feedback nudge, carried here only.
const UPVOTE_DESCRIPTION: &str = "Upvote the memories that shaped this task — one that changed a \
decision, dodged a bug, or you followed its law. A bare fetch is a visit, not this. Pass every key \
that earned its keep in one call; under-voting starves the store's own rankings.";

/// The firing cue that rides the `memory_get` *result* payload, not just its static description.
/// The model's attention is on the body it just consumed — a trigger fires where attention sits at
/// the moment of use (cf. the telemetry lesson).
const GET_NUDGE: &str = "Shaped your task? memory_upvote this key.";

/// `memory_dupes` description. A finder for the merge rule, not a merger.
const DUPES_DESCRIPTION: &str = "\
Find duplicate *candidates*: memory pairs whose documents sit closer than a tight `max_distance` \
(default 0.05). Nearest first, titles shown. Candidates, not verdicts — `memory_get` both before \
merging. The finder for the merge rule.";

/// Cap on returned dupe pairs for a bare `memory_dupes` call.
const DUPES_LIMIT: usize = 50;

/// `memory_coherent` description. The split-review law rides here, in its owning tool only.
const COHERENT_DESCRIPTION: &str = "\
Record a memory's split-review verdict: `true` = one coherent topic, `false` = a grab-bag worth splitting, omit/`null` = unjudged. Reset to null automatically on the next upsert, so an edit re-opens review. Find the fat tail with `memory_search` (`min_chars`, `coherent`).";

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
Retire one memory into another: repoint every `[[from]]` reference at `into` — bodies, the `[[…]]` \
graph, and any project `preload` naming it — then delete `from`. This is how to merge WITHOUT \
leaving dangling links. The PROSE fold is yours: write the merged facts into `into` first \
(`memory_upsert`, or a patch that keeps the rest), then retire `from` here; this never edits \
`into`'s body. `[[prj:from]]` is not rewritten (that names a project). Both keys must exist — a \
merge into a typo would destroy the source. Reports what moved.";

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
    /// Aspect tags.
    #[serde(default)]
    #[schemars(
        description = "Aspects of the subject, e.g. storage, gotchas. Omit to keep the stored tags; `[]` clears them."
    )]
    pub tags: Option<Vec<String>>,
}

impl UpsertParams {
    /// Convert the request into a partial write — `None` means "leave this field alone".
    fn into_patch(self) -> store::Patch {
        store::Patch {
            key: self.key,
            title: self.title,
            body: self.body,
            source: self.source,
            tags: self.tags,
        }
    }
}

/// Parameters for [`MemoryServer::memory_search`].
#[derive(Debug, Deserialize, schemars::JsonSchema)]
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
    /// Coherence-verdict filter.
    #[serde(default)]
    #[schemars(description = "Split-review verdict: `true`, `false`, `null`, or negate with `!`.")]
    pub coherent: Option<CoherentFilter>,
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
    /// Split-review verdict.
    Coherent,
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
            FieldParam::Coherent => Field::Coherent,
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

/// The `memory_search` coherence filter — a nullable bool with negation. `!X` reads "not X",
/// so the six values are the six non-empty proper subsets of `{true, false, null}`.
#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema)]
pub enum CoherentFilter {
    /// `coherent = true` — one topic.
    #[serde(rename = "true")]
    True,
    /// `coherent = false` — a grab-bag.
    #[serde(rename = "false")]
    False,
    /// `coherent IS NULL` — unjudged.
    #[serde(rename = "null")]
    Null,
    /// Not proven coherent (false or null).
    #[serde(rename = "!true")]
    NotTrue,
    /// Not proven a grab-bag (true or null).
    #[serde(rename = "!false")]
    NotFalse,
    /// Judged at all (true or false).
    #[serde(rename = "!null")]
    NotNull,
}

impl CoherentFilter {
    /// Map the wire enum onto the search-layer predicate.
    fn to_coherent(self) -> crate::search::Coherent {
        use crate::search::Coherent;
        match self {
            CoherentFilter::True => Coherent::True,
            CoherentFilter::False => Coherent::False,
            CoherentFilter::Null => Coherent::Null,
            CoherentFilter::NotTrue => Coherent::NotTrue,
            CoherentFilter::NotFalse => Coherent::NotFalse,
            CoherentFilter::NotNull => Coherent::NotNull,
        }
    }
}

/// A single-key request.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct KeyParams {
    /// The memory's key.
    #[schemars(description = "Memory key. Slug.")]
    pub key: String,
}

/// Parameters for [`MemoryServer::memory_merge`].
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MergeParams {
    /// The key to retire.
    #[schemars(
        description = "The key to retire — its `[[…]]` references are repointed at `into`, then it is deleted."
    )]
    pub from: String,
    /// The key that survives.
    #[schemars(
        description = "The key that survives. Must already exist; its body is NOT edited (write the merged facts into it first)."
    )]
    pub into: String,
}

/// A many-key request (feedback sweep).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct KeysParams {
    /// The memories' keys.
    #[schemars(description = "Memory keys (one or more). Slugs.")]
    pub keys: Vec<String>,
}

/// Parameters for [`MemoryServer::memory_stats`].
#[derive(Debug, Deserialize, schemars::JsonSchema)]
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
pub struct ListParams {}

/// Parameters for [`MemoryServer::memory_dupes`].
#[derive(Debug, Deserialize, schemars::JsonSchema)]
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

/// Parameters for [`MemoryServer::memory_coherent`].
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CoherentParams {
    /// The memory's key.
    #[schemars(description = "Memory key. Slug.")]
    pub key: String,
    /// The verdict.
    #[serde(default)]
    #[schemars(
        description = "true = one topic; false = a grab-bag to split; omit/null = unjudged."
    )]
    pub coherent: Option<bool>,
}

/// Parameters for [`MemoryServer::project_get`].
#[derive(Debug, Deserialize, schemars::JsonSchema)]
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

/// The MCP server. Holds the single store [`Session`] behind a mutex; `#[tool_handler]`
/// reaches the tool table through the generated `Self::tool_router()`, so there is no field.
#[derive(Clone)]
pub struct MemoryServer {
    session: Arc<Mutex<Session>>,
}

impl MemoryServer {
    /// Build a server over an open session — the `ellmephant mcp` path.
    pub fn new(session: Session) -> Self {
        Self::from_shared(Arc::new(Mutex::new(session)))
    }

    /// Build a server over a session already behind a shared lock (tests).
    pub fn from_shared(session: Arc<Mutex<Session>>) -> Self {
        Self { session }
    }

    /// Run `f` against the store on a blocking thread, turning an [`Error`] into a
    /// tool-error string. SQLite and the encoder can hold the store lock for a while, so
    /// this must not occupy a tokio worker.
    ///
    /// `tool` labels the call in logs: a success is a `debug!` with its latency, a store
    /// error a `warn!`, and a panic/join failure an `error!`. That is the one place every
    /// tool failure surfaces, so `docker logs` never loses it to a client-side error string.
    async fn with_store<T, F>(&self, tool: &'static str, f: F) -> std::result::Result<T, String>
    where
        T: Send + 'static,
        F: FnOnce(&mut Session) -> crate::error::Result<T> + Send + 'static,
    {
        let session = Arc::clone(&self.session);
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
}

/// One JSON object as the tool's text result.
fn json_ok(value: serde_json::Value) -> String {
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

#[tool_router]
impl MemoryServer {
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
        Parameters(p): Parameters<UpsertParams>,
    ) -> std::result::Result<String, String> {
        let patch = p.into_patch();
        let outcome = self
            .with_store("memory_upsert", move |s| s.memory_merge(&patch))
            .await?;
        tracing::info!(
            tool = "memory_upsert",
            key = %outcome.key,
            created = outcome.created,
            reembedded = outcome.reembedded,
            coherent_reset = outcome.coherent_reset,
            "memory stored"
        );
        Ok(json_ok(serde_json::json!({
            "status": if outcome.created { "created" } else { "updated" },
            "key": outcome.key,
            "reembedded": outcome.reembedded,
            "coherent_reset": outcome.coherent_reset,
        })))
    }

    /// Delete one memory by key.
    #[tool(
        name = "memory_delete",
        description = "Kill one memory by `key`. No undo. Gone.",
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
        Parameters(p): Parameters<KeyParams>,
    ) -> std::result::Result<String, String> {
        let key = p.key;
        let rm_key = key.clone();
        if self
            .with_store("memory_delete", move |s| s.rm(&rm_key))
            .await?
        {
            tracing::info!(tool = "memory_delete", key = %key, "memory removed");
            Ok(json_ok(
                serde_json::json!({ "status": "removed", "key": key }),
            ))
        } else {
            tracing::warn!(tool = "memory_delete", key = %key, "no such memory");
            Err(format!("no such memory: {key}"))
        }
    }

    /// Retire one memory into another, repointing the graph — the safe way to merge.
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
        Parameters(p): Parameters<MergeParams>,
    ) -> std::result::Result<String, String> {
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
        let query = p.query;
        let pattern = p.pattern;
        let mode = p.mode.map(MatchModeParam::to_mode).unwrap_or_default();
        let k = p.k;
        let max_distance = p.max_distance;
        let expand = p.expand.unwrap_or(0);
        let tags = p.tags;
        let min_chars = p.min_chars;
        let max_chars = p.max_chars;
        let coherent = p.coherent.map(CoherentFilter::to_coherent);
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
            .with_store("memory_search", move |s| {
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
                    coherent,
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
        Parameters(p): Parameters<KeyParams>,
    ) -> std::result::Result<String, String> {
        let key = p.key;
        let lookup = key.clone();
        let mem = self
            .with_store("memory_get", move |s| s.fetch(&lookup))
            .await?;
        tracing::debug!(tool = "memory_get", key = %key, "memory fetched");
        // Carry the firing cue in the payload, not only the static description: the model reads
        // the returned body here, so this is where the upvote ask must live.
        let mut v = serde_json::to_value(&mem).map_err(|e| e.to_string())?;
        if let Some(obj) = v.as_object_mut() {
            obj.insert("_nudge".to_string(), serde_json::json!(GET_NUDGE));
        }
        serde_json::to_string(&v).map_err(|e| e.to_string())
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
        Parameters(p): Parameters<KeysParams>,
    ) -> std::result::Result<String, String> {
        let keys = p.keys;
        if keys.is_empty() {
            return Err("memory_upvote needs at least one key".to_string());
        }
        let sweep = keys.clone();
        let (upvoted, missing) = self
            .with_store("memory_upvote", move |s| s.record_useful_many(&sweep))
            .await?;
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
            .with_store("memory_backlinks", move |s| s.backlinks(&lookup))
            .await?;
        tracing::debug!(tool = "memory_backlinks", key = %key, count = links.len(), "backlinks");
        backlinks_to_wire_json(&links).map_err(|e| e.to_string())
    }

    /// Record a memory's split-review verdict (coherent / grab-bag / unjudged).
    #[tool(
        name = "memory_coherent",
        description = COHERENT_DESCRIPTION,
        annotations(
            title = "Mark coherent",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn memory_coherent(
        &self,
        Parameters(p): Parameters<CoherentParams>,
    ) -> std::result::Result<String, String> {
        let key = p.key;
        let coherent = p.coherent;
        let mark = key.clone();
        self.with_store("memory_coherent", move |s| s.mark_coherent(&mark, coherent))
            .await?;
        tracing::info!(tool = "memory_coherent", key = %key, coherent = ?coherent, "marked");
        Ok(json_ok(serde_json::json!({
            "status": "marked",
            "key": key,
            "coherent": coherent,
        })))
    }

    /// Read-only digest: how much is stored, how it is tagged and linked, and what earned its keep.
    #[tool(
        name = "memory_stats",
        description = "Read-only digest of the whole store — counts, size, tag/link shape, top tags, usage (`visits`/`times_useful`), top useful, age. One keyed JSON object. The lay of the land; glance before a big hunt. Pass `tags: true` to add `all_tags` — the full tag vocabulary.",
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
            .with_store("memory_stats", move |s| s.stats(with_tags))
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
            .with_store("memory_dupes", move |s| s.dupes(max_distance, limit))
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
        Parameters(_): Parameters<ListParams>,
    ) -> std::result::Result<String, String> {
        let report = self
            .with_store("memory_reembed", move |s| s.reembed())
            .await?;
        tracing::info!(
            tool = "memory_reembed",
            rows = report.rows,
            dim = report.dim,
            rebuilt = report.rebuilt,
            model = %report.model,
            "re-embedded the store"
        );
        // `Report` is not Serialize; spell the fields out so the response stays explicit.
        Ok(json_ok(serde_json::json!({
            "status": "re-embedded",
            "rows": report.rows,
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
        Parameters(p): Parameters<ProjectUpsertParams>,
    ) -> std::result::Result<String, String> {
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
            .with_store("project_get", move |s| s.project_view(&lookup, hydrate))
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
            .with_store("project_list", move |s| s.project_list(&filter))
            .await?;
        tracing::info!(tool = "project_list", projects = rows.len(), "projects");
        project::list_to_wire_json(&rows).map_err(|e| e.to_string())
    }
}

#[tool_handler]
impl ServerHandler for MemoryServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("ellmephant", crate::VERSION))
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
    let server = MemoryServer::new(Session::open(paths, cfg, spec)?);
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
            Transport::Http { host, port } => serve_http(server, &host, port).await,
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
pub fn http_router(server: MemoryServer, bind_host: &str) -> axum::Router {
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
    let service: StreamableHttpService<MemoryServer, LocalSessionManager> =
        StreamableHttpService::new(move || Ok(server.clone()), sessions.into(), config);
    axum::Router::new().nest_service("/mcp", service)
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
async fn serve_http(server: MemoryServer, host: &str, port: u16) -> crate::error::Result<()> {
    let listener = tokio::net::TcpListener::bind((host, port)).await?;
    let addr = listener.local_addr()?;
    tracing::info!(%addr, "ellmephant mcp listening on http://{addr}/mcp");
    let app = http_router(server, host);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
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
        // The split-review mark writes, non-destructively.
        assert_eq!(ann("memory_coherent").read_only_hint, Some(false));
        assert_eq!(ann("memory_coherent").destructive_hint, Some(false));
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
                "coherent",
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
        for want in ["key", "title", "tags", "coherent", "chars", "distance"] {
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
