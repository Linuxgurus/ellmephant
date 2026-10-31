-- RETIRE: the recess policy rows moved to the brain (migration 0041, 2026-10-31).
--
-- 0039 and 0040 seeded three config rows so ONE store was the place a recess was priced:
--
--   recess_daily_seconds      the bank, in seconds
--   recess_sitting_seconds    one sitting, in seconds
--   recess_daily_grant_at     when the day's salary lands, HH:MM local
--
-- ★ THE RECESS IS NOT A MEMORY, IT IS A LEDGER. James, 2026-10-31: the memstore is for memories
-- (and the policy that configures it), not for event logs. A clock, its spends and its bonuses were
-- rows written by a Ruby client, and the launcher priced the child it was about to run — so the
-- store was being used as ELK, and the ledger's owner was the thing it paid.
--
-- ★ THE POLICY MOVED WITH THE BANK. `brain.db` migration v14 keeps the same four answers in
-- `recess_config` (daily_secs, sitting_secs, grant_at, plus the dated rate boundary). A config row
-- here that no longer has a reader is not a policy: it is a second copy of one fact, and the copy
-- that nobody updates is always the one somebody eventually reads.
--
-- ★ DELETE, NOT A TOMBSTONE. A row whose value says "moved to brain.db" is still a row a generic
-- `config_list` shows as configuration. The honest address is the brain's own table, and the place
-- to learn where it went is this file — which is in git, dated, and reviewable.
--
-- ★ THE MIGRATION IS THE ONLY DOOR. `config` has no delete verb on purpose (see 0038): a policy is
-- changed by an operator through `config_set`, and authority is handed out by `config_define`. A
-- retirement is a fact about the past, so it belongs here rather than in any pen's credentials.
--
-- The memories about the recess itself (`recess`, `prj:recess`) are NOT here: they are memories and
-- they stay. Only the three policy rows, and only because a different program now answers them.

DELETE FROM config WHERE key IN (
    'recess_daily_seconds',
    'recess_sitting_seconds',
    'recess_daily_grant_at'
);
