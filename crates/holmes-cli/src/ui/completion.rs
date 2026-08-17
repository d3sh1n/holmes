//! `@path` fuzzy file completion for the inline prompt.
//!
//! Typing `@` opens path completion (grok's mechanism, adapted): the token under the
//! caret is the query, a background thread fuzzy-matches it against a cached walk of
//! the working directory via nucleo, and the UI renders the top candidates above the
//! input box. Design constraints:
//!
//! - **The UI thread never matches.** Walking and scoring run on a worker thread that
//!   only starts on the first `@` (process-wide, once). The UI sends queries with a
//!   monotonically increasing *generation* and only accepts the response for the
//!   latest one — a late answer to a stale query would otherwise flash outdated
//!   candidates for a frame (the "generation fence").
//! - This is input assistance only: `expand_file_mentions` (submit-time attachment)
//!   keeps its own semantics and is untouched.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::ui::theme::theme;

/// Max candidates shown in the list.
pub const MAX_CANDIDATES: usize = 8;
/// Cap on indexed paths so a huge tree can't stall the walker.
const MAX_INDEX_ENTRIES: usize = 20_000;
/// Recursion depth cap (also bounds symlink loops, which we follow via `metadata`).
const MAX_DEPTH: usize = 12;
/// Directories never worth completing.
const SKIP_DIRS: [&str; 3] = [".git", "target", "node_modules"];
/// The index is rebuilt when a query arrives and the cache is older than this. Cheap
/// enough (the walk is shallow-capped) and means newly created files show up without
/// any invalidation machinery.
const INDEX_TTL: Duration = Duration::from_secs(30);

/// An active `@` query: the token from `@` up to the caret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AtQuery {
    /// Byte offset of the `@` in the input's display text.
    pub at_start: usize,
    /// Byte offset where the replaceable query text starts (after `@` / `@!`).
    pub replace_from: usize,
    /// The query text (`@` and optional `!` stripped).
    pub query: String,
    /// `@!` prefix: include hidden files/dirs in candidates.
    pub hidden: bool,
    /// Query ends with `/`: only directories match (path-prefix browsing).
    pub dirs_only: bool,
}

/// Detect an `@` completion context at `cursor` in `input`. Triggers only when the
/// `@` starts a whitespace-delimited token — `foo@bar` (email-ish) and `a/@b` never
/// trigger because their `@` is not at a token boundary. A query containing a space
/// can't happen by construction (the token IS whitespace-delimited), which is exactly
/// why "query 含空格不触发" falls out naturally.
pub fn at_query(input: &str, cursor: usize) -> Option<AtQuery> {
    let cursor = cursor.min(input.len());
    if !input.is_char_boundary(cursor) {
        return None;
    }
    let left = &input[..cursor];
    let start = left
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    let token = &left[start..];
    let rest = token.strip_prefix('@')?;
    let (hidden, query) = match rest.strip_prefix('!') {
        Some(q) => (true, q),
        None => (false, rest),
    };
    Some(AtQuery {
        at_start: start,
        replace_from: cursor - query.len(),
        query: query.to_string(),
        hidden,
        dirs_only: query.ends_with('/'),
    })
}

/// The ghost-text suffix for a candidate: only when the candidate literally continues
/// what was typed (fuzzy matches have no clean "remainder", so they get no ghost).
pub fn ghost_suffix(query: &AtQuery, candidate: &str) -> Option<String> {
    candidate
        .strip_prefix(query.query.as_str())
        .filter(|rest| !rest.is_empty())
        .map(str::to_string)
}

/// What accepting `candidate` writes into the input: `(replace_start, insert_text)`.
/// Replacement starts right after the `@` — in particular it swallows the `!` of an
/// `@!` query, because submit-time `expand_file_mentions` only understands `@path`
/// (a leftover bang would make the attached path unresolvable). File candidates get a
/// trailing space (query over); directory candidates don't (completion stays open for
/// drill-down).
pub fn accept_replacement(query: &AtQuery, candidate: &str) -> (usize, String) {
    let insert = if candidate.ends_with('/') {
        candidate.to_string()
    } else {
        format!("{candidate} ")
    };
    (query.at_start + 1, insert)
}

/// Monotonic generation counter — the UI half of the fence. Every keystroke that
/// changes the query bumps it; responses tagged with an older generation are dropped.
#[derive(Default)]
pub struct GenerationFence {
    current: u64,
}

impl GenerationFence {
    /// Bump and return the new current generation.
    pub fn next_generation(&mut self) -> u64 {
        self.current += 1;
        self.current
    }

    /// Only the in-flight generation is acceptable.
    pub fn accept(&self, generation: u64) -> bool {
        generation == self.current && generation != 0
    }
}

/// One indexed path (relative to the completion root, `/`-suffixed for directories).
#[derive(Clone, Debug)]
pub struct IndexEntry {
    pub path: String,
    pub is_dir: bool,
    /// Any path component starts with `.` (only surfaced on `@!`).
    pub hidden: bool,
}

/// Walk `root` into a flat, sorted index. Skips `SKIP_DIRS`, caps depth and entry
/// count so the worst case stays a few milliseconds of background work.
pub fn build_index(root: &Path) -> Vec<IndexEntry> {
    let mut out = Vec::new();
    walk(root, root, 0, &mut out);
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

fn walk(root: &Path, dir: &Path, depth: usize, out: &mut Vec<IndexEntry>) {
    if depth >= MAX_DEPTH || out.len() >= MAX_INDEX_ENTRIES {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        if out.len() >= MAX_INDEX_ENTRIES {
            return;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if SKIP_DIRS.contains(&name.as_ref()) {
            continue;
        }
        let path = entry.path();
        // `metadata` follows symlinks; depth-capped so loops terminate.
        let Ok(meta) = entry.metadata() else { continue };
        let is_dir = meta.is_dir();
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let mut rel = rel.to_string_lossy().replace('\\', "/");
        if is_dir {
            rel.push('/');
        }
        let hidden = rel.split('/').any(|c| c.starts_with('.'));
        out.push(IndexEntry {
            path: rel,
            is_dir,
            hidden,
        });
        if is_dir {
            walk(root, &path, depth + 1, out);
        }
    }
}

/// Score `entries` against the query (nucleo fuzzy, Smart case, path-aware config) and
/// return the top `MAX_CANDIDATES` paths. Pure function — unit tests drive it without
/// the worker thread.
pub fn search(matcher: &mut Matcher, entries: &[IndexEntry], req: &Request) -> Vec<String> {
    let mut scored: Vec<(u32, &str)> = Vec::new();
    if req.query.is_empty() {
        // Nothing typed yet: no meaningful fuzzy score — alphabetical prefix of the
        // (already sorted) index.
        for e in entries {
            if (e.hidden && !req.hidden) || (req.dirs_only && !e.is_dir) {
                continue;
            }
            scored.push((0, &e.path));
        }
        scored.sort_by(|a, b| a.1.cmp(b.1));
    } else {
        let pattern = Pattern::parse(&req.query, CaseMatching::Smart, Normalization::Smart);
        let mut buf = Vec::new();
        for e in entries {
            if (e.hidden && !req.hidden) || (req.dirs_only && !e.is_dir) {
                continue;
            }
            let haystack = Utf32Str::new(&e.path, &mut buf);
            if let Some(score) = pattern.score(haystack, matcher) {
                scored.push((score, &e.path));
            }
        }
        // Highest score wins; ties break alphabetically for stable output.
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    }
    scored.truncate(MAX_CANDIDATES);
    scored.into_iter().map(|(_, p)| p.to_string()).collect()
}

/// A query for the worker. `generation` is the fence tag.
#[derive(Clone, Debug)]
pub struct Request {
    pub generation: u64,
    pub query: String,
    pub hidden: bool,
    pub dirs_only: bool,
}

/// The worker's answer. Stale generations are dropped by the receiver.
#[derive(Clone, Debug)]
pub struct Response {
    pub generation: u64,
    pub matches: Vec<String>,
}

/// Handle to the lazily spawned completion worker (index + match run off the UI
/// thread). The thread exits when the handle is dropped (channel closes). Responses
/// ride a tokio channel so the async idle loop can `.recv().await` them as wake-ups
/// — that is what keeps idle zero-tick (purely event-driven) instead of polling.
pub struct FileCompleter {
    tx: Sender<Request>,
    pub rx: UnboundedReceiver<Response>,
}

impl FileCompleter {
    pub fn spawn(cwd: PathBuf) -> std::io::Result<Self> {
        let (tx, req_rx) = channel::<Request>();
        let (resp_tx, rx) = tokio::sync::mpsc::unbounded_channel::<Response>();
        std::thread::Builder::new()
            .name("holmes-completion".into())
            .spawn(move || completer_loop(cwd, req_rx, resp_tx))?;
        Ok(Self { tx, rx })
    }

    pub fn query(&self, req: Request) {
        let _ = self.tx.send(req);
    }
}

fn completer_loop(cwd: PathBuf, rx: Receiver<Request>, tx: UnboundedSender<Response>) {
    // `UnboundedSender::send` is synchronous, so this plain std thread can use it.
    let mut index: Option<(Instant, Vec<IndexEntry>)> = None;
    let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
    while let Ok(mut req) = rx.recv() {
        // Coalesce keystroke bursts: only the newest queued query is worth work.
        while let Ok(newer) = rx.try_recv() {
            req = newer;
        }
        let stale = index
            .as_ref()
            .map(|(built, _)| built.elapsed() > INDEX_TTL)
            .unwrap_or(true);
        if stale {
            index = Some((Instant::now(), build_index(&cwd)));
        }
        let entries = &index.as_ref().expect("index just built").1;
        let resp = Response {
            generation: req.generation,
            matches: search(&mut matcher, entries, &req),
        };
        if tx.send(resp).is_err() {
            return; // UI gone
        }
    }
}

/// Render the candidate list (≤8 rows): selected row highlighted with the menu
/// selection colors; within each path the directory part is muted so the file name —
/// the part you actually scan for — pops.
pub fn candidate_lines(candidates: &[String], selected: usize) -> Vec<Line<'static>> {
    candidates
        .iter()
        .enumerate()
        .take(MAX_CANDIDATES)
        .map(|(i, path)| {
            let (dir, file) = match path.rfind('/') {
                Some(pos) => (&path[..pos + 1], &path[pos + 1..]),
                None => ("", path.as_str()),
            };
            let mut spans = vec![
                Span::raw("  ".to_string()),
                Span::styled(dir.to_string(), Style::default().fg(theme().text_faint)),
                Span::styled(file.to_string(), Style::default().fg(theme().text)),
            ];
            if i == selected {
                for span in &mut spans {
                    span.style = span
                        .style
                        .fg(theme().menu_sel_fg)
                        .bg(theme().menu_sel_bg)
                        .add_modifier(Modifier::BOLD);
                }
            }
            Line::from(spans)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str) -> IndexEntry {
        IndexEntry {
            path: path.to_string(),
            is_dir: path.ends_with('/'),
            hidden: path.split('/').any(|c| c.starts_with('.')),
        }
    }

    fn matcher() -> Matcher {
        Matcher::new(Config::DEFAULT.match_paths())
    }

    fn req(query: &str) -> Request {
        Request {
            generation: 1,
            query: query.to_string(),
            hidden: false,
            dirs_only: query.ends_with('/'),
        }
    }

    // ── at_query trigger / exclusion ──

    #[test]
    fn triggers_at_line_start_and_after_whitespace() {
        let q = at_query("@src/ma", 7).expect("line-start @ triggers");
        assert_eq!(q.query, "src/ma");
        assert_eq!(q.at_start, 0);
        assert_eq!(q.replace_from, 1);
        let q = at_query("look at @READ", 13).expect("after-space @ triggers");
        assert_eq!(q.query, "READ");
        assert_eq!(q.at_start, 8);
    }

    #[test]
    fn bare_at_triggers_with_empty_query() {
        let q = at_query("@", 1).expect("bare @ opens the browser");
        assert_eq!(q.query, "");
        assert!(!q.hidden);
    }

    #[test]
    fn email_and_embedded_at_never_trigger() {
        assert!(at_query("mail foo@bar", 12).is_none(), "email excluded");
        assert!(at_query("a/@b", 4).is_none(), "@ not at token boundary");
        assert!(at_query("x@y", 3).is_none());
    }

    #[test]
    fn typing_past_the_token_stops_triggering() {
        // Cursor moved on: the token under the caret no longer starts with '@'.
        assert!(at_query("@foo bar", 8).is_none());
        // …but the caret mid-token still completes that token.
        let q = at_query("@foo bar", 4).expect("caret inside the @ token");
        assert_eq!(q.query, "foo");
    }

    #[test]
    fn bang_prefix_means_include_hidden() {
        let q = at_query("@!env", 5).expect("@! triggers");
        assert!(q.hidden);
        assert_eq!(q.query, "env");
        assert_eq!(q.replace_from, 2);
    }

    #[test]
    fn trailing_slash_means_directories_only() {
        let q = at_query("@src/", 5).expect("trailing slash triggers");
        assert!(q.dirs_only);
        assert_eq!(q.query, "src/");
    }

    // ── search / matching ──

    #[test]
    fn fuzzy_match_ranks_and_caps_at_eight() {
        let entries: Vec<_> = (0..20)
            .map(|i| entry(&format!("src/mod{i:02}.rs")))
            .collect();
        let out = search(&mut matcher(), &entries, &req("mod"));
        assert_eq!(out.len(), MAX_CANDIDATES, "capped at 8");
        assert!(out.iter().all(|p| p.starts_with("src/mod")));
    }

    #[test]
    fn smart_case_lowercase_matches_everything_uppercase_is_exact() {
        let entries = vec![entry("README.md"), entry("src/readme_notes.rs")];
        let lower = search(&mut matcher(), &entries, &req("readme"));
        assert_eq!(lower.len(), 2, "lowercase query is case-insensitive");
        let upper = search(&mut matcher(), &entries, &req("README"));
        assert_eq!(upper, vec!["README.md"], "uppercase pins the case");
    }

    #[test]
    fn hidden_entries_only_surface_with_bang() {
        let entries = vec![entry(".env"), entry("env_notes.md"), entry(".config/")];
        let plain = search(&mut matcher(), &entries, &req("env"));
        assert_eq!(plain, vec!["env_notes.md"], "hidden filtered by default");
        let mut bang = req("env");
        bang.hidden = true;
        let with_hidden = search(&mut matcher(), &entries, &bang);
        assert!(with_hidden.contains(&".env".to_string()));
    }

    #[test]
    fn dirs_only_query_lists_directories() {
        let entries = vec![entry("src/"), entry("src/main.rs"), entry("scripts/")];
        let out = search(&mut matcher(), &entries, &req("s/"));
        assert!(out.iter().all(|p| p.ends_with('/')), "dirs only: {out:?}");
        assert!(out.contains(&"src/".to_string()));
    }

    #[test]
    fn empty_query_returns_sorted_prefix() {
        let entries = vec![entry("b.rs"), entry("a.rs"), entry("c.rs")];
        let out = search(&mut matcher(), &entries, &req(""));
        assert_eq!(out, vec!["a.rs", "b.rs", "c.rs"]);
    }

    // ── generation fence ──

    #[test]
    fn fence_rejects_stale_generations() {
        let mut fence = GenerationFence::default();
        let g1 = fence.next_generation();
        let g2 = fence.next_generation();
        assert!(!fence.accept(g1), "older generation dropped");
        assert!(fence.accept(g2), "current generation accepted");
        assert!(!fence.accept(0), "never a real generation");
    }

    #[test]
    fn ghost_suffix_only_on_literal_continuation() {
        let q = at_query("@src/ma", 7).unwrap();
        assert_eq!(ghost_suffix(&q, "src/main.rs"), Some("in.rs".to_string()));
        assert_eq!(ghost_suffix(&q, "src/main.rs/"), Some("in.rs/".to_string()));
        // A fuzzy-but-not-prefix candidate gets no ghost (no clean remainder).
        let fuzzy = at_query("@mrs", 4).unwrap();
        assert_eq!(ghost_suffix(&fuzzy, "src/main.rs"), None);
        // Exact full match → nothing left to show.
        let done = at_query("@a.rs", 5).unwrap();
        assert_eq!(ghost_suffix(&done, "a.rs"), None);
    }

    #[tokio::test]
    async fn worker_thread_round_trips_a_query() {
        let root = std::env::temp_dir().join(format!("holmes_compl_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(root.join("README.md"), "x").unwrap();
        let mut completer = FileCompleter::spawn(root.clone()).expect("spawn worker");
        completer.query(Request {
            generation: 7,
            query: "main".into(),
            hidden: false,
            dirs_only: false,
        });
        let resp = tokio::time::timeout(std::time::Duration::from_secs(10), completer.rx.recv())
            .await
            .expect("worker answers")
            .expect("channel open");
        assert_eq!(resp.generation, 7, "fence tag round-trips");
        assert_eq!(resp.matches, vec!["src/main.rs".to_string()]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn accept_replacement_keeps_at_and_swallows_bang() {
        // Plain query: replace after the '@', file gets a trailing space.
        let q = at_query("@src/ma", 7).unwrap();
        let (start, insert) = accept_replacement(&q, "src/main.rs");
        assert_eq!(start, 1);
        assert_eq!(insert, "src/main.rs ");
        // Directory: no trailing space (completion stays open for drill-down).
        let (start, insert) = accept_replacement(&q, "src/main/");
        assert_eq!((start, insert.as_str()), (1, "src/main/"));
        // `@!` query: the bang goes with the replacement, so the submitted text is a
        // plain `@path` that expand_file_mentions can resolve.
        let q = at_query("@!en", 4).unwrap();
        let (start, insert) = accept_replacement(&q, ".env");
        assert_eq!(start, 1, "replace starts right after '@', swallowing '!'");
        assert_eq!(insert, ".env ");
    }

    #[test]
    fn candidate_lines_highlight_selection_and_mute_dirs() {
        let lines = candidate_lines(&["src/main.rs".to_string(), "README.md".to_string()], 0);
        assert_eq!(lines.len(), 2);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("src/"));
        assert!(text.contains("main.rs"));
        assert_eq!(lines[0].spans[1].content.as_ref(), "src/");
        assert!(
            lines[0].spans[1].style.bg == Some(theme().menu_sel_bg),
            "selected row highlighted"
        );
        assert!(
            lines[1].spans[1].style.bg.is_none(),
            "unselected row not highlighted"
        );
    }

    #[test]
    fn index_skips_noise_dirs_and_marks_hidden() {
        let root = std::env::temp_dir().join(format!("holmes_index_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join(".git/objects")).unwrap();
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        std::fs::create_dir_all(root.join(".hidden")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(root.join(".git/config"), "x").unwrap();
        std::fs::write(root.join(".env"), "x").unwrap();
        let entries = build_index(&root);
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert!(paths.contains(&"src/") && paths.contains(&"src/main.rs"));
        assert!(!paths.iter().any(|p| p.contains(".git")), "git skipped");
        assert!(
            !paths.iter().any(|p| p.starts_with("target")),
            "target skipped"
        );
        assert!(paths.contains(&".env"));
        assert!(paths.contains(&".hidden/"));
        let dotenv = entries.iter().find(|e| e.path == ".env").unwrap();
        assert!(dotenv.hidden);
        let main = entries.iter().find(|e| e.path == "src/main.rs").unwrap();
        assert!(!main.hidden);
        let _ = std::fs::remove_dir_all(&root);
    }
}
