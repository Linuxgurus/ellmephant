
DELETE FROM config WHERE key IN (
    'recess_daily_seconds',
    'recess_sitting_seconds',
    'recess_daily_grant_at'
);
