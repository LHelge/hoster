-- Admin sessions and pending GitHub OAuth logins (ADR-0031).
--
-- Timestamps are milliseconds since the Unix epoch (UTC), as in 0001.

-- One server-side admin session. The cookie carries a random token; only its SHA-256 is stored,
-- so a copy of the database holds no usable session. `expires_at` is the absolute limit (12 h
-- after login); the idle limit is checked against `last_seen_at`.
CREATE TABLE sessions (
    token_hash   TEXT    PRIMARY KEY NOT NULL,
    csrf_token   TEXT    NOT NULL,
    github_login TEXT    NOT NULL,
    github_id    INTEGER NOT NULL,
    created_at   INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL,
    expires_at   INTEGER NOT NULL
) STRICT;

CREATE INDEX sessions_by_expiry ON sessions (expires_at);

-- An authorization request sent to GitHub and not yet answered: the OAuth `state` and the PKCE
-- verifier that goes with it. Taken (deleted) by the callback, so each one works once.
CREATE TABLE oauth_logins (
    state         TEXT    PRIMARY KEY NOT NULL,
    pkce_verifier TEXT    NOT NULL,
    created_at    INTEGER NOT NULL
) STRICT;
