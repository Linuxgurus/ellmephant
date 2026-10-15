-- 0008: the encode queue — the rows that owe the vector index a document.
--
-- Until now a write embedded its document INLINE, under the store lock: `add_impl` is careful to
-- embed before it opens the transaction, but it is still inside `with_store`, which holds the one
-- `Arc<Mutex<Session>>` for the whole call. So one slow write stalled every other request, reads
-- included, and `memory_reembed` held the same lock for its entire run — a total write outage
-- (measured: ~45 minutes on the live store, 2026-10-15).
--
-- The queue splits the two. A write upserts the row and schedules the document in the SAME
-- transaction — one SQLite file, so the enqueue cannot be lost while the row survives, which is
-- exactly what a second database could not promise — and a worker embeds it later, holding the lock
-- for nothing longer than a row write.
--
-- Derived state, like `memory_vec` and `memory_link`: the rows are the truth, so a lost or corrupted
-- queue costs a re-embed, never a memory. `key` is the primary key because re-enqueueing a key is
-- not two jobs — it is one, for the text as it stands now.
CREATE TABLE encode_queue (
  key        TEXT PRIMARY KEY,           -- the memory this document belongs to
  kind       TEXT NOT NULL DEFAULT 'memory',
  priority   INTEGER NOT NULL DEFAULT 0, -- 0 = interactive, 1 = backfill; lower sorts first
  doc_hash   TEXT NOT NULL,              -- the document THIS entry is for; the dequeue guard
  queued_at  TEXT NOT NULL DEFAULT (datetime('now')),
  attempts   INTEGER NOT NULL DEFAULT 0,
  last_error TEXT
);

-- The worker's one hot query: the next entry is the lowest priority, then the oldest.
CREATE INDEX encode_queue_due ON encode_queue (priority, queued_at);
