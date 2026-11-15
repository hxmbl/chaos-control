//! The local user's profile (STRM-644): display name and (later) avatar.
//!
//! Stored separately from project-scoped state so it survives project switches.
//! Persisted in the app data directory as `profile.json`.

use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use serde::{Deserialize, Serialize};

const FILE: &str = "profile.json";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    /// Display name shown in chat/mentions/UI. Empty means not set.
    #[serde(default)]
    pub name: String,
}

impl Default for Profile {
    fn default() -> Self {
        Self { name: String::new() }
    }
}

impl Profile {
    pub fn display_name(&self) -> &str {
        let name = self.name.trim();
        if name.is_empty() { "You" } else { name }
    }

    pub fn monogram(&self) -> String {
        let name = self.display_name();
        name.chars()
            .find(|c| c.is_alphanumeric())
            .map(|c| c.to_uppercase().collect::<String>())
            .unwrap_or_else(|| "?".to_string())
    }

    /// Deterministic hue-like index from name for placeholder avatar color.
    pub fn color_seed(&self) -> u32 {
        let s = self.name.trim().to_lowercase();
        let mut h: u32 = 2166136261;
        for b in s.as_bytes() {
            h ^= *b as u32;
            h = h.wrapping_mul(16777619);
        }
        h
    }
}

pub fn path(data_dir: &Path) -> PathBuf {
    data_dir.join(FILE)
}

pub fn load(data_dir: &Path) -> Profile {
    let path = path(data_dir);
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => Profile::default(),
    }
}

pub fn save(data_dir: &Path, profile: &Profile) -> io::Result<()> {
    let path = path(data_dir);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut text = serde_json::to_string_pretty(profile).map_err(io::Error::other)?;
    text.push('\n');
    // Best-effort atomic-ish write.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(tmp, path)
}

pub fn save_in_background(data_dir: Arc<Path>, profile: Profile) {
    std::thread::spawn(move || {
        if let Err(e) = save(&data_dir, &profile) {
            eprintln!(
                "chaos-control: could not save profile {}: {e}",
                path(&data_dir).display()
            );
        }
    });
}
