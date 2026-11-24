//! Owner-only permissions for Chaos Control's local data.
//!
//! On Unix, directories are created with mode `0700` and files with `0600`.
//! Existing paths are tightened as well, so an install created before this
//! policy gets the same protection on upgrade. Windows uses ACLs and inherits
//! the user's access, so there is no portable mode-bit equivalent there: the
//! helpers are no-ops off Unix. See `docs/storage-security.md` for the threat
//! model and what this does not protect against.

use std::io;
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};

/// Create `dir` and missing parents with owner-only access on Unix.
///
/// `DirBuilder::mode` does not apply to components that already existed,
/// including `dir` itself, so the target is tightened explicitly. Parents are
/// deliberately left as they are: the app only owns its data directory, not
/// the user's home. Call this for each level the app owns.
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir)?;
    Ok(())
}

/// Tighten an existing file to owner-only access on Unix. No-op if missing.
pub fn restrict_file(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    if path.exists() {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Create an empty file with owner-only access on Unix, without truncating an
/// existing file. Used before handing a path to SQLite.
pub fn create_private_file(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(path)?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Write bytes to `path` through a private file: no world-readable window
/// during the write, even when the umask is permissive.
pub fn write_private_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    create_private_file(path)?;
    restrict_file(path)?;
    std::fs::write(path, bytes)
}

/// Tighten a SQLite database and its sidecars.
///
/// SQLite creates `-wal` and `-shm` with the same mode as the database file,
/// so tightening the database before the first write covers new sidecars;
/// leftovers from an earlier run are tightened here too. `-journal` covers
/// databases opened in rollback mode.
pub fn restrict_database(path: &Path) -> io::Result<()> {
    restrict_file(path)?;
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        let sidecar = std::path::PathBuf::from(name);
        if sidecar.exists() {
            restrict_file(&sidecar)?;
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("cc-perms-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        dir
    }

    #[test]
    fn private_dir_is_tightened_when_it_already_exists() {
        let dir = temp_dir("dir");
        assert_eq!(mode(&dir), 0o755);
        ensure_private_dir(&dir).unwrap();
        assert_eq!(mode(&dir), 0o700);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn private_dir_applies_to_new_nested_dirs() {
        let root = temp_dir("nested");
        let nested = root.join("Projects").join("abc");
        ensure_private_dir(&nested).unwrap();
        assert_eq!(mode(&root.join("Projects")), 0o700);
        assert_eq!(mode(&nested), 0o700);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn private_file_is_tightened_on_write() {
        let dir = temp_dir("file");
        let path = dir.join("data.json");
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_private_file(&path, b"new").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert_eq!(mode(&path), 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn database_sidecars_are_tightened() {
        let dir = temp_dir("db");
        let db = dir.join("core.db");
        let sidecars = ["-wal", "-shm", "-journal"];
        std::fs::write(&db, b"db").unwrap();
        for suffix in sidecars {
            std::fs::write(dir.join(format!("core.db{suffix}")), b"sidecar").unwrap();
        }
        for suffix in sidecars {
            let path = dir.join(format!("core.db{suffix}"));
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o644)).unwrap();
        restrict_database(&db).unwrap();
        assert_eq!(mode(&db), 0o600);
        for suffix in sidecars {
            assert_eq!(mode(&dir.join(format!("core.db{suffix}"))), 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
