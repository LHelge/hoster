-- Reverts 0002_sessions.
DROP TABLE oauth_logins;
DROP INDEX sessions_by_expiry;
DROP TABLE sessions;
