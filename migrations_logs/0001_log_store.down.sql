-- Reverts 0001_log_store: an empty `logs.db`. The records and request rows go with the tables
-- (ADR-0067: logs are not kept across a rollback, as they are not in backups).
DROP TABLE log_cursors;
DROP TABLE requests;
DROP TABLE log_records;
