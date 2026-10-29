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
//! Core runs the launch folder's project (see `project.rs`): its id comes
//! from `.chaos/project.toml`, and the human and every agent are granted that
//! project and joined to its own #main. Other projects' channels stay out of
//! reach of this run's agents.
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
//!     "acp_v2": true, "chat_tools": "auto", "runs_in": "host",
//!     "ping_answer_reactions": ["✅", "👍"]
//!   }]
//! }
//! ```
//!
//! `brand` only picks the icon; the runtime never branches on it. `record`
//! appends every ACP frame as NDJSON `{dir, frame}` (the replay-fixture format).
//! `chat_tools` is `"auto"` (HTTP when the agent offers it, else the stdio
//! bridge) or `"stdio"` (always `chaos-control mcp-bridge`). `runs_in` is
//! `"host"` (the default: permission requests go to the humans, without
//! blocking the agent) or `"vm"` (the agent's own VM: everything is approved).
//! `ping_answer_reactions` (optional) is this agent's own list of reactions
//! that answer a ping, over the project's (Settings) and the default (✅ 👍):
//! emoji, `:shortcodes:`, or `"*"` for any emoji but 👀 (which never answers).
//! A list with junk in it is skipped with a message naming each bad entry,
//! and the agent uses the project's list.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use chaos_core::{
    acp::{AcpAdapter, ChatToolsPreference, SpawnTransport},
    adapter::BridgeCommand,
    engine::CoreEvent,
    host::{AgentSpec, Core, HostConfig, SystemClock},
    permission::RunsIn,
    reaction::PingAnswers,
    types::{Channel, ChannelId, MemberId, Project, ProjectId},
};
use model::Brand;
use serde::Deserialize;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::project::{self, Confirm, FileState, Known, Plan, Target};

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
    /// `"host"` (default: permission requests go to the humans, never
    /// blocking) or `"vm"` (the agent's own VM: everything is approved).
    #[serde(default)]
    pub runs_in: RunsIn,
    /// This agent's own reactions that answer a ping (see the module docs).
    #[serde(default)]
    pub ping_answer_reactions: Option<Vec<String>>,
}

impl AgentConfig {
    /// The agent's own ping-answer list, validated like the project's.
    pub fn ping_answers(&self) -> Result<Option<PingAnswers>, String> {
        self.ping_answer_reactions
            .as_deref()
            .map(PingAnswers::parse)
            .transpose()
            .map_err(|e| format!("@{}: {e}", self.handle))
    }
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

/// The project this run shows, as core has it.
#[derive(Clone, Debug)]
pub struct OpenProject {
    pub project: Project,
    /// Its #main, behind the app's #main.
    pub main: Channel,
    /// All its channels, in `::N` order.
    pub channels: Vec<Channel>,
    /// Every project core knows, for the picker.
    pub all: Vec<Project>,
}

/// A running core plus what the app needs to talk to it.
pub struct CoreLink {
    pub core: Core,
    /// The launch folder (the project's root).
    pub folder: PathBuf,
    pub open: OpenProject,
    /// An identity question for the user (see `project::decide`), if the
    /// folder's file disagreed with what core remembers.
    pub confirm: Option<Confirm>,
    /// The human at this keyboard.
    pub human: MemberId,
    pub agents: Vec<LinkedAgent>,
    pub events: Option<UnboundedReceiver<CoreEvent>>,
    /// Keeps core's tasks alive for as long as the app holds the link.
    _runtime: Arc<tokio::runtime::Runtime>,
}

impl CoreLink {
    /// Core's channel behind the app's #main.
    pub fn main(&self) -> ChannelId {
        self.open.main.id
    }
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
    let folder = std::env::current_dir().map_err(|e| format!("no working directory: {e}"))?;
    let (core, events, open, confirm, human, agents) = runtime.block_on(async {
        let (core, events) = Core::start(host, Arc::new(SystemClock)).await?;
        let plan = plan_for(&core, &folder).await?;
        let project = apply_plan(&core, &folder, &plan.open, plan.write_file).await?;
        let main = core.channel(&project.id, "main").await?.id;
        let human = core
            .human(&config.human.handle, &config.human.name, main)
            .await?
            .id;
        let mut agents = Vec::new();
        for a in config.agents {
            let ping_answers = a.ping_answers().unwrap_or_else(|e| {
                eprintln!("chaos-control: ignoring ping_answer_reactions for {e}");
                None
            });
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
                        runs_in: a.runs_in,
                        ping_answers,
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
        let open = describe(&core, project).await?;
        Ok::<_, String>((core, events, open, plan.confirm, human, agents))
    })?;
    Ok(CoreLink {
        core,
        folder,
        open,
        confirm,
        human,
        agents,
        events: Some(events),
        _runtime: Arc::new(runtime),
    })
}

fn known(p: &Project) -> Known {
    Known {
        id: p.id.to_string(),
        name: p.name.to_string(),
        root: p.root_path.as_deref().map(str::to_string),
    }
}

fn parse_id(id: &str) -> Result<ProjectId, String> {
    ProjectId::parse(id).ok_or_else(|| format!("{id:?} is not a project id"))
}

/// Reads the folder's `.chaos/project.toml`, asks core what it knows about
/// the folder and the file's UUID, and decides (see `project::decide`).
async fn plan_for(core: &Core, folder: &Path) -> Result<Plan, String> {
    let file = project::read(folder, |s| ProjectId::parse(s).map(|p| p.to_string()));
    if let FileState::Unusable(why) = &file {
        eprintln!(
            "chaos-control: ignoring {} ({why}); it is left as it is",
            project::file_path(folder).display()
        );
    }
    let root = project::root_key(folder);
    let by_root = core.project_by_root(&root).await?.map(|p| known(&p));
    let mut by_file_id = None;
    let mut other_root_still_claims_it = false;
    if let FileState::Present(f) = &file {
        by_file_id = core.project(parse_id(&f.id)?).await?.map(|p| known(&p));
        if let Some(other) = by_file_id.as_ref().and_then(|k| k.root.clone())
            && other != root
        {
            other_root_still_claims_it = matches!(
                project::read(Path::new(&other), |s| ProjectId::parse(s).map(|p| p.to_string())),
                FileState::Present(o) if o.id == f.id
            );
        }
    }
    // Pre-project history is only offered to a folder with no file.
    let unclaimed_import = match &file {
        FileState::Missing => core.unclaimed_import().await?.map(|p| known(&p)),
        _ => None,
    };
    let by_root_is_empty = match (&by_root, &unclaimed_import) {
        (Some(k), Some(_)) => !core.project_has_messages(parse_id(&k.id)?).await?,
        _ => false,
    };
    let fresh = uuid::Uuid::new_v4().to_string();
    let name = project::folder_name(folder);
    let plan = project::decide(&project::Inputs {
        file: &file,
        by_root: by_root.as_ref(),
        by_file_id: by_file_id.as_ref(),
        other_root_still_claims_it,
        folder_name: &name,
        fresh_id: &fresh,
        unclaimed_import: unclaimed_import.as_ref(),
        by_root_is_empty,
    });
    Ok(plan)
}

/// Records `target` in core as this folder's project, writes the file when
/// asked and keeps `.chaos/` out of git. Grants nobody anything.
async fn apply_plan(
    core: &Core,
    folder: &Path,
    target: &Target,
    write_file: bool,
) -> Result<Project, String> {
    let root = project::root_key(folder);
    let project = core
        .ensure_project(parse_id(&target.id)?, &target.name, Some(&root))
        .await?;
    if write_file && let Err(e) = project::write(folder, target) {
        eprintln!(
            "chaos-control: could not write {}: {e}",
            project::file_path(folder).display()
        );
    }
    if folder.join(project::DIR).is_dir()
        && let Err(e) = project::exclude_from_git(folder)
    {
        eprintln!("chaos-control: could not add .chaos/ to .git/info/exclude: {e}");
    }
    Ok(project)
}

/// The project's #main, its channels and the picker list.
async fn describe(core: &Core, project: Project) -> Result<OpenProject, String> {
    let main = core.channel(&project.id, "main").await?;
    let channels = core.channels(&project.id).await?;
    let all = core.projects().await?;
    Ok(OpenProject {
        project,
        main,
        channels,
        all,
    })
}

/// Opens `target` as this folder's project in a running core (the user's
/// answer to an identity question): records it, grants the human and the
/// agents the project, joins them to its #main and describes it.
pub async fn switch_project(
    core: Core,
    folder: PathBuf,
    target: Target,
    write_file: bool,
    members: Vec<MemberId>,
) -> Result<OpenProject, String> {
    let project = apply_plan(&core, &folder, &target, write_file).await?;
    let main = core.channel(&project.id, "main").await?;
    for m in members {
        core.join_project(&project.id, m).await?;
        core.join(m, main.id).await?;
    }
    describe(&core, project).await
}

/// Adopts the unclaimed pre-project history (`target`) as this folder's
/// project: core gives it the folder and `target`'s name (refusing if it has
/// a folder or was claimed meanwhile), then it opens like any switch and the
/// file is written with its UUID.
pub async fn adopt_import(
    core: Core,
    folder: PathBuf,
    target: Target,
    members: Vec<MemberId>,
) -> Result<OpenProject, String> {
    let root = project::root_key(&folder);
    core.adopt_import(parse_id(&target.id)?, &target.name, &root)
        .await?;
    switch_project(core, folder, target, true, members).await
}

/// Deletes `id` if it is an empty placeholder left without a folder by a
/// switch (see `Core::forget_empty_project`). Anything with a message stays.
pub async fn forget_placeholder(core: &Core, id: &str) {
    let Ok(id) = parse_id(id) else {
        return;
    };
    if let Err(e) = core.forget_empty_project(id).await {
        eprintln!("chaos-control: could not remove an empty project: {e}");
    }
}

/// Rewrites the folder's file to name `target` (keeping what is open).
pub fn keep(folder: &Path, target: &Target) {
    if let Err(e) = project::write(folder, target) {
        eprintln!(
            "chaos-control: could not write {}: {e}",
            project::file_path(folder).display()
        );
    }
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
        assert_eq!(c.agents[0].ping_answers(), Ok(None));
    }

    #[test]
    fn ping_answer_reactions_override() {
        let c: AgentsConfig = serde_json::from_str(
            r#"{"agents":[
                {"handle":"a","command":"x","ping_answer_reactions":["✅",":+1:"]},
                {"handle":"b","command":"x","ping_answer_reactions":["*"]},
                {"handle":"c","command":"x","ping_answer_reactions":["✅","lol","👀"]}
            ]}"#,
        )
        .unwrap();
        let a = c.agents[0].ping_answers().unwrap().unwrap();
        assert_eq!(a.to_list(), vec!["✅", "👍"]);
        assert_eq!(c.agents[1].ping_answers(), Ok(Some(PingAnswers::Any)));
        let err = c.agents[2].ping_answers().unwrap_err();
        assert!(
            err.starts_with("@c: invalid ping_answer_reactions"),
            "{err}"
        );
        assert!(err.contains("\"lol\"") && err.contains("\"👀\""), "{err}");
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
