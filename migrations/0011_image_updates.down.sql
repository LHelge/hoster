-- Reverts 0011_image_updates. The update settings and what the checker found go with the
-- tables; the older release does not check for updates. Its `EventKind` has no
-- `update_available`, and a row it cannot decode would break its Events page and dashboard
-- feed, so those events go too, with the time of the last pass.
DELETE FROM events WHERE kind = 'update_available';
DELETE FROM kv WHERE key = 'updates.last_pass';
DROP TABLE image_updates;
DROP TABLE image_checks;
DROP TABLE update_settings;
