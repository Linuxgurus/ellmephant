-- CONFIG: a stored value with a WRITTEN ACL (migration 0038, 2026-10-31).
--
-- Why not a memory: a memory is a document a pen edits freely, and `audience` protects READS only.
-- Configuration is the other shape — a small answer to "how should this run" — and it has TWO
-- boundaries, not one: who may CHANGE the value, and who may SEE it. A memory row cannot express a
-- write boundary at all. `sudoers` is the pattern that can: policy in columns, one row per edge,
-- and the reason recorded on the row.
--
-- ★ TWO AUDIENCES, TWO QUESTIONS. `set_audience` is who may write the value; `read_audience` is who
-- may read it. Both use the SAME grammar as `memory.audience` (`public`, or comma-separated
-- `pen:<name>` / `subtree:<name>`), deliberately: one parser, one predicate, learned once. A second
-- grammar for permissions is a second place to get the boundary wrong.
--
-- ★ AN ACL IS NOT SET THROUGH `config_set`. Changing `set_audience` is provisioning, like `sudoers`:
-- whoever can write the ACL can hand out authority. `config_define` is the separate act, it may not
-- name an address the caller cannot reach (no amplification), and it REQUIRES a `source` so the
-- reason is visible in the row rather than inferred.
--
-- ★ `set_audience` MAY NOT BE `public`. A value anyone can write is not configuration; it is a
-- shared scratchpad, and it will be written by whichever pen runs first. Reads may be public.
--
-- ★ THE KEY IS AN ADDRESS, NOT A SLUG. `recess_daily_time` is the operator's name; case is folded
-- (`COLLATE NOCASE`) because two spellings of one setting are one setting.

CREATE TABLE config (
    key           TEXT PRIMARY KEY COLLATE NOCASE,
    value         TEXT NOT NULL,
    -- WHO MAY WRITE the value.
    set_audience  TEXT NOT NULL DEFAULT 'pen:estate',
    -- WHO MAY READ it.
    read_audience TEXT NOT NULL DEFAULT 'public',
    -- Who set the CURRENT value — a pen row, not a string, so a retirement shows here too.
    owner         INTEGER REFERENCES pen(id),
    updated_at    TEXT NOT NULL DEFAULT (datetime('now')),
    -- WHY this row exists / where the value came from. The trust root records its reason.
    source        TEXT NOT NULL DEFAULT ''
);

-- A reader asks "which items may I read" and an operator asks "what may this pen write"; both scan
-- by audience. The table is expected to be tiny, so the index costs nothing and makes the intent
-- legible.
CREATE INDEX config_set_audience ON config(set_audience);
CREATE INDEX config_read_audience ON config(read_audience);
