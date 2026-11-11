-- Reverts 0012_auto_updates. The older release has no `auto_update` deployment source and
-- cannot decode a row that has one, so those deployments are relabelled as applies, which is
-- what they were: a new revision deployed through the apply path.
UPDATE deployments SET source = 'apply' WHERE source = 'auto_update';
DROP TABLE auto_update_attempts;
