//! Hardcoded sample data for the static shell. Nothing here is persisted.

use crate::*;

const MAIN: Id = Id(1);
const STRM_335: Id = Id(2);
const STRM_327: Id = Id(3);

const CODEX_PLAN: Id = Id(10);
const CLAUDE: Id = Id(11);
const DEVIN: Id = Id(12);
const CODEX_REVIEW: Id = Id(13);
const COPILOT: Id = Id(14);
const MISTRAL: Id = Id(15);
const OPENCODE: Id = Id(16);
const PI: Id = Id(17);

fn s(text: &str) -> Arc<str> {
    Arc::from(text)
}

/// The workspace from the wireframe: one project, three channels, eight agents.
pub fn sample() -> Workspace {
    let channel = |id: Id, name: &str, linear: bool, unread| Channel {
        id,
        name: s(name),
        ordinal: Some(id.0),
        linear_issue: linear.then(|| s(name)),
        unread,
        project_wide: id == MAIN,
    };
    let agent = |id, name: &str, brand, state| Agent {
        id,
        name: s(name),
        brand,
        state,
        presence: None,
    };
    let link = |label: &str, brand, url: &str| QuickLink {
        label: s(label),
        brand,
        url: s(url),
    };
    let mention = |id, agent, channel, text: &str| Mention {
        id: Id(id),
        agent,
        channel,
        text: s(text),
    };

    Workspace {
        project: Project {
            id: Id(0),
            name: s("Starpoint"),
            emoji: s("⭐"),
            path: Some(s("~/Projects/starpoint")),
        },
        projects: vec![ProjectEntry {
            name: s("Starpoint"),
            path: Some(s("~/Projects/starpoint")),
            current: true,
        }],
        channels: vec![
            channel(MAIN, "main", false, false),
            channel(STRM_335, "STRM-335", true, false),
            channel(STRM_327, "STRM-327", true, true),
        ],
        quick_links: vec![
            link("GitHub", Brand::GitHub, "https://github.com"),
            link("Linear", Brand::Linear, "https://linear.app"),
            link("Zed", Brand::Zed, "https://zed.dev"),
            link("Vercel", Brand::Vercel, "https://vercel.com"),
        ],
        // Order follows the wireframe's 4x2 grid, left to right, top row first.
        agents: vec![
            agent(
                CODEX_PLAN,
                "Codex (planner)",
                Brand::OpenAi,
                AgentState::Active,
            ),
            agent(CLAUDE, "Claude Code", Brand::Claude, AgentState::Sleeping),
            agent(DEVIN, "Devin", Brand::Devin, AgentState::Active),
            agent(
                CODEX_REVIEW,
                "Codex (reviewer)",
                Brand::OpenAi,
                AgentState::Active,
            ),
            agent(
                COPILOT,
                "GitHub Copilot",
                Brand::Copilot,
                AgentState::Active,
            ),
            agent(
                MISTRAL,
                "Mistral Vibe",
                Brand::Mistral,
                AgentState::Unresponsive,
            ),
            agent(OPENCODE, "opencode", Brand::Opencode, AgentState::Sleeping),
            agent(PI, "Pi", Brand::Pi, AgentState::Unresponsive),
        ],
        mentions: vec![
            mention(
                100,
                OPENCODE,
                MAIN,
                "Hey @Hx. So what's the direction? I've mapped out two approaches for the sync layer and need you to pick one.",
            ),
            mention(
                101,
                OPENCODE,
                STRM_335,
                "Hey @Hx. Wake up already, we have work to do and the release branch is waiting on your review.",
            ),
            mention(
                102,
                OPENCODE,
                STRM_327,
                "Hey @Hx. everything you made is failing the new lint rules, so I've queued a fix-up PR for you to look at.",
            ),
            mention(
                103,
                OPENCODE,
                MAIN,
                "Hey @Hx. Just got your code improved and benchmarked: cold start is down 38% on the M3.",
            ),
            mention(
                104,
                CLAUDE,
                STRM_335,
                "Hey @Hx. The schema migration is ready, but it drops a column, so I'd like a yes before I run it.",
            ),
            mention(
                105,
                CODEX_REVIEW,
                STRM_327,
                "Hey @Hx. Left eleven review comments on the sync PR; three of them are blocking.",
            ),
            mention(
                106,
                DEVIN,
                MAIN,
                "Hey @Hx. Staging is back up. The outage was an expired certificate, now renewed and on auto-renew.",
            ),
            mention(
                107,
                MISTRAL,
                STRM_335,
                "Hey @Hx. I can't reach the design tokens repo; can you add me or point me at a mirror?",
            ),
            mention(
                108,
                PI,
                MAIN,
                "Hey @Hx. Nightly build passed on all three platforms for the first time this week.",
            ),
            mention(
                109,
                COPILOT,
                STRM_327,
                "Hey @Hx. Suggested a smaller fix for the flaky scroll test in the thread, if you want it instead.",
            ),
        ],
    }
}

/// A deterministic, synthetic history for exercising the message list with large
/// channels (see `CHAOS_DEMO_MESSAGES` in the app). Not shown by default.
pub fn demo_history(count: usize) -> impl Iterator<Item = Message> {
    const AGENTS: [Id; 8] = [
        CODEX_PLAN,
        CLAUDE,
        DEVIN,
        CODEX_REVIEW,
        COPILOT,
        MISTRAL,
        OPENCODE,
        PI,
    ];
    const LINES: [&str; 6] = [
        "Pushed the fix. CI is green on my side.",
        "I'm picking up the sync layer next unless someone already has it.",
        "Benchmarked the cold start again: down another 4%. Numbers are in the PR description.",
        "Blocked on review. The migration touches the schema, so I'd rather a human looked first.",
        "Done. I split the change into three commits so it's easier to read: types, storage, then the view.",
        "Heads up: the lint rules changed upstream, so a few files will need a pass.",
    ];
    let start = 1_761_300_000u64;
    (0..count).map(move |i| {
        // Small xorshift so the history looks varied but is identical on every run.
        let mut x = (i as u32).wrapping_mul(2_654_435_761).max(1);
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        let author = if x.is_multiple_of(7) {
            Author::Human
        } else {
            Author::Agent(AGENTS[(x as usize) % AGENTS.len()])
        };
        let body = LINES[(x as usize / 7) % LINES.len()];
        Message {
            id: Id(1_000 + i as u32),
            author,
            sent_at: start + i as u64 * 47,
            body: Arc::from(body),
            reply_to: None,
        }
    })
}

/// Extra issue channels for checking how the sidebar's channel list grows and
/// scrolls (see `CHAOS_DEMO_CHANNELS` in the app). Not shown by default.
pub fn demo_channels(count: usize) -> impl Iterator<Item = Channel> {
    (0..count).map(|i| {
        let n = 400 + i as u32;
        let name = s(&format!("STRM-{n}"));
        Channel {
            id: Id(1_000 + i as u32),
            name: name.clone(),
            ordinal: Some(4 + i as u32),
            linear_issue: Some(name),
            unread: i % 5 == 1,
            project_wide: false,
        }
    })
}

/// A few messages for #main so the chat isn't empty on first run, including
/// one long review that shows collapsed with "Open".
pub fn sample_history() -> (Id, Vec<Message>) {
    let start = 1_761_472_800u64; // 10:00 UTC
    let lines: [(Author, &str); 5] = [
        (
            Author::Agent(DEVIN),
            "Staging is back up. The outage was an expired certificate, now renewed and on auto-renew.",
        ),
        (
            Author::Human,
            "Thanks. @Claude Code can you go through the provider-resolution issues before we cut the branch?",
        ),
        (
            Author::Agent(CLAUDE),
            "Went through the provider-resolution issues against the spec. Most of them hold up; a few need a decision before anyone builds them.\n\n\
             • STRM-331 (ordered provider preference) cuts against the \"Starpoint decides, the user doesn't choose\" line. It's probably the right escape hatch, but the description should say how it interacts with the resolver.\n\
             • The system provider being read-only, plus prune never touching global installs, is right. I'd keep both as hard rules in the resolver rather than per-provider flags.\n\
             • STRM-329 (MCP install modes) is under-specified: following plugin-style updates only works if the CLI-managed state machine owns every write. Today two code paths write the config.\n\
             • STRM-334 (audit) matches the spec well with one gap: it never says what happens when a provider disappears between scan and fix.\n\
             • Pin-by-hash is mentioned in three issues with three slightly different meanings. One definition in the resolver doc would settle it.\n\n\
             Suggested order: settle the provider-preference question first, then STRM-329, then the audit. I can draft the resolver doc section if that helps.",
        ),
        (
            Author::Agent(CODEX_REVIEW),
            "Agree on the order. I'll hold my review comments on the sync PR until the resolver doc lands. Worth a skim before the branch cut, too.",
        ),
        (
            Author::Human,
            "Fair. I'll actually read the docs this time.",
        ),
    ];
    let messages = lines
        .into_iter()
        .enumerate()
        .map(|(i, (author, body))| Message {
            id: Id(500 + i as u32),
            author,
            sent_at: start + i as u64 * 240,
            body: Arc::from(body),
            reply_to: None,
        })
        .collect();
    (MAIN, messages)
}
