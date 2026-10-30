//! The link between the app and `chaos-core`, the agent runtime.
//!
//! Core starts when an `agents.json` exists **or** at least one harness is
//! installed (auto-detection, `chaos_core::harness`). It runs in-process on
//! its own small tokio runtime; the GPUI side talks to it through the
//! cloneable [`Core`] handle and hears back through [`CoreEvent`]s (see
//! `ChatStore::attach_core`). Nothing here holds room policy: that all lives
//! in core (`docs/agents.md`).
//!
//! Where things live:
//! - data directory: `$CHAOS_CONTROL_DATA`, else the platform data dir
//!   (`~/Library/Application Support/Chaos Control` on macOS,
//!   `$XDG_DATA_HOME/chaos-control` or `~/.local/share/chaos-control` elsewhere);
//! - agents file: `$CHAOS_CONTROL_AGENTS`, else `<data>/agents.json`;
//! - database: `<data>/core.sqlite`.
//!
//! # Members are models
//!
//! A chat member is a model running through a harness (Codex, fx, Vibe,
//! Claude Code, ...). The project's roster lives in core's store: the first
//! time a project opens, every detected harness adds its default model, and
//! from then on the user adds and removes members in Agent Status.
//!
//! # `agents.json` (optional)
//!
//! ```json
//! {
//!   "human": { "handle": "hx", "name": "Hx" },
//!   "detect": true,
//!   "harnesses": {
//!     "codex": { "command": "/opt/codex/bin/codex", "args": ["app-server"] },
//!     "fx": { "env": { "FX_LOG": "warn" } }
//!   },
//!   "members": [
//!     { "harness": "codex", "model": "gpt-6.1-sol", "name": "GPT-6.1 Sol" },
//!     { "harness": "fx", "model": "anthropic/claude-opus-5.5", "runs_in": "vm" }
//!   ],
//!   "agents": [{
//!     "handle": "claude", "name": "Claude Code", "brand": "claude",
//!     "command": "npx", "args": ["-y", "@agentclientprotocol/claude-agent-acp@0.99.1"]
//!   }]
//! }
//! ```
//!
//! - `detect` (default `true`; `CHAOS_CONTROL_DETECT=0` also turns it off):
//!   look for installed harnesses.
//! - `harnesses`: per-preset overrides (`command`, `args`, `env`, `record`,
//!   `acp_v2`, `chat_tools`). A harness with a `command` here counts as
//!   installed even if detection did not find it.
//! - `members`: an explicit roster. When present it is used as is at every
//!   start (no seeding from detection); adds and removes in the app still
//!   work for the session and are kept in the store.
//! - `agents`: the original shape (one ACP command per agent, `brand` only
//!   picks the icon). Still loads unchanged; such agents are not on the
//!   roster and can't be removed from the app.
//!
//! `record` appends every ACP frame as NDJSON `{dir, frame}` (the replay
//! fixture format). `chat_tools` is `"auto"` (HTTP when the agent offers it,
//! else the stdio bridge) or `"stdio"`. `runs_in` is `"host"` (the default:
//! permission requests go to the humans, without blocking the agent) or
//! `"vm"` (the agent's own VM: everything is approved).

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use chaos_core::{
    acp::{AcpAdapter, ChatToolsPreference, SpawnTransport},
    adapter::{AgentAdapter, BridgeCommand},
    engine::CoreEvent,
    harness::{self, AdapterOptions, DetectEnv, HarnessPreset, Transport},
    host::{AgentSpec, Core, HostConfig, MemberSpec, SystemClock},
    permission::RunsIn,
    roster::ModelMember,
    types::{Channel, ChannelId, MemberId, Project, ProjectId},
};
use model::Brand;
use serde::Deserialize;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::project::{self, Confirm, FileState, Known, Plan, Target};

#[derive(Debug, Default, Deserialize)]
pub struct AgentsConfig {
    #[serde(default)]
    pub human: HumanConfig,
    #[serde(default = "yes")]
    pub detect: bool,
    #[serde(default)]
    pub harnesses: HashMap<String, HarnessOverride>,
    #[serde(default)]
    pub members: Option<Vec<MemberConfig>>,
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

/// Overrides for one preset harness.
#[derive(Debug, Default, Deserialize)]
pub struct HarnessOverride {
    pub command: Option<PathBuf>,
    pub args: Option<Vec<String>>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    pub record: Option<PathBuf>,
    #[serde(default = "yes")]
    pub acp_v2: bool,
    #[serde(default)]
    pub chat_tools: ChatTools,
}

/// One member of an explicit roster.
#[derive(Debug, Deserialize)]
pub struct MemberConfig {
    pub harness: String,
    /// The harness's model id; absent runs its default model.
    pub model: Option<String>,
    /// Display name; defaults to the model id (or "<harness> default").
    pub name: Option<String>,
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub runs_in: RunsIn,
}

/// The original shape: one ACP command per agent.
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

impl ChatTools {
    fn preference(self) -> ChatToolsPreference {
        match self {
            ChatTools::Auto => ChatToolsPreference::Auto,
            ChatTools::Stdio => ChatToolsPreference::StdioBridge,
        }
    }
}

/// An installed (or configured) harness this run can start.
#[derive(Clone, Debug)]
pub struct Harness {
    pub preset: &'static HarnessPreset,
    pub command: PathBuf,
    pub args: Vec<String>,
    pub opts: AdapterOptions,
}

impl Harness {
    /// The adapter for one member running `model` on this harness.
    pub fn adapter(&self, model: Option<&str>) -> Arc<dyn AgentAdapter> {
        harness::adapter_for(
            self.preset.transport,
            self.command.clone(),
            self.args.clone(),
            model.map(Arc::from),
            self.opts.clone(),
        )
    }

    pub fn brand(&self) -> Brand {
        parse_brand(Some(self.preset.icon))
    }
}

/// The adapter for a roster member whose harness isn't installed: its probe
/// fails, so the member shows red ("not found") instead of vanishing.
fn missing_adapter(preset: &HarnessPreset) -> Arc<dyn AgentAdapter> {
    let command = match preset.launch.command {
        harness::BIN => preset.detect[0],
        c => c,
    };
    harness::adapter_for(
        preset.transport,
        PathBuf::from(command),
        Vec::new(),
        None,
        AdapterOptions::default(),
    )
}

/// Every harness this run can use: detected ones (unless detection is off)
/// plus any with an explicit `command` override, in preset order.
pub fn harnesses(config: &AgentsConfig, env: &DetectEnv) -> Vec<Harness> {
    let detect = config.detect && std::env::var("CHAOS_CONTROL_DETECT").as_deref() != Ok("0");
    harness::PRESETS
        .iter()
        .filter_map(|preset| {
            let o = config.harnesses.get(preset.id);
            let found = detect.then(|| harness::detect_one(preset, env)).flatten();
            let (command, args) = match (o.and_then(|o| o.command.clone()), found) {
                (Some(command), _) => {
                    let args = o.and_then(|o| o.args.clone()).unwrap_or_else(|| {
                        preset.launch.args.iter().map(|a| a.to_string()).collect()
                    });
                    (command, args)
                }
                (None, Some(d)) => (d.command, o.and_then(|o| o.args.clone()).unwrap_or(d.args)),
                (None, None) => return None,
            };
            let opts = AdapterOptions {
                env: o
                    .map(|o| o.env.clone().into_iter().collect())
                    .unwrap_or_default(),
                record: o.and_then(|o| o.record.clone()),
                acp_v2: o.is_none_or(|o| o.acp_v2),
                chat_tools: o.map_or(ChatToolsPreference::Auto, |o| o.chat_tools.preference()),
            };
            Some(Harness {
                preset,
                command,
                args,
                opts,
            })
        })
        .collect()
}

/// An agent core knows about, as the app shows it.
#[derive(Clone, Debug)]
pub struct LinkedAgent {
    pub member: MemberId,
    pub name: Arc<str>,
    /// The harness mark (or the agents.json `brand`).
    pub brand: Brand,
    /// The harness's name, for the tooltip ("via Codex").
    pub harness: Option<Arc<str>>,
    /// On the project's roster (a model member), so the user can remove it.
    pub removable: bool,
}

impl LinkedAgent {
    fn from_member(m: &ModelMember) -> Self {
        let preset = harness::preset(&m.harness);
        LinkedAgent {
            member: m.member.id,
            name: m.model_name.clone(),
            brand: parse_brand(preset.map(|p| p.icon)),
            harness: preset.map(|p| Arc::from(p.name)),
            removable: true,
        }
    }
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
    /// The harnesses this run can start, for the model picker.
    pub harnesses: Vec<Harness>,
    pub events: Option<UnboundedReceiver<CoreEvent>>,
    /// Keeps core's tasks alive for as long as the app holds the link.
    _runtime: Arc<tokio::runtime::Runtime>,
}

impl CoreLink {
    /// Core's channel behind the app's #main.
    pub fn main(&self) -> ChannelId {
        self.open.main.id
    }

    pub fn harness(&self, id: &str) -> Option<&Harness> {
        self.harnesses.iter().find(|h| h.preset.id == id)
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
        Some("mistral") | Some("vibe") => Brand::Mistral,
        Some("zed") => Brand::Zed,
        Some("pi") => Brand::Pi,
        Some("vercel") | Some("fx") => Brand::Vercel,
        Some("gemini") => Brand::Gemini,
        Some("cursor") => Brand::Cursor,
        // A neutral mark for anything else.
        _ => Brand::Opencode,
    }
}

/// Starts core if an agents file exists or a harness is installed. Prints
/// why it did not start otherwise (nothing to run is silent: the app then
/// runs on sample data alone).
pub fn start() -> Option<CoreLink> {
    let path = agents_file();
    let config = match std::fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str::<AgentsConfig>(&text) {
            Ok(c) => Some(c),
            Err(e) => {
                eprintln!("chaos-control: {} is not valid: {e}", path.display());
                return None;
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            eprintln!("chaos-control: could not read {}: {e}", path.display());
            return None;
        }
    };
    let had_file = config.is_some();
    let config = config.unwrap_or_else(|| AgentsConfig {
        detect: true,
        ..AgentsConfig::default()
    });
    let found = harnesses(&config, &DetectEnv::from_process());
    if !had_file && found.is_empty() {
        return None;
    }
    match start_with(config, found) {
        Ok(link) => Some(link),
        Err(e) => {
            eprintln!("chaos-control: agent runtime did not start: {e}");
            None
        }
    }
}

/// The member a fresh project gets for a detected harness: its first preset
/// model (the harness default).
fn seed_member(h: &Harness) -> MemberSpec {
    let first = harness::preset_models(h.preset)
        .into_iter()
        .next()
        .unwrap_or_else(|| harness::ModelInfo::default_for(h.preset));
    MemberSpec {
        harness: h.preset.id.to_string(),
        model: first.id.as_deref().map(str::to_string),
        model_name: first.name.to_string(),
        adapter: h.adapter(first.id.as_deref()),
        cwd: None,
        runs_in: RunsIn::Host,
    }
}

/// The spec that adds (or re-registers) the member running `model` on
/// `harness` this run. A harness that isn't installed still gets an adapter,
/// one whose probe fails, so the member shows red instead of vanishing.
pub fn spec_for(
    harnesses: &[Harness],
    harness: &str,
    model: Option<&str>,
    name: &str,
) -> MemberSpec {
    let adapter = match harnesses.iter().find(|h| h.preset.id == harness) {
        Some(h) => h.adapter(model),
        None => match harness::preset(harness) {
            Some(p) => missing_adapter(p),
            // A harness this build doesn't know: a command that won't be found.
            None => harness::adapter_for(
                Transport::Acp,
                PathBuf::from(harness),
                Vec::new(),
                None,
                AdapterOptions::default(),
            ),
        },
    };
    MemberSpec {
        harness: harness.to_string(),
        model: model.map(str::to_string),
        model_name: name.to_string(),
        adapter,
        cwd: None,
        runs_in: RunsIn::Host,
    }
}

pub fn start_with(config: AgentsConfig, harnesses: Vec<Harness>) -> Result<CoreLink, String> {
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
        // The original shape: plain ACP agents, not on the roster.
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
                .chat_tools(a.chat_tools.preference());
            let member = core
                .add_agent(
                    AgentSpec {
                        handle: a.handle.clone(),
                        name: name.clone(),
                        adapter: Arc::new(adapter),
                        cwd: a.cwd,
                        runs_in: a.runs_in,
                    },
                    main,
                )
                .await?;
            agents.push(LinkedAgent {
                member: member.id,
                name: Arc::from(name),
                brand: parse_brand(a.brand.as_deref()),
                harness: None,
                removable: false,
            });
        }
        // Model members.
        let specs: Vec<MemberSpec> = match config.members {
            Some(list) => list
                .into_iter()
                .map(|m| {
                    let name = m.name.clone().unwrap_or_else(|| match &m.model {
                        Some(id) => id.clone(),
                        None => format!(
                            "{} default",
                            harness::preset(&m.harness).map_or(m.harness.as_str(), |p| p.name)
                        ),
                    });
                    MemberSpec {
                        cwd: m.cwd,
                        runs_in: m.runs_in,
                        ..spec_for(&harnesses, &m.harness, m.model.as_deref(), &name)
                    }
                })
                .collect(),
            None => {
                let mut specs: Vec<MemberSpec> = Vec::new();
                // A fresh project: every detected harness brings its
                // default model. After this the roster is the user's.
                if core.seed_roster_once(&project.id).await? {
                    for h in &harnesses {
                        if let Err(e) = core.add_model_member(seed_member(h), main).await {
                            eprintln!("chaos-control: could not add {}: {e}", h.preset.name);
                        }
                    }
                }
                for m in core.roster(&project.id).await? {
                    specs.push(spec_for(
                        &harnesses,
                        &m.harness,
                        m.model.as_deref(),
                        &m.model_name,
                    ));
                }
                specs
            }
        };
        for spec in specs {
            match core.add_model_member(spec, main).await {
                Ok(m) => {
                    if !agents.iter().any(|a: &LinkedAgent| a.member == m.member.id) {
                        agents.push(LinkedAgent::from_member(&m));
                    }
                }
                Err(e) => eprintln!("chaos-control: could not add a member: {e}"),
            }
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
        harnesses,
        events: Some(events),
        _runtime: Arc::new(runtime),
    })
}

/// Adds the model member `model` on `harness` to the project behind `main`.
pub async fn add_member(
    core: Core,
    harness: Harness,
    model: Option<Arc<str>>,
    name: Arc<str>,
    main: ChannelId,
) -> Result<LinkedAgent, String> {
    let m = core
        .add_model_member(
            MemberSpec {
                harness: harness.preset.id.to_string(),
                model: model.as_deref().map(str::to_string),
                model_name: name.to_string(),
                adapter: harness.adapter(model.as_deref()),
                cwd: None,
                runs_in: RunsIn::Host,
            },
            main,
        )
        .await?;
    Ok(LinkedAgent::from_member(&m))
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
    fn the_original_shape_still_loads() {
        let c: AgentsConfig = serde_json::from_str(
            r#"{"agents":[{"handle":"claude","command":"npx","args":["-y","x"]}]}"#,
        )
        .unwrap();
        assert_eq!(c.human.handle, "hx");
        assert!(c.detect, "detection is on unless turned off");
        assert!(c.members.is_none());
        assert!(c.agents[0].acp_v2);
        assert!(matches!(c.agents[0].chat_tools, ChatTools::Auto));
        assert_eq!(parse_brand(Some("Claude")), Brand::Claude);
        assert_eq!(parse_brand(Some("vercel")), Brand::Vercel);
        assert_eq!(parse_brand(None), Brand::Opencode);
    }

    #[test]
    fn the_model_shape_parses() {
        let c: AgentsConfig = serde_json::from_str(
            r#"{"detect": false,
                "harnesses": {"codex": {"command": "/opt/codex", "env": {"A": "1"}}},
                "members": [{"harness": "codex", "model": "gpt-6.1-sol", "runs_in": "vm"},
                            {"harness": "fx"}]}"#,
        )
        .unwrap();
        assert!(!c.detect);
        let members = c.members.as_ref().unwrap();
        assert_eq!(members[0].model.as_deref(), Some("gpt-6.1-sol"));
        assert_eq!(members[0].runs_in, RunsIn::Vm);
        assert!(members[1].model.is_none());
        assert_eq!(c.harnesses["codex"].env["A"], "1");
    }

    #[test]
    fn harnesses_come_from_detection_and_command_overrides() {
        let files: std::collections::HashSet<PathBuf> =
            ["/bin/fx"].iter().map(PathBuf::from).collect();
        let exists = |p: &Path| files.contains(p);
        let env = DetectEnv {
            path: vec![PathBuf::from("/bin")],
            home: None,
            is_executable: &exists,
        };
        let config: AgentsConfig = serde_json::from_str(
            r#"{"harnesses": {"codex": {"command": "/opt/codex"}}}"#,
        )
        .unwrap();
        let found = harnesses(&config, &env);
        let ids: Vec<_> = found.iter().map(|h| h.preset.id).collect();
        // Codex is not on PATH but its command override counts.
        assert_eq!(ids, ["codex", "fx"]);
        assert_eq!(found[0].command, PathBuf::from("/opt/codex"));
        assert_eq!(found[0].args, ["app-server"]);
        assert_eq!(found[1].args, ["acp"]);

        let off: AgentsConfig = serde_json::from_str(r#"{"detect": false}"#).unwrap();
        assert!(harnesses(&off, &env).is_empty());
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
