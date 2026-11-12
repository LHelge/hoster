-- Two lines can carry the same podman timestamp, so a cursor's time alone cannot say which of
-- them was the last one stored (ADR-0067 §1). `nth` is the cursor line's place in the run of
-- consecutive lines stamped `time_ns` in the container's log, 1 for the first: a follower that
-- resumes skips the log up to and including that line. Cursors written before this column
-- existed took one line at their time as read, which is what 1 says.
ALTER TABLE log_cursors ADD COLUMN nth INTEGER NOT NULL DEFAULT 1;
