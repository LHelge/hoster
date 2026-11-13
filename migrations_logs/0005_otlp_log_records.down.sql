-- Reverts 0005_otlp_log_records. Records received over OTLP are deleted: the previous binary
-- knows no `otlp` stream and could not read a page holding one.
DELETE FROM log_records WHERE stream = 'otlp';
ALTER TABLE log_records DROP COLUMN received;
