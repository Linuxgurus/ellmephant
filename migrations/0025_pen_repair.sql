-- REPAIR: `anonymous` was never a pen (soma/ellmephant#109, found in the pre-roll rehearsal).
--
-- 0021 seeded every name that had ever spoken, and `anonymous` is not a name — it means **no currently
-- existing pen wrote this** (James). Seeding it made a pen, and 0023's backfill then handed it
-- every row whose author was never recorded: on a copy of the live store, 249 rows.
--
-- The consequence is the opposite of what the store wants to say. Those rows are UNATTRIBUTED — nobody
-- wrote them and nobody answers for them — and they must be visible as such, claimable by somebody,
-- rather than silently owned by a fiction. 0021 is fixed at the source for fresh stores; this repairs
-- any store that already ran the old seed.
--
-- ★ The rows are NOT deleted, and nobody is ASSIGNED. Un-owning them restores the question ("who
-- answers for these?") instead of answering it with `nobody`.

-- ★ EVERY reference to the ghost goes before the ghost does. Checked against the schema rather than
-- remembered: `pen` is referenced by memory.owner, memory_version.edited_by, three pen_event columns,
-- and both sudo tables — and the first attempt at this migration failed on a foreign key, which is
-- the check doing its job on an assumption.

-- A session or a policy that names `anonymous` is a fiction about a fiction: it authorises nobody to
-- borrow from nobody. Deleted, not nulled — there is nothing to reassign.
DELETE FROM active_sudo WHERE powerful IN (SELECT id FROM pen WHERE lower(name) = 'anonymous')
                           OR weak     IN (SELECT id FROM pen WHERE lower(name) = 'anonymous');
DELETE FROM sudoers    WHERE powerful IN (SELECT id FROM pen WHERE lower(name) = 'anonymous')
                           OR weak     IN (SELECT id FROM pen WHERE lower(name) = 'anonymous');

-- Version snapshots record who WROTE the state. A ghost author is the same bug one table over: the
-- fact is "nobody was recorded", which the snapshot can say with NULL.
UPDATE memory_version SET edited_by = NULL
 WHERE edited_by IN (SELECT id FROM pen WHERE lower(name) = 'anonymous');

-- The ledger's references to the ghost go first: a pen_event naming a pen that is about to be deleted
-- would be a record pointing at nothing.
UPDATE pen_event SET pen_id  = NULL WHERE pen_id  IN (SELECT id FROM pen WHERE lower(name) = 'anonymous');
UPDATE pen_event SET from_pen = NULL WHERE from_pen IN (SELECT id FROM pen WHERE lower(name) = 'anonymous');
UPDATE pen_event SET to_pen  = NULL WHERE to_pen  IN (SELECT id FROM pen WHERE lower(name) = 'anonymous');

-- Then the ownership the ghost should never have had. `owner IS NULL` is the commons: anybody may
-- write, and anybody may adopt — which for these rows is the honest starting point, because nobody
-- has claimed them yet.
UPDATE memory SET owner = NULL
 WHERE owner IN (SELECT id FROM pen WHERE lower(name) = 'anonymous');

DELETE FROM pen WHERE lower(name) = 'anonymous';
