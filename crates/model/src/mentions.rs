//! How inline `@mentions` read in the chat (STRM-561).
//!
//! A message keeps the text it was sent with. Only what the chat draws
//! changes: a mention of an agent that is no longer a member, by its handle
//! or a nickname, reads as `@previous-agent`, the way that agent's own
//! messages read as "Previous agent".

use std::borrow::Cow;

/// What a former agent's mention reads as, `@` included.
pub const PREVIOUS_AGENT_MENTION: &str = "@previous-agent";

/// `body` with every `@token` for which `is_former` is true drawn as
/// [`PREVIOUS_AGENT_MENTION`]. `is_former` gets the token as typed, without
/// `@`; compare it case-insensitively. Borrowed when nothing changes.
///
/// Tokens follow core's `policy::parse_mentions`: letters, digits, `_`, `-`
/// and `.` after an `@` that isn't preceded by a word character (so
/// `a@b.com` is not a mention), with trailing dots left as punctuation.
pub fn rewrite_former_mentions<'a>(
    body: &'a str,
    is_former: impl Fn(&str) -> bool,
) -> Cow<'a, str> {
    if !body.contains('@') {
        return Cow::Borrowed(body);
    }
    let mut out = String::new();
    // Byte offset in `body` up to which `out` holds the text.
    let mut copied = 0;
    let mut prev: Option<char> = None;
    let mut chars = body.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        let starts_mention = c == '@' && !prev.is_some_and(|p| p.is_alphanumeric() || p == '_');
        prev = Some(c);
        if !starts_mention {
            continue;
        }
        let start = at + c.len_utf8();
        let mut end = start;
        while let Some(&(i, d)) = chars.peek() {
            if d.is_alphanumeric() || matches!(d, '_' | '-' | '.') {
                end = i + d.len_utf8();
                prev = Some(d);
                chars.next();
            } else {
                break;
            }
        }
        // Trailing dots are punctuation ("thanks @codex.").
        let token = body[start..end].trim_end_matches('.');
        if !token.is_empty() && is_former(token) {
            out.push_str(&body[copied..at]);
            out.push_str(PREVIOUS_AGENT_MENTION);
            copied = start + token.len();
        }
    }
    if copied == 0 {
        Cow::Borrowed(body)
    } else {
        out.push_str(&body[copied..]);
        Cow::Owned(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn former(token: &str) -> bool {
        ["codex", "cody"]
            .iter()
            .any(|f| f.eq_ignore_ascii_case(token))
    }

    #[test]
    fn a_former_agents_handle_reads_as_previous_agent() {
        assert_eq!(
            rewrite_former_mentions("@codex can you check?", former),
            "@previous-agent can you check?"
        );
        assert_eq!(
            rewrite_former_mentions("thanks @Codex.", former),
            "thanks @previous-agent."
        );
        assert_eq!(
            rewrite_former_mentions("@claude and @CODEX, then @codex", former),
            "@claude and @previous-agent, then @previous-agent"
        );
    }

    #[test]
    fn a_former_agents_nickname_reads_as_previous_agent() {
        assert_eq!(
            rewrite_former_mentions("(@cody) over to you", former),
            "(@previous-agent) over to you"
        );
    }

    #[test]
    fn members_and_other_text_are_left_alone() {
        for body in [
            "@claude can you check?",
            "@codex-reviewer is a different handle",
            "@codex_2 too",
            "mail codex at a@codex.com",
            "x@codex is not a mention",
            "no mentions here @ all",
            "@ codex",
            "",
        ] {
            let out = rewrite_former_mentions(body, former);
            assert!(matches!(out, Cow::Borrowed(_)), "{body:?} -> {out:?}");
            assert_eq!(out, body);
        }
    }

    #[test]
    fn non_ascii_text_around_mentions_survives() {
        assert_eq!(
            rewrite_former_mentions("héllo @codex — ça va? 👀 @cody…", former),
            "héllo @previous-agent — ça va? 👀 @previous-agent…"
        );
    }

    #[test]
    fn a_mention_after_a_word_character_is_not_one() {
        assert_eq!(rewrite_former_mentions("é@codex", former), "é@codex");
        assert_eq!(rewrite_former_mentions("_@codex", former), "_@codex");
        assert_eq!(
            rewrite_former_mentions("-@codex", former),
            "-@previous-agent"
        );
    }
}
