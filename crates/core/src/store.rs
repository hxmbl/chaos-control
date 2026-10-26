//! Durable state in SQLite: channels, members, messages, cursors, triggers,
//! batches and reply obligations.
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

use std::{path::Path, sync::Arc};

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::types::*;

pub type Result<T> = rusqlite::Result<T>;

/// Ordered migrations. `PRAGMA user_version` records how many have run; add new
/// ones at the end, never edit old ones.
const MIGRATIONS: &[&str] = &[r#"
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
"#];

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
        for (ix, sql) in MIGRATIONS.iter().enumerate().skip(version) {
            let tx = self.conn.transaction()?;
            tx.execute_batch(sql)?;
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

// ---------- channels & members ----------

pub fn ensure_channel(c: &Connection, name: &str, now: Millis) -> Result<Channel> {
    c.execute(
        "INSERT INTO channels (name, created_at) VALUES (?1, ?2) ON CONFLICT(name) DO NOTHING",
        params![name, now as i64],
    )?;
    c.query_row(
        "SELECT id, name FROM channels WHERE name = ?1",
        [name],
        |r| {
            Ok(Channel {
                id: ChannelId(r.get(0)?),
                name: Arc::from(r.get::<_, String>(1)?),
            })
        },
    )
}

pub fn channel(c: &Connection, id: ChannelId) -> Result<Option<Channel>> {
    c.query_row("SELECT id, name FROM channels WHERE id = ?1", [id.0], |r| {
        Ok(Channel {
            id: ChannelId(r.get(0)?),
            name: Arc::from(r.get::<_, String>(1)?),
        })
    })
    .optional()
}

pub fn channel_by_name(c: &Connection, name: &str) -> Result<Option<Channel>> {
    c.query_row(
        "SELECT id, name FROM channels WHERE name = ?1",
        [name.trim_start_matches('#')],
        |r| {
            Ok(Channel {
                id: ChannelId(r.get(0)?),
                name: Arc::from(r.get::<_, String>(1)?),
            })
        },
    )
    .optional()
}

fn member_from_row(r: &Row<'_>) -> Result<Member> {
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

pub fn members_of(c: &Connection, channel: ChannelId) -> Result<Vec<Member>> {
    let mut stmt = c.prepare_cached(
        "SELECT m.id, m.kind, m.handle, m.name FROM members m
         JOIN memberships s ON s.member_id = m.id
         WHERE s.channel_id = ?1 ORDER BY m.id",
    )?;
    stmt.query_map([channel.0], member_from_row)?.collect()
}

pub fn channels_of(c: &Connection, member: MemberId) -> Result<Vec<Channel>> {
    let mut stmt = c.prepare_cached(
        "SELECT c.id, c.name FROM channels c JOIN memberships s ON s.channel_id = c.id
         WHERE s.member_id = ?1 ORDER BY c.id",
    )?;
    stmt.query_map([member.0], |r| {
        Ok(Channel {
            id: ChannelId(r.get(0)?),
            name: Arc::from(r.get::<_, String>(1)?),
        })
    })?
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
        "SELECT delivered_through, read_through FROM cursors WHERE agent_id = ?1 AND channel_id = ?2",
        params![agent.0, channel.0],
        |r| {
            Ok(Cursor {
                delivered_through: r.get::<_, i64>(0)? as Seq,
                read_through: r.get::<_, i64>(1)? as Seq,
            })
        },
    )
    .optional()
    .map(|o| {
        o.unwrap_or(Cursor {
            delivered_through: 0,
            read_through: 0,
        })
    })
}

/// Moves `delivered_through` (and `read_through` with it) forward to `seq`.
/// Never moves a cursor backwards.
pub fn advance_delivered(
    c: &Connection,
    agent: MemberId,
    channel: ChannelId,
    seq: Seq,
) -> Result<()> {
    c.execute(
        "INSERT INTO cursors (agent_id, channel_id, delivered_through, read_through)
         VALUES (?1, ?2, ?3, ?3)
         ON CONFLICT(agent_id, channel_id) DO UPDATE SET
            delivered_through = MAX(delivered_through, excluded.delivered_through),
            read_through = MAX(read_through, excluded.delivered_through)",
        params![agent.0, channel.0, seq as i64],
    )?;
    Ok(())
}

/// Moves `read_through` forward to `seq` (forward `chat_read`).
pub fn advance_read(c: &Connection, agent: MemberId, channel: ChannelId, seq: Seq) -> Result<()> {
    c.execute(
        "INSERT INTO cursors (agent_id, channel_id, delivered_through, read_through)
         VALUES (?1, ?2, 0, ?3)
         ON CONFLICT(agent_id, channel_id) DO UPDATE SET
            read_through = MAX(read_through, excluded.read_through)",
        params![agent.0, channel.0, seq as i64],
    )?;
    Ok(())
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

    fn setup() -> (Store, Channel, Member, Member) {
        let mut s = Store::open_in_memory().unwrap();
        let (ch, hx, bot) = s
            .tx::<_, rusqlite::Error>(|c| {
                let ch = ensure_channel(c, "main", 0)?;
                let hx = ensure_member(c, MemberKind::Human, "hx", "Hx")?;
                let bot = ensure_member(c, MemberKind::Agent, "claude", "Claude")?;
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
                let ch = ensure_channel(c, "main", 0)?;
                insert_message(c, &msg(ch.id, MemberId(1), "x")).map(|m| m.id)
            })
            .ok();
            let s2 = &mut s;
            s2.tx::<_, rusqlite::Error>(|c| {
                let ch = ensure_channel(c, "main", 0)?;
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
            .tx::<_, rusqlite::Error>(|c| ensure_channel(c, "STRM-1", 0))
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
            join(c, ch.id, &late, 20, 0)?;
            assert_eq!(cursor(c, late.id, ch.id)?.read_through, 10);
            advance_delivered(c, late.id, ch.id, 5)?;
            assert_eq!(
                cursor(c, late.id, ch.id)?.delivered_through,
                10,
                "never backwards"
            );
            Ok(())
        })
        .unwrap();
    }
}
