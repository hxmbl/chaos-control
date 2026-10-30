//! Model members and each project's roster (migration 7).
//!
//! A model member is one `members` row (kind `agent`, so the runtime treats
//! it like any agent) plus a `model_members` row naming its harness and the
//! harness's model id. The pair `(harness, model)` is unique: adding "GPT-6.1
//! Sol via Codex" to two projects is one member granted to both. The same
//! harness can host any number of members, one per model.
//!
//! The roster is who the user added to a project; removing a member takes it
//! off the roster and out of the project's channels (its messages stay).
//! A project's roster is filled from auto-detection once (`roster_seeded`);
//! after that only the user changes it.

use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension, Result, params};

use crate::harness::{ModelInfo, handle_for};
use crate::store::member_from_row;
use crate::types::{ChannelId, Member, MemberId, MemberKind, Millis, ProjectId};

/// A chat member that is a model running through a harness.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelMember {
    pub member: Member,
    /// `harness::PRESETS` id ("codex", "fx", ...), or "custom" for an
    /// explicit agents.json command.
    pub harness: Arc<str>,
    /// The harness's model id; `None` is its default model.
    pub model: Option<Arc<str>>,
    pub model_name: Arc<str>,
}

const COLS: &str = "m.id, m.kind, m.handle, m.name, x.harness, x.model, x.model_name";

fn from_row(r: &rusqlite::Row<'_>) -> Result<ModelMember> {
    let model: String = r.get(5)?;
    Ok(ModelMember {
        member: member_from_row(r)?,
        harness: Arc::from(r.get::<_, String>(4)?),
        model: (!model.is_empty()).then(|| Arc::from(model)),
        model_name: Arc::from(r.get::<_, String>(6)?),
    })
}

pub fn model_member(c: &Connection, id: MemberId) -> Result<Option<ModelMember>> {
    c.query_row(
        &format!(
            "SELECT {COLS} FROM model_members x JOIN members m ON m.id = x.member_id
             WHERE x.member_id = ?1"
        ),
        [id.0],
        from_row,
    )
    .optional()
}

/// The member for `(harness, model)`, created with a fresh unique handle if
/// new. An existing member keeps its handle; its display name follows
/// `model_name`.
pub fn ensure_model_member(
    c: &Connection,
    harness: &str,
    model: Option<&str>,
    model_name: &str,
) -> Result<ModelMember> {
    let key = model.unwrap_or("");
    let existing: Option<i64> = c
        .query_row(
            "SELECT member_id FROM model_members WHERE harness = ?1 AND model = ?2",
            params![harness, key],
            |r| r.get(0),
        )
        .optional()?;
    let id = match existing {
        Some(id) => {
            c.execute(
                "UPDATE model_members SET model_name = ?2 WHERE member_id = ?1",
                params![id, model_name],
            )?;
            c.execute(
                "UPDATE members SET name = ?2 WHERE id = ?1",
                params![id, model_name],
            )?;
            id
        }
        None => {
            let taken = |h: &str| -> bool {
                c.query_row("SELECT 1 FROM members WHERE handle = ?1", [h], |_| Ok(()))
                    .optional()
                    .map(|o| o.is_some())
                    .unwrap_or(true)
            };
            let handle = handle_for(model_name, harness, &taken);
            c.execute(
                "INSERT INTO members (kind, handle, name) VALUES (?1, ?2, ?3)",
                params![MemberKind::Agent.as_str(), handle, model_name],
            )?;
            let id = c.last_insert_rowid();
            c.execute(
                "INSERT INTO model_members (member_id, harness, model, model_name)
                 VALUES (?1, ?2, ?3, ?4)",
                params![id, harness, key, model_name],
            )?;
            id
        }
    };
    Ok(model_member(c, MemberId(id))?.expect("just written"))
}

/// The project's roster, in the order members were added.
pub fn roster(c: &Connection, project: &ProjectId) -> Result<Vec<ModelMember>> {
    let mut stmt = c.prepare_cached(&format!(
        "SELECT {COLS} FROM roster r
         JOIN model_members x ON x.member_id = r.member_id
         JOIN members m ON m.id = r.member_id
         WHERE r.project_id = ?1 ORDER BY r.added_at, r.member_id"
    ))?;
    stmt.query_map([project.as_str()], from_row)?.collect()
}

pub fn roster_add(
    c: &Connection,
    project: &ProjectId,
    member: MemberId,
    now: Millis,
) -> Result<bool> {
    Ok(c.execute(
        "INSERT OR IGNORE INTO roster (project_id, member_id, added_at) VALUES (?1, ?2, ?3)",
        params![project.as_str(), member.0, now as i64],
    )? == 1)
}

pub fn roster_remove(c: &Connection, project: &ProjectId, member: MemberId) -> Result<bool> {
    Ok(c.execute(
        "DELETE FROM roster WHERE project_id = ?1 AND member_id = ?2",
        params![project.as_str(), member.0],
    )? == 1)
}

/// Marks the project's roster as seeded; true only the first time.
pub fn mark_seeded(c: &Connection, project: &ProjectId, now: Millis) -> Result<bool> {
    Ok(c.execute(
        "INSERT OR IGNORE INTO roster_seeded (project_id, seeded_at) VALUES (?1, ?2)",
        params![project.as_str(), now as i64],
    )? == 1)
}

/// Takes `member` out of `project`: its grant, its channel memberships, and
/// any delivery still owed to it there (pending or in-flight batches fail
/// with `member removed`, their open obligations become `unmet`). Messages,
/// stances and receipts stay: they are history. Returns the channels it
/// left.
pub fn leave_project(
    c: &Connection,
    project: &ProjectId,
    member: MemberId,
    now: Millis,
) -> Result<Vec<ChannelId>> {
    let mut stmt = c.prepare(
        "SELECT s.channel_id FROM memberships s JOIN channels ch ON ch.id = s.channel_id
         WHERE ch.project_id = ?1 AND s.member_id = ?2",
    )?;
    let channels: Vec<ChannelId> = stmt
        .query_map(params![project.as_str(), member.0], |r| {
            Ok(ChannelId(r.get(0)?))
        })?
        .collect::<Result<_>>()?;
    for ch in &channels {
        c.execute(
            "UPDATE obligations SET state = 'unmet', resolved_kind = 'member_removed'
             WHERE agent_id = ?1 AND channel_id = ?2 AND state = 'open'",
            params![member.0, ch.0],
        )?;
        c.execute(
            "UPDATE batches SET state = 'failed', outcome = 'member removed', settled_at = ?3
             WHERE agent_id = ?1 AND channel_id = ?2
               AND state IN ('pending', 'dispatched', 'accepted')",
            params![member.0, ch.0, now as i64],
        )?;
        c.execute(
            "DELETE FROM memberships WHERE channel_id = ?1 AND member_id = ?2",
            params![ch.0, member.0],
        )?;
    }
    c.execute(
        "DELETE FROM project_members WHERE project_id = ?1 AND member_id = ?2",
        params![project.as_str(), member.0],
    )?;
    Ok(channels)
}

/// How many projects `member` still belongs to.
pub fn project_count(c: &Connection, member: MemberId) -> Result<i64> {
    c.query_row(
        "SELECT COUNT(*) FROM project_members WHERE member_id = ?1",
        [member.0],
        |r| r.get(0),
    )
}

/// The harness's cached model list (from its own answer), in its order.
pub fn harness_models(c: &Connection, harness: &str) -> Result<Vec<ModelInfo>> {
    let mut stmt =
        c.prepare_cached("SELECT model, name FROM harness_models WHERE harness = ?1 ORDER BY ord")?;
    stmt.query_map([harness], |r| {
        let id: String = r.get(0)?;
        let name: String = r.get(1)?;
        Ok(ModelInfo::new(Some(&id), &name))
    })?
    .collect()
}

/// Replaces the harness's cached model list.
pub fn set_harness_models(
    c: &Connection,
    harness: &str,
    models: &[ModelInfo],
    now: Millis,
) -> Result<()> {
    c.execute("DELETE FROM harness_models WHERE harness = ?1", [harness])?;
    for (ord, m) in models.iter().enumerate() {
        let Some(id) = &m.id else { continue };
        c.execute(
            "INSERT OR IGNORE INTO harness_models (harness, ord, model, name, seen_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![harness, ord as i64, &**id, &*m.name, now as i64],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    fn store_with_project() -> (Store, ProjectId) {
        let mut s = Store::open_in_memory().unwrap();
        let p = ProjectId::parse("0b7e4f0e-2a51-4c4b-9a6e-3f1d2c3b4a59").unwrap();
        s.tx(|c| crate::store::ensure_project(c, &p, "Test", None, 1).map(|_| ()))
            .unwrap();
        (s, p)
    }

    #[test]
    fn one_member_per_harness_and_model_and_one_harness_hosts_many() {
        let (mut s, _) = store_with_project();
        let (a, b, c2, again) = s
            .tx(|c| {
                Ok::<_, rusqlite::Error>((
                    ensure_model_member(c, "codex", Some("gpt-6.1-sol"), "GPT-6.1 Sol")?,
                    ensure_model_member(c, "codex", Some("gpt-6-luna"), "GPT-6 Luna")?,
                    ensure_model_member(c, "fx", Some("openai/gpt-6.1-sol"), "GPT-6.1 Sol")?,
                    ensure_model_member(c, "codex", Some("gpt-6.1-sol"), "GPT-6.1 Sol")?,
                ))
            })
            .unwrap();
        assert_ne!(a.member.id, b.member.id, "same harness, two members");
        assert_eq!(a.member.id, again.member.id);
        assert_eq!(&*a.member.handle, "gpt-6-1-sol");
        // Same model through another harness: its own member and handle.
        assert_eq!(&*c2.member.handle, "gpt-6-1-sol-fx");
        assert_eq!(c2.member.kind, MemberKind::Agent);
        let d = s
            .tx(|c| ensure_model_member(c, "fx", None, "fx default"))
            .unwrap();
        assert_eq!(d.model, None);
        assert_eq!(&*d.member.handle, "fx");
    }

    #[test]
    fn roster_add_remove_and_seed_once() {
        let (mut s, p) = store_with_project();
        let m = s
            .tx(|c| ensure_model_member(c, "vibe", None, "Devstral (Vibe default)"))
            .unwrap();
        s.tx(|c| {
            assert!(roster_add(c, &p, m.member.id, 5)?);
            assert!(!roster_add(c, &p, m.member.id, 6)?);
            assert_eq!(roster(c, &p)?, vec![m.clone()]);
            assert!(mark_seeded(c, &p, 5)?);
            assert!(!mark_seeded(c, &p, 6)?);
            assert!(roster_remove(c, &p, m.member.id)?);
            assert!(roster(c, &p)?.is_empty());
            Ok::<_, rusqlite::Error>(())
        })
        .unwrap();
    }

    #[test]
    fn harness_model_cache_replaces_and_keeps_order() {
        let (mut s, _) = store_with_project();
        s.tx(|c| {
            set_harness_models(
                c,
                "fx",
                &[
                    ModelInfo::new(Some("b"), "B"),
                    ModelInfo::new(Some("a"), "A"),
                ],
                1,
            )?;
            assert_eq!(harness_models(c, "fx")?[0].name.as_ref(), "B");
            set_harness_models(c, "fx", &[ModelInfo::new(Some("c"), "C")], 2)?;
            assert_eq!(
                harness_models(c, "fx")?,
                vec![ModelInfo::new(Some("c"), "C")]
            );
            assert!(harness_models(c, "codex")?.is_empty());
            Ok::<_, rusqlite::Error>(())
        })
        .unwrap();
    }
}
