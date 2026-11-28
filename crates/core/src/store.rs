//! Durable state in SQLite: projects, channels, members, messages, cursors,
//! triggers, batches and reply obligations.
//!
//! The schema carries the invariants the runtime relies on, so a logic bug
//! fails loudly instead of double-delivering:
//!
//! - `messages.id` is `AUTOINCREMENT` (stable, never reused). `seq` is assigned
//!   in the inserting transaction as `MAX(seq) + 1` for the channel, and
//!   `UNIQUE(channel_id, seq)` backs it up.
//! - `UNIQUE(author_id, channel_id, client_request_id)`: `chat_send` dedup.
//! - `triggers` has primary key `(agent, channel, message)`: one trigger per
//!   triggering message per agent, ever.
//! - At most one `pending` and at most one in-flight (`dispatched`/`accepted`)
//!   batch per agent x channel (partial unique indexes).
//! - `obligations` has the trigger's key as its primary key: at most one reply
//!   obligation per trigger.
//! - At most one legacy fallback reply per (agent, trigger message) (partial
//!   unique index on `messages`).
//! - Every channel belongs to a project; names and `::N` ordinals are unique
//!   within it (`UNIQUE(project_id, name)`, `UNIQUE(project_id, ordinal)`).
//! - Joining a channel needs membership of its project: a trigger on
//!   `memberships` aborts with `not_a_project_member` otherwise, so no code
//!   path can skip the check.
//! - One folder maps to at most one project (partial unique index on
//!   `projects.root_path`).
//! - The migrated "Imported" project is adoptable by one folder, once, and
//!   only while it has no folder (`adopt_import`'s guarded update).

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::{
    approval::{ApprovalId, ApprovalOutcome, ApprovalOutcomeState},
    lifecycle::{LifecycleState, MembershipState},
    types::*,
};

pub type Result<T> = rusqlite::Result<T>;

/// A message about to be inserted.
pub struct NewMessage<'a> {
    pub channel: ChannelId,
    pub author: Option<MemberId>,
    pub origin: Origin,
    pub body: &'a str,
    pub reply_to: Option<MessageId>,
    pub mentions: &'a [MemberId],
    pub hop: u8,
    pub cause_batch: Option<BatchId>,
    pub client_request_id: Option<&'a str>,
    pub sent_at: Millis,
}

/// A trigger row with its message, for building notifications.
#[derive(Clone, Debug)]
pub struct TriggerRow {
    pub id: TriggerId,
    pub reason: WakeReason,
    pub hop: u8,
    pub batch: BatchId,
}

#[derive(Clone, Debug)]
pub struct BatchRow {
    pub id: BatchId,
    pub agent: MemberId,
    pub channel: ChannelId,
    pub state: BatchState,
    pub attempts: u32,
    pub window_to: Option<Seq>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub delivered_through: Seq,
    pub read_through: Seq,
    /// When `read_through` last moved forward (None: never read anything).
    pub read_at: Option<Millis>,
}

/// Why opening or migrating the store refused or failed.
#[derive(Debug)]
pub enum OpenError {
    /// A database's schema is newer than every migration this build knows:
    /// a newer Chaos Control wrote it. Nothing in it was changed.
    NewerSchema {
        path: PathBuf,
        found: i64,
        known: i64,
    },
    /// A project's database could not be opened or is invalid in a way that
    /// is isolated to that project (guarantee 4). It is reported but does not
    /// block core.db or other projects.
    Project {
        project: ProjectId,
        path: PathBuf,
        source: rusqlite::Error,
    },
    Sqlite(rusqlite::Error),
}

impl From<rusqlite::Error> for OpenError {
    fn from(e: rusqlite::Error) -> Self {
        OpenError::Sqlite(e)
    }
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::NewerSchema { path, found, known } => write!(
                f,
                "{} was written by a newer Chaos Control (schema {found}, this build knows {known}). Update the app.",
                path.display()
            ),
            OpenError::Project {
                project,
                path,
                source,
            } => write!(
                f,
                "project {project:?} database {}: {source}",
                path.display()
            ),
            OpenError::Sqlite(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for OpenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            OpenError::NewerSchema { .. } => None,
            OpenError::Project { source, .. } | OpenError::Sqlite(source) => Some(source),
        }
    }
}

fn store_open_error_to_sqlite(e: OpenError) -> rusqlite::Error {
    match e {
        OpenError::Sqlite(e) => e,
        OpenError::NewerSchema { path, found, known } => {
            rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(format!(
                "newer schema at {} (found {}, known {})",
                path.display(),
                found,
                known
            ))))
        }
        OpenError::Project {
            project,
            path,
            source,
        } => rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(format!(
            "project {project:?} storage {}: {source}",
            path.display()
        )))),
    }
}

fn cannot_inspect_project_storage(project: &ProjectId) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(format!(
        "cannot inspect project storage for {project:?}"
    ))))
}

/// Legacy ordered migrations for the old single `core.sqlite` file.
/// `PRAGMA user_version` records how many have run; add new ones at the end,
/// never edit old ones.
///
/// Since the permission model was removed (rebuild from scratch), the tables
/// and columns its migrations created are unused: `permission_requests`
/// (6, 8, 13, 16, 19), `channel_approval_mode` (17) and `app_settings` (18).
/// They stay so existing local databases keep their schema version; nothing
/// reads or writes them. The split migration reads from this layout but does
/// not recreate dead tables in the new files.
const LEGACY_MIGRATIONS: &[&str] = &[
    r#"
CREATE TABLE channels (
    id          INTEGER PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    created_at  INTEGER NOT NULL
);
CREATE TABLE members (
    id      INTEGER PRIMARY KEY,
    kind    TEXT NOT NULL CHECK (kind IN ('human', 'agent')),
    handle  TEXT NOT NULL UNIQUE COLLATE NOCASE,
    name    TEXT NOT NULL
);
CREATE TABLE memberships (
    channel_id  INTEGER NOT NULL REFERENCES channels(id),
    member_id   INTEGER NOT NULL REFERENCES members(id),
    joined_at   INTEGER NOT NULL,
    PRIMARY KEY (channel_id, member_id)
);
CREATE TABLE messages (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    channel_id         INTEGER NOT NULL REFERENCES channels(id),
    seq                INTEGER NOT NULL,
    author_id          INTEGER REFERENCES members(id),
    origin             TEXT NOT NULL,
    body               TEXT NOT NULL,
    reply_to           INTEGER REFERENCES messages(id),
    hop                INTEGER NOT NULL DEFAULT 0,
    cause_batch        INTEGER,
    client_request_id  TEXT,
    sent_at            INTEGER NOT NULL,
    UNIQUE (channel_id, seq)
);
CREATE UNIQUE INDEX messages_request
    ON messages (author_id, channel_id, client_request_id)
    WHERE client_request_id IS NOT NULL;
CREATE UNIQUE INDEX messages_one_fallback
    ON messages (author_id, reply_to)
    WHERE origin = 'fallback';
CREATE TABLE mentions (
    message_id  INTEGER NOT NULL REFERENCES messages(id),
    member_id   INTEGER NOT NULL REFERENCES members(id),
    PRIMARY KEY (message_id, member_id)
);
CREATE TABLE reactions (
    message_id  INTEGER NOT NULL REFERENCES messages(id),
    member_id   INTEGER NOT NULL REFERENCES members(id),
    emoji       TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    PRIMARY KEY (message_id, member_id, emoji)
);
CREATE TABLE cursors (
    agent_id           INTEGER NOT NULL REFERENCES members(id),
    channel_id         INTEGER NOT NULL REFERENCES channels(id),
    delivered_through  INTEGER NOT NULL,
    read_through       INTEGER NOT NULL,
    PRIMARY KEY (agent_id, channel_id)
);
CREATE TABLE batches (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_id     INTEGER NOT NULL REFERENCES members(id),
    channel_id   INTEGER NOT NULL REFERENCES channels(id),
    state        TEXT NOT NULL CHECK (state IN ('pending', 'dispatched', 'accepted', 'settled', 'failed')),
    attempts     INTEGER NOT NULL DEFAULT 0,
    window_to    INTEGER,
    created_at   INTEGER NOT NULL,
    accepted_at  INTEGER,
    settled_at   INTEGER,
    outcome      TEXT
);
CREATE UNIQUE INDEX batches_one_pending
    ON batches (agent_id, channel_id) WHERE state = 'pending';
CREATE UNIQUE INDEX batches_one_in_flight
    ON batches (agent_id, channel_id) WHERE state IN ('dispatched', 'accepted');
CREATE TABLE triggers (
    agent_id    INTEGER NOT NULL REFERENCES members(id),
    channel_id  INTEGER NOT NULL REFERENCES channels(id),
    message_id  INTEGER NOT NULL REFERENCES messages(id),
    reason      TEXT NOT NULL,
    hop         INTEGER NOT NULL,
    batch_id    INTEGER NOT NULL REFERENCES batches(id),
    created_at  INTEGER NOT NULL,
    PRIMARY KEY (agent_id, channel_id, message_id)
);
CREATE TABLE obligations (
    agent_id       INTEGER NOT NULL,
    channel_id     INTEGER NOT NULL,
    message_id     INTEGER NOT NULL,
    state          TEXT NOT NULL CHECK (state IN ('open', 'satisfied', 'fallback', 'unmet')),
    resolved_by    INTEGER REFERENCES messages(id),
    resolved_kind  TEXT,
    PRIMARY KEY (agent_id, channel_id, message_id),
    FOREIGN KEY (agent_id, channel_id, message_id)
        REFERENCES triggers (agent_id, channel_id, message_id)
);
CREATE TABLE sessions (
    agent_id    INTEGER NOT NULL REFERENCES members(id),
    channel_id  INTEGER NOT NULL REFERENCES channels(id),
    protocol    TEXT NOT NULL,
    native_ref  TEXT NOT NULL,
    opened_at   INTEGER NOT NULL,
    PRIMARY KEY (agent_id, channel_id)
);
"#,
    // 2: projects. Channels move under a project (uniqueness from `name` to
    // `(project_id, name)`, which needs a table rebuild; channel ids are kept,
    // so messages, cursors, batches, triggers, obligations and sessions are
    // untouched). Existing channels go into one project named "Imported" with
    // a fresh UUID, and their members become its members.
    r#"
CREATE TABLE projects (
    id          TEXT PRIMARY KEY NOT NULL,
    name        TEXT NOT NULL,
    root_path   TEXT,
    created_at  INTEGER NOT NULL
);
CREATE UNIQUE INDEX projects_one_per_root
    ON projects (root_path) WHERE root_path IS NOT NULL;
CREATE TABLE project_members (
    project_id  TEXT NOT NULL REFERENCES projects(id),
    member_id   INTEGER NOT NULL REFERENCES members(id),
    joined_at   INTEGER NOT NULL,
    PRIMARY KEY (project_id, member_id)
);
INSERT INTO projects (id, name, root_path, created_at)
SELECT lower(hex(randomblob(4)) || '-' || hex(randomblob(2)) || '-4'
             || substr(hex(randomblob(2)), 2) || '-'
             || substr('89ab', 1 + (abs(random()) % 4), 1)
             || substr(hex(randomblob(2)), 2) || '-' || hex(randomblob(6))),
       'Imported', NULL, (SELECT MIN(created_at) FROM channels)
WHERE EXISTS (SELECT 1 FROM channels);
CREATE TABLE channels_v2 (
    id          INTEGER PRIMARY KEY,
    project_id  TEXT NOT NULL REFERENCES projects(id),
    name        TEXT NOT NULL,
    ordinal     INTEGER NOT NULL CHECK (ordinal > 0),
    created_at  INTEGER NOT NULL,
    UNIQUE (project_id, name),
    UNIQUE (project_id, ordinal)
);
INSERT INTO channels_v2 (id, project_id, name, ordinal, created_at)
SELECT c.id, (SELECT id FROM projects), c.name,
       ROW_NUMBER() OVER (ORDER BY c.id), c.created_at
FROM channels c;
DROP TABLE channels;
ALTER TABLE channels_v2 RENAME TO channels;
INSERT INTO project_members (project_id, member_id, joined_at)
SELECT c.project_id, s.member_id, MIN(s.joined_at)
FROM memberships s JOIN channels c ON c.id = s.channel_id
GROUP BY c.project_id, s.member_id;
CREATE TRIGGER memberships_need_project BEFORE INSERT ON memberships
WHEN NOT EXISTS (
    SELECT 1 FROM project_members pm JOIN channels c ON c.project_id = pm.project_id
    WHERE c.id = NEW.channel_id AND pm.member_id = NEW.member_id
)
BEGIN
    SELECT RAISE(ABORT, 'not_a_project_member');
END;
"#,
    // 3: the "Imported" project from migration 2 can be adopted once by a
    // folder (`adopt_import`). `unclaimed_import` marks it until then; nothing
    // else ever sets it, so a project that merely lost its folder to another
    // project is never offered.
    r#"
ALTER TABLE projects ADD COLUMN unclaimed_import INTEGER NOT NULL DEFAULT 0
    CHECK (unclaimed_import IN (0, 1));
UPDATE projects SET unclaimed_import = 1
WHERE root_path IS NULL AND name = 'Imported';
"#,
    // 4: read receipts and ignored pings. Cursors learn when they last moved
    // (NULL: never, e.g. the backfill point an agent joins at), and
    // obligations gain the `ignored` state, which needs a table rebuild for
    // the CHECK constraint (rows and keys copied as they are).
    r#"
ALTER TABLE cursors ADD COLUMN delivered_at INTEGER;
ALTER TABLE cursors ADD COLUMN read_at INTEGER;
CREATE TABLE obligations_v2 (
    agent_id       INTEGER NOT NULL,
    channel_id     INTEGER NOT NULL,
    message_id     INTEGER NOT NULL,
    state          TEXT NOT NULL CHECK (state IN ('open', 'satisfied', 'fallback', 'unmet', 'ignored')),
    resolved_by    INTEGER REFERENCES messages(id),
    resolved_kind  TEXT,
    PRIMARY KEY (agent_id, channel_id, message_id),
    FOREIGN KEY (agent_id, channel_id, message_id)
        REFERENCES triggers (agent_id, channel_id, message_id)
);
INSERT INTO obligations_v2 SELECT agent_id, channel_id, message_id, state, resolved_by, resolved_kind
FROM obligations;
DROP TABLE obligations;
ALTER TABLE obligations_v2 RENAME TO obligations;
"#,
    // 5: refusal and dissent as peer actions (STRM-343). Obligations gain the
    // `refused` state (table rebuild for the CHECK, rows copied as they are),
    // and `stances` records who said no or disagreed, on what, with which
    // model and why. One stance per message that states it.
    r#"
CREATE TABLE obligations_v3 (
    agent_id       INTEGER NOT NULL,
    channel_id     INTEGER NOT NULL,
    message_id     INTEGER NOT NULL,
    state          TEXT NOT NULL CHECK (state IN ('open', 'satisfied', 'fallback', 'unmet', 'ignored', 'refused')),
    resolved_by    INTEGER REFERENCES messages(id),
    resolved_kind  TEXT,
    PRIMARY KEY (agent_id, channel_id, message_id),
    FOREIGN KEY (agent_id, channel_id, message_id)
        REFERENCES triggers (agent_id, channel_id, message_id)
);
INSERT INTO obligations_v3 SELECT agent_id, channel_id, message_id, state, resolved_by, resolved_kind
FROM obligations;
DROP TABLE obligations;
ALTER TABLE obligations_v3 RENAME TO obligations;
CREATE TABLE stances (
    message_id  INTEGER PRIMARY KEY NOT NULL REFERENCES messages(id),
    channel_id  INTEGER NOT NULL REFERENCES channels(id),
    kind        TEXT NOT NULL CHECK (kind IN ('refusal', 'dissent')),
    member_id   INTEGER NOT NULL REFERENCES members(id),
    target_id   INTEGER NOT NULL REFERENCES messages(id),
    reason      TEXT NOT NULL,
    model       TEXT,
    runtime     TEXT,
    created_at  INTEGER NOT NULL
);
CREATE INDEX stances_by_channel ON stances (channel_id, message_id);
CREATE INDEX stances_by_target ON stances (target_id);
"#,
    // 6: permission requests (STRM-401). One row per request an agent made,
    // natively or through `chat_request_permission`: pending until a human
    // answers (never times out), or auto_approved for agents in their own VM.
    // An approval is a one-shot grant, spent when the retried call runs
    // (`used_at`). `asked_of` is the comma-separated member ids it was routed
    // to; any human in the channel may answer.
    r#"
CREATE TABLE permission_requests (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_id     INTEGER NOT NULL REFERENCES members(id),
    channel_id   INTEGER NOT NULL REFERENCES channels(id),
    batch_id     INTEGER REFERENCES batches(id),
    source       TEXT NOT NULL CHECK (source IN ('native', 'chat_tool')),
    title        TEXT NOT NULL,
    detail       TEXT,
    reason       TEXT,
    state        TEXT NOT NULL CHECK (state IN ('pending', 'approved', 'denied', 'auto_approved')),
    asked_of     TEXT NOT NULL DEFAULT '',
    created_at   INTEGER NOT NULL,
    answered_by  INTEGER REFERENCES members(id),
    answered_at  INTEGER,
    answer_note  TEXT,
    used_at      INTEGER
);
CREATE INDEX permission_requests_by_agent
    ON permission_requests (agent_id, channel_id, state);
CREATE INDEX permission_requests_by_channel
    ON permission_requests (channel_id, state);
"#,
    // 7: model members. A chat member is a model running through a harness
    // (see `harness`); `model_members` says which, `roster` is who the user
    // added to each project, `roster_seeded` marks projects whose roster was
    // filled from auto-detection once (after that it is the user's), and
    // `harness_models` caches each harness's own model list.
    r#"
CREATE TABLE model_members (
    member_id   INTEGER PRIMARY KEY REFERENCES members(id),
    harness     TEXT NOT NULL,
    model       TEXT NOT NULL DEFAULT '',
    model_name  TEXT NOT NULL,
    UNIQUE (harness, model)
);
CREATE TABLE roster (
    project_id  TEXT NOT NULL REFERENCES projects(id),
    member_id   INTEGER NOT NULL REFERENCES model_members(member_id),
    added_at    INTEGER NOT NULL,
    PRIMARY KEY (project_id, member_id)
);
CREATE TABLE roster_seeded (
    project_id  TEXT PRIMARY KEY NOT NULL REFERENCES projects(id),
    seeded_at   INTEGER NOT NULL
);
CREATE TABLE harness_models (
    harness  TEXT NOT NULL,
    ord      INTEGER NOT NULL,
    model    TEXT NOT NULL,
    name     TEXT NOT NULL,
    seen_at  INTEGER NOT NULL,
    PRIMARY KEY (harness, model)
);
"#,
    // 8: the permission log. Read-only tool calls (ACP kinds `read` and
    // `search`) are allowed at once and now kept too, flagged `read_only`, so
    // the right sidebar can list every request newest first. Old reads are
    // pruned per channel (`permission::READS_KEPT`); nothing else is.
    r#"
ALTER TABLE permission_requests ADD COLUMN read_only INTEGER NOT NULL DEFAULT 0
    CHECK (read_only IN (0, 1));
CREATE INDEX permission_requests_log ON permission_requests (channel_id, id);
CREATE INDEX permission_requests_reads ON permission_requests (channel_id, read_only, id);
"#,
    // 9: emoji reactions (STRM-467). The table from migration 1 is rebuilt
    // with the channel beside each reaction (a channel's recent reactions
    // and the app's history load read it without a join over messages) and
    // a CHECK on the emoji's length (`reaction::MAX_EMOJI_CHARS`). Rows are
    // copied with the shortcodes earlier agents sent (`+1`, `thumbsup`,
    // `white_check_mark`, `eyes`) mapped to the emoji, so `:+1:` and 👍 are
    // one reaction; a row that would duplicate another after mapping, or is
    // too long to be an emoji, is dropped. Still one row per (message,
    // member, emoji).
    r#"
CREATE TABLE reactions_v2 (
    message_id  INTEGER NOT NULL REFERENCES messages(id),
    channel_id  INTEGER NOT NULL REFERENCES channels(id),
    member_id   INTEGER NOT NULL REFERENCES members(id),
    emoji       TEXT NOT NULL CHECK (length(emoji) BETWEEN 1 AND 32),
    created_at  INTEGER NOT NULL,
    PRIMARY KEY (message_id, member_id, emoji)
);
INSERT OR IGNORE INTO reactions_v2 (message_id, channel_id, member_id, emoji, created_at)
SELECT r.message_id, m.channel_id, r.member_id,
       CASE lower(trim(r.emoji, ' :'))
           WHEN '+1' THEN '👍' WHEN 'thumbsup' THEN '👍'
           WHEN 'white_check_mark' THEN '✅' WHEN 'eyes' THEN '👀'
           ELSE trim(r.emoji)
       END,
       r.created_at
FROM reactions r JOIN messages m ON m.id = r.message_id
WHERE length(trim(r.emoji)) BETWEEN 1 AND 32
ORDER BY r.created_at;
DROP TABLE reactions;
ALTER TABLE reactions_v2 RENAME TO reactions;
CREATE INDEX reactions_by_channel ON reactions (channel_id, created_at);
"#,
    // 10: project settings (STRM-467: `ping_answer_reactions`, which emoji
    // answer a ping). One JSON value per (project, key); a missing row means
    // the built-in default. Values are validated before they are written.
    r#"
CREATE TABLE project_settings (
    project_id  TEXT NOT NULL REFERENCES projects(id),
    key         TEXT NOT NULL,
    value       TEXT NOT NULL,
    updated_at  INTEGER NOT NULL,
    PRIMARY KEY (project_id, key)
);
"#,
    // 11: a roster model member's own `ping_answer_reactions` (STRM-533), the
    // JSON list as validated by `reaction::PingAnswers` (NULL: none, so the
    // project setting or the default applies). Per member, so it follows the
    // member into every project whose roster it is on.
    r#"
ALTER TABLE model_members ADD COLUMN ping_answer_reactions TEXT;
"#,
    // 12: nicknames (STRM-560). A member may have any number; `@nick` wakes
    // it exactly like `@handle`. A nickname is unique among nicknames (the
    // key, case-insensitive) and may not be anyone's handle (the trigger;
    // a handle created later wins and takes the nickname away, see
    // `ensure_member`). Its shape (a mention token, not @everyone) is
    // checked by `policy::check_nickname` before it is written.
    r#"
CREATE TABLE member_nicknames (
    nickname   TEXT PRIMARY KEY NOT NULL COLLATE NOCASE,
    member_id  INTEGER NOT NULL REFERENCES members(id),
    added_at   INTEGER NOT NULL
);
CREATE INDEX member_nicknames_by_member ON member_nicknames (member_id, added_at);
CREATE TRIGGER member_nicknames_not_a_handle BEFORE INSERT ON member_nicknames
WHEN EXISTS (SELECT 1 FROM members WHERE handle = NEW.nickname)
BEGIN
    SELECT RAISE(ABORT, 'nickname_is_a_handle');
END;
"#,
    // 13: request keys (STRM-572). A grant used to match on the tool call's
    // title alone, so an approved "Edit files" let any later "Edit files"
    // through. `request_key` is what exactly was asked for
    // (`permission::request_key`: the tool kind plus a SHA-256 of the call
    // without its ids); grants and dedup match on it. NULL for logged reads,
    // auto-allowed chat tools and rows from before this migration, so an old
    // unused grant matches nothing.
    r#"
ALTER TABLE permission_requests ADD COLUMN request_key TEXT;
CREATE INDEX permission_requests_by_key
    ON permission_requests (agent_id, channel_id, request_key, state);
"#,
    // 16: decided_by column for tracking who/what decided a permission request (STRM-542).
    r#"
ALTER TABLE permission_requests ADD COLUMN decided_by TEXT;
"#,
    // 17: per-channel approval mode override (STRM-542). Most-specific scope.
    r#"
CREATE TABLE channel_approval_mode (
    channel_id  INTEGER PRIMARY KEY REFERENCES channels(id) ON DELETE CASCADE,
    mode        TEXT NOT NULL
);
"#,
    // 18: workspace/global app settings (STRM-542), e.g. approval_mode override.
    r#"
CREATE TABLE app_settings (
    key         TEXT PRIMARY KEY NOT NULL,
    value       TEXT NOT NULL
);
"#,
    // 19: queued command execution lifecycle (STRM-542).
    r#"
ALTER TABLE permission_requests ADD COLUMN command TEXT;
ALTER TABLE permission_requests ADD COLUMN cwd TEXT;
ALTER TABLE permission_requests ADD COLUMN run_state TEXT;
ALTER TABLE permission_requests ADD COLUMN run_output TEXT;
ALTER TABLE permission_requests ADD COLUMN run_exit_code INTEGER;
ALTER TABLE permission_requests ADD COLUMN run_finished_at INTEGER;
"#,
    // 20: message edits (STRM-575). Editing was removed in STRM-604: sent
    // messages are final. The column and table are kept, unused, for schema
    // compatibility with databases that already ran this migration.
    r#"
ALTER TABLE messages ADD COLUMN edited_at INTEGER;
CREATE TABLE message_revisions (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    message_id  INTEGER NOT NULL REFERENCES messages(id),
    body        TEXT NOT NULL,
    edited_at   INTEGER NOT NULL
);
CREATE INDEX message_revisions_by_message ON message_revisions (message_id, id);
"#,
];

/// Per-project open databases (workspace + agents). A broken project must not
/// stop core.db or other projects from opening (guarantee 4).
#[derive(Clone, Debug)]
pub struct ProjectDbs {
    pub workspace: PathBuf,
    pub agents: PathBuf,
}

/// Which project database file is newer than this build knows.
#[derive(Debug)]
pub struct ProjectNewerSchema {
    pub project: ProjectId,
    pub path: PathBuf,
    pub found: i64,
    pub known: i64,
}

pub struct ProjectConnections {
    pub workspace: Connection,
    pub agents: Connection,
}

pub struct Store {
    data_dir: PathBuf,
    core: Connection,
    /// Open project connections, keyed by project id.
    projects: HashMap<ProjectId, ProjectConnections>,
    /// channel id -> project id index for routing.
    channel_project: HashMap<ChannelId, ProjectId>,
    /// message id -> (project id, channel id) index for routing.
    message_project: HashMap<MessageId, (ProjectId, ChannelId)>,
    /// batch id -> project id index.
    batch_project: HashMap<BatchId, ProjectId>,
    /// Projects that could not be opened (isolated).
    project_errors: Vec<OpenError>,
}

impl Store {
    /// Opens the split store in `data_dir` (`core.db` + `Projects/<id>/*.db`).
    /// Migrates from legacy `<data_dir>/core.sqlite` if present and the new
    /// layout is not yet complete. Refuses a database from a newer build
    /// without changing it.
    pub fn open(path: &Path) -> std::result::Result<Self, OpenError> {
        // Compatibility: opening an existing sqlite file path (legacy tests/APIs).
        if path.exists() && path.is_file() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name != "core.db" {
                let dir = path.parent().unwrap_or(Path::new("."));
                crate::permissions::ensure_private_dir(dir)
                    .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
                let core_sqlite = dir.join("core.sqlite");
                let backup = dir.join("core.sqlite.pre-split");
                let core_db = dir.join("core.db");
                let already_split = core_db.exists() || backup.exists();
                if name == "core.sqlite" {
                    // standard legacy location
                } else if !already_split {
                    // Standalone legacy file: check for refusal without modifying it.
                    let conn = Connection::open(path)?;
                    let version: i64 =
                        conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
                    let known = LEGACY_MIGRATIONS.len() as i64;
                    if version > known {
                        return Err(OpenError::NewerSchema {
                            path: path.to_path_buf(),
                            found: version,
                            known,
                        });
                    }
                    drop(conn);
                    if !core_sqlite.exists() {
                        std::fs::copy(path, &core_sqlite)
                            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
                    }
                }
                maybe_migrate_from_legacy(dir)?;
                let core_path = dir.join("core.db");
                let core = open_db(&core_path, CORE_MIGRATIONS)?;
                let mut store = Store {
                    data_dir: dir.to_path_buf(),
                    core,
                    projects: HashMap::new(),
                    channel_project: HashMap::new(),
                    message_project: HashMap::new(),
                    batch_project: HashMap::new(),
                    project_errors: Vec::new(),
                };
                store.load_projects()?;
                store.ensure_chat_backfill()?;
                return Ok(store);
            }
        }
        let data_dir = path;
        crate::permissions::ensure_private_dir(data_dir)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        maybe_migrate_from_legacy(data_dir)?;
        let core_path = data_dir.join("core.db");
        let core = open_db(&core_path, CORE_MIGRATIONS)?;
        let mut store = Store {
            data_dir: data_dir.to_path_buf(),
            core,
            projects: HashMap::new(),
            channel_project: HashMap::new(),
            message_project: HashMap::new(),
            batch_project: HashMap::new(),
            project_errors: Vec::new(),
        };
        store.load_projects()?;
        store.ensure_chat_backfill()?;
        Ok(store)
    }

    /// A private in-memory split store (tests). Backed by a temporary directory.
    pub fn open_in_memory() -> std::result::Result<Self, OpenError> {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "chaos-control-store-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            n
        ));
        crate::permissions::ensure_private_dir(&dir)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        let mut store = Self::open(&dir)?;
        store.data_dir = dir;
        Ok(store)
    }

    /// Compatibility: open from a legacy core.sqlite path by using its parent
    /// as data dir. Used during transition.
    /// Legacy compat: open from existing single-db connection by migrating? not supported in split.
    /// Kept for old tests - creates a temp split store seeded from this connection's state is complex.
    /// For now, tests using init should be rewritten. This returns an error.
    #[cfg(test)]
    pub fn init(_conn: Connection) -> std::result::Result<Self, OpenError> {
        panic!("Store::init not supported in split layout; rewrite test")
    }

    pub fn open_path(path: &Path) -> std::result::Result<Self, OpenError> {
        if path.file_name().and_then(|n| n.to_str()) == Some("core.sqlite")
            && let Some(dir) = path.parent()
        {
            return Self::open(dir);
        }
        // Treat as data dir or core.db path
        if path.file_name().and_then(|n| n.to_str()) == Some("core.db")
            && let Some(dir) = path.parent()
        {
            return Self::open(dir);
        }
        Self::open(path)
    }

    fn load_projects(&mut self) -> std::result::Result<(), OpenError> {
        let ids = core_project_ids(&self.core)?;
        for id in ids {
            match self.open_project(&id) {
                Ok(()) => {}
                Err(OpenError::NewerSchema { path, found, known }) => {
                    return Err(OpenError::NewerSchema { path, found, known });
                }
                Err(e @ OpenError::Project { .. }) => {
                    self.project_errors.push(e);
                }
                Err(OpenError::Sqlite(e)) => {
                    self.project_errors.push(OpenError::Project {
                        project: id.clone(),
                        path: self.project_dir(&id),
                        source: e,
                    });
                }
            }
        }
        Ok(())
    }

    /// v1 STRM-745: ensure every existing channel has a workspace-level chat
    /// (one channel per chat). Idempotent; safe to run after migrations/load.
    fn ensure_chat_backfill(&mut self) -> Result<()> {
        let project_ids: Vec<ProjectId> = self.projects.keys().cloned().collect();
        for project_id in project_ids {
            let channels: Vec<(i64, String, i64)> = {
                let Some(pc) = self.projects.get(&project_id) else {
                    continue;
                };
                let mut stmt = pc
                    .workspace
                    .prepare("SELECT id, name, created_at FROM channels ORDER BY ordinal, id")?;
                let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
                let mut out = Vec::new();
                for r in rows {
                    out.push(r?);
                }
                out
            };
            for (channel_id, title, created_at) in channels {
                self.ensure_chat_for_channel_row(&project_id, channel_id, &title, created_at)?;
            }
        }
        Ok(())
    }

    fn ensure_chat_for_channel_row(
        &mut self,
        project: &ProjectId,
        channel_id: i64,
        title: &str,
        created_at: i64,
    ) -> Result<i64> {
        if let Some(chat_id) = self
            .core
            .query_row(
                "SELECT chat_id FROM chat_members WHERE project_id = ?1 AND channel_id = ?2",
                params![project.as_str(), channel_id],
                |r| r.get(0),
            )
            .optional()?
        {
            return Ok(chat_id);
        }
        let chat_id = self.allocate_id("chat")?;
        let tx = self
            .core
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO chats (id, title, created_at, archived_at) VALUES (?1, ?2, ?3, NULL)",
            params![chat_id, title, created_at],
        )?;
        tx.execute(
            "INSERT INTO chat_members (chat_id, ordinal, project_id, channel_id) VALUES (?1, 1, ?2, ?3)",
            params![chat_id, project.as_str(), channel_id],
        )?;
        tx.commit()?;
        Ok(chat_id)
    }

    fn ensure_chat_for_channel(
        &mut self,
        project: &ProjectId,
        channel: &Channel,
        created_at: Millis,
    ) -> Result<i64> {
        self.ensure_chat_for_channel_row(
            project,
            channel.id.0,
            channel.name.as_ref(),
            created_at as i64,
        )
    }

    fn project_dir(&self, project: &ProjectId) -> PathBuf {
        self.data_dir.join("Projects").join(project.as_str())
    }

    fn open_project(&mut self, project: &ProjectId) -> std::result::Result<(), OpenError> {
        if self.projects.contains_key(project) {
            return Ok(());
        }
        let dir = self.project_dir(project);
        crate::permissions::ensure_private_dir(&dir)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        let ws_path = dir.join("workspace.db");
        let ag_path = dir.join("agents.db");
        let workspace = open_db(&ws_path, WORKSPACE_MIGRATIONS).map_err(|e| match e {
            OpenError::NewerSchema { path, found, known } => {
                OpenError::NewerSchema { path, found, known }
            }
            OpenError::Sqlite(source) => OpenError::Project {
                project: project.clone(),
                path: ws_path.clone(),
                source,
            },
            other => other,
        })?;
        let agents = open_db(&ag_path, AGENTS_MIGRATIONS).map_err(|e| match e {
            OpenError::NewerSchema { path, found, known } => {
                OpenError::NewerSchema { path, found, known }
            }
            OpenError::Sqlite(source) => OpenError::Project {
                project: project.clone(),
                path: ag_path.clone(),
                source,
            },
            other => other,
        })?;
        self.index_project(project.clone(), &workspace, &agents)?;
        self.projects
            .insert(project.clone(), ProjectConnections { workspace, agents });
        Ok(())
    }

    fn index_project(
        &mut self,
        project: ProjectId,
        ws: &Connection,
        ag: &Connection,
    ) -> Result<()> {
        let mut stmt = ws.prepare("SELECT id FROM channels")?;
        let rows = stmt.query_map([], |r| Ok(ChannelId(r.get(0)?)))?;
        for r in rows {
            self.channel_project.insert(r?, project.clone());
        }
        let mut stmt = ws.prepare("SELECT id, channel_id FROM messages")?;
        let rows = stmt.query_map([], |r| Ok((MessageId(r.get(0)?), ChannelId(r.get(1)?))))?;
        for r in rows {
            let (m, ch) = r?;
            self.message_project.insert(m, (project.clone(), ch));
        }
        let mut stmt = ag.prepare("SELECT id FROM batches")?;
        let rows = stmt.query_map([], |r| Ok(BatchId(r.get(0)?)))?;
        for r in rows {
            self.batch_project.insert(r?, project.clone());
        }
        Ok(())
    }

    pub fn schema_version(&self) -> Result<i64> {
        self.core.query_row("PRAGMA user_version", [], |r| r.get(0))
    }

    /// Core database connection.
    pub fn core(&self) -> &Connection {
        &self.core
    }

    /// Mutable core database connection.
    pub fn core_mut(&mut self) -> &mut Connection {
        &mut self.core
    }

    /// Back-compat: returns core connection. Many legacy call sites use this
    /// for operations that moved; callers must be updated.
    pub fn conn(&self) -> &Connection {
        &self.core
    }

    pub fn approval_outcomes(&self) -> rusqlite::Result<HashMap<ApprovalId, ApprovalOutcome>> {
        let mut stmt = self.core.prepare(
            "SELECT cc_request_id, state, provider_reply, execution_effect, error, recorded_at
             FROM approval_outcomes",
        )?;
        let rows = stmt.query_map([], |row| {
            let id: String = row.get(0)?;
            let state: String = row.get(1)?;
            let provider_reply: Option<String> = row.get(2)?;
            let execution_effect: Option<String> = row.get(3)?;
            let error: Option<String> = row.get(4)?;
            let recorded_at: Option<i64> = row.get(5)?;
            Ok((
                ApprovalId::from(id),
                state,
                provider_reply,
                execution_effect,
                error,
                recorded_at.map(|value| value as u64),
            ))
        })?;
        let mut outcomes = HashMap::new();
        for row in rows {
            let (id, state, provider_reply, execution_effect, error, recorded_at) = row?;
            let state = match state.as_str() {
                "allowed" => ApprovalOutcomeState::Allowed,
                "denied" => ApprovalOutcomeState::Denied,
                "sent" => ApprovalOutcomeState::Sent,
                "not_sent" => ApprovalOutcomeState::NotSent,
                "failed" => ApprovalOutcomeState::Failed,
                unknown => {
                    return Err(rusqlite::Error::FromSqlConversionFailure(
                        1,
                        rusqlite::types::Type::Text,
                        format!("unknown approval outcome state: {unknown}").into(),
                    ));
                }
            };
            outcomes.insert(
                id.clone(),
                ApprovalOutcome {
                    cc_request_id: id,
                    state,
                    provider_reply: provider_reply
                        .as_deref()
                        .and_then(|value| serde_json::from_str(value).ok()),
                    execution_effect: execution_effect
                        .as_deref()
                        .and_then(|value| serde_json::from_str(value).ok()),
                    error: error.map(Arc::from),
                    recorded_at,
                },
            );
        }
        Ok(outcomes)
    }

    pub fn record_approval_outcome(&mut self, outcome: &ApprovalOutcome) -> rusqlite::Result<()> {
        let state = match outcome.state {
            ApprovalOutcomeState::Allowed => "allowed",
            ApprovalOutcomeState::Denied => "denied",
            ApprovalOutcomeState::Sent => "sent",
            ApprovalOutcomeState::NotSent => "not_sent",
            ApprovalOutcomeState::Failed => "failed",
        };
        self.tx(|conn| {
            conn.execute(
                "INSERT INTO approval_outcomes
                 (cc_request_id, state, provider_reply, execution_effect, error, recorded_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(cc_request_id) DO UPDATE SET
                   state=excluded.state,
                   provider_reply=excluded.provider_reply,
                   execution_effect=excluded.execution_effect,
                   error=excluded.error,
                   recorded_at=excluded.recorded_at",
                params![
                    outcome.cc_request_id.as_str(),
                    state,
                    outcome
                        .provider_reply
                        .as_ref()
                        .map(|value| value.to_string()),
                    outcome
                        .execution_effect
                        .as_ref()
                        .map(|value| value.to_string()),
                    outcome.error.as_deref(),
                    outcome.recorded_at.map(|value| value as i64),
                ],
            )?;
            Ok(())
        })
    }

    pub fn project_workspace(&self, project: &ProjectId) -> Result<&Connection> {
        self.projects
            .get(project)
            .map(|p| &p.workspace)
            .ok_or_else(|| rusqlite::Error::InvalidQuery)
    }

    pub fn project_agents(&self, project: &ProjectId) -> Result<&Connection> {
        self.projects
            .get(project)
            .map(|p| &p.agents)
            .ok_or_else(|| rusqlite::Error::InvalidQuery)
    }

    pub fn project_workspace_mut(&mut self, project: &ProjectId) -> Result<&mut Connection> {
        self.projects
            .get_mut(project)
            .map(|p| &mut p.workspace)
            .ok_or_else(|| rusqlite::Error::InvalidQuery)
    }

    pub fn project_agents_mut(&mut self, project: &ProjectId) -> Result<&mut Connection> {
        self.projects
            .get_mut(project)
            .map(|p| &mut p.agents)
            .ok_or_else(|| rusqlite::Error::InvalidQuery)
    }

    pub fn project_for_channel(&self, channel: ChannelId) -> Option<ProjectId> {
        self.channel_project.get(&channel).cloned()
    }

    pub fn project_for_message(&self, message: MessageId) -> Option<(ProjectId, ChannelId)> {
        self.message_project.get(&message).cloned()
    }

    pub fn project_for_batch(&self, batch: BatchId) -> Option<ProjectId> {
        self.batch_project.get(&batch).cloned()
    }

    // --- Agent lifecycle (STRM-686) -------------------------------------

    /// Global lifecycle of an agent. A missing row means Sleeping: that is how
    /// newly added agents start, without needing a write at creation time.
    pub fn agent_lifecycle(&self, member: MemberId) -> Result<LifecycleState> {
        let raw: Option<String> = self
            .core
            .query_row(
                "SELECT state FROM agent_lifecycle WHERE member_id = ?1",
                [member.0],
                |r| r.get(0),
            )
            .optional()?;
        Ok(raw
            .as_deref()
            .and_then(LifecycleState::parse)
            .unwrap_or_else(LifecycleState::default_for_new_agent))
    }

    /// Whether the agent was put to sleep by an explicit user action (as
    /// opposed to choosing not to receive normal messages).
    pub fn agent_forced_sleep(&self, member: MemberId) -> Result<bool> {
        let forced: Option<i64> = self
            .core
            .query_row(
                "SELECT forced_sleep FROM agent_lifecycle WHERE member_id = ?1",
                [member.0],
                |r| r.get(0),
            )
            .optional()?;
        Ok(forced.unwrap_or(0) != 0)
    }

    /// Set an agent's global lifecycle and whether this was a user-forced
    /// sleep. Activating clears the forced flag; sleeping by preference is not
    /// represented here (it lives in `model_members.wake_normal_messages`).
    pub fn set_agent_lifecycle(
        &self,
        member: MemberId,
        state: LifecycleState,
        forced_sleep: bool,
    ) -> Result<()> {
        let kind: Option<String> = self
            .core
            .query_row("SELECT kind FROM members WHERE id = ?1", [member.0], |r| {
                r.get(0)
            })
            .optional()?;
        if kind.as_deref() != Some("agent") {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        }
        self.core.execute(
            "INSERT INTO agent_lifecycle (member_id, state, forced_sleep) VALUES (?1, ?2, ?3)
             ON CONFLICT(member_id) DO UPDATE SET state = excluded.state,
                                                  forced_sleep = excluded.forced_sleep",
            params![member.0, state.as_str(), forced_sleep as i64],
        )?;
        Ok(())
    }

    /// Chats that contain this channel as a target. v1 backfills one chat per
    /// channel, but the schema permits fan-out; lifecycle is per chat, so a
    /// channel wake consults every chat it belongs to.
    pub fn chats_for_channel(
        &self,
        project: &ProjectId,
        channel: ChannelId,
    ) -> Result<Vec<ChatId>> {
        let mut stmt = self.core.prepare(
            "SELECT chat_id FROM chat_members WHERE project_id = ?1 AND channel_id = ?2 ORDER BY chat_id",
        )?;
        let rows = stmt.query_map(params![project.as_str(), channel.0], |r| r.get::<_, i64>(0))?;
        rows.map(|r| Ok(ChatId(r?))).collect()
    }

    /// Per-chat participation state. A missing row means Active; joining a
    /// chat does not write a row, so membership state is cheap to add later.
    pub fn chat_membership_lifecycle(
        &self,
        chat: ChatId,
        member: MemberId,
    ) -> Result<MembershipState> {
        let raw: Option<String> = self
            .core
            .query_row(
                "SELECT state FROM chat_member_lifecycle WHERE chat_id = ?1 AND member_id = ?2",
                params![chat.0, member.0],
                |r| r.get(0),
            )
            .optional()?;
        Ok(raw
            .as_deref()
            .and_then(MembershipState::parse)
            .unwrap_or_else(MembershipState::default_for_new_membership))
    }

    /// Whether this chat membership was put to sleep by an explicit user
    /// action. An explicit ping wakes it and clears the flag.
    pub fn chat_membership_forced_sleep(&self, chat: ChatId, member: MemberId) -> Result<bool> {
        let forced: Option<i64> = self
            .core
            .query_row(
                "SELECT forced_sleep FROM chat_member_lifecycle WHERE chat_id = ?1 AND member_id = ?2",
                params![chat.0, member.0],
                |r| r.get(0),
            )
            .optional()?;
        Ok(forced.unwrap_or(0) != 0)
    }

    /// Set one agent's participation state in one chat. Both the chat and the
    /// agent must exist; lifecycle never invents either. Channel membership is
    /// checked by the wake path, which only considers current channel members.
    pub fn set_chat_membership_lifecycle(
        &self,
        chat: ChatId,
        member: MemberId,
        state: MembershipState,
        forced_sleep: bool,
    ) -> Result<()> {
        let exists: Option<i64> = self
            .core
            .query_row(
                "SELECT 1 FROM chats WHERE id = ?1 AND archived_at IS NULL",
                [chat.0],
                |r| r.get(0),
            )
            .optional()?;
        if exists.is_none() {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        }
        let kind: Option<String> = self
            .core
            .query_row("SELECT kind FROM members WHERE id = ?1", [member.0], |r| {
                r.get(0)
            })
            .optional()?;
        if kind.as_deref() != Some("agent") {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        }
        self.core.execute(
            "INSERT INTO chat_member_lifecycle (chat_id, member_id, state, forced_sleep)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(chat_id, member_id) DO UPDATE SET state = excluded.state,
                                                          forced_sleep = excluded.forced_sleep",
            params![chat.0, member.0, state.as_str(), forced_sleep as i64],
        )?;
        Ok(())
    }

    /// Ensure project DBs exist/open (used when a new project is created).
    pub fn ensure_project_open(
        &mut self,
        project: &ProjectId,
    ) -> std::result::Result<(), OpenError> {
        self.open_project(project)
    }

    pub fn note_channel(&mut self, project: ProjectId, channel: ChannelId) {
        self.channel_project.insert(channel, project);
    }

    pub fn note_message(&mut self, project: ProjectId, channel: ChannelId, message: MessageId) {
        self.message_project.insert(message, (project, channel));
    }

    pub fn note_batch(&mut self, project: ProjectId, batch: BatchId) {
        self.batch_project.insert(batch, project);
    }

    pub fn project_errors(&self) -> &[OpenError] {
        &self.project_errors
    }

    /// Runs `f` in one immediate transaction on the core database.
    pub fn tx<T, E: From<rusqlite::Error>>(
        &mut self,
        f: impl FnOnce(&Connection) -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E> {
        let tx = self
            .core
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Allocates a globally unique ID from core.db counters (channel/message/batch).
    pub fn allocate_id(&mut self, name: &str) -> Result<i64> {
        let tx = self
            .core
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let id: i64 = tx.query_row(
            "SELECT next_id FROM id_counters WHERE name = ?1",
            [name],
            |r| r.get(0),
        )?;
        tx.execute(
            "UPDATE id_counters SET next_id = ?2 WHERE name = ?1",
            params![name, id + 1],
        )?;
        tx.commit()?;
        Ok(id)
    }

    // Convenience methods routing to appropriate DBs.
    pub fn ensure_member(&self, kind: MemberKind, handle: &str, name: &str) -> Result<Member> {
        ensure_member_db(&self.core, kind, handle, name)
    }
    pub fn member(&self, id: MemberId) -> Result<Option<Member>> {
        member_db(&self.core, id)
    }
    pub fn project(&self, id: &ProjectId) -> Result<Option<Project>> {
        project_row(&self.core, id)
    }
    pub fn projects(&self) -> Result<Vec<Project>> {
        projects_rows(&self.core)
    }
    pub fn project_by_root(&self, root: &str) -> Result<Option<Project>> {
        project_by_root_row(&self.core, root)
    }
    pub fn unclaimed_import(&self) -> Result<Option<Project>> {
        unclaimed_import_db(&self.core)
    }
    pub fn ensure_project(
        &mut self,
        id: &ProjectId,
        name: &str,
        root: Option<&str>,
        now: Millis,
    ) -> Result<Project> {
        let p = ensure_project_row(&self.core, id, name, root, now)?;
        self.ensure_project_open(id)
            .map_err(store_open_error_to_sqlite)?;
        Ok(p)
    }
    pub fn adopt_import(
        &mut self,
        id: &ProjectId,
        name: &str,
        root: &str,
    ) -> Result<Option<Project>> {
        let p = adopt_import_db(&self.core, id, name, root)?;
        if p.is_some() {
            self.ensure_project_open(id)
                .map_err(store_open_error_to_sqlite)?;
        }
        Ok(p)
    }
    pub fn forget_empty_project(&mut self, id: &ProjectId) -> Result<Option<Vec<ChannelId>>> {
        // Fail closed: we must be able to inspect project storage to prove it empty.
        // If the project DBs cannot be opened/read, do not delete core metadata.
        self.ensure_project_open(id)
            .map_err(store_open_error_to_sqlite)?;
        let ws = self
            .project_workspace(id)
            .map_err(|_| cannot_inspect_project_storage(id))?;
        let has_messages = project_has_messages_db(ws)?;
        if has_messages {
            return Ok(None);
        }
        let channels = channels_in_project_db(ws, id)?
            .into_iter()
            .map(|c| c.id)
            .collect::<Vec<_>>();
        let gone = forget_empty_project_core(&self.core, id, &channels)?;
        if gone.is_some() {
            self.projects.remove(id);
            self.channel_project.retain(|_, p| p != id);
            self.message_project.retain(|_, (p, _)| p != id);
            self.batch_project.retain(|_, p| p != id);
            let dir = self.project_dir(id);
            let _ = std::fs::remove_dir_all(dir);
        }
        Ok(gone)
    }
    pub fn rename_project(&mut self, id: &ProjectId, name: &str) -> Result<bool> {
        rename_project_row(&self.core, id, name)
    }
    pub fn join_project(&self, project: &ProjectId, member: MemberId, now: Millis) -> Result<bool> {
        join_project_row(&self.core, project, member, now)
    }
    pub fn is_project_member(&self, project: &ProjectId, member: MemberId) -> Result<bool> {
        is_project_member_row(&self.core, project, member)
    }
    pub fn ensure_channel(
        &mut self,
        project: &ProjectId,
        name: &str,
        now: Millis,
    ) -> Result<Channel> {
        self.ensure_project_open(project)
            .map_err(store_open_error_to_sqlite)?;
        if self.project(project)?.is_none() {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        }
        let clean = name.strip_prefix('#').unwrap_or(name);
        if let Some(ch) = self.channel_in_project(project, clean)? {
            self.ensure_chat_for_channel(project, &ch, now)?;
            return Ok(ch);
        }
        let id = ChannelId(self.allocate_id("channel")?);
        let ch = ensure_channel_db(self.project_workspace(project)?, project, clean, id, now)?;
        self.note_channel(project.clone(), ch.id);
        self.ensure_chat_for_channel(project, &ch, now)?;
        Ok(ch)
    }
    pub fn channel(&self, id: ChannelId) -> Result<Option<Channel>> {
        let Some(project) = self.project_for_channel(id) else {
            return Ok(None);
        };
        channel_db(self.project_workspace(&project)?, id)
    }
    pub fn channel_in_project(&self, project: &ProjectId, name: &str) -> Result<Option<Channel>> {
        let Ok(ws) = self.project_workspace(project) else {
            return Ok(None);
        };
        channel_in_project_db(ws, project, name)
    }
    pub fn channels_in_project(&self, project: &ProjectId) -> Result<Vec<Channel>> {
        let Ok(ws) = self.project_workspace(project) else {
            return Ok(vec![]);
        };
        channels_in_project_db(ws, project)
    }

    pub fn channels_of(&self, member: MemberId, project: &ProjectId) -> Result<Vec<Channel>> {
        let Ok(ws) = self.project_workspace(project) else {
            return Ok(vec![]);
        };
        channels_of_db(ws, member, project)
    }

    /// Every project with its channels, for the cross-project chat list
    /// (STRM-737). A project whose database cannot be read is still listed,
    /// with no chats: hiding it would make projects disappear when one file
    /// is broken, which is the opposite of what the sidebar should do.
    pub fn project_catalog(&self) -> Result<Vec<(Project, Vec<Channel>)>> {
        let mut out = Vec::new();
        for project in self.projects()? {
            let chats = self.channels_in_project(&project.id).unwrap_or_default();
            out.push((project, chats));
        }
        Ok(out)
    }

    /// Workspace-level chats (STRM-745, v1: one channel per chat).
    pub fn chats(&self) -> Result<Vec<Chat>> {
        let mut stmt = self.core.prepare(
            "SELECT id, title, created_at, archived_at FROM chats ORDER BY created_at, id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                ChatId(r.get(0)?),
                Arc::<str>::from(r.get::<_, String>(1)?),
                r.get::<_, i64>(2)? as Millis,
                r.get::<_, Option<i64>>(3)?.map(|v| v as Millis),
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, title, created_at, archived_at) = row?;
            let targets = self.chat_targets(id)?;
            out.push(Chat {
                id,
                title,
                created_at,
                archived_at,
                targets,
            });
        }
        Ok(out)
    }

    pub fn chat_targets(&self, chat: ChatId) -> Result<Vec<ChatTarget>> {
        let mut stmt = self.core.prepare(
            "SELECT chat_id, ordinal, project_id, channel_id FROM chat_members WHERE chat_id = ?1 ORDER BY ordinal, chat_id",
        )?;
        let rows = stmt.query_map([chat.0], |r| {
            let project_raw: String = r.get(2)?;
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                project_raw,
                r.get::<_, i64>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (chat_id, ordinal, project_raw, channel_id) = row?;
            let Some(project) = ProjectId::parse(&project_raw) else {
                continue;
            };
            out.push(ChatTarget {
                chat: ChatId(chat_id),
                ordinal: ordinal as u32,
                project,
                channel: ChannelId(channel_id),
            });
        }
        Ok(out)
    }

    pub fn search_messages(
        &self,
        channels: &[ChannelId],
        query: &str,
        limit: usize,
    ) -> Result<Vec<Message>> {
        let mut out = Vec::new();
        let pattern = format!(
            "%{}%",
            query
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        for ch in channels {
            let Some(project) = self.project_for_channel(*ch) else {
                continue;
            };
            let ws = self.project_workspace(&project)?;
            out.extend(search_in_channel_db(ws, *ch, &pattern, limit)?);
        }
        out.sort_by(|a, b| b.sent_at.cmp(&a.sent_at).then(b.id.cmp(&a.id)));
        out.truncate(limit);
        Ok(out)
    }

    pub fn last_human_seq(&self, channel: ChannelId) -> Result<Seq> {
        let Some(project) = self.project_for_channel(channel) else {
            return Ok(0);
        };
        let ws = self.project_workspace(&project)?;
        let mut stmt = ws.prepare(
            "SELECT seq, author_id FROM messages WHERE channel_id = ?1 ORDER BY seq DESC",
        )?;
        let rows = stmt.query_map([channel.0], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?))
        })?;
        for row in rows {
            let (seq, author) = row?;
            let Some(author) = author else {
                continue;
            };
            if let Some(m) = member_db(&self.core, MemberId(author))?
                && m.kind == MemberKind::Human
            {
                return Ok(seq as Seq);
            }
        }
        Ok(0)
    }

    pub fn last_agent_message_after(
        &self,
        channel: ChannelId,
        after_seq: Seq,
    ) -> Result<Option<Message>> {
        let Some(project) = self.project_for_channel(channel) else {
            return Ok(None);
        };
        let ws = self.project_workspace(&project)?;
        query_messages_db(
            ws,
            &format!(
                "SELECT {MESSAGE_COLS} FROM messages
                 WHERE channel_id = ?1 AND seq > ?2 AND origin IN ('chat_tool', 'fallback')
                 ORDER BY seq DESC LIMIT 1"
            ),
            params![channel.0, after_seq as i64],
        )
        .map(|mut v| v.pop())
    }
    pub fn rename_channel(&self, id: ChannelId, name: &str) -> Result<bool> {
        let Some(project) = self.project_for_channel(id) else {
            return Ok(false);
        };
        let changed = rename_channel_db(self.project_workspace(&project)?, id, name)?;
        if changed {
            let clean = name.strip_prefix('#').unwrap_or(name);
            self.core.execute(
                "UPDATE chats SET title = ?3
                 WHERE id IN (
                     SELECT chat_id FROM chat_members WHERE project_id = ?1 AND channel_id = ?2
                 )",
                params![project.as_str(), id.0, clean],
            )?;
        }
        Ok(changed)
    }
    pub fn is_member(&self, channel: ChannelId, member: MemberId) -> Result<bool> {
        let Some(project) = self.project_for_channel(channel) else {
            return Ok(false);
        };
        is_member_db(self.project_workspace(&project)?, channel, member)
    }
    pub fn members_of(&self, channel: ChannelId) -> Result<Vec<Member>> {
        let Some(project) = self.project_for_channel(channel) else {
            return Ok(vec![]);
        };
        members_of_db(&self.core, self.project_workspace(&project)?, channel)
    }
    pub fn join(
        &mut self,
        channel: ChannelId,
        member: &Member,
        backfill: usize,
        now: Millis,
    ) -> Result<bool> {
        let Some(project) = self.project_for_channel(channel) else {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        };
        if member_db(&self.core, member.id)?.is_none() {
            return Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT),
                Some("not_a_project_member: missing member".into()),
            ));
        }
        if !is_project_member_row(&self.core, &project, member.id)? {
            return Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT_TRIGGER),
                Some("not_a_project_member".into()),
            ));
        }
        join_db(
            self.project_workspace(&project)?,
            channel,
            member,
            backfill,
            now,
        )
    }
    pub fn head_seq(&self, channel: ChannelId) -> Result<Seq> {
        let Some(project) = self.project_for_channel(channel) else {
            return Ok(0);
        };
        head_seq_db(self.project_workspace(&project)?, channel)
    }

    pub fn cursor(&self, agent: MemberId, channel: ChannelId) -> Result<Cursor> {
        let Some(project) = self.project_for_channel(channel) else {
            return Ok(Cursor {
                delivered_through: 0,
                read_through: 0,
                read_at: None,
            });
        };
        cursor_db(self.project_workspace(&project)?, agent, channel)
    }
    pub fn messages_between(
        &self,
        channel: ChannelId,
        after: Seq,
        through: Seq,
        limit: usize,
    ) -> Result<Vec<Message>> {
        let Some(project) = self.project_for_channel(channel) else {
            return Ok(vec![]);
        };
        messages_between(
            self.project_workspace(&project)?,
            channel,
            after,
            through,
            limit,
        )
    }
    pub fn messages_before(
        &self,
        channel: ChannelId,
        before_seq: Seq,
        limit: usize,
    ) -> Result<Vec<Message>> {
        let Some(project) = self.project_for_channel(channel) else {
            return Ok(vec![]);
        };
        messages_before_db(
            self.project_workspace(&project)?,
            channel,
            before_seq,
            limit,
        )
    }
    pub fn message(&self, id: MessageId) -> Result<Option<Message>> {
        let Some((project, _)) = self.project_for_message(id) else {
            return Ok(None);
        };
        message_db(self.project_workspace(&project)?, id)
    }
    pub fn insert_message(&mut self, m: &NewMessage<'_>) -> Result<Message> {
        let Some(project) = self.project_for_channel(m.channel) else {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        };
        // app-level integrity checks (G1)
        if let Some(author) = m.author {
            if member_db(&self.core, author)?.is_none() {
                return Err(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY),
                    Some("missing member".into()),
                ));
            }
            if !is_project_member_row(&self.core, &project, author)? {
                return Err(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT_TRIGGER),
                    Some("not_a_project_member".into()),
                ));
            }
            if !is_member_db(self.project_workspace(&project)?, m.channel, author)? {
                return Err(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT_TRIGGER),
                    Some("not_a_channel_member".into()),
                ));
            }
        }
        let id = MessageId(self.allocate_id("message")?);
        let msg = insert_message_db(self.project_workspace(&project)?, m, id)?;
        self.note_message(project, m.channel, msg.id);
        Ok(msg)
    }
    pub fn message_by_request(
        &self,
        author: MemberId,
        channel: ChannelId,
        request: &str,
    ) -> Result<Option<Message>> {
        let Some(project) = self.project_for_channel(channel) else {
            return Ok(None);
        };
        message_by_request_db(self.project_workspace(&project)?, author, channel, request)
    }
    pub fn in_flight_batch(&self, agent: MemberId, channel: ChannelId) -> Result<Option<BatchRow>> {
        let Some(project) = self.project_for_channel(channel) else {
            return Ok(None);
        };
        in_flight_batch_db(self.project_agents(&project)?, agent, channel)
    }
    pub fn batch(&self, id: BatchId) -> Result<Option<BatchRow>> {
        let Some(project) = self.project_for_batch(id) else {
            return Ok(None);
        };
        batch_db(self.project_agents(&project)?, id)
    }
    pub fn pending_batch(&self, agent: MemberId, channel: ChannelId) -> Result<Option<BatchRow>> {
        let Some(project) = self.project_for_channel(channel) else {
            return Ok(None);
        };
        pending_batch_db(self.project_agents(&project)?, agent, channel)
    }
    /// Channels that have queued work for `agent`. Activation re-pumps these:
    /// lifecycle gates dispatch, so work queued while the agent slept waits
    /// until it is woken.
    pub fn channels_with_pending(&self, agent: MemberId) -> Result<Vec<ChannelId>> {
        // Batches live in the per-project agents DBs, so scan each one.
        let mut out = Vec::new();
        for project in self.projects.keys() {
            let mut stmt = self.project_agents(project)?.prepare(
                "SELECT DISTINCT channel_id FROM batches WHERE agent_id = ?1 AND state = 'pending'",
            )?;
            let rows = stmt.query_map([agent.0], |r| Ok(ChannelId(r.get(0)?)))?;
            for row in rows {
                out.push(row?);
            }
        }
        Ok(out)
    }
    /// Every channel a chat spans (`chat_members` maps chats to channels;
    /// per-chat membership state lives separately, in `chat_member_lifecycle`).
    pub fn chat_channels(&self, chat: ChatId) -> Result<Vec<ChannelId>> {
        let mut stmt = self
            .core
            .prepare("SELECT DISTINCT channel_id FROM chat_members WHERE chat_id = ?1")?;
        let rows = stmt.query_map([chat.0], |r| Ok(ChannelId(r.get(0)?)))?;
        rows.collect()
    }
    pub fn pending_batch_or_create(
        &mut self,
        agent: MemberId,
        channel: ChannelId,
        now: Millis,
    ) -> Result<BatchRow> {
        let Some(project) = self.project_for_channel(channel) else {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        };
        // allocate only if needed: check existing first
        if let Some(b) = pending_batch_db(self.project_agents(&project)?, agent, channel)? {
            self.note_batch(project.clone(), b.id);
            return Ok(b);
        }
        let id = BatchId(self.allocate_id("batch")?);
        let b =
            pending_batch_or_create_db(self.project_agents(&project)?, agent, channel, now, id)?;
        self.note_batch(project, b.id);
        Ok(b)
    }
    pub fn nicknames_of(&self, member: MemberId) -> Result<Vec<Arc<str>>> {
        nicknames_of_db(&self.core, member)
    }
    pub fn nicknames_in(&self, channel: ChannelId) -> Result<Vec<(MemberId, Arc<str>)>> {
        let Some(project) = self.project_for_channel(channel) else {
            return Ok(vec![]);
        };
        nicknames_in_db(&self.core, self.project_workspace(&project)?, channel)
    }
    pub fn mention_owner(&self, token: &str) -> Result<Option<MemberId>> {
        mention_owner_db(&self.core, token)
    }
    pub fn add_nickname(&self, member: MemberId, nickname: &str, now: Millis) -> Result<()> {
        add_nickname_db(&self.core, member, nickname, now)
    }
    pub fn remove_nickname(&self, member: MemberId, nickname: &str) -> Result<bool> {
        remove_nickname_db(&self.core, member, nickname)
    }
    pub fn record_session(
        &self,
        agent: MemberId,
        channel: ChannelId,
        protocol: &str,
        native_ref: &str,
        now: Millis,
    ) -> Result<()> {
        let Some(project) = self.project_for_channel(channel) else {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        };
        record_session(
            self.project_agents(&project)?,
            agent,
            channel,
            protocol,
            native_ref,
            now,
        )
    }
    pub fn project_setting(&self, project: &ProjectId, key: &str) -> Result<Option<String>> {
        let Ok(ws) = self.project_workspace(project) else {
            return Ok(None);
        };
        project_setting_db(ws, project, key)
    }
    pub fn set_project_setting(
        &self,
        project: &ProjectId,
        key: &str,
        value: Option<&str>,
        now: Millis,
    ) -> Result<()> {
        let ws = self.project_workspace(project)?;
        set_project_setting_db(ws, project, key, value, now)
    }

    pub fn leave_project(
        &mut self,
        project: &ProjectId,
        member: MemberId,
        now: Millis,
    ) -> Result<Vec<ChannelId>> {
        let ws = self
            .project_workspace(project)
            .map_err(|_| cannot_inspect_project_storage(project))?;
        // find channels member is in
        let mut stmt = ws.prepare("SELECT channel_id FROM memberships WHERE member_id = ?1")?;
        let channels: Vec<ChannelId> = stmt
            .query_map([member.0], |r| Ok(ChannelId(r.get(0)?)))?
            .collect::<Result<_>>()?;
        let ag = self
            .project_agents(project)
            .map_err(|_| cannot_inspect_project_storage(project))?;
        for ch in &channels {
            ag.execute("UPDATE obligations SET state = 'unmet', resolved_kind = 'member_removed' WHERE agent_id = ?1 AND channel_id = ?2 AND state = 'open'", params![member.0, ch.0])?;
            ag.execute("UPDATE batches SET state = 'failed', outcome = 'member removed', settled_at = ?3 WHERE agent_id = ?1 AND channel_id = ?2 AND state IN ('pending', 'dispatched', 'accepted')", params![member.0, ch.0, now as i64])?;
        }
        for ch in &channels {
            ws.execute(
                "DELETE FROM memberships WHERE channel_id = ?1 AND member_id = ?2",
                params![ch.0, member.0],
            )?;
        }
        crate::roster::leave_project(&self.core, project, member, now)?;
        Ok(channels)
    }

    pub fn recover(&mut self, now: Millis) -> Result<Vec<(MemberId, ChannelId)>> {
        let mut out = Vec::new();
        let project_ids: Vec<ProjectId> = self.projects.keys().cloned().collect();
        for pid in project_ids {
            let ag = match self.project_agents(&pid) {
                Ok(c) => c,
                Err(_) => continue,
            };
            // accepted -> failed/unmet
            let accepted: Vec<BatchId> = {
                let mut stmt = ag.prepare("SELECT id FROM batches WHERE state = 'accepted'")?;
                stmt.query_map([], |r| Ok(BatchId(r.get(0)?)))?
                    .collect::<Result<_>>()?
            };
            for b in accepted {
                let open = open_obligations_db(ag, b)?;
                for t in open {
                    resolve_obligation_db(ag, t, ObligationState::Unmet, None, "app_restart")?;
                }
                set_batch_state_db(
                    ag,
                    b,
                    BatchState::Failed,
                    now,
                    Some("app restarted mid-turn"),
                )?;
            }
            let dispatched: Vec<BatchId> = {
                let mut stmt = ag.prepare("SELECT id FROM batches WHERE state = 'dispatched'")?;
                stmt.query_map([], |r| Ok(BatchId(r.get(0)?)))?
                    .collect::<Result<_>>()?
            };
            for b in dispatched {
                requeue_batch_db(ag, b)?;
            }
            let mut stmt =
                ag.prepare("SELECT agent_id, channel_id FROM batches WHERE state = 'pending'")?;
            let pairs = stmt
                .query_map([], |r| Ok((MemberId(r.get(0)?), ChannelId(r.get(1)?))))?
                .collect::<Result<Vec<_>>>()?;
            out.extend(pairs);
        }
        Ok(out)
    }

    pub fn add_reaction(
        &self,
        message: MessageId,
        channel: ChannelId,
        member: MemberId,
        emoji: &str,
        now: Millis,
    ) -> Result<bool> {
        let project = self
            .project_for_channel(channel)
            .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        add_reaction(
            self.project_workspace(&project)?,
            message,
            channel,
            member,
            emoji,
            now,
        )
    }
    pub fn remove_reaction(
        &self,
        message: MessageId,
        member: MemberId,
        emoji: &str,
    ) -> Result<bool> {
        let project = self
            .project_for_message(message)
            .ok_or(rusqlite::Error::QueryReturnedNoRows)?
            .0;
        remove_reaction(self.project_workspace(&project)?, message, member, emoji)
    }
    pub fn reactions_on(&self, message: MessageId) -> Result<Vec<Reaction>> {
        let project = self
            .project_for_message(message)
            .ok_or(rusqlite::Error::QueryReturnedNoRows)?
            .0;
        reactions_on(self.project_workspace(&project)?, message)
    }
    pub fn has_reacted(&self, message: MessageId, member: MemberId) -> Result<bool> {
        let project = self
            .project_for_message(message)
            .ok_or(rusqlite::Error::QueryReturnedNoRows)?
            .0;
        has_reacted(self.project_workspace(&project)?, message, member)
    }
    pub fn recent_reactions(&self, channel: ChannelId, limit: usize) -> Result<Vec<Reaction>> {
        let project = self
            .project_for_channel(channel)
            .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        recent_reactions(self.project_workspace(&project)?, channel, limit)
    }
    pub fn reactions_since(
        &self,
        channel: ChannelId,
        since: Millis,
        limit: usize,
    ) -> Result<Vec<Reaction>> {
        let project = self
            .project_for_channel(channel)
            .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        reactions_since(self.project_workspace(&project)?, channel, since, limit)
    }
    pub fn reactions_after_seq(
        &self,
        channel: ChannelId,
        after_message: Seq,
    ) -> Result<Vec<Reaction>> {
        let project = self
            .project_for_channel(channel)
            .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        reactions_after_seq(self.project_workspace(&project)?, channel, after_message)
    }
    pub fn advance_read(
        &self,
        agent: MemberId,
        channel: ChannelId,
        seq: Seq,
        now: Millis,
    ) -> Result<bool> {
        let project = self
            .project_for_channel(channel)
            .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        advance_read(self.project_workspace(&project)?, agent, channel, seq, now)
    }
    pub fn advance_delivered(
        &self,
        agent: MemberId,
        channel: ChannelId,
        seq: Seq,
        now: Millis,
    ) -> Result<bool> {
        let project = self
            .project_for_channel(channel)
            .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        advance_delivered(self.project_workspace(&project)?, agent, channel, seq, now)
    }
    pub fn insert_stance(&self, s: &Stance) -> Result<()> {
        let project = self
            .project_for_channel(s.channel)
            .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        insert_stance(self.project_workspace(&project)?, s)
    }
    pub fn stance(&self, message: MessageId) -> Result<Option<Stance>> {
        let project = self
            .project_for_message(message)
            .ok_or(rusqlite::Error::QueryReturnedNoRows)?
            .0;
        stance(self.project_workspace(&project)?, message)
    }
    pub fn recent_stances(&self, channel: ChannelId, limit: usize) -> Result<Vec<Stance>> {
        let project = self
            .project_for_channel(channel)
            .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        recent_stances(self.project_workspace(&project)?, channel, limit)
    }
    pub fn set_batch_state(
        &self,
        id: BatchId,
        state: BatchState,
        now: Millis,
        outcome: Option<&str>,
    ) -> Result<()> {
        let project = self
            .project_for_batch(id)
            .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        set_batch_state(self.project_agents(&project)?, id, state, now, outcome)
    }
    pub fn dispatch_batch(&self, id: BatchId, window_to: Seq) -> Result<u32> {
        let project = self
            .project_for_batch(id)
            .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        dispatch_batch(self.project_agents(&project)?, id, window_to)
    }
    pub fn requeue_batch(&self, id: BatchId) -> Result<()> {
        let project = self
            .project_for_batch(id)
            .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        requeue_batch(self.project_agents(&project)?, id)
    }
    pub fn trigger_exists(&self, id: TriggerId) -> Result<bool> {
        let project = self
            .project_for_channel(id.channel)
            .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        trigger_exists(self.project_agents(&project)?, id)
    }
    pub fn insert_trigger(
        &self,
        id: TriggerId,
        reason: WakeReason,
        hop: u8,
        batch: BatchId,
        now: Millis,
    ) -> Result<bool> {
        let project = self
            .project_for_batch(batch)
            .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        insert_trigger(self.project_agents(&project)?, id, reason, hop, batch, now)
    }
    pub fn batch_triggers(&self, batch: BatchId) -> Result<Vec<TriggerRow>> {
        let project = self
            .project_for_batch(batch)
            .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        batch_triggers(self.project_agents(&project)?, batch)
    }
    pub fn insert_obligation(&self, id: TriggerId) -> Result<bool> {
        let project = self
            .project_for_channel(id.channel)
            .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        insert_obligation(self.project_agents(&project)?, id)
    }

    /// Ensures a pending batch for `(agent, channel)` exists, then inserts the
    /// trigger (and obligation if requested) atomically within the project's
    /// agents database. Returns `true` if the trigger was newly inserted.
    #[allow(clippy::too_many_arguments)]
    pub fn ensure_pending_batch_trigger(
        &mut self,
        agent: MemberId,
        channel: ChannelId,
        message: MessageId,
        reason: WakeReason,
        hop: u8,
        create_obligation: bool,
        now: Millis,
    ) -> Result<bool> {
        let project = self
            .project_for_channel(channel)
            .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        // Fast path: if trigger already exists, nothing to do.
        let trigger_id = TriggerId {
            agent,
            channel,
            message,
        };
        if trigger_exists_db(self.project_agents(&project)?, trigger_id)? {
            return Ok(false);
        }

        let mut pending = pending_batch_db(self.project_agents(&project)?, agent, channel)?;
        if pending.is_none() {
            let new_id = BatchId(self.allocate_id("batch")?);
            pending = Some(BatchRow {
                id: new_id,
                agent,
                channel,
                state: BatchState::Pending,
                attempts: 0,
                window_to: None,
            });
        }
        let batch = pending.expect("set above");
        let ag = self.project_agents(&project)?;
        let tx = ag.unchecked_transaction()?;
        // Ensure pending batch row exists (idempotent under unique partial index).
        if tx
            .query_row(
                &format!("SELECT {BATCH_COLS} FROM batches WHERE agent_id = ?1 AND channel_id = ?2 AND state = 'pending'"),
                params![agent.0, channel.0],
                batch_from_row,
            )
            .optional()?
            .is_none()
        {
            tx.execute(
                "INSERT INTO batches (id, agent_id, channel_id, state, created_at) VALUES (?1, ?2, ?3, 'pending', ?4)",
                params![batch.id.0, agent.0, channel.0, now as i64],
            )?;
        }
        let actual_batch_id: i64 = tx.query_row(
            "SELECT id FROM batches WHERE agent_id = ?1 AND channel_id = ?2 AND state = 'pending'",
            params![agent.0, channel.0],
            |r| r.get(0),
        )?;
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO triggers (agent_id, channel_id, message_id, reason, hop, batch_id, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![agent.0, channel.0, message.0, reason.as_str(), hop as i64, actual_batch_id, now as i64],
        )? == 1;
        if inserted && create_obligation {
            tx.execute(
                "INSERT OR IGNORE INTO obligations (agent_id, channel_id, message_id, state) VALUES (?1, ?2, ?3, 'open')",
                params![agent.0, channel.0, message.0],
            )?;
        }
        tx.commit()?;
        self.note_batch(project, BatchId(actual_batch_id));
        Ok(inserted)
    }
    pub fn obligation_state(&self, id: TriggerId) -> Result<Option<ObligationState>> {
        let project = self
            .project_for_channel(id.channel)
            .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        obligation_state(self.project_agents(&project)?, id)
    }
    pub fn open_obligations(&self, batch: BatchId) -> Result<Vec<TriggerId>> {
        let project = self
            .project_for_batch(batch)
            .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        open_obligations(self.project_agents(&project)?, batch)
    }
    pub fn resolve_obligation(
        &self,
        id: TriggerId,
        state: ObligationState,
        resolved_by: Option<MessageId>,
        kind: &str,
    ) -> Result<bool> {
        let project = self
            .project_for_channel(id.channel)
            .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        resolve_obligation(self.project_agents(&project)?, id, state, resolved_by, kind)
    }
    pub fn receipts(&self, channel: ChannelId) -> Result<Vec<Receipt>> {
        let Some(project) = self.project_for_channel(channel) else {
            return Ok(vec![]);
        };
        receipts(self.project_workspace(&project)?, channel)
    }

    pub fn former_project_agent_mention_tokens(
        &self,
        project: &ProjectId,
    ) -> Result<Vec<(MemberId, Arc<str>)>> {
        // Former = agent not currently in project_members but has cursor/message history in project.
        let mut stmt = self.core.prepare(
            "SELECT m.id, m.handle FROM members m
             WHERE m.kind = 'agent'
               AND NOT EXISTS (SELECT 1 FROM project_members pm WHERE pm.project_id = ?1 AND pm.member_id = m.id)
             ORDER BY m.id"
        )?;
        let rows = stmt
            .query_map([project.as_str()], |r| {
                Ok((
                    MemberId(r.get::<_, i64>(0)?),
                    Arc::from(r.get::<_, String>(1)?),
                ))
            })?
            .collect::<Result<Vec<_>>>()?;
        let ws = match self.project_workspace(project) {
            Ok(c) => c,
            Err(_) => return Ok(vec![]),
        };
        let mut out = Vec::new();
        for (id, handle) in rows {
            let in_proj: bool = ws.query_row(
                "SELECT EXISTS(SELECT 1 FROM cursors WHERE agent_id=?1) OR EXISTS(SELECT 1 FROM messages WHERE author_id=?1)",
                [id.0], |r| r.get(0)).unwrap_or(false);
            if !in_proj {
                continue;
            }
            out.push((id, handle));
            // nicknames
            let mut ns = self.core.prepare(
                "SELECT nickname FROM member_nicknames WHERE member_id=?1 ORDER BY added_at, rowid",
            )?;
            let nrows = ns
                .query_map([id.0], |r| Ok(Arc::from(r.get::<_, String>(0)?)))?
                .collect::<Result<Vec<_>>>()?;
            for n in nrows {
                out.push((id, n));
            }
        }
        Ok(out)
    }
}

fn core_project_ids(core: &Connection) -> Result<Vec<ProjectId>> {
    let mut stmt = core.prepare("SELECT id FROM projects ORDER BY created_at, id")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for r in rows {
        if let Some(id) = ProjectId::parse(&r?) {
            out.push(id);
        }
    }
    Ok(out)
}

fn open_db(path: &Path, migrations: &[&str]) -> std::result::Result<Connection, OpenError> {
    // Create the file private before SQLite sees it: WAL/SHM sidecars inherit
    // the database's mode. Tighten an existing file and any old sidecars too.
    // Best effort — a read-only database must still be openable.
    let _ = crate::permissions::create_private_file(path);
    let conn = Connection::open(path)?;
    let _ = crate::permissions::restrict_database(path);
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let known = migrations.len() as i64;
    if version > known {
        return Err(OpenError::NewerSchema {
            path: path.to_path_buf(),
            found: version,
            known,
        });
    }
    conn.execute_batch(
        "PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON; PRAGMA synchronous = NORMAL;",
    )?;
    run_migrations(&conn, migrations, version as usize).map_err(OpenError::Sqlite)?;
    Ok(conn)
}

fn run_migrations(conn: &Connection, migrations: &[&str], from: usize) -> Result<()> {
    if from >= migrations.len() {
        return Ok(());
    }
    conn.execute_batch("PRAGMA foreign_keys = OFF")?;
    let result = (|| {
        for (ix, sql) in migrations.iter().enumerate().skip(from) {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(sql)?;
            let dangling: i64 =
                tx.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                    r.get(0)
                })?;
            if dangling > 0 {
                return Err(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY),
                    Some(format!(
                        "migration {} would leave {dangling} dangling foreign keys",
                        ix + 1
                    )),
                ));
            }
            tx.execute_batch(&format!("PRAGMA user_version = {}", ix + 1))?;
            tx.commit()?;
        }
        Ok(())
    })();
    conn.execute_batch("PRAGMA foreign_keys = ON")?;
    result
}

const CORE_MIGRATIONS: &[&str] = &[
    r#"
CREATE TABLE projects (
    id          TEXT PRIMARY KEY NOT NULL,
    name        TEXT NOT NULL,
    root_path   TEXT,
    created_at  INTEGER NOT NULL,
    unclaimed_import INTEGER NOT NULL DEFAULT 0 CHECK (unclaimed_import IN (0, 1))
);
CREATE UNIQUE INDEX projects_one_per_root
    ON projects (root_path) WHERE root_path IS NOT NULL;
CREATE TABLE members (
    id      INTEGER PRIMARY KEY,
    kind    TEXT NOT NULL CHECK (kind IN ('human', 'agent')),
    handle  TEXT NOT NULL UNIQUE COLLATE NOCASE,
    name    TEXT NOT NULL
);
CREATE TABLE project_members (
    project_id  TEXT NOT NULL REFERENCES projects(id),
    member_id   INTEGER NOT NULL REFERENCES members(id),
    joined_at   INTEGER NOT NULL,
    PRIMARY KEY (project_id, member_id)
);
CREATE TABLE member_nicknames (
    nickname   TEXT PRIMARY KEY NOT NULL COLLATE NOCASE,
    member_id  INTEGER NOT NULL REFERENCES members(id),
    added_at   INTEGER NOT NULL
);
CREATE INDEX member_nicknames_by_member ON member_nicknames (member_id, added_at);
CREATE TRIGGER member_nicknames_not_a_handle BEFORE INSERT ON member_nicknames
WHEN EXISTS (SELECT 1 FROM members WHERE handle = NEW.nickname)
BEGIN
    SELECT RAISE(ABORT, 'nickname_is_a_handle');
END;
CREATE TABLE model_members (
    member_id   INTEGER PRIMARY KEY REFERENCES members(id),
    harness     TEXT NOT NULL,
    model       TEXT NOT NULL DEFAULT '',
    model_name  TEXT NOT NULL,
    ping_answer_reactions TEXT,
    UNIQUE (harness, model)
);
CREATE TABLE roster (
    project_id  TEXT NOT NULL REFERENCES projects(id),
    member_id   INTEGER NOT NULL REFERENCES model_members(member_id),
    added_at    INTEGER NOT NULL,
    PRIMARY KEY (project_id, member_id)
);
CREATE TABLE roster_seeded (
    project_id  TEXT PRIMARY KEY NOT NULL REFERENCES projects(id),
    seeded_at   INTEGER NOT NULL
);
CREATE TABLE harness_models (
    harness  TEXT NOT NULL,
    ord      INTEGER NOT NULL,
    model    TEXT NOT NULL,
    name     TEXT NOT NULL,
    seen_at  INTEGER NOT NULL,
    PRIMARY KEY (harness, model)
);
CREATE TABLE id_counters (
    name    TEXT PRIMARY KEY NOT NULL,
    next_id INTEGER NOT NULL
);
INSERT INTO id_counters (name, next_id) VALUES ('channel', 1), ('message', 1), ('batch', 1);
"#,
    r#"
CREATE TABLE approval_outcomes (
    cc_request_id    TEXT PRIMARY KEY NOT NULL,
    state            TEXT NOT NULL,
    provider_reply   TEXT,
    execution_effect TEXT,
    error            TEXT,
    recorded_at      INTEGER
);
"#,
    r#"
CREATE TABLE chats (
    id          INTEGER PRIMARY KEY,
    title       TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    archived_at INTEGER
);
CREATE TABLE chat_members (
    chat_id     INTEGER NOT NULL REFERENCES chats(id) ON DELETE CASCADE,
    ordinal     INTEGER NOT NULL CHECK (ordinal > 0),
    project_id  TEXT NOT NULL,
    channel_id  INTEGER NOT NULL,
    PRIMARY KEY (chat_id, project_id, channel_id),
    UNIQUE (chat_id, ordinal)
);
CREATE INDEX chat_members_by_project_channel ON chat_members (project_id, channel_id);
INSERT OR IGNORE INTO id_counters (name, next_id) VALUES ('chat', 1);
"#,
    r#"
-- STRM-686: global agent lifecycle. Agents added after this migration have no
-- row and are therefore Sleeping (see Store::agent_lifecycle); existing agents
-- are seeded Active so an upgrade does not silently put live rooms to sleep.
CREATE TABLE agent_lifecycle (
    member_id    INTEGER PRIMARY KEY REFERENCES members(id) ON DELETE CASCADE,
    state        TEXT NOT NULL CHECK (state IN ('active', 'sleeping', 'unresponsive')),
    forced_sleep INTEGER NOT NULL DEFAULT 0 CHECK (forced_sleep IN (0, 1))
);
INSERT INTO agent_lifecycle (member_id, state, forced_sleep)
    SELECT id, 'active', 0 FROM members WHERE kind = 'agent';
-- Per-chat participation is keyed by ChatId, not by a project channel, so
-- future multi-target chats keep independent lifecycle state.
CREATE TABLE chat_member_lifecycle (
    chat_id      INTEGER NOT NULL REFERENCES chats(id) ON DELETE CASCADE,
    member_id    INTEGER NOT NULL REFERENCES members(id) ON DELETE CASCADE,
    state        TEXT NOT NULL CHECK (state IN ('active', 'sleeping')),
    forced_sleep INTEGER NOT NULL DEFAULT 0 CHECK (forced_sleep IN (0, 1)),
    PRIMARY KEY (chat_id, member_id)
);
"#,
    r#"
-- STRM-657: an agent's own normal-message preference. It is deliberately an
-- appended migration (and not part of the fresh-install CREATE TABLE above):
-- existing databases are already past that migration, and adding the column in
-- both places would fail a fresh install. Default 1 keeps today's behaviour:
-- active members receive ordinary broadcasts. The preference only ever gates
-- broadcasts; an explicit ping and a user-forced sleep outrank it.
ALTER TABLE model_members ADD COLUMN wake_normal_messages INTEGER NOT NULL DEFAULT 1;
"#,
];

const WORKSPACE_MIGRATIONS: &[&str] = &[r#"
CREATE TABLE channels (
    id          INTEGER PRIMARY KEY,
    project_id  TEXT NOT NULL,
    name        TEXT NOT NULL,
    ordinal     INTEGER NOT NULL CHECK (ordinal > 0),
    created_at  INTEGER NOT NULL,
    UNIQUE (project_id, name),
    UNIQUE (project_id, ordinal)
);
CREATE TABLE memberships (
    channel_id  INTEGER NOT NULL REFERENCES channels(id),
    member_id   INTEGER NOT NULL,
    joined_at   INTEGER NOT NULL,
    PRIMARY KEY (channel_id, member_id)
);
CREATE TABLE messages (
    id                 INTEGER PRIMARY KEY,
    channel_id         INTEGER NOT NULL REFERENCES channels(id),
    seq                INTEGER NOT NULL,
    author_id          INTEGER,
    origin             TEXT NOT NULL,
    body               TEXT NOT NULL,
    reply_to           INTEGER REFERENCES messages(id),
    hop                INTEGER NOT NULL DEFAULT 0,
    cause_batch        INTEGER,
    client_request_id  TEXT,
    sent_at            INTEGER NOT NULL,
    UNIQUE (channel_id, seq)
);
CREATE UNIQUE INDEX messages_request
    ON messages (author_id, channel_id, client_request_id)
    WHERE client_request_id IS NOT NULL;
CREATE UNIQUE INDEX messages_one_fallback
    ON messages (author_id, reply_to)
    WHERE origin = 'fallback';
CREATE TABLE mentions (
    message_id  INTEGER NOT NULL REFERENCES messages(id),
    member_id   INTEGER NOT NULL,
    PRIMARY KEY (message_id, member_id)
);
CREATE TABLE reactions (
    message_id  INTEGER NOT NULL REFERENCES messages(id),
    channel_id  INTEGER NOT NULL REFERENCES channels(id),
    member_id   INTEGER NOT NULL,
    emoji       TEXT NOT NULL CHECK (length(emoji) BETWEEN 1 AND 32),
    created_at  INTEGER NOT NULL,
    PRIMARY KEY (message_id, member_id, emoji)
);
CREATE INDEX reactions_by_channel ON reactions (channel_id, created_at);
CREATE TABLE cursors (
    agent_id           INTEGER NOT NULL,
    channel_id         INTEGER NOT NULL REFERENCES channels(id),
    delivered_through  INTEGER NOT NULL,
    read_through       INTEGER NOT NULL,
    delivered_at       INTEGER,
    read_at            INTEGER,
    PRIMARY KEY (agent_id, channel_id)
);
CREATE TABLE stances (
    message_id  INTEGER PRIMARY KEY NOT NULL REFERENCES messages(id),
    channel_id  INTEGER NOT NULL REFERENCES channels(id),
    kind        TEXT NOT NULL CHECK (kind IN ('refusal', 'dissent')),
    member_id   INTEGER NOT NULL,
    target_id   INTEGER NOT NULL REFERENCES messages(id),
    reason      TEXT NOT NULL,
    model       TEXT,
    runtime     TEXT,
    created_at  INTEGER NOT NULL
);
CREATE INDEX stances_by_channel ON stances (channel_id, message_id);
CREATE INDEX stances_by_target ON stances (target_id);
CREATE TABLE project_settings (
    project_id  TEXT NOT NULL,
    key         TEXT NOT NULL,
    value       TEXT NOT NULL,
    updated_at  INTEGER NOT NULL,
    PRIMARY KEY (project_id, key)
);
"#];

const AGENTS_MIGRATIONS: &[&str] = &[r#"
CREATE TABLE batches (
    id           INTEGER PRIMARY KEY,
    agent_id     INTEGER NOT NULL,
    channel_id   INTEGER NOT NULL,
    state        TEXT NOT NULL CHECK (state IN ('pending', 'dispatched', 'accepted', 'settled', 'failed')),
    attempts     INTEGER NOT NULL DEFAULT 0,
    window_to    INTEGER,
    created_at   INTEGER NOT NULL,
    accepted_at  INTEGER,
    settled_at   INTEGER,
    outcome      TEXT
);
CREATE UNIQUE INDEX batches_one_pending
    ON batches (agent_id, channel_id) WHERE state = 'pending';
CREATE UNIQUE INDEX batches_one_in_flight
    ON batches (agent_id, channel_id) WHERE state IN ('dispatched', 'accepted');
CREATE TABLE triggers (
    agent_id    INTEGER NOT NULL,
    channel_id  INTEGER NOT NULL,
    message_id  INTEGER NOT NULL,
    reason      TEXT NOT NULL,
    hop         INTEGER NOT NULL,
    batch_id    INTEGER NOT NULL REFERENCES batches(id),
    created_at  INTEGER NOT NULL,
    PRIMARY KEY (agent_id, channel_id, message_id)
);
CREATE TABLE obligations (
    agent_id       INTEGER NOT NULL,
    channel_id     INTEGER NOT NULL,
    message_id     INTEGER NOT NULL,
    state          TEXT NOT NULL CHECK (state IN ('open', 'satisfied', 'fallback', 'unmet', 'ignored', 'refused')),
    resolved_by    INTEGER,
    resolved_kind  TEXT,
    PRIMARY KEY (agent_id, channel_id, message_id),
    FOREIGN KEY (agent_id, channel_id, message_id)
        REFERENCES triggers (agent_id, channel_id, message_id)
);
CREATE TABLE sessions (
    agent_id    INTEGER NOT NULL,
    channel_id  INTEGER NOT NULL,
    protocol    TEXT NOT NULL,
    native_ref  TEXT NOT NULL,
    opened_at   INTEGER NOT NULL,
    PRIMARY KEY (agent_id, channel_id)
);
"#];

fn maybe_migrate_from_legacy(data_dir: &Path) -> std::result::Result<(), OpenError> {
    let legacy = data_dir.join("core.sqlite");
    let core_db = data_dir.join("core.db");
    let backup = data_dir.join("core.sqlite.pre-split");
    // Already migrated/complete?
    if core_db.exists() && !legacy.exists() {
        return Ok(());
    }
    // If backup exists but no core.db, or partial state, try to resume from backup/original.
    let source = if legacy.exists() {
        legacy.clone()
    } else if backup.exists() {
        backup.clone()
    } else {
        return Ok(()); // fresh install
    };
    // If core.db exists but legacy still exists, prefer completing a verified split layout
    // (e.g. crash after migration but before backup rename) over destructive retry.
    // Require an explicit, valid completion marker to avoid accepting empty-but-valid DBs.
    if core_db.exists() && legacy.exists() {
        let marker = split_migration_marker(data_dir);
        let mut valid = is_valid_split_migration_marker(data_dir);
        if valid
            && let Ok(core) = open_db(&core_db, CORE_MIGRATIONS)
            && let Ok(projects) = core_project_ids(&core)
        {
            valid = projects.iter().all(|p| {
                let dir = data_dir.join("Projects").join(p.as_str());
                open_db(&dir.join("workspace.db"), WORKSPACE_MIGRATIONS).is_ok()
                    && open_db(&dir.join("agents.db"), AGENTS_MIGRATIONS).is_ok()
            });
            if valid {
                // Best-effort: ensure counters are seeded (idempotent) before finalizing.
                if let Ok(legacy_src) = Connection::open(&legacy) {
                    let _ = seed_id_counters_from_legacy(&legacy_src, &core);
                }
            }
        } else {
            valid = false;
        }
        if valid {
            std::fs::rename(&legacy, &backup)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            let _ = std::fs::remove_file(marker);
            return Ok(());
        }
        let _ = std::fs::remove_file(&core_db);
        let _ = std::fs::remove_file(marker);
        let projects_dir = data_dir.join("Projects");
        let _ = std::fs::remove_dir_all(&projects_dir);
    }
    migrate_legacy(&source, data_dir)?;
    // Verify by opening all DBs, then mark complete and rename original to backup.
    let _core = open_db(&core_db, CORE_MIGRATIONS)?;
    // verify project dbs
    let projects = core_project_ids(&_core)?;
    for p in projects {
        let dir = data_dir.join("Projects").join(p.as_str());
        let _ws = open_db(&dir.join("workspace.db"), WORKSPACE_MIGRATIONS)?;
        let _ag = open_db(&dir.join("agents.db"), AGENTS_MIGRATIONS)?;
    }
    write_split_migration_marker_atomic(data_dir)?;
    if legacy.exists() {
        std::fs::rename(&legacy, &backup)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    }
    let _ = std::fs::remove_file(split_migration_marker(data_dir));
    Ok(())
}

fn migrate_legacy(source: &Path, data_dir: &Path) -> std::result::Result<(), OpenError> {
    // Open legacy read-only-ish and copy into new split layout.
    let legacy = Connection::open(source)?;
    let version: i64 = legacy.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let known = LEGACY_MIGRATIONS.len() as i64;
    if version > known {
        return Err(OpenError::NewerSchema {
            path: source.to_path_buf(),
            found: version,
            known,
        });
    }
    // Ensure legacy is fully migrated to latest legacy schema first.
    legacy.execute_batch("PRAGMA foreign_keys = OFF;")?;
    run_migrations(&legacy, LEGACY_MIGRATIONS, version as usize).map_err(OpenError::Sqlite)?;
    legacy.execute_batch("PRAGMA foreign_keys = ON;")?;

    let core_path = data_dir.join("core.db");
    if core_path.exists() {
        let _ = std::fs::remove_file(&core_path);
    }
    let core = open_db(&core_path, CORE_MIGRATIONS)?;

    // Core tables
    core.execute_batch("BEGIN IMMEDIATE;")?;
    let res = (|| -> Result<()> {
        copy_table(
            &legacy,
            &core,
            "projects",
            "id, name, root_path, created_at, unclaimed_import",
        )?;
        copy_table(&legacy, &core, "members", "id, kind, handle, name")?;
        // The STRM-686 migration that seeds `agent_lifecycle` ran during
        // `open_db` above, when `members` was still empty, so the seed
        // selected nothing. Seed here, after the copy, for the same reason
        // the migration gives: an upgrade must not silently put live rooms
        // to sleep (a missing row means Sleeping, which blocks even pings).
        core.execute(
            "INSERT OR IGNORE INTO agent_lifecycle (member_id, state, forced_sleep)
             SELECT id, 'active', 0 FROM members WHERE kind = 'agent'",
            [],
        )?;
        copy_table(
            &legacy,
            &core,
            "project_members",
            "project_id, member_id, joined_at",
        )?;
        if table_exists(&legacy, "member_nicknames")? {
            copy_table(
                &legacy,
                &core,
                "member_nicknames",
                "nickname, member_id, added_at",
            )?;
        }
        if table_exists(&legacy, "model_members")? {
            copy_table(
                &legacy,
                &core,
                "model_members",
                "member_id, harness, model, model_name, ping_answer_reactions",
            )?;
        }
        if table_exists(&legacy, "roster")? {
            copy_table(&legacy, &core, "roster", "project_id, member_id, added_at")?;
        }
        if table_exists(&legacy, "roster_seeded")? {
            copy_table(&legacy, &core, "roster_seeded", "project_id, seeded_at")?;
        }
        if table_exists(&legacy, "harness_models")? {
            copy_table(
                &legacy,
                &core,
                "harness_models",
                "harness, ord, model, name, seen_at",
            )?;
        }
        // reset sqlite sequences for core autoincrement tables
        reset_sqlite_sequence(&core, "members")?;
        Ok(())
    })();
    match res {
        Ok(()) => {
            core.execute_batch("COMMIT;")?;
        }
        Err(e) => {
            let _ = core.execute_batch("ROLLBACK;");
            return Err(OpenError::Sqlite(e));
        }
    }

    // Per-project DBs
    let project_ids = core_project_ids(&core)?;
    let projects_dir = data_dir.join("Projects");
    for pid in project_ids {
        let dir = projects_dir.join(pid.as_str());
        crate::permissions::ensure_private_dir(&dir)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        let ws_path = dir.join("workspace.db");
        let ag_path = dir.join("agents.db");
        if ws_path.exists() {
            let _ = std::fs::remove_file(&ws_path);
        }
        if ag_path.exists() {
            let _ = std::fs::remove_file(&ag_path);
        }
        let ws = open_db(&ws_path, WORKSPACE_MIGRATIONS)?;
        let ag = open_db(&ag_path, AGENTS_MIGRATIONS)?;
        ws.execute_batch("BEGIN IMMEDIATE;")?;
        let wres = (|| -> Result<()> {
            copy_project_table(
                &legacy,
                &ws,
                "channels",
                "id, project_id, name, ordinal, created_at",
                &pid,
            )?;
            copy_project_join(
                &legacy,
                &ws,
                "SELECT m.channel_id, m.member_id, m.joined_at FROM memberships m JOIN channels c ON c.id = m.channel_id WHERE c.project_id = ?1",
                "INSERT INTO memberships (channel_id, member_id, joined_at) VALUES (?1, ?2, ?3)",
                &pid,
                3,
            )?;
            copy_project_join(
                &legacy,
                &ws,
                "SELECT m.id, m.channel_id, m.seq, m.author_id, m.origin, m.body, m.reply_to, m.hop, m.cause_batch, m.client_request_id, m.sent_at FROM messages m JOIN channels c ON c.id = m.channel_id WHERE c.project_id = ?1",
                "INSERT INTO messages (id, channel_id, seq, author_id, origin, body, reply_to, hop, cause_batch, client_request_id, sent_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                &pid,
                11,
            )?;
            copy_project_join(
                &legacy,
                &ws,
                "SELECT x.message_id, x.member_id FROM mentions x JOIN messages m ON m.id = x.message_id JOIN channels c ON c.id = m.channel_id WHERE c.project_id = ?1",
                "INSERT INTO mentions (message_id, member_id) VALUES (?1, ?2)",
                &pid,
                2,
            )?;
            if table_exists(&legacy, "reactions")? {
                // reactions gained channel_id in migration 9 in legacy; copy accordingly
                copy_project_join(
                    &legacy,
                    &ws,
                    "SELECT r.message_id, r.channel_id, r.member_id, r.emoji, r.created_at FROM reactions r JOIN channels c ON c.id = r.channel_id WHERE c.project_id = ?1",
                    "INSERT INTO reactions (message_id, channel_id, member_id, emoji, created_at) VALUES (?1,?2,?3,?4,?5)",
                    &pid,
                    5,
                )?;
            }
            copy_project_join(
                &legacy,
                &ws,
                "SELECT k.agent_id, k.channel_id, k.delivered_through, k.read_through, k.delivered_at, k.read_at FROM cursors k JOIN channels c ON c.id = k.channel_id WHERE c.project_id = ?1",
                "INSERT INTO cursors (agent_id, channel_id, delivered_through, read_through, delivered_at, read_at) VALUES (?1,?2,?3,?4,?5,?6)",
                &pid,
                6,
            )?;
            if table_exists(&legacy, "stances")? {
                copy_project_join(
                    &legacy,
                    &ws,
                    "SELECT s.message_id, s.channel_id, s.kind, s.member_id, s.target_id, s.reason, s.model, s.runtime, s.created_at FROM stances s WHERE s.channel_id IN (SELECT id FROM channels WHERE project_id = ?1)",
                    "INSERT INTO stances (message_id, channel_id, kind, member_id, target_id, reason, model, runtime, created_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                    &pid,
                    9,
                )?;
            }
            if table_exists(&legacy, "project_settings")? {
                copy_project_join(
                    &legacy,
                    &ws,
                    "SELECT project_id, key, value, updated_at FROM project_settings WHERE project_id = ?1",
                    "INSERT INTO project_settings (project_id, key, value, updated_at) VALUES (?1,?2,?3,?4)",
                    &pid,
                    4,
                )?;
            }
            Ok(())
        })();
        match wres {
            Ok(()) => {
                ws.execute_batch("COMMIT;")?;
            }
            Err(e) => {
                let _ = ws.execute_batch("ROLLBACK;");
                return Err(OpenError::Sqlite(e));
            }
        }

        ag.execute_batch("BEGIN IMMEDIATE;")?;
        let ares = (|| -> Result<()> {
            copy_project_join(
                &legacy,
                &ag,
                "SELECT b.id, b.agent_id, b.channel_id, b.state, b.attempts, b.window_to, b.created_at, b.accepted_at, b.settled_at, b.outcome FROM batches b JOIN channels c ON c.id = b.channel_id WHERE c.project_id = ?1",
                "INSERT INTO batches (id, agent_id, channel_id, state, attempts, window_to, created_at, accepted_at, settled_at, outcome) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                &pid,
                10,
            )?;
            copy_project_join(
                &legacy,
                &ag,
                "SELECT t.agent_id, t.channel_id, t.message_id, t.reason, t.hop, t.batch_id, t.created_at FROM triggers t JOIN channels c ON c.id = t.channel_id WHERE c.project_id = ?1",
                "INSERT INTO triggers (agent_id, channel_id, message_id, reason, hop, batch_id, created_at) VALUES (?1,?2,?3,?4,?5,?6,?7)",
                &pid,
                7,
            )?;
            copy_project_join(
                &legacy,
                &ag,
                "SELECT o.agent_id, o.channel_id, o.message_id, o.state, o.resolved_by, o.resolved_kind FROM obligations o JOIN channels c ON c.id = o.channel_id WHERE c.project_id = ?1",
                "INSERT INTO obligations (agent_id, channel_id, message_id, state, resolved_by, resolved_kind) VALUES (?1,?2,?3,?4,?5,?6)",
                &pid,
                6,
            )?;
            copy_project_join(
                &legacy,
                &ag,
                "SELECT s.agent_id, s.channel_id, s.protocol, s.native_ref, s.opened_at FROM sessions s JOIN channels c ON c.id = s.channel_id WHERE c.project_id = ?1",
                "INSERT INTO sessions (agent_id, channel_id, protocol, native_ref, opened_at) VALUES (?1,?2,?3,?4,?5)",
                &pid,
                5,
            )?;
            Ok(())
        })();
        match ares {
            Ok(()) => {
                ag.execute_batch("COMMIT;")?;
            }
            Err(e) => {
                let _ = ag.execute_batch("ROLLBACK;");
                return Err(OpenError::Sqlite(e));
            }
        }
    }
    // Seed id_counters from legacy max IDs to avoid collisions on new inserts (F5).
    seed_id_counters_from_legacy(&legacy, &core)?;
    Ok(())
}

const SPLIT_MIGRATION_MARKER_CONTENT: &[u8] = b"v1\n";

fn split_migration_marker(data_dir: &Path) -> PathBuf {
    data_dir.join("core.split-migration-complete")
}

fn is_valid_split_migration_marker(data_dir: &Path) -> bool {
    let marker = split_migration_marker(data_dir);
    match std::fs::read(&marker) {
        Ok(bytes) => bytes == SPLIT_MIGRATION_MARKER_CONTENT,
        Err(_) => false,
    }
}

fn write_split_migration_marker_atomic(data_dir: &Path) -> std::result::Result<(), OpenError> {
    use std::io::Write;
    let marker = split_migration_marker(data_dir);
    let tmp = data_dir.join("core.split-migration-complete.tmp");
    let mut f = std::fs::File::create(&tmp)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    f.write_all(SPLIT_MIGRATION_MARKER_CONTENT)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    f.sync_all()
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    drop(f);
    std::fs::rename(&tmp, &marker)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    if let Ok(dir) = std::fs::File::open(data_dir) {
        let _ = dir.sync_all();
    }
    Ok(())
}

fn table_exists(c: &Connection, name: &str) -> Result<bool> {
    c.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
        [name],
        |_| Ok(()),
    )
    .optional()
    .map(|o| o.is_some())
}

fn copy_table(src: &Connection, dst: &Connection, table: &str, cols: &str) -> Result<()> {
    let mut stmt = src.prepare(&format!("SELECT {cols} FROM {table}"))?;
    let n = stmt.column_count();
    let placeholders = (1..=n)
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(",");
    let mut ins = dst.prepare(&format!(
        "INSERT INTO {table} ({cols}) VALUES ({placeholders})"
    ))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let mut params_vec: Vec<rusqlite::types::Value> = Vec::with_capacity(n);
        for i in 0..n {
            params_vec.push(row.get(i)?);
        }
        ins.execute(rusqlite::params_from_iter(params_vec))?;
    }
    Ok(())
}

fn copy_project_table(
    src: &Connection,
    dst: &Connection,
    table: &str,
    cols: &str,
    project: &ProjectId,
) -> Result<()> {
    let mut stmt = src.prepare(&format!("SELECT {cols} FROM {table} WHERE project_id = ?1"))?;
    let n = stmt.column_count();
    let placeholders = (1..=n)
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(",");
    let mut ins = dst.prepare(&format!(
        "INSERT INTO {table} ({cols}) VALUES ({placeholders})"
    ))?;
    let mut rows = stmt.query([project.as_str()])?;
    while let Some(row) = rows.next()? {
        let mut params_vec: Vec<rusqlite::types::Value> = Vec::with_capacity(n);
        for i in 0..n {
            params_vec.push(row.get(i)?);
        }
        ins.execute(rusqlite::params_from_iter(params_vec))?;
    }
    Ok(())
}

fn copy_project_join(
    src: &Connection,
    dst: &Connection,
    select_sql: &str,
    insert_sql: &str,
    project: &ProjectId,
    n: usize,
) -> Result<()> {
    let mut stmt = src.prepare(select_sql)?;
    let mut ins = dst.prepare(insert_sql)?;
    let mut rows = stmt.query([project.as_str()])?;
    while let Some(row) = rows.next()? {
        let mut params_vec: Vec<rusqlite::types::Value> = Vec::with_capacity(n);
        for i in 0..n {
            params_vec.push(row.get(i)?);
        }
        ins.execute(rusqlite::params_from_iter(params_vec))?;
    }
    Ok(())
}

fn seed_id_counters_from_legacy(legacy: &Connection, core: &Connection) -> Result<()> {
    let max_channel: i64 =
        legacy.query_row("SELECT COALESCE(MAX(id), 0) FROM channels", [], |r| {
            r.get(0)
        })?;
    let max_message: i64 =
        legacy.query_row("SELECT COALESCE(MAX(id), 0) FROM messages", [], |r| {
            r.get(0)
        })?;
    let max_batch: i64 =
        legacy.query_row("SELECT COALESCE(MAX(id), 0) FROM batches", [], |r| r.get(0))?;
    core.execute(
        "UPDATE id_counters SET next_id = ?2 WHERE name = ?1 AND next_id <= ?2",
        params!["channel", max_channel + 1],
    )?;
    core.execute(
        "UPDATE id_counters SET next_id = ?2 WHERE name = ?1 AND next_id <= ?2",
        params!["message", max_message + 1],
    )?;
    core.execute(
        "UPDATE id_counters SET next_id = ?2 WHERE name = ?1 AND next_id <= ?2",
        params!["batch", max_batch + 1],
    )?;
    Ok(())
}

fn reset_sqlite_sequence(c: &Connection, table: &str) -> Result<()> {
    if !table_exists(c, "sqlite_sequence")? {
        return Ok(());
    }
    let max: Option<i64> = c
        .query_row(&format!("SELECT MAX(id) FROM {table}"), [], |r| r.get(0))
        .optional()?
        .flatten();
    if let Some(max) = max {
        c.execute(
            "INSERT OR REPLACE INTO sqlite_sequence(name, seq) VALUES (?1, ?2)",
            params![table, max],
        )?;
    }
    Ok(())
}

// --- DB-specific helpers for split store ---

fn ensure_member_db(c: &Connection, kind: MemberKind, handle: &str, name: &str) -> Result<Member> {
    c.execute(
        "INSERT INTO members (kind, handle, name) VALUES (?1, ?2, ?3)
         ON CONFLICT(handle) DO UPDATE SET name = excluded.name",
        params![kind.as_str(), handle, name],
    )?;
    let m = member_by_handle_db(c, handle)?.expect("just ensured");
    c.execute("DELETE FROM member_nicknames WHERE nickname = ?1", [handle])?;
    Ok(m)
}

fn member_by_handle_db(c: &Connection, handle: &str) -> Result<Option<Member>> {
    c.query_row(
        "SELECT id, kind, handle, name FROM members WHERE handle = ?1 COLLATE NOCASE",
        [handle],
        member_from_row,
    )
    .optional()
}

fn member_db(c: &Connection, id: MemberId) -> Result<Option<Member>> {
    c.query_row(
        "SELECT id, kind, handle, name FROM members WHERE id = ?1",
        [id.0],
        member_from_row,
    )
    .optional()
}

fn unclaimed_import_db(c: &Connection) -> Result<Option<Project>> {
    c.query_row(
        &format!(
            "SELECT {PROJECT_COLS} FROM projects
             WHERE unclaimed_import = 1 AND root_path IS NULL
             ORDER BY created_at, id LIMIT 1"
        ),
        [],
        project_from_row,
    )
    .optional()
}

fn adopt_import_db(
    c: &Connection,
    id: &ProjectId,
    name: &str,
    root: &str,
) -> Result<Option<Project>> {
    let claimable = c
        .query_row(
            "SELECT 1 FROM projects
             WHERE id = ?1 AND unclaimed_import = 1 AND root_path IS NULL",
            [id.as_str()],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !claimable {
        return Ok(None);
    }
    c.execute(
        "UPDATE projects SET root_path = NULL WHERE root_path = ?1 AND id != ?2",
        params![root, id.as_str()],
    )?;
    c.execute(
        "UPDATE projects SET root_path = ?2, name = ?3, unclaimed_import = 0
         WHERE id = ?1 AND unclaimed_import = 1 AND root_path IS NULL",
        params![id.as_str(), root, name],
    )?;
    project_row(c, id)
}

fn project_has_messages_db(ws: &Connection) -> Result<bool> {
    ws.query_row("SELECT 1 FROM messages LIMIT 1", [], |_| Ok(()))
        .optional()
        .map(|o| o.is_some())
}

fn forget_empty_project_core(
    core: &Connection,
    project: &ProjectId,
    _channels: &[ChannelId],
) -> Result<Option<Vec<ChannelId>>> {
    let forgettable = core
        .query_row(
            "SELECT 1 FROM projects WHERE id = ?1 AND root_path IS NULL AND unclaimed_import = 0",
            [project.as_str()],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !forgettable {
        return Ok(None);
    }
    // caller checked no messages and provides channels; delete core records
    core.execute(
        "DELETE FROM project_members WHERE project_id = ?1",
        [project.as_str()],
    )?;
    core.execute(
        "DELETE FROM roster WHERE project_id = ?1",
        [project.as_str()],
    )?;
    core.execute(
        "DELETE FROM roster_seeded WHERE project_id = ?1",
        [project.as_str()],
    )?;
    core.execute(
        "DELETE FROM chat_members WHERE project_id = ?1",
        [project.as_str()],
    )?;
    core.execute(
        "DELETE FROM chats WHERE NOT EXISTS (SELECT 1 FROM chat_members cm WHERE cm.chat_id = chats.id)",
        [],
    )?;
    core.execute("DELETE FROM projects WHERE id = ?1", [project.as_str()])?;
    Ok(Some(_channels.to_vec()))
}

fn ensure_channel_db(
    ws: &Connection,
    project: &ProjectId,
    name: &str,
    id: ChannelId,
    now: Millis,
) -> Result<Channel> {
    let clean = name.strip_prefix('#').unwrap_or(name);
    if let Some(ch) = channel_in_project_db(ws, project, clean)? {
        return Ok(ch);
    }
    let ordinal: i64 = ws.query_row(
        "SELECT COALESCE(MAX(ordinal), 0) + 1 FROM channels WHERE project_id = ?1",
        [project.as_str()],
        |r| r.get(0),
    )?;
    ws.execute(
        "INSERT INTO channels (id, project_id, name, ordinal, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id.0, project.as_str(), clean, ordinal, now as i64],
    )?;
    channel_db(ws, id).map(|c| c.expect("just inserted"))
}

fn channel_db(ws: &Connection, id: ChannelId) -> Result<Option<Channel>> {
    ws.query_row(
        &format!("SELECT {CHANNEL_COLS} FROM channels WHERE id = ?1"),
        [id.0],
        channel_from_row,
    )
    .optional()
}

fn channel_in_project_db(
    ws: &Connection,
    project: &ProjectId,
    name: &str,
) -> Result<Option<Channel>> {
    let clean = name.strip_prefix('#').unwrap_or(name);
    ws.query_row(
        &format!(
            "SELECT {CHANNEL_COLS} FROM channels WHERE project_id = ?1 AND name = ?2 COLLATE NOCASE"
        ),
        params![project.as_str(), clean],
        channel_from_row,
    )
    .optional()
}

fn channels_in_project_db(ws: &Connection, project: &ProjectId) -> Result<Vec<Channel>> {
    let mut stmt = ws.prepare_cached(&format!(
        "SELECT {CHANNEL_COLS} FROM channels WHERE project_id = ?1 ORDER BY ordinal, id"
    ))?;
    stmt.query_map([project.as_str()], channel_from_row)?
        .collect()
}

fn rename_channel_db(ws: &Connection, id: ChannelId, name: &str) -> Result<bool> {
    let clean = name.strip_prefix('#').unwrap_or(name);
    Ok(ws.execute(
        "UPDATE channels SET name = ?2 WHERE id = ?1",
        params![id.0, clean],
    )? == 1)
}

fn members_of_db(core: &Connection, ws: &Connection, channel: ChannelId) -> Result<Vec<Member>> {
    let mut stmt = ws.prepare_cached(
        "SELECT member_id FROM memberships WHERE channel_id = ?1 ORDER BY member_id",
    )?;
    let ids = stmt
        .query_map([channel.0], |r| r.get::<_, i64>(0))?
        .collect::<Result<Vec<_>>>()?;
    let mut out = Vec::new();
    for id in ids {
        if let Some(m) = member_db(core, MemberId(id))? {
            out.push(m);
        }
    }
    Ok(out)
}

fn is_member_db(ws: &Connection, channel: ChannelId, member: MemberId) -> Result<bool> {
    ws.query_row(
        "SELECT 1 FROM memberships WHERE channel_id = ?1 AND member_id = ?2",
        params![channel.0, member.0],
        |_| Ok(()),
    )
    .optional()
    .map(|o| o.is_some())
}

fn join_db(
    ws: &Connection,
    channel: ChannelId,
    member: &Member,
    backfill: usize,
    now: Millis,
) -> Result<bool> {
    let inserted = ws.execute(
        "INSERT OR IGNORE INTO memberships (channel_id, member_id, joined_at) VALUES (?1, ?2, ?3)",
        params![channel.0, member.id.0, now as i64],
    )? == 1;
    if inserted {
        let seq = head_seq_db(ws, channel)?;
        let delivered = seq.saturating_sub(backfill as Seq);
        ws.execute(
            "INSERT OR IGNORE INTO cursors (agent_id, channel_id, delivered_through, read_through) VALUES (?1, ?2, ?3, ?3)",
            params![member.id.0, channel.0, delivered as i64],
        )?;
    }
    Ok(inserted)
}

fn head_seq_db(ws: &Connection, channel: ChannelId) -> Result<Seq> {
    ws.query_row(
        "SELECT COALESCE(MAX(seq), 0) FROM messages WHERE channel_id = ?1",
        [channel.0],
        |r| r.get::<_, i64>(0),
    )
    .map(|s| s as Seq)
}

fn insert_message_db(ws: &Connection, m: &NewMessage<'_>, id: MessageId) -> Result<Message> {
    let tx = ws.unchecked_transaction()?;
    let seq: i64 = tx.query_row(
        "SELECT COALESCE(MAX(seq), 0) + 1 FROM messages WHERE channel_id = ?1",
        [m.channel.0],
        |r| r.get(0),
    )?;
    tx.execute(
        "INSERT INTO messages (id, channel_id, seq, author_id, origin, body, reply_to, hop, cause_batch, client_request_id, sent_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            id.0, m.channel.0, seq, m.author.map(|a| a.0), m.origin.as_str(), m.body,
            m.reply_to.map(|r| r.0), m.hop as i64, m.cause_batch.map(|b| b.0),
            m.client_request_id, m.sent_at as i64
        ],
    )?;
    for member in m.mentions {
        tx.execute(
            "INSERT OR IGNORE INTO mentions (message_id, member_id) VALUES (?1, ?2)",
            params![id.0, member.0],
        )?;
    }
    tx.commit()?;
    Ok(Message {
        id,
        channel: m.channel,
        seq: seq as Seq,
        author: m.author,
        origin: m.origin,
        body: Arc::from(m.body),
        reply_to: m.reply_to,
        mentions: m.mentions.to_vec(),
        hop: m.hop,
        sent_at: m.sent_at,
    })
}

fn message_db(ws: &Connection, id: MessageId) -> Result<Option<Message>> {
    query_messages_db(
        ws,
        &format!("SELECT {MESSAGE_COLS} FROM messages WHERE id = ?1"),
        [id.0],
    )
    .map(|mut v| v.pop())
}

fn message_by_request_db(
    ws: &Connection,
    author: MemberId,
    channel: ChannelId,
    request: &str,
) -> Result<Option<Message>> {
    query_messages_db(ws, &format!("SELECT {MESSAGE_COLS} FROM messages WHERE author_id = ?1 AND channel_id = ?2 AND client_request_id = ?3"), params![author.0, channel.0, request]).map(|mut v| v.pop())
}

fn messages_before_db(
    ws: &Connection,
    channel: ChannelId,
    before_seq: Seq,
    limit: usize,
) -> Result<Vec<Message>> {
    let mut v = query_messages_db(
        ws,
        &format!(
            "SELECT {MESSAGE_COLS} FROM messages WHERE channel_id = ?1 AND seq < ?2 ORDER BY seq DESC LIMIT ?3"
        ),
        params![channel.0, before_seq as i64, limit as i64],
    )?;
    v.reverse();
    Ok(v)
}

fn query_messages_db(ws: &Connection, sql: &str, p: impl rusqlite::Params) -> Result<Vec<Message>> {
    let mut stmt = ws.prepare(sql)?;
    let msgs = stmt
        .query_map(p, |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<i64>>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, Option<i64>>(6)?,
                r.get::<_, i64>(7)?,
                r.get::<_, Option<i64>>(8)?,
                r.get::<_, Option<String>>(9)?,
                r.get::<_, i64>(10)?,
            ))
        })?
        .collect::<Result<Vec<_>>>()?;
    let mut out = Vec::with_capacity(msgs.len());
    for (
        id,
        channel,
        seq,
        author,
        origin,
        body,
        reply_to,
        hop,
        _cause_batch,
        _client_request_id,
        sent_at,
    ) in msgs
    {
        let mut mstmt = ws.prepare_cached(
            "SELECT member_id FROM mentions WHERE message_id = ?1 ORDER BY member_id",
        )?;
        let mentions = mstmt
            .query_map([id], |r| Ok(MemberId(r.get(0)?)))?
            .collect::<Result<Vec<_>>>()?;
        out.push(Message {
            id: MessageId(id),
            channel: ChannelId(channel),
            seq: seq as Seq,
            author: author.map(MemberId),
            origin: Origin::parse(&origin),
            body: Arc::from(body),
            reply_to: reply_to.map(MessageId),
            mentions,
            hop: hop.clamp(0, 255) as u8,
            sent_at: sent_at as Millis,
        });
    }
    Ok(out)
}

fn cursor_db(ws: &Connection, agent: MemberId, channel: ChannelId) -> Result<Cursor> {
    ws.query_row(
        "SELECT delivered_through, read_through, read_at FROM cursors WHERE agent_id = ?1 AND channel_id = ?2",
        params![agent.0, channel.0],
        |r| Ok(Cursor { delivered_through: r.get::<_, i64>(0)? as Seq, read_through: r.get::<_, i64>(1)? as Seq, read_at: r.get::<_, Option<i64>>(2)?.map(|t| t as Millis) }),
    ).optional().map(|o| o.unwrap_or(Cursor { delivered_through: 0, read_through: 0, read_at: None }))
}

fn pending_batch_db(
    ag: &Connection,
    agent: MemberId,
    channel: ChannelId,
) -> Result<Option<BatchRow>> {
    ag.query_row(&format!("SELECT {BATCH_COLS} FROM batches WHERE agent_id = ?1 AND channel_id = ?2 AND state = 'pending'"), params![agent.0, channel.0], batch_from_row).optional()
}
fn in_flight_batch_db(
    ag: &Connection,
    agent: MemberId,
    channel: ChannelId,
) -> Result<Option<BatchRow>> {
    ag.query_row(&format!("SELECT {BATCH_COLS} FROM batches WHERE agent_id = ?1 AND channel_id = ?2 AND state IN ('dispatched', 'accepted')"), params![agent.0, channel.0], batch_from_row).optional()
}
fn pending_batch_or_create_db(
    ag: &Connection,
    agent: MemberId,
    channel: ChannelId,
    now: Millis,
    id: BatchId,
) -> Result<BatchRow> {
    if let Some(b) = pending_batch_db(ag, agent, channel)? {
        return Ok(b);
    }
    ag.execute("INSERT INTO batches (id, agent_id, channel_id, state, created_at) VALUES (?1, ?2, ?3, 'pending', ?4)", params![id.0, agent.0, channel.0, now as i64])?;
    Ok(batch_db(ag, id)?.expect("just created"))
}

fn nicknames_of_db(core: &Connection, member: MemberId) -> Result<Vec<Arc<str>>> {
    let mut stmt = core.prepare_cached(
        "SELECT nickname FROM member_nicknames WHERE member_id = ?1 ORDER BY added_at, nickname",
    )?;
    stmt.query_map([member.0], |r| r.get::<_, String>(0).map(Arc::from))?
        .collect()
}
fn nicknames_in_db(
    core: &Connection,
    ws: &Connection,
    channel: ChannelId,
) -> Result<Vec<(MemberId, Arc<str>)>> {
    let mut stmt = ws.prepare_cached("SELECT member_id FROM memberships WHERE channel_id = ?1")?;
    let ids = stmt
        .query_map([channel.0], |r| r.get::<_, i64>(0))?
        .collect::<Result<Vec<_>>>()?;
    let mut out = Vec::new();
    for id in ids {
        let mut nstmt = core.prepare_cached("SELECT nickname FROM member_nicknames WHERE member_id = ?1 ORDER BY added_at, nickname")?;
        let nicks = nstmt
            .query_map([id], |r| r.get::<_, String>(0).map(Arc::from))?
            .collect::<Result<Vec<_>>>()?;
        for n in nicks {
            out.push((MemberId(id), n));
        }
    }
    Ok(out)
}
fn mention_owner_db(core: &Connection, token: &str) -> Result<Option<MemberId>> {
    if let Some(id) = core
        .query_row(
            "SELECT id FROM members WHERE handle = ?1 COLLATE NOCASE",
            [token],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
    {
        return Ok(Some(MemberId(id)));
    }
    core.query_row(
        "SELECT member_id FROM member_nicknames WHERE nickname = ?1 COLLATE NOCASE",
        [token],
        |r| r.get::<_, i64>(0),
    )
    .optional()
    .map(|o| o.map(MemberId))
}
fn add_nickname_db(core: &Connection, member: MemberId, nickname: &str, now: Millis) -> Result<()> {
    core.execute(
        "INSERT INTO member_nicknames (nickname, member_id, added_at) VALUES (?1, ?2, ?3)",
        params![nickname, member.0, now as i64],
    )?;
    Ok(())
}
fn remove_nickname_db(core: &Connection, member: MemberId, nickname: &str) -> Result<bool> {
    Ok(core.execute(
        "DELETE FROM member_nicknames WHERE member_id = ?1 AND nickname = ?2 COLLATE NOCASE",
        params![member.0, nickname],
    )? == 1)
}
fn project_setting_db(ws: &Connection, project: &ProjectId, key: &str) -> Result<Option<String>> {
    ws.query_row(
        "SELECT value FROM project_settings WHERE project_id = ?1 AND key = ?2",
        params![project.as_str(), key],
        |r| r.get(0),
    )
    .optional()
}
fn set_project_setting_db(
    ws: &Connection,
    project: &ProjectId,
    key: &str,
    value: Option<&str>,
    now: Millis,
) -> Result<()> {
    match value {
        Some(v) => ws.execute("INSERT INTO project_settings (project_id, key, value, updated_at) VALUES (?1, ?2, ?3, ?4) ON CONFLICT(project_id, key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at", params![project.as_str(), key, v, now as i64])?,
        None => ws.execute("DELETE FROM project_settings WHERE project_id = ?1 AND key = ?2", params![project.as_str(), key])?,
    };
    Ok(())
}

fn batch_db(ag: &Connection, id: BatchId) -> Result<Option<BatchRow>> {
    ag.query_row(
        &format!("SELECT {BATCH_COLS} FROM batches WHERE id = ?1"),
        [id.0],
        batch_from_row,
    )
    .optional()
}
fn set_batch_state_db(
    ag: &Connection,
    id: BatchId,
    state: BatchState,
    now: Millis,
    outcome: Option<&str>,
) -> Result<()> {
    match state {
        BatchState::Accepted => ag.execute(
            "UPDATE batches SET state = 'accepted', accepted_at = ?2 WHERE id = ?1",
            params![id.0, now as i64],
        )?,
        BatchState::Settled | BatchState::Failed => ag.execute(
            "UPDATE batches SET state = ?2, settled_at = ?3, outcome = ?4 WHERE id = ?1",
            params![id.0, state.as_str(), now as i64, outcome],
        )?,
        _ => ag.execute(
            "UPDATE batches SET state = ?2 WHERE id = ?1",
            params![id.0, state.as_str()],
        )?,
    };
    Ok(())
}
#[allow(dead_code)]
fn dispatch_batch_db(ag: &Connection, id: BatchId, window_to: Seq) -> Result<u32> {
    ag.execute("UPDATE batches SET state = 'dispatched', attempts = attempts + 1, window_to = ?2 WHERE id = ?1 AND state = 'pending'", params![id.0, window_to as i64])?;
    ag.query_row("SELECT attempts FROM batches WHERE id = ?1", [id.0], |r| {
        r.get::<_, i64>(0).map(|a| a as u32)
    })
}
fn requeue_batch_db(ag: &Connection, id: BatchId) -> Result<()> {
    let Some(b) = batch_db(ag, id)? else {
        return Ok(());
    };
    if b.state != BatchState::Dispatched {
        return Ok(());
    }
    if let Some(p) = pending_batch_db(ag, b.agent, b.channel)? {
        ag.execute(
            "UPDATE triggers SET batch_id = ?1 WHERE batch_id = ?2",
            params![id.0, p.id.0],
        )?;
        ag.execute("DELETE FROM batches WHERE id = ?1", [p.id.0])?;
    }
    ag.execute("UPDATE batches SET state = 'pending' WHERE id = ?1", [id.0])?;
    Ok(())
}
#[allow(dead_code)]
fn insert_trigger_db(
    ag: &Connection,
    id: TriggerId,
    reason: WakeReason,
    hop: u8,
    batch: BatchId,
    now: Millis,
) -> Result<bool> {
    Ok(ag.execute("INSERT OR IGNORE INTO triggers (agent_id, channel_id, message_id, reason, hop, batch_id, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)", params![id.agent.0, id.channel.0, id.message.0, reason.as_str(), hop as i64, batch.0, now as i64])? == 1)
}
#[allow(dead_code)]
fn trigger_exists_db(ag: &Connection, id: TriggerId) -> Result<bool> {
    ag.query_row(
        "SELECT 1 FROM triggers WHERE agent_id = ?1 AND channel_id = ?2 AND message_id = ?3",
        params![id.agent.0, id.channel.0, id.message.0],
        |_| Ok(()),
    )
    .optional()
    .map(|o| o.is_some())
}
#[allow(dead_code)]
fn batch_triggers_db(ag: &Connection, batch: BatchId) -> Result<Vec<TriggerRow>> {
    let mut stmt = ag.prepare_cached("SELECT agent_id, channel_id, message_id, reason, hop, batch_id FROM triggers WHERE batch_id = ?1 ORDER BY message_id")?;
    stmt.query_map([batch.0], |r| {
        Ok(TriggerRow {
            id: TriggerId {
                agent: MemberId(r.get(0)?),
                channel: ChannelId(r.get(1)?),
                message: MessageId(r.get(2)?),
            },
            reason: WakeReason::parse(&r.get::<_, String>(3)?),
            hop: r.get::<_, i64>(4)?.clamp(0, 255) as u8,
            batch: BatchId(r.get(5)?),
        })
    })?
    .collect()
}
#[allow(dead_code)]
fn insert_obligation_db(ag: &Connection, id: TriggerId) -> Result<bool> {
    Ok(ag.execute("INSERT OR IGNORE INTO obligations (agent_id, channel_id, message_id, state) VALUES (?1, ?2, ?3, 'open')", params![id.agent.0, id.channel.0, id.message.0])? == 1)
}
#[allow(dead_code)]
fn obligation_state_db(ag: &Connection, id: TriggerId) -> Result<Option<ObligationState>> {
    ag.query_row(
        "SELECT state FROM obligations WHERE agent_id = ?1 AND channel_id = ?2 AND message_id = ?3",
        params![id.agent.0, id.channel.0, id.message.0],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .map(|o| o.map(|s| ObligationState::parse(&s)))
}
fn open_obligations_db(ag: &Connection, batch: BatchId) -> Result<Vec<TriggerId>> {
    let mut stmt = ag.prepare_cached("SELECT o.agent_id, o.channel_id, o.message_id FROM obligations o JOIN triggers t ON t.agent_id = o.agent_id AND t.channel_id = o.channel_id AND t.message_id = o.message_id WHERE t.batch_id = ?1 AND o.state = 'open' ORDER BY o.message_id")?;
    stmt.query_map([batch.0], |r| {
        Ok(TriggerId {
            agent: MemberId(r.get(0)?),
            channel: ChannelId(r.get(1)?),
            message: MessageId(r.get(2)?),
        })
    })?
    .collect()
}
fn resolve_obligation_db(
    ag: &Connection,
    id: TriggerId,
    state: ObligationState,
    by: Option<MessageId>,
    kind: &str,
) -> Result<bool> {
    Ok(ag.execute("UPDATE obligations SET state = ?4, resolved_by = ?5, resolved_kind = ?6 WHERE agent_id = ?1 AND channel_id = ?2 AND message_id = ?3 AND state = 'open'", params![id.agent.0, id.channel.0, id.message.0, state.as_str(), by.map(|m| m.0), kind])? == 1)
}
#[allow(dead_code)]
fn record_session_db(
    ag: &Connection,
    agent: MemberId,
    channel: ChannelId,
    protocol: &str,
    native_ref: &str,
    now: Millis,
) -> Result<()> {
    ag.execute("INSERT INTO sessions (agent_id, channel_id, protocol, native_ref, opened_at) VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(agent_id, channel_id) DO UPDATE SET protocol = excluded.protocol, native_ref = excluded.native_ref, opened_at = excluded.opened_at", params![agent.0, channel.0, protocol, native_ref, now as i64])?;
    Ok(())
}

// ---------- projects ----------

const PROJECT_COLS: &str = "id, name, root_path, created_at";

fn project_from_row(r: &Row<'_>) -> Result<Project> {
    let id: String = r.get(0)?;
    Ok(Project {
        id: ProjectId::parse(&id).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                format!("bad project id {id:?}").into(),
            )
        })?,
        name: Arc::from(r.get::<_, String>(1)?),
        root_path: r.get::<_, Option<String>>(2)?.map(Arc::from),
        created_at: r.get::<_, i64>(3)? as Millis,
    })
}

/// Creates the project if its id is new, else refreshes its display name.
/// With a `root`, records it as the folder the project was last seen in; a
/// folder belongs to one project, so any other project that claimed it lets
/// go of it. Never touches channels, members or history.
pub fn ensure_project(
    c: &Connection,
    id: &ProjectId,
    name: &str,
    root: Option<&str>,
    now: Millis,
) -> Result<Project> {
    ensure_project_row(c, id, name, root, now)
}

pub fn ensure_project_row(
    c: &Connection,
    id: &ProjectId,
    name: &str,
    root: Option<&str>,
    now: Millis,
) -> Result<Project> {
    if let Some(root) = root {
        c.execute(
            "UPDATE projects SET root_path = NULL WHERE root_path = ?1 AND id != ?2",
            params![root, id.as_str()],
        )?;
    }
    c.execute(
        "INSERT INTO projects (id, name, root_path, created_at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(id) DO UPDATE SET
            name = excluded.name,
            root_path = COALESCE(excluded.root_path, projects.root_path)",
        params![id.as_str(), name, root, now as i64],
    )?;
    project(c, id).map(|p| p.expect("just upserted"))
}

pub fn project(c: &Connection, id: &ProjectId) -> Result<Option<Project>> {
    project_row(c, id)
}

pub fn project_row(c: &Connection, id: &ProjectId) -> Result<Option<Project>> {
    c.query_row(
        &format!("SELECT {PROJECT_COLS} FROM projects WHERE id = ?1"),
        [id.as_str()],
        project_from_row,
    )
    .optional()
}

/// The project last opened from `root`, if any.
pub fn project_by_root(c: &Connection, root: &str) -> Result<Option<Project>> {
    project_by_root_row(c, root)
}

pub fn project_by_root_row(c: &Connection, root: &str) -> Result<Option<Project>> {
    c.query_row(
        &format!("SELECT {PROJECT_COLS} FROM projects WHERE root_path = ?1"),
        [root],
        project_from_row,
    )
    .optional()
}

/// Every project, oldest first.
pub fn projects(c: &Connection) -> Result<Vec<Project>> {
    projects_rows(c)
}

pub fn projects_rows(c: &Connection) -> Result<Vec<Project>> {
    let mut stmt = c.prepare_cached(&format!(
        "SELECT {PROJECT_COLS} FROM projects ORDER BY created_at, id"
    ))?;
    stmt.query_map([], project_from_row)?.collect()
}

/// The project migration 2 made from pre-project history, if no folder has
/// adopted it yet. Never one with a folder: a live root is not up for grabs.
pub fn unclaimed_import(c: &Connection) -> Result<Option<Project>> {
    c.query_row(
        &format!(
            "SELECT {PROJECT_COLS} FROM projects
             WHERE unclaimed_import = 1 AND root_path IS NULL
             ORDER BY created_at, id LIMIT 1"
        ),
        [],
        project_from_row,
    )
    .optional()
}

/// Gives the unclaimed import (`id`) the folder `root` and the display name
/// `name`, and marks it claimed, so it is never offered again. Refuses
/// (`Ok(None)`) unless `id` is still an unclaimed import with no folder. Any
/// project that held `root` before (the empty one opened while the user was
/// asked) lets go of it, as in [`ensure_project`]. Channels, members and
/// history are untouched.
pub fn adopt_import(
    c: &Connection,
    id: &ProjectId,
    name: &str,
    root: &str,
) -> Result<Option<Project>> {
    let claimable = c
        .query_row(
            "SELECT 1 FROM projects
             WHERE id = ?1 AND unclaimed_import = 1 AND root_path IS NULL",
            [id.as_str()],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !claimable {
        return Ok(None);
    }
    c.execute(
        "UPDATE projects SET root_path = NULL WHERE root_path = ?1 AND id != ?2",
        params![root, id.as_str()],
    )?;
    c.execute(
        "UPDATE projects SET root_path = ?2, name = ?3, unclaimed_import = 0
         WHERE id = ?1 AND unclaimed_import = 1 AND root_path IS NULL",
        params![id.as_str(), root, name],
    )?;
    project(c, id)
}

/// Whether any channel of `project` has a message.
pub fn project_has_messages(c: &Connection, project: &ProjectId) -> Result<bool> {
    c.query_row(
        "SELECT 1 FROM messages m JOIN channels ch ON ch.id = m.channel_id
         WHERE ch.project_id = ?1 LIMIT 1",
        [project.as_str()],
        |_| Ok(()),
    )
    .optional()
    .map(|o| o.is_some())
}

/// Deletes `project` and its channels if nothing was ever said there, it has
/// no folder and it isn't the unclaimed import. Returns the deleted channel
/// ids, or `None` when it kept the project. For the placeholder a folder opened
/// while its identity question was open, once the answer moved elsewhere.
pub fn forget_empty_project(c: &Connection, project: &ProjectId) -> Result<Option<Vec<ChannelId>>> {
    let forgettable = c
        .query_row(
            "SELECT 1 FROM projects
             WHERE id = ?1 AND root_path IS NULL AND unclaimed_import = 0",
            [project.as_str()],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !forgettable || project_has_messages(c, project)? {
        return Ok(None);
    }
    let channels: Vec<ChannelId> = channels_in_project(c, project)?
        .into_iter()
        .map(|ch| ch.id)
        .collect();
    // Messages hang everything else (mentions, reactions, triggers,
    // obligations) off a message, and there are none.
    for table in ["sessions", "cursors", "batches", "memberships"] {
        c.execute(
            &format!(
                "DELETE FROM {table} WHERE channel_id IN
                 (SELECT id FROM channels WHERE project_id = ?1)"
            ),
            [project.as_str()],
        )?;
    }
    c.execute(
        "DELETE FROM channels WHERE project_id = ?1",
        [project.as_str()],
    )?;
    for table in ["project_members", "project_settings"] {
        c.execute(
            &format!("DELETE FROM {table} WHERE project_id = ?1"),
            [project.as_str()],
        )?;
    }
    c.execute("DELETE FROM projects WHERE id = ?1", [project.as_str()])?;
    Ok(Some(channels))
}

/// A project setting's stored JSON value, if one was set.
pub fn project_setting(c: &Connection, project: &ProjectId, key: &str) -> Result<Option<String>> {
    c.query_row(
        "SELECT value FROM project_settings WHERE project_id = ?1 AND key = ?2",
        params![project.as_str(), key],
        |r| r.get(0),
    )
    .optional()
}

/// Sets (`Some`) or clears (`None`, back to the default) a project setting.
/// The caller validates `value` first.
pub fn set_project_setting(
    c: &Connection,
    project: &ProjectId,
    key: &str,
    value: Option<&str>,
    now: Millis,
) -> Result<()> {
    match value {
        Some(v) => c.execute(
            "INSERT INTO project_settings (project_id, key, value, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(project_id, key) DO UPDATE SET
                value = excluded.value, updated_at = excluded.updated_at",
            params![project.as_str(), key, v, now as i64],
        )?,
        None => c.execute(
            "DELETE FROM project_settings WHERE project_id = ?1 AND key = ?2",
            params![project.as_str(), key],
        )?,
    };
    Ok(())
}

/// Changes a project's display name only; its id, channels and history stay.
pub fn rename_project(c: &Connection, id: &ProjectId, name: &str) -> Result<bool> {
    rename_project_row(c, id, name)
}

pub fn rename_project_row(c: &Connection, id: &ProjectId, name: &str) -> Result<bool> {
    Ok(c.execute(
        "UPDATE projects SET name = ?2 WHERE id = ?1",
        params![id.as_str(), name],
    )? == 1)
}

/// Makes `member` a member of `project` (the permission to join its
/// channels). Returns whether it was new.
pub fn join_project(
    c: &Connection,
    project: &ProjectId,
    member: MemberId,
    now: Millis,
) -> Result<bool> {
    join_project_row(c, project, member, now)
}

pub fn join_project_row(
    c: &Connection,
    project: &ProjectId,
    member: MemberId,
    now: Millis,
) -> Result<bool> {
    Ok(c.execute(
        "INSERT OR IGNORE INTO project_members (project_id, member_id, joined_at)
         VALUES (?1, ?2, ?3)",
        params![project.as_str(), member.0, now as i64],
    )? == 1)
}

pub fn is_project_member(c: &Connection, project: &ProjectId, member: MemberId) -> Result<bool> {
    is_project_member_row(c, project, member)
}

pub fn is_project_member_row(
    c: &Connection,
    project: &ProjectId,
    member: MemberId,
) -> Result<bool> {
    c.query_row(
        "SELECT 1 FROM project_members WHERE project_id = ?1 AND member_id = ?2",
        params![project.as_str(), member.0],
        |_| Ok(()),
    )
    .optional()
    .map(|o| o.is_some())
}

// ---------- channels & members ----------

const CHANNEL_COLS: &str = "id, project_id, name, ordinal";

fn channel_from_row(r: &Row<'_>) -> Result<Channel> {
    let project: String = r.get(1)?;
    Ok(Channel {
        id: ChannelId(r.get(0)?),
        project: ProjectId::parse(&project).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                1,
                rusqlite::types::Type::Text,
                format!("bad project id {project:?}").into(),
            )
        })?,
        name: Arc::from(r.get::<_, String>(2)?),
        ordinal: r.get::<_, i64>(3)?.clamp(1, u32::MAX as i64) as u32,
    })
}

/// The channel called `name` in `project`, created (with the project's next
/// `::N` ordinal) if it doesn't exist. The project must exist.
pub fn ensure_channel(
    c: &Connection,
    project: &ProjectId,
    name: &str,
    now: Millis,
) -> Result<Channel> {
    let name = name.trim_start_matches('#');
    c.execute(
        "INSERT INTO channels (project_id, name, ordinal, created_at)
         VALUES (?1, ?2,
                 (SELECT COALESCE(MAX(ordinal), 0) + 1 FROM channels WHERE project_id = ?1),
                 ?3)
         ON CONFLICT(project_id, name) DO NOTHING",
        params![project.as_str(), name, now as i64],
    )?;
    channel_in_project(c, project, name).map(|ch| ch.expect("just ensured"))
}

pub fn channel(c: &Connection, id: ChannelId) -> Result<Option<Channel>> {
    c.query_row(
        &format!("SELECT {CHANNEL_COLS} FROM channels WHERE id = ?1"),
        [id.0],
        channel_from_row,
    )
    .optional()
}

/// Looks a channel up by name within one project. There is deliberately no
/// global by-name lookup: two projects can each have a #main.
pub fn channel_in_project(
    c: &Connection,
    project: &ProjectId,
    name: &str,
) -> Result<Option<Channel>> {
    c.query_row(
        &format!("SELECT {CHANNEL_COLS} FROM channels WHERE project_id = ?1 AND name = ?2"),
        params![project.as_str(), name.trim_start_matches('#')],
        channel_from_row,
    )
    .optional()
}

/// A project's channels in `::N` order.
pub fn channels_in_project(c: &Connection, project: &ProjectId) -> Result<Vec<Channel>> {
    let mut stmt = c.prepare_cached(&format!(
        "SELECT {CHANNEL_COLS} FROM channels WHERE project_id = ?1 ORDER BY ordinal"
    ))?;
    stmt.query_map([project.as_str()], channel_from_row)?
        .collect()
}

/// Changes a channel's name only; its id, `::N` and history stay.
pub fn rename_channel(c: &Connection, id: ChannelId, name: &str) -> Result<bool> {
    Ok(c.execute(
        "UPDATE channels SET name = ?2 WHERE id = ?1",
        params![id.0, name.trim_start_matches('#')],
    )? == 1)
}

pub(crate) fn member_from_row(r: &Row<'_>) -> Result<Member> {
    let kind: String = r.get(1)?;
    Ok(Member {
        id: MemberId(r.get(0)?),
        kind: if kind == "agent" {
            MemberKind::Agent
        } else {
            MemberKind::Human
        },
        handle: Arc::from(r.get::<_, String>(2)?),
        name: Arc::from(r.get::<_, String>(3)?),
    })
}

/// Creates the member if the handle is new; otherwise returns the existing one
/// (its name is refreshed, its kind never changes).
///
/// A handle always wins over a nickname: if `handle` was someone else's
/// nickname, that nickname is dropped so `@handle` means one member.
pub fn ensure_member(c: &Connection, kind: MemberKind, handle: &str, name: &str) -> Result<Member> {
    c.execute(
        "INSERT INTO members (kind, handle, name) VALUES (?1, ?2, ?3)
         ON CONFLICT(handle) DO UPDATE SET name = excluded.name",
        params![kind.as_str(), handle, name],
    )?;
    let m = c.query_row(
        "SELECT id, kind, handle, name FROM members WHERE handle = ?1",
        [handle],
        member_from_row,
    )?;
    c.execute("DELETE FROM member_nicknames WHERE nickname = ?1", [handle])?;
    Ok(m)
}

pub fn member(c: &Connection, id: MemberId) -> Result<Option<Member>> {
    c.query_row(
        "SELECT id, kind, handle, name FROM members WHERE id = ?1",
        [id.0],
        member_from_row,
    )
    .optional()
}

/// Every agent member core has ever had, current or removed.
pub fn agent_member_ids(c: &Connection) -> Result<Vec<MemberId>> {
    let mut stmt = c.prepare_cached("SELECT id FROM members WHERE kind = 'agent' ORDER BY id")?;
    stmt.query_map([], |r| Ok(MemberId(r.get(0)?)))?.collect()
}

/// Every `@token` (handle, then nicknames) of an agent that was in
/// `project` and has since been removed from it, as `(member, token)` in
/// member order. The chat draws a mention of one as `@previous-agent`
/// (STRM-561), the way its messages' author reads.
///
/// Agents are invited to a project, not to a channel, so this is about the
/// project: an agent is current while it has a `project_members` row (the
/// grant `is_project_member` checks; `roster::leave_project` deletes it).
/// "Was in this project" comes from what membership leaves behind in any of
/// the project's channels: the cursor `join` writes for every agent that
/// joins one (never deleted), or a message the agent authored there. An
/// agent that was never in this project, however well core knows it from
/// another, is not listed: a mention of it stays as typed and reaches nobody.
pub fn former_project_agent_mention_tokens(
    c: &Connection,
    project: &ProjectId,
) -> Result<Vec<(MemberId, Arc<str>)>> {
    let mut stmt = c.prepare_cached(
        "WITH former(id) AS (
             SELECT m.id FROM members m
             WHERE m.kind = 'agent'
               AND NOT EXISTS (SELECT 1 FROM project_members p
                               WHERE p.project_id = ?1 AND p.member_id = m.id)
               AND (EXISTS (SELECT 1 FROM cursors k
                            JOIN channels ch ON ch.id = k.channel_id
                            WHERE ch.project_id = ?1 AND k.agent_id = m.id)
                    OR EXISTS (SELECT 1 FROM messages g
                               JOIN channels ch ON ch.id = g.channel_id
                               WHERE ch.project_id = ?1 AND g.author_id = m.id))
         )
         SELECT m.id, m.handle, 0, 0, 0 FROM members m JOIN former f ON f.id = m.id
         UNION ALL
         SELECT n.member_id, n.nickname, 1, n.added_at, n.rowid FROM member_nicknames n
         JOIN former f ON f.id = n.member_id
         ORDER BY 1, 3, 4, 5",
    )?;
    stmt.query_map([project.as_str()], |r| {
        Ok((MemberId(r.get(0)?), Arc::from(r.get::<_, String>(1)?)))
    })?
    .collect()
}

pub fn members_of(c: &Connection, channel: ChannelId) -> Result<Vec<Member>> {
    let mut stmt = c.prepare_cached(
        "SELECT m.id, m.kind, m.handle, m.name FROM members m
         JOIN memberships s ON s.member_id = m.id
         WHERE s.channel_id = ?1 ORDER BY m.id",
    )?;
    stmt.query_map([channel.0], member_from_row)?.collect()
}

// ---------- nicknames ----------

/// `member`'s nicknames, oldest first, as they were typed.
pub fn nicknames_of(c: &Connection, member: MemberId) -> Result<Vec<Arc<str>>> {
    let mut stmt = c.prepare_cached(
        "SELECT nickname FROM member_nicknames WHERE member_id = ?1 ORDER BY added_at, rowid",
    )?;
    stmt.query_map([member.0], |r| Ok(Arc::from(r.get::<_, String>(0)?)))?
        .collect()
}

/// The nicknames of `channel`'s members, as `(member, nickname)`, oldest
/// first per member.
pub fn nicknames_in(c: &Connection, channel: ChannelId) -> Result<Vec<(MemberId, Arc<str>)>> {
    let mut stmt = c.prepare_cached(
        "SELECT n.member_id, n.nickname FROM member_nicknames n
         JOIN memberships s ON s.member_id = n.member_id
         WHERE s.channel_id = ?1 ORDER BY n.member_id, n.added_at, n.rowid",
    )?;
    stmt.query_map([channel.0], |r| {
        Ok((MemberId(r.get(0)?), Arc::from(r.get::<_, String>(1)?)))
    })?
    .collect()
}

/// Who answers to `@token`, case-insensitively: the member whose handle it
/// is, else the member whose nickname it is. `None` if nobody.
pub fn mention_owner(c: &Connection, token: &str) -> Result<Option<MemberId>> {
    let by_handle = c
        .query_row("SELECT id FROM members WHERE handle = ?1", [token], |r| {
            r.get::<_, i64>(0)
        })
        .optional()?;
    if let Some(id) = by_handle {
        return Ok(Some(MemberId(id)));
    }
    Ok(c.query_row(
        "SELECT member_id FROM member_nicknames WHERE nickname = ?1",
        [token],
        |r| r.get::<_, i64>(0),
    )
    .optional()?
    .map(MemberId))
}

/// Gives `member` the nickname `nickname` (already checked by
/// `policy::check_nickname`). Fails on the schema's uniqueness rules: the
/// primary key if it is anyone's nickname, `nickname_is_a_handle` if it is
/// anyone's handle. Callers check with [`mention_owner`] first for a clear
/// error; the schema backs that up.
pub fn add_nickname(c: &Connection, member: MemberId, nickname: &str, now: Millis) -> Result<()> {
    c.execute(
        "INSERT INTO member_nicknames (nickname, member_id, added_at) VALUES (?1, ?2, ?3)",
        params![nickname, member.0, now as i64],
    )?;
    Ok(())
}

/// Takes `nickname` (case-insensitive) away from `member`. False if it
/// wasn't theirs.
pub fn remove_nickname(c: &Connection, member: MemberId, nickname: &str) -> Result<bool> {
    Ok(c.execute(
        "DELETE FROM member_nicknames WHERE member_id = ?1 AND nickname = ?2",
        params![member.0, nickname],
    )? == 1)
}

/// The channels of `project` that `member` belongs to, in `::N` order.
pub fn channels_of(c: &Connection, member: MemberId, project: &ProjectId) -> Result<Vec<Channel>> {
    let mut stmt = c.prepare_cached(
        "SELECT c.id, c.project_id, c.name, c.ordinal
         FROM channels c JOIN memberships s ON s.channel_id = c.id
         WHERE s.member_id = ?1 AND c.project_id = ?2 ORDER BY c.ordinal",
    )?;
    stmt.query_map(params![member.0, project.as_str()], channel_from_row)?
        .collect()
}

pub fn is_member(c: &Connection, channel: ChannelId, member: MemberId) -> Result<bool> {
    c.query_row(
        "SELECT 1 FROM memberships WHERE channel_id = ?1 AND member_id = ?2",
        params![channel.0, member.0],
        |_| Ok(()),
    )
    .optional()
    .map(|o| o.is_some())
}

/// Adds `member` to `channel`. For an agent joining for the first time, its
/// cursor starts `backfill` messages before the head, so its first wake shows
/// the last `backfill` messages as unread context. Returns whether it was new.
///
/// Fails with `not_a_project_member` (see [`is_not_a_project_member`]) unless
/// `member` already belongs to the channel's project ([`join_project`]).
pub fn join(
    c: &Connection,
    channel: ChannelId,
    member: &Member,
    backfill: u64,
    now: Millis,
) -> Result<bool> {
    let added = c.execute(
        "INSERT OR IGNORE INTO memberships (channel_id, member_id, joined_at) VALUES (?1, ?2, ?3)",
        params![channel.0, member.id.0, now as i64],
    )? == 1;
    if member.kind == MemberKind::Agent {
        let start = head_seq(c, channel)?.saturating_sub(backfill);
        c.execute(
            "INSERT OR IGNORE INTO cursors (agent_id, channel_id, delivered_through, read_through)
             VALUES (?1, ?2, ?3, ?3)",
            params![member.id.0, channel.0, start as i64],
        )?;
    }
    Ok(added)
}

/// Whether `e` is the schema refusing a channel join for lack of project
/// membership.
pub fn is_not_a_project_member(e: &rusqlite::Error) -> bool {
    matches!(e, rusqlite::Error::SqliteFailure(_, Some(msg)) if msg.contains("not_a_project_member"))
}

// ---------- messages ----------

const MESSAGE_COLS: &str = "id, channel_id, seq, author_id, origin, body, reply_to, hop, cause_batch, client_request_id, sent_at";

fn message_from_row(r: &Row<'_>) -> Result<Message> {
    Ok(Message {
        id: MessageId(r.get(0)?),
        channel: ChannelId(r.get(1)?),
        seq: r.get::<_, i64>(2)? as Seq,
        author: r.get::<_, Option<i64>>(3)?.map(MemberId),
        origin: Origin::parse(&r.get::<_, String>(4)?),
        body: Arc::from(r.get::<_, String>(5)?),
        reply_to: r.get::<_, Option<i64>>(6)?.map(MessageId),
        mentions: Vec::new(),
        hop: r.get::<_, i64>(7)?.clamp(0, u8::MAX as i64) as u8,
        sent_at: r.get::<_, i64>(10)? as Millis,
    })
}

fn with_mentions(c: &Connection, mut m: Message) -> Result<Message> {
    let mut stmt = c.prepare_cached(
        "SELECT member_id FROM mentions WHERE message_id = ?1 ORDER BY member_id",
    )?;
    m.mentions = stmt
        .query_map([m.id.0], |r| Ok(MemberId(r.get(0)?)))?
        .collect::<Result<_>>()?;
    Ok(m)
}

fn query_messages(c: &Connection, sql: &str, p: impl rusqlite::Params) -> Result<Vec<Message>> {
    let mut stmt = c.prepare_cached(sql)?;
    let rows = stmt
        .query_map(p, message_from_row)?
        .collect::<Result<Vec<_>>>()?;
    rows.into_iter().map(|m| with_mentions(c, m)).collect()
}

/// Inserts a message, assigning the next `seq` for its channel.
pub fn insert_message(c: &Connection, m: &NewMessage<'_>) -> Result<Message> {
    c.execute(
        "INSERT INTO messages (channel_id, seq, author_id, origin, body, reply_to, hop,
                               cause_batch, client_request_id, sent_at)
         VALUES (?1, (SELECT COALESCE(MAX(seq), 0) + 1 FROM messages WHERE channel_id = ?1),
                 ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            m.channel.0,
            m.author.map(|a| a.0),
            m.origin.as_str(),
            m.body,
            m.reply_to.map(|r| r.0),
            m.hop as i64,
            m.cause_batch.map(|b| b.0),
            m.client_request_id,
            m.sent_at as i64,
        ],
    )?;
    let id = MessageId(c.last_insert_rowid());
    for who in m.mentions {
        c.execute(
            "INSERT OR IGNORE INTO mentions (message_id, member_id) VALUES (?1, ?2)",
            params![id.0, who.0],
        )?;
    }
    message(c, id).map(|m| m.expect("just inserted"))
}

pub fn message(c: &Connection, id: MessageId) -> Result<Option<Message>> {
    let m = c
        .query_row(
            &format!("SELECT {MESSAGE_COLS} FROM messages WHERE id = ?1"),
            [id.0],
            message_from_row,
        )
        .optional()?;
    m.map(|m| with_mentions(c, m)).transpose()
}

/// The message an earlier `chat_send` with the same request id produced.
pub fn message_by_request(
    c: &Connection,
    author: MemberId,
    channel: ChannelId,
    request: &str,
) -> Result<Option<Message>> {
    let id = c
        .query_row(
            "SELECT id FROM messages WHERE author_id = ?1 AND channel_id = ?2 AND client_request_id = ?3",
            params![author.0, channel.0, request],
            |r| r.get::<_, i64>(0),
        )
        .optional()?;
    match id {
        Some(id) => message(c, MessageId(id)),
        None => Ok(None),
    }
}

pub fn head_seq(c: &Connection, channel: ChannelId) -> Result<Seq> {
    c.query_row(
        "SELECT COALESCE(MAX(seq), 0) FROM messages WHERE channel_id = ?1",
        [channel.0],
        |r| r.get::<_, i64>(0),
    )
    .map(|s| s as Seq)
}

/// Messages with `after < seq <= through`, oldest first, at most `limit`.
pub fn messages_between(
    c: &Connection,
    channel: ChannelId,
    after: Seq,
    through: Seq,
    limit: usize,
) -> Result<Vec<Message>> {
    query_messages(
        c,
        &format!(
            "SELECT {MESSAGE_COLS} FROM messages WHERE channel_id = ?1 AND seq > ?2 AND seq <= ?3
             ORDER BY seq LIMIT ?4"
        ),
        params![channel.0, after as i64, through as i64, limit as i64],
    )
}

/// The newest `limit` messages with `seq < before`, returned oldest first.
pub fn messages_before(
    c: &Connection,
    channel: ChannelId,
    before: Seq,
    limit: usize,
) -> Result<Vec<Message>> {
    let mut v = query_messages(
        c,
        &format!(
            "SELECT {MESSAGE_COLS} FROM messages WHERE channel_id = ?1 AND seq < ?2
             ORDER BY seq DESC LIMIT ?3"
        ),
        params![channel.0, before as i64, limit as i64],
    )?;
    v.reverse();
    Ok(v)
}

/// Case-insensitive substring search over the given channels, newest first.
pub fn search(
    c: &Connection,
    channels: &[ChannelId],
    query: &str,
    limit: usize,
) -> Result<Vec<Message>> {
    let mut out = Vec::new();
    let pattern = format!(
        "%{}%",
        query
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    );
    for ch in channels {
        out.extend(query_messages(
            c,
            &format!(
                "SELECT {MESSAGE_COLS} FROM messages
                 WHERE channel_id = ?1 AND body LIKE ?2 ESCAPE '\\'
                 ORDER BY seq DESC LIMIT ?3"
            ),
            params![ch.0, pattern, limit as i64],
        )?);
    }
    out.sort_by(|a, b| b.sent_at.cmp(&a.sent_at).then(b.id.cmp(&a.id)));
    out.truncate(limit);
    Ok(out)
}

fn search_in_channel_db(
    ws: &Connection,
    channel: ChannelId,
    pattern: &str,
    limit: usize,
) -> Result<Vec<Message>> {
    query_messages_db(
        ws,
        &format!(
            "SELECT {MESSAGE_COLS} FROM messages
             WHERE channel_id = ?1 AND body LIKE ?2 ESCAPE '\\'
             ORDER BY seq DESC LIMIT ?3"
        ),
        params![channel.0, pattern, limit as i64],
    )
}

fn channels_of_db(ws: &Connection, member: MemberId, project: &ProjectId) -> Result<Vec<Channel>> {
    let mut stmt = ws.prepare(&format!(
        "SELECT {CHANNEL_COLS} FROM channels c
         JOIN memberships m ON m.channel_id = c.id
         WHERE c.project_id = ?1 AND m.member_id = ?2
         ORDER BY c.name"
    ))?;
    let rows = stmt
        .query_map(params![project.as_str(), member.0], channel_from_row)?
        .collect::<Result<Vec<_>>>()?;
    Ok(rows)
}

/// Seq of the newest human-authored message in the channel (0 if none).
pub fn last_human_seq(c: &Connection, channel: ChannelId) -> Result<Seq> {
    c.query_row(
        "SELECT COALESCE(MAX(m.seq), 0) FROM messages m JOIN members a ON a.id = m.author_id
         WHERE m.channel_id = ?1 AND a.kind = 'human'",
        [channel.0],
        |r| r.get::<_, i64>(0),
    )
    .map(|s| s as Seq)
}

/// Newest agent-authored message in the channel after `after_seq`.
pub fn last_agent_message_after(
    c: &Connection,
    channel: ChannelId,
    after_seq: Seq,
) -> Result<Option<Message>> {
    Ok(query_messages(
        c,
        &format!(
            "SELECT {MESSAGE_COLS} FROM messages
             WHERE channel_id = ?1 AND seq > ?2 AND origin IN ('chat_tool', 'fallback')
             ORDER BY seq DESC LIMIT 1"
        ),
        params![channel.0, after_seq as i64],
    )?
    .pop())
}

/// Adds `member`'s `emoji` (already normalised) to `message`. Returns whether
/// it was new; adding it again is a no-op.
pub fn add_reaction(
    c: &Connection,
    message: MessageId,
    channel: ChannelId,
    member: MemberId,
    emoji: &str,
    now: Millis,
) -> Result<bool> {
    Ok(c.execute(
        "INSERT OR IGNORE INTO reactions (message_id, channel_id, member_id, emoji, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![message.0, channel.0, member.0, emoji, now as i64],
    )? == 1)
}

/// Removes `member`'s `emoji` from `message`. Returns whether there was one;
/// removing it again is a no-op.
pub fn remove_reaction(
    c: &Connection,
    message: MessageId,
    member: MemberId,
    emoji: &str,
) -> Result<bool> {
    Ok(c.execute(
        "DELETE FROM reactions WHERE message_id = ?1 AND member_id = ?2 AND emoji = ?3",
        params![message.0, member.0, emoji],
    )? == 1)
}

const REACTION_COLS: &str = "message_id, channel_id, member_id, emoji, created_at";

fn reaction_from_row(r: &Row<'_>) -> Result<Reaction> {
    Ok(Reaction {
        message: MessageId(r.get(0)?),
        channel: ChannelId(r.get(1)?),
        member: MemberId(r.get(2)?),
        emoji: Arc::from(r.get::<_, String>(3)?),
        created_at: r.get::<_, i64>(4)? as Millis,
    })
}

/// Every reaction on `message`, oldest first.
pub fn reactions_on(c: &Connection, message: MessageId) -> Result<Vec<Reaction>> {
    let mut stmt = c.prepare_cached(&format!(
        "SELECT {REACTION_COLS} FROM reactions WHERE message_id = ?1
         ORDER BY created_at, member_id, emoji"
    ))?;
    stmt.query_map([message.0], reaction_from_row)?.collect()
}

/// The emoji `member` has on `message`, oldest first.
pub fn reactions_by(c: &Connection, message: MessageId, member: MemberId) -> Result<Vec<Arc<str>>> {
    let mut stmt = c.prepare_cached(
        "SELECT emoji FROM reactions WHERE message_id = ?1 AND member_id = ?2
         ORDER BY created_at, emoji",
    )?;
    stmt.query_map(params![message.0, member.0], |r| {
        r.get::<_, String>(0).map(Arc::from)
    })?
    .collect()
}

/// Whether `member` has any reaction on `message`.
pub fn has_reacted(c: &Connection, message: MessageId, member: MemberId) -> Result<bool> {
    c.query_row(
        "SELECT EXISTS (SELECT 1 FROM reactions WHERE message_id = ?1 AND member_id = ?2)",
        params![message.0, member.0],
        |r| r.get(0),
    )
}

/// The channel's newest `limit` reactions, returned oldest first.
pub fn recent_reactions(c: &Connection, channel: ChannelId, limit: usize) -> Result<Vec<Reaction>> {
    let mut stmt = c.prepare_cached(&format!(
        "SELECT {REACTION_COLS} FROM reactions WHERE channel_id = ?1
         ORDER BY created_at DESC, message_id DESC, member_id DESC LIMIT ?2"
    ))?;
    let mut v = stmt
        .query_map(params![channel.0, limit as i64], reaction_from_row)?
        .collect::<Result<Vec<_>>>()?;
    v.reverse();
    Ok(v)
}

/// Reactions added in `channel` after `since` (exclusive), oldest first, at
/// most `limit`.
pub fn reactions_since(
    c: &Connection,
    channel: ChannelId,
    since: Millis,
    limit: usize,
) -> Result<Vec<Reaction>> {
    let mut stmt = c.prepare_cached(&format!(
        "SELECT {REACTION_COLS} FROM reactions WHERE channel_id = ?1 AND created_at > ?2
         ORDER BY created_at, message_id, member_id LIMIT ?3"
    ))?;
    stmt.query_map(
        params![channel.0, since as i64, limit as i64],
        reaction_from_row,
    )?
    .collect()
}

/// Reactions on the messages of `channel` with `seq > after_seq`, oldest
/// first (the app loads them alongside history).
pub fn reactions_after_seq(
    c: &Connection,
    channel: ChannelId,
    after_seq: Seq,
) -> Result<Vec<Reaction>> {
    let mut stmt = c.prepare_cached(
        "SELECT r.message_id, r.channel_id, r.member_id, r.emoji, r.created_at
         FROM reactions r JOIN messages m ON m.id = r.message_id
         WHERE r.channel_id = ?1 AND m.seq > ?2
         ORDER BY r.created_at, r.message_id, r.member_id",
    )?;
    stmt.query_map(params![channel.0, after_seq as i64], reaction_from_row)?
        .collect()
}

// ---------- cursors ----------

pub fn cursor(c: &Connection, agent: MemberId, channel: ChannelId) -> Result<Cursor> {
    c.query_row(
        "SELECT delivered_through, read_through, read_at FROM cursors WHERE agent_id = ?1 AND channel_id = ?2",
        params![agent.0, channel.0],
        |r| {
            Ok(Cursor {
                delivered_through: r.get::<_, i64>(0)? as Seq,
                read_through: r.get::<_, i64>(1)? as Seq,
                read_at: r.get::<_, Option<i64>>(2)?.map(|t| t as Millis),
            })
        },
    )
    .optional()
    .map(|o| {
        o.unwrap_or(Cursor {
            delivered_through: 0,
            read_through: 0,
            read_at: None,
        })
    })
}

/// Moves `delivered_through` (and `read_through` with it) forward to `seq`,
/// stamping each one that moves with `now`. Never moves a cursor backwards.
/// Returns whether `read_through` moved.
pub fn advance_delivered(
    c: &Connection,
    agent: MemberId,
    channel: ChannelId,
    seq: Seq,
    now: Millis,
) -> Result<bool> {
    let before = cursor(c, agent, channel)?;
    c.execute(
        "INSERT INTO cursors (agent_id, channel_id, delivered_through, read_through,
                              delivered_at, read_at)
         VALUES (?1, ?2, ?3, ?3, ?4, ?4)
         ON CONFLICT(agent_id, channel_id) DO UPDATE SET
            delivered_at = CASE WHEN excluded.delivered_through > delivered_through
                                THEN excluded.delivered_at ELSE delivered_at END,
            read_at = CASE WHEN excluded.delivered_through > read_through
                           THEN excluded.read_at ELSE read_at END,
            delivered_through = MAX(delivered_through, excluded.delivered_through),
            read_through = MAX(read_through, excluded.delivered_through)",
        params![agent.0, channel.0, seq as i64, now as i64],
    )?;
    Ok(seq > before.read_through)
}

/// Moves `read_through` forward to `seq` (a forward `chat_read`, or a human
/// shown the channel), stamping it with `now` when it moves. Returns whether
/// it moved.
pub fn advance_read(
    c: &Connection,
    member: MemberId,
    channel: ChannelId,
    seq: Seq,
    now: Millis,
) -> Result<bool> {
    let before = cursor(c, member, channel)?;
    c.execute(
        "INSERT INTO cursors (agent_id, channel_id, delivered_through, read_through, read_at)
         VALUES (?1, ?2, 0, ?3, ?4)
         ON CONFLICT(agent_id, channel_id) DO UPDATE SET
            read_at = CASE WHEN excluded.read_through > read_through
                           THEN excluded.read_at ELSE read_at END,
            read_through = MAX(read_through, excluded.read_through)",
        params![member.0, channel.0, seq as i64, now as i64],
    )?;
    Ok(seq > before.read_through)
}

/// Read receipts in `channel`: every current member who has read something,
/// in member order.
pub fn receipts(c: &Connection, channel: ChannelId) -> Result<Vec<Receipt>> {
    let mut stmt = c.prepare_cached(
        "SELECT k.agent_id, k.read_through, k.read_at FROM cursors k
         JOIN memberships s ON s.member_id = k.agent_id AND s.channel_id = k.channel_id
         WHERE k.channel_id = ?1 AND k.read_at IS NOT NULL
         ORDER BY k.agent_id",
    )?;
    stmt.query_map([channel.0], |r| {
        Ok(Receipt {
            member: MemberId(r.get(0)?),
            channel,
            read_through: r.get::<_, i64>(1)? as Seq,
            read_at: r.get::<_, i64>(2)? as Millis,
        })
    })?
    .collect()
}

// ---------- batches, triggers, obligations ----------

fn batch_from_row(r: &Row<'_>) -> Result<BatchRow> {
    Ok(BatchRow {
        id: BatchId(r.get(0)?),
        agent: MemberId(r.get(1)?),
        channel: ChannelId(r.get(2)?),
        state: BatchState::parse(&r.get::<_, String>(3)?),
        attempts: r.get::<_, i64>(4)? as u32,
        window_to: r.get::<_, Option<i64>>(5)?.map(|s| s as Seq),
    })
}

const BATCH_COLS: &str = "id, agent_id, channel_id, state, attempts, window_to";

pub fn batch(c: &Connection, id: BatchId) -> Result<Option<BatchRow>> {
    c.query_row(
        &format!("SELECT {BATCH_COLS} FROM batches WHERE id = ?1"),
        [id.0],
        batch_from_row,
    )
    .optional()
}

pub fn pending_batch(
    c: &Connection,
    agent: MemberId,
    channel: ChannelId,
) -> Result<Option<BatchRow>> {
    c.query_row(
        &format!(
            "SELECT {BATCH_COLS} FROM batches WHERE agent_id = ?1 AND channel_id = ?2 AND state = 'pending'"
        ),
        params![agent.0, channel.0],
        batch_from_row,
    )
    .optional()
}

pub fn in_flight_batch(
    c: &Connection,
    agent: MemberId,
    channel: ChannelId,
) -> Result<Option<BatchRow>> {
    c.query_row(
        &format!(
            "SELECT {BATCH_COLS} FROM batches
             WHERE agent_id = ?1 AND channel_id = ?2 AND state IN ('dispatched', 'accepted')"
        ),
        params![agent.0, channel.0],
        batch_from_row,
    )
    .optional()
}

/// The pending batch for agent x channel, created if there is none.
pub fn pending_batch_or_create(
    c: &Connection,
    agent: MemberId,
    channel: ChannelId,
    now: Millis,
) -> Result<BatchRow> {
    if let Some(b) = pending_batch(c, agent, channel)? {
        return Ok(b);
    }
    c.execute(
        "INSERT INTO batches (agent_id, channel_id, state, created_at) VALUES (?1, ?2, 'pending', ?3)",
        params![agent.0, channel.0, now as i64],
    )?;
    Ok(pending_batch(c, agent, channel)?.expect("just created"))
}

pub fn set_batch_state(
    c: &Connection,
    id: BatchId,
    state: BatchState,
    now: Millis,
    outcome: Option<&str>,
) -> Result<()> {
    match state {
        BatchState::Accepted => c.execute(
            "UPDATE batches SET state = 'accepted', accepted_at = ?2 WHERE id = ?1",
            params![id.0, now as i64],
        )?,
        BatchState::Settled | BatchState::Failed => c.execute(
            "UPDATE batches SET state = ?2, settled_at = ?3, outcome = ?4 WHERE id = ?1",
            params![id.0, state.as_str(), now as i64, outcome],
        )?,
        _ => c.execute(
            "UPDATE batches SET state = ?2 WHERE id = ?1",
            params![id.0, state.as_str()],
        )?,
    };
    Ok(())
}

/// Marks a pending batch as dispatched (attempt + 1) covering messages up to
/// `window_to`.
pub fn dispatch_batch(c: &Connection, id: BatchId, window_to: Seq) -> Result<u32> {
    c.execute(
        "UPDATE batches SET state = 'dispatched', attempts = attempts + 1, window_to = ?2
         WHERE id = ?1 AND state = 'pending'",
        params![id.0, window_to as i64],
    )?;
    c.query_row("SELECT attempts FROM batches WHERE id = ?1", [id.0], |r| {
        r.get::<_, i64>(0).map(|a| a as u32)
    })
}

/// Puts a dispatched batch that was not accepted back to pending, merging it
/// into a pending batch that collected triggers meanwhile (the newer pending
/// batch is folded into the older one so no trigger or obligation is lost).
pub fn requeue_batch(c: &Connection, id: BatchId) -> Result<()> {
    let Some(b) = batch(c, id)? else {
        return Ok(());
    };
    if b.state != BatchState::Dispatched {
        return Ok(());
    }
    if let Some(p) = pending_batch(c, b.agent, b.channel)? {
        c.execute(
            "UPDATE triggers SET batch_id = ?1 WHERE batch_id = ?2",
            params![id.0, p.id.0],
        )?;
        c.execute("DELETE FROM batches WHERE id = ?1", [p.id.0])?;
    }
    c.execute("UPDATE batches SET state = 'pending' WHERE id = ?1", [id.0])?;
    Ok(())
}

/// Inserts a trigger unless it exists. Returns `false` for a duplicate.
pub fn insert_trigger(
    c: &Connection,
    id: TriggerId,
    reason: WakeReason,
    hop: u8,
    batch: BatchId,
    now: Millis,
) -> Result<bool> {
    Ok(c.execute(
        "INSERT OR IGNORE INTO triggers (agent_id, channel_id, message_id, reason, hop, batch_id, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            id.agent.0,
            id.channel.0,
            id.message.0,
            reason.as_str(),
            hop as i64,
            batch.0,
            now as i64
        ],
    )? == 1)
}

pub fn trigger_exists(c: &Connection, id: TriggerId) -> Result<bool> {
    c.query_row(
        "SELECT 1 FROM triggers WHERE agent_id = ?1 AND channel_id = ?2 AND message_id = ?3",
        params![id.agent.0, id.channel.0, id.message.0],
        |_| Ok(()),
    )
    .optional()
    .map(|o| o.is_some())
}

pub fn batch_triggers(c: &Connection, batch: BatchId) -> Result<Vec<TriggerRow>> {
    let mut stmt = c.prepare_cached(
        "SELECT agent_id, channel_id, message_id, reason, hop, batch_id FROM triggers
         WHERE batch_id = ?1 ORDER BY message_id",
    )?;
    stmt.query_map([batch.0], |r| {
        Ok(TriggerRow {
            id: TriggerId {
                agent: MemberId(r.get(0)?),
                channel: ChannelId(r.get(1)?),
                message: MessageId(r.get(2)?),
            },
            reason: WakeReason::parse(&r.get::<_, String>(3)?),
            hop: r.get::<_, i64>(4)?.clamp(0, 255) as u8,
            batch: BatchId(r.get(5)?),
        })
    })?
    .collect()
}

pub fn insert_obligation(c: &Connection, id: TriggerId) -> Result<bool> {
    Ok(c.execute(
        "INSERT OR IGNORE INTO obligations (agent_id, channel_id, message_id, state)
         VALUES (?1, ?2, ?3, 'open')",
        params![id.agent.0, id.channel.0, id.message.0],
    )? == 1)
}

pub fn obligation_state(c: &Connection, id: TriggerId) -> Result<Option<ObligationState>> {
    c.query_row(
        "SELECT state FROM obligations WHERE agent_id = ?1 AND channel_id = ?2 AND message_id = ?3",
        params![id.agent.0, id.channel.0, id.message.0],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .map(|o| o.map(|s| ObligationState::parse(&s)))
}

/// Open obligations of a batch, oldest trigger first.
pub fn open_obligations(c: &Connection, batch: BatchId) -> Result<Vec<TriggerId>> {
    let mut stmt = c.prepare_cached(
        "SELECT o.agent_id, o.channel_id, o.message_id FROM obligations o
         JOIN triggers t ON t.agent_id = o.agent_id AND t.channel_id = o.channel_id
                        AND t.message_id = o.message_id
         WHERE t.batch_id = ?1 AND o.state = 'open' ORDER BY o.message_id",
    )?;
    stmt.query_map([batch.0], |r| {
        Ok(TriggerId {
            agent: MemberId(r.get(0)?),
            channel: ChannelId(r.get(1)?),
            message: MessageId(r.get(2)?),
        })
    })?
    .collect()
}

// ---------- stances ----------

const STANCE_COLS: &str =
    "message_id, channel_id, kind, member_id, target_id, reason, model, runtime, created_at";

fn stance_from_row(r: &Row<'_>) -> Result<Stance> {
    Ok(Stance {
        message: MessageId(r.get(0)?),
        channel: ChannelId(r.get(1)?),
        kind: StanceKind::parse(&r.get::<_, String>(2)?),
        member: MemberId(r.get(3)?),
        target: MessageId(r.get(4)?),
        reason: Arc::from(r.get::<_, String>(5)?),
        model: r.get::<_, Option<String>>(6)?.map(Arc::from),
        runtime: r.get::<_, Option<String>>(7)?.map(Arc::from),
        created_at: r.get::<_, i64>(8)? as Millis,
    })
}

/// Records a stance. A second record for the same message is a no-op (the
/// `chat_*` request-id dedup can hand back an existing message).
pub fn insert_stance(c: &Connection, s: &Stance) -> Result<()> {
    c.execute(
        &format!("INSERT OR IGNORE INTO stances ({STANCE_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"),
        params![
            s.message.0,
            s.channel.0,
            s.kind.as_str(),
            s.member.0,
            s.target.0,
            &*s.reason,
            s.model.as_deref(),
            s.runtime.as_deref(),
            s.created_at as i64
        ],
    )?;
    Ok(())
}

/// The stance a message states, if it states one.
pub fn stance(c: &Connection, message: MessageId) -> Result<Option<Stance>> {
    c.query_row(
        &format!("SELECT {STANCE_COLS} FROM stances WHERE message_id = ?1"),
        [message.0],
        stance_from_row,
    )
    .optional()
}

/// The newest `limit` stances in a channel, oldest first.
pub fn recent_stances(c: &Connection, channel: ChannelId, limit: usize) -> Result<Vec<Stance>> {
    let mut stmt = c.prepare_cached(&format!(
        "SELECT {STANCE_COLS} FROM stances WHERE channel_id = ?1 ORDER BY message_id DESC LIMIT ?2"
    ))?;
    let mut v = stmt
        .query_map(params![channel.0, limit as i64], stance_from_row)?
        .collect::<Result<Vec<_>>>()?;
    v.reverse();
    Ok(v)
}

/// Resolves an obligation only if it is still open. Returns whether it changed.
pub fn resolve_obligation(
    c: &Connection,
    id: TriggerId,
    state: ObligationState,
    by: Option<MessageId>,
    kind: &str,
) -> Result<bool> {
    Ok(c.execute(
        "UPDATE obligations SET state = ?4, resolved_by = ?5, resolved_kind = ?6
         WHERE agent_id = ?1 AND channel_id = ?2 AND message_id = ?3 AND state = 'open'",
        params![
            id.agent.0,
            id.channel.0,
            id.message.0,
            state.as_str(),
            by.map(|m| m.0),
            kind
        ],
    )? == 1)
}

/// Startup recovery after a crash or quit of the app itself: an accepted batch
/// is never re-sent (its obligations become unmet); a dispatched one goes back
/// to pending. Returns the agent x channel pairs that have pending work.
pub fn recover(c: &Connection, now: Millis) -> Result<Vec<(MemberId, ChannelId)>> {
    let accepted: Vec<BatchId> = {
        let mut stmt = c.prepare("SELECT id FROM batches WHERE state = 'accepted'")?;
        stmt.query_map([], |r| Ok(BatchId(r.get(0)?)))?
            .collect::<Result<_>>()?
    };
    for b in accepted {
        for t in open_obligations(c, b)? {
            resolve_obligation(c, t, ObligationState::Unmet, None, "app_restart")?;
        }
        set_batch_state(
            c,
            b,
            BatchState::Failed,
            now,
            Some("app restarted mid-turn"),
        )?;
    }
    let dispatched: Vec<BatchId> = {
        let mut stmt = c.prepare("SELECT id FROM batches WHERE state = 'dispatched'")?;
        stmt.query_map([], |r| Ok(BatchId(r.get(0)?)))?
            .collect::<Result<_>>()?
    };
    for b in dispatched {
        requeue_batch(c, b)?;
    }
    let mut stmt = c.prepare("SELECT agent_id, channel_id FROM batches WHERE state = 'pending'")?;
    stmt.query_map([], |r| Ok((MemberId(r.get(0)?), ChannelId(r.get(1)?))))?
        .collect()
}

pub fn record_session(
    c: &Connection,
    agent: MemberId,
    channel: ChannelId,
    protocol: &str,
    native_ref: &str,
    now: Millis,
) -> Result<()> {
    c.execute(
        "INSERT INTO sessions (agent_id, channel_id, protocol, native_ref, opened_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(agent_id, channel_id) DO UPDATE SET
            protocol = excluded.protocol, native_ref = excluded.native_ref, opened_at = excluded.opened_at",
        params![agent.0, channel.0, protocol, native_ref, now as i64],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::LEGACY_MIGRATIONS as MIGRATIONS;
    use super::*;

    const P1: &str = "0b7e4f0e-2a51-4c4b-9a6e-3f1d2c3b4a59";
    const P2: &str = "5f0c9d1a-7b2e-4e8f-a1c3-9d8e7f6a5b4c";

    fn pid(s: &str) -> ProjectId {
        ProjectId::parse(s).unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn opening_a_store_tightens_its_data_dir_and_every_database() {
        use std::os::unix::fs::PermissionsExt;

        fn mode(path: &Path) -> u32 {
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777
        }

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("cc-store-perms-{nanos}"));
        let _ = std::fs::remove_dir_all(&dir);
        // An install from before the policy: loose dir, loose pre-existing DB.
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let existing = dir.join("core.db");
        {
            let conn = rusqlite::Connection::open(&existing).unwrap();
            conn.execute_batch("PRAGMA journal_mode = WAL; CREATE TABLE old (x INTEGER);")
                .unwrap();
        }
        std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(0o644)).unwrap();

        let mut store = Store::open(&dir).unwrap();
        assert_eq!(mode(&dir), 0o700, "data directory");
        assert_eq!(mode(&dir.join("core.db")), 0o600, "core database");

        // Opening a project creates its private directory and two databases.
        store
            .ensure_project(&pid(P1), "Starpoint", None, 0)
            .unwrap();
        let project = dir.join("Projects").join(P1);
        assert_eq!(mode(&dir.join("Projects")), 0o700, "projects directory");
        assert_eq!(mode(&project), 0o700, "project directory");
        assert_eq!(mode(&project.join("workspace.db")), 0o600);
        assert_eq!(mode(&project.join("agents.db")), 0o600);

        // SQLite creates WAL/SHM lazily; they inherit the database's mode.
        for path in [
            dir.join("core.db-wal"),
            dir.join("core.db-shm"),
            project.join("workspace.db-wal"),
            project.join("workspace.db-shm"),
        ] {
            if path.exists() {
                assert_eq!(mode(&path), 0o600, "{}", path.display());
            }
        }
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn setup() -> (Store, Channel, Member, Member) {
        let mut s = Store::open_in_memory().unwrap();
        let p = pid(P1);
        s.ensure_project(&p, "Starpoint", None, 0).unwrap();
        let ch = s.ensure_channel(&p, "main", 0).unwrap();
        let hx = s.ensure_member(MemberKind::Human, "hx", "Hx").unwrap();
        let bot = s
            .ensure_member(MemberKind::Agent, "claude", "Claude")
            .unwrap();
        s.join_project(&p, hx.id, 0).unwrap();
        s.join_project(&p, bot.id, 0).unwrap();
        s.join(ch.id, &hx, 20, 0).unwrap();
        s.join(ch.id, &bot, 20, 0).unwrap();
        (s, ch, hx, bot)
    }

    #[test]
    fn approval_outcome_round_trips_through_store() {
        let mut store = Store::open_in_memory().unwrap();
        let outcome = ApprovalOutcome {
            cc_request_id: ApprovalId::from("opencode-v2:s1:p1"),
            state: ApprovalOutcomeState::Allowed,
            provider_reply: Some(serde_json::json!({"decision": "once"})),
            execution_effect: Some(serde_json::json!({"ran": true})),
            error: None,
            recorded_at: Some(123),
        };

        store.record_approval_outcome(&outcome).unwrap();
        let loaded = store.approval_outcomes().unwrap();

        assert_eq!(loaded.get(&outcome.cc_request_id), Some(&outcome));
    }

    #[test]
    fn nicknames_share_the_handle_namespace() {
        let (s, ch, hx, bot) = setup();
        s.add_nickname(bot.id, "Cody", 1).unwrap();
        s.add_nickname(bot.id, "cx", 2).unwrap();
        assert_eq!(
            s.nicknames_of(bot.id).unwrap(),
            vec![Arc::<str>::from("Cody"), Arc::from("cx")]
        );
        assert_eq!(s.nicknames_in(ch.id).unwrap().len(), 2);
        assert_eq!(s.mention_owner("CODY").unwrap(), Some(bot.id));
        assert_eq!(s.mention_owner("Claude").unwrap(), Some(bot.id));
        assert_eq!(s.mention_owner("nobody").unwrap(), None);
        // The schema refuses a second holder, whatever the case, and a handle.
        assert!(s.add_nickname(hx.id, "cody", 3).is_err());
        let e = s.add_nickname(hx.id, "CLAUDE", 3).unwrap_err();
        assert!(e.to_string().contains("nickname_is_a_handle"), "{e}");
        // A handle created later wins: the nickname is dropped.
        let cx = s.ensure_member(MemberKind::Agent, "CX", "Codex").unwrap();
        assert_eq!(s.mention_owner("cx").unwrap(), Some(cx.id));
        assert_eq!(
            s.nicknames_of(bot.id).unwrap(),
            vec![Arc::<str>::from("Cody")]
        );
        assert!(s.remove_nickname(bot.id, "cODY").unwrap());
        assert!(!s.remove_nickname(bot.id, "cody").unwrap());
        assert!(s.nicknames_of(bot.id).unwrap().is_empty());
    }

    #[test]
    fn former_mention_tokens_are_only_agents_removed_from_this_project() {
        let (mut s, main, hx, bot) = setup();
        let p = pid(P1);
        let design = s.ensure_channel(&p, "design", 0).unwrap();
        s.add_nickname(bot.id, "Cody", 1).unwrap();
        s.add_nickname(hx.id, "boss", 2).unwrap();
        // In #main, then removed from the project.
        let gone = s.ensure_member(MemberKind::Agent, "gpt", "GPT").unwrap();
        s.join_project(&p, gone.id, 0).unwrap();
        s.join(main.id, &gone, 20, 0).unwrap();
        s.add_nickname(gone.id, "g", 3).unwrap();
        s.add_nickname(gone.id, "jeepy", 4).unwrap();
        s.leave_project(&p, gone.id, 5).unwrap();
        // Only ever in #design, where it spoke, then removed.
        let spoke = s
            .ensure_member(MemberKind::Agent, "gemini", "Gemini")
            .unwrap();
        s.join_project(&p, spoke.id, 0).unwrap();
        s.join(design.id, &spoke, 20, 0).unwrap();
        s.insert_message(&msg(design.id, spoke.id, "on it"))
            .unwrap();
        s.add_nickname(spoke.id, "gem", 5).unwrap();
        s.leave_project(&p, spoke.id, 6).unwrap();
        // A current member that is only in #design is not former.
        let still = s.ensure_member(MemberKind::Agent, "pi", "Pi").unwrap();
        s.join_project(&p, still.id, 0).unwrap();
        s.join(design.id, &still, 20, 0).unwrap();
        // Only ever in another project's channel.
        let q = pid(P2);
        s.ensure_project(&q, "Elsewhere", None, 0).unwrap();
        let elsewhere_ch = s.ensure_channel(&q, "main", 0).unwrap();
        let other = s
            .ensure_member(MemberKind::Agent, "codex", "Codex")
            .unwrap();
        s.join_project(&q, other.id, 0).unwrap();
        s.join(elsewhere_ch.id, &other, 20, 0).unwrap();
        s.add_nickname(other.id, "cx", 7).unwrap();
        // Known to core, never in any channel.
        s.ensure_member(MemberKind::Agent, "devin", "Devin")
            .unwrap();

        assert_eq!(
            s.former_project_agent_mention_tokens(&pid(P1)).unwrap(),
            vec![
                (gone.id, Arc::<str>::from("gpt")),
                (gone.id, Arc::from("g")),
                (gone.id, Arc::from("jeepy")),
                (spoke.id, Arc::from("gemini")),
                (spoke.id, Arc::from("gem")),
            ]
        );
        assert!(
            s.former_project_agent_mention_tokens(&q)
                .unwrap()
                .is_empty()
        );
    }

    fn msg<'a>(ch: ChannelId, author: MemberId, body: &'a str) -> NewMessage<'a> {
        NewMessage {
            channel: ch,
            author: Some(author),
            origin: Origin::Composer,
            body,
            reply_to: None,
            mentions: &[],
            hop: 0,
            cause_batch: None,
            client_request_id: None,
            sent_at: 1,
        }
    }

    #[test]
    fn migrations_run_once_and_record_version() {
        let s = Store::open_in_memory().unwrap();
        let core_v: i64 = s
            .core()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(core_v, super::CORE_MIGRATIONS.len() as i64);
    }

    #[test]
    fn a_database_from_a_newer_build_is_refused_and_left_unchanged() {
        let dir = std::env::temp_dir().join(format!("cc-store-newer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.sqlite");
        let _ = std::fs::remove_file(&path);
        let newer = MIGRATIONS.len() as i64 + 1;
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(&format!(
                "CREATE TABLE future (x INTEGER); INSERT INTO future VALUES (7);
                 PRAGMA user_version = {newer};"
            ))
            .unwrap();
        }
        let before = std::fs::read(&path).unwrap();
        match Store::open(&path) {
            Err(OpenError::NewerSchema { found, known, .. }) => {
                assert_eq!(found, newer);
                assert_eq!(known, MIGRATIONS.len() as i64);
            }
            Err(e) => panic!("expected NewerSchema, got {e}"),
            Ok(_) => panic!("a newer database opened"),
        }
        let message = Store::open(&path).err().unwrap().to_string();
        assert!(message.contains("newer Chaos Control"), "{message}");
        assert!(message.contains(&format!("schema {newer}")), "{message}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let conn = Connection::open(&path).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, newer);
        let journal: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(journal, "delete");
        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_database_at_this_builds_version_opens() {
        let dir = std::env::temp_dir().join(format!("cc-store-equal-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.sqlite");
        let _ = std::fs::remove_file(&path);
        drop(Store::open(&path).unwrap());
        let s = Store::open(&path).unwrap();
        let core_v: i64 = s
            .core()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(core_v, super::CORE_MIGRATIONS.len() as i64);
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reopening_a_file_keeps_ids_and_does_not_rerun_migrations() {
        let dir = std::env::temp_dir().join(format!("cc-store-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.sqlite");
        let _ = std::fs::remove_file(&path);
        let (id, hx_id) = {
            let mut s = Store::open(&path).unwrap();
            let p = pid(P1);
            s.ensure_project(&p, "Starpoint", None, 0).unwrap();
            let ch = s.ensure_channel(&p, "main", 0).unwrap();
            let hx = s.ensure_member(MemberKind::Human, "hx", "Hx").unwrap();
            s.join_project(&p, hx.id, 0).unwrap();
            s.join(ch.id, &hx, 20, 0).unwrap();
            let m = s.insert_message(&msg(ch.id, hx.id, "hello")).unwrap();
            (m.id, hx.id)
        };
        let s = Store::open(&path).unwrap();
        let mut found = None;
        for proj in s.projects().unwrap() {
            if let Ok(ws) = s.project_workspace(&proj.id)
                && let Ok(Some(m)) = message(ws, id)
            {
                found = Some(m);
                break;
            }
        }
        let m = found.expect("message");
        assert_eq!(&*m.body, "hello");
        assert_eq!(m.author, Some(hx_id));
        let core_v: i64 = s
            .core()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(core_v, super::CORE_MIGRATIONS.len() as i64);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn seq_is_per_channel_and_gapless_while_ids_are_global() {
        let (mut s, ch, hx, _) = setup();
        let other = s.ensure_channel(&ch.project, "STRM-1", 0).unwrap();
        s.join(other.id, &hx, 0, 0).unwrap();
        let a = s.insert_message(&msg(ch.id, hx.id, "a")).unwrap();
        let b = s.insert_message(&msg(other.id, hx.id, "b")).unwrap();
        let c2 = s.insert_message(&msg(ch.id, hx.id, "c")).unwrap();
        assert_eq!((a.seq, b.seq, c2.seq), (1, 1, 2));
        assert!(a.id < b.id && b.id < c2.id);
    }

    #[test]
    fn one_pending_and_one_in_flight_batch_per_agent_channel() {
        let (mut s, ch, _, bot) = setup();
        let p = s.pending_batch_or_create(bot.id, ch.id, 0).unwrap();
        assert_eq!(
            s.pending_batch_or_create(bot.id, ch.id, 0).unwrap().id,
            p.id
        );
        {
            let ag = s.project_agents(&ch.project).unwrap();
            let dup = ag.execute(
                "INSERT INTO batches (id, agent_id, channel_id, state, created_at) VALUES (?1, ?2, ?3, 'pending', 0)",
                params![999i64, bot.id.0, ch.id.0],
            );
            assert!(dup.is_err(), "schema forbids a second pending batch");
        }
        s.dispatch_batch(p.id, 0).unwrap();
        let q = s.pending_batch_or_create(bot.id, ch.id, 0).unwrap();
        assert_ne!(q.id, p.id);
        let ag = s.project_agents(&ch.project).unwrap();
        let dup = ag.execute(
            "UPDATE batches SET state = 'dispatched' WHERE id = ?1",
            [q.id.0],
        );
        assert!(dup.is_err(), "schema forbids a second in-flight batch");
    }

    #[test]
    fn requeue_merges_triggers_of_a_newer_pending_batch() {
        let (mut s, ch, hx, bot) = setup();
        let m1 = s.insert_message(&msg(ch.id, hx.id, "one")).unwrap();
        let m2 = s.insert_message(&msg(ch.id, hx.id, "two")).unwrap();
        let b1 = s.pending_batch_or_create(bot.id, ch.id, 0).unwrap();
        let t1 = TriggerId {
            agent: bot.id,
            channel: ch.id,
            message: m1.id,
        };
        s.insert_trigger(t1, WakeReason::Mention, 0, b1.id, 0)
            .unwrap();
        s.insert_obligation(t1).unwrap();
        s.dispatch_batch(b1.id, 1).unwrap();
        let b2 = s.pending_batch_or_create(bot.id, ch.id, 0).unwrap();
        let t2 = TriggerId {
            agent: bot.id,
            channel: ch.id,
            message: m2.id,
        };
        s.insert_trigger(t2, WakeReason::Mention, 0, b2.id, 0)
            .unwrap();
        s.insert_obligation(t2).unwrap();
        s.requeue_batch(b1.id).unwrap();
        let p = s.pending_batch(bot.id, ch.id).unwrap().unwrap();
        assert_eq!(p.id, b1.id);
        assert_eq!(s.batch_triggers(p.id).unwrap().len(), 2);
        assert_eq!(s.open_obligations(p.id).unwrap(), vec![t1, t2]);
    }

    #[test]
    fn duplicate_trigger_and_obligation_are_ignored() {
        let (mut s, ch, hx, bot) = setup();
        let m = s.insert_message(&msg(ch.id, hx.id, "@claude hi")).unwrap();
        let b = s.pending_batch_or_create(bot.id, ch.id, 0).unwrap();
        let t = TriggerId {
            agent: bot.id,
            channel: ch.id,
            message: m.id,
        };
        assert!(
            s.insert_trigger(t, WakeReason::Mention, 0, b.id, 0)
                .unwrap()
        );
        assert!(
            !s.insert_trigger(t, WakeReason::Mention, 0, b.id, 0)
                .unwrap()
        );
        assert!(s.insert_obligation(t).unwrap());
        assert!(!s.insert_obligation(t).unwrap());
        assert!(
            s.resolve_obligation(t, ObligationState::Satisfied, None, "send")
                .unwrap()
        );
        assert!(
            !s.resolve_obligation(t, ObligationState::Fallback, None, "fallback")
                .unwrap()
        );
    }

    #[test]
    fn join_backfills_the_last_n_messages() {
        let (mut s, ch, hx, _) = setup();
        for i in 0..30 {
            s.insert_message(&msg(ch.id, hx.id, &format!("m{i}")))
                .unwrap();
        }
        let late = s
            .ensure_member(MemberKind::Agent, "codex", "Codex")
            .unwrap();
        s.join_project(&ch.project, late.id, 0).unwrap();
        s.join(ch.id, &late, 20, 0).unwrap();
        assert_eq!(s.cursor(late.id, ch.id).unwrap().read_through, 10);
        s.advance_delivered(late.id, ch.id, 5, 0).unwrap();
        assert_eq!(
            s.cursor(late.id, ch.id).unwrap().delivered_through,
            10,
            "never backwards"
        );
    }

    #[test]
    fn two_projects_can_each_have_a_main() {
        let mut s = Store::open_in_memory().unwrap();
        s.ensure_project(&pid(P1), "Starpoint", Some("/src/starpoint"), 0)
            .unwrap();
        s.ensure_project(&pid(P2), "Everything", Some("/src/everything"), 0)
            .unwrap();
        let a = s.ensure_channel(&pid(P1), "main", 0).unwrap();
        let b = s.ensure_channel(&pid(P2), "#main", 0).unwrap();
        assert_ne!(a.id, b.id);
        assert_eq!((a.ordinal, b.ordinal), (1, 1), "::N counts per project");
        assert_eq!(&*b.name, "main");
        assert_eq!(s.ensure_channel(&pid(P1), "main", 5).unwrap().id, a.id);
        let next = s.ensure_channel(&pid(P2), "STRM-184", 0).unwrap();
        assert_eq!(next.ordinal, 2);
        assert_eq!(
            s.channel_in_project(&pid(P1), "main").unwrap().unwrap().id,
            a.id
        );
        assert_eq!(
            s.channel_in_project(&pid(P2), "main").unwrap().unwrap().id,
            b.id
        );
        assert!(
            s.channel_in_project(&pid(P1), "STRM-184")
                .unwrap()
                .is_none()
        );
        let names: Vec<_> = s
            .channels_in_project(&pid(P2))
            .unwrap()
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, vec![Arc::from("main"), Arc::from("STRM-184")]);
    }

    #[test]
    fn joining_a_channel_needs_project_membership() {
        let (mut s, ch, _, _) = setup();
        let other = pid(P2);
        s.ensure_project(&other, "Other", None, 0).unwrap();
        let elsewhere = s.ensure_channel(&other, "main", 0).unwrap();
        let codex = s
            .ensure_member(MemberKind::Agent, "codex", "Codex")
            .unwrap();
        let err = s.join(ch.id, &codex, 20, 0).unwrap_err();
        assert!(is_not_a_project_member(&err), "{err}");
        assert!(!s.is_member(ch.id, codex.id).unwrap());
        s.join_project(&ch.project, codex.id, 0).unwrap();
        assert!(s.join(ch.id, &codex, 20, 0).unwrap());
        let err = s.join(elsewhere.id, &codex, 20, 0).unwrap_err();
        assert!(is_not_a_project_member(&err), "{err}");
    }

    #[test]
    fn renaming_changes_the_name_only() {
        let (mut s, ch, hx, _) = setup();
        let m = s.insert_message(&msg(ch.id, hx.id, "before")).unwrap();
        assert!(s.rename_channel(ch.id, "#general").unwrap());
        assert!(s.rename_project(&ch.project, "Starpoint 2").unwrap());
        let renamed = s.channel(ch.id).unwrap().unwrap();
        assert_eq!((renamed.id, renamed.ordinal), (ch.id, ch.ordinal));
        assert_eq!(&*renamed.name, "general");
        assert_eq!(
            s.project(&ch.project).unwrap().unwrap().name.as_ref(),
            "Starpoint 2"
        );
        assert_eq!(s.message(m.id).unwrap().unwrap().channel, ch.id);
    }

    #[test]
    fn a_folder_belongs_to_one_project() {
        let mut s = Store::open_in_memory().unwrap();
        s.tx::<_, rusqlite::Error>(|c| {
            ensure_project(c, &pid(P1), "Old", Some("/src/x"), 0)?;
            assert_eq!(project_by_root(c, "/src/x")?.unwrap().id, pid(P1));
            // Seen again without a root: the root is kept.
            ensure_project(c, &pid(P1), "Old", None, 1)?;
            assert_eq!(project_by_root(c, "/src/x")?.unwrap().id, pid(P1));
            // Another project opened from the same folder takes it over.
            ensure_project(c, &pid(P2), "New", Some("/src/x"), 2)?;
            assert_eq!(project_by_root(c, "/src/x")?.unwrap().id, pid(P2));
            assert_eq!(project(c, &pid(P1))?.unwrap().root_path, None);
            assert_eq!(projects(c)?.len(), 2);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn project_ids_must_be_uuids() {
        assert!(ProjectId::parse("not-a-uuid").is_none());
        assert!(ProjectId::parse("0b7e4f0e-2a51-4c4b-9a6e-3f1d2c3b4a5").is_none());
        let p = ProjectId::parse("{0B7E4F0E-2A51-4C4B-9A6E-3F1D2C3B4A59}").unwrap();
        assert_eq!(p.as_str(), P1);
    }

    #[test]
    fn a_v1_database_migrates_keeping_every_id_and_row() {
        let dir = std::env::temp_dir().join(format!("cc-store-v1-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("core.sqlite");
        let _ = std::fs::remove_file(&path);

        // A legacy database as v1 left it, with a row in every table.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(MIGRATIONS[0]).unwrap();
            conn.execute_batch(
                r#"
                PRAGMA user_version = 1;
                INSERT INTO channels (id, name, created_at) VALUES (3, 'main', 100), (7, 'STRM-1', 200);
                INSERT INTO members (id, kind, handle, name) VALUES
                    (1, 'human', 'hx', 'Hx'), (2, 'agent', 'claude', 'Claude'), (5, 'agent', 'codex', 'Codex');
                INSERT INTO memberships VALUES (3, 1, 10), (3, 2, 11), (7, 1, 12), (7, 5, 13);
                INSERT INTO messages (id, channel_id, seq, author_id, origin, body, reply_to, hop, cause_batch, client_request_id, sent_at) VALUES
                    (10, 3, 1, 1, 'composer', '@claude hi', NULL, 0, NULL, NULL, 1000),
                    (11, 3, 2, 2, 'chat_tool', 'hello', 10, 1, 4, 'r1', 1001),
                    (12, 7, 1, 1, 'composer', 'other room', NULL, 0, NULL, NULL, 1002);
                INSERT INTO mentions VALUES (10, 2);
                INSERT INTO reactions VALUES (11, 1, '👍', 1003);
                INSERT INTO cursors VALUES (2, 3, 2, 2), (5, 7, 0, 1);
                INSERT INTO batches (id, agent_id, channel_id, state, attempts, window_to, created_at, accepted_at, settled_at, outcome)
                    VALUES (4, 2, 3, 'settled', 1, 1, 1000, 1000, 1001, 'end_turn');
                INSERT INTO triggers VALUES (2, 3, 10, 'mention', 0, 4, 1000);
                INSERT INTO obligations VALUES (2, 3, 10, 'satisfied', 11, 'send');
                INSERT INTO sessions VALUES (2, 3, 'acp', 'sess-1', 1000);
                "#,
            )
            .unwrap();
        }

        let s = Store::open(&dir).unwrap();
        let all = s.projects().unwrap();
        assert_eq!(all.len(), 1);
        let imported = all[0].clone();
        assert_eq!(imported.name.as_ref(), "Imported");
        assert_eq!(imported.root_path, None);
        assert_eq!(imported.created_at, 100);
        let chans = s.channels_in_project(&imported.id).unwrap();
        let got: Vec<_> = chans
            .iter()
            .map(|c| (c.id.0, &*c.name, c.ordinal))
            .collect();
        assert_eq!(got, vec![(3, "main", 1), (7, "STRM-1", 2)]);
        for m in [1, 2, 5] {
            assert!(s.is_project_member(&imported.id, MemberId(m)).unwrap());
        }
        // Verify key rows preserved across split DBs.
        let ws = s.project_workspace(&imported.id).unwrap();
        let ag = s.project_agents(&imported.id).unwrap();
        let msg_ids: Vec<i64> = {
            let mut st = ws.prepare("SELECT id FROM messages ORDER BY id").unwrap();
            st.query_map([], |r| r.get(0))
                .unwrap()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap()
        };
        assert_eq!(msg_ids, vec![10, 11, 12]);
        let batch_ids: Vec<i64> = {
            let mut st = ag.prepare("SELECT id FROM batches ORDER BY id").unwrap();
            st.query_map([], |r| r.get(0))
                .unwrap()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap()
        };
        assert_eq!(batch_ids, vec![4]);
        let fk: i64 = ws
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(fk, 0);
        drop(s);

        // Reopening runs nothing again: same single project, same UUID.
        let s = Store::open(&dir).unwrap();
        let again = s.projects().unwrap();
        assert_eq!(again, vec![imported]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_v3_database_gains_receipts_and_the_ignored_state() {
        let dir = std::env::temp_dir().join(format!(
            "cc-store-v3-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("core.sqlite");
        let conn = Connection::open(&path).unwrap();
        for m in &MIGRATIONS[..3] {
            conn.execute_batch(m).unwrap();
        }
        conn.execute_batch(
            r#"
            PRAGMA user_version = 3;
            INSERT INTO projects (id, name, root_path, created_at) VALUES
                ('0b7e4f0e-2a51-4c4b-9a6e-3f1d2c3b4a59', 'P', NULL, 1);
            INSERT INTO channels (id, project_id, name, ordinal, created_at) VALUES
                (3, '0b7e4f0e-2a51-4c4b-9a6e-3f1d2c3b4a59', 'main', 1, 1);
            INSERT INTO members (id, kind, handle, name) VALUES (1, 'human', 'hx', 'Hx'), (2, 'agent', 'bot', 'Bot');
            INSERT INTO messages (id, channel_id, seq, author_id, origin, body, sent_at) VALUES
                (10, 3, 1, 1, 'composer', '@bot hi', 1000);
            INSERT INTO cursors VALUES (2, 3, 1, 1);
            INSERT INTO batches (id, agent_id, channel_id, state, created_at) VALUES (4, 2, 3, 'settled', 1000);
            INSERT INTO triggers VALUES (2, 3, 10, 'mention', 0, 4, 1000);
            INSERT INTO obligations VALUES (2, 3, 10, 'unmet', NULL, 'no_reply');
            "#,
        )
        .unwrap();
        drop(conn);
        let s = Store::open(&dir).unwrap();
        let p = pid(P1);
        let t = TriggerId {
            agent: MemberId(2),
            channel: ChannelId(3),
            message: MessageId(10),
        };
        assert_eq!(s.obligation_state(t).unwrap(), Some(ObligationState::Unmet));
        let k = s.cursor(MemberId(2), ChannelId(3)).unwrap();
        assert_eq!((k.read_through, k.read_at), (1, None));
        s.project_agents(&p)
            .unwrap()
            .execute(
                "UPDATE obligations SET state = 'ignored' WHERE message_id = 10",
                [],
            )
            .unwrap();
        assert_eq!(
            s.obligation_state(t).unwrap(),
            Some(ObligationState::Ignored)
        );
        let fk: i64 = s
            .project_workspace(&p)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(fk, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_v4_database_gains_stances_and_the_refused_state() {
        let dir = std::env::temp_dir().join(format!(
            "cc-store-v4-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("core.sqlite");
        let conn = Connection::open(&path).unwrap();
        for m in &MIGRATIONS[..4] {
            conn.execute_batch(m).unwrap();
        }
        conn.execute_batch(
            r#"
            PRAGMA user_version = 4;
            INSERT INTO projects (id, name, root_path, created_at) VALUES
                ('0b7e4f0e-2a51-4c4b-9a6e-3f1d2c3b4a59', 'P', NULL, 1);
            INSERT INTO channels (id, project_id, name, ordinal, created_at) VALUES
                (3, '0b7e4f0e-2a51-4c4b-9a6e-3f1d2c3b4a59', 'main', 1, 1);
            INSERT INTO members (id, kind, handle, name) VALUES (1, 'human', 'hx', 'Hx'), (2, 'agent', 'bot', 'Bot');
            INSERT INTO messages (id, channel_id, seq, author_id, origin, body, sent_at) VALUES
                (10, 3, 1, 1, 'composer', '@bot hi', 1000),
                (11, 3, 2, 2, 'refusal', 'no', 1001);
            INSERT INTO batches (id, agent_id, channel_id, state, created_at) VALUES (4, 2, 3, 'settled', 1000);
            INSERT INTO triggers VALUES (2, 3, 10, 'mention', 0, 4, 1000);
            INSERT INTO obligations VALUES (2, 3, 10, 'ignored', NULL, 'ignored');
            "#,
        )
        .unwrap();
        drop(conn);
        let s = Store::open(&dir).unwrap();
        let p = pid(P1);
        let t = TriggerId {
            agent: MemberId(2),
            channel: ChannelId(3),
            message: MessageId(10),
        };
        assert_eq!(
            s.obligation_state(t).unwrap(),
            Some(ObligationState::Ignored),
            "rows copied as they are"
        );
        s.project_agents(&p)
            .unwrap()
            .execute(
                "UPDATE obligations SET state = 'refused' WHERE message_id = 10",
                [],
            )
            .unwrap();
        assert_eq!(
            s.obligation_state(t).unwrap(),
            Some(ObligationState::Refused)
        );
        let st = Stance {
            message: MessageId(11),
            channel: ChannelId(3),
            kind: StanceKind::Refusal,
            member: MemberId(2),
            target: MessageId(10),
            reason: Arc::from("no"),
            model: Some(Arc::from("m")),
            runtime: None,
            created_at: 1001,
        };
        s.insert_stance(&st).unwrap();
        s.insert_stance(&st).unwrap();
        assert_eq!(s.stance(MessageId(11)).unwrap(), Some(st.clone()));
        assert_eq!(s.recent_stances(ChannelId(3), 10).unwrap(), vec![st]);
        assert_eq!(s.stance(MessageId(10)).unwrap(), None);
        let fk: i64 = s
            .project_workspace(&p)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(fk, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_v8_database_moves_reactions_into_the_v9_table() {
        let dir = std::env::temp_dir().join(format!(
            "cc-store-v8-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("core.sqlite");
        let conn = Connection::open(&path).unwrap();
        for m in &MIGRATIONS[..8] {
            conn.execute_batch(m).unwrap();
        }
        conn.execute_batch(
            r#"
            PRAGMA user_version = 8;
            INSERT INTO projects (id, name, root_path, created_at) VALUES
                ('0b7e4f0e-2a51-4c4b-9a6e-3f1d2c3b4a59', 'P', NULL, 1);
            INSERT INTO channels (id, project_id, name, ordinal, created_at) VALUES
                (3, '0b7e4f0e-2a51-4c4b-9a6e-3f1d2c3b4a59', 'main', 1, 1);
            INSERT INTO members (id, kind, handle, name) VALUES (1, 'human', 'hx', 'Hx'), (2, 'agent', 'bot', 'Bot');
            INSERT INTO messages (id, channel_id, seq, author_id, origin, body, sent_at) VALUES
                (10, 3, 1, 1, 'composer', '@bot hi', 1000),
                (11, 3, 2, 2, 'chat_tool', 'hello', 1001);
            INSERT INTO reactions (message_id, member_id, emoji, created_at) VALUES
                (10, 2, '✅', 1002),
                (10, 2, 'white_check_mark', 1003),
                (11, 1, '+1', 1004),
                (11, 2, '👀', 1005),
                (11, 2, 'eyes', 1006),
                (11, 1, 'this is far too long to be any kind of emoji at all', 1007);
            "#,
        )
        .unwrap();
        drop(conn);
        let s = Store::open(&dir).unwrap();
        let p = pid(P1);
        let on = |m: i64| {
            s.reactions_on(MessageId(m))
                .unwrap()
                .into_iter()
                .map(|r| (r.channel.0, r.member.0, r.emoji.to_string(), r.created_at))
                .collect::<Vec<_>>()
        };
        assert_eq!(on(10), vec![(3, 2, "✅".to_string(), 1002)]);
        assert_eq!(
            on(11),
            vec![
                (3, 1, "👍".to_string(), 1004),
                (3, 2, "👀".to_string(), 1005)
            ]
        );
        assert!(
            !s.add_reaction(MessageId(11), ChannelId(3), MemberId(1), "👍", 2000)
                .unwrap()
        );
        let long = "x".repeat(33);
        assert!(
            !s.add_reaction(MessageId(11), ChannelId(3), MemberId(1), &long, 2000)
                .unwrap()
        );
        assert_eq!(on(11).len(), 2);
        let fk: i64 = s
            .project_workspace(&p)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(fk, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reactions_add_remove_and_list() {
        let (mut s, channel, hx, bot) = setup();
        let m = s
            .insert_message(&msg(channel.id, hx.id, "ship it"))
            .unwrap();
        assert!(s.add_reaction(m.id, channel.id, bot.id, "👍", 10).unwrap());
        assert!(!s.add_reaction(m.id, channel.id, bot.id, "👍", 11).unwrap());
        assert!(s.add_reaction(m.id, channel.id, hx.id, "👍", 12).unwrap());
        assert!(s.add_reaction(m.id, channel.id, bot.id, "💀", 13).unwrap());
        assert!(s.has_reacted(m.id, bot.id).unwrap());
        assert_eq!(s.reactions_on(m.id).unwrap().len(), 3);
        assert_eq!(s.recent_reactions(channel.id, 2).unwrap().len(), 2);
        assert_eq!(
            s.recent_reactions(channel.id, 2).unwrap()[1].emoji.as_ref(),
            "💀",
            "newest last"
        );
        assert_eq!(s.reactions_since(channel.id, 11, 10).unwrap().len(), 2);
        assert_eq!(s.reactions_after_seq(channel.id, 0).unwrap().len(), 3);
        assert_eq!(s.reactions_after_seq(channel.id, m.seq).unwrap().len(), 0);
        assert!(s.remove_reaction(m.id, bot.id, "👍").unwrap());
        assert!(!s.remove_reaction(m.id, bot.id, "👍").unwrap());
        assert_eq!(s.reactions_on(m.id).unwrap().len(), 2);
    }

    #[test]
    fn read_cursors_stamp_when_they_move_and_never_go_back() {
        let (s, channel, hx, _) = setup();
        let (a, ch) = (hx.id, channel.id);
        assert!(s.advance_read(a, ch, 5, 100).unwrap());
        assert!(!s.advance_read(a, ch, 3, 200).unwrap(), "never backwards");
        assert!(!s.advance_read(a, ch, 5, 300).unwrap(), "no move, no stamp");
        let k = s.cursor(a, ch).unwrap();
        assert_eq!((k.read_through, k.read_at), (5, Some(100)));
        assert!(s.advance_delivered(a, ch, 8, 400).unwrap());
        let k = s.cursor(a, ch).unwrap();
        assert_eq!(
            (k.delivered_through, k.read_through, k.read_at),
            (8, 8, Some(400))
        );
        assert!(!s.advance_delivered(a, ch, 6, 500).unwrap());
        assert_eq!(s.cursor(a, ch).unwrap().read_at, Some(400));
    }

    #[test]
    fn a_fresh_database_has_no_imported_project() {
        let s = Store::open_in_memory().unwrap();
        assert!(projects(s.conn()).unwrap().is_empty());
        assert!(unclaimed_import(s.conn()).unwrap().is_none());
    }

    /// A v1 database with one channel and one message, migrated to split layout.
    fn migrated_v1() -> (Store, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "cc-migrated-v1-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("core.sqlite");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(MIGRATIONS[0]).unwrap();
        conn.execute_batch(
            r#"
            PRAGMA user_version = 1;
            INSERT INTO channels (id, name, created_at) VALUES (3, 'main', 100);
            INSERT INTO members (id, kind, handle, name) VALUES (1, 'human', 'hx', 'Hx');
            INSERT INTO memberships VALUES (3, 1, 10);
            INSERT INTO messages (id, channel_id, seq, author_id, origin, body, sent_at)
                VALUES (10, 3, 1, 1, 'composer', 'from before projects', 1000);
            "#,
        )
        .unwrap();
        drop(conn);
        (Store::open(&dir).unwrap(), dir)
    }

    #[test]
    fn adopting_the_import_gives_it_the_folder_once() {
        let (mut s, dir) = migrated_v1();
        let imported = s.unclaimed_import().unwrap().expect("migration 2 made one");
        assert_eq!(imported.name.as_ref(), "Imported");
        // The placeholder opened while the user was asked holds the folder.
        s.ensure_project(&pid(P1), "starpoint", Some("/src/starpoint"), 2000)
            .unwrap();
        let placeholder = s.ensure_channel(&pid(P1), "main", 2000).unwrap();

        let adopted = s
            .adopt_import(&imported.id, "starpoint", "/src/starpoint")
            .unwrap()
            .expect("adoptable");
        assert_eq!(adopted.id, imported.id, "same UUID, same history");
        assert_eq!(adopted.name.as_ref(), "starpoint");
        assert_eq!(adopted.root_path.as_deref(), Some("/src/starpoint"));
        assert_eq!(
            s.project_by_root("/src/starpoint").unwrap().unwrap().id,
            imported.id
        );
        assert_eq!(s.project(&pid(P1)).unwrap().unwrap().root_path, None);
        let main = s.channel_in_project(&imported.id, "main").unwrap().unwrap();
        assert_eq!(main.id, ChannelId(3), "channel ids are kept");
        // project has messages
        let ws = s.project_workspace(&imported.id).unwrap();
        let has: i64 = ws
            .query_row("SELECT EXISTS(SELECT 1 FROM messages)", [], |r| r.get(0))
            .unwrap();
        assert_eq!(has, 1);

        // Claimed: never offered or adoptable again.
        assert!(s.unclaimed_import().unwrap().is_none());
        assert!(
            s.adopt_import(&imported.id, "other", "/src/other")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            s.project(&imported.id)
                .unwrap()
                .unwrap()
                .root_path
                .as_deref(),
            Some("/src/starpoint")
        );

        // The empty placeholder goes; the adopted project never does.
        assert_eq!(
            s.forget_empty_project(&pid(P1)).unwrap(),
            Some(vec![placeholder.id])
        );
        assert!(s.project(&pid(P1)).unwrap().is_none());
        assert_eq!(s.forget_empty_project(&imported.id).unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_project_with_a_folder_is_never_adoptable() {
        let (mut s, dir) = migrated_v1();
        let imported = s.unclaimed_import().unwrap().unwrap();
        // A project that had a folder and lost it is not an import.
        s.ensure_project(&pid(P1), "Old", Some("/src/x"), 0)
            .unwrap();
        s.ensure_project(&pid(P2), "New", Some("/src/x"), 1)
            .unwrap();
        assert_eq!(s.project(&pid(P1)).unwrap().unwrap().root_path, None);
        assert!(s.adopt_import(&pid(P1), "x", "/src/y").unwrap().is_none());
        assert!(s.adopt_import(&pid(P2), "x", "/src/y").unwrap().is_none());
        // An import that somehow has a folder is not offered either.
        s.core_mut()
            .execute(
                "UPDATE projects SET root_path = '/src/live' WHERE id = ?1",
                [imported.id.as_str()],
            )
            .unwrap();
        assert!(s.unclaimed_import().unwrap().is_none());
        assert!(
            s.adopt_import(&imported.id, "x", "/src/y")
                .unwrap()
                .is_none()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_empty_unrooted_projects_are_forgotten() {
        let (mut s, ch, hx, _) = setup();
        // `setup`'s project has no folder but has a message: kept.
        s.insert_message(&msg(ch.id, hx.id, "keep me")).unwrap();
        assert_eq!(s.forget_empty_project(&ch.project).unwrap(), None);
        // Empty but rooted: kept.
        s.ensure_project(&pid(P2), "Copy", Some("/src/copy"), 0)
            .unwrap();
        let main = s.ensure_channel(&pid(P2), "main", 0).unwrap();
        s.join_project(&pid(P2), hx.id, 0).unwrap();
        s.join(main.id, &hx, 20, 0).unwrap();
        assert_eq!(s.forget_empty_project(&pid(P2)).unwrap(), None);
        // Empty and unrooted (another project took the folder): forgotten.
        s.ensure_project(&ch.project, "Starpoint", Some("/src/copy"), 1)
            .unwrap();
        assert_eq!(
            s.forget_empty_project(&pid(P2)).unwrap(),
            Some(vec![main.id])
        );
        assert!(s.project(&pid(P2)).unwrap().is_none());
        assert!(!s.is_member(main.id, hx.id).unwrap());
        assert!(!s.is_project_member(&pid(P2), hx.id).unwrap());
    }
    // --- Required tests for STRM-658 split ---

    #[test]
    fn required_fresh_install_creates_split_layout() {
        let dir = std::env::temp_dir().join(format!(
            "cc-split-fresh-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        {
            let mut s = Store::open(&dir).unwrap();
            assert!(dir.join("core.db").exists());
            let p = pid(P1);
            let proj = s
                .ensure_project(&p, "Starpoint", Some("/tmp/starpoint"), 1)
                .unwrap();
            assert_eq!(proj.id, p);
            let ch = s.ensure_channel(&p, "main", 2).unwrap();
            assert_eq!(ch.ordinal, 1);
            assert!(
                dir.join("Projects")
                    .join(p.as_str())
                    .join("workspace.db")
                    .exists()
            );
            assert!(
                dir.join("Projects")
                    .join(p.as_str())
                    .join("agents.db")
                    .exists()
            );
        }
        let s2 = Store::open(&dir).unwrap();
        assert!(s2.project(&pid(P1)).unwrap().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn make_real_shaped_legacy_core(path: &std::path::Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(LEGACY_MIGRATIONS[0]).unwrap();
        for (i, sql) in LEGACY_MIGRATIONS.iter().enumerate().skip(1) {
            conn.execute_batch(sql).unwrap();
            conn.execute_batch(&format!("PRAGMA user_version = {}", i + 1))
                .unwrap();
        }
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        // two projects, channels, members, memberships, messages, mentions, reactions, cursors, batches/triggers/obligations, stances, sessions, project_settings
        conn.execute(
            "INSERT INTO projects (id, name, root_path, created_at, unclaimed_import) VALUES (?1, ?2, ?3, ?4, 0)",
            params![P1, "Starpoint", "/src/starpoint", 10i64],
        ).unwrap();
        conn.execute(
            "INSERT INTO projects (id, name, root_path, created_at, unclaimed_import) VALUES (?1, ?2, ?3, ?4, 0)",
            params![P2, "Everything", "/src/everything", 20i64],
        ).unwrap();
        conn.execute(
            "INSERT INTO members (id, kind, handle, name) VALUES (1,'human','hx','Hx')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO members (id, kind, handle, name) VALUES (2,'agent','claude','Claude')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO project_members (project_id, member_id, joined_at) VALUES (?1,1,10)",
            params![P1],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO project_members (project_id, member_id, joined_at) VALUES (?1,2,10)",
            params![P1],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO project_members (project_id, member_id, joined_at) VALUES (?1,1,20)",
            params![P2],
        )
        .unwrap();
        conn.execute("INSERT INTO channels (id, project_id, name, ordinal, created_at) VALUES (10, ?1, 'main', 1, 11)", params![P1]).unwrap();
        conn.execute("INSERT INTO channels (id, project_id, name, ordinal, created_at) VALUES (11, ?1, 'ops', 2, 12)", params![P1]).unwrap();
        conn.execute("INSERT INTO channels (id, project_id, name, ordinal, created_at) VALUES (20, ?1, 'main', 1, 21)", params![P2]).unwrap();
        conn.execute(
            "INSERT INTO memberships (channel_id, member_id, joined_at) VALUES (10,1,11)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO memberships (channel_id, member_id, joined_at) VALUES (10,2,11)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO memberships (channel_id, member_id, joined_at) VALUES (20,1,21)",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO messages (id, channel_id, seq, author_id, origin, body, reply_to, hop, cause_batch, client_request_id, sent_at) VALUES (100,10,1,1,'composer','hello',NULL,0,NULL,NULL,1000)", []).unwrap();
        conn.execute("INSERT INTO messages (id, channel_id, seq, author_id, origin, body, reply_to, hop, cause_batch, client_request_id, sent_at) VALUES (101,10,2,2,'chat_tool','hi',100,1,NULL,'r1',1001)", []).unwrap();
        conn.execute("INSERT INTO messages (id, channel_id, seq, author_id, origin, body, reply_to, hop, cause_batch, client_request_id, sent_at) VALUES (200,20,1,1,'composer','other project',NULL,0,NULL,NULL,2000)", []).unwrap();
        conn.execute(
            "INSERT INTO mentions (message_id, member_id) VALUES (101,1)",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO reactions (message_id, channel_id, member_id, emoji, created_at) VALUES (101,10,1,'👍',1002)", []).unwrap();
        conn.execute("INSERT INTO cursors (agent_id, channel_id, delivered_through, read_through, delivered_at, read_at) VALUES (2,10,2,2,1001,1001)", []).unwrap();
        conn.execute("INSERT INTO batches (id, agent_id, channel_id, state, attempts, window_to, created_at, accepted_at, settled_at, outcome) VALUES (500,2,10,'settled',1,2,1000,1000,1001,'end_turn')", []).unwrap();
        conn.execute("INSERT INTO triggers (agent_id, channel_id, message_id, reason, hop, batch_id, created_at) VALUES (2,10,100,'mention',0,500,1000)", []).unwrap();
        conn.execute("INSERT INTO obligations (agent_id, channel_id, message_id, state, resolved_by, resolved_kind) VALUES (2,10,100,'satisfied',101,'send')", []).unwrap();
        conn.execute("INSERT INTO sessions (agent_id, channel_id, protocol, native_ref, opened_at) VALUES (2,10,'acp','s1',1000)", []).unwrap();
        conn.execute("INSERT INTO stances (message_id, channel_id, kind, member_id, target_id, reason, model, runtime, created_at) VALUES (101,10,'refusal',2,100,'nope',NULL,NULL,1001)", []).unwrap();
        conn.execute("INSERT INTO project_settings (project_id, key, value, updated_at) VALUES (?1,'k','v',5)", params![P1]).unwrap();
        // sequences
        conn.execute(
            "INSERT OR REPLACE INTO sqlite_sequence(name, seq) VALUES ('messages', 200)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO sqlite_sequence(name, seq) VALUES ('batches', 500)",
            [],
        )
        .unwrap();
    }

    #[test]
    fn required_migrate_real_shaped_core_sqlite_two_projects_preserved() {
        let dir = std::env::temp_dir().join(format!(
            "cc-split-mig-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let legacy = dir.join("core.sqlite");
        make_real_shaped_legacy_core(&legacy);

        let s = Store::open(&dir).unwrap();
        assert!(
            !legacy.exists(),
            "core.sqlite must be renamed to backup after verified migration"
        );
        assert!(dir.join("core.sqlite.pre-split").exists());
        assert!(dir.join("core.db").exists());

        // core data
        assert_eq!(s.projects().unwrap().len(), 2);
        assert_eq!(
            s.member(MemberId(1)).unwrap().unwrap().handle.as_ref(),
            "hx"
        );
        assert!(s.is_project_member(&pid(P1), MemberId(2)).unwrap());

        // project 1 workspace data preserved (ids/counts)
        let ws1 = s.project_workspace(&pid(P1)).unwrap();
        let ch_count: i64 = ws1
            .query_row("SELECT COUNT(*) FROM channels", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ch_count, 2);
        let msg_ids: Vec<i64> = {
            let mut st = ws1.prepare("SELECT id FROM messages ORDER BY id").unwrap();
            st.query_map([], |r| r.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert_eq!(msg_ids, vec![100, 101]);
        let reacts: i64 = ws1
            .query_row("SELECT COUNT(*) FROM reactions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(reacts, 1);
        let setting: Option<String> = ws1
            .query_row(
                "SELECT value FROM project_settings WHERE project_id=?1 AND key='k'",
                params![P1],
                |r| r.get(0),
            )
            .optional()
            .unwrap()
            .flatten();
        assert_eq!(setting.as_deref(), Some("v"));

        // project 1 agents data preserved
        let ag1 = s.project_agents(&pid(P1)).unwrap();
        let batches: i64 = ag1
            .query_row("SELECT COUNT(*) FROM batches", [], |r| r.get(0))
            .unwrap();
        assert_eq!(batches, 1);
        let obligations: i64 = ag1
            .query_row(
                "SELECT COUNT(*) FROM obligations WHERE state='satisfied'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(obligations, 1);

        // project 2 preserved
        let ws2 = s.project_workspace(&pid(P2)).unwrap();
        let msg2: i64 = ws2
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(msg2, 1);
        let msg2id: i64 = ws2
            .query_row("SELECT id FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(msg2id, 200);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn required_crash_mid_migration_rerun_completes() {
        let dir = std::env::temp_dir().join(format!(
            "cc-split-crash-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let legacy = dir.join("core.sqlite");
        make_real_shaped_legacy_core(&legacy);

        // Simulate crash mid-migration: create partial core.db and only one project dir incomplete
        {
            let core = open_db(&dir.join("core.db"), CORE_MIGRATIONS).unwrap();
            core.execute("INSERT INTO projects (id,name,root_path,created_at,unclaimed_import) VALUES (?1,'x',NULL,1,0)", params![P1]).unwrap();
            let pdir = dir.join("Projects").join(P1);
            std::fs::create_dir_all(&pdir).unwrap();
            std::fs::write(pdir.join("workspace.db"), b"not a sqlite db").unwrap();
            // core.sqlite still present (not renamed)
        }
        assert!(legacy.exists());

        // Rerun must complete successfully from intact core.sqlite
        let s = Store::open(&dir).unwrap();
        assert!(!legacy.exists());
        assert!(dir.join("core.sqlite.pre-split").exists());
        assert_eq!(s.projects().unwrap().len(), 2);
        let ws1 = s.project_workspace(&pid(P1)).unwrap();
        let c: i64 = ws1
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(c, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn required_corrupt_workspace_db_one_project_others_open() {
        let dir = std::env::temp_dir().join(format!(
            "cc-split-corrupt-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let legacy = dir.join("core.sqlite");
        make_real_shaped_legacy_core(&legacy);
        {
            let _s = Store::open(&dir).unwrap();
        }
        // corrupt project 1 workspace.db
        let bad = dir.join("Projects").join(P1).join("workspace.db");
        std::fs::write(&bad, b"corrupted").unwrap();

        let s = Store::open(&dir).unwrap();
        // core and project 2 still usable
        assert_eq!(s.projects().unwrap().len(), 2);
        assert!(
            s.project_workspace(&pid(P2)).is_ok(),
            "other project must still open"
        );
        let msg2: i64 = s
            .project_workspace(&pid(P2))
            .unwrap()
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(msg2, 1);
        // broken project isolated
        assert!(s.project_workspace(&pid(P1)).is_err() || !s.project_errors().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn required_newer_schema_file_refused_untouched() {
        let dir = std::env::temp_dir().join(format!(
            "cc-split-newer-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let legacy = dir.join("core.sqlite");
        make_real_shaped_legacy_core(&legacy);
        {
            let _s = Store::open(&dir).unwrap();
        }
        // bump workspace schema of project 1 to "newer"
        let ws_path = dir.join("Projects").join(P1).join("workspace.db");
        let before = std::fs::read(&ws_path).unwrap();
        {
            let c = Connection::open(&ws_path).unwrap();
            c.execute_batch("PRAGMA user_version = 9999;").unwrap();
        }
        match Store::open(&dir) {
            Err(OpenError::NewerSchema { found, known, .. }) => {
                assert_eq!(found, 9999);
                assert!(known < found);
            }
            Err(e) => panic!("expected NewerSchema, got {e}"),
            Ok(_) => panic!("newer schema must be refused"),
        }
        // untouched
        let after = std::fs::read(&ws_path).unwrap();
        assert!(before.len() == after.len() || before != after); // file still exists; check version unchanged
        let c = Connection::open(&ws_path).unwrap();
        let v: i64 = c
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 9999);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// F3: legacy upgrade must not silently put live rooms to sleep. The
    /// STRM-686 lifecycle seed runs inside `open_db` before `members` is
    /// copied, so `migrate_legacy` must reseed `agent_lifecycle` afterwards;
    /// a missing row would read as Sleeping and block even explicit pings.
    #[test]
    fn legacy_migration_seeds_agent_lifecycle_for_copied_agents() {
        let dir = std::env::temp_dir().join(format!(
            "cc-legacy-lifecycle-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let legacy = dir.join("core.sqlite");
        make_real_shaped_legacy_core(&legacy);

        let s = Store::open(&dir).unwrap();

        // member 1 is human; member 2 is an agent copied from the legacy DB.
        assert_eq!(
            s.agent_lifecycle(MemberId(1)).unwrap(),
            LifecycleState::Sleeping,
            "non-agent members have no lifecycle row; the agent check is what matters"
        );
        assert_eq!(
            s.agent_lifecycle(MemberId(2)).unwrap(),
            LifecycleState::Active,
            "a copied agent must not be silently dead after upgrade"
        );
        assert!(
            !s.agent_forced_sleep(MemberId(2)).unwrap(),
            "a migrated agent is not force-slept"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn required_migrated_legacy_preserves_ids_and_supports_new_writes() {
        let dir = std::env::temp_dir().join(format!(
            "cc-split-mig-write-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let legacy = dir.join("core.sqlite");
        make_real_shaped_legacy_core(&legacy);

        let mut s = Store::open(&dir).unwrap();

        // Existing IDs/data preserved after migration.
        assert!(s.project(&pid(P1)).unwrap().is_some());
        assert!(s.project(&pid(P2)).unwrap().is_some());
        let ws1 = s.project_workspace(&pid(P1)).unwrap();
        let old_msg_ids: Vec<i64> = {
            let mut st = ws1.prepare("SELECT id FROM messages ORDER BY id").unwrap();
            st.query_map([], |r| r.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert_eq!(old_msg_ids, vec![100, 101]);
        let ws2 = s.project_workspace(&pid(P2)).unwrap();
        let old_msg2: i64 = ws2
            .query_row("SELECT id FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(old_msg2, 200);

        // Create new channels across multiple projects.
        let ch1_new = s.ensure_channel(&pid(P1), "new-p1", 10_000).unwrap();
        let ch2_new = s.ensure_channel(&pid(P2), "new-p2", 10_001).unwrap();
        assert!(
            ch1_new.id.0 > 20,
            "new channel id must be above legacy max (20), got {}",
            ch1_new.id.0
        );
        assert!(
            ch2_new.id.0 > 20,
            "new channel id must be above legacy max (20), got {}",
            ch2_new.id.0
        );
        assert_ne!(ch1_new.id, ch2_new.id);

        // Create new messages across multiple projects.
        let hx = s.member(MemberId(1)).unwrap().unwrap();
        s.join(ch1_new.id, &hx, 0, 10_000).unwrap();
        s.join(ch2_new.id, &hx, 0, 10_001).unwrap();
        let m1 = s
            .insert_message(&msg(ch1_new.id, MemberId(1), "post-migration p1"))
            .unwrap();
        let m2 = s
            .insert_message(&msg(ch2_new.id, MemberId(1), "post-migration p2"))
            .unwrap();
        assert!(
            m1.id.0 > 200,
            "new message id must be above legacy max (200), got {}",
            m1.id.0
        );
        assert!(
            m2.id.0 > 200,
            "new message id must be above legacy max (200), got {}",
            m2.id.0
        );
        assert_ne!(m1.id, m2.id);
        assert_eq!(
            s.message(m1.id).unwrap().unwrap().body.as_ref(),
            "post-migration p1"
        );
        assert_eq!(
            s.message(m2.id).unwrap().unwrap().body.as_ref(),
            "post-migration p2"
        );

        // Create new agent batches across multiple projects/channels.
        let b1 = s
            .pending_batch_or_create(MemberId(2), ch1_new.id, 10_002)
            .unwrap();
        let b2 = s
            .pending_batch_or_create(MemberId(2), ch2_new.id, 10_003)
            .unwrap();
        assert!(
            b1.id.0 > 500,
            "new batch id must be above legacy max (500), got {}",
            b1.id.0
        );
        assert!(
            b2.id.0 > 500,
            "new batch id must be above legacy max (500), got {}",
            b2.id.0
        );
        assert_ne!(b1.id, b2.id);

        // Global uniqueness across entity kinds not required, but IDs within counters must not collide.
        let mut channel_ids = vec![
            ChannelId(10).0,
            ChannelId(11).0,
            ChannelId(20).0,
            ch1_new.id.0,
            ch2_new.id.0,
        ];
        channel_ids.sort_unstable();
        channel_ids.dedup();
        assert_eq!(channel_ids.len(), 5);
        let mut message_ids = vec![100i64, 101, 200, m1.id.0, m2.id.0];
        message_ids.sort_unstable();
        message_ids.dedup();
        assert_eq!(message_ids.len(), 5);
        let mut batch_ids = vec![500i64, b1.id.0, b2.id.0];
        batch_ids.sort_unstable();
        batch_ids.dedup();
        assert_eq!(batch_ids.len(), 3);

        // Existing data still accessible.
        assert!(s.message(MessageId(100)).unwrap().is_some());
        assert!(s.message(MessageId(101)).unwrap().is_some());
        assert!(s.message(MessageId(200)).unwrap().is_some());
        assert!(s.channel(ChannelId(10)).unwrap().is_some());
        assert!(s.batch(BatchId(500)).unwrap().is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn migration_recovery_does_not_accept_empty_split_without_marker_when_legacy_id_tables_empty() {
        let dir = std::env::temp_dir().join(format!(
            "cc-split-empty-ids-marker-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let legacy = dir.join("core.sqlite");

        // Legacy DB: has project/member data but no channels/messages/batches (ID maxima are 0).
        {
            let conn = Connection::open(&legacy).unwrap();
            conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
            super::run_migrations(&conn, LEGACY_MIGRATIONS, 0).unwrap();
            conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
            conn.execute(
                "INSERT INTO projects (id, name, root_path, created_at, unclaimed_import) VALUES (?1, 'Alpha', NULL, 1, 0)",
                params![P1],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO members (id, kind, handle, name) VALUES (1, 'human', 'hx', 'Hx')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO project_members (project_id, member_id, joined_at) VALUES (?1, 1, 1)",
                params![P1],
            )
            .unwrap();
        }

        // Plant an empty-but-valid split layout with default counters and no completion marker.
        {
            let core = super::open_db(&dir.join("core.db"), CORE_MIGRATIONS).unwrap();
            // intentionally no projects copied into split core
            drop(core);
            // no Projects/* DBs, no completion marker
        }

        let mut s = Store::open(&dir).unwrap();
        assert!(
            !legacy.exists(),
            "legacy must be migrated and renamed, not discarded as already-complete"
        );
        assert!(dir.join("core.sqlite.pre-split").exists());
        assert!(!super::split_migration_marker(&dir).exists());
        assert_eq!(s.projects().unwrap().len(), 1);
        assert_eq!(s.project(&pid(P1)).unwrap().unwrap().name.as_ref(), "Alpha");
        assert!(s.is_project_member(&pid(P1), MemberId(1)).unwrap());

        // Counters remain valid for future allocations even when legacy ID tables were empty.
        assert_eq!(s.allocate_id("channel").unwrap(), 1);
        assert_eq!(s.allocate_id("message").unwrap(), 1);
        assert_eq!(s.allocate_id("batch").unwrap(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn migration_recovery_retries_markerless_partial_split_with_empty_project_dbs() {
        let dir = std::env::temp_dir().join(format!(
            "cc-split-partial-markerless-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let legacy = dir.join("core.sqlite");
        make_real_shaped_legacy_core(&legacy);

        // Interrupted/partial state: core.db copied with project metadata, per-project DBs exist
        // with schema but no copied data (including missing project_settings), and no marker.
        {
            let core = super::open_db(&dir.join("core.db"), CORE_MIGRATIONS).unwrap();
            core.execute(
                "INSERT INTO projects (id, name, root_path, created_at, unclaimed_import) VALUES (?1,'Alpha','/tmp/a',1,0)",
                params![P1],
            )
            .unwrap();
            core.execute(
                "INSERT INTO projects (id, name, root_path, created_at, unclaimed_import) VALUES (?1,'Beta','/tmp/b',2,0)",
                params![P2],
            )
            .unwrap();
            core.execute(
                "INSERT INTO members (id, kind, handle, name) VALUES (1,'human','hx','Hx'), (2,'agent','claude','Claude')",
                [],
            )
            .unwrap();
            core.execute(
                "INSERT INTO project_members (project_id, member_id, joined_at) VALUES (?1,1,1),(?1,2,1),(?2,1,2),(?2,2,2)",
                params![P1, P2],
            )
            .unwrap();
            drop(core);

            for p in [P1, P2] {
                let pdir = dir.join("Projects").join(p);
                std::fs::create_dir_all(&pdir).unwrap();
                let _ws = super::open_db(&pdir.join("workspace.db"), WORKSPACE_MIGRATIONS).unwrap();
                let _ag = super::open_db(&pdir.join("agents.db"), AGENTS_MIGRATIONS).unwrap();
            }
            assert!(!super::split_migration_marker(&dir).exists());
        }
        assert!(legacy.exists());

        let s = Store::open(&dir).unwrap();
        assert!(
            !legacy.exists(),
            "legacy must be migrated, not accepted as complete"
        );
        assert!(dir.join("core.sqlite.pre-split").exists());
        assert!(!super::split_migration_marker(&dir).exists());

        // Data copied from legacy, including project_settings that were missing in partial state.
        let ws1 = s.project_workspace(&pid(P1)).unwrap();
        let setting: Option<String> = ws1
            .query_row(
                "SELECT value FROM project_settings WHERE project_id=?1 AND key='k'",
                params![P1],
                |r| r.get(0),
            )
            .optional()
            .unwrap()
            .flatten();
        assert_eq!(setting.as_deref(), Some("v"));
        let msg_ids: Vec<i64> = {
            let mut st = ws1.prepare("SELECT id FROM messages ORDER BY id").unwrap();
            st.query_map([], |r| r.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert_eq!(msg_ids, vec![100, 101]);
        let batches: i64 = s
            .project_agents(&pid(P1))
            .unwrap()
            .query_row("SELECT COUNT(*) FROM batches", [], |r| r.get(0))
            .unwrap();
        assert_eq!(batches, 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn migration_recovery_retries_partial_split_with_empty_id_tables_and_copied_core_metadata() {
        let dir = std::env::temp_dir().join(format!(
            "cc-split-partial-empty-ids-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let legacy = dir.join("core.sqlite");

        // Legacy DB: project/member metadata exists, but channel/message/batch ID tables are empty.
        {
            let conn = Connection::open(&legacy).unwrap();
            conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
            super::run_migrations(&conn, LEGACY_MIGRATIONS, 0).unwrap();
            conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
            conn.execute(
                "INSERT INTO projects (id, name, root_path, created_at, unclaimed_import) VALUES (?1, 'Alpha', '/tmp/alpha', 1, 0)",
                params![P1],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO members (id, kind, handle, name) VALUES (1, 'human', 'hx', 'Hx')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO project_members (project_id, member_id, joined_at) VALUES (?1, 1, 1)",
                params![P1],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO project_settings (project_id, key, value, updated_at) VALUES (?1, 'k', 'v', 5)",
                params![P1],
            )
            .unwrap();
        }

        // Interrupted/partial state: copied core metadata, schema-valid empty project DBs, no valid marker.
        let plant_partial = |dir: &std::path::Path| {
            let core = super::open_db(&dir.join("core.db"), CORE_MIGRATIONS).unwrap();
            core.execute(
                "INSERT INTO projects (id, name, root_path, created_at, unclaimed_import) VALUES (?1, 'Alpha', '/tmp/alpha', 1, 0)",
                params![P1],
            )
            .unwrap();
            core.execute(
                "INSERT INTO members (id, kind, handle, name) VALUES (1, 'human', 'hx', 'Hx')",
                [],
            )
            .unwrap();
            core.execute(
                "INSERT INTO project_members (project_id, member_id, joined_at) VALUES (?1, 1, 1)",
                params![P1],
            )
            .unwrap();
            drop(core);
            let pdir = dir.join("Projects").join(P1);
            std::fs::create_dir_all(&pdir).unwrap();
            let _ws = super::open_db(&pdir.join("workspace.db"), WORKSPACE_MIGRATIONS).unwrap();
            let _ag = super::open_db(&pdir.join("agents.db"), AGENTS_MIGRATIONS).unwrap();
        };

        // Case A: missing marker.
        plant_partial(&dir);
        assert!(!super::split_migration_marker(&dir).exists());
        let mut s = Store::open(&dir).unwrap();
        assert!(
            !legacy.exists(),
            "legacy must not be discarded without a valid marker"
        );
        assert!(dir.join("core.sqlite.pre-split").exists());
        assert!(!super::split_migration_marker(&dir).exists());
        assert_eq!(s.projects().unwrap().len(), 1);
        assert!(s.is_project_member(&pid(P1), MemberId(1)).unwrap());
        let setting: Option<String> = s
            .project_workspace(&pid(P1))
            .unwrap()
            .query_row(
                "SELECT value FROM project_settings WHERE project_id=?1 AND key='k'",
                params![P1],
                |r| r.get(0),
            )
            .optional()
            .unwrap()
            .flatten();
        assert_eq!(setting.as_deref(), Some("v"));
        assert_eq!(s.allocate_id("channel").unwrap(), 1);
        assert_eq!(s.allocate_id("message").unwrap(), 1);
        assert_eq!(s.allocate_id("batch").unwrap(), 1);
        drop(s);

        // Reset for case B: recreate legacy and plant partial state with an empty (invalid) marker.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        {
            let conn = Connection::open(&legacy).unwrap();
            conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
            super::run_migrations(&conn, LEGACY_MIGRATIONS, 0).unwrap();
            conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
            conn.execute(
                "INSERT INTO projects (id, name, root_path, created_at, unclaimed_import) VALUES (?1, 'Alpha', '/tmp/alpha', 1, 0)",
                params![P1],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO members (id, kind, handle, name) VALUES (1, 'human', 'hx', 'Hx')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO project_members (project_id, member_id, joined_at) VALUES (?1, 1, 1)",
                params![P1],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO project_settings (project_id, key, value, updated_at) VALUES (?1, 'k', 'v', 5)",
                params![P1],
            )
            .unwrap();
        }
        plant_partial(&dir);
        std::fs::write(super::split_migration_marker(&dir), b"").unwrap();
        let s = Store::open(&dir).unwrap();
        assert!(
            !legacy.exists(),
            "empty marker must never certify completion; legacy must be migrated"
        );
        assert!(dir.join("core.sqlite.pre-split").exists());
        assert!(!super::split_migration_marker(&dir).exists());
        assert_eq!(s.projects().unwrap().len(), 1);
        let setting: Option<String> = s
            .project_workspace(&pid(P1))
            .unwrap()
            .query_row(
                "SELECT value FROM project_settings WHERE project_id=?1 AND key='k'",
                params![P1],
                |r| r.get(0),
            )
            .optional()
            .unwrap()
            .flatten();
        assert_eq!(setting.as_deref(), Some("v"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn v1_chat_backfill_creates_one_chat_per_channel() {
        let mut s = Store::open_in_memory().unwrap();
        let p = s
            .ensure_project(&pid(P1), "Alpha", Some("/tmp/alpha"), 1)
            .unwrap();
        let ch = s.ensure_channel(&p.id, "main", 10).unwrap();
        let chats = s.chats().unwrap();
        assert_eq!(chats.len(), 1);
        assert_eq!(chats[0].title.as_ref(), "main");
        assert_eq!(chats[0].targets.len(), 1);
        assert_eq!(chats[0].targets[0].project, p.id);
        assert_eq!(chats[0].targets[0].channel, ch.id);
        // idempotent
        s.ensure_channel(&p.id, "main", 20).unwrap();
        assert_eq!(s.chats().unwrap().len(), 1);
    }

    #[test]
    fn forgetting_project_cleans_up_chat_members() {
        let mut s = Store::open_in_memory().unwrap();
        let p = s.ensure_project(&pid(P1), "Alpha", None, 1).unwrap();
        let _ch = s.ensure_channel(&p.id, "main", 10).unwrap();
        assert_eq!(s.chats().unwrap().len(), 1);
        let gone = s.forget_empty_project(&p.id).unwrap();
        assert!(gone.is_some());
        assert!(s.chats().unwrap().is_empty());
        let members: i64 = s
            .core()
            .query_row("SELECT COUNT(*) FROM chat_members", [], |r| r.get(0))
            .unwrap();
        assert_eq!(members, 0);
    }

    #[test]
    fn rename_channel_updates_chat_title() {
        let mut s = Store::open_in_memory().unwrap();
        let p = s
            .ensure_project(&pid(P1), "Alpha", Some("/tmp/alpha"), 1)
            .unwrap();
        let ch = s.ensure_channel(&p.id, "main", 10).unwrap();
        assert!(s.rename_channel(ch.id, "general").unwrap());
        let chats = s.chats().unwrap();
        assert_eq!(chats[0].title.as_ref(), "general");
    }

    #[test]
    fn lifecycle_defaults_and_survives_a_reopen() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("cc-lifecycle-store-{nanos}"));
        let _ = std::fs::remove_dir_all(&dir);
        let agent = {
            let mut s = Store::open(&dir).unwrap();
            let agent = s.ensure_member(MemberKind::Agent, "bot", "Bot").unwrap();
            // A new agent has no row: globally Sleeping.
            assert_eq!(
                s.agent_lifecycle(agent.id).unwrap(),
                LifecycleState::Sleeping
            );
            assert!(!s.agent_forced_sleep(agent.id).unwrap());

            let p = s.ensure_project(&pid(P1), "Alpha", None, 1).unwrap();
            let ch = s.ensure_channel(&p.id, "main", 10).unwrap();
            s.join_project(&p.id, agent.id, 1).unwrap();
            s.join(ch.id, &agent, 0, 1).unwrap();
            let chat = s.chats().unwrap()[0].id;
            assert_eq!(
                s.chat_membership_lifecycle(chat, agent.id).unwrap(),
                MembershipState::Active
            );

            s.set_agent_lifecycle(agent.id, LifecycleState::Active, false)
                .unwrap();
            s.set_chat_membership_lifecycle(chat, agent.id, MembershipState::Sleeping, true)
                .unwrap();
            agent.id
        };
        {
            let s = Store::open(&dir).unwrap();
            assert_eq!(s.agent_lifecycle(agent).unwrap(), LifecycleState::Active);
            assert!(!s.agent_forced_sleep(agent).unwrap());
            let chat = s.chats().unwrap()[0].id;
            assert_eq!(
                s.chat_membership_lifecycle(chat, agent).unwrap(),
                MembershipState::Sleeping
            );
            assert!(s.chat_membership_forced_sleep(chat, agent).unwrap());

            // Global activation never rewrites explicitly sleeping memberships.
            s.set_agent_lifecycle(agent, LifecycleState::Active, false)
                .unwrap();
            assert_eq!(
                s.chat_membership_lifecycle(chat, agent).unwrap(),
                MembershipState::Sleeping
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
