//! Recently opened projects and launch restoration state.
//!
//! Stored in `<data>/recents.json` as plain JSON. The file is written
//! atomically (temp file then rename). A missing or unreadable file is treated
//! as "no recents" and never crashes the app.

use std::{
    fs, io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

pub const FILE: &str = "recents.json";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecentsFile {
    /// The project id last active at launch, if any.
    pub last_active_project_id: Option<String>,
    /// The channel name last selected in the active project (e.g. "main").
    /// Restored on launch; not a running session/draft.
    pub last_active_channel: Option<String>,
    /// Known projects, most recently opened last.
    pub projects: Vec<RecentProject>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecentProject {
    /// Stable project id (matches `.chaos/project.toml`).
    pub id: String,
    /// Last known working directory for this project.
    pub path: Option<String>,
    /// Display name.
    pub name: String,
    /// Seconds since the Unix epoch when last opened.
    pub last_opened: u64,
    /// True when the last known path was missing/unavailable at restore/open.
    /// Kept in recents; never deleted automatically.
    pub unavailable: bool,
}

pub fn path(data_dir: &Path) -> PathBuf {
    data_dir.join(FILE)
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

pub fn load(data_dir: &Path) -> RecentsFile {
    let path = path(data_dir);
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return RecentsFile::default(),
    };
    serde_json::from_str(&text).unwrap_or_default()
}

pub fn save(data_dir: &Path, recents: &RecentsFile) -> io::Result<()> {
    chaos_core::permissions::ensure_private_dir(data_dir)?;
    let path = path(data_dir);
    let tmp = data_dir.join(format!("{FILE}.tmp"));
    let text = serde_json::to_string_pretty(recents)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    chaos_core::permissions::write_private_file(&tmp, format!("{text}\n").as_bytes())?;
    fs::rename(tmp, path)
}

impl RecentsFile {
    pub fn upsert_opened(
        &mut self,
        id: &str,
        name: &str,
        project_path: Option<&str>,
        channel: Option<&str>,
    ) {
        self.last_active_project_id = Some(id.to_string());
        if let Some(channel) = channel {
            self.last_active_channel = Some(channel.to_string());
        }
        if let Some(entry) = self.projects.iter_mut().find(|p| p.id == id) {
            entry.name = name.to_string();
            if project_path.is_some() {
                entry.path = project_path.map(str::to_string);
            }
            entry.last_opened = now_secs();
            if project_path.is_some() {
                entry.unavailable = false;
            }
        } else {
            self.projects.push(RecentProject {
                id: id.to_string(),
                path: project_path.map(str::to_string),
                name: name.to_string(),
                last_opened: now_secs(),
                unavailable: false,
            });
        }
        self.projects.sort_by_key(|p| p.last_opened);
    }

    pub fn mark_unavailable(&mut self, id: &str) {
        if let Some(entry) = self.projects.iter_mut().find(|p| p.id == id) {
            entry.unavailable = true;
        }
        if self.last_active_project_id.as_deref() == Some(id) {
            // Keep last_active_project_id so the entry remains identifiable;
            // restoration will see it unavailable and open with no project.
        }
    }

    pub fn last_active(&self) -> Option<&RecentProject> {
        let id = self.last_active_project_id.as_deref()?;
        self.projects.iter().find(|p| p.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cc-recents-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_or_corrupt_means_no_recents() {
        let dir = temp("missing");
        assert_eq!(load(&dir), RecentsFile::default());
        fs::write(path(&dir), "{ not json").unwrap();
        assert_eq!(load(&dir), RecentsFile::default());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = temp("roundtrip");
        let mut r = RecentsFile::default();
        r.upsert_opened("p1", "One", Some("/a/one"), Some("main"));
        r.upsert_opened("p2", "Two", Some("/b/two"), Some("dev"));
        save(&dir, &r).unwrap();
        let loaded = load(&dir);
        assert_eq!(loaded.last_active_project_id.as_deref(), Some("p2"));
        assert_eq!(loaded.last_active_channel.as_deref(), Some("dev"));
        assert_eq!(loaded.projects.len(), 2);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn unavailable_is_kept_not_deleted() {
        let mut r = RecentsFile::default();
        r.upsert_opened("p1", "One", Some("/gone"), Some("main"));
        r.mark_unavailable("p1");
        assert!(r.projects.iter().any(|p| p.id == "p1" && p.unavailable));
        assert_eq!(r.last_active_project_id.as_deref(), Some("p1"));
        let active = r.last_active().unwrap();
        assert!(active.unavailable);
        assert_eq!(active.path.as_deref(), Some("/gone"));
    }

    #[test]
    fn restore_state_when_last_project_path_is_gone_persists_as_unavailable() {
        let dir = temp("gone-path");
        let mut r = RecentsFile::default();
        r.upsert_opened("p1", "One", Some("/path/does/not/exist"), Some("design"));
        r.mark_unavailable("p1");
        save(&dir, &r).unwrap();
        let loaded = load(&dir);
        let active = loaded.last_active().expect("last active kept");
        assert_eq!(active.id, "p1");
        assert!(active.unavailable, "entry must remain marked unavailable");
        assert_eq!(loaded.last_active_channel.as_deref(), Some("design"));
        let _ = fs::remove_dir_all(dir);
    }
}
