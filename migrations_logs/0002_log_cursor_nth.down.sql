-- Reverts 0002_log_cursor_nth. A cursor in the middle of a run of equal timestamps then reads
-- as the first line of the run, so the rest of that run may be stored again after a restart.
ALTER TABLE log_cursors DROP COLUMN nth;
