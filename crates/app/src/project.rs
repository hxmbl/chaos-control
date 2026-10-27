//! Which project the launch folder is.
//!
//! The folder Chaos Control is launched from is the project. Its identity
//! travels with the folder in `.chaos/project.toml`, which holds only a stable
//! UUID and a display name:
//!
//! ```toml
//! version = 1
//! id = "0b7e4f0e-2a51-4c4b-9a6e-3f1d2c3b4a59"
//! name = "starpoint"
//! ```
//!
//! Everything else (channels, messages, membership, cursors, agents) lives in
//! the app's data directory, keyed by that UUID (see "Storage model" in
//! `ARCHITECTURE.md`). The file is a hint, not a key: [`decide`] never lets a
//! changed or copied file quietly open another project's history. When the
//! file and what core remembers about the folder disagree, the app keeps what
//! it had and asks in the pane.
//!
//! `.chaos/` is added to the repository's `.git/info/exclude` (never
//! `.gitignore`), so nothing shows up in `git status`. Committing the file is
//! a later, explicit "Share project identity" action.

use std::{
    io::Write,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

pub const DIR: &str = ".chaos";
pub const FILE: &str = "project.toml";
pub const VERSION: u32 = 1;

/// The contents of `.chaos/project.toml`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectFile {
    pub version: u32,
    pub id: String,
    pub name: String,
}

/// What reading the file found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileState {
    Missing,
    /// There, but not something this version can use (bad TOML, a future
    /// `version`, an id that isn't a UUID). Left alone on disk.
    Unusable(String),
    Present(ProjectFile),
}

/// A project core already knows, as far as deciding needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Known {
    pub id: String,
    pub name: String,
    pub root: Option<String>,
}

/// Everything [`decide`] looks at, gathered by the caller.
#[derive(Clone, Debug)]
pub struct Inputs<'a> {
    pub file: &'a FileState,
    /// The project core last opened from this folder.
    pub by_root: Option<&'a Known>,
    /// The project the file's UUID names, if core knows it.
    pub by_file_id: Option<&'a Known>,
    /// For `by_file_id` with another root: whether that folder still exists
    /// and still names the same UUID (a copy or clone, not a move).
    pub other_root_still_claims_it: bool,
    /// This folder's name, the default display name.
    pub folder_name: &'a str,
    /// A freshly minted UUID, used if a new project is needed.
    pub fresh_id: &'a str,
    /// The project migration 2 made from pre-project history, if no folder
    /// has adopted it yet (`Core::unclaimed_import`).
    pub unclaimed_import: Option<&'a Known>,
    /// Whether `by_root` has no messages at all (the placeholder opened while
    /// an earlier import question went unanswered).
    pub by_root_is_empty: bool,
}

/// The project to open now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub id: String,
    pub name: String,
}

/// Why the user is being asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Question {
    /// A folder core knows now names a different UUID (`git pull`, a hand
    /// edit, a copied file). We kept the old project open.
    Changed,
    /// The file names a project core knows from another folder that still
    /// exists, so this is a copy rather than a move. We opened a separate
    /// project for this folder.
    CopiedFrom { root: String },
    /// A folder with no file and nothing (or only an empty placeholder) in
    /// core, while history from before projects is still unclaimed. A fresh
    /// project is open; the user may adopt the old history here instead.
    Imported,
}

/// A pending identity question, shown in the pane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Confirm {
    pub question: Question,
    /// What is open now.
    pub current: Target,
    /// What the file asks for.
    pub proposed: Target,
}

/// Which branch [`decide`] took. For tests and logs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Case {
    /// No usable file; core knows this folder: relink to that project.
    Relink,
    /// No usable file and an unknown folder: a new project.
    Create,
    /// The file and core agree.
    Match,
    /// Known folder, different UUID in the file: keep the old one and ask.
    Changed,
    /// Unknown folder, unknown UUID (a cloned repo that shared its file): a
    /// new local project under that UUID. Shared identity, not shared chat.
    Unknown,
    /// Unknown folder, a UUID core knows from a folder that is gone: the
    /// project moved here.
    Moved,
    /// Unknown folder, a UUID core knows from a folder that still exists and
    /// still claims it: a copy. A separate project opens and the user is asked.
    Copied,
    /// No file, an unknown folder (or one whose project is still empty) and
    /// unclaimed pre-project history: open fresh and offer the history.
    OfferImport,
}

/// The outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub case: Case,
    pub open: Target,
    /// Rewrite `.chaos/project.toml` to name `open`.
    pub write_file: bool,
    pub confirm: Option<Confirm>,
}

/// Decides which project this folder opens as. Pure, so every trust rule is
/// unit-tested: an identity change never silently grants another project's
/// history.
pub fn decide(i: &Inputs<'_>) -> Plan {
    let file = match i.file {
        FileState::Present(f) => Some(f),
        _ => None,
    };
    // Never overwrite a file we couldn't read: it may be newer than us.
    let may_write = !matches!(i.file, FileState::Unusable(_));
    let target = |k: &Known| Target {
        id: k.id.clone(),
        name: k.name.clone(),
    };
    // Only a missing file: an unreadable one can't record the answer (and is
    // never overwritten). Only an import with no folder: a live root is never
    // up for adoption.
    let import = i
        .unclaimed_import
        .filter(|k| k.root.is_none() && matches!(i.file, FileState::Missing));
    let Some(file) = file else {
        if let Some(import) = import {
            let current = match i.by_root {
                None => Target {
                    id: i.fresh_id.to_string(),
                    name: i.folder_name.to_string(),
                },
                Some(k) if i.by_root_is_empty && k.id != import.id => target(k),
                Some(_) => return relink_or_create(i, may_write),
            };
            return Plan {
                case: Case::OfferImport,
                open: current.clone(),
                // Not yet: the answer is what writes the file. "Start fresh"
                // writes it for `current`, so this folder never asks again.
                write_file: false,
                confirm: Some(Confirm {
                    question: Question::Imported,
                    current,
                    // Adopted, it takes the folder's name ("Imported" says
                    // nothing once it lives somewhere).
                    proposed: Target {
                        id: import.id.clone(),
                        name: i.folder_name.to_string(),
                    },
                }),
            };
        }
        return relink_or_create(i, may_write);
    };
    let from_file = Target {
        id: file.id.clone(),
        name: file.name.clone(),
    };
    match i.by_root {
        Some(known) if known.id == file.id => Plan {
            case: Case::Match,
            // The file carries the display name: renaming there renames here.
            open: from_file,
            write_file: false,
            confirm: None,
        },
        Some(known) => Plan {
            case: Case::Changed,
            open: target(known),
            write_file: false,
            confirm: Some(Confirm {
                question: Question::Changed,
                current: target(known),
                proposed: from_file,
            }),
        },
        None => match i.by_file_id {
            None => Plan {
                case: Case::Unknown,
                open: from_file,
                write_file: false,
                confirm: None,
            },
            Some(_) if !i.other_root_still_claims_it => Plan {
                case: Case::Moved,
                open: from_file,
                write_file: false,
                confirm: None,
            },
            Some(known) => {
                let separate = Target {
                    id: i.fresh_id.to_string(),
                    name: file.name.clone(),
                };
                Plan {
                    case: Case::Copied,
                    open: separate.clone(),
                    // Not yet: until the user answers, the file keeps naming
                    // the original. Next launch this folder is known (as the
                    // separate project), so it asks again as `Changed`.
                    write_file: false,
                    confirm: Some(Confirm {
                        question: Question::CopiedFrom {
                            root: known.root.clone().unwrap_or_default(),
                        },
                        current: separate,
                        proposed: from_file,
                    }),
                }
            }
        },
    }
}

/// No usable file and nothing to offer: relink to what core knows about the
/// folder, or start a new project named after it.
fn relink_or_create(i: &Inputs<'_>, may_write: bool) -> Plan {
    match i.by_root {
        Some(known) => Plan {
            case: Case::Relink,
            open: Target {
                id: known.id.clone(),
                name: known.name.clone(),
            },
            write_file: may_write,
            confirm: None,
        },
        None => Plan {
            case: Case::Create,
            open: Target {
                id: i.fresh_id.to_string(),
                name: i.folder_name.to_string(),
            },
            write_file: may_write,
            confirm: None,
        },
    }
}

/// The folder as core stores it: canonical when possible, so a symlinked
/// path finds the same project.
pub fn root_key(folder: &Path) -> String {
    std::fs::canonicalize(folder)
        .unwrap_or_else(|_| folder.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

pub fn folder_name(folder: &Path) -> String {
    std::fs::canonicalize(folder)
        .unwrap_or_else(|_| folder.to_path_buf())
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "Untitled".into())
}

/// A folder as the header shows it: under the home directory it starts
/// with `~` (`~/Projects/everything`).
pub fn display_path(folder: &Path, home: Option<&Path>) -> String {
    if let Some(home) = home
        && !home.as_os_str().is_empty()
        && let Ok(rest) = folder.strip_prefix(home)
    {
        return if rest.as_os_str().is_empty() {
            "~".into()
        } else {
            format!("~/{}", rest.display())
        };
    }
    folder.display().to_string()
}

pub fn file_path(folder: &Path) -> PathBuf {
    folder.join(DIR).join(FILE)
}

/// Reads `.chaos/project.toml`. `valid_id` is core's UUID check.
pub fn read(folder: &Path, valid_id: impl Fn(&str) -> Option<String>) -> FileState {
    let path = file_path(folder);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return FileState::Missing,
        Err(e) => return FileState::Unusable(format!("{}: {e}", path.display())),
    };
    parse(&text, valid_id)
}

pub fn parse(text: &str, valid_id: impl Fn(&str) -> Option<String>) -> FileState {
    let file: ProjectFile = match toml::from_str(text) {
        Ok(f) => f,
        Err(e) => return FileState::Unusable(format!("not valid: {e}")),
    };
    if file.version != VERSION {
        return FileState::Unusable(format!(
            "version {} (this build reads version {VERSION})",
            file.version
        ));
    }
    let Some(id) = valid_id(&file.id) else {
        return FileState::Unusable(format!("id {:?} is not a UUID", file.id));
    };
    let name = file.name.trim();
    FileState::Present(ProjectFile {
        version: VERSION,
        id,
        name: if name.is_empty() {
            "Untitled".into()
        } else {
            name.into()
        },
    })
}

pub fn render(target: &Target) -> String {
    let body = toml::to_string(&ProjectFile {
        version: VERSION,
        id: target.id.clone(),
        name: target.name.clone(),
    })
    .unwrap_or_default();
    format!(
        "# Chaos Control project identity. Only the id and display name live\n\
         # here; chat history stays in the app's data directory.\n{body}"
    )
}

/// Writes `.chaos/project.toml` for `target`.
pub fn write(folder: &Path, target: &Target) -> std::io::Result<()> {
    let dir = folder.join(DIR);
    std::fs::create_dir_all(&dir)?;
    // Write then rename, so a crash never leaves half a file.
    let tmp = dir.join(format!("{FILE}.tmp"));
    std::fs::write(&tmp, render(target))?;
    std::fs::rename(tmp, file_path(folder))
}

// ---------- git ----------

/// The directory git reads `info/exclude` from for the repository holding
/// `folder`, if any. Walks up to the repository root. A `.git` *file*
/// (worktrees, submodules) points at the real git dir with `gitdir: ...`; a
/// worktree's git dir has a `commondir` file naming the shared one, which is
/// where `info/exclude` lives.
pub fn git_common_dir(folder: &Path) -> Option<PathBuf> {
    let start = std::fs::canonicalize(folder).unwrap_or_else(|_| folder.to_path_buf());
    for dir in start.ancestors() {
        let dot_git = dir.join(".git");
        if dot_git.is_dir() {
            return Some(dot_git);
        }
        if dot_git.is_file() {
            let text = std::fs::read_to_string(&dot_git).ok()?;
            let gitdir = text.lines().find_map(|l| l.strip_prefix("gitdir:"))?.trim();
            let gitdir = dir.join(gitdir);
            let common = std::fs::read_to_string(gitdir.join("commondir"))
                .ok()
                .map(|c| gitdir.join(c.trim()));
            return Some(common.unwrap_or(gitdir));
        }
    }
    None
}

/// Whether an exclude file already ignores `.chaos/`.
fn excludes_chaos(text: &str) -> bool {
    text.lines()
        .map(str::trim)
        .any(|l| matches!(l, ".chaos" | ".chaos/" | "/.chaos" | "/.chaos/"))
}

/// Adds `.chaos/` to the repository's `info/exclude` once. Returns whether it
/// changed the file. Never touches `.gitignore`; does nothing outside a repo.
pub fn exclude_from_git(folder: &Path) -> std::io::Result<bool> {
    let Some(git) = git_common_dir(folder) else {
        return Ok(false);
    };
    let info = git.join("info");
    let path = info.join("exclude");
    let existing = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    if excludes_chaos(&existing) {
        return Ok(false);
    }
    std::fs::create_dir_all(&info)?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    let lead = if existing.is_empty() || existing.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    write!(
        f,
        "{lead}# Chaos Control project identity (local unless shared)\n.chaos/\n"
    )?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "0b7e4f0e-2a51-4c4b-9a6e-3f1d2c3b4a59";
    const B: &str = "5f0c9d1a-7b2e-4e8f-a1c3-9d8e7f6a5b4c";
    const FRESH: &str = "9a8b7c6d-5e4f-4a3b-8c2d-1e0f9a8b7c6d";

    fn known(id: &str, name: &str, root: &str) -> Known {
        Known {
            id: id.into(),
            name: name.into(),
            root: Some(root.into()),
        }
    }

    fn file(id: &str, name: &str) -> FileState {
        FileState::Present(ProjectFile {
            version: 1,
            id: id.into(),
            name: name.into(),
        })
    }

    fn inputs<'a>(
        file: &'a FileState,
        by_root: Option<&'a Known>,
        by_file_id: Option<&'a Known>,
    ) -> Inputs<'a> {
        Inputs {
            file,
            by_root,
            by_file_id,
            other_root_still_claims_it: false,
            folder_name: "starpoint",
            fresh_id: FRESH,
            unclaimed_import: None,
            by_root_is_empty: false,
        }
    }

    const IMPORTED: &str = "1c2d3e4f-5a6b-4c7d-8e9f-0a1b2c3d4e5f";

    fn imported() -> Known {
        Known {
            id: IMPORTED.into(),
            name: "Imported".into(),
            root: None,
        }
    }

    #[test]
    fn a_new_folder_is_offered_the_unclaimed_import() {
        let imp = imported();
        let mut i = inputs(&FileState::Missing, None, None);
        i.unclaimed_import = Some(&imp);
        let plan = decide(&i);
        assert_eq!(plan.case, Case::OfferImport);
        assert_eq!(plan.open.id, FRESH, "starts fresh until the user answers");
        assert!(!plan.write_file, "the answer writes the file");
        let c = plan.confirm.unwrap();
        assert_eq!(c.question, Question::Imported);
        assert_eq!(c.current.id, FRESH);
        assert_eq!(c.proposed.id, IMPORTED);
        assert_eq!(
            c.proposed.name, "starpoint",
            "adopted, it takes the folder's name"
        );
    }

    #[test]
    fn an_unanswered_offer_is_made_again_while_the_placeholder_is_empty() {
        let imp = imported();
        let placeholder = known(A, "starpoint", "/src/starpoint");
        let mut i = inputs(&FileState::Missing, Some(&placeholder), None);
        i.unclaimed_import = Some(&imp);
        i.by_root_is_empty = true;
        let plan = decide(&i);
        assert_eq!(plan.case, Case::OfferImport);
        assert_eq!(plan.open.id, A, "keeps the placeholder, no second project");
        assert_eq!(plan.confirm.unwrap().current.id, A);
        // Once something was said there, the folder is just known.
        i.by_root_is_empty = false;
        let plan = decide(&i);
        assert_eq!(plan.case, Case::Relink);
        assert!(plan.confirm.is_none());
    }

    #[test]
    fn the_import_is_never_offered_once_the_folder_has_a_file() {
        let imp = imported();
        // "Start fresh" wrote the file: it matches, so nothing is asked.
        let k = known(A, "starpoint", "/src/starpoint");
        let f = file(A, "starpoint");
        let mut i = inputs(&f, Some(&k), Some(&k));
        i.unclaimed_import = Some(&imp);
        i.by_root_is_empty = true;
        assert_eq!(decide(&i).case, Case::Match);
        assert!(decide(&i).confirm.is_none());
        // A cloned repo's file with an unknown UUID: its own project.
        let f = file(B, "everything");
        let mut i = inputs(&f, None, None);
        i.unclaimed_import = Some(&imp);
        assert_eq!(decide(&i).case, Case::Unknown);
        // An unreadable file can't record an answer: no offer.
        let f = FileState::Unusable("version 2".into());
        let mut i = inputs(&f, None, None);
        i.unclaimed_import = Some(&imp);
        assert_eq!(decide(&i).case, Case::Create);
    }

    #[test]
    fn a_project_with_a_live_root_is_never_offered() {
        let rooted = Known {
            root: Some("/src/elsewhere".into()),
            ..imported()
        };
        let mut i = inputs(&FileState::Missing, None, None);
        i.unclaimed_import = Some(&rooted);
        let plan = decide(&i);
        assert_eq!(plan.case, Case::Create);
        assert!(plan.confirm.is_none());
    }

    #[test]
    fn missing_file_in_an_unknown_folder_creates_a_project() {
        let plan = decide(&inputs(&FileState::Missing, None, None));
        assert_eq!(plan.case, Case::Create);
        assert_eq!(plan.open.id, FRESH);
        assert_eq!(plan.open.name, "starpoint");
        assert!(plan.write_file);
        assert!(plan.confirm.is_none());
    }

    #[test]
    fn missing_file_in_a_known_folder_relinks() {
        let k = known(A, "Starpoint", "/src/starpoint");
        let plan = decide(&inputs(&FileState::Missing, Some(&k), None));
        assert_eq!(plan.case, Case::Relink);
        assert_eq!(plan.open.id, A);
        assert_eq!(plan.open.name, "Starpoint");
        assert!(plan.write_file, "rewrites the file with the known UUID");
    }

    #[test]
    fn matching_file_opens_it_and_takes_its_name() {
        let k = known(A, "Old name", "/src/starpoint");
        let f = file(A, "New name");
        let plan = decide(&inputs(&f, Some(&k), Some(&k)));
        assert_eq!(plan.case, Case::Match);
        assert_eq!(plan.open.id, A);
        assert_eq!(plan.open.name, "New name", "renaming changes the name only");
        assert!(!plan.write_file);
        assert!(plan.confirm.is_none());
    }

    #[test]
    fn changed_uuid_in_a_known_folder_keeps_the_old_project_and_asks() {
        let k = known(A, "Starpoint", "/src/starpoint");
        let f = file(B, "Someone else's");
        let plan = decide(&inputs(&f, Some(&k), None));
        assert_eq!(plan.case, Case::Changed);
        assert_eq!(plan.open.id, A, "never switches silently");
        assert!(
            !plan.write_file,
            "the file is left as found until the user answers"
        );
        let c = plan.confirm.unwrap();
        assert_eq!(c.question, Question::Changed);
        assert_eq!(c.current.id, A);
        assert_eq!(c.proposed.id, B);
    }

    #[test]
    fn unknown_uuid_starts_a_local_project_under_it() {
        let f = file(B, "everything");
        let plan = decide(&inputs(&f, None, None));
        assert_eq!(plan.case, Case::Unknown);
        assert_eq!(plan.open.id, B);
        assert_eq!(plan.open.name, "everything");
        assert!(!plan.write_file);
        assert!(plan.confirm.is_none());
    }

    #[test]
    fn known_uuid_from_a_folder_that_is_gone_moved_here() {
        let k = known(B, "everything", "/old/everything");
        let f = file(B, "everything");
        let plan = decide(&inputs(&f, None, Some(&k)));
        assert_eq!(plan.case, Case::Moved);
        assert_eq!(plan.open.id, B);
    }

    #[test]
    fn known_uuid_from_a_folder_that_still_claims_it_is_a_copy() {
        let k = known(B, "everything", "/src/everything");
        let f = file(B, "everything");
        let mut i = inputs(&f, None, Some(&k));
        i.other_root_still_claims_it = true;
        let plan = decide(&i);
        assert_eq!(plan.case, Case::Copied);
        assert_eq!(
            plan.open.id, FRESH,
            "a copy never opens the original's history"
        );
        assert!(!plan.write_file);
        let c = plan.confirm.unwrap();
        assert_eq!(
            c.question,
            Question::CopiedFrom {
                root: "/src/everything".into()
            }
        );
        assert_eq!(c.proposed.id, B);
    }

    #[test]
    fn unusable_file_is_never_overwritten() {
        let f = FileState::Unusable("version 2".into());
        let plan = decide(&inputs(&f, None, None));
        assert_eq!(plan.case, Case::Create);
        assert!(!plan.write_file);
        let k = known(A, "Starpoint", "/src/starpoint");
        let plan = decide(&inputs(&f, Some(&k), None));
        assert_eq!(plan.case, Case::Relink);
        assert!(!plan.write_file);
    }

    #[test]
    fn paths_under_home_start_with_a_tilde() {
        let home = Path::new("/home/hx");
        assert_eq!(
            display_path(Path::new("/home/hx/Projects/everything"), Some(home)),
            "~/Projects/everything"
        );
        assert_eq!(display_path(Path::new("/home/hx"), Some(home)), "~");
        assert_eq!(
            display_path(Path::new("/home/hxx/a"), Some(home)),
            "/home/hxx/a"
        );
        assert_eq!(display_path(Path::new("/srv/x"), None), "/srv/x");
    }

    fn uuid_ok(s: &str) -> Option<String> {
        (s.len() == 36).then(|| s.to_ascii_lowercase())
    }

    #[test]
    fn parse_and_render_round_trip() {
        let t = Target {
            id: A.into(),
            name: "Star \"point\"".into(),
        };
        let text = render(&t);
        assert!(text.starts_with('#'));
        match parse(&text, uuid_ok) {
            FileState::Present(f) => {
                assert_eq!(f.id, A);
                assert_eq!(f.name, "Star \"point\"");
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            parse("version = 2\nid = \"x\"\nname = \"y\"", uuid_ok),
            FileState::Unusable(_)
        ));
        assert!(matches!(
            parse("version = 1\nid = \"nope\"\nname = \"y\"", uuid_ok),
            FileState::Unusable(_)
        ));
        assert!(matches!(
            parse("not toml [", uuid_ok),
            FileState::Unusable(_)
        ));
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cc-project-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn write_then_read() {
        let dir = temp("rw");
        assert_eq!(read(&dir, uuid_ok), FileState::Missing);
        let t = Target {
            id: A.into(),
            name: "starpoint".into(),
        };
        write(&dir, &t).unwrap();
        assert_eq!(
            read(&dir, uuid_ok),
            FileState::Present(ProjectFile {
                version: 1,
                id: A.into(),
                name: "starpoint".into()
            })
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn exclude_is_appended_once_and_gitignore_is_untouched() {
        let repo = temp("exclude");
        std::fs::create_dir_all(repo.join(".git/info")).unwrap();
        std::fs::write(
            repo.join(".git/info/exclude"),
            "# git ls-files --others\n*.log",
        )
        .unwrap();
        std::fs::write(repo.join(".gitignore"), "target/\n").unwrap();
        let sub = repo.join("crates/app");
        std::fs::create_dir_all(&sub).unwrap();
        assert!(exclude_from_git(&repo).unwrap());
        assert!(!exclude_from_git(&repo).unwrap(), "idempotent");
        assert!(
            !exclude_from_git(&sub).unwrap(),
            "found from a subfolder too"
        );
        let text = std::fs::read_to_string(repo.join(".git/info/exclude")).unwrap();
        assert_eq!(text.matches(".chaos/").count(), 1);
        assert!(text.starts_with("# git ls-files --others\n*.log\n"));
        assert_eq!(
            std::fs::read_to_string(repo.join(".gitignore")).unwrap(),
            "target/\n"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn exclude_creates_info_and_respects_an_existing_entry() {
        let repo = temp("exclude-new");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        assert!(exclude_from_git(&repo).unwrap());
        assert!(repo.join(".git/info/exclude").is_file());
        let other = temp("exclude-existing");
        std::fs::create_dir_all(other.join(".git/info")).unwrap();
        std::fs::write(other.join(".git/info/exclude"), "/.chaos\n").unwrap();
        assert!(!exclude_from_git(&other).unwrap());
        let none = temp("no-repo");
        assert!(!exclude_from_git(&none).unwrap());
        for d in [repo, other, none] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[test]
    fn worktrees_exclude_in_the_common_git_dir() {
        let main = temp("wt-main");
        std::fs::create_dir_all(main.join(".git/worktrees/feature")).unwrap();
        std::fs::write(main.join(".git/worktrees/feature/commondir"), "../..\n").unwrap();
        let wt = temp("wt-feature");
        std::fs::write(
            wt.join(".git"),
            format!(
                "gitdir: {}\n",
                main.join(".git/worktrees/feature").display()
            ),
        )
        .unwrap();
        assert!(exclude_from_git(&wt).unwrap());
        let text = std::fs::read_to_string(main.join(".git/info/exclude")).unwrap();
        assert!(text.contains(".chaos/"));
        assert!(!wt.join(".git/info").exists());
        for d in [main, wt] {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}
