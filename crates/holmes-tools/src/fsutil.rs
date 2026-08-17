//! Crash-safe filesystem primitives shared by the builtin file tools and the
//! runtime checkpoint hook (P1-05): atomic same-filesystem replacement with
//! fsync, content hashing, path normalization, and per-path write locks.
//!
//! Replacement metadata policy (P2-05):
//! - the existing target's Unix permission bits are carried onto the
//!   replacement (an `0755` executable stays executable, a `0600` secret stays
//!   private); brand-new files keep the process-umask default;
//! - ACLs and xattrs are NOT preserved — portable Rust has no xattr/ACL API
//!   and the tools never create files that rely on them; this is a documented
//!   limitation, not an oversight;
//! - a symlink at the target path is REFUSED (`InvalidInput`): the tools and
//!   the checkpoint hook identify files by their canonical (symlink-resolved)
//!   path, so silently replacing the link itself would split a file's
//!   identity from its mutation. Resolve the link and edit the real path.

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

/// Lowercase hex SHA-256 of `bytes`. Used for checkpoint manifests and for the
/// optimistic-concurrency `expected_hash` precondition of the file tools.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Best-effort absolute, symlink-resolved form of `path`. Used as the identity
/// of a file for checkpoint ids and lock keys, so checkpoint-time and
/// restore-time lookups must agree even when the file does not exist yet:
/// canonicalize the deepest existing ancestor and re-append the missing tail;
/// fall back to a plain absolute path when nothing canonicalizes.
pub fn normalize_path(path: &Path) -> PathBuf {
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    let mut cursor = Some(path);
    while let Some(p) = cursor {
        if let Ok(canon) = std::fs::canonicalize(p) {
            let mut out = canon;
            for comp in missing.iter().rev() {
                out.push(comp);
            }
            return out;
        }
        match p.file_name() {
            Some(name) => {
                missing.push(name.to_os_string());
                cursor = p.parent();
            }
            None => break,
        }
    }
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|dir| dir.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

fn path_locks() -> &'static Mutex<HashMap<PathBuf, Weak<tokio::sync::Mutex<()>>>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Weak<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    LOCKS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Per-path async write lock. All mutating file tools hold this around their
/// read-check-write sequence, so concurrent writers (parallel tool calls,
/// in-process subagents) serialize on the same file instead of interleaving.
/// Cross-agent lost updates are additionally caught by the `expected_hash`
/// precondition, which also protects against writers outside this process.
///
/// The registry stores weak references and prunes dead entries on every
/// acquisition (P2-05): once the last guard for a path is released, its entry
/// drops out on the next call, so a long-lived process does not grow the map
/// by every file it has ever touched.
pub async fn file_lock(path: &Path) -> tokio::sync::OwnedMutexGuard<()> {
    let key = normalize_path(path);
    let lock = {
        let mut map = path_locks()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        map.retain(|_, weak| weak.strong_count() > 0);
        match map.get(&key).and_then(Weak::upgrade) {
            Some(lock) => lock,
            None => {
                let lock = Arc::new(tokio::sync::Mutex::new(()));
                map.insert(key, Arc::downgrade(&lock));
                lock
            }
        }
    };
    lock.lock_owned().await
}

#[cfg(test)]
fn path_lock_contains(path: &Path) -> bool {
    path_locks()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .contains_key(&normalize_path(path))
}

fn temp_sibling(path: &Path) -> std::io::Result<(PathBuf, PathBuf)> {
    let file_name = path.file_name().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no file name")
    })?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let tmp = parent.join(format!(
        ".{}.holmes-tmp-{}",
        file_name.to_string_lossy(),
        uuid::Uuid::new_v4()
    ));
    Ok((parent, tmp))
}

/// P2-05: decide what the existing target means for an atomic replacement.
/// Returns the permissions to carry onto the replacement (regular files), or
/// `None` when there is nothing to preserve. A symlink at the target path is
/// REFUSED: checkpoint and lock identity are the canonical resolved path, so
/// replacing the link itself would split identity from mutation.
fn classify_existing_target(
    meta: &std::fs::Metadata,
    path: &Path,
) -> std::io::Result<Option<std::fs::Permissions>> {
    if meta.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "refusing to replace symlink '{}': resolve it and write the target path directly",
                path.display()
            ),
        ));
    }
    if meta.is_file() {
        return Ok(Some(meta.permissions()));
    }
    Ok(None)
}

/// Crash-safe file replacement: write the full content to a uniquely named
/// temp file on the *same filesystem* as the target, fsync it, atomically
/// rename over the target, then fsync the parent directory. A crash before
/// the rename leaves the original file byte-for-byte intact (plus a stray
/// `.holmes-tmp-*` file); a crash after it cannot leave a truncated target.
/// An existing regular file's permission bits are preserved (mode set and
/// fsynced before the rename); a symlink target is refused (P2-05).
pub async fn atomic_write(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let (parent, tmp) = temp_sibling(path)?;
    let result = async {
        let existing_perms = match tokio::fs::symlink_metadata(path).await {
            Ok(meta) => classify_existing_target(&meta, path)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let mut file = tokio::fs::File::create(&tmp).await?;
        file.write_all(contents).await?;
        if let Some(perms) = existing_perms {
            file.set_permissions(perms).await?;
        }
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&tmp, path).await?;
        tokio::fs::File::open(&parent).await?.sync_all().await?;
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    result
}

/// Synchronous variant of [`atomic_write`] for non-async callers (the agent
/// hook trait is synchronous).
pub fn atomic_write_sync(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let (parent, tmp) = temp_sibling(path)?;
    let result = (|| {
        let existing_perms = match std::fs::symlink_metadata(path) {
            Ok(meta) => classify_existing_target(&meta, path)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(contents)?;
        if let Some(perms) = existing_perms {
            file.set_permissions(perms)?;
        }
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)?;
        std::fs::File::open(&parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_hex_matches_known_vector() {
        // sha256("") and sha256("abc") from the standard test vectors.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn normalize_path_resolves_existing_and_missing_tails() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("real.txt");
        std::fs::write(&existing, "x").unwrap();
        assert_eq!(
            normalize_path(&existing),
            std::fs::canonicalize(&existing).unwrap()
        );
        // Missing file under an existing parent: parent canonicalizes, tail is appended.
        let missing = dir.path().join("sub").join("new.txt");
        let expected = std::fs::canonicalize(dir.path())
            .unwrap()
            .join("sub")
            .join("new.txt");
        assert_eq!(normalize_path(&missing), expected);
    }

    #[tokio::test]
    async fn atomic_write_replaces_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.txt");
        std::fs::write(&target, "old").unwrap();
        atomic_write(&target, b"new contents").await.unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new contents");
        // No temp files remain on success.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".holmes-tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "leftover temps: {leftovers:?}");
    }

    #[tokio::test]
    async fn stranded_temp_file_does_not_touch_the_original() {
        // Simulates a crash between temp-write and rename: the temp file exists
        // but the rename never happened, so the original must be intact.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.txt");
        std::fs::write(&target, "original").unwrap();
        std::fs::write(
            dir.path().join(".target.txt.holmes-tmp-deadbeef"),
            "partial",
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "original");
        // A subsequent atomic write still succeeds alongside the stranded temp.
        atomic_write(&target, b"recovered").await.unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "recovered");
    }

    #[tokio::test]
    async fn file_lock_serializes_same_path() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("shared.txt");
        let g1 = file_lock(&target).await;
        let target2 = target.clone();
        let attempt = tokio::spawn(async move {
            tokio::time::timeout(std::time::Duration::from_millis(100), file_lock(&target2)).await
        });
        // Second acquisition of the same normalized path must block while held.
        assert!(attempt.await.unwrap().is_err());
        drop(g1);
        // And succeed once released.
        let _g2 = tokio::time::timeout(std::time::Duration::from_millis(500), file_lock(&target))
            .await
            .expect("lock released");
    }

    #[test]
    fn atomic_write_sync_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("sync.txt");
        atomic_write_sync(&target, b"sync-bytes").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"sync-bytes");
    }

    /// P2-05: replacing a regular file preserves its permission bits.
    #[cfg(unix)]
    #[tokio::test]
    async fn atomic_write_preserves_executable_and_restrictive_modes() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();

        let exe = dir.path().join("tool.sh");
        std::fs::write(&exe, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        atomic_write(&exe, b"#!/bin/sh\necho hi\n").await.unwrap();
        assert_eq!(
            std::fs::metadata(&exe).unwrap().permissions().mode() & 0o777,
            0o755,
            "executable bit must survive an atomic edit"
        );

        let secret = dir.path().join("secret.txt");
        atomic_write_sync(&secret, b"v1").unwrap();
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
        atomic_write_sync(&secret, b"v2").unwrap();
        assert_eq!(std::fs::read_to_string(&secret).unwrap(), "v2");
        assert_eq!(
            std::fs::metadata(&secret).unwrap().permissions().mode() & 0o777,
            0o600,
            "restrictive mode must survive an atomic edit"
        );
    }

    /// P2-05: a symlink at the target path is refused — the link AND its
    /// target are both left untouched.
    #[cfg(unix)]
    #[tokio::test]
    async fn atomic_write_refuses_symlink_targets() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.txt");
        std::fs::write(&real, "real-contents").unwrap();
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let err = atomic_write(&link, b"rewritten").await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("symlink"), "got: {err}");
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "real-contents");
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the link itself must not be replaced"
        );

        let err = atomic_write_sync(&link, b"rewritten").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "real-contents");
    }

    /// P2-05: the per-path lock registry drops entries whose last guard was
    /// released instead of growing by every file ever touched. (The registry
    /// is process-global and shared with parallel tests, so assert on
    /// specific keys rather than a global count.)
    #[tokio::test]
    async fn file_lock_registry_prunes_released_paths() {
        let dir = tempfile::tempdir().unwrap();
        let mut transients = Vec::new();
        for i in 0..8 {
            let path = dir.path().join(format!("transient-{i}.txt"));
            let guard = file_lock(&path).await;
            drop(guard);
            transients.push(path);
        }
        // Acquiring a lock for a new path prunes the released entries first.
        let held_path = dir.path().join("held.txt");
        let _held = file_lock(&held_path).await;
        for path in &transients {
            assert!(
                !path_lock_contains(path),
                "released path must be pruned: {}",
                path.display()
            );
        }
        assert!(path_lock_contains(&held_path));
    }
}
