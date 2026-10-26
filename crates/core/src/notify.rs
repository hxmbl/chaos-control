//! What an agent receives when it is woken: one plain-text notification that
//! works on every protocol.
//!
//! Context, in priority order: the triggers in full, their `reply_to` parents,
//! then other unread messages newest first, until 20 messages or 6,000 chars;
//! then back into chronological order. The omitted range is stated so pulling
//! more is one `chat_read` call.

use std::fmt::Write as _;

use rusqlite::Connection;

use crate::{store, types::*};

pub const MAX_CONTEXT_MESSAGES: usize = 20;
pub const MAX_CONTEXT_CHARS: usize = 6_000;
pub const MAX_BODY_CHARS: usize = 1_500;

fn author_label(c: &Connection, m: &Message) -> String {
    match m.author.and_then(|a| store::member(c, a).ok().flatten()) {
        Some(who) => format!("{} (@{}, {})", who.name, who.handle, who.kind.as_str()),
        None => "system".into(),
    }
}

fn clip(body: &str, max: usize) -> (String, bool) {
    if body.chars().count() <= max {
        (body.to_string(), false)
    } else {
        (body.chars().take(max).collect::<String>() + "…", true)
    }
}

fn line(c: &Connection, m: &Message, max: usize) -> String {
    let (body, cut) = clip(&m.body, max);
    let reply = m
        .reply_to
        .map(|r| format!(" ↩ id:{r}"))
        .unwrap_or_default();
    let cut = if cut {
        format!(" [truncated; chat_read around={} for all of it]", m.id)
    } else {
        String::new()
    };
    format!(
        "#{} id:{}{} {}: {}{}",
        m.seq,
        m.id,
        reply,
        author_label(c, m),
        body,
        cut
    )
}

/// The turn prompt for one batch.
pub fn compose(
    c: &Connection,
    agent: MemberId,
    channel: ChannelId,
    batch: BatchId,
    window_to: Seq,
) -> rusqlite::Result<String> {
    let ch = store::channel(c, channel)?.map(|c| c.name).unwrap_or_default();
    let cursor = store::cursor(c, agent, channel)?;
    let triggers = store::batch_triggers(c, batch)?;
    let mut trigger_msgs = Vec::new();
    for t in &triggers {
        if let Some(m) = store::message(c, t.id.message)? {
            trigger_msgs.push((t.clone(), m));
        }
    }

    // Pick context.
    let mut picked: Vec<Message> = Vec::new();
    let mut chars = 0usize;
    let push = |m: Message, picked: &mut Vec<Message>, chars: &mut usize| {
        if !picked.iter().any(|p| p.id == m.id) {
            *chars += m.body.len().min(MAX_BODY_CHARS);
            picked.push(m);
        }
    };
    for (_, m) in &trigger_msgs {
        push(m.clone(), &mut picked, &mut chars);
    }
    for (_, m) in &trigger_msgs {
        if let Some(p) = m.reply_to.and_then(|r| store::message(c, r).ok().flatten()) {
            push(p, &mut picked, &mut chars);
        }
    }
    let unread_from = cursor.read_through;
    let unread = store::messages_between(c, channel, unread_from, window_to, 10_000)?;
    let unread_total = unread.len();
    // Unread that mention this agent first, then the rest, newest first.
    let (mine, rest): (Vec<_>, Vec<_>) = unread.into_iter().rev().partition(|m| m.mentions.contains(&agent));
    for m in mine.into_iter().chain(rest) {
        if picked.len() >= MAX_CONTEXT_MESSAGES || chars >= MAX_CONTEXT_CHARS {
            break;
        }
        push(m, &mut picked, &mut chars);
    }
    picked.sort_by_key(|m| m.seq);
    let shown_unread = picked
        .iter()
        .filter(|m| m.seq > unread_from && m.seq <= window_to)
        .count();
    let omitted = unread_total.saturating_sub(shown_unread);

    let mut out = String::new();
    let who: Vec<String> = trigger_msgs
        .iter()
        .map(|(t, m)| {
            let how = match t.reason {
                WakeReason::Mention => "mentioned you",
                WakeReason::ReplyToMe => "replied to you",
                WakeReason::Everyone => "pinged @everyone",
            };
            format!("{} {how}", author_label(c, m))
        })
        .collect();
    let _ = writeln!(out, "[chaos-control] #{ch} · {}", who.join("; "));
    if omitted > 0 {
        let first_shown = picked
            .iter()
            .find(|m| m.seq > unread_from)
            .map(|m| m.seq)
            .unwrap_or(window_to);
        let _ = writeln!(
            out,
            "— new since you last read ({shown_unread} of {unread_total} unread shown; older: chat_read after_seq={unread_from} before_seq={first_shown}) —"
        );
    } else {
        let _ = writeln!(out, "— new since you last read —");
    }
    for m in &picked {
        let max = if trigger_msgs.iter().any(|(_, t)| t.id == m.id) {
            4 * MAX_BODY_CHARS
        } else {
            MAX_BODY_CHARS
        };
        let _ = writeln!(out, "{}", line(c, m, max));
    }
    let owed: Vec<String> = trigger_msgs
        .iter()
        .filter(|(t, _)| {
            t.reason.is_direct()
                && store::obligation_state(c, t.id).ok().flatten() == Some(ObligationState::Open)
        })
        .map(|(_, m)| format!("id:{} (#{})", m.id, m.seq))
        .collect();
    if owed.is_empty() {
        let _ = writeln!(
            out,
            "No reply is owed. Post with chat_send only if you have something useful to add."
        );
    } else {
        let _ = writeln!(
            out,
            "You owe a reply to {}. Answer with chat_send (reply_to=<id>), or react ✅ if no words are needed. If you end your turn without doing either, your final message is posted as your reply.",
            owed.join(", ")
        );
    }
    Ok(out)
}

/// Room norms, sent once at the start of a session.
pub fn orientation(c: &Connection, agent: MemberId, channel: ChannelId) -> rusqlite::Result<String> {
    let me = store::member(c, agent)?;
    let ch = store::channel(c, channel)?.map(|c| c.name).unwrap_or_default();
    let members: Vec<String> = store::members_of(c, channel)?
        .into_iter()
        .filter(|m| m.id != agent)
        .map(|m| format!("@{} ({})", m.handle, m.kind.as_str()))
        .collect();
    let handle = me.map(|m| m.handle).unwrap_or_default();
    Ok(format!(
        "You are @{handle} in #{ch} on Chaos Control, a group chat where humans and agents work things out together. \
Members: {}.\n\
How this room works:\n\
- You speak only through the chaos-control tools: chat_send to post, chat_react to react, chat_read / chat_search to look back, chat_context for who's here, chat_status for a one-line status. Your other output is private working-out; nobody sees your tool calls.\n\
- You are woken when someone mentions you (@{handle}) or replies to you. Reply to what was asked, briefly. If someone else is already on it, add only what's new or stay quiet.\n\
- Mention others with @handle only when you need them. @everyone is not available to agents.\n\
- Chaos Control never approves permission requests for you; if a tool needs approval it will be declined.\n",
        if members.is_empty() { "just you".to_string() } else { members.join(", ") }
    ))
}
