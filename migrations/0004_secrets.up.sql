-- The encrypted secret store (ADR-0029).
--
-- One row per secret. `ciphertext` is the XChaCha20-Poly1305 sealing of the value (tag
-- included) under the key kept outside the state directory, with the secret's `name` as
-- associated data, so a value cannot be moved to another name. `nonce` is fresh random for every
-- write, so it also serves as the secret's version in the per-service config hash. Timestamps are
-- milliseconds since the Unix epoch (UTC), as in 0001.
CREATE TABLE secrets (
    name       TEXT    PRIMARY KEY NOT NULL,
    nonce      BLOB    NOT NULL CHECK (length(nonce) = 24),
    ciphertext BLOB    NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;
