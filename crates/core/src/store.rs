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
//! - At most one fallback reply per (agent, trigger message) (partial unique
//!   index on `messages`).
//! - Every channel belongs to a project; names and `::N` ordinals are unique
//!   within it (`UNIQUE(project_id, name)`, `UNIQUE(project_id, ordinal)`).
//! - Joining a channel needs membership of its project: a trigger on
//!   `memberships` aborts with `not_a_project_member` otherwise, so no code
//!   path can skip the check.
//! - One folder maps to at most one project (partial unique index on
//!   `projects.root_path`).
//! - The migrated "Imported" project is adoptable by one folder, once, and
//!   only while it has no folder (`adopt_import`'s guarded update).

use std::{path::Path, sync::Arc};

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::types::*;

pub type Result<T> = rusqlite::Result<T>;

/// Ordered migrations. `PRAGMA user_version` records how many have run; add new
/// ones at the end, never edit old ones.
const MIGRATIONS: &[&str] = &[
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
];

pub struct Store {
    conn: Connection,
}

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

impl Store {
    /// Opens (or creates) the database at `path` and runs pending migrations.
    pub fn open(path: &Path) -> Result<Self> {
        Self::init(Connection::open(path)?)
    }

    /// A private in-memory database (tests, demos).
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON; PRAGMA synchronous = NORMAL;",
        )?;
        let mut store = Store { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&mut self) -> Result<()> {
        let version: usize =
            self.conn
                .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))? as usize;
        if version >= MIGRATIONS.len() {
            return Ok(());
        }
        // A table rebuild (migration 2) drops a table other tables point at,
        // which SQLite only allows with foreign keys off. The pragma is a
        // no-op inside a transaction, so it is set around them; each
        // migration then proves nothing dangles before it commits.
        self.conn.execute_batch("PRAGMA foreign_keys = OFF")?;
        let result = self.run_migrations(version);
        self.conn.execute_batch("PRAGMA foreign_keys = ON")?;
        result
    }

    fn run_migrations(&mut self, from: usize) -> Result<()> {
        for (ix, sql) in MIGRATIONS.iter().enumerate().skip(from) {
            let tx = self.conn.transaction()?;
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
    }

    pub fn schema_version(&self) -> Result<i64> {
        self.conn.query_row("PRAGMA user_version", [], |r| r.get(0))
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Runs `f` in one immediate transaction; commits on `Ok`.
    pub fn tx<T, E: From<rusqlite::Error>>(
        &mut self,
        f: impl FnOnce(&Connection) -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }
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
    c.query_row(
        &format!("SELECT {PROJECT_COLS} FROM projects WHERE id = ?1"),
        [id.as_str()],
        project_from_row,
    )
    .optional()
}

/// The project last opened from `root`, if any.
pub fn project_by_root(c: &Connection, root: &str) -> Result<Option<Project>> {
    c.query_row(
        &format!("SELECT {PROJECT_COLS} FROM projects WHERE root_path = ?1"),
        [root],
        project_from_row,
    )
    .optional()
}

/// Every project, oldest first.
pub fn projects(c: &Connection) -> Result<Vec<Project>> {
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
    c.execute(
        "DELETE FROM project_members WHERE project_id = ?1",
        [project.as_str()],
    )?;
    c.execute("DELETE FROM projects WHERE id = ?1", [project.as_str()])?;
    Ok(Some(channels))
}

/// Changes a project's display name only; its id, channels and history stay.
pub fn rename_project(c: &Connection, id: &ProjectId, name: &str) -> Result<bool> {
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
    Ok(c.execute(
        "INSERT OR IGNORE INTO project_members (project_id, member_id, joined_at)
         VALUES (?1, ?2, ?3)",
        params![project.as_str(), member.0, now as i64],
    )? == 1)
}

pub fn is_project_member(c: &Connection, project: &ProjectId, member: MemberId) -> Result<bool> {
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
pub fn ensure_member(c: &Connection, kind: MemberKind, handle: &str, name: &str) -> Result<Member> {
    c.execute(
        "INSERT INTO members (kind, handle, name) VALUES (?1, ?2, ?3)
         ON CONFLICT(handle) DO UPDATE SET name = excluded.name",
        params![kind.as_str(), handle, name],
    )?;
    c.query_row(
        "SELECT id, kind, handle, name FROM members WHERE handle = ?1",
        [handle],
        member_from_row,
    )
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

pub fn members_of(c: &Connection, channel: ChannelId) -> Result<Vec<Member>> {
    let mut stmt = c.prepare_cached(
        "SELECT m.id, m.kind, m.handle, m.name FROM members m
         JOIN memberships s ON s.member_id = m.id
         WHERE s.channel_id = ?1 ORDER BY m.id",
    )?;
    stmt.query_map([channel.0], member_from_row)?.collect()
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

const MESSAGE_COLS: &str = "id, channel_id, seq, author_id, origin, body, reply_to, hop, sent_at";

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
        sent_at: r.get::<_, i64>(8)? as Millis,
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

pub fn add_reaction(
    c: &Connection,
    message: MessageId,
    member: MemberId,
    emoji: &str,
    now: Millis,
) -> Result<bool> {
    Ok(c.execute(
        "INSERT OR IGNORE INTO reactions (message_id, member_id, emoji, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![message.0, member.0, emoji, now as i64],
    )? == 1)
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
    use super::*;

    const P1: &str = "0b7e4f0e-2a51-4c4b-9a6e-3f1d2c3b4a59";
    const P2: &str = "5f0c9d1a-7b2e-4e8f-a1c3-9d8e7f6a5b4c";

    fn pid(s: &str) -> ProjectId {
        ProjectId::parse(s).unwrap()
    }

    fn setup() -> (Store, Channel, Member, Member) {
        let mut s = Store::open_in_memory().unwrap();
        let (ch, hx, bot) = s
            .tx::<_, rusqlite::Error>(|c| {
                let p = ensure_project(c, &pid(P1), "Starpoint", None, 0)?;
                let ch = ensure_channel(c, &p.id, "main", 0)?;
                let hx = ensure_member(c, MemberKind::Human, "hx", "Hx")?;
                let bot = ensure_member(c, MemberKind::Agent, "claude", "Claude")?;
                join_project(c, &p.id, hx.id, 0)?;
                join_project(c, &p.id, bot.id, 0)?;
                join(c, ch.id, &hx, 20, 0)?;
                join(c, ch.id, &bot, 20, 0)?;
                Ok((ch, hx, bot))
            })
            .unwrap();
        (s, ch, hx, bot)
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
        assert_eq!(s.schema_version().unwrap(), MIGRATIONS.len() as i64);
    }

    #[test]
    fn reopening_a_file_keeps_ids_and_does_not_rerun_migrations() {
        let dir = std::env::temp_dir().join(format!("cc-store-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.sqlite");
        let _ = std::fs::remove_file(&path);
        let id = {
            let mut s = Store::open(&path).unwrap();
            s.tx::<_, rusqlite::Error>(|c| {
                ensure_project(c, &pid(P1), "Starpoint", None, 0)?;
                let ch = ensure_channel(c, &pid(P1), "main", 0)?;
                insert_message(c, &msg(ch.id, MemberId(1), "x")).map(|m| m.id)
            })
            .ok();
            let s2 = &mut s;
            s2.tx::<_, rusqlite::Error>(|c| {
                ensure_project(c, &pid(P1), "Starpoint", None, 0)?;
                let ch = ensure_channel(c, &pid(P1), "main", 0)?;
                let hx = ensure_member(c, MemberKind::Human, "hx", "Hx")?;
                insert_message(c, &msg(ch.id, hx.id, "hello")).map(|m| m.id)
            })
            .unwrap()
        };
        let s = Store::open(&path).unwrap();
        let m = message(s.conn(), id).unwrap().unwrap();
        assert_eq!(&*m.body, "hello");
        assert_eq!(s.schema_version().unwrap(), MIGRATIONS.len() as i64);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn seq_is_per_channel_and_gapless_while_ids_are_global() {
        let (mut s, ch, hx, _) = setup();
        let other = s
            .tx::<_, rusqlite::Error>(|c| ensure_channel(c, &ch.project, "STRM-1", 0))
            .unwrap();
        let (a, b, c2) = s
            .tx::<_, rusqlite::Error>(|c| {
                let a = insert_message(c, &msg(ch.id, hx.id, "a"))?;
                let b = insert_message(c, &msg(other.id, hx.id, "b"))?;
                let c2 = insert_message(c, &msg(ch.id, hx.id, "c"))?;
                Ok((a, b, c2))
            })
            .unwrap();
        assert_eq!((a.seq, b.seq, c2.seq), (1, 1, 2));
        assert!(a.id < b.id && b.id < c2.id);
    }

    #[test]
    fn one_pending_and_one_in_flight_batch_per_agent_channel() {
        let (mut s, ch, _, bot) = setup();
        s.tx::<_, rusqlite::Error>(|c| {
            let p = pending_batch_or_create(c, bot.id, ch.id, 0)?;
            assert_eq!(pending_batch_or_create(c, bot.id, ch.id, 0)?.id, p.id);
            let dup = c.execute(
                "INSERT INTO batches (agent_id, channel_id, state, created_at) VALUES (?1, ?2, 'pending', 0)",
                params![bot.id.0, ch.id.0],
            );
            assert!(dup.is_err(), "schema forbids a second pending batch");
            dispatch_batch(c, p.id, 0)?;
            let q = pending_batch_or_create(c, bot.id, ch.id, 0)?;
            assert_ne!(q.id, p.id);
            let dup = c.execute(
                "UPDATE batches SET state = 'dispatched' WHERE id = ?1",
                [q.id.0],
            );
            assert!(dup.is_err(), "schema forbids a second in-flight batch");
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn requeue_merges_triggers_of_a_newer_pending_batch() {
        let (mut s, ch, hx, bot) = setup();
        s.tx::<_, rusqlite::Error>(|c| {
            let m1 = insert_message(c, &msg(ch.id, hx.id, "one"))?;
            let m2 = insert_message(c, &msg(ch.id, hx.id, "two"))?;
            let b1 = pending_batch_or_create(c, bot.id, ch.id, 0)?;
            let t1 = TriggerId {
                agent: bot.id,
                channel: ch.id,
                message: m1.id,
            };
            insert_trigger(c, t1, WakeReason::Mention, 0, b1.id, 0)?;
            insert_obligation(c, t1)?;
            dispatch_batch(c, b1.id, 1)?;
            let b2 = pending_batch_or_create(c, bot.id, ch.id, 0)?;
            let t2 = TriggerId {
                agent: bot.id,
                channel: ch.id,
                message: m2.id,
            };
            insert_trigger(c, t2, WakeReason::Mention, 0, b2.id, 0)?;
            insert_obligation(c, t2)?;
            requeue_batch(c, b1.id)?;
            let p = pending_batch(c, bot.id, ch.id)?.unwrap();
            assert_eq!(p.id, b1.id);
            assert_eq!(batch_triggers(c, p.id)?.len(), 2);
            assert_eq!(open_obligations(c, p.id)?, vec![t1, t2]);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn duplicate_trigger_and_obligation_are_ignored() {
        let (mut s, ch, hx, bot) = setup();
        s.tx::<_, rusqlite::Error>(|c| {
            let m = insert_message(c, &msg(ch.id, hx.id, "@claude hi"))?;
            let b = pending_batch_or_create(c, bot.id, ch.id, 0)?;
            let t = TriggerId {
                agent: bot.id,
                channel: ch.id,
                message: m.id,
            };
            assert!(insert_trigger(c, t, WakeReason::Mention, 0, b.id, 0)?);
            assert!(!insert_trigger(c, t, WakeReason::Mention, 0, b.id, 0)?);
            assert!(insert_obligation(c, t)?);
            assert!(!insert_obligation(c, t)?);
            assert!(resolve_obligation(
                c,
                t,
                ObligationState::Satisfied,
                None,
                "send"
            )?);
            assert!(!resolve_obligation(
                c,
                t,
                ObligationState::Fallback,
                None,
                "fallback"
            )?);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn join_backfills_the_last_n_messages() {
        let (mut s, ch, hx, _) = setup();
        s.tx::<_, rusqlite::Error>(|c| {
            for i in 0..30 {
                insert_message(c, &msg(ch.id, hx.id, &format!("m{i}")))?;
            }
            let late = ensure_member(c, MemberKind::Agent, "codex", "Codex")?;
            join_project(c, &ch.project, late.id, 0)?;
            join(c, ch.id, &late, 20, 0)?;
            assert_eq!(cursor(c, late.id, ch.id)?.read_through, 10);
            advance_delivered(c, late.id, ch.id, 5, 0)?;
            assert_eq!(
                cursor(c, late.id, ch.id)?.delivered_through,
                10,
                "never backwards"
            );
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn two_projects_can_each_have_a_main() {
        let mut s = Store::open_in_memory().unwrap();
        s.tx::<_, rusqlite::Error>(|c| {
            ensure_project(c, &pid(P1), "Starpoint", Some("/src/starpoint"), 0)?;
            ensure_project(c, &pid(P2), "Everything", Some("/src/everything"), 0)?;
            let a = ensure_channel(c, &pid(P1), "main", 0)?;
            let b = ensure_channel(c, &pid(P2), "#main", 0)?;
            assert_ne!(a.id, b.id);
            assert_eq!((a.ordinal, b.ordinal), (1, 1), "::N counts per project");
            assert_eq!(&*b.name, "main");
            assert_eq!(ensure_channel(c, &pid(P1), "main", 5)?.id, a.id);
            let next = ensure_channel(c, &pid(P2), "STRM-184", 0)?;
            assert_eq!(next.ordinal, 2);
            assert_eq!(channel_in_project(c, &pid(P1), "main")?.unwrap().id, a.id);
            assert_eq!(channel_in_project(c, &pid(P2), "main")?.unwrap().id, b.id);
            assert!(channel_in_project(c, &pid(P1), "STRM-184")?.is_none());
            let names: Vec<_> = channels_in_project(c, &pid(P2))?
                .into_iter()
                .map(|c| c.name)
                .collect();
            assert_eq!(names, vec![Arc::from("main"), Arc::from("STRM-184")]);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn joining_a_channel_needs_project_membership() {
        let (mut s, ch, _, _) = setup();
        s.tx::<_, rusqlite::Error>(|c| {
            let other = ensure_project(c, &pid(P2), "Other", None, 0)?;
            let elsewhere = ensure_channel(c, &other.id, "main", 0)?;
            let codex = ensure_member(c, MemberKind::Agent, "codex", "Codex")?;
            let err = join(c, ch.id, &codex, 20, 0).unwrap_err();
            assert!(is_not_a_project_member(&err), "{err}");
            assert!(!is_member(c, ch.id, codex.id)?);
            join_project(c, &ch.project, codex.id, 0)?;
            assert!(join(c, ch.id, &codex, 20, 0)?);
            // Membership of one project grants nothing in another.
            let err = join(c, elsewhere.id, &codex, 20, 0).unwrap_err();
            assert!(is_not_a_project_member(&err), "{err}");
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn renaming_changes_the_name_only() {
        let (mut s, ch, hx, _) = setup();
        s.tx::<_, rusqlite::Error>(|c| {
            let m = insert_message(c, &msg(ch.id, hx.id, "before"))?;
            assert!(rename_channel(c, ch.id, "#general")?);
            assert!(rename_project(c, &ch.project, "Starpoint 2")?);
            let renamed = channel(c, ch.id)?.unwrap();
            assert_eq!((renamed.id, renamed.ordinal), (ch.id, ch.ordinal));
            assert_eq!(&*renamed.name, "general");
            assert_eq!(
                project(c, &ch.project)?.unwrap().name.as_ref(),
                "Starpoint 2"
            );
            assert_eq!(message(c, m.id)?.unwrap().channel, ch.id);
            Ok(())
        })
        .unwrap();
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

    /// Every row of `table` as text, ordered, so two snapshots compare exactly.
    fn dump(c: &Connection, table: &str, cols: &str) -> Vec<String> {
        let mut stmt = c
            .prepare(&format!("SELECT {cols} FROM {table} ORDER BY {cols}"))
            .unwrap();
        let n = stmt.column_count();
        stmt.query_map([], |r| {
            Ok((0..n)
                .map(|i| format!("{:?}", r.get_ref(i).unwrap()))
                .collect::<Vec<_>>()
                .join("|"))
        })
        .unwrap()
        .collect::<Result<_>>()
        .unwrap()
    }

    /// The tables migration 2 must leave exactly as they were.
    const UNTOUCHED: &[(&str, &str)] = &[
        ("members", "id, kind, handle, name"),
        ("memberships", "channel_id, member_id, joined_at"),
        (
            "messages",
            "id, channel_id, seq, author_id, origin, body, reply_to, hop, cause_batch, client_request_id, sent_at",
        ),
        ("mentions", "message_id, member_id"),
        ("reactions", "message_id, member_id, emoji, created_at"),
        (
            "cursors",
            "agent_id, channel_id, delivered_through, read_through",
        ),
        (
            "batches",
            "id, agent_id, channel_id, state, attempts, window_to, created_at, accepted_at, settled_at, outcome",
        ),
        (
            "triggers",
            "agent_id, channel_id, message_id, reason, hop, batch_id, created_at",
        ),
        (
            "obligations",
            "agent_id, channel_id, message_id, state, resolved_by, resolved_kind",
        ),
        (
            "sessions",
            "agent_id, channel_id, protocol, native_ref, opened_at",
        ),
    ];

    #[test]
    fn a_v1_database_migrates_keeping_every_id_and_row() {
        let dir = std::env::temp_dir().join(format!("cc-store-v1-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("v1.sqlite");
        let _ = std::fs::remove_file(&path);

        // A database exactly as v1 left it, with a row in every table.
        let before: Vec<Vec<String>> = {
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
            UNTOUCHED
                .iter()
                .map(|(t, cols)| dump(&conn, t, cols))
                .collect()
        };

        let s = Store::open(&path).unwrap();
        assert_eq!(s.schema_version().unwrap(), MIGRATIONS.len() as i64);
        let c = s.conn();
        let after: Vec<Vec<String>> = UNTOUCHED.iter().map(|(t, cols)| dump(c, t, cols)).collect();
        assert_eq!(before, after, "migration 2 must not touch these tables");

        let all = projects(c).unwrap();
        assert_eq!(all.len(), 1);
        let imported = all[0].clone();
        assert_eq!(imported.name.as_ref(), "Imported");
        assert_eq!(imported.root_path, None);
        assert_eq!(imported.created_at, 100);
        let chans = channels_in_project(c, &imported.id).unwrap();
        let got: Vec<_> = chans
            .iter()
            .map(|c| (c.id.0, &*c.name, c.ordinal))
            .collect();
        assert_eq!(got, vec![(3, "main", 1), (7, "STRM-1", 2)]);
        for m in [1, 2, 5] {
            assert!(is_project_member(c, &imported.id, MemberId(m)).unwrap());
        }
        let fk: i64 = c
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(fk, 0);
        let fk_on: i64 = c
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fk_on, 1, "foreign keys are back on after migrating");
        drop(s);

        // Reopening runs nothing again: same single project, same UUID.
        let s = Store::open(&path).unwrap();
        let again = projects(s.conn()).unwrap();
        assert_eq!(again, vec![imported]);
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_v3_database_gains_receipts_and_the_ignored_state() {
        let conn = Connection::open_in_memory().unwrap();
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
        let s = Store::init(conn).unwrap();
        let c = s.conn();
        assert_eq!(s.schema_version().unwrap(), MIGRATIONS.len() as i64);
        let t = TriggerId {
            agent: MemberId(2),
            channel: ChannelId(3),
            message: MessageId(10),
        };
        assert_eq!(
            obligation_state(c, t).unwrap(),
            Some(ObligationState::Unmet)
        );
        // Existing cursors were never stamped: no receipt until they move.
        let k = cursor(c, MemberId(2), ChannelId(3)).unwrap();
        assert_eq!((k.read_through, k.read_at), (1, None));
        c.execute(
            "UPDATE obligations SET state = 'ignored' WHERE message_id = 10",
            [],
        )
        .unwrap();
        assert_eq!(
            obligation_state(c, t).unwrap(),
            Some(ObligationState::Ignored)
        );
        let fk: i64 = c
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(fk, 0);
    }

    #[test]
    fn a_v4_database_gains_stances_and_the_refused_state() {
        let conn = Connection::open_in_memory().unwrap();
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
        let s = Store::init(conn).unwrap();
        let c = s.conn();
        assert_eq!(s.schema_version().unwrap(), MIGRATIONS.len() as i64);
        let t = TriggerId {
            agent: MemberId(2),
            channel: ChannelId(3),
            message: MessageId(10),
        };
        assert_eq!(
            obligation_state(c, t).unwrap(),
            Some(ObligationState::Ignored),
            "rows copied as they are"
        );
        c.execute(
            "UPDATE obligations SET state = 'refused' WHERE message_id = 10",
            [],
        )
        .unwrap();
        assert_eq!(
            obligation_state(c, t).unwrap(),
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
        insert_stance(c, &st).unwrap();
        insert_stance(c, &st).unwrap();
        assert_eq!(stance(c, MessageId(11)).unwrap(), Some(st.clone()));
        assert_eq!(recent_stances(c, ChannelId(3), 10).unwrap(), vec![st]);
        assert_eq!(stance(c, MessageId(10)).unwrap(), None);
        let fk: i64 = c
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(fk, 0);
    }

    #[test]
    fn read_cursors_stamp_when_they_move_and_never_go_back() {
        let (s, channel, hx, _) = setup();
        let c = s.conn();
        let (a, ch) = (hx.id, channel.id);
        assert!(advance_read(c, a, ch, 5, 100).unwrap());
        assert!(!advance_read(c, a, ch, 3, 200).unwrap(), "never backwards");
        assert!(
            !advance_read(c, a, ch, 5, 300).unwrap(),
            "no move, no stamp"
        );
        let k = cursor(c, a, ch).unwrap();
        assert_eq!((k.read_through, k.read_at), (5, Some(100)));
        // Delivery carries read along.
        assert!(advance_delivered(c, a, ch, 8, 400).unwrap());
        let k = cursor(c, a, ch).unwrap();
        assert_eq!(
            (k.delivered_through, k.read_through, k.read_at),
            (8, 8, Some(400))
        );
        assert!(!advance_delivered(c, a, ch, 6, 500).unwrap());
        assert_eq!(cursor(c, a, ch).unwrap().read_at, Some(400));
    }

    #[test]
    fn a_fresh_database_has_no_imported_project() {
        let s = Store::open_in_memory().unwrap();
        assert!(projects(s.conn()).unwrap().is_empty());
        assert!(unclaimed_import(s.conn()).unwrap().is_none());
    }

    /// A v1 database with one channel and one message, migrated.
    fn migrated_v1() -> Store {
        let conn = Connection::open_in_memory().unwrap();
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
        Store::init(conn).unwrap()
    }

    #[test]
    fn adopting_the_import_gives_it_the_folder_once() {
        let mut s = migrated_v1();
        s.tx::<_, rusqlite::Error>(|c| {
            let imported = unclaimed_import(c)?.expect("migration 2 made one");
            assert_eq!(imported.name.as_ref(), "Imported");
            // The placeholder opened while the user was asked holds the folder.
            ensure_project(c, &pid(P1), "starpoint", Some("/src/starpoint"), 2000)?;
            let placeholder = ensure_channel(c, &pid(P1), "main", 2000)?;

            let adopted =
                adopt_import(c, &imported.id, "starpoint", "/src/starpoint")?.expect("adoptable");
            assert_eq!(adopted.id, imported.id, "same UUID, same history");
            assert_eq!(adopted.name.as_ref(), "starpoint");
            assert_eq!(adopted.root_path.as_deref(), Some("/src/starpoint"));
            assert_eq!(
                project_by_root(c, "/src/starpoint")?.unwrap().id,
                imported.id
            );
            assert_eq!(project(c, &pid(P1))?.unwrap().root_path, None);
            let main = channel_in_project(c, &imported.id, "main")?.unwrap();
            assert_eq!(main.id, ChannelId(3), "channel ids are kept");
            assert!(project_has_messages(c, &imported.id)?);

            // Claimed: never offered or adoptable again, even from elsewhere.
            assert!(unclaimed_import(c)?.is_none());
            assert!(adopt_import(c, &imported.id, "other", "/src/other")?.is_none());
            assert_eq!(
                project(c, &imported.id)?.unwrap().root_path.as_deref(),
                Some("/src/starpoint")
            );

            // The empty placeholder goes; the adopted project never does.
            assert_eq!(
                forget_empty_project(c, &pid(P1))?,
                Some(vec![placeholder.id])
            );
            assert!(project(c, &pid(P1))?.is_none());
            assert_eq!(forget_empty_project(c, &imported.id)?, None);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn a_project_with_a_folder_is_never_adoptable() {
        let mut s = migrated_v1();
        s.tx::<_, rusqlite::Error>(|c| {
            let imported = unclaimed_import(c)?.unwrap();
            // A project that had a folder and lost it is not an import.
            ensure_project(c, &pid(P1), "Old", Some("/src/x"), 0)?;
            ensure_project(c, &pid(P2), "New", Some("/src/x"), 1)?;
            assert_eq!(project(c, &pid(P1))?.unwrap().root_path, None);
            assert!(adopt_import(c, &pid(P1), "x", "/src/y")?.is_none());
            assert!(adopt_import(c, &pid(P2), "x", "/src/y")?.is_none());
            // An import that somehow has a folder is not offered either.
            c.execute(
                "UPDATE projects SET root_path = '/src/live' WHERE id = ?1",
                [imported.id.as_str()],
            )?;
            assert!(unclaimed_import(c)?.is_none());
            assert!(adopt_import(c, &imported.id, "x", "/src/y")?.is_none());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn only_empty_unrooted_projects_are_forgotten() {
        let (mut s, ch, hx, _) = setup();
        s.tx::<_, rusqlite::Error>(|c| {
            // `setup`'s project has no folder but has a message: kept.
            insert_message(c, &msg(ch.id, hx.id, "keep me"))?;
            assert_eq!(forget_empty_project(c, &ch.project)?, None);
            // Empty but rooted: kept.
            ensure_project(c, &pid(P2), "Copy", Some("/src/copy"), 0)?;
            let main = ensure_channel(c, &pid(P2), "main", 0)?;
            join_project(c, &pid(P2), hx.id, 0)?;
            join(c, main.id, &hx, 20, 0)?;
            assert_eq!(forget_empty_project(c, &pid(P2))?, None);
            // Empty and unrooted (another project took the folder): forgotten,
            // memberships and all.
            ensure_project(c, &ch.project, "Starpoint", Some("/src/copy"), 1)?;
            assert_eq!(forget_empty_project(c, &pid(P2))?, Some(vec![main.id]));
            assert!(project(c, &pid(P2))?.is_none());
            assert!(!is_member(c, main.id, hx.id)?);
            assert!(!is_project_member(c, &pid(P2), hx.id)?);
            Ok(())
        })
        .unwrap();
    }
}
