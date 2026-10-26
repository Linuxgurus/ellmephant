-- A BOUNDARY IS AN IDENTITY, NOT A CLICK (#159 review).
--
-- `present` used to add a miss on EVERY call. A body that presents on each step of a multi-step
-- turn — or retries once — therefore locked itself for doing its work, which is the exact failure
-- the gate was redesigned to avoid. A turn boundary now names itself, and a second call with the
-- same name is the SAME opportunity: no extra miss.
--
-- Deliberately per pen: a boundary token is a caller's own word, and two bodies must not be able to
-- collide by choosing the same turn id.
CREATE TABLE channel_boundary (
    pen_id   INTEGER PRIMARY KEY REFERENCES pen(id),
    boundary TEXT NOT NULL,
    at       TEXT NOT NULL DEFAULT (datetime('now'))
);
