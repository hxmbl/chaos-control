//! Devin-only setup: per-folder workspace trust.
//!
//! `devin acp` refuses to run in a folder the member hasn't trusted, and it
//! has no per-folder flag or environment variable for it (the CLI's
//! `--respect-workspace-trust false` only works on the top-level `devin`, and
//! `skip_workspace_trust` in its config turns the check off everywhere).
//! Devin keeps the folders it trusts in
//! `$XDG_DATA_HOME/devin/cli/trusted_workspaces.json` (default
//! `~/.local/share/...`) as `{"trusted_paths": [...]}`, and a subfolder of a
//! trusted path counts as trusted. Before every Devin session start and model
//! lookup the ACP adapter adds the session's folder there with
//! [`ensure_trusted`]. See `docs/agent-adapters.md`.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

use serde_json::Value;

/// Devin's trust file under the data dir.
const TRUST_FILE: [&str; 3] = ["devin", "cli", "trusted_workspaces.json"];

/// Whether `command` is the Devin CLI, by the binary names the Devin preset
/// detects (`crate::harness`).
pub fn is_devin(command: &Path) -> bool {
    let Some(stem) = command.file_stem().and_then(|s| s.to_str()) else {
        return false;
    };
    crate::harness::preset("devin").is_some_and(|p| p.detect.contains(&stem))
}

/// Devin's trusted-workspaces file: `$XDG_DATA_HOME` (when set to an absolute
/// path), else `$HOME/.local/share`. `None` without either.
pub fn trusted_workspaces_file() -> Option<PathBuf> {
    trust_file_from(std::env::var_os("XDG_DATA_HOME"), std::env::var_os("HOME"))
}

fn trust_file_from(xdg_data_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let base = match xdg_data_home.map(PathBuf::from).filter(|p| p.is_absolute()) {
        Some(dir) => dir,
        None => PathBuf::from(home.filter(|h| !h.is_empty())?)
            .join(".local")
            .join("share"),
    };
    Some(TRUST_FILE.iter().fold(base, |p, part| p.join(part)))
}

/// What [`ensure_trusted_in`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trust {
    /// The folder was added.
    Added(PathBuf),
    /// The folder, or a folder above it, was already listed.
    AlreadyTrusted(PathBuf),
    /// Nothing was written; the reason (a malformed file is left untouched).
    Skipped(String),
}

/// Adds `folder` to Devin's trusted workspaces (the file from
/// [`trusted_workspaces_file`]), logging a warning when it can't.
pub fn ensure_trusted(folder: &Path) -> Trust {
    let trust = match trusted_workspaces_file() {
        Some(file) => ensure_trusted_in(&file, folder),
        None => Trust::Skipped("neither XDG_DATA_HOME nor HOME is set".into()),
    };
    if let Trust::Skipped(why) = &trust {
        eprintln!(
            "chaos-control: couldn't mark {} as trusted for Devin ({why}); continuing",
            folder.display()
        );
    }
    trust
}

/// Adds `folder` (canonicalised, absolute) to the `trusted_paths` of `file`,
/// creating the file and its directories if missing and keeping every
/// existing entry. Skips when the folder or a parent is already listed, and
/// leaves a malformed file untouched.
pub fn ensure_trusted_in(file: &Path, folder: &Path) -> Trust {
    let folder = match std::fs::canonicalize(folder) {
        Ok(p) => p,
        Err(e) => return Trust::Skipped(format!("can't resolve {}: {e}", folder.display())),
    };
    let mut doc = match std::fs::read_to_string(file) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(v) if well_formed(&v) => v,
            _ => {
                return Trust::Skipped(format!(
                    "{} is not a trusted_paths list; leaving it alone",
                    file.display()
                ));
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            serde_json::json!({ "trusted_paths": [] })
        }
        Err(e) => return Trust::Skipped(format!("can't read {}: {e}", file.display())),
    };
    let Some(paths) = doc
        .as_object_mut()
        .map(|o| {
            o.entry("trusted_paths")
                .or_insert_with(|| Value::Array(Vec::new()))
        })
        .and_then(Value::as_array_mut)
    else {
        return Trust::Skipped(format!("{} is not a trusted_paths list", file.display()));
    };
    if paths
        .iter()
        .filter_map(Value::as_str)
        .any(|p| folder.starts_with(Path::new(p)))
    {
        return Trust::AlreadyTrusted(folder);
    }
    paths.push(Value::String(folder.to_string_lossy().into_owned()));
    match write_atomically(file, &doc) {
        Ok(()) => Trust::Added(folder),
        Err(e) => Trust::Skipped(format!("can't write {}: {e}", file.display())),
    }
}

/// An object whose `trusted_paths`, if present, is an array.
fn well_formed(v: &Value) -> bool {
    match v.as_object() {
        Some(o) => o.get("trusted_paths").is_none_or(Value::is_array),
        None => false,
    }
}

fn write_atomically(file: &Path, doc: &Value) -> std::io::Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut text = serde_json::to_string_pretty(doc).map_err(std::io::Error::other)?;
    text.push('\n');
    let tmp = file.with_extension(format!("json.chaos-{}.tmp", std::process::id()));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, file).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh, empty temp dir unique to `name` and this process.
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("chaos-devin-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The trust file under `data` (an XDG_DATA_HOME override, passed in
    /// explicitly so tests never touch the process environment).
    fn trust_file(data: &Path) -> PathBuf {
        trust_file_from(Some(data.as_os_str().to_owned()), None).unwrap()
    }

    fn trusted(file: &Path) -> Vec<String> {
        let v: Value = serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
        v["trusted_paths"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p.as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn data_dir_is_xdg_else_home_local_share() {
        assert_eq!(
            trust_file_from(Some("/data".into()), Some("/home/m".into())),
            Some(PathBuf::from("/data/devin/cli/trusted_workspaces.json"))
        );
        // Unset, empty or relative XDG_DATA_HOME falls back to HOME.
        for xdg in [None, Some(""), Some("rel/dir")] {
            assert_eq!(
                trust_file_from(xdg.map(OsString::from), Some("/home/m".into())),
                Some(PathBuf::from(
                    "/home/m/.local/share/devin/cli/trusted_workspaces.json"
                ))
            );
        }
        assert_eq!(trust_file_from(None, None), None);
    }

    #[test]
    fn detects_devin_by_binary_name() {
        assert!(is_devin(Path::new("devin")));
        assert!(is_devin(Path::new("/home/m/.devin/bin/devin")));
        assert!(!is_devin(Path::new("/usr/bin/opencode")));
        assert!(!is_devin(Path::new("/usr/bin/devin-helper")));
    }

    #[test]
    fn adds_a_new_path_creating_the_file() {
        let root = temp_dir("adds");
        let work = root.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let file = trust_file(&root.join("data"));
        let canon = std::fs::canonicalize(&work).unwrap();
        assert_eq!(ensure_trusted_in(&file, &work), Trust::Added(canon.clone()));
        assert_eq!(trusted(&file), vec![canon.to_string_lossy().into_owned()]);
        // A second call is a no-op.
        assert_eq!(
            ensure_trusted_in(&file, &work),
            Trust::AlreadyTrusted(canon.clone())
        );
        assert_eq!(trusted(&file).len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn keeps_existing_entries_and_keys() {
        let root = temp_dir("keeps");
        let work = root.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let file = trust_file(&root.join("data"));
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(
            &file,
            r#"{"trusted_paths": ["/somewhere/else"], "other": 1}"#,
        )
        .unwrap();
        let canon = std::fs::canonicalize(&work).unwrap();
        assert_eq!(ensure_trusted_in(&file, &work), Trust::Added(canon.clone()));
        assert_eq!(
            trusted(&file),
            vec![
                "/somewhere/else".to_string(),
                canon.to_string_lossy().into_owned()
            ]
        );
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(v["other"], 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn skips_when_a_parent_is_trusted() {
        let root = temp_dir("parent");
        let work = root.join("work").join("sub");
        std::fs::create_dir_all(&work).unwrap();
        let parent = std::fs::canonicalize(root.join("work")).unwrap();
        let file = trust_file(&root.join("data"));
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let before = serde_json::json!({ "trusted_paths": [parent] }).to_string();
        std::fs::write(&file, &before).unwrap();
        assert!(matches!(
            ensure_trusted_in(&file, &work),
            Trust::AlreadyTrusted(_)
        ));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn leaves_malformed_json_alone() {
        let root = temp_dir("malformed");
        let work = root.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let file = trust_file(&root.join("data"));
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        for bad in ["{not json", "[1, 2]", r#"{"trusted_paths": "nope"}"#] {
            std::fs::write(&file, bad).unwrap();
            assert!(matches!(ensure_trusted_in(&file, &work), Trust::Skipped(_)));
            assert_eq!(std::fs::read_to_string(&file).unwrap(), bad);
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_folder_is_skipped() {
        let root = temp_dir("missing");
        let file = trust_file(&root.join("data"));
        assert!(matches!(
            ensure_trusted_in(&file, &root.join("nope")),
            Trust::Skipped(_)
        ));
        assert!(!file.exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
