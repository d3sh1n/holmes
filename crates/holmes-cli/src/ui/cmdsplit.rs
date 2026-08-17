//! Tree-sitter based splitting of compound shell commands into individual
//! segments, so the Ask-mode permission card can expose each command of
//! `ls && rm -rf x` separately instead of approving the string wholesale.
//!
//! Modelled after grok-build's `bash_command_splitting`: top-level `command`
//! nodes joined by `&&`, `||`, `;` and `|` each become a segment. Pipes split
//! too — the right-hand side of a pipe is a fresh command semantically
//! (`ls | wc -l` prompts for both `ls` and `wc`), matching grok's word-only
//! sequence model.
//!
//! Command substitutions (`$(...)`, backticks) are NOT split: their content is
//! argument semantics of the containing command, which stays a single segment
//! with the substitution verbatim in its text. (grok rejects such scripts into
//! a different code path; holmes keeps them opaque — the runtime's
//! dangerous_command pre-guard is the orthogonal backstop for smuggled
//! payloads.)
//!
//! Unparseable input falls back to a single segment spanning the whole
//! command — conservative, since prefix matching then applies to the whole
//! string exactly as before this mechanism existed.

use tree_sitter::{Node, Parser};
use tree_sitter_bash::LANGUAGE as BASH;

/// One command of a (possibly compound) shell command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// Verbatim segment text (trimmed), e.g. `rm -rf x`.
    pub text: String,
    /// First word — the invoked program, e.g. `rm`.
    pub program: String,
}

/// Node kinds the descent may pass through to reach a top-level segment.
/// Anything else (command substitutions, subshells, if/for/while bodies,
/// function definitions) stops the descent, so nested commands never split
/// out on their own.
const TRANSPARENT: &[&str] = &["program", "list", "pipeline", "negated_command"];

/// Split a shell command line into its top-level command segments.
///
/// Always returns at least one segment: on any parse problem the whole
/// command is returned as a single segment (fail-conservative).
pub fn split_command(cmd: &str) -> Vec<Segment> {
    let fallback = || vec![whole_segment(cmd)];

    let mut parser = Parser::new();
    if parser.set_language(&BASH.into()).is_err() {
        return fallback();
    }
    let Some(tree) = parser.parse(cmd, None) else {
        return fallback();
    };
    if tree.root_node().has_error() {
        return fallback();
    }

    let mut out = Vec::new();
    collect(tree.root_node(), cmd, &mut out);
    if out.is_empty() {
        fallback()
    } else {
        out
    }
}

fn whole_segment(cmd: &str) -> Segment {
    Segment {
        text: cmd.trim().to_string(),
        program: cmd.split_whitespace().next().unwrap_or("").to_string(),
    }
}

fn collect(node: Node, src: &str, out: &mut Vec<Segment>) {
    match node.kind() {
        "command" => {
            if let Some(seg) = segment_from(node, src) {
                out.push(seg);
            }
        }
        "redirected_statement" => {
            let mut cursor = node.walk();
            let children: Vec<Node> = node.named_children(&mut cursor).collect();
            if children.iter().any(|c| c.kind() == "command") {
                // Plain command with redirects: one segment, redirects included
                // in its text (e.g. `echo done > /tmp/x`).
                if let Some(seg) = segment_from(node, src) {
                    out.push(seg);
                }
            } else {
                // The redirect binds a whole list/pipeline (`a && b > f`):
                // split the body, then re-attach the redirect text to the
                // final segment so the card still shows where output goes.
                let redirects: Vec<String> = children
                    .iter()
                    .filter(|c| !matches!(c.kind(), "list" | "pipeline"))
                    .filter_map(|c| c.utf8_text(src.as_bytes()).ok())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                let before = out.len();
                for child in children {
                    collect(child, src, out);
                }
                if !redirects.is_empty() && out.len() > before {
                    if let Some(last) = out.last_mut() {
                        last.text = format!("{} {}", last.text, redirects.join(" "));
                    }
                }
            }
        }
        kind if TRANSPARENT.contains(&kind) => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect(child, src, out);
            }
        }
        // Substitutions, subshells, control flow, declarations: not split.
        _ => {}
    }
}

fn segment_from(node: Node, src: &str) -> Option<Segment> {
    let text = node.utf8_text(src.as_bytes()).ok()?.trim().to_string();
    if text.is_empty() {
        return None;
    }
    let program = command_name_text(node, src)
        .or_else(|| text.split_whitespace().next().map(str::to_string))?;
    Some(Segment { text, program })
}

/// The `command_name` word of a `command` (or of the inner command of a
/// `redirected_statement`), verbatim from the source.
fn command_name_text(node: Node, src: &str) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "command_name" => {
                return child.utf8_text(src.as_bytes()).ok().map(str::to_string);
            }
            "command" => return command_name_text(child, src),
            _ => {}
        }
    }
    None
}

/// `command` starts with `prefix` on a word boundary (so "cargo" authorizes
/// "cargo build" but not "cargoless-x").
pub fn prefix_matches(command: &str, prefix: &str) -> bool {
    command == prefix
        || command
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with(' '))
}

/// The leading `words` words of a segment's text — the scope an "Always
/// allow" grants, e.g. `segment_prefix("cargo test --workspace", 2)` is
/// "cargo test".
pub fn segment_prefix(text: &str, words: usize) -> String {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    let n = words.clamp(1, tokens.len().max(1));
    tokens[..n.min(tokens.len())].join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(cmd: &str) -> Vec<String> {
        split_command(cmd).into_iter().map(|s| s.text).collect()
    }

    #[test]
    fn single_command_is_one_segment() {
        let segs = split_command("cargo test --workspace");
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].text, "cargo test --workspace");
        assert_eq!(segs[0].program, "cargo");
    }

    #[test]
    fn splits_on_and_or_semicolon_pipe() {
        assert_eq!(texts("ls -la && cargo build"), ["ls -la", "cargo build"]);
        assert_eq!(texts("ls; cargo build"), ["ls", "cargo build"]);
        assert_eq!(texts("ls || echo fail"), ["ls", "echo fail"]);
        assert_eq!(
            texts("cat f | grep x | wc -l"),
            ["cat f", "grep x", "wc -l"]
        );
    }

    #[test]
    fn splits_mixed_operators_in_source_order() {
        assert_eq!(texts("a && b | c ; d || e"), ["a", "b", "c", "d", "e"]);
    }

    #[test]
    fn quoted_operators_do_not_split() {
        let segs = split_command(r#"echo "a && b" && ls"#);
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].text, r#"echo "a && b""#);
        assert_eq!(segs[0].program, "echo");
        assert_eq!(segs[1].text, "ls");

        let segs = split_command("echo 'a; b | c'");
        assert_eq!(segs.len(), 1);
    }

    #[test]
    fn command_substitution_is_not_split() {
        let segs = split_command("echo $(rm -rf x) && ls");
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].text, "echo $(rm -rf x)");
        assert_eq!(segs[0].program, "echo");
        assert_eq!(segs[1].text, "ls");

        let segs = split_command("echo `id`");
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].program, "echo");
    }

    #[test]
    fn redirects_stay_with_their_command() {
        let segs = split_command("cargo test 2>&1 | tail -5 && echo done > /tmp/x");
        assert_eq!(segs.len(), 3);
        assert_eq!(segs[0].program, "cargo");
        assert_eq!(segs[1].text, "tail -5");
        assert_eq!(segs[2].program, "echo");
        assert!(segs[2].text.contains("> /tmp/x"));
    }

    #[test]
    fn leading_env_assignment_keeps_program_name() {
        let segs = split_command("FOO=bar cargo test");
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].program, "cargo");
        assert_eq!(segs[0].text, "FOO=bar cargo test");
    }

    #[test]
    fn broken_syntax_falls_back_to_single_segment() {
        for bad in ["ls &&", "foo |", "((", "if then"] {
            let segs = split_command(bad);
            assert_eq!(segs.len(), 1, "fallback for {bad:?}");
            assert_eq!(segs[0].text, bad.trim());
        }
    }

    #[test]
    fn empty_command_falls_back_to_single_segment() {
        let segs = split_command("");
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].text, "");
    }

    #[test]
    fn prefix_matching_respects_word_boundaries() {
        assert!(prefix_matches("cargo", "cargo"));
        assert!(prefix_matches("cargo build", "cargo"));
        assert!(prefix_matches("cargo test --workspace", "cargo test"));
        assert!(!prefix_matches("cargoless-x", "cargo"));
        assert!(!prefix_matches("cargo", "cargo test"));
        assert!(!prefix_matches("git push", "git status"));
    }

    #[test]
    fn segment_prefix_takes_leading_words() {
        assert_eq!(segment_prefix("cargo test --workspace", 1), "cargo");
        assert_eq!(segment_prefix("cargo test --workspace", 2), "cargo test");
        assert_eq!(
            segment_prefix("cargo test --workspace", 99),
            "cargo test --workspace"
        );
        assert_eq!(segment_prefix("ls", 2), "ls");
    }
}
