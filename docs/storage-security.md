# Local data at rest

Chaos Control stores chat history, members, approvals and recent projects in
the user's data directory. This page records what protects those files today,
what it does not protect against, and why we are **not** adding SQLCipher yet.

## Where the data lives

`crates/app/src/agents.rs::data_dir()` picks the directory, in order:

1. `CHAOS_CONTROL_DATA` if set;
2. `~/Library/Application Support/Chaos Control` on macOS;
3. `$XDG_DATA_HOME/chaos-control` (or `~/.local/share/chaos-control`) elsewhere.

Inside it: `core.db`, `Projects/<project-id>/workspace.db` and `agents.db`,
plus JSON files (`agents.json`, `recents.json`, `profile.json`,
`composer_suggestions.json`, cached model lists). SQLite runs in WAL mode, so
`-wal` and `-shm` sidecars appear next to each database while it is open.

## What we do

Implemented in `crates/core/src/permissions.rs` and applied by the store and
the JSON writers:

- Directories the app owns are created with mode `0700` on Unix. Existing
  directories are tightened on open, so an install made before this policy is
  protected on upgrade. Only directories the app owns are changed; the user's
  home directory is left alone.
- Database and JSON files are created with mode `0600` on Unix, and existing
  files are tightened when touched.
- SQLite creates `-wal` and `-shm` with the same mode as the database file, so
  a private database yields private sidecars. Leftover sidecars from an older
  run are tightened on open, as is `-journal` for rollback-mode databases.
- Atomic writes use a private temp file in the same directory, then a rename.
  There is no window where a half-written or world-readable copy exists.

This is deliberately the least-complex protection that changes the threat
model: another local user can no longer read the data directory or its files,
and processes running as other users cannot read WAL/SHM either.

## What this does not cover

- **A process running as the same user.** It can `open()` any `0600` file
  owned by that user. Only encryption with a key the app holds separately
  would change that, and the key would be available to the same process. This
  is not a sandbox boundary.
- **Root / administrators, backups, cloud sync, or physical access.** File
  modes do not follow copies to destinations that do not preserve them.
- **Files inside the user's projects.** `.chaos/project.toml` and
  `.git/info/exclude` keep the permissions the user or git gave them: they may
  be committed or shared with teammates on purpose. The app does not chmod
  project files.
- **Platforms without Unix mode bits.** On Windows these helpers are no-ops.
  Files inherit the user's ACL from the data directory, which normally grants
  access only to that user (plus administrators). On FAT/exFAT or some network
  shares mode bits are ignored; treat those locations as unprotected.

## Why not SQLCipher (yet)

Encrypting `core.db` would only help if the key has a lifecycle we can stand
behind. We do not have one today:

- **Key storage:** macOS Keychain and the Linux Secret Service are the natural
  homes, but both can be unavailable (headless Linux, locked keyring, CI). We
  would need a documented fallback and a first-run unlock flow.
- **Recovery:** losing the key means losing all chat history irreversibly. We
  would need recovery codes or an explicit "this store is unrecoverable"
  flow, and a test for it.
- **Migration:** existing plaintext databases have to be re-encrypted in
  place, atomically, with a crash-safe marker, and the split store has three
  databases per project plus a legacy migration path.
- **Threat model:** the main gain is against same-user processes, which can
  also read the key once the app has unlocked it. The gain over `0700`/`0600`
  is small for a single-user desktop app.

If a requirement arrives that genuinely needs encryption at rest, the first
step is to design the key lifecycle (creation, unlock, rotation, recovery,
headless fallback) and write it down here. Adopting SQLCipher without that is
a data-loss risk, not a security improvement.

## Verification

- `cargo test -p chaos-core --lib permissions` checks directory and file
  tightening, including database sidecars.
- `cargo test -p chaos-core --lib opening_a_store_tightens` checks a real
  `Store::open`: data dir, `core.db`, project dirs, project databases and any
  WAL/SHM files that SQLite created.
- Manual check on Unix: `ls -ld "$CHAOS_CONTROL_DATA"` should show `drwx------`,
  and `ls -l` inside it should show `-rw-------` for databases and JSON files.
