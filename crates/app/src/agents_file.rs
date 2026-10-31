//! Writing the roster back to `agents.json` (Hxmbl's decision, 2026-10-31).
//!
//! `agents.json`'s `members` list is the roster loaded at every start. Adding
//! or removing a model in Agent Status rewrites that list straight away, so
//! what the user did in the app is what the next start loads. The file is
//! created in the data directory when there is none.
//!
//! Only `members` changes. Every other key, and every other field of an
//! existing member entry (`cwd`, `runs_in`, anything unknown), is kept, in
//! its order; the file is re-indented with two spaces. A file that isn't
//! valid JSON is never overwritten: the change is refused and reported, so a
//! typo the user is in the middle of fixing can't cost them the file. Writes
//! are atomic (a temporary file renamed over the old one) and one at a time.

use std::{
    io,
    path::Path,
    sync::{Arc, Mutex},
};

use serde_json::{Map, Value, json};

/// One roster entry as `agents.json` writes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub harness: String,
    /// The harness's model id; `None` runs its default.
    pub model: Option<String>,
    pub name: String,
}

impl Entry {
    pub fn new(harness: &str, model: Option<&str>, name: &str) -> Self {
        Entry {
            harness: harness.to_string(),
            model: model.map(str::to_string),
            name: name.to_string(),
        }
    }

    fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("harness".into(), json!(self.harness));
        if let Some(model) = &self.model {
            m.insert("model".into(), json!(model));
        }
        m.insert("name".into(), json!(self.name));
        Value::Object(m)
    }
}

/// Read-modify-write of the file happens one at a time.
static LOCK: Mutex<()> = Mutex::new(());

/// Whether a member entry in the file is `harness` running `model` (a
/// missing or `null` model is the harness default).
fn is(entry: &Value, harness: &str, model: Option<&str>) -> bool {
    entry.get("harness").and_then(Value::as_str) == Some(harness)
        && entry.get("model").and_then(Value::as_str) == model
}

/// Applies `change` to the file's `members` array (created if missing) and
/// writes the file back. A missing file starts as `{}`.
fn edit(path: &Path, change: impl FnOnce(&mut Vec<Value>)) -> io::Result<()> {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut root = match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str::<Value>(&text).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} is not valid JSON ({e}); left as it is", path.display()),
            )
        })?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Value::Object(Map::new()),
        Err(e) => return Err(e),
    };
    let Some(obj) = root.as_object_mut() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not a JSON object; left as it is", path.display()),
        ));
    };
    let mut members = match obj.remove("members") {
        Some(Value::Array(list)) => list,
        _ => Vec::new(),
    };
    change(&mut members);
    obj.insert("members".into(), Value::Array(members));
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut text = serde_json::to_string_pretty(&root).map_err(io::Error::other)?;
    text.push('\n');
    chaos_core::model_cache::write_atomic(path, text.as_bytes())
}

/// Replaces the whole `members` list (a fresh project's seeded roster, or the
/// roster carried over from the store). Entries already in the file that are
/// in `list` keep their extra fields.
pub fn set_members(path: &Path, list: &[Entry]) -> io::Result<()> {
    edit(path, |members| {
        let old = std::mem::take(members);
        for e in list {
            let kept = old
                .iter()
                .find(|v| is(v, &e.harness, e.model.as_deref()))
                .cloned();
            members.push(kept.unwrap_or_else(|| e.to_json()));
        }
    })
}

/// Adds `entry` to `members` unless it is already there.
pub fn add_member(path: &Path, entry: &Entry) -> io::Result<()> {
    edit(path, |members| {
        if !members
            .iter()
            .any(|v| is(v, &entry.harness, entry.model.as_deref()))
        {
            members.push(entry.to_json());
        }
    })
}

/// Takes `harness` running `model` out of `members`.
pub fn remove_member(path: &Path, harness: &str, model: Option<&str>) -> io::Result<()> {
    edit(path, |members| members.retain(|v| !is(v, harness, model)))
}

/// Runs a write off the UI thread; a failure is printed (the roster change
/// itself already happened in core).
pub fn in_background(path: Arc<Path>, write: impl FnOnce(&Path) -> io::Result<()> + Send + 'static) {
    std::thread::spawn(move || {
        if let Err(e) = write(&path) {
            eprintln!(
                "chaos-control: could not update the members in {}: {e}",
                path.display()
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "chaos-agents-file-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("agents.json")
    }

    fn read(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn a_missing_file_is_created_with_the_members() {
        let path = temp_file("create");
        add_member(&path, &Entry::new("codex", None, "Codex default")).unwrap();
        add_member(&path, &Entry::new("fx", Some("big-2"), "Big Two")).unwrap();
        // Adding the same member twice keeps one.
        add_member(&path, &Entry::new("fx", Some("big-2"), "Big Two")).unwrap();
        assert_eq!(
            read(&path),
            json!({"members": [
                {"harness": "codex", "name": "Codex default"},
                {"harness": "fx", "model": "big-2", "name": "Big Two"}
            ]})
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn other_keys_and_member_fields_are_kept_in_order() {
        let path = temp_file("keep");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"human": {"handle": "hx", "name": "Hx"},
                "members": [
                  {"harness": "fx", "model": "a", "runs_in": "vm", "cwd": "/w"},
                  {"harness": "codex", "model": null}
                ],
                "detect": true}"#,
        )
        .unwrap();
        remove_member(&path, "codex", None).unwrap();
        add_member(&path, &Entry::new("vibe", Some("devstral"), "Devstral")).unwrap();
        let v = read(&path);
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, ["human", "detect", "members"]);
        assert_eq!(v["human"]["handle"], "hx");
        assert_eq!(
            v["members"],
            json!([
                {"harness": "fx", "model": "a", "runs_in": "vm", "cwd": "/w"},
                {"harness": "vibe", "model": "devstral", "name": "Devstral"}
            ])
        );
        // Replacing the list keeps an entry's own fields.
        set_members(
            &path,
            &[
                Entry::new("vibe", Some("devstral"), "Devstral"),
                Entry::new("fx", Some("a"), "A"),
            ],
        )
        .unwrap();
        assert_eq!(
            read(&path)["members"],
            json!([
                {"harness": "vibe", "model": "devstral", "name": "Devstral"},
                {"harness": "fx", "model": "a", "runs_in": "vm", "cwd": "/w"}
            ])
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_broken_file_is_never_overwritten() {
        let path = temp_file("broken");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{\"members\": [").unwrap();
        let err = add_member(&path, &Entry::new("fx", None, "fx default")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"members\": [");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
