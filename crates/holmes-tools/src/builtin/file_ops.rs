//! Precise file-operation tools (read / write / edit) mirroring the structured file
//! primitives of a modern coding agent. These replace fragile shell `cat`/`sed` usage:
//! reads are line-numbered and range-bounded, edits are exact-match and fail loudly
//! rather than silently corrupting a file.

use anyhow::{anyhow, Result};
use serde::Deserialize;
use serde_json::json;
use tracing::debug;

use crate::registry::Tool;
use holmes_core::{FunctionDefinition, ToolDefinition};

const READ_DEFAULT_LIMIT: usize = 2000;
const MAX_LINE_CHARS: usize = 2000;

// ─────────────────────────── read_file ───────────────────────────

pub struct ReadFileTool;

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

#[async_trait::async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn definition(&self) -> ToolDefinition {
        def(
            "read_file",
            "Read a UTF-8 text file from the local filesystem. Returns line-numbered \
             content (`<lineno>\\t<text>`). Use `offset` (1-indexed start line) and \
             `limit` for large files; long lines are truncated. For PDFs use `read_pdf`.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the file." },
                    "offset": { "type": "integer", "description": "1-indexed line to start at (default 1)." },
                    "limit": { "type": "integer", "description": "Max lines to return (default 2000)." }
                },
                "required": ["path"]
            }),
        )
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(&self, args: &str) -> Result<String> {
        let parsed: ReadArgs =
            serde_json::from_str(args).map_err(|e| anyhow!("invalid arguments: {e}"))?;
        debug!(path = %parsed.path, "read_file");
        // Read bytes so a binary file is reported cleanly instead of erroring on
        // invalid UTF-8 (the model should not retry-loop on a PNG/binary).
        let bytes = tokio::fs::read(&parsed.path)
            .await
            .map_err(|e| anyhow!("cannot read '{}': {e}", parsed.path))?;
        let content = match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(e) => {
                return Ok(format!(
                    "(binary file, {} bytes — not UTF-8 text, cannot display. If it is an \
                     image and you need to see it, view it via the browser; otherwise inspect \
                     it with a shell tool.)",
                    e.into_bytes().len()
                ));
            }
        };

        let start = parsed.offset.unwrap_or(1).max(1);
        let limit = parsed.limit.unwrap_or(READ_DEFAULT_LIMIT);
        let total = content.lines().count();

        let mut out = String::new();
        for (idx, line) in content.lines().enumerate().skip(start - 1).take(limit) {
            let line = if line.chars().count() > MAX_LINE_CHARS {
                let truncated: String = line.chars().take(MAX_LINE_CHARS).collect();
                format!("{truncated}… [line truncated]")
            } else {
                line.to_string()
            };
            out.push_str(&format!("{}\t{}\n", idx + 1, line));
        }
        if out.is_empty() {
            return Ok(format!("(no lines in range; file has {total} line(s))"));
        }
        Ok(out)
    }
}

// ─────────────────────────── write_file ───────────────────────────

pub struct WriteFileTool;

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
    #[serde(default)]
    expected_hash: Option<String>,
}

/// Optimistic-concurrency precondition shared by write_file/edit_file: when the
/// caller passes `expected_hash` (the sha256 the file had when they last read
/// it), refuse to clobber a file that changed since — a conflicting writer
/// (another subagent, a human editor) must never be silently overwritten.
fn check_expected_hash(path: &str, expected: &str, current: Option<&[u8]>) -> Result<()> {
    let actual = current.map(crate::fsutil::sha256_hex);
    if actual.as_deref() != Some(expected) {
        return Err(anyhow!(
            "conflict: '{path}' changed since it was read (expected sha256 {}, found {}); \
             re-read the file and retry with fresh content",
            expected,
            actual.as_deref().unwrap_or("<file does not exist>"),
        ));
    }
    Ok(())
}

#[async_trait::async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn definition(&self) -> ToolDefinition {
        def(
            "write_file",
            "Write (create or overwrite) a text file with the given content. Creates \
             parent directories as needed. Overwrites the whole file — use `edit_file` \
             for surgical changes. The write is crash-safe (atomic rename) and \
             serialized against concurrent writes to the same path. Pass \
             `expected_hash` (the sha256 the file had when you last read it) to make \
             the write fail with a conflict instead of silently losing someone \
             else's concurrent update.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to write." },
                    "content": { "type": "string", "description": "Full file content." },
                    "expected_hash": { "type": "string", "description": "Optional sha256 (hex) of the file content you last read. If the file changed since, the write fails with a conflict — re-read and retry." }
                },
                "required": ["path", "content"]
            }),
        )
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn execute(&self, args: &str) -> Result<String> {
        let parsed: WriteArgs =
            serde_json::from_str(args).map_err(|e| anyhow!("invalid arguments: {e}"))?;
        debug!(path = %parsed.path, "write_file");
        // Serialize against other writers of the same path (parallel tool calls,
        // in-process subagents) for the whole check-then-write sequence.
        let target = std::path::Path::new(&parsed.path);
        let _guard = crate::fsutil::file_lock(target).await;
        if let Some(parent) = target.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| anyhow!("cannot create dir for '{}': {e}", parsed.path))?;
            }
        }
        if let Some(expected) = &parsed.expected_hash {
            let current = tokio::fs::read(target).await.ok();
            check_expected_hash(&parsed.path, expected, current.as_deref())?;
        }
        let bytes = parsed.content.len();
        crate::fsutil::atomic_write(target, parsed.content.as_bytes())
            .await
            .map_err(|e| anyhow!("cannot write '{}': {e}", parsed.path))?;
        Ok(json!({
            "path": parsed.path,
            "bytes_written": bytes,
            "content_hash": crate::fsutil::sha256_hex(parsed.content.as_bytes()),
        })
        .to_string())
    }
}

// ─────────────────────────── edit_file ───────────────────────────

pub struct EditFileTool;

#[derive(Deserialize)]
struct EditArgs {
    path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
    #[serde(default)]
    expected_hash: Option<String>,
}

#[async_trait::async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn definition(&self) -> ToolDefinition {
        def(
            "edit_file",
            "Replace an exact string in a file. `old_string` must match the file exactly \
             (including whitespace) and, unless `replace_all` is true, must be unique — \
             otherwise the edit fails rather than guessing. Prefer this over shell sed for \
             surgical changes. The write is crash-safe (atomic rename) and serialized \
             against concurrent writes to the same path. Pass `expected_hash` (the sha256 \
             the file had when you last read it) to fail with a conflict instead of \
             silently editing a stale version.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to edit." },
                    "old_string": { "type": "string", "description": "Exact text to replace." },
                    "new_string": { "type": "string", "description": "Replacement text." },
                    "replace_all": { "type": "boolean", "description": "Replace every occurrence (default false)." },
                    "expected_hash": { "type": "string", "description": "Optional sha256 (hex) of the file content you last read. If the file changed since, the edit fails with a conflict — re-read and retry." }
                },
                "required": ["path", "old_string", "new_string"]
            }),
        )
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn execute(&self, args: &str) -> Result<String> {
        let parsed: EditArgs =
            serde_json::from_str(args).map_err(|e| anyhow!("invalid arguments: {e}"))?;
        debug!(path = %parsed.path, "edit_file");
        if parsed.old_string == parsed.new_string {
            return Err(anyhow!("old_string and new_string are identical"));
        }
        // Hold the per-path lock across read-modify-write so a concurrent writer
        // cannot interleave between our read and the atomic replace.
        let target = std::path::Path::new(&parsed.path);
        let _guard = crate::fsutil::file_lock(target).await;
        let content = tokio::fs::read_to_string(target)
            .await
            .map_err(|e| anyhow!("cannot read '{}': {e}", parsed.path))?;
        if let Some(expected) = &parsed.expected_hash {
            check_expected_hash(&parsed.path, expected, Some(content.as_bytes()))?;
        }

        let occurrences = content.matches(&parsed.old_string).count();
        if occurrences == 0 {
            return Err(anyhow!("old_string not found in '{}'", parsed.path));
        }
        if occurrences > 1 && !parsed.replace_all {
            return Err(anyhow!(
                "old_string is not unique in '{}' ({} occurrences); pass replace_all or add context",
                parsed.path,
                occurrences
            ));
        }

        let updated = if parsed.replace_all {
            content.replace(&parsed.old_string, &parsed.new_string)
        } else {
            content.replacen(&parsed.old_string, &parsed.new_string, 1)
        };
        crate::fsutil::atomic_write(target, updated.as_bytes())
            .await
            .map_err(|e| anyhow!("cannot write '{}': {e}", parsed.path))?;
        Ok(json!({
            "path": parsed.path,
            "replacements": occurrences.min(if parsed.replace_all { occurrences } else { 1 }),
            "content_hash": crate::fsutil::sha256_hex(updated.as_bytes()),
        })
        .to_string())
    }
}

// ─────────────────────────── write_todos ───────────────────────────

pub struct WriteTodosTool;

#[async_trait::async_trait]
impl Tool for WriteTodosTool {
    fn name(&self) -> &str {
        "write_todos"
    }

    fn definition(&self) -> ToolDefinition {
        def(
            "write_todos",
            "Record/replace your working task list for the engagement. Pass the FULL \
             current plan each time (it replaces the previous one). Keep exactly one item \
             `in_progress`. Use it to plan multi-step work and track progress — the plan \
             is shown back to you each turn under `[Plan]`.",
            json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "description": "The full task list.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": { "type": "string", "description": "The task." },
                                "status": { "type": "string", "enum": ["pending", "in_progress", "completed"] }
                            },
                            "required": ["content", "status"]
                        }
                    }
                },
                "required": ["todos"]
            }),
        )
    }

    fn is_read_only(&self) -> bool {
        // Records agent-internal plan state; no external side effects.
        true
    }

    async fn execute(&self, args: &str) -> Result<String> {
        // The PlanTracker PostGuard captures the plan into AttackState; here we just
        // validate and acknowledge.
        let parsed: serde_json::Value =
            serde_json::from_str(args).map_err(|e| anyhow!("invalid arguments: {e}"))?;
        let count = parsed
            .get("todos")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .ok_or_else(|| anyhow!("`todos` must be an array"))?;
        Ok(json!({ "ok": true, "items": count }).to_string())
    }
}

fn def(name: &str, description: &str, parameters: serde_json::Value) -> ToolDefinition {
    ToolDefinition {
        tool_type: "function".into(),
        function: FunctionDefinition {
            name: name.into(),
            description: description.into(),
            parameters,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn tmp(content: &str) -> tempfile::NamedTempFile {
        use std::io::Write as _;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f
    }

    #[tokio::test]
    async fn read_file_numbers_lines_and_honors_range() {
        let f = tmp("alpha\nbravo\ncharlie\ndelta\n").await;
        let path = f.path().to_string_lossy().to_string();
        let out = ReadFileTool
            .execute(&json!({ "path": path, "offset": 2, "limit": 2 }).to_string())
            .await
            .unwrap();
        assert_eq!(out, "2\tbravo\n3\tcharlie\n");
    }

    #[tokio::test]
    async fn read_file_reports_binary_instead_of_erroring() {
        use std::io::Write as _;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(&[0x89, 0x50, 0x4e, 0x47, 0x00, 0xff, 0xfe])
            .unwrap(); // PNG-ish bytes
        let path = f.path().to_string_lossy().to_string();
        let out = ReadFileTool
            .execute(&json!({ "path": path }).to_string())
            .await
            .unwrap();
        assert!(out.contains("binary file"), "got: {out}");
    }

    #[tokio::test]
    async fn read_file_missing_reports_error() {
        let err = ReadFileTool
            .execute(r#"{"path":"/no/such/holmes-file.txt"}"#)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cannot read"));
    }

    #[tokio::test]
    async fn edit_file_requires_unique_match() {
        let f = tmp("x = 1\nx = 1\n").await;
        let path = f.path().to_string_lossy().to_string();
        let err = EditFileTool
            .execute(
                &json!({ "path": path, "old_string": "x = 1", "new_string": "x = 2" }).to_string(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not unique"));
    }

    #[tokio::test]
    async fn edit_file_replaces_unique_and_all() {
        let f = tmp("a = 1\nb = 2\n").await;
        let path = f.path().to_string_lossy().to_string();
        EditFileTool
            .execute(
                &json!({ "path": path, "old_string": "a = 1", "new_string": "a = 9" }).to_string(),
            )
            .await
            .unwrap();
        let after = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(after, "a = 9\nb = 2\n");

        let f2 = tmp("z\nz\nz\n").await;
        let path2 = f2.path().to_string_lossy().to_string();
        EditFileTool
            .execute(&json!({ "path": path2, "old_string": "z", "new_string": "q", "replace_all": true }).to_string())
            .await
            .unwrap();
        assert_eq!(
            tokio::fs::read_to_string(&path2).await.unwrap(),
            "q\nq\nq\n"
        );
    }

    #[tokio::test]
    async fn write_file_creates_and_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/new.txt").to_string_lossy().to_string();
        WriteFileTool
            .execute(&json!({ "path": path, "content": "hello" }).to_string())
            .await
            .unwrap();
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), "hello");
    }

    #[test]
    fn read_only_flags_are_correct() {
        assert!(ReadFileTool.is_read_only());
        assert!(!WriteFileTool.is_read_only());
        assert!(!EditFileTool.is_read_only());
    }

    #[tokio::test]
    async fn write_file_returns_content_hash_and_enforces_expected_hash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt").to_string_lossy().to_string();
        let out = WriteFileTool
            .execute(&json!({ "path": path, "content": "v1" }).to_string())
            .await
            .unwrap();
        let hash_v1 = out
            .split("\"content_hash\":\"")
            .nth(1)
            .and_then(|s| s.split('"').next())
            .expect("content_hash in result")
            .to_string();
        assert_eq!(hash_v1, crate::fsutil::sha256_hex(b"v1"));

        // Matching precondition: write goes through.
        WriteFileTool
            .execute(
                &json!({ "path": path, "content": "v2", "expected_hash": hash_v1 }).to_string(),
            )
            .await
            .unwrap();
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), "v2");

        // Stale precondition: conflict, file untouched, message demands a re-read.
        let err = WriteFileTool
            .execute(
                &json!({ "path": path, "content": "v3", "expected_hash": hash_v1 }).to_string(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("conflict"), "got: {err}");
        assert!(err.to_string().contains("re-read"), "got: {err}");
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), "v2");
    }

    #[tokio::test]
    async fn edit_file_enforces_expected_hash() {
        let f = tmp("a = 1\n").await;
        let path = f.path().to_string_lossy().to_string();
        let err = EditFileTool
            .execute(
                &json!({
                    "path": path,
                    "old_string": "a = 1",
                    "new_string": "a = 2",
                    "expected_hash": crate::fsutil::sha256_hex(b"stale")
                })
                .to_string(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("conflict"), "got: {err}");
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), "a = 1\n");

        EditFileTool
            .execute(
                &json!({
                    "path": path,
                    "old_string": "a = 1",
                    "new_string": "a = 2",
                    "expected_hash": crate::fsutil::sha256_hex(b"a = 1\n")
                })
                .to_string(),
            )
            .await
            .unwrap();
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), "a = 2\n");
    }

    #[tokio::test]
    async fn concurrent_writers_with_expected_hash_lose_no_update() {
        // Two agents (subagent-style concurrent tasks) both read "base" and both
        // try to write. Serialization + the hash precondition guarantee one
        // succeeds and the other gets a conflict — never a silent lost update.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shared.txt");
        std::fs::write(&path, "base").unwrap();
        let base_hash = crate::fsutil::sha256_hex(b"base");

        let mut outcomes = Vec::new();
        let mut handles = Vec::new();
        for label in ["A", "B"] {
            let p = path.to_string_lossy().to_string();
            let h = base_hash.clone();
            handles.push(tokio::spawn(async move {
                WriteFileTool
                    .execute(
                        &json!({ "path": p, "content": format!("from-{label}"), "expected_hash": h })
                            .to_string(),
                    )
                    .await
            }));
        }
        for h in handles {
            outcomes.push(h.await.unwrap());
        }
        let successes = outcomes.iter().filter(|o| o.is_ok()).count();
        let conflicts = outcomes
            .iter()
            .filter_map(|o| o.as_ref().err())
            .filter(|e| e.to_string().contains("conflict"))
            .count();
        assert_eq!((successes, conflicts), (1, 1), "outcomes: {outcomes:?}");
        let final_content = std::fs::read_to_string(&path).unwrap();
        assert!(final_content == "from-A" || final_content == "from-B");
    }
}
