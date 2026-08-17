//! Search primitives: `grep` (regex content search across files) and `glob` (filename
//! pattern matching). Read-only, so they are safe to run in parallel / under read_only
//! permission mode. These replace fragile shell `grep -r` / `find` pipelines.

use anyhow::{anyhow, Result};
use serde::Deserialize;
use serde_json::json;
use tracing::debug;

use crate::registry::Tool;
use holmes_core::{FunctionDefinition, ToolDefinition};

const DEFAULT_MAX_RESULTS: usize = 200;
/// Directories skipped during recursive walks to avoid noise/huge trees.
const SKIP_DIRS: &[&str] = &[".git", "target", "node_modules", ".venv", "dist", "build"];

// ─────────────────────────── grep ───────────────────────────

pub struct GrepTool;

#[derive(Deserialize)]
struct GrepArgs {
    pattern: String,
    #[serde(default = "default_path")]
    path: String,
    #[serde(default)]
    glob: Option<String>,
    #[serde(default)]
    max_results: Option<usize>,
}

fn default_path() -> String {
    ".".to_string()
}

#[async_trait::async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn definition(&self) -> ToolDefinition {
        def(
            "grep",
            "Search file contents with a regular expression. Recursively walks `path` \
             (a file or directory), optionally filtered by a `glob` (e.g. \"*.rs\"). \
             Returns `path:line: text` matches. Skips .git/target/node_modules.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Rust-regex pattern to match per line." },
                    "path": { "type": "string", "description": "File or directory to search (default \".\")." },
                    "glob": { "type": "string", "description": "Optional filename filter, e.g. \"*.py\"." },
                    "max_results": { "type": "integer", "description": "Cap on matches returned (default 200)." }
                },
                "required": ["pattern"]
            }),
        )
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(&self, args: &str) -> Result<String> {
        let parsed: GrepArgs =
            serde_json::from_str(args).map_err(|e| anyhow!("invalid arguments: {e}"))?;
        debug!(pattern = %parsed.pattern, path = %parsed.path, "grep");
        let max = parsed.max_results.unwrap_or(DEFAULT_MAX_RESULTS);
        let regex =
            regex::Regex::new(&parsed.pattern).map_err(|e| anyhow!("invalid regex: {e}"))?;
        let name_filter = match parsed.glob.as_deref() {
            Some(g) => Some(glob::Pattern::new(g).map_err(|e| anyhow!("invalid glob: {e}"))?),
            None => None,
        };

        // Blocking filesystem walk + scan off the async runtime.
        let path = parsed.path.clone();
        let result = tokio::task::spawn_blocking(move || {
            grep_walk(&path, &regex, name_filter.as_ref(), max)
        })
        .await
        .map_err(|e| anyhow!("grep task failed: {e}"))??;

        if result.matches.is_empty() {
            return Ok("(no matches)".to_string());
        }
        let mut out = result.matches.join("\n");
        if result.truncated {
            out.push_str(&format!("\n… [truncated at {max} matches]"));
        }
        Ok(out)
    }
}

struct GrepResult {
    matches: Vec<String>,
    truncated: bool,
}

fn grep_walk(
    root: &str,
    regex: &regex::Regex,
    name_filter: Option<&glob::Pattern>,
    max: usize,
) -> Result<GrepResult> {
    let mut matches = Vec::new();
    let mut truncated = false;
    let meta = std::fs::metadata(root).map_err(|e| anyhow!("cannot access '{root}': {e}"))?;
    let files: Vec<std::path::PathBuf> = if meta.is_file() {
        vec![std::path::PathBuf::from(root)]
    } else {
        collect_files(std::path::Path::new(root))
    };

    'outer: for file in files {
        if let Some(filter) = name_filter {
            let name = file.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !filter.matches(name) {
                continue;
            }
        }
        let Ok(content) = std::fs::read_to_string(&file) else {
            continue; // skip binary / unreadable
        };
        for (idx, line) in content.lines().enumerate() {
            if regex.is_match(line) {
                let text: String = line.chars().take(300).collect();
                matches.push(format!(
                    "{}:{}: {}",
                    file.display(),
                    idx + 1,
                    text.trim_end()
                ));
                if matches.len() >= max {
                    truncated = true;
                    break 'outer;
                }
            }
        }
    }
    Ok(GrepResult { matches, truncated })
}

fn collect_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if SKIP_DIRS.contains(&name) || name.starts_with('.') {
                    continue;
                }
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
}

// ─────────────────────────── glob ───────────────────────────

pub struct GlobTool;

#[derive(Deserialize)]
struct GlobArgs {
    pattern: String,
    #[serde(default)]
    max_results: Option<usize>,
}

#[async_trait::async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn definition(&self) -> ToolDefinition {
        def(
            "glob",
            "List files matching a glob pattern (e.g. \"src/**/*.rs\", \"**/*.yaml\"). \
             Returns matching paths, one per line.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Glob pattern (supports ** and *)." },
                    "max_results": { "type": "integer", "description": "Cap on paths returned (default 200)." }
                },
                "required": ["pattern"]
            }),
        )
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(&self, args: &str) -> Result<String> {
        let parsed: GlobArgs =
            serde_json::from_str(args).map_err(|e| anyhow!("invalid arguments: {e}"))?;
        debug!(pattern = %parsed.pattern, "glob");
        let max = parsed.max_results.unwrap_or(DEFAULT_MAX_RESULTS);
        let pattern = parsed.pattern.clone();

        let paths = tokio::task::spawn_blocking(move || -> Result<Vec<String>> {
            let mut out = Vec::new();
            let entries = glob::glob(&pattern).map_err(|e| anyhow!("invalid glob: {e}"))?;
            for path in entries.flatten() {
                out.push(path.display().to_string());
                if out.len() >= max {
                    break;
                }
            }
            Ok(out)
        })
        .await
        .map_err(|e| anyhow!("glob task failed: {e}"))??;

        if paths.is_empty() {
            return Ok("(no files match)".to_string());
        }
        Ok(paths.join("\n"))
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

    #[tokio::test]
    async fn grep_finds_matches_with_glob_filter() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn main() {}\nlet x = 1;\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "let x = 2;\n").unwrap();
        let out = GrepTool
            .execute(
                &json!({ "pattern": "let x", "path": dir.path().to_string_lossy(), "glob": "*.rs" })
                    .to_string(),
            )
            .await
            .unwrap();
        assert!(out.contains("a.rs"));
        assert!(out.contains("let x = 1;"));
        assert!(!out.contains("b.txt"), "glob filter should exclude .txt");
    }

    #[tokio::test]
    async fn grep_no_matches_is_clean() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "nothing here\n").unwrap();
        let out = GrepTool
            .execute(&json!({ "pattern": "zzz", "path": dir.path().to_string_lossy() }).to_string())
            .await
            .unwrap();
        assert_eq!(out, "(no matches)");
    }

    #[tokio::test]
    async fn grep_rejects_bad_regex() {
        let err = GrepTool
            .execute(&json!({ "pattern": "(unclosed", "path": "." }).to_string())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("invalid regex"));
    }

    #[tokio::test]
    async fn glob_lists_matching_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("one.rs"), "").unwrap();
        std::fs::write(dir.path().join("two.rs"), "").unwrap();
        std::fs::write(dir.path().join("skip.txt"), "").unwrap();
        let pattern = format!("{}/*.rs", dir.path().to_string_lossy());
        let out = GlobTool
            .execute(&json!({ "pattern": pattern }).to_string())
            .await
            .unwrap();
        assert!(out.contains("one.rs") && out.contains("two.rs"));
        assert!(!out.contains("skip.txt"));
    }
}
