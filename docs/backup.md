# Backing up and restoring hoster

hoster v1 does not run backups itself (ADR-0030). This page lists what to back up, how to take
each piece consistently, and how to restore a host from it. Built-in scheduled backups are a
later milestone.

Paths below assume the defaults: the state directory `~/.local/share/hoster` and the config
directory `~/.config/hoster` of the `hoster` user (ADR-0023, ADR-0024). If you set `state_dir`,
use that instead. Run every command as the `hoster` user.

## What to back up

| What | Where | Needed? | How |
| --- | --- | --- | --- |
| State database | `<state_dir>/hoster.db` (plus `-wal`/`-shm`) | Yes | `hoster backup db <file>` |
| Secret-store key | key file in `~/.config/hoster/`, or the user systemd credential | Yes, **stored separately** | Copy the file / credential |
| Configuration | `~/.config/hoster/hoster.toml`, the OAuth client secret file, the systemd unit and drop-ins | Yes | Copy the files |
| ACME account | `<state_dir>/acme/` | Optional | Copy the directory |
| Certificates | `<state_dir>/certs/` | Optional | Copy the directory |
| App volumes | podman volumes `hoster-<stack>-<volume>` | Per app | `podman volume export` |

### State database

The database holds stacks and their revisions, deployment history, port allocations, sessions and
the encrypted secrets (ADR-0022, ADR-0029). Never copy `hoster.db` with `cp` while hoster runs:
the newest writes may still be in `hoster.db-wal`, and a plain copy can be torn. Use:

```sh
hoster backup db ~/backups/hoster-$(date +%Y%m%d-%H%M%S).db
```

This writes a consistent snapshot with SQLite's `VACUUM INTO`. It is safe while hoster runs: the
snapshot is one read transaction and does not block hoster's writes for long. The command:

- opens the database read-only and never creates or migrates it;
- writes the snapshot `0600` to a hidden temporary file next to the target, checks it with
  `PRAGMA integrity_check` and moves it into place, so the target only ever appears complete;
- refuses to overwrite an existing file, so pick a new name each time;
- uses the same `--config` and `HOSTER_*` overrides as `hoster run` to find `state_dir`.

The snapshot is a single self-contained SQLite file (no `-wal`/`-shm`). It contains the secrets
encrypted, so it is useless without the key, and the key is useless without it.

Self-update and rollback (ADR-0052) take the same kind of snapshot automatically, into the state
directory: `hoster-pre-update-<from>-to-<to>-<unix time>.db` before switching to a new version
and `hoster-pre-rollback-<from>-to-<to>-<unix time>.db` before undoing the schema. hoster does
not delete them; remove old ones by hand once the new version runs well.

### Secret-store key

The key that encrypts secrets lives outside the state directory on purpose (ADR-0029): either a
`0600` key file in `~/.config/hoster/` or a user-scoped systemd credential. **Back it up
separately from the database**, for example in a password manager or on different storage. If
you lose it, every secret (app secrets and registry credentials) has to be set again; there is
no recovery. If someone gets both the key and a database backup, they have every secret.

### ACME account and certificates

`acme/account.json` is the ACME account key and `certs/<host>/` holds each certificate and its
private key (docs/architecture.md §7.4). Both are optional: without them hoster registers a new
account and re-issues certificates when the stacks are applied (ADR-0036). Keeping them avoids
hitting Let's Encrypt rate limits after a restore, which matters if you host many hostnames.
They are plain files that are written atomically, so copying them while hoster runs is fine:

```sh
tar -C ~/.local/share/hoster -czf ~/backups/hoster-tls.tar.gz acme certs
```

They contain private keys; keep the archive `0600`.

### Volumes

App data lives in named podman volumes `hoster-<stack>-<volume>` (ADR-0030). They are kept when
a stack is removed. List them and export each one:

```sh
podman volume ls --filter label=hoster.managed=true
podman volume export hoster-<stack>-<volume> --output ~/backups/hoster-<stack>-<volume>.tar
```

`podman volume export` copies files as they are. For a database or anything else that writes
continuously, consistency is the app's concern: stop the service first, or use the app's own dump
tool (for example `pg_dump` via `podman exec`) and back up the dump instead.

## Restoring

To restore onto a fresh host (or the same host after a loss):

1. **Set up the host** as in [install.md](install.md): the `hoster` user, linger, the sysctl,
   `podman.socket`, the binary and the systemd unit. Run `hoster doctor`.
2. **Restore the configuration**: `hoster.toml`, the OAuth client secret, and the
   **secret-store key** file (mode `0600`) or the systemd credential, exactly as before.
3. **Make sure hoster is stopped**: `systemctl --user stop hoster`.
4. **Restore the database**. Put the snapshot in place as `hoster.db` and remove any stale
   `-wal`/`-shm` files, which belong to a different database:

   ```sh
   mkdir -p -m 0700 ~/.local/share/hoster
   rm -f ~/.local/share/hoster/hoster.db-wal ~/.local/share/hoster/hoster.db-shm
   install -m 0600 ~/backups/hoster-20261107-120000.db ~/.local/share/hoster/hoster.db
   ```

5. **Optionally restore ACME state and certificates**:

   ```sh
   tar -C ~/.local/share/hoster -xzf ~/backups/hoster-tls.tar.gz
   ```

6. **Restore volumes** before the stacks start, so the apps find their data. Create each volume
   with hoster's labels and import the archive:

   ```sh
   podman volume create --label hoster.managed=true --label hoster.stack=<stack> \
     hoster-<stack>-<volume>
   podman volume import hoster-<stack>-<volume> ~/backups/hoster-<stack>-<volume>.tar
   ```

7. **Start hoster**: `systemctl --user start hoster`. On startup it checks the database's
   integrity, migrates it if the binary is newer, and reconciles: the services recorded in the
   database are started again from their image digests (ADR-0028). Images are pulled from the
   registry, so the registry credentials (in the restored database) must still be valid.
8. **Check** the admin UI: the stacks, their latest deployments and the secret names are there,
   and the apps answer on their hostnames. If DNS now points at a new IP, update it first;
   certificates that were not restored are issued as soon as the hostnames resolve.

A snapshot from a newer hoster may not open with an older binary; restore with the same or a
newer version.
