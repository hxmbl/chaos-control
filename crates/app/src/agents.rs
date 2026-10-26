//! The link between the app and `chaos-core`, the agent runtime.
//!
//! Core starts only when an `agents.json` exists. It runs in-process on its own
//! small tokio runtime; the GPUI side talks to it through the cloneable
//! [`Core`] handle and hears back through [`CoreEvent`]s (see
//! `ChatStore::attach_core`). Nothing here holds room policy: that all lives in
//! core (`docs/agents.md`).
//!
//! Where things live:
//! - data directory: `$CHAOS_CONTROL_DATA`, else the platform data dir
//!   (`~/Library/Application Support/Chaos Control` on macOS,
//!   `$XDG_DATA_HOME/chaos-control` or `~/.local/share/chaos-control` elsewhere);
//! - agents file: `$CHAOS_CONTROL_AGENTS`, else `<data>/agents.json`;
//! - database: `<data>/core.sqlite`.
//!
//! `agents.json`:
//!
//! ```json
//! {
//!   "human": { "handle": "hx", "name": "Hx" },
//!   "agents": [{
//!     "handle": "claude", "name": "Claude Code", "brand": "claude",
//!     "command": "npx", "args": ["-y", "@agentclientprotocol/claude-agent-acp@0.97.0"],
//!     "env": {}, "cwd": "/path/to/repo",
//!     "record": "/tmp/claude-acp.ndjson",
//!     "acp_v2": true, "chat_tools": "auto"
//!   }]
//! }
//! ```
//!
//! `brand` only picks the icon; the runtime never branches on it. `record`
//! appends every ACP frame as NDJSON `{dir, frame}` (the replay-fixture format).
//! `chat_tools` is `"auto"` (HTTP when the agent offers it, else the stdio
//! bridge) or `"stdio"` (always `chaos-control mcp-bridge`).

use std::{collections::HashMap, path::PathBuf, sync::Arc};

use chaos_core::{
    acp::{AcpAdapter, ChatToolsPreference, SpawnTransport},
    adapter::BridgeCommand,
    engine::CoreEvent,
    host::{AgentSpec, Core, HostConfig, SystemClock},
    types::{ChannelId, MemberId},
};
use model::Brand;
use serde::Deserialize;
use tokio::sync::mpsc::UnboundedReceiver;

#[derive(Debug, Deserialize)]
pub struct AgentsConfig {
    #[serde(default)]
    pub human: HumanConfig,
    #[serde(default)]
    pub agents: Vec<AgentConfig>,
}

#[derive(Debug, Deserialize)]
pub struct HumanConfig {
    pub handle: String,
    pub name: String,
}

impl Default for HumanConfig {
    fn default() -> Self {
        HumanConfig {
            handle: "hx".into(),
            name: "Hx".into(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct AgentConfig {
    pub handle: String,
    pub name: Option<String>,
    pub brand: Option<String>,
    pub command: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    pub cwd: Option<PathBuf>,
    pub record: Option<PathBuf>,
    #[serde(default = "yes")]
    pub acp_v2: bool,
    #[serde(default)]
    pub chat_tools: ChatTools,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Default, Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum ChatTools {
    #[default]
    Auto,
    Stdio,
}

/// An agent core knows about, as the app shows it.
pub struct LinkedAgent {
    pub member: MemberId,
    pub name: Arc<str>,
    pub brand: Brand,
}

/// A running core plus what the app needs to talk to it.
pub struct CoreLink {
    pub core: Core,
    /// Core's channel behind the app's #main.
    pub main: ChannelId,
    /// The human at this keyboard.
    pub human: MemberId,
    pub agents: Vec<LinkedAgent>,
    pub events: Option<UnboundedReceiver<CoreEvent>>,
    /// Keeps core's tasks alive for as long as the app holds the link.
    _runtime: Arc<tokio::runtime::Runtime>,
}

pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("CHAOS_CONTROL_DATA") {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    if cfg!(target_os = "macos") {
        return home.join("Library/Application Support/Chaos Control");
    }
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"))
        .join("chaos-control")
}

pub fn agents_file() -> PathBuf {
    std::env::var_os("CHAOS_CONTROL_AGENTS")
        .map(PathBuf::from)
        .unwrap_or_else(|| data_dir().join("agents.json"))
}

pub fn parse_brand(name: Option<&str>) -> Brand {
    match name.map(str::to_ascii_lowercase).as_deref() {
        Some("claude") => Brand::Claude,
        Some("openai") | Some("codex") => Brand::OpenAi,
        Some("devin") => Brand::Devin,
        Some("copilot") | Some("github") => Brand::Copilot,
        Some("mistral") => Brand::Mistral,
        Some("zed") => Brand::Zed,
        Some("pi") => Brand::Pi,
        // A neutral mark for anything else.
        _ => Brand::Opencode,
    }
}

/// Starts core if an agents file exists. Prints why it did not start otherwise
/// (a missing file is silent: the app then runs on sample data alone).
pub fn start() -> Option<CoreLink> {
    let path = agents_file();
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            eprintln!("chaos-control: could not read {}: {e}", path.display());
            return None;
        }
    };
    let config: AgentsConfig = match serde_json::from_str(&text) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("chaos-control: {} is not valid: {e}", path.display());
            return None;
        }
    };
    match start_with(config) {
        Ok(link) => Some(link),
        Err(e) => {
            eprintln!("chaos-control: agent runtime did not start: {e}");
            None
        }
    }
}

pub fn start_with(config: AgentsConfig) -> Result<CoreLink, String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("chaos-core")
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let data = data_dir();
    let bridge = std::env::current_exe().ok().map(|command| BridgeCommand {
        command,
        args: vec!["mcp-bridge".into()],
    });
    let host = HostConfig {
        db: Some(data.join("core.sqlite")),
        bridge,
        default_cwd: std::env::current_dir().unwrap_or_else(|_| data.clone()),
        ..HostConfig::default()
    };
    let (core, events, main, human, agents) = runtime.block_on(async {
        let (core, events) = Core::start(host, Arc::new(SystemClock)).await?;
        let main = core.channel("main").await?.id;
        let human = core
            .human(&config.human.handle, &config.human.name, main)
            .await?
            .id;
        let mut agents = Vec::new();
        for a in config.agents {
            let name = a.name.clone().unwrap_or_else(|| a.handle.clone());
            let transport = SpawnTransport {
                command: a.command,
                args: a.args,
                env: a.env.into_iter().collect(),
                record: a.record,
            };
            let adapter = AcpAdapter::new(Arc::new(transport))
                .prefer_v2(a.acp_v2)
                .chat_tools(match a.chat_tools {
                    ChatTools::Auto => ChatToolsPreference::Auto,
                    ChatTools::Stdio => ChatToolsPreference::StdioBridge,
                });
            let member = core
                .add_agent(
                    AgentSpec {
                        handle: a.handle.clone(),
                        name: name.clone(),
                        adapter: Arc::new(adapter),
                        cwd: a.cwd,
                    },
                    main,
                )
                .await?;
            agents.push(LinkedAgent {
                member: member.id,
                name: Arc::from(name),
                brand: parse_brand(a.brand.as_deref()),
            });
        }
        Ok::<_, String>((core, events, main, human, agents))
    })?;
    Ok(CoreLink {
        core,
        main,
        human,
        agents,
        events: Some(events),
        _runtime: Arc::new(runtime),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults() {
        let c: AgentsConfig = serde_json::from_str(
            r#"{"agents":[{"handle":"claude","command":"npx","args":["-y","x"]}]}"#,
        )
        .unwrap();
        assert_eq!(c.human.handle, "hx");
        assert!(c.agents[0].acp_v2);
        assert!(matches!(c.agents[0].chat_tools, ChatTools::Auto));
        assert_eq!(parse_brand(Some("Claude")), Brand::Claude);
        assert_eq!(parse_brand(None), Brand::Opencode);
    }

    #[test]
    fn data_dir_honours_the_env_override() {
        // Only reads the variable; nothing is created.
        if let Some(dir) = std::env::var_os("CHAOS_CONTROL_DATA") {
            assert_eq!(data_dir(), PathBuf::from(dir));
        } else {
            assert!(data_dir().ends_with("chaos-control") || data_dir().ends_with("Chaos Control"));
        }
    }
}
