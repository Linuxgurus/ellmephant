-- CHANNEL RETENTION (#163): a chat log may expire its TEXT and keep its RECEIPTS.
--
-- 0029 said it plainly: pruning may empty `body` and set `pruned_at`, but the delivery rows stay,
-- because "you were told" must remain answerable after the text is gone. The mechanism was never
-- built, so the one disposable table was the only one with no expiry.
--
-- ★ THE POLICY LIVES IN THE ROW, NOT IN A CRON LINE. `keep_secs = 0` means forever (the default:
-- silence must not delete anything). A value is a DECISION, recorded on the channel like a gate,
-- and `channel_set_retention` is the act that makes it — keeper-only, and it writes a channel_event.
ALTER TABLE channel ADD COLUMN keep_secs INTEGER NOT NULL DEFAULT 0 CHECK (keep_secs >= 0);
