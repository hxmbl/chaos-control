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
    let channel = |id, name: &str, linear: bool, unread| Channel {
        id,
        name: s(name),
        linear_issue: linear.then(|| s(name)),
        unread,
        project_wide: id == MAIN,
    };
    let agent = |id, name: &str, brand, status| Agent {
        id,
        name: s(name),
        brand,
        status,
    };
    let link = |label: &str, brand, url: &str| QuickLink {
        label: s(label),
        brand,
        url: s(url),
    };
    let mention = |id, channel, text: &str| Mention {
        id: Id(id),
        agent: OPENCODE,
        channel,
        text: s(text),
    };

    Workspace {
        project: Project {
            id: Id(0),
            name: s("Starpoint"),
            emoji: s("⭐"),
        },
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
                AgentStatus::Working,
            ),
            agent(CLAUDE, "Claude Code", Brand::Claude, AgentStatus::Idle),
            agent(DEVIN, "Devin", Brand::Devin, AgentStatus::Working),
            agent(
                CODEX_REVIEW,
                "Codex (reviewer)",
                Brand::OpenAi,
                AgentStatus::Working,
            ),
            agent(
                COPILOT,
                "GitHub Copilot",
                Brand::Copilot,
                AgentStatus::Working,
            ),
            agent(
                MISTRAL,
                "Mistral Vibe",
                Brand::Mistral,
                AgentStatus::Blocked,
            ),
            agent(OPENCODE, "opencode", Brand::Opencode, AgentStatus::Idle),
            agent(PI, "Pi", Brand::Pi, AgentStatus::Blocked),
        ],
        mentions: vec![
            mention(
                100,
                MAIN,
                "Hey @Hx. So what's the direction? I've mapped out two approaches for the sync layer and need you to pick one.",
            ),
            mention(
                101,
                STRM_335,
                "Hey @Hx. Wake up already, we have work to do and the release branch is waiting on your review.",
            ),
            mention(
                102,
                STRM_327,
                "Hey @Hx. everything you made is failing the new lint rules, so I've queued a fix-up PR for you to look at.",
            ),
            mention(
                103,
                MAIN,
                "Hey @Hx. Just got your code improved and benchmarked: cold start is down 38% on the M3.",
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
        let author = if x % 7 == 0 {
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
        }
    })
}
