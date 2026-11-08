-- Reverts 0006_webhook: drops the webhook tables and the deployment provenance columns.
DROP INDEX webhook_rejections_by_stack;
DROP TABLE webhook_rejections;
DROP INDEX webhook_tokens_by_expiry;
DROP TABLE webhook_tokens;
ALTER TABLE deployments DROP COLUMN source_tag;
ALTER TABLE deployments DROP COLUMN source_actor;
ALTER TABLE deployments DROP COLUMN source_run_id;
ALTER TABLE deployments DROP COLUMN source_ref;
ALTER TABLE deployments DROP COLUMN source_sha;
ALTER TABLE deployments DROP COLUMN source_repository;
