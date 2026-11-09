-- Run-time settings of this instance (ADR-0058 §1, §2): one typed row, edited in Settings.

-- The instance name, a display label shown in the sidebar and in mail subjects. NULL means the
-- default, the machine's hostname. The row exists once something has been set.
CREATE TABLE instance_settings (
    id         INTEGER PRIMARY KEY NOT NULL CHECK (id = 1),
    name       TEXT CHECK (name IS NULL OR length(name) BETWEEN 1 AND 64),
    updated_at INTEGER NOT NULL
) STRICT;
