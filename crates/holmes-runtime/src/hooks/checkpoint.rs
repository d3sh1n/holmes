//! Crash-safe, precisely addressable file checkpoints (P1-05).
//!
//! Every mutating file tool call is checkpointed before it runs. A checkpoint
//! is a pair of files under `<backup_dir>/`:
//!
//! - `payloads/<checkpoint-id>.bak` — the pre-write content (absent for a
//!   *tombstone* checkpoint, which records "this file did not exist"), written
//!   via same-filesystem temp + fsync + atomic rename + parent fsync.
//! - `manifests/<checkpoint-id>.json` — the [`CheckpointManifest`]: canonical
//!   path, pre/post content hashes and sizes, creating tool, session id.
//!
//! The checkpoint id is `<sha256(canonical_path)[..16]>-<epoch-nanos>-<uuid8>`,
//! so same-named files in different directories never collide and two writes
//! inside the same second (or nanosecond) never share an id. Restore matches
//! the manifest's canonical path exactly — no basename prefix guessing — and
//! a tombstone restore deletes the file the write created.

use holmes_core::hook::AgentHook;
use holmes_core::tool_types::{ToolCall, ToolResult};
use holmes_tools::fsutil::{atomic_write_sync, normalize_path, sha256_hex};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Newest checkpoints kept per canonical path.
const MAX_CHECKPOINTS_PER_PATH: usize = 10;
/// Total checkpoint payload bytes kept per session; oldest beyond the cap are pruned.
const MAX_TOTAL_PAYLOAD_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointManifest {
    pub version: u32,
    pub checkpoint_id: String,
    pub session_id: String,
    /// Mutating tool whose call triggered the checkpoint.
    pub reason: String,
    /// Target path exactly as the tool call passed it.
    pub original_path: String,
    /// Normalized absolute path; the exact-match restore key.
    pub canonical_path: String,
    /// False => tombstone: the target did not exist before the write.
    pub pre_existed: bool,
    pub pre_hash: Option<String>,
    pub pre_size: Option<u64>,
    /// Filled by `post_tool_use` once the mutation completed successfully.
    #[serde(default)]
    pub post_hash: Option<String>,
    #[serde(default)]
    pub post_size: Option<u64>,
    pub created_at_ms: u128,
    /// Payload file name relative to `payloads/`; `None` for tombstones.
    pub backup_file: Option<String>,
}

#[derive(Debug)]
pub struct CheckpointHook {
    pub backup_dir: PathBuf,
    session_id: String,
    /// Tool-call id → checkpoint id, recorded by `pre_tool_use` and consumed by
    /// `post_tool_use` to fill the manifest's post-write hash.
    pending: Mutex<HashMap<String, String>>,
}

impl CheckpointHook {
    /// Legacy fallback location (system temp) for callers without a session
    /// store; production runtimes go through [`CheckpointHook::for_session`].
    pub fn new(session_id: &str) -> Self {
        let backup_dir = std::env::temp_dir()
            .join("holmes_checkpoints")
            .join(session_id);
        Self::with_dir(backup_dir, session_id)
    }

    /// Checkpoints under the persistent per-session directory
    /// (`<sessions_dir>/<session-id>/checkpoints`) when the session store has
    /// one and the id is path-safe; falls back to [`CheckpointHook::new`].
    pub fn for_session(session_id: &str, sessions_dir: Option<&Path>) -> Self {
        let path_safe = !session_id.is_empty()
            && session_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        match (sessions_dir, path_safe) {
            (Some(dir), true) => {
                Self::with_dir(dir.join(session_id).join("checkpoints"), session_id)
            }
            _ => Self::new(session_id),
        }
    }

    pub fn with_dir(backup_dir: PathBuf, session_id: &str) -> Self {
        let _ = fs::create_dir_all(backup_dir.join("payloads"));
        let _ = fs::create_dir_all(backup_dir.join("manifests"));
        Self {
            backup_dir,
            session_id: session_id.to_string(),
            pending: Mutex::new(HashMap::new()),
        }
    }

    fn manifests_dir(&self) -> PathBuf {
        self.backup_dir.join("manifests")
    }

    fn payloads_dir(&self) -> PathBuf {
        self.backup_dir.join("payloads")
    }

    /// Checkpoint `target_path` before `reason` mutates it. Returns the new
    /// checkpoint id. Never fails the tool call over housekeeping: a payload
    /// or manifest write error aborts the mutation (fail-closed, as before),
    /// but pruning problems are only logged.
    fn checkpoint(&self, target_path: &Path, reason: &str) -> Result<String, String> {
        let canonical = normalize_path(target_path);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let path_key = &sha256_hex(canonical.to_string_lossy().as_bytes())[..16];
        let uuid = uuid::Uuid::new_v4().simple().to_string();
        let checkpoint_id = format!("{}-{}-{}", path_key, now.as_nanos(), &uuid[..8]);

        let existed = target_path.is_file();
        let (pre_hash, pre_size, backup_file) = if existed {
            let bytes = fs::read(target_path).map_err(|e| {
                format!(
                    "failed to read {} for checkpoint: {e}",
                    target_path.display()
                )
            })?;
            let file_name = format!("{checkpoint_id}.bak");
            let payload = self.payloads_dir().join(&file_name);
            atomic_write_sync(&payload, &bytes).map_err(|e| {
                format!(
                    "failed to write checkpoint payload {}: {e}",
                    payload.display()
                )
            })?;
            (
                Some(sha256_hex(&bytes)),
                Some(bytes.len() as u64),
                Some(file_name),
            )
        } else {
            (None, None, None) // tombstone: restore deletes the created file
        };

        let manifest = CheckpointManifest {
            version: 1,
            checkpoint_id: checkpoint_id.clone(),
            session_id: self.session_id.clone(),
            reason: reason.to_string(),
            original_path: target_path.display().to_string(),
            canonical_path: canonical.display().to_string(),
            pre_existed: existed,
            pre_hash,
            pre_size,
            post_hash: None,
            post_size: None,
            created_at_ms: now.as_millis(),
            backup_file,
        };
        write_manifest(&self.manifests_dir(), &manifest)?;

        holmes_core::metrics::metrics().count("checkpoint.created");
        tracing::info!(
            event = "CheckpointCreated",
            path = %target_path.display(),
            checkpoint_id = %checkpoint_id,
            tombstone = !existed,
            "file checkpoint created before mutation"
        );
        if let Err(e) = prune_checkpoints(
            &self.manifests_dir(),
            &self.payloads_dir(),
            MAX_CHECKPOINTS_PER_PATH,
            MAX_TOTAL_PAYLOAD_BYTES,
        ) {
            tracing::warn!(
                event = "CheckpointPruneFailed",
                error = %e,
                "failed to prune old checkpoints"
            );
        }
        Ok(checkpoint_id)
    }
}

fn write_manifest(manifests_dir: &Path, manifest: &CheckpointManifest) -> Result<(), String> {
    let json = serde_json::to_vec_pretty(manifest)
        .map_err(|e| format!("failed to serialize checkpoint manifest: {e}"))?;
    atomic_write_sync(
        &manifests_dir.join(format!("{}.json", manifest.checkpoint_id)),
        &json,
    )
    .map_err(|e| format!("failed to write checkpoint manifest: {e}"))
}

fn load_manifests(manifests_dir: &Path) -> Vec<CheckpointManifest> {
    let Ok(entries) = fs::read_dir(manifests_dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".json"))
        .filter_map(|entry| {
            let bytes = fs::read(entry.path()).ok()?;
            serde_json::from_slice(&bytes).ok()
        })
        .collect()
}

fn delete_checkpoint(manifests_dir: &Path, payloads_dir: &Path, manifest: &CheckpointManifest) {
    let _ = fs::remove_file(manifests_dir.join(format!("{}.json", manifest.checkpoint_id)));
    if let Some(backup_file) = &manifest.backup_file {
        let _ = fs::remove_file(payloads_dir.join(backup_file));
    }
}

/// Retention: keep at most `per_path` checkpoints per canonical path and at
/// most `max_payload_bytes` of payloads overall; oldest lose. Exposed with
/// explicit limits so tests can exercise it without creating 10+ checkpoints.
pub fn prune_checkpoints(
    manifests_dir: &Path,
    payloads_dir: &Path,
    per_path: usize,
    max_payload_bytes: u64,
) -> Result<(), String> {
    let mut manifests = load_manifests(manifests_dir);
    manifests.sort_by_key(|m| (m.created_at_ms, m.checkpoint_id.clone()));

    // Per-path cap: delete everything older than the newest `per_path`.
    let mut by_path: HashMap<String, Vec<&CheckpointManifest>> = HashMap::new();
    for m in &manifests {
        by_path.entry(m.canonical_path.clone()).or_default().push(m);
    }
    let mut pruned: Vec<String> = Vec::new();
    for group in by_path.values() {
        for m in group.iter().take(group.len().saturating_sub(per_path)) {
            delete_checkpoint(manifests_dir, payloads_dir, m);
            pruned.push(m.checkpoint_id.clone());
        }
    }

    // Global payload cap: delete oldest remaining until under the cap.
    manifests.retain(|m| !pruned.contains(&m.checkpoint_id));
    let mut total: u64 = manifests.iter().map(|m| m.pre_size.unwrap_or(0)).sum();
    for m in &manifests {
        if total <= max_payload_bytes {
            break;
        }
        total = total.saturating_sub(m.pre_size.unwrap_or(0));
        delete_checkpoint(manifests_dir, payloads_dir, m);
    }
    Ok(())
}

impl AgentHook for CheckpointHook {
    fn pre_tool_use(&self, call: &ToolCall) -> Result<(), String> {
        // File-mutating tools whose target must be backed up before the write.
        let is_modifying_tool = matches!(
            call.function.name.as_str(),
            "write_file"
                | "edit_file"
                // Legacy aliases kept for older sessions/prompts.
                | "replace_file_content"
                | "multi_replace_file_content"
                | "write_to_file"
        );

        if !is_modifying_tool {
            return Ok(());
        }

        // Try to extract the target file path from arguments. The builtin file tools
        // (write_file / edit_file) use `path`; `TargetFile` is kept as a fallback for
        // legacy callers.
        let args: serde_json::Value = match serde_json::from_str(&call.function.arguments) {
            Ok(val) => val,
            Err(_) => return Ok(()),
        };

        let target_file_str = args
            .get("path")
            .or_else(|| args.get("TargetFile"))
            .and_then(|v| v.as_str());
        let Some(target_file_str) = target_file_str else {
            return Ok(()); // Some tools might not have this exact parameter name
        };

        let target_path = Path::new(target_file_str);
        let checkpoint_id = self.checkpoint(target_path, &call.function.name)?;
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(call.id.clone(), checkpoint_id);
        Ok(())
    }

    fn post_tool_use(&self, call: &ToolCall, result: &ToolResult) -> Result<(), String> {
        let checkpoint_id = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&call.id);
        let Some(checkpoint_id) = checkpoint_id else {
            return Ok(());
        };
        if result.is_error {
            return Ok(()); // mutation did not happen; post hash stays empty
        }
        let manifests_dir = self.manifests_dir();
        let manifest_path = manifests_dir.join(format!("{checkpoint_id}.json"));
        let Ok(bytes) = fs::read(&manifest_path) else {
            return Ok(()); // pruned already, or never written — nothing to update
        };
        let Ok(mut manifest) = serde_json::from_slice::<CheckpointManifest>(&bytes) else {
            return Ok(());
        };
        let target = Path::new(&manifest.canonical_path);
        if let Ok(post) = fs::read(target) {
            manifest.post_hash = Some(sha256_hex(&post));
            manifest.post_size = Some(post.len() as u64);
            if let Err(e) = write_manifest(&manifests_dir, &manifest) {
                tracing::warn!(
                    event = "CheckpointManifestUpdateFailed",
                    checkpoint_id = %checkpoint_id,
                    error = %e,
                    "failed to record post-write hash"
                );
            }
        }
        Ok(())
    }
}

/// What a restore did to the target file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreAction {
    /// Pre-write content copied back over the target from this payload.
    Restored { backup: PathBuf },
    /// Tombstone checkpoint: the target did not exist before the write, so the
    /// file the mutation created was deleted.
    DeletedNewFile,
}

/// Restore `target_path` from its newest checkpoint in `backup_dir` (AGT-004
/// operator path). Emits `CheckpointRestored` / `CheckpointFailed`. Matching is
/// exact on the manifest's canonical path — no basename prefix guessing, so
/// `foo` can never pick up a checkpoint of `foobar` — and the payload's hash
/// is verified against the manifest before it is written back. This is the
/// programmatic form of the manual runbook step; the runtime never restores
/// automatically — a restore is always an operator decision.
pub fn restore_latest_checkpoint(
    backup_dir: &Path,
    target_path: &Path,
) -> Result<RestoreAction, String> {
    let manifests_dir = backup_dir.join("manifests");
    let payloads_dir = backup_dir.join("payloads");
    let canonical = normalize_path(target_path);
    let canonical_str = canonical.display().to_string();

    let newest = load_manifests(&manifests_dir)
        .into_iter()
        .filter(|m| m.canonical_path == canonical_str)
        .max_by_key(|m| (m.created_at_ms, m.checkpoint_id.clone()));

    let Some(manifest) = newest else {
        let message = format!(
            "no checkpoint for {} found in {}",
            target_path.display(),
            backup_dir.display()
        );
        tracing::warn!(
            event = "CheckpointFailed",
            path = %target_path.display(),
            reason = "no checkpoint found",
            "{message}"
        );
        return Err(message);
    };

    if !manifest.pre_existed {
        // Tombstone: the file did not exist before the checkpointed write, so
        // restoring means precisely deleting what that write created.
        if target_path.exists() {
            fs::remove_file(target_path).map_err(|e| {
                format!(
                    "failed to delete {} (tombstone restore of {}): {e}",
                    target_path.display(),
                    manifest.checkpoint_id
                )
            })?;
        }
        holmes_core::metrics::metrics().count("checkpoint.restored");
        tracing::info!(
            event = "CheckpointRestored",
            path = %target_path.display(),
            checkpoint_id = %manifest.checkpoint_id,
            action = "delete-new-file",
            "tombstone checkpoint restored by deleting the created file"
        );
        return Ok(RestoreAction::DeletedNewFile);
    }

    let backup_file = manifest.backup_file.clone().ok_or_else(|| {
        format!(
            "checkpoint {} is missing its payload reference",
            manifest.checkpoint_id
        )
    })?;
    let backup = payloads_dir.join(&backup_file);
    let bytes = fs::read(&backup).map_err(|e| {
        format!(
            "failed to read checkpoint payload {}: {e}",
            backup.display()
        )
    })?;
    if let Some(expected) = &manifest.pre_hash {
        let actual = sha256_hex(&bytes);
        if &actual != expected {
            let message = format!(
                "checkpoint payload {} is corrupt (expected sha256 {expected}, found {actual})",
                backup.display()
            );
            tracing::warn!(
                event = "CheckpointFailed",
                path = %target_path.display(),
                checkpoint_id = %manifest.checkpoint_id,
                reason = "payload hash mismatch",
                "{message}"
            );
            return Err(message);
        }
    }

    atomic_write_sync(target_path, &bytes).map_err(|e| {
        format!(
            "failed to restore {} from {}: {e}",
            target_path.display(),
            backup.display()
        )
    })?;
    holmes_core::metrics::metrics().count("checkpoint.restored");
    tracing::info!(
        event = "CheckpointRestored",
        path = %target_path.display(),
        checkpoint_id = %manifest.checkpoint_id,
        backup = %backup.display(),
        "file restored from checkpoint"
    );
    Ok(RestoreAction::Restored { backup })
}

#[cfg(test)]
mod tests {
    use super::*;
    use holmes_core::tool_types::{FunctionCall, ToolCall};

    fn call(name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "call-1".into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: name.into(),
                arguments: args.to_string(),
            },
        }
    }

    fn manifests(hook: &CheckpointHook) -> Vec<CheckpointManifest> {
        load_manifests(&hook.manifests_dir())
    }

    fn hook_in(dir: &Path) -> CheckpointHook {
        CheckpointHook::with_dir(dir.join("backups"), "test-session")
    }

    #[test]
    fn write_file_call_creates_restorable_checkpoint_with_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.txt");
        fs::write(&target, "original contents").unwrap();

        let hook = hook_in(dir.path());
        hook.pre_tool_use(&call(
            "write_file",
            serde_json::json!({ "path": target.to_str().unwrap(), "content": "new" }),
        ))
        .expect("checkpoint");

        let manifest = manifests(&hook).into_iter().next().expect("one manifest");
        assert!(manifest.pre_existed);
        assert_eq!(
            manifest.pre_hash.as_deref(),
            Some(sha256_hex(b"original contents").as_str())
        );
        assert_eq!(manifest.pre_size, Some(17));
        assert_eq!(manifest.reason, "write_file");
        assert_eq!(manifest.session_id, "test-session");
        assert_eq!(
            manifest.canonical_path,
            normalize_path(&target).display().to_string()
        );
        assert!(manifest
            .checkpoint_id
            .starts_with(&sha256_hex(normalize_path(&target).to_string_lossy().as_bytes())[..16]));
        let payload = hook
            .payloads_dir()
            .join(manifest.backup_file.as_ref().unwrap());
        assert_eq!(fs::read_to_string(&payload).unwrap(), "original contents");

        // Post-write hash is filled in by post_tool_use after a successful call.
        fs::write(&target, "new").unwrap();
        hook.post_tool_use(
            &call("write_file", serde_json::json!({})),
            &ToolResult::success("call-1", "write_file", "ok"),
        )
        .unwrap();
        let manifest = manifests(&hook).into_iter().next().unwrap();
        assert_eq!(
            manifest.post_hash.as_deref(),
            Some(sha256_hex(b"new").as_str())
        );

        // Restore brings the original back via exact manifest match.
        let action = restore_latest_checkpoint(&hook.backup_dir, &target).expect("restore");
        assert!(matches!(action, RestoreAction::Restored { .. }));
        assert_eq!(fs::read_to_string(&target).unwrap(), "original contents");
    }

    #[test]
    fn same_named_files_in_different_dirs_do_not_collide() {
        let dir = tempfile::tempdir().unwrap();
        let dir_a = dir.path().join("a");
        let dir_b = dir.path().join("b");
        fs::create_dir_all(&dir_a).unwrap();
        fs::create_dir_all(&dir_b).unwrap();
        let target_a = dir_a.join("same.txt");
        let target_b = dir_b.join("same.txt");
        fs::write(&target_a, "contents of a").unwrap();
        fs::write(&target_b, "contents of b").unwrap();

        let hook = hook_in(dir.path());
        for target in [&target_a, &target_b] {
            hook.pre_tool_use(&call(
                "write_file",
                serde_json::json!({ "path": target.to_str().unwrap(), "content": "x" }),
            ))
            .expect("checkpoint");
        }
        assert_eq!(manifests(&hook).len(), 2);

        fs::write(&target_a, "clobbered").unwrap();
        fs::write(&target_b, "clobbered").unwrap();
        restore_latest_checkpoint(&hook.backup_dir, &target_a).expect("restore a");
        restore_latest_checkpoint(&hook.backup_dir, &target_b).expect("restore b");
        assert_eq!(fs::read_to_string(&target_a).unwrap(), "contents of a");
        assert_eq!(fs::read_to_string(&target_b).unwrap(), "contents of b");
    }

    #[test]
    fn foo_restore_never_picks_foobar_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let foo = dir.path().join("foo");
        let foobar = dir.path().join("foobar");
        fs::write(&foobar, "longer name contents").unwrap();

        let hook = hook_in(dir.path());
        hook.pre_tool_use(&call(
            "write_file",
            serde_json::json!({ "path": foobar.to_str().unwrap(), "content": "x" }),
        ))
        .expect("checkpoint");

        // Restoring `foo` must fail loudly — the only checkpoint is `foobar`'s.
        let err = restore_latest_checkpoint(&hook.backup_dir, &foo).unwrap_err();
        assert!(err.contains("no checkpoint"), "got: {err}");
        assert!(!foo.exists());

        // And `foobar` restores its own content.
        fs::write(&foobar, "clobbered").unwrap();
        restore_latest_checkpoint(&hook.backup_dir, &foobar).expect("restore foobar");
        assert_eq!(fs::read_to_string(&foobar).unwrap(), "longer name contents");
    }

    #[test]
    fn new_file_tombstone_restores_by_precise_delete() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("fresh.txt");
        let hook = hook_in(dir.path());

        // Checkpoint a write to a file that does not exist yet -> tombstone.
        hook.pre_tool_use(&call(
            "write_file",
            serde_json::json!({ "path": target.to_str().unwrap(), "content": "created" }),
        ))
        .expect("checkpoint");
        let manifest = manifests(&hook).into_iter().next().expect("manifest");
        assert!(!manifest.pre_existed);
        assert!(manifest.pre_hash.is_none());
        assert!(manifest.backup_file.is_none());
        assert!(fs::read_dir(hook.payloads_dir()).unwrap().next().is_none());

        // The write then creates the file; restore deletes exactly it.
        fs::write(&target, "created").unwrap();
        let action = restore_latest_checkpoint(&hook.backup_dir, &target).expect("restore");
        assert_eq!(action, RestoreAction::DeletedNewFile);
        assert!(!target.exists());

        // Restoring again is a no-op delete, not an error.
        let action = restore_latest_checkpoint(&hook.backup_dir, &target).expect("restore");
        assert_eq!(action, RestoreAction::DeletedNewFile);
    }

    #[test]
    fn restore_refuses_a_corrupt_payload() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("data.txt");
        fs::write(&target, "good").unwrap();
        let hook = hook_in(dir.path());
        hook.pre_tool_use(&call(
            "write_file",
            serde_json::json!({ "path": target.to_str().unwrap(), "content": "x" }),
        ))
        .expect("checkpoint");

        let manifest = manifests(&hook).into_iter().next().unwrap();
        let payload = hook.payloads_dir().join(manifest.backup_file.unwrap());
        fs::write(&payload, "tampered").unwrap();

        fs::write(&target, "clobbered").unwrap();
        let err = restore_latest_checkpoint(&hook.backup_dir, &target).unwrap_err();
        assert!(err.contains("corrupt"), "got: {err}");
        // Target untouched on integrity failure.
        assert_eq!(fs::read_to_string(&target).unwrap(), "clobbered");
    }

    #[test]
    fn prune_enforces_per_path_and_total_caps() {
        let dir = tempfile::tempdir().unwrap();
        let manifests_dir = dir.path().join("manifests");
        let payloads_dir = dir.path().join("payloads");
        fs::create_dir_all(&manifests_dir).unwrap();
        fs::create_dir_all(&payloads_dir).unwrap();

        // 5 checkpoints of one path, 2 of another, 1-byte payloads.
        for i in 0..5u128 {
            let id = format!("aaa-{i:03}");
            atomic_write_sync(&payloads_dir.join(format!("{id}.bak")), b"x").unwrap();
            let m = CheckpointManifest {
                version: 1,
                checkpoint_id: id.clone(),
                session_id: "s".into(),
                reason: "write_file".into(),
                original_path: "/p/one".into(),
                canonical_path: "/p/one".into(),
                pre_existed: true,
                pre_hash: None,
                pre_size: Some(1),
                post_hash: None,
                post_size: None,
                created_at_ms: i,
                backup_file: Some(format!("{id}.bak")),
            };
            write_manifest(&manifests_dir, &m).unwrap();
        }
        for i in 0..2u128 {
            let id = format!("bbb-{i:03}");
            atomic_write_sync(&payloads_dir.join(format!("{id}.bak")), b"y").unwrap();
            let m = CheckpointManifest {
                version: 1,
                checkpoint_id: id.clone(),
                session_id: "s".into(),
                reason: "write_file".into(),
                original_path: "/p/two".into(),
                canonical_path: "/p/two".into(),
                pre_existed: true,
                pre_hash: None,
                pre_size: Some(1),
                post_hash: None,
                post_size: None,
                created_at_ms: 100 + i,
                backup_file: Some(format!("{id}.bak")),
            };
            write_manifest(&manifests_dir, &m).unwrap();
        }

        // Per-path cap 2: oldest 3 of /p/one go; /p/two untouched (2 <= 2).
        prune_checkpoints(&manifests_dir, &payloads_dir, 2, u64::MAX).unwrap();
        let remaining = load_manifests(&manifests_dir);
        assert_eq!(remaining.len(), 4);
        assert!(remaining
            .iter()
            .all(|m| !m.checkpoint_id.starts_with("aaa-00")
                || m.checkpoint_id == "aaa-003"
                || m.checkpoint_id == "aaa-004"));
        assert!(!payloads_dir.join("aaa-000.bak").exists());

        // Total cap 3 bytes: oldest overall (aaa-003, then bbb-000) pruned.
        prune_checkpoints(&manifests_dir, &payloads_dir, usize::MAX, 3).unwrap();
        let remaining = load_manifests(&manifests_dir);
        assert_eq!(remaining.len(), 3);
        let mut ids: Vec<&str> = remaining.iter().map(|m| m.checkpoint_id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec!["aaa-004", "bbb-000", "bbb-001"]);
    }

    #[test]
    fn for_session_uses_persistent_session_dir_and_falls_back_safely() {
        let dir = tempfile::tempdir().unwrap();
        let hook = CheckpointHook::for_session("sess-123", Some(dir.path()));
        assert_eq!(
            hook.backup_dir,
            dir.path().join("sess-123").join("checkpoints")
        );
        assert!(hook.backup_dir.join("manifests").is_dir());
        assert!(hook.backup_dir.join("payloads").is_dir());
        // Path-unsafe session id or no store dir: fall back to system temp
        // rather than escaping the sessions directory.
        let hook = CheckpointHook::for_session("../escape", Some(dir.path()));
        assert!(hook.backup_dir.starts_with(std::env::temp_dir()));
        let hook = CheckpointHook::for_session("sess-123", None);
        assert!(hook.backup_dir.starts_with(std::env::temp_dir()));
    }

    #[test]
    fn non_file_tools_and_missing_paths_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let hook = hook_in(dir.path());

        // Read-only tool: no checkpoint.
        hook.pre_tool_use(&call(
            "read_file",
            serde_json::json!({ "path": "/tmp/whatever" }),
        ))
        .unwrap();
        // Modifying tool without a path argument: no checkpoint, no error.
        hook.pre_tool_use(&call("write_file", serde_json::json!({ "content": "x" })))
            .unwrap();

        assert!(manifests(&hook).is_empty());
        let _ = fs::remove_dir_all(&hook.backup_dir);
    }
}
