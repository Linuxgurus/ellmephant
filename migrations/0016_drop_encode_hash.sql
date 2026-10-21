-- 0016_drop_encode_hash — the stamp goes, and nothing replaces it.
--
-- WHY: `encode_hash` was a per-row copy of a fact about a DERIVED index, stored on a truth row. It is
-- what made two silent no-ops possible: a reset queue that drained embedding nothing (the hash still
-- matched, so the worker declined to embed), and a hash claiming "indexed" over a torn index (the
-- vector was gone and the column still said otherwise).
--
-- The work now lives where it can be checked: `encode_queue` holds the RENDERED text and its own
-- settle guard (content equality), and the index itself answers whether a vector exists. A document's
-- vector is current iff no queue entry names it — an invariant that survives a torn index, which a
-- hash on the row could not.
--
-- Nothing reads this column any more: "did the document change?" is a comparison of title and body,
-- which is exactly what the renderer reads.
--
-- ★ This is a ONE-WAY migration in the strict sense: the data is not recoverable from the column after
-- it drops. It is a hash of text that still exists in the row, so nothing is lost — but the column is
-- the last copy of the OLD recipe's expectation, and that is the point of removing it.

ALTER TABLE memory DROP COLUMN encode_hash;
