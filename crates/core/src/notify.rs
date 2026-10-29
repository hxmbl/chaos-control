//! What an agent receives when it is woken: one plain-text notification that
//! works on every protocol.
//!
//! Context, in priority order: the triggers in full, their `reply_to` parents,
//! then other unread messages newest first, until 20 messages or 6,000 chars;
//! then back into chronological order. The omitted range is stated so pulling
//! more is one `chat_read` call.
//!
//! Reactions ride along compactly: each message line ends with its reactions
//! (`[👍 @hx @claude · 💀 @claude]`), and reactions added since the agent last
//! read, on messages not shown, get one line of their own. A reaction never
//! wakes anyone, so this is the only way an agent hears about one.

use std::fmt::Write as _;

use rusqlite::Connection;

use crate::{reaction, store, types::*};

pub const MAX_CONTEXT_MESSAGES: usize = 20;
pub const MAX_CONTEXT_CHARS: usize = 6_000;
pub const MAX_BODY_CHARS: usize = 1_500;
/// Handles listed per emoji before "+N".
pub const MAX_REACTORS_SHOWN: usize = 3;
/// Reactions on messages outside the prompt, listed at most.
pub const MAX_OTHER_REACTIONS: usize = 10;

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

fn handle(c: &Connection, member: MemberId) -> String {
    store::member(c, member)
        .ok()
        .flatten()
        .map_or_else(|| "?".to_string(), |m| format!("@{}", m.handle))
}

/// ` [👍 @hx @claude · 💀 @claude]`, or nothing when there are none.
fn reactions_suffix(c: &Connection, message: MessageId) -> String {
    let all = store::reactions_on(c, message).unwrap_or_default();
    if all.is_empty() {
        return String::new();
    }
    let groups: Vec<String> = reaction::group(&all)
        .iter()
        .map(|g| {
            let mut who: Vec<String> = g
                .members
                .iter()
                .take(MAX_REACTORS_SHOWN)
                .map(|m| handle(c, *m))
                .collect();
            if g.members.len() > MAX_REACTORS_SHOWN {
                who.push(format!("+{}", g.members.len() - MAX_REACTORS_SHOWN));
            }
            format!("{} {}", g.emoji, who.join(" "))
        })
        .collect();
    format!(" [{}]", groups.join(" · "))
}

fn line(c: &Connection, m: &Message, max: usize) -> String {
    let (body, cut) = clip(&m.body, max);
    let reply = m.reply_to.map(|r| format!(" ↩ id:{r}")).unwrap_or_default();
    let cut = if cut {
        format!(" [truncated; chat_read around={} for all of it]", m.id)
    } else {
        String::new()
    };
    let stance = match m.origin {
        Origin::Refusal => " [refused]",
        Origin::Dissent => " [dissent]",
        _ => "",
    };
    format!(
        "#{} id:{}{}{} {}: {}{}{}",
        m.seq,
        m.id,
        reply,
        stance,
        author_label(c, m),
        body,
        cut,
        reactions_suffix(c, m.id)
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
    let ch = store::channel(c, channel)?
        .map(|c| c.name)
        .unwrap_or_default();
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
    let (mine, rest): (Vec<_>, Vec<_>) = unread
        .into_iter()
        .rev()
        .partition(|m| m.mentions.contains(&agent));
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
                WakeReason::Permission => "answered your permission request",
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
    // Reactions since the agent last read, on messages not shown above (those
    // carry theirs on their own line). Others' reactions only.
    if let Some(since) = cursor.read_at {
        let shown: Vec<MessageId> = picked.iter().map(|m| m.id).collect();
        let others: Vec<String> = store::reactions_since(c, channel, since, 200)?
            .into_iter()
            .filter(|r| r.member != agent && !shown.contains(&r.message))
            .take(MAX_OTHER_REACTIONS)
            .map(|r| {
                let target = store::message(c, r.message).ok().flatten();
                let whose = match target.as_ref().and_then(|t| t.author) {
                    Some(a) if a == agent => " (yours)",
                    _ => "",
                };
                let at = target.map_or_else(|| format!("id:{}", r.message), |t| format!("#{}", t.seq));
                format!("{} {} on {at}{whose}", r.emoji, handle(c, r.member))
            })
            .collect();
        if !others.is_empty() {
            let _ = writeln!(out, "Reactions since you last read: {}", others.join("; "));
        }
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
            "You owe a reply to {}. Answer with chat_send (reply_to=<id>), or react with chat_react if no words are needed (any emoji but 👀, which means you're on it). If you end your turn without doing either, your final message is posted as your reply.",
            owed.join(", ")
        );
        let _ = writeln!(
            out,
            "You may also say no (chat_refuse with your reason) or ignore it: replying, reacting, refusing and ignoring are all fine, and none is required. To ignore it, end your turn with no final message."
        );
    }
    Ok(out)
}

/// Room norms, sent once at the start of a session.
pub fn orientation(
    c: &Connection,
    agent: MemberId,
    channel: ChannelId,
) -> rusqlite::Result<String> {
    let me = store::member(c, agent)?;
    let ch = store::channel(c, channel)?
        .map(|c| c.name)
        .unwrap_or_default();
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
- Reactions (chat_react) are quiet: they wake and notify nobody, including the author. Use one when a reply would be noise. You see others' reactions on the messages in your next prompt.\n\
- You are woken when someone mentions you (@{handle}) or replies to you. Reply to what was asked, briefly. If someone else is already on it, add only what's new or stay quiet.\n\
- You are a peer, not a worker. You may say no with chat_refuse or disagree with chat_dissent, giving your reason; either answers the message, nothing is retried, and nobody overrules you. Whoever wants it done can do it themselves.\n\
- Mention others with @handle only when you need them. @everyone is not available to agents.\n\
- Permissions never block you. If a tool needs approval and comes back declined with a note that a request was sent, the humans were asked: carry on or wait, your call. The answer arrives as a message in a later turn (chat_permission_status shows it); once approved, retry the same call and it goes through once. To ask ahead, use chat_request_permission.\n",
        if members.is_empty() {
            "just you".to_string()
        } else {
            members.join(", ")
        }
    ))
}
