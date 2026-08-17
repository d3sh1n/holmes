use anyhow::{anyhow, Result};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashSet;
use tracing::debug;

use crate::registry::Tool;
use holmes_core::{FunctionDefinition, ToolDefinition};

/// Default cap on returned characters — enough for a report section without
/// flooding the context. The model can raise it or narrow via `pages`.
const DEFAULT_MAX_CHARS: usize = 20_000;
/// Absolute ceiling so a huge document can never be dumped wholesale into context.
const HARD_MAX_CHARS: usize = 200_000;

/// Extracts text from a local PDF. Read-only, so it is safe to run in parallel and
/// under `read_only` permission mode. Text-based PDFs only — scanned/image-only PDFs
/// carry no extractable text (there is no OCR here).
pub struct ReadPdfTool;

#[derive(Deserialize)]
struct Args {
    /// Path to the PDF file on the local filesystem.
    path: String,
    /// Optional 1-indexed page selection, e.g. "1", "1-5", or "2,4,7-9".
    #[serde(default)]
    pages: Option<String>,
    /// Optional cap on returned characters (clamped to HARD_MAX_CHARS).
    #[serde(default)]
    max_chars: Option<usize>,
}

#[async_trait::async_trait]
impl Tool for ReadPdfTool {
    fn name(&self) -> &str {
        "read_pdf"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            tool_type: "function".into(),
            function: FunctionDefinition {
                name: "read_pdf".into(),
                description: "Read and extract text from a local PDF file. Returns the extracted \
                    text (with per-page markers), the document's page count, and whether the \
                    output was truncated. Use `pages` to select specific pages (e.g. \"1-5\" or \
                    \"2,4,7\") and `max_chars` to bound the output size. Text-based PDFs only: \
                    scanned/image-only PDFs have no extractable text (no OCR)."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Path to the PDF file on the local filesystem." },
                        "pages": { "type": "string", "description": "Optional 1-indexed page selection, e.g. \"1\", \"1-5\", or \"2,4,7-9\". Omit for the whole document." },
                        "max_chars": { "type": "integer", "description": "Optional cap on returned characters (default 20000)." }
                    },
                    "required": ["path"]
                }),
            },
        }
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(&self, args: &str) -> Result<String> {
        let parsed: Args =
            serde_json::from_str(args).map_err(|e| anyhow!("invalid arguments: {e}"))?;
        let max_chars = parsed
            .max_chars
            .unwrap_or(DEFAULT_MAX_CHARS)
            .clamp(1, HARD_MAX_CHARS);
        debug!(path = %parsed.path, "reading pdf");

        let bytes = tokio::fs::read(&parsed.path)
            .await
            .map_err(|e| anyhow!("cannot read file '{}': {e}", parsed.path))?;
        if bytes.is_empty() {
            return Err(anyhow!("file '{}' is empty", parsed.path));
        }

        let pages_text = extract_pages_blocking(bytes).await?;
        build_output(&parsed.path, pages_text, parsed.pages.as_deref(), max_chars)
    }
}

/// Extract per-page text off the async runtime. Running in `spawn_blocking` keeps the
/// CPU-bound parse from stalling other tasks AND turns a panic on a malformed/hostile
/// PDF into a recoverable error instead of tearing down the agent.
async fn extract_pages_blocking(bytes: Vec<u8>) -> Result<Vec<String>> {
    tokio::task::spawn_blocking(move || pdf_extract::extract_text_from_mem_by_pages(&bytes))
        .await
        .map_err(|join_err| {
            if join_err.is_panic() {
                anyhow!(
                    "PDF parser panicked — the file is likely malformed, encrypted, or unsupported"
                )
            } else {
                anyhow!("PDF extraction task failed: {join_err}")
            }
        })?
        .map_err(|e| {
            anyhow!(
                "failed to extract text (the PDF may be encrypted, corrupted, or image-only): {e}"
            )
        })
}

/// Assemble the JSON tool output from the per-page text: apply the optional page
/// selection, add page markers, truncate to `max_chars`, and report metadata.
fn build_output(
    path: &str,
    pages_text: Vec<String>,
    page_spec: Option<&str>,
    max_chars: usize,
) -> Result<String> {
    let total = pages_text.len();
    let selected: Vec<usize> = match page_spec {
        Some(spec) => parse_page_ranges(spec, total)?,
        None => (0..total).collect(),
    };

    let has_text = selected
        .iter()
        .any(|&i| pages_text.get(i).is_some_and(|p| !p.trim().is_empty()));

    let mut combined = String::new();
    for &idx in &selected {
        let page_no = idx + 1;
        let body = pages_text.get(idx).map(String::as_str).unwrap_or("");
        combined.push_str(&format!("\n----- Page {page_no} -----\n"));
        combined.push_str(body.trim_end());
        combined.push('\n');
    }
    let combined = combined.trim().to_string();
    let full_len = combined.chars().count();
    let (text, truncated) = truncate_chars(&combined, max_chars);

    let mut out = json!({
        "path": path,
        "page_count": total,
        "returned_pages": selected.len(),
        "char_count": full_len,
        "truncated": truncated,
        "text": text,
    });
    if !has_text {
        out["note"] = json!(
            "No extractable text on the selected page(s). The PDF is likely scanned/image-only; \
             OCR is not supported."
        );
    }
    if truncated {
        out["hint"] = json!(format!(
            "Output truncated at {max_chars} chars; narrow with `pages` or raise `max_chars` to see more."
        ));
    }
    Ok(out.to_string())
}

/// Parse a 1-indexed page selection like "1", "1-5", "2,4,7-9" into deduped, ordered,
/// 0-based indices clamped to `[0, total)`. Errors on malformed input or an empty
/// result (e.g. all requested pages are out of range).
fn parse_page_ranges(spec: &str, total: usize) -> Result<Vec<usize>> {
    let mut out: Vec<usize> = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((start_str, end_str)) = part.split_once('-') {
            let start: usize = start_str
                .trim()
                .parse()
                .map_err(|_| anyhow!("invalid page range '{part}'"))?;
            let end: usize = end_str
                .trim()
                .parse()
                .map_err(|_| anyhow!("invalid page range '{part}'"))?;
            if start == 0 || end == 0 {
                return Err(anyhow!("pages are 1-indexed; '{part}' is invalid"));
            }
            if start > end {
                return Err(anyhow!("invalid page range '{part}' (start > end)"));
            }
            for page in start..=end {
                if page <= total {
                    out.push(page - 1);
                }
            }
        } else {
            let page: usize = part
                .parse()
                .map_err(|_| anyhow!("invalid page number '{part}'"))?;
            if page == 0 {
                return Err(anyhow!("pages are 1-indexed; '0' is invalid"));
            }
            if page <= total {
                out.push(page - 1);
            }
        }
    }

    let mut seen = HashSet::new();
    out.retain(|page| seen.insert(*page));

    if out.is_empty() {
        return Err(anyhow!(
            "no valid pages in '{spec}' (document has {total} page(s))"
        ));
    }
    Ok(out)
}

/// Truncate to at most `max` characters on a char boundary (never mid-codepoint),
/// returning the text and whether truncation occurred.
fn truncate_chars(s: &str, max: usize) -> (String, bool) {
    // The byte offset of the (max+1)-th char is where `max` chars end; if there is no
    // such char, the string is already within budget.
    match s.char_indices().nth(max) {
        Some((byte_idx, _)) => (s[..byte_idx].to_string(), true),
        None => (s.to_string(), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ranges_expands_and_dedupes() {
        assert_eq!(parse_page_ranges("1-3", 10).unwrap(), vec![0, 1, 2]);
        assert_eq!(parse_page_ranges("2,4", 10).unwrap(), vec![1, 3]);
        assert_eq!(parse_page_ranges("1,1,2", 10).unwrap(), vec![0, 1]);
        // out-of-range tail is clamped away, not an error
        assert_eq!(parse_page_ranges("1-5", 2).unwrap(), vec![0, 1]);
    }

    #[test]
    fn parse_ranges_rejects_bad_input() {
        assert!(parse_page_ranges("0", 5).is_err()); // 1-indexed
        assert!(parse_page_ranges("3-1", 5).is_err()); // reversed
        assert!(parse_page_ranges("abc", 5).is_err()); // not a number
        assert!(parse_page_ranges("9", 3).is_err()); // entirely out of range -> empty
    }

    #[test]
    fn truncate_is_char_boundary_safe() {
        let (t, cut) = truncate_chars("héllo", 3); // multibyte 'é'
        assert_eq!(t, "hél");
        assert!(cut);
        let (t, cut) = truncate_chars("hi", 10);
        assert_eq!(t, "hi");
        assert!(!cut);
    }

    #[test]
    fn build_output_selects_pages_and_reports_metadata() {
        let pages = vec!["alpha text".to_string(), "bravo text".to_string()];
        let out = build_output("doc.pdf", pages.clone(), None, 1000).unwrap();
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["page_count"], 2);
        assert_eq!(value["returned_pages"], 2);
        assert_eq!(value["truncated"], false);
        let text = value["text"].as_str().unwrap();
        assert!(text.contains("Page 1") && text.contains("alpha"));
        assert!(text.contains("Page 2") && text.contains("bravo"));

        // Page selection returns only the requested page.
        let out = build_output("doc.pdf", pages, Some("2"), 1000).unwrap();
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["returned_pages"], 1);
        let text = value["text"].as_str().unwrap();
        assert!(text.contains("bravo") && !text.contains("alpha"));
    }

    #[test]
    fn build_output_flags_image_only_pdf() {
        let pages = vec!["   ".to_string(), "\n".to_string()];
        let out = build_output("scan.pdf", pages, None, 1000).unwrap();
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(value["note"]
            .as_str()
            .unwrap()
            .contains("scanned/image-only"));
    }

    #[test]
    fn build_output_truncates_and_hints() {
        let pages = vec!["x".repeat(500)];
        let out = build_output("big.pdf", pages, None, 50).unwrap();
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["truncated"], true);
        assert!(value["hint"].as_str().unwrap().contains("truncated"));
    }

    #[tokio::test]
    async fn garbage_bytes_error_without_panicking() {
        // Hostile / non-PDF input must surface as a recoverable error, never a panic.
        let result = extract_pages_blocking(b"this is not a pdf at all".to_vec()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn missing_file_reports_clear_error() {
        let tool = ReadPdfTool;
        let err = tool
            .execute(r#"{"path":"/no/such/file/holmes-xyz.pdf"}"#)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cannot read file"));
    }

    /// Build a minimal one-page PDF with extractable text using lopdf, then drive the
    /// real `read_pdf` tool end-to-end through a temp file.
    #[tokio::test]
    async fn extracts_text_from_a_real_pdf_end_to_end() {
        use lopdf::content::{Content, Operation};
        use lopdf::{dictionary, Document, Object, Stream};
        use std::io::Write as _;

        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
        });
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! { "F1" => font_id },
        });
        let content = Content {
            operations: vec![
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec!["F1".into(), 24.into()]),
                Operation::new("Td", vec![100.into(), 700.into()]),
                Operation::new("Tj", vec![Object::string_literal("Hello Holmes")]),
                Operation::new("ET", vec![]),
            ],
        };
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Resources" => resources_id,
        });
        let pages = dictionary! {
            "Type" => "Pages", "Kids" => vec![page_id.into()], "Count" => 1,
        };
        doc.objects.insert(pages_id, Object::Dictionary(pages));
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog", "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);

        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();

        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&bytes).unwrap();
        let path = file.path().to_string_lossy().to_string();

        let tool = ReadPdfTool;
        let args = json!({ "path": path }).to_string();
        let out = tool.execute(&args).await.expect("read_pdf should succeed");
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();

        assert_eq!(value["page_count"], 1, "one-page document");
        assert_eq!(value["truncated"], false);
        assert!(
            value["text"].as_str().unwrap().contains("Hello Holmes"),
            "extracted text should contain the page content, got: {}",
            value["text"]
        );
    }
}
