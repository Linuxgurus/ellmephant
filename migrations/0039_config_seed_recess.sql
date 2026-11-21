
INSERT OR IGNORE INTO config (key, value, set_audience, read_audience, source)
VALUES
    ('recess_daily_seconds', '1800', 'pen:estate', 'public',
     'seed 0039 — James, 2026-10-31: the DEFAULT daily amount for a store with no policy is 30m. The LIVE rate is a separate, estate-authorized config_set (7200 = 2h) made immediately after this migration, before any consumer is deployed; 1800 is never the live rate.'),
    ('recess_daily_grant_at', '18:00', 'pen:estate', 'public',
     'seed 0039 — James, 2026-10-31: the daily salary lands at 18:00 local. A default, because the amount above is a separate decision.');
