//! The model lists harnesses reported, kept on disk per harness and version.
//!
//! The model picker reads this cache, so it opens instantly and never starts
//! a harness itself. Each installed harness is probed once in the background
//! when core starts (and again when its list goes stale); the answer is
//! written here as one JSON file per harness:
//!
//! ```text
//! <data>/models/<harness>.json
//! {"harness": "codex", "version": "...", "fetched_at": 1793404800,
//!  "models": [{"id": "gpt-6.1-sol", "name": "GPT-6.1 Sol"}]}
//! ```
//!
//! **Version.** A harness's version is read from its launch command without
//! running it: the resolved binary path (Homebrew and npm keep a version in
//! it), the binary's size and modification time, and the launch arguments
//! (Claude Code's `npx` package is pinned there). Upgrading a harness
//! replaces its binary, so the version changes and the cached list counts as
//! stale.
//!
//! **Stale.** A list from another version, or older than [`MAX_AGE_SECS`], is
//! still shown (it is the best guess there is) while a background probe
//! replaces it. Files are written atomically (a temporary file renamed over
//! the old one), so a crash mid-write never leaves half a file; an unreadable
//! file is treated as missing.

use std::{
    io,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use serde::{Deserialize, Serialize};

use crate::harness::ModelInfo;

/// How long a cached list counts as fresh: one day.
pub const MAX_AGE_SECS: u64 = 24 * 60 * 60;

/// One cached model.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedModel {
    /// Harness-native id; `None` is the harness default.
    #[serde(default)]
    pub id: Option<String>,
    pub name: String,
}

/// One harness's cached model list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedModels {
    pub harness: String,
    /// See [`version_of`].
    pub version: String,
    /// When the harness reported it, seconds since the Unix epoch.
    pub fetched_at: u64,
    pub models: Vec<CachedModel>,
}

/// How a cached list compares with the installed harness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Freshness {
    /// Same version, younger than [`MAX_AGE_SECS`]: no probe needed.
    Fresh,
    /// Same version but old: shown, and probed again in the background.
    Stale,
    /// Reported by another version of the harness: shown, and probed again.
    OtherVersion,
}

impl CachedModels {
    pub fn new(harness: &str, version: &str, fetched_at: u64, models: &[ModelInfo]) -> Self {
        CachedModels {
            harness: harness.to_string(),
            version: version.to_string(),
            fetched_at,
            models: models
                .iter()
                .map(|m| CachedModel {
                    id: m.id.as_deref().map(str::to_string),
                    name: m.name.to_string(),
                })
                .collect(),
        }
    }

    pub fn models(&self) -> Vec<ModelInfo> {
        self.models
            .iter()
            .map(|m| ModelInfo::new(m.id.as_deref(), &m.name))
            .collect()
    }

    pub fn freshness(&self, version: &str, now: u64) -> Freshness {
        if self.version != version {
            Freshness::OtherVersion
        } else if now.saturating_sub(self.fetched_at) >= MAX_AGE_SECS {
            Freshness::Stale
        } else {
            Freshness::Fresh
        }
    }
}

/// Seconds since the Unix epoch.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// The harness's version, read from its launch command without running it
/// (see the module docs). A command that can't be read (not a file) is still
/// a version: its name and arguments.
pub fn version_of(command: &Path, args: &[String]) -> String {
    let resolved = std::fs::canonicalize(command).unwrap_or_else(|_| command.to_path_buf());
    let (size, modified) = match std::fs::metadata(&resolved) {
        Ok(m) => (
            m.len().to_string(),
            m.modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or_else(|| "?".to_string(), |d| d.as_secs().to_string()),
        ),
        Err(_) => ("?".to_string(), "?".to_string()),
    };
    format!(
        "{} {size} {modified} {}",
        resolved.display(),
        args.join(" ")
    )
}

/// The cache directory under the app's data directory.
pub fn dir(data: &Path) -> PathBuf {
    data.join("models")
}

/// The file for `harness` in `dir`. Harness ids are preset ids
/// (`claude-code`); anything else is reduced to safe characters.
pub fn path(dir: &Path, harness: &str) -> PathBuf {
    let safe: String = harness
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    dir.join(format!("{safe}.json"))
}

/// `harness`'s cached list, if there is a readable one.
pub fn load(dir: &Path, harness: &str) -> Option<CachedModels> {
    let text = std::fs::read_to_string(path(dir, harness)).ok()?;
    let cached: CachedModels = serde_json::from_str(&text).ok()?;
    (cached.harness == harness).then_some(cached)
}

/// Writes `cached` atomically: a temporary file in the same directory, then
/// a rename over the old file.
pub fn save(dir: &Path, cached: &CachedModels) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let target = path(dir, &cached.harness);
    let text = serde_json::to_string_pretty(cached).map_err(io::Error::other)?;
    write_atomic(&target, text.as_bytes())
}

/// Writes `bytes` to `target` through a temporary sibling and a rename, so
/// readers see the old file or the new one, never half of one.
pub fn write_atomic(target: &Path, bytes: &[u8]) -> io::Result<()> {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let tmp = target.with_file_name(format!(".{name}.tmp-{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    match std::fs::rename(&tmp, target) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "chaos-model-cache-{name}-{}-{}",
            std::process::id(),
            now_secs()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_saved_list_loads_back_and_knows_when_it_is_stale() {
        let dir = temp_dir("roundtrip");
        let models = [
            ModelInfo::new(None, "Codex default"),
            ModelInfo::new(Some("gpt-6.1-sol"), "GPT-6.1 Sol"),
        ];
        let cached = CachedModels::new("codex", "v1", 1_000, &models);
        save(&dir, &cached).unwrap();
        let back = load(&dir, "codex").unwrap();
        assert_eq!(back, cached);
        assert_eq!(back.models(), models);
        assert_eq!(back.freshness("v1", 1_000 + 60), Freshness::Fresh);
        assert_eq!(back.freshness("v1", 1_000 + MAX_AGE_SECS), Freshness::Stale);
        assert_eq!(back.freshness("v2", 1_000), Freshness::OtherVersion);
        // Saving again replaces it; no temporary file is left behind.
        save(&dir, &CachedModels::new("codex", "v2", 2_000, &models[1..])).unwrap();
        assert_eq!(load(&dir, "codex").unwrap().version, "v2");
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["codex.json"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_broken_or_mismatched_file_is_no_cache() {
        let dir = temp_dir("broken");
        assert!(load(&dir, "fx").is_none());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(path(&dir, "fx"), "{not json").unwrap();
        assert!(load(&dir, "fx").is_none());
        // A file whose content names another harness is not this one's.
        let other = CachedModels::new("vibe", "v", 1, &[]);
        std::fs::write(path(&dir, "fx"), serde_json::to_string(&other).unwrap()).unwrap();
        assert!(load(&dir, "fx").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_version_changes_when_the_binary_or_its_arguments_do() {
        let dir = temp_dir("version");
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("fx");
        std::fs::write(&bin, "one").unwrap();
        let a = version_of(&bin, &["acp".to_string()]);
        assert_eq!(a, version_of(&bin, &["acp".to_string()]));
        assert_ne!(a, version_of(&bin, &["acp".to_string(), "--x".to_string()]));
        std::fs::write(&bin, "a longer binary").unwrap();
        assert_ne!(a, version_of(&bin, &["acp".to_string()]), "size changed");
        // A bare command name is still a version.
        assert!(version_of(Path::new("npx"), &[]).starts_with("npx"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn harness_ids_stay_inside_the_cache_directory() {
        let dir = Path::new("/data/models");
        assert_eq!(
            path(dir, "claude-code"),
            Path::new("/data/models/claude-code.json")
        );
        assert_eq!(
            path(dir, "../etc/passwd"),
            Path::new("/data/models/___etc_passwd.json")
        );
    }
}
