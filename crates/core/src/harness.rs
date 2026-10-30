//! Harnesses and the models they run.
//!
//! A chat member is a **model** ("GPT-6.1 Sol", "Claude Opus 5.5", "Devstral")
//! running through a **harness** (Codex, fx, Mistral Vibe, Claude Code, ...).
//! The harness is a detail underneath: it decides how the process is started
//! and which protocol it speaks, never how the room treats the member. One
//! harness can host several model members; each member gets its own adapter
//! (and so its own processes and sessions).
//!
//! This module is pure data and logic, no I/O except [`detect`]'s file checks
//! (injected for tests):
//!
//! - [`PRESETS`]: one row per harness we know: how to find it, how to launch
//!   it, which transport it speaks and a fallback model list.
//! - [`detect`]: which harnesses are installed, from `PATH` plus the usual
//!   install directories (a macOS app launched from Finder gets a minimal
//!   `PATH`, so the directories matter).
//! - [`ModelInfo`] / [`Catalog`]: the models each harness offers (live when a
//!   harness reported them, else the preset list), grouped by model so the
//!   user picks a model first and a harness only when several can run it.
//! - [`rank`]: the model picker's search ranking, ported from T3 Code.
//!
//! Which presets exist, and why each transport was chosen, is written up in
//! `docs/agent-adapters.md`.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

/// The wire protocol a harness is driven over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    /// Agent Client Protocol over stdio (`crate::acp`), natively or through an
    /// adapter binary.
    Acp,
    /// Codex's own app-server JSON-RPC over stdio (`crate::codex`).
    CodexAppServer,
}

impl Transport {
    pub fn label(self) -> &'static str {
        match self {
            Transport::Acp => "acp",
            Transport::CodexAppServer => "codex app-server",
        }
    }
}

/// How a harness picks its model for a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelSelect {
    /// ACP `session/set_config_option` on the session's model option
    /// (category `model`, or id `model`), after `session/new`.
    AcpConfigOption,
    /// `model` on Codex's `thread/start`.
    CodexThreadStart,
}

/// One way to start a harness: a binary and its arguments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Launch {
    pub command: &'static str,
    pub args: &'static [&'static str],
}

/// A model a preset knows about without asking the harness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModelPreset {
    /// The harness's own id, passed to it as is. `None` is "whatever the
    /// harness is configured to use".
    pub id: Option<&'static str>,
    pub name: &'static str,
}

#[derive(Debug)]
pub struct HarnessPreset {
    /// Stable id, stored with every member ("codex", "fx", "vibe", ...).
    pub id: &'static str,
    pub name: &'static str,
    /// Brand mark id (`model::Brand` names it on the app side).
    pub icon: &'static str,
    pub transport: Transport,
    /// Binaries whose presence means the harness is installed, best first.
    pub detect: &'static [&'static str],
    /// How to launch it once a detected binary is known: `{bin}` in
    /// `command` is that binary.
    pub launch: Launch,
    /// An adapter binary to prefer when it is installed (Claude Code's
    /// `claude-agent-acp`), and the `npx` package to fall back to.
    pub adapter: Option<Adapter>,
    pub model_select: ModelSelect,
    /// Fallback list, shown until the harness reports its own. The first
    /// entry is the one a fresh project's auto-detected member uses.
    pub models: &'static [ModelPreset],
    pub install: &'static str,
}

#[derive(Debug)]
pub struct Adapter {
    pub binary: &'static str,
    pub args: &'static [&'static str],
    pub npx_package: &'static str,
}

/// `{bin}` in [`Launch::command`]: the detected binary itself.
pub const BIN: &str = "{bin}";

const DEFAULT_ONLY: &[ModelPreset] = &[ModelPreset {
    id: None,
    name: "Default model",
}];

/// Every harness we know, in Hxmbl's priority order (2026-10-30): Codex, fx,
/// Mistral Vibe, then the rest. Model fallbacks for Codex and Claude follow
/// T3 Code's model manifest of 2026-10-28 (MIT, github.com/pingdotgg/t3code);
/// the rest only know "the harness default" until the harness reports its
/// list.
pub static PRESETS: &[HarnessPreset] = &[
    HarnessPreset {
        id: "codex",
        name: "Codex",
        icon: "openai",
        transport: Transport::CodexAppServer,
        detect: &["codex"],
        launch: Launch {
            command: BIN,
            args: &["app-server"],
        },
        adapter: None,
        model_select: ModelSelect::CodexThreadStart,
        models: &[
            ModelPreset {
                id: None,
                name: "Default model",
            },
            ModelPreset {
                id: Some("gpt-6.1-sol"),
                name: "GPT-6.1 Sol",
            },
            ModelPreset {
                id: Some("gpt-6-astra"),
                name: "GPT-6 Astra",
            },
            ModelPreset {
                id: Some("gpt-6-luna"),
                name: "GPT-6 Luna",
            },
        ],
        install: "npm i -g @openai/codex",
    },
    HarnessPreset {
        id: "fx",
        name: "fx",
        icon: "vercel",
        transport: Transport::Acp,
        detect: &["fx"],
        launch: Launch {
            command: BIN,
            args: &["acp"],
        },
        adapter: None,
        model_select: ModelSelect::AcpConfigOption,
        models: DEFAULT_ONLY,
        install: "curl -fsSL https://fx.sh/setup.sh | bash",
    },
    HarnessPreset {
        id: "vibe",
        name: "Mistral Vibe",
        icon: "mistral",
        transport: Transport::Acp,
        detect: &["vibe-acp", "vibe"],
        // `vibe-acp` is its own binary, installed next to `vibe`.
        launch: Launch {
            command: "vibe-acp",
            args: &[],
        },
        adapter: None,
        model_select: ModelSelect::AcpConfigOption,
        models: &[ModelPreset {
            id: None,
            name: "Devstral (Vibe default)",
        }],
        install: "curl -LsSf https://mistral.ai/vibe/install.sh | bash",
    },
    HarnessPreset {
        id: "claude-code",
        name: "Claude Code",
        icon: "claude",
        transport: Transport::Acp,
        detect: &["claude", "claude-agent-acp"],
        launch: Launch {
            command: "claude-agent-acp",
            args: &[],
        },
        adapter: Some(Adapter {
            binary: "claude-agent-acp",
            args: &[],
            npx_package: "@agentclientprotocol/claude-agent-acp@0.99.1",
        }),
        model_select: ModelSelect::AcpConfigOption,
        models: &[
            ModelPreset {
                id: None,
                name: "Default model",
            },
            ModelPreset {
                id: Some("claude-opus-5-5"),
                name: "Claude Opus 5.5",
            },
            ModelPreset {
                id: Some("claude-sonnet-5-5"),
                name: "Claude Sonnet 5.5",
            },
            ModelPreset {
                id: Some("claude-haiku-5-5"),
                name: "Claude Haiku 5.5",
            },
        ],
        install: "npm i -g @anthropic-ai/claude-code",
    },
    HarnessPreset {
        id: "copilot",
        name: "GitHub Copilot CLI",
        icon: "copilot",
        transport: Transport::Acp,
        detect: &["copilot"],
        launch: Launch {
            command: BIN,
            args: &["--acp"],
        },
        adapter: None,
        model_select: ModelSelect::AcpConfigOption,
        models: DEFAULT_ONLY,
        install: "npm i -g @github/copilot",
    },
    HarnessPreset {
        id: "opencode",
        name: "opencode",
        icon: "opencode",
        transport: Transport::Acp,
        detect: &["opencode"],
        launch: Launch {
            command: BIN,
            args: &["acp"],
        },
        adapter: None,
        model_select: ModelSelect::AcpConfigOption,
        models: DEFAULT_ONLY,
        install: "curl -fsSL https://opencode.ai/install | bash",
    },
    HarnessPreset {
        id: "devin",
        name: "Devin",
        icon: "devin",
        transport: Transport::Acp,
        detect: &["devin"],
        launch: Launch {
            command: BIN,
            args: &["acp"],
        },
        adapter: None,
        model_select: ModelSelect::AcpConfigOption,
        models: DEFAULT_ONLY,
        install: "Devin for Terminal (devin CLI), see the ACP registry",
    },
    HarnessPreset {
        id: "cursor",
        name: "Cursor Agent",
        icon: "cursor",
        transport: Transport::Acp,
        detect: &["cursor-agent"],
        launch: Launch {
            command: BIN,
            args: &["acp"],
        },
        adapter: None,
        model_select: ModelSelect::AcpConfigOption,
        models: DEFAULT_ONLY,
        install: "curl https://cursor.com/install -fsS | bash",
    },
    HarnessPreset {
        id: "gemini",
        name: "Gemini CLI",
        icon: "gemini",
        transport: Transport::Acp,
        detect: &["gemini"],
        launch: Launch {
            command: BIN,
            args: &["--acp"],
        },
        adapter: None,
        model_select: ModelSelect::AcpConfigOption,
        models: DEFAULT_ONLY,
        install: "npm i -g @google/gemini-cli",
    },
];

pub fn preset(id: &str) -> Option<&'static HarnessPreset> {
    PRESETS.iter().find(|p| p.id == id)
}

// ---------- detection ----------

/// What [`detect`] looks at. `exists` is injected so tests never touch the
/// real filesystem.
pub struct DetectEnv<'a> {
    /// `PATH`, split.
    pub path: Vec<PathBuf>,
    pub home: Option<PathBuf>,
    pub is_executable: &'a dyn Fn(&Path) -> bool,
}

impl DetectEnv<'static> {
    /// The real environment.
    pub fn from_process() -> DetectEnv<'static> {
        DetectEnv {
            path: std::env::var_os("PATH")
                .map(|p| std::env::split_paths(&p).collect())
                .unwrap_or_default(),
            home: std::env::var_os("HOME").map(PathBuf::from),
            is_executable: &is_executable_file,
        }
    }
}

fn is_executable_file(p: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(p) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Install directories checked after `PATH`, relative to `$HOME` unless
/// absolute. Covers npm/bun/volta globals, cargo, Homebrew and the curl
/// installers (fx, opencode, Cursor, Vibe write to `~/.local/bin` or their
/// own `bin`).
pub const USUAL_DIRS: &[&str] = &[
    ".local/bin",
    ".fx/bin",
    ".opencode/bin",
    ".bun/bin",
    ".npm-global/bin",
    ".volta/bin",
    ".cargo/bin",
    ".cursor/bin",
    ".devin/bin",
    "/opt/homebrew/bin",
    "/usr/local/bin",
    "/usr/bin",
];

impl DetectEnv<'_> {
    fn dirs(&self) -> Vec<PathBuf> {
        let mut dirs = self.path.clone();
        for d in USUAL_DIRS {
            let p = Path::new(d);
            let full = if p.is_absolute() {
                p.to_path_buf()
            } else if let Some(home) = &self.home {
                home.join(p)
            } else {
                continue;
            };
            if !dirs.contains(&full) {
                dirs.push(full);
            }
        }
        dirs
    }

    /// The first `name` in the searched directories.
    pub fn find(&self, name: &str) -> Option<PathBuf> {
        let exe = if cfg!(windows) {
            format!("{name}.exe")
        } else {
            name.to_string()
        };
        self.dirs()
            .into_iter()
            .map(|d| d.join(&exe))
            .find(|p| (self.is_executable)(p))
    }
}

/// An installed harness and the exact command that starts it.
#[derive(Clone, Debug)]
pub struct Detected {
    pub preset: &'static HarnessPreset,
    /// The binary that showed it is installed.
    pub found: PathBuf,
    pub command: PathBuf,
    pub args: Vec<String>,
}

/// Every installed harness, in [`PRESETS`] order.
pub fn detect(env: &DetectEnv) -> Vec<Detected> {
    PRESETS.iter().filter_map(|p| detect_one(p, env)).collect()
}

pub fn detect_one(p: &'static HarnessPreset, env: &DetectEnv) -> Option<Detected> {
    let found = p.detect.iter().find_map(|b| env.find(b))?;
    let (command, args) = launch_for(p, &found, env)?;
    Some(Detected {
        preset: p,
        found,
        command,
        args,
    })
}

fn launch_for(
    p: &HarnessPreset,
    found: &Path,
    env: &DetectEnv,
) -> Option<(PathBuf, Vec<String>)> {
    let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    if let Some(adapter) = &p.adapter {
        if let Some(bin) = env.find(adapter.binary) {
            return Some((bin, args(adapter.args)));
        }
        // Not installed: run it through npx, which needs node.
        let npx = env.find("npx")?;
        let mut a = vec!["-y".to_string(), adapter.npx_package.to_string()];
        a.extend(args(adapter.args));
        return Some((npx, a));
    }
    let command = if p.launch.command == BIN {
        found.to_path_buf()
    } else {
        // A sibling binary (`vibe-acp` next to `vibe`), else a PATH lookup.
        let sibling = found.with_file_name(p.launch.command);
        if (env.is_executable)(&sibling) {
            sibling
        } else {
            env.find(p.launch.command)?
        }
    };
    Some((command, args(p.launch.args)))
}

// ---------- models ----------

/// Where a model list came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelSource {
    /// The preset table: a guess until the harness answers.
    Preset,
    /// The harness reported it (ACP session config options, Codex
    /// `model/list`).
    Live,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelInfo {
    /// Harness-native id; `None` is the harness default.
    pub id: Option<Arc<str>>,
    pub name: Arc<str>,
}

impl ModelInfo {
    pub fn new(id: Option<&str>, name: &str) -> Self {
        ModelInfo {
            id: id.map(Arc::from),
            name: Arc::from(name),
        }
    }

    pub fn default_for(harness: &HarnessPreset) -> Self {
        ModelInfo::new(None, &format!("{} default", harness.name))
    }
}

pub fn preset_models(p: &HarnessPreset) -> Vec<ModelInfo> {
    p.models
        .iter()
        .map(|m| match m.id {
            None if m.name == "Default model" => ModelInfo::default_for(p),
            id => ModelInfo::new(id, m.name),
        })
        .collect()
}

/// The key that makes the same model through two harnesses one row: the
/// part after any `vendor/` prefix, lowercased, with `.`, `_` and spaces
/// as `-`. So `anthropic/claude-opus-5.5` (fx through the AI Gateway) and
/// `claude-opus-5-5` (Claude Code) are one model. A harness default has no
/// key of its own: it is that harness's row.
pub fn model_key(harness: &str, m: &ModelInfo) -> String {
    let Some(id) = &m.id else {
        return format!("default@{harness}");
    };
    let tail = id.rsplit('/').next().unwrap_or(id);
    let mut out = String::with_capacity(tail.len());
    for ch in tail.chars() {
        let ch = match ch {
            '.' | '_' | ' ' | ':' => '-',
            c => c.to_ascii_lowercase(),
        };
        if ch == '-' && out.ends_with('-') {
            continue;
        }
        out.push(ch);
    }
    out.trim_matches('-').to_string()
}

/// One harness's models as known now.
#[derive(Clone, Debug)]
pub struct HarnessModels {
    pub harness: &'static HarnessPreset,
    pub source: ModelSource,
    pub models: Vec<ModelInfo>,
    /// Why the live list is missing, if a discovery failed ("signed out").
    pub note: Option<Arc<str>>,
}

/// A model and every installed harness that can run it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogModel {
    pub key: String,
    pub name: Arc<str>,
    pub offers: Vec<Offer>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offer {
    pub harness: &'static str,
    pub harness_name: &'static str,
    pub icon: &'static str,
    /// The id to hand that harness.
    pub model: Option<Arc<str>>,
    pub source: ModelSource,
}

/// Models grouped across harnesses, in harness priority order, then each
/// harness's own order.
#[derive(Clone, Debug, Default)]
pub struct Catalog {
    pub models: Vec<CatalogModel>,
}

impl Catalog {
    pub fn build(harnesses: &[HarnessModels]) -> Catalog {
        let mut order: Vec<String> = Vec::new();
        let mut by_key: BTreeMap<String, CatalogModel> = BTreeMap::new();
        let mut ranked: Vec<&HarnessModels> = harnesses.iter().collect();
        ranked.sort_by_key(|h| PRESETS.iter().position(|p| p.id == h.harness.id));
        for h in ranked {
            for m in &h.models {
                let key = model_key(h.harness.id, m);
                let offer = Offer {
                    harness: h.harness.id,
                    harness_name: h.harness.name,
                    icon: h.harness.icon,
                    model: m.id.clone(),
                    source: h.source,
                };
                match by_key.get_mut(&key) {
                    Some(row) => {
                        if !row.offers.iter().any(|o| o.harness == offer.harness) {
                            row.offers.push(offer);
                        }
                    }
                    None => {
                        order.push(key.clone());
                        by_key.insert(
                            key.clone(),
                            CatalogModel {
                                key,
                                name: m.name.clone(),
                                offers: vec![offer],
                            },
                        );
                    }
                }
            }
        }
        Catalog {
            models: order
                .into_iter()
                .filter_map(|k| by_key.remove(&k))
                .collect(),
        }
    }

    /// Models one harness offers (the picker's harness filter).
    pub fn for_harness<'a>(&'a self, harness: &'a str) -> impl Iterator<Item = &'a CatalogModel> {
        self.models
            .iter()
            .filter(move |m| m.offers.iter().any(|o| o.harness == harness))
    }
}

// ---------- picker search (ported from T3 Code) ----------
//
// Ported from T3 Code's `apps/web/src/components/chat/modelPickerSearch.ts`
// and the `scoreQueryMatch` it uses (MIT, Copyright (c) 2026 T3 Tools Inc.,
// github.com/pingdotgg/t3code). Same shape: every query token must match
// some field; earlier fields and tighter matches (exact < prefix < word
// boundary < substring < fuzzy) score lower, which is better; an exact name
// match wins outright. Favourites are left out: there are none here yet.

const EXACT_NAME_SCORE: i64 = -1000;

fn normalize(s: &str) -> String {
    s.trim().to_lowercase()
}

fn score_field(field: &str, token: &str, base: i64) -> Option<i64> {
    if field.is_empty() {
        return None;
    }
    if field == token {
        return Some(base);
    }
    if field.starts_with(token) {
        return Some(base + 2);
    }
    let boundary = field
        .match_indices(token)
        .any(|(i, _)| i > 0 && !field.as_bytes()[i - 1].is_ascii_alphanumeric());
    if boundary {
        return Some(base + 4);
    }
    if field.contains(token) {
        return Some(base + 6);
    }
    if token.chars().count() >= 3 && is_subsequence(token, field) {
        return Some(base + 100);
    }
    None
}

fn is_subsequence(needle: &str, hay: &str) -> bool {
    let mut it = hay.chars();
    needle.chars().all(|c| it.any(|h| h == c))
}

/// What the picker searches, per row.
pub struct Searchable<'a> {
    pub name: &'a str,
    pub harness_names: Vec<&'a str>,
    pub ids: Vec<&'a str>,
}

/// `None` when the row does not match; lower is better.
pub fn score(row: &Searchable, query: &str) -> Option<i64> {
    let tokens: Vec<String> = normalize(query)
        .split_whitespace()
        .map(str::to_string)
        .collect();
    if tokens.is_empty() {
        return Some(0);
    }
    let name = normalize(row.name);
    if tokens.join(" ") == name.split_whitespace().collect::<Vec<_>>().join(" ") {
        return Some(EXACT_NAME_SCORE);
    }
    let mut fields = vec![name];
    fields.extend(row.ids.iter().map(|s| normalize(s)));
    fields.extend(row.harness_names.iter().map(|s| normalize(s)));
    fields.push(normalize(
        &std::iter::once(row.name)
            .chain(row.ids.iter().copied())
            .chain(row.harness_names.iter().copied())
            .collect::<Vec<_>>()
            .join(" "),
    ));
    let mut total = 0;
    for t in &tokens {
        let best = fields
            .iter()
            .enumerate()
            .filter_map(|(i, f)| score_field(f, t, i as i64 * 10))
            .min()?;
        total += best;
    }
    Some(total)
}

/// Rows matching `query`, best first; ties keep their catalog order.
pub fn rank<'a>(models: impl IntoIterator<Item = &'a CatalogModel>, query: &str) -> Vec<&'a CatalogModel> {
    let mut scored: Vec<(i64, usize, &CatalogModel)> = models
        .into_iter()
        .enumerate()
        .filter_map(|(i, m)| {
            let row = Searchable {
                name: &m.name,
                harness_names: m.offers.iter().map(|o| o.harness_name).collect(),
                ids: m.offers.iter().filter_map(|o| o.model.as_deref()).collect(),
            };
            score(&row, query).map(|s| (s, i, m))
        })
        .collect();
    scored.sort_by_key(|(s, i, _)| (*s, *i));
    scored.into_iter().map(|(_, _, m)| m).collect()
}

/// A handle for a new model member: the model's short name, unique against
/// `taken` (case-insensitive) by adding the harness, then a number.
pub fn handle_for(model_name: &str, harness: &str, taken: &dyn Fn(&str) -> bool) -> String {
    let mut base: String = model_name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    // "claude-opus-5-5" reads fine; "default" alone does not.
    while base.contains("--") {
        base = base.replace("--", "-");
    }
    let base = base.trim_matches('-').to_string();
    let base = if base.is_empty() || base.ends_with("default") {
        harness.to_string()
    } else {
        base
    };
    if !taken(&base) {
        return base;
    }
    let with_harness = format!("{base}-{harness}");
    if !taken(&with_harness) {
        return with_harness;
    }
    (2..)
        .map(|n| format!("{with_harness}-{n}"))
        .find(|h| !taken(h))
        .expect("an unused handle")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn env_with<'a>(
        files: &'a HashSet<PathBuf>,
        path: &[&str],
        exists: &'a dyn Fn(&Path) -> bool,
    ) -> DetectEnv<'a> {
        let _ = files;
        DetectEnv {
            path: path.iter().map(PathBuf::from).collect(),
            home: Some(PathBuf::from("/home/hx")),
            is_executable: exists,
        }
    }

    #[test]
    fn presets_are_in_priority_order_and_unique() {
        let ids: Vec<_> = PRESETS.iter().map(|p| p.id).collect();
        assert_eq!(&ids[..3], &["codex", "fx", "vibe"]);
        assert!(!ids.contains(&"pi"), "Pi was dropped (Hxmbl, 2026-10-30)");
        let set: HashSet<_> = ids.iter().collect();
        assert_eq!(set.len(), ids.len());
        for p in PRESETS {
            assert!(!p.models.is_empty(), "{} has a fallback model", p.id);
            assert!(!p.detect.is_empty());
        }
    }

    #[test]
    fn detects_from_path_and_usual_dirs() {
        let files: HashSet<PathBuf> = [
            "/usr/x/bin/codex",
            "/home/hx/.fx/bin/fx",
            "/home/hx/.local/bin/vibe",
            "/home/hx/.local/bin/vibe-acp",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        let exists = |p: &Path| files.contains(p);
        let env = env_with(&files, &["/usr/x/bin"], &exists);
        let found = detect(&env);
        let ids: Vec<_> = found.iter().map(|d| d.preset.id).collect();
        assert_eq!(ids, ["codex", "fx", "vibe"]);
        assert_eq!(found[0].command, PathBuf::from("/usr/x/bin/codex"));
        assert_eq!(found[0].args, ["app-server"]);
        assert_eq!(found[1].args, ["acp"]);
        // Vibe runs its sibling ACP binary, not the TUI.
        assert_eq!(found[2].command, PathBuf::from("/home/hx/.local/bin/vibe-acp"));
    }

    #[test]
    fn claude_code_prefers_the_installed_adapter_then_npx() {
        let files: HashSet<PathBuf> = ["/bin/claude", "/bin/npx"].iter().map(PathBuf::from).collect();
        let exists = |p: &Path| files.contains(p);
        let env = env_with(&files, &["/bin"], &exists);
        let d = detect_one(preset("claude-code").unwrap(), &env).unwrap();
        assert_eq!(d.command, PathBuf::from("/bin/npx"));
        assert_eq!(d.args[0], "-y");
        assert!(d.args[1].starts_with("@agentclientprotocol/claude-agent-acp@"));

        let files: HashSet<PathBuf> = ["/bin/claude", "/bin/claude-agent-acp"]
            .iter()
            .map(PathBuf::from)
            .collect();
        let exists = |p: &Path| files.contains(p);
        let env = env_with(&files, &["/bin"], &exists);
        let d = detect_one(preset("claude-code").unwrap(), &env).unwrap();
        assert_eq!(d.command, PathBuf::from("/bin/claude-agent-acp"));

        // Neither the adapter nor npx: not runnable, so not detected.
        let files: HashSet<PathBuf> = ["/bin/claude"].iter().map(PathBuf::from).collect();
        let exists = |p: &Path| files.contains(p);
        let env = env_with(&files, &["/bin"], &exists);
        assert!(detect_one(preset("claude-code").unwrap(), &env).is_none());
    }

    #[test]
    fn nothing_installed_detects_nothing() {
        let files = HashSet::new();
        let exists = |_: &Path| false;
        assert!(detect(&env_with(&files, &["/bin"], &exists)).is_empty());
    }

    #[test]
    fn model_keys_merge_vendor_prefixed_and_dotted_ids() {
        let a = ModelInfo::new(Some("anthropic/claude-opus-5.5"), "Claude Opus 5.5");
        let b = ModelInfo::new(Some("claude-opus-5-5"), "Claude Opus 5.5");
        assert_eq!(model_key("fx", &a), model_key("claude-code", &b));
        let d = ModelInfo::new(None, "fx default");
        assert_eq!(model_key("fx", &d), "default@fx");
        assert_ne!(model_key("fx", &d), model_key("codex", &d));
    }

    fn hm(id: &str, source: ModelSource, models: &[(Option<&str>, &str)]) -> HarnessModels {
        HarnessModels {
            harness: preset(id).unwrap(),
            source,
            models: models.iter().map(|(i, n)| ModelInfo::new(*i, n)).collect(),
            note: None,
        }
    }

    #[test]
    fn catalog_groups_one_model_across_harnesses() {
        let cat = Catalog::build(&[
            // Out of priority order on purpose.
            hm(
                "claude-code",
                ModelSource::Preset,
                &[(Some("claude-opus-5-5"), "Claude Opus 5.5")],
            ),
            hm(
                "fx",
                ModelSource::Live,
                &[
                    (None, "fx default"),
                    (Some("anthropic/claude-opus-5.5"), "Claude Opus 5.5"),
                    (Some("openai/gpt-6.1-sol"), "GPT-6.1 Sol"),
                ],
            ),
            hm("codex", ModelSource::Live, &[(Some("gpt-6.1-sol"), "GPT-6.1 Sol")]),
        ]);
        let names: Vec<_> = cat.models.iter().map(|m| &*m.name).collect();
        // Codex first (priority), so GPT-6.1 Sol leads; fx's rows follow.
        assert_eq!(names, ["GPT-6.1 Sol", "fx default", "Claude Opus 5.5"]);
        let sol = &cat.models[0];
        let harnesses: Vec<_> = sol.offers.iter().map(|o| o.harness).collect();
        assert_eq!(harnesses, ["codex", "fx"]);
        assert_eq!(sol.offers[1].model.as_deref(), Some("openai/gpt-6.1-sol"));
        let opus = &cat.models[2];
        assert_eq!(
            opus.offers.iter().map(|o| o.harness).collect::<Vec<_>>(),
            ["fx", "claude-code"]
        );
        assert_eq!(cat.for_harness("codex").count(), 1);
        assert_eq!(cat.for_harness("fx").count(), 3);
    }

    #[test]
    fn search_ranks_exact_then_prefix_then_fuzzy() {
        let cat = Catalog::build(&[
            hm(
                "codex",
                ModelSource::Preset,
                &[
                    (Some("gpt-6-luna"), "GPT-6 Luna"),
                    (Some("gpt-6.1-sol"), "GPT-6.1 Sol"),
                ],
            ),
            hm(
                "claude-code",
                ModelSource::Preset,
                &[
                    (Some("claude-sonnet-5-5"), "Claude Sonnet 5.5"),
                    (Some("claude-opus-5-5"), "Claude Opus 5.5"),
                ],
            ),
        ]);
        let names = |q: &str| {
            rank(&cat.models, q)
                .into_iter()
                .map(|m| m.name.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(names("").len(), 4);
        assert_eq!(names("opus"), ["Claude Opus 5.5"]);
        assert_eq!(names("claude opus 5.5")[0], "Claude Opus 5.5");
        // Every token must match something.
        assert!(names("opus luna").is_empty());
        // Harness names are searchable too.
        assert_eq!(names("codex").len(), 2);
        // Fuzzy (subsequence) only from three characters, and ranked last.
        assert_eq!(names("sol")[0], "GPT-6.1 Sol");
        assert_eq!(names("gpt sol"), ["GPT-6.1 Sol"]);
    }

    #[test]
    fn handles_stay_unique() {
        let taken: HashSet<&str> = ["gpt-6-1-sol", "gpt-6-1-sol-fx"].into_iter().collect();
        let t = |h: &str| taken.contains(h);
        assert_eq!(handle_for("Claude Opus 5.5", "claude-code", &t), "claude-opus-5-5");
        assert_eq!(handle_for("GPT-6.1 Sol", "codex", &t), "gpt-6-1-sol-codex");
        assert_eq!(handle_for("GPT-6.1 Sol", "fx", &t), "gpt-6-1-sol-fx-2");
        assert_eq!(handle_for("fx default", "fx", &|_| false), "fx");
    }
}
