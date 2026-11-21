use super::*;

pub(super) const REEMBED_DESCRIPTION: &str = "\
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

pub(super) const UPSERT_DESCRIPTION: &str = "\
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

pub(super) const UPVOTE_DESCRIPTION: &str = "Upvote the memories that shaped this task — one that changed a \
decision, dodged a bug, or you followed its law. A bare fetch is a visit, not this. Pass every key \
that earned its keep in one call; under-voting starves the store's own rankings.";

pub(crate) const GET_NUDGE: &str = "Shaped your task? memory_upvote this key.";

pub(super) const HISTORY_DESCRIPTION: &str = "\
List a memory's versions: the states its DOCUMENT (`title` + `body`) held before each change, oldest \
first — so `n = 1` is the first state that was replaced, and the state you see in the live row is the \
one that is NOT here. Bodies are not returned: pick an `n` from this list, then read it with \
`memory_get version=N`. Omit `key` for the INDEX of every key holding versions, where `orphan` marks \
keys whose memory is GONE — that history is then the only copy of its content, which is why a delete \
leaves something findable. A version is reached by ADDRESS and is never embedded: `memory_history KEY` \
answers for a deleted key exactly as it does for a live one.";

pub(crate) const HISTORY_LIMIT: usize = 100;

pub(super) const DUPES_DESCRIPTION: &str = "\
Find duplicate *candidates*: memory pairs whose documents sit closer than a tight `max_distance` \
(default 0.05). Nearest first, titles shown. Candidates, not verdicts — `memory_get` both before \
merging. **Every pair carries a `class`, and distance cannot decide it**: `duplicate` (near-identical \
bodies — a keeper/loser plan is safe), `series` (both keys are dated steps of one series: NEVER merge \
— each row is the only record of its day), `artifact` (both bodies are verbatim captures with a \
recoverable source — collapse into ONE row that cites it), `distinct` (close by embedding, different \
by content: report and move on). `plan: true` returns the merge plan instead: keeper/absorbed steps \
for `duplicate` only, and a refusal with its reason for every other pair.";

pub(crate) const DUPES_LIMIT: usize = 50;

pub(super) const SEARCH_DESCRIPTION: &str = "\
Hunt memory by meaning (`query`) or by literal (`pattern`) — semantics reach sense; `pattern` reaches \
a half-remembered string (a key, a filename, an error). `mode` reads `pattern` as `literal` (a \
substring, the default) or `regex`. Both legs and the filters intersect (all must hold); omit all to \
filter the whole store. `created_after`/`created_before` and `updated_after`/`updated_before` are ISO \
windows over the timestamps (`updated_after` is the \"what changed since I last woke\" filter). Hits \
carry `key` and `via` (which leg — `semantic`/`lexical`/`both`/`listing` — found the row) by \
default; `fields` adds more, body never. `memory_get` \
the hit(s) you'll use; a known `key` goes straight there. Nothing fits? Widen — raise `k`, loosen \
`max_distance`.";

pub(super) const MERGE_DESCRIPTION: &str = "\
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

pub(super) const FOLD_DESCRIPTION: &str = "\
Fold one memory's lines into another WITHOUT deleting the source and WITHOUT needing write \
authority on the keeper. This is the cross-pen maintenance verb: `memory_merge` is a PATCH, so it \
needs the survivor's write authority, and two sibling pens can never tidy each other's rows with \
it. A fold can only APPEND — it cannot change the keeper's title, audience, tags, or any line it \
already carries — so it takes only the READ gate on both rows, plus the keeper's lease if it is \
PROTECTED. It records `edited_by` (the pen that folded) and the absorbed key, and it names both in \
its report. It never deletes `from`, never repoints `[[links]]`, and never touches a project \
preload: removing the absorbed row is the act of the pen that answers for it, and this verb \
deliberately cannot do it. It requires a PROVEN pen: an append nobody is named for is \
indistinguishable from a mistake, so an unproven caller is refused rather than recorded with no \
actor. If the keeper already carries every line, it says so and writes nothing \
rather than paraphrasing. Use `memory_merge` when you DO hold the survivor and want the source gone.";

pub(super) const ADOPT_DESCRIPTION: &str = "\
Take responsibility for a memory: `owner` becomes the calling pen. This is NOT authorship — \
`asserted_by` is frozen and unaffected, and `owner` is who is ANSWERABLE, not who typed it. You may \
adopt an OWNERLESS row you can read; a row that already has an owner can be taken only by an \
ancestor of that owner, and only with `force: true`, because a pen's work is not up for grabs by a \
sibling or a stranger who happens to notice it. A row you cannot read cannot be adopted: taking a \
private row by noticing it exists is not a right. A finger adopts as its parent. Recorded in the \
pen ledger (`adopt`, with the previous owner). An ownerless row is a legitimate state — adopting is \
a choice, never a duty. PASS `to` TO GIVE THE ROW TO ANOTHER PEN: a pen may name itself or its own \
subtree, and naming anyone else needs a LIVE BORROW of `estate` (root reach is lent, never assumed). \
Taking an already-owned row needs `force: true`.";

pub(super) const PROJECT_UPSERT_DESCRIPTION: &str = "\
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

pub(super) const PROJECT_GET_DESCRIPTION: &str = "\
Read one project whole, with its `preload` resolved — the ordered context to load (project -> \
memory), titles hydrated. A referenced key whose title is null resolves to nothing: a forward \
reference or rot. The read reports it rather than hiding it. `hydrate` (`none` default, `snippet`, \
`full`) optionally carries each preload memory's BODY, so a project can be a warm start rather than \
a reading list; the bodies ride in a sister `bodies` table, in preload order. Membership is a TAG, \
not a relation: a project's memories are the ones carrying `prj:<key>`, found with `memory_search`'s \
`tags` filter (intersect with a topic tag to narrow to one facet).";

pub(super) const PROJECT_LIST_DESCRIPTION: &str = "\
List projects — the cheap index over them (`key`, `status`, `owner`, `tags`, `title`, `goal`, \
`start_date`, `end_date`; never `state`, which is a `project_get` away). Filters intersect: `status`, `tags` (every listed \
tag must be present) and a literal/regex `pattern` over `key`/`title`/`goal`/`state`. Omit them \
all to list every project.";

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HistorySearchParams {
    #[schemars(
        description = "Words to find in ANY past state of any document — superseded, rewritten or deleted. Exact tokens, not meaning: this is the axis for hashes, keys and negations."
    )]
    pub query: String,
    #[serde(default)]
    #[schemars(description = "Most hits to return (default 10).")]
    pub k: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HistoryEraseParams {
    #[serde(default)]
    #[schemars(
        description = "One key's history to erase. It must have NO live memory row — delete the row first, which snapshots it, then erase the history."
    )]
    pub key: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Erase the history of EVERY key whose memory row is gone — the pile a delete leaves behind. One transaction; reports how many keys and versions went."
    )]
    pub orphans: Option<bool>,
    #[serde(default)]
    #[schemars(
        description = "REQUIRED and must be true. This is the store's only act that destroys text: versions are not backed up anywhere else, and nothing can restore them afterwards."
    )]
    pub confirm: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpsertParams {
    #[serde(default)]
    #[schemars(description = "Stable slug. Reuse patches that memory; a new key is created.")]
    pub key: Option<String>,
    #[serde(default)]
    #[schemars(description = "One sharp line. Omit to keep the stored one; never empty.")]
    pub title: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Facts, one per line. Link memory with `[[other-key]]`. Omit to keep the stored body; never empty."
    )]
    pub body: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Proof stone: command/path/repo that derive this. Never live state. Omit to keep it; an empty string clears it."
    )]
    pub source: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Where this came from, when it is NOT you: a URL, a file, a command, `my inference`. Recorded BESIDE the store-stamped `asserted_by` (who spoke), so relaying a fact never makes you its source. Omit when you are the origin."
    )]
    pub origin: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Aspects of the subject, e.g. storage, gotchas. Omit to keep the stored tags; `[]` clears them."
    )]
    pub tags: Option<Vec<String>>,
    #[serde(default, rename = "as")]
    #[schemars(
        description = "Credit this write to one of your OWN DESCENDANTS — a finger doing the work — instead of yourself. The store checks that it is below you AND that you hold a LIVE lend for it (`pen_lend`): parentage alone is not enough, because a lend expires and can be revoked, and an inheritance cannot. `asserted_by` names the descendant; `owner` stays with the pen that answers for it (a finger's work is its parent's). Omit to write as yourself."
    )]
    pub as_pen: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Make this row PRIVATE to the writing pen (`true`), or public (`false`). Omit to keep the stored audience. Private is the READ tier: the row is invisible to every other pen — not as a hit, a title, a count or a backlink — and readable by you alone, plus the owner's recorded forensic path. The audience is derived from the PROVEN identity, so this flag cannot name someone else. Unencrypted: the file, the snapshot and the off-cluster copy hold it in plaintext, so it protects against another agent using the tools and against nothing else."
    )]
    pub private: Option<bool>,
    #[serde(default)]
    #[schemars(
        description = "An explicit AUDIENCE: `public`, or up to 8 comma-separated addresses — `pen:<name>` (exactly that pen) or `subtree:<name>` (that pen and everything below it). This NAMES others, so it is a SHARE and it is validated against the registry; `private: true` is the derived form that can only name the writer. Examples: `pen:james,pen:cline` = me and Cline and nobody else; `subtree:james` = James and his whole branch. Note that a pen above you does NOT see a `pen:` row by ancestry — ask for the ancestor explicitly if you want it. Omit to keep the stored audience."
    )]
    pub audience: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Why THIS change — recorded beside the version this write creates, so a later reader (`memory_history`) learns the intent instead of reconstructing it. Only a change to `title`/`body` creates a version; a note alone does not."
    )]
    pub version_note: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Bypass the run-shape gate. `memory_upsert` REFUSES a body that looks like a play-by-play (three timestamps, or one plus two run markers) and names `record append` on the brain instead, because a memory is a claim and an occurrence belongs on the shelf. Pass `force: true` when you read that refusal and still mean it."
    )]
    pub force: Option<bool>,
    #[serde(default)]
    #[schemars(
        description = "Write only if the memory is STILL at this `version` (the field `memory_get` returns). A mismatch writes nothing and returns `status: conflict` with the current version — the safe way to read-modify-write (a bank balance, a counter). `0` means 'this key does not exist yet'. Omit for last-write-wins."
    )]
    pub expected_version: Option<i64>,
}

impl UpsertParams {
    pub(super) fn into_patch(self) -> store::Patch {
        store::Patch {
            acted_by: None,
            key: self.key,
            title: self.title,
            body: self.body,
            source: self.source,
            asserted_by: None,
            origin: self.origin,
            tags: self.tags,
            private: self.private,
            audience: self.audience,
            version_note: self.version_note,
            expected_version: self.expected_version,
            force: self.force.unwrap_or(false),
        }
    }
}

pub(super) const SEMANTIC_SEARCH_DESCRIPTION: &str = "\
Search the whole store by meaning: every document type that has vectors, one `type` if you name it, \
or the GRAVEYARD of deleted documents when `deleted` is true. Live and deleted results are never \
merged — a tombstone hit gives `type`/`key`/`distance`/`deleted_at` and no body, because a deleted \
document is a name to look up rather than text to read (`memory_history KEY` holds what it said). \
`memory_search` remains the memory-only alias.";

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SemanticSearchParams {
    #[serde(default)]
    #[schemars(description = "Phrase to hunt with, matched against each document's embedding.")]
    pub query: Option<String>,
    #[serde(rename = "type", alias = "kind", default)]
    #[schemars(
        description = "Restrict to one document type (`memory`, `project`). Absent = every type that has vectors."
    )]
    pub doc_type: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Search the GRAVEYARD: documents that were deleted. Never merged with live hits — it is a different index. Returns keys and `deleted_at`, never bodies."
    )]
    pub deleted: Option<bool>,
    #[serde(default)]
    #[schemars(description = "Most hits to bring back (default = configured k).")]
    pub k: Option<usize>,
    #[serde(default)]
    #[schemars(description = "Cosine-distance ceiling (0-2). Smaller = closer.")]
    pub max_distance: Option<f32>,
    #[serde(default)]
    #[schemars(
        description = "Extra hit fields for live hits (tombstones always return `type`, `key`, `distance`, `deleted_at`)."
    )]
    pub fields: Vec<FieldParam>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchParams {
    #[serde(default)]
    #[schemars(description = "Phrase to hunt with. Matched against embedded document.")]
    pub query: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Literal/regex pattern over `key`/`title`/`body` — for a half-remembered literal (a key, a filename, an error string). Intersects with `query` and the filters. Blank = no lexical leg."
    )]
    pub pattern: Option<String>,
    #[serde(default)]
    #[schemars(description = "How `pattern` is read: `literal` (substring, default) or `regex`.")]
    pub mode: Option<MatchModeParam>,
    #[serde(default)]
    #[schemars(description = "Most hits bring back (default = configured k).")]
    pub k: Option<usize>,
    #[serde(default)]
    #[schemars(
        description = "Cosine-distance ceiling (0-2). Smaller = closer. Applies to the SEMANTIC leg only — if it cuts everything the list is filled from the word index and `search.semantic_candidates` reads 0."
    )]
    pub max_distance: Option<f32>,
    #[serde(default)]
    #[schemars(
        description = "Keep only rows the semantic leg ranked. An empty list then means NO instead of being refilled from the word index. Still subject to `max_distance`, so at the default ceiling it asks 'did my phrase reach anything under the ceiling', not 'anything real' — pass a wide `max_distance` (e.g. 1.2) when testing existence."
    )]
    pub semantic_only: Option<bool>,
    #[serde(default)]
    #[schemars(description = "Link-neighbour hops drag in (0 = off).")]
    pub expand: Option<u8>,
    #[serde(default)]
    #[schemars(description = "Tag filter; a memory must carry EVERY listed tag.")]
    pub tags: Vec<String>,
    #[serde(default)]
    #[schemars(description = "Minimum embedded-document size, in chars.")]
    pub min_chars: Option<usize>,
    #[serde(default)]
    #[schemars(description = "Maximum embedded-document size, in chars.")]
    pub max_chars: Option<usize>,
    #[serde(default)]
    #[schemars(
        description = "Only memories created at or after this ISO timestamp (`YYYY-MM-DD` or `YYYY-MM-DD HH:MM:SS`). Compared lexicographically, so a date bounds the whole day."
    )]
    pub created_after: Option<String>,
    #[serde(default)]
    #[schemars(description = "Only memories created at or before this ISO timestamp.")]
    pub created_before: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Only memories last written at or after this ISO timestamp — the \"what changed since I last woke\" filter."
    )]
    pub updated_after: Option<String>,
    #[serde(default)]
    #[schemars(description = "Only memories last written at or before this ISO timestamp.")]
    pub updated_before: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "EXACT address: return only the row whose key is this. Never a body scan — use `pattern` for \"this word appears somewhere\"."
    )]
    pub key: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Read `key` as a PREFIX (`alerts-2026-11-`). Explicit on purpose: a prefix that silently widened an exact request is the failure this predicate removes."
    )]
    pub key_prefix: Option<bool>,
    #[serde(default)]
    #[schemars(description = "Case-insensitive substring, scoped to the TITLE field only.")]
    pub title: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "PROJECTS ONLY (`active`, `paused`, …). A memory search REFUSES it by name rather than ignoring it — a dropped filter is a widened search."
    )]
    pub status: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Extra hit fields to return, alongside the always-present `key` and `via`. Omit for the lean default (`key` + `via`); `via` may be named but is always returned. E.g. `[\"title\",\"tags\",\"distance\"]`."
    )]
    pub fields: Vec<FieldParam>,
}

#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(
    description = "A hit field a caller can request on `memory_search` (the `fields` selection). `key` is always\nreturned, so naming it here is a harmless no-op."
)]
pub enum FieldParam {
    #[schemars(description = "Slug.")]
    Key,
    #[schemars(description = "Which leg(s) found the row — always returned; naming it is a no-op.")]
    Via,
    #[schemars(description = "Salient line.")]
    Title,
    #[schemars(description = "Attached tags.")]
    Tags,
    #[schemars(description = "Embedded-document length (chars).")]
    Chars,
    #[schemars(description = "Cosine distance (semantic ranking only).")]
    Distance,
    #[schemars(
        description = "BM25 for the lexical leg (lower is better, like distance — a different unit)."
    )]
    Score,
}

impl FieldParam {
    pub(super) fn to_field(self) -> Field {
        match self {
            FieldParam::Key => Field::Key,
            FieldParam::Via => Field::Via,
            FieldParam::Title => Field::Title,
            FieldParam::Tags => Field::Tags,
            FieldParam::Chars => Field::Chars,
            FieldParam::Distance => Field::Distance,
            FieldParam::Score => Field::Score,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(description = "How a `pattern` is read on `memory_search` (the lexical leg).")]
pub enum MatchModeParam {
    #[schemars(
        description = "A plain substring, case-insensitive; regex metacharacters are literal text."
    )]
    Literal,
    #[schemars(
        description = "A regex, case-insensitive by default (opt out with a leading `(?-i)`)."
    )]
    Regex,
}

impl MatchModeParam {
    pub(super) fn to_mode(self) -> MatchMode {
        match self {
            MatchModeParam::Literal => MatchMode::Literal,
            MatchModeParam::Regex => MatchMode::Regex,
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KeyParams {
    #[schemars(description = "Memory key. Slug.")]
    pub key: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UnownedParams {
    #[schemars(description = "Maximum rows to list, newest first. Default 50.")]
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WhoisParams {
    #[schemars(description = "Pen name. Slug. `whoami` reports your own.")]
    pub pen: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MintParams {
    #[schemars(
        description = "Name of the pen to create. A name is SPENT once minted: a retired pen keeps its name, so a minting parent must generate a unique one (a hash suffix is the obvious way: `finger-9f3a1c`)."
    )]
    pub name: String,
    #[schemars(
        description = "`finger` for a mechanism a hand uses for one job, `self` for something that stands as itself. Never inferred from the name: a slash proves nothing."
    )]
    pub kind: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RotateParams {
    #[schemars(
        description = "The pen whose secret to replace. Omitted = YOURS. A pen may rotate itself or a DESCENDANT, never a peer and never an ancestor."
    )]
    pub pen: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LendParams {
    #[schemars(
        description = "The DESCENDANT to lend to — a finger you minted. It must be below you."
    )]
    pub pen: String,
    #[schemars(
        description = "How long the lend lasts, in seconds. It expires on its own; `sudo_release` ends it sooner. Maximum 86400."
    )]
    pub secs: i64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SudoParams {
    #[schemars(
        description = "The pen whose authority you want — usually `estate`, which has no identity of its own and can only ever be used this way."
    )]
    pub powerful: String,
    #[schemars(
        description = "How long the session lasts. It cannot outlive the policy that permits it, expires by itself, and `sudo_release` ends it sooner. No cache: revocation takes effect on the next call."
    )]
    pub secs: i64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SudoPassParams {
    #[schemars(
        description = "The authority being passed — the `powerful` pen of the session you hold."
    )]
    pub powerful: String,
    #[schemars(
        description = "A DESCENDANT of yours. A session may be passed down, never sideways to a peer and never back up."
    )]
    pub pen: String,
    #[schemars(
        description = "How long the receiver holds it. It cannot outlive the session it came from; asking for more is refused, not clamped."
    )]
    pub secs: i64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SudoSessionsParams {
    #[schemars(
        description = "Whose live sessions to list. Omit to see ALL pens — the question an operator asks before an audit; name a pen to see just that one. A session is live until it expires, and `expires_at` is the store's own clock."
    )]
    #[serde(default)]
    pub pen: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SudoReleaseParams {
    #[schemars(
        description = "The `powerful` pen of the session to end. Its whole pass-down chain dies with it."
    )]
    pub powerful: String,
    #[schemars(
        description = "Whose session to end: omit for your own, or name a DESCENDANT (a finger you lent to). A pen that lent authority must be able to stop what it lent, or a runaway finger is stoppable only by the finger."
    )]
    #[serde(default)]
    pub pen: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetParams {
    #[schemars(description = "Memory key. Slug.")]
    pub key: String,
    #[schemars(
        description = "Read version `n` instead of the live row — the state that change replaced. `memory_history KEY` lists them. Counts no `visits`: a past state is not the memory."
    )]
    #[serde(default)]
    pub version: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HistoryParams {
    #[schemars(
        description = "Memory key. Slug. OMIT for the index of every key holding versions — which is how a key whose memory is GONE is found at all (`orphan`)."
    )]
    #[serde(default)]
    pub key: Option<String>,
    #[schemars(
        description = "Index mode only (`key` omitted): return just the keys whose memory is GONE — history that is now the only copy of its content."
    )]
    #[serde(default)]
    pub orphans_only: bool,
    #[schemars(description = "Most rows to return. The reply states the true total regardless.")]
    #[serde(default)]
    pub limit: Option<u32>,
}

pub(super) const APPEND_DESCRIPTION: &str = "\
Continue a subject with ONE line — the cheap verb, and the one to reach for mid-work. No key read, \
no title, no tags, no body resent: one call appends the line to the existing memory and returns the \
version it superseded. It NEVER creates (a missing key is refused and names `memory_upsert`, because \
a subject that does not exist yet needs a title) and it never changes the title, audience, tags or \
an existing line. A line the body already carries is a no-op, so a retry is safe. Use this instead \
of writing a second memory about the same thing: a subject split across rows is a trail nobody can \
read, and 24 rows for one review is what that looks like.";

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AppendParams {
    #[schemars(
        description = "The memory to continue. It must exist: an append is a continuation, not a create."
    )]
    pub key: String,
    #[schemars(
        description = "ONE line, the fact to add. No newlines — one call per line. A line the body already carries is a no-op, so retrying is safe."
    )]
    pub line: String,
    #[serde(default)]
    #[schemars(
        description = "Write even if the RESULT would look like a run record. The gate inspects the body after the append, not the line, because three terse appends of an `HH:MMZ ...` line build exactly the body `memory_upsert` refuses. Deliberate override, same as upsert's."
    )]
    pub force: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MergeParams {
    #[schemars(
        description = "The key to delete — the weaker memory. Its lines are integrated into `into`, its `[[…]]` references are repointed at `into`, then it is deleted."
    )]
    pub from: String,
    #[schemars(
        description = "The key that survives — the stronger memory. You are judging which is which by naming it here. Any line of `from` this does not already carry is appended to it verbatim, so its information is in it when the call returns."
    )]
    pub into: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FoldParams {
    #[schemars(
        description = "The key whose lines are appended to `into`. It is NOT deleted, keeps its owner, and can only be removed by the pen that answers for it."
    )]
    pub from: String,
    #[schemars(
        description = "The key that gains the lines. Its title, audience and tags are untouched; only its body grows, by the lines of `from` it does not already carry."
    )]
    pub into: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AdoptParams {
    #[schemars(description = "Memory key. Slug. You must be able to read the row.")]
    pub key: String,
    #[schemars(
        description = "Set true only when the row ALREADY has an owner and you are an ancestor of it. An ownerless row never needs this."
    )]
    pub force: Option<bool>,
    #[schemars(
        description = "Optional owner: give the row to this pen instead of taking it yourself. A pen may name itself or its own subtree; naming anyone else needs a live borrow of `estate`. Omit to adopt as yourself."
    )]
    pub to: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KeysParams {
    #[schemars(
        description = "Memory keys: ONE key as a string, or several as an array. Both are accepted, because a single upvote is the common case and making the caller wrap it in a one-element array is ceremony."
    )]
    pub keys: OneOrMany,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
#[schemars(
    description = "One key or many, from either door.\n\n★ A ONE-ELEMENT ARRAY IS NOT A BATCH. Most upvotes are for a single memory, and a client that has\none key should not have to remember a shape. `#[serde(untagged)]` keeps both spellings valid and\nthe store still receives a list, so exactly one code path runs."
)]
pub enum OneOrMany {
    #[schemars(description = "A single key.")]
    One(String),
    #[schemars(description = "Several keys.")]
    Many(Vec<String>),
}

impl OneOrMany {
    pub fn into_vec(self) -> Vec<String> {
        let raw = match self {
            OneOrMany::One(k) => vec![k],
            OneOrMany::Many(keys) => keys,
        };
        raw.into_iter()
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty())
            .collect()
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StatsParams {
    #[serde(default)]
    #[schemars(
        description = "Also return `all_tags` — the FULL tag vocabulary with counts (count-desc, then name). Off by default so the digest stays lean; on, it is how you see the whole tag set (and its sprawl) instead of inventing a synonym."
    )]
    pub tags: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListParams {}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigGetParams {
    #[schemars(description = "The config item's name, e.g. `recess_daily_time`. Case is folded.")]
    pub key: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigSetParams {
    #[schemars(
        description = "The config item's name. It must ALREADY exist: `config_set` cannot \
                             invent an item, because a value with no written ACL is whoever writes \
                             it first."
    )]
    pub key: String,
    #[schemars(
        description = "The new value, as text. Non-empty; the consumer parses its own units \
                             (`2h`, `true`, a name)."
    )]
    pub value: String,
    #[serde(default)]
    #[schemars(
        description = "Optional: why this value changed. Recorded on the row beside the \
                             previous `source`; omitted keeps the existing reason."
    )]
    pub source: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigDefineParams {
    #[schemars(description = "The config item's name, created if absent.")]
    pub key: String,
    #[schemars(description = "The value to store, as text. Non-empty.")]
    pub value: String,
    #[schemars(
        description = "Who may WRITE the value: an audience — `pen:<name>`, `subtree:<name>`, or a \
                       comma-separated list, up to 8. NOT `public`: a value anyone can write is not \
                       configuration. You may only name addresses you can reach (yourself, a pen \
                       below you, or anything under the root)."
    )]
    pub set_audience: String,
    #[schemars(
        description = "Who may READ the item: `public` or the same `pen:` / `subtree:` grammar. You \
                       may only name addresses you can reach."
    )]
    pub read_audience: String,
    #[schemars(
        description = "REQUIRED: why this item exists / where the value came from. Configuration is \
                       the trust root, and the reason belongs on the row."
    )]
    pub source: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigListParams {
    #[serde(default)]
    #[schemars(
        description = "Optional prefix to filter names by, e.g. `recess` or `recess_`. Omit for every \
                       item you may read."
    )]
    pub prefix: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClaimAcquireParams {
    #[schemars(
        description = "The name to lease — free text, a handle two writers share (a `hands-bank` key, a handoff topic). Trimmed; must not be empty."
    )]
    pub name: String,
    #[schemars(
        description = "Who is taking it — your own identifier. The SAME owner re-acquiring RENEWS rather than conflicts; a DIFFERENT owner is refused while the lease is live."
    )]
    pub owner: String,
    #[serde(default)]
    #[schemars(
        description = "Lease length in seconds (default 900, max 86400). The lease EXPIRES on its own, so a crashed holder frees the name without help."
    )]
    pub ttl_seconds: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClaimReleaseParams {
    #[schemars(description = "The name the lease is over.")]
    pub name: String,
    #[schemars(
        description = "Advisory. Authority is the PROVEN pen, compared with the row's owner: the holder may always release, and an ANCESTOR of the holder may release with a `reason`. The value here is history and cannot grant reach you do not have."
    )]
    pub owner: Option<String>,
    #[schemars(
        description = "Why an ancestor is ending a descendant's lease. Required when the caller is not the holder; a quiet release is indistinguishable from a leak."
    )]
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClaimListParams {
    #[serde(default)]
    #[schemars(
        description = "Optional: one name to inspect. Omit for every claim, EXPIRED rows included — the audit view, so an unexplained stale claim is visible rather than hidden."
    )]
    pub name: Option<String>,
    #[schemars(
        description = "Also return `revocations`: every recorded release by an ANCESTOR of the holder, newest first, with the reason it gave. A holder's own release writes nothing — this is the log of acts on other pens' leases."
    )]
    pub include_revocations: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MailSendParams {
    #[schemars(
        description = "Who is writing: `stet` for the self, or `stet/finger-<id>` for one of its fingers. ASSERTED by the caller, not authenticated by the store."
    )]
    pub sender: String,
    #[schemars(
        description = "The SELF the letter is addressed to (`stet`, `twine`). Never a finger: a finger is how a self ANSWERS mail, not who receives it."
    )]
    pub recipient: String,
    #[serde(default)]
    #[schemars(description = "One line, so an inbox can be scanned without fetching bodies.")]
    pub subject: Option<String>,
    #[schemars(
        description = "The letter. Free text — no verbs, no priority, no task fields. If you want something done, ASK, and let the reader decide."
    )]
    pub body: String,
    #[serde(default)]
    #[schemars(
        description = "How the body is encoded: `text` (default), `json`, `gzip+base64`. Declared so the reader does not spend a finger decoding gibberish."
    )]
    pub encoding: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MailGrantParams {
    #[schemars(
        description = "Whose name is lent. The caller's OWN name: a pen grants itself, never another pen."
    )]
    pub grantor: String,
    #[schemars(description = "The pen that may send as `grantor`.")]
    pub surrogate: String,
    #[serde(default)]
    #[schemars(description = "Comma list of `send`, `read`, `ack`. Default `send`.")]
    pub scopes: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Optional ISO datetime after which the grant lapses. Omit for standing until revoked."
    )]
    pub expires_at: Option<String>,
    #[serde(default)]
    #[schemars(description = "One line, so a later reader knows why the name was lent.")]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MailRevokeParams {
    #[schemars(description = "Whose name was lent.")]
    pub grantor: String,
    #[schemars(description = "Whose grant is being taken back.")]
    pub surrogate: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MailSurrogatesParams {
    #[serde(default)]
    #[schemars(description = "The pen whose grants to list. Omit for your own.")]
    pub surrogate: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MailInboxParams {
    #[schemars(description = "Whose box: the self whose mail this is (`stet`, `twine`).")]
    pub recipient: String,
    #[serde(default)]
    #[schemars(
        description = "The highest `seq` you have already dealt with, if you want to override the STORED cursor for this call. Omit it and the store uses YOUR cursor (one row per proven pen and box, `mail_cursor`) — which is what you should normally do: there is nothing to remember and nothing to pass."
    )]
    pub since: Option<i64>,
    #[serde(default)]
    #[schemars(
        description = "Block up to this many seconds (max 25) and return the moment a letter lands. This is how a hand waits for mail without polling; omit it to check and return immediately."
    )]
    pub wait_seconds: Option<u64>,
    #[serde(default)]
    #[schemars(description = "Most envelopes to return (default 50, max 500).")]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MailReadParams {
    #[schemars(description = "The `seq` from `mail_inbox`.")]
    pub seq: i64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MailAckParams {
    #[schemars(
        description = "The box whose cursor you are advancing — your own mail, or a box a declared surrogate hands you."
    )]
    pub recipient: String,
    #[schemars(
        description = "The highest `seq` you have DEALT WITH (usually the last letter in the page you just read). It only moves forward."
    )]
    pub seq: i64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MailPruneParams {
    #[schemars(
        description = "Delete letters older than this many days. ★ Letters expire because they are kept for a reader, not for the truth; memories have no such path."
    )]
    pub keep_days: i64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DupesParams {
    #[serde(default)]
    #[schemars(description = "Cosine-distance ceiling; tight by default. Smaller = stricter.")]
    pub max_distance: Option<f32>,
    #[serde(default)]
    #[schemars(description = "Most pairs to return, nearest first.")]
    pub limit: Option<usize>,
    #[serde(default)]
    #[schemars(
        description = "Emit the MERGE PLAN instead of the pair list: one keeper/absorbed step per `duplicate` pair, with the reason the keeper is the keeper, and every other pair REFUSED with its class and reason. A `series` pair is refused here by construction — a dated sequence is not redundancy, and the refusal is what stops a hand-run merge being ordered against one."
    )]
    pub plan: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectKeyParams {
    #[schemars(description = "Project key — a plain slug (`prj:`/`prj-` accepted and stripped).")]
    pub key: String,
    #[serde(default)]
    #[schemars(
        description = "How much of each `preload` memory's BODY to carry: `none` (default — keys and titles only, a reading list), `snippet` (first ~280 chars), `full` (whole body). Order is preserved; a key that resolves to nothing is shown with a null title, not dropped."
    )]
    pub hydrate: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectUpsertParams {
    #[serde(default)]
    #[schemars(description = "Stable slug — a plain name; reuse replaces that project.")]
    pub key: Option<String>,
    #[serde(default)]
    #[schemars(description = "One sharp line. Omit to keep the stored one; never empty.")]
    pub title: Option<String>,
    #[serde(default)]
    #[schemars(description = "What this is for. One line.")]
    pub goal: Option<String>,
    #[serde(default)]
    #[schemars(description = "Mutable current-state prose: where it stands, what's next.")]
    pub state: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "todo | active | paused | done | aborted. Omit to keep the stored status (a fresh project is `todo`); never empty."
    )]
    pub status: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Whose it is: a set of names, e.g. [\"Cline\",\"James\"]. Free text; empty = unassigned."
    )]
    pub owner: Option<Vec<String>>,
    #[serde(default)]
    #[schemars(
        description = "Aspects of the subject, e.g. continuity, storage. Omit to keep them; `[]` clears them."
    )]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    #[schemars(
        description = "ORDERED memory keys to load to work in here; replaces the previous list. May name memories this project did not author."
    )]
    pub preload: Option<Vec<String>>,
    #[serde(default)]
    #[schemars(
        description = "When the work started, ISO `YYYY-MM-DD`. Leave it out and it is stamped the moment the project stops being `todo` (a `todo` project has not started); it is never cleared. State one to backfill a late entry."
    )]
    pub start_date: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "When it ended, ISO `YYYY-MM-DD` — only with status done|aborted (both END a project; `status` says how, this says when). Stamped when omitted, cleared if reopened."
    )]
    pub end_date: Option<String>,
}

impl ProjectUpsertParams {
    pub(super) fn into_patch(self) -> project::Patch {
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

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectListParams {
    #[serde(default)]
    #[schemars(description = "Only this status.")]
    pub status: Option<String>,
    #[serde(default)]
    #[schemars(description = "Tag filter; a project must carry EVERY listed tag.")]
    pub tags: Vec<String>,
    #[serde(default)]
    #[schemars(
        description = "Owner filter; a project must list EVERY given name (case-insensitive)."
    )]
    pub owner: Vec<String>,
    #[serde(default)]
    #[schemars(
        description = "Literal/regex pattern over `key`/`title`/`goal`/`state`. Blank = no lexical leg."
    )]
    pub pattern: Option<String>,
    #[serde(default)]
    #[schemars(description = "How `pattern` is read: `literal` (substring, default) or `regex`.")]
    pub mode: Option<MatchModeParam>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChannelSendParams {
    #[schemars(
        description = "The channel key, without the `#`: `general`, `changes`, `emergency`, `ops_team`."
    )]
    pub channel: String,
    #[schemars(
        description = "The message. One log line, not a memory — a fact from a channel becomes a memory only when a pen writes one deliberately."
    )]
    pub body: String,
    #[serde(default)]
    #[schemars(
        description = "Optional `seq` of a message in the SAME channel. A reply to nothing is a new message, and the store says so rather than guessing."
    )]
    pub reply_to: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChannelHistoryParams {
    #[schemars(description = "The channel key, without the `#`.")]
    pub channel: String,
    #[serde(default)]
    #[schemars(
        description = "Return messages with `seq` greater than this. Omit for the beginning."
    )]
    pub since: Option<i64>,
    #[serde(default)]
    #[schemars(description = "Maximum messages to return (default 50, capped at 500).")]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChannelFetchParams {
    #[serde(default)]
    #[schemars(
        description = "Fetch only this channel's unread messages. Omit to listen to everything at once."
    )]
    pub channel: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Maximum messages to FETCH (default 50, capped at 500). Fetching marks them listened; use `channel_history` to re-read without touching the ledger."
    )]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChannelMemberParams {
    #[schemars(
        description = "The channel key, without the `#`. Joining `ops_team` also joins `emergency`, in the same transaction: a team that runs the cluster must be reachable when it falls over."
    )]
    pub channel: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChannelInviteParams {
    #[schemars(description = "The channel key, without the `#`.")]
    pub channel: String,
    #[schemars(
        description = "The pen to put in the channel. `estate` or `james` only: an invitation has an inviter. Inviting to `ops_team` also puts the pen in `emergency`."
    )]
    pub pen: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChannelPresentParams {
    #[serde(default)]
    #[schemars(
        description = "A token for THIS turn boundary (e.g. the task key plus a counter). The same token twice is the SAME opportunity and adds no miss, so a retry or a second step in one turn cannot lock the pen. Omit only if every call really is a new boundary."
    )]
    pub boundary: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChannelWaitParams {
    #[serde(default)]
    #[schemars(
        description = "Return messages the pen was TOLD with `seq` greater than this. Omit (or 0) for everything it has been told."
    )]
    pub after: Option<i64>,
    #[serde(default)]
    #[schemars(
        description = "Seconds to wait for something to arrive (default 0 = answer now, max 25). ★ This is a WAIT, not a listen: it moves no receipt, so it can never make a pen look up to date."
    )]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChannelMembersParams {
    #[schemars(description = "The channel key, without the `#`.")]
    pub channel: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChannelCreateParams {
    #[schemars(
        description = "The channel key, without the `#`: lowercase letters, digits, `_` and `-`. A key is created once; there is no rename."
    )]
    pub channel: String,
    #[schemars(description = "Human title, shown in the listing.")]
    pub title: String,
    #[serde(default)]
    #[schemars(
        description = "`open` (every live self unless it opts out, default) or `list` (invite-only)."
    )]
    pub members: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "What stops working while a member is behind: `none` (default), `writes`, or `all`. ★ A channel with any gate but `none` can only be created by `estate` or `james`, because a channel that can stop a pen writing to the record is a decision that gets a name attached."
    )]
    pub gate: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChannelGateParams {
    #[schemars(description = "The channel key, without the `#`.")]
    pub channel: String,
    #[schemars(
        description = "`none`, `writes` or `all`. ★ `estate` or `james` only: every change is recorded as a `channel_event` row, so a lock always has a name on it."
    )]
    pub gate: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChannelEventsParams {
    #[schemars(description = "The channel key, without the `#`.")]
    pub channel: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChannelRetentionParams {
    #[schemars(description = "The channel key, without the `#`.")]
    pub channel: String,
    #[schemars(
        description = "Seconds of message TEXT to keep; `0` = forever (the default). ★ A prune empties `body` and sets `pruned_at`; the message row, its seq and every delivery row survive, so 'you were told seq N' stays answerable. `estate` or `james` only, and the change is recorded."
    )]
    pub keep_secs: i64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChannelPruneParams {
    #[serde(default)]
    #[schemars(
        description = "Prune only this channel. Omit to prune every channel that has a `keep_secs`."
    )]
    pub channel: Option<String>,
}
