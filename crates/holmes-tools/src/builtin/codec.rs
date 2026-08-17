//! `codec` — fast, native encode/decode helpers (base64, URL, hex, JWT decode) so the
//! agent doesn't spawn a python subprocess for every micro-transform. Read-only, pure.

use anyhow::{anyhow, Result};
use base64::Engine;
use serde::Deserialize;
use serde_json::json;

use crate::registry::Tool;
use holmes_core::{FunctionDefinition, ToolDefinition};

pub struct CodecTool;

#[derive(Deserialize)]
struct Args {
    action: String,
    input: String,
}

#[async_trait::async_trait]
impl Tool for CodecTool {
    fn name(&self) -> &str {
        "codec"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            tool_type: "function".into(),
            function: FunctionDefinition {
                name: "codec".into(),
                description: "Encode/decode data for payload crafting and token inspection. \
                    Actions: base64_encode, base64_decode, base64url_decode, url_encode, \
                    url_decode, hex_encode, hex_decode, jwt_decode (splits a JWT and decodes \
                    its header+payload). Faster than shelling out to python."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["base64_encode","base64_decode","base64url_decode","url_encode","url_decode","hex_encode","hex_decode","jwt_decode"]
                        },
                        "input": { "type": "string", "description": "The data to transform." }
                    },
                    "required": ["action", "input"]
                }),
            },
        }
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(&self, args: &str) -> Result<String> {
        let a: Args = serde_json::from_str(args).map_err(|e| anyhow!("invalid arguments: {e}"))?;
        let out = match a.action.as_str() {
            "base64_encode" => base64::engine::general_purpose::STANDARD.encode(a.input.as_bytes()),
            "base64_decode" => decode_b64(&a.input, false)?,
            "base64url_decode" => decode_b64(&a.input, true)?,
            "url_encode" => url_encode(&a.input),
            "url_decode" => url_decode(&a.input)?,
            "hex_encode" => a
                .input
                .as_bytes()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
            "hex_decode" => hex_decode(&a.input)?,
            "jwt_decode" => jwt_decode(&a.input)?,
            other => return Err(anyhow!("unknown action '{other}'")),
        };
        Ok(out)
    }
}

fn decode_b64(input: &str, url_safe: bool) -> Result<String> {
    let engine = if url_safe {
        base64::engine::general_purpose::URL_SAFE_NO_PAD
    } else {
        base64::engine::general_purpose::STANDARD
    };
    let bytes = engine
        .decode(input.trim().trim_end_matches('='))
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(input.trim()))
        .map_err(|e| anyhow!("base64 decode failed: {e}"))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn url_decode(s: &str) -> Result<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                let v = u8::from_str_radix(hex, 16).map_err(|_| anyhow!("bad %-escape"))?;
                out.push(v);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

fn hex_decode(s: &str) -> Result<String> {
    let clean: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if !clean.len().is_multiple_of(2) {
        return Err(anyhow!("hex string has odd length"));
    }
    let mut bytes = Vec::with_capacity(clean.len() / 2);
    let cb = clean.as_bytes();
    for pair in cb.chunks(2) {
        let h = std::str::from_utf8(pair).unwrap_or("");
        bytes.push(u8::from_str_radix(h, 16).map_err(|_| anyhow!("invalid hex"))?);
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn jwt_decode(token: &str) -> Result<String> {
    let parts: Vec<&str> = token.trim().split('.').collect();
    if parts.len() < 2 {
        return Err(anyhow!("not a JWT (expected header.payload.signature)"));
    }
    let header = decode_b64(parts[0], true)?;
    let payload = decode_b64(parts[1], true)?;
    Ok(format!("header: {header}\npayload: {payload}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn run(action: &str, input: &str) -> Result<String> {
        CodecTool
            .execute(&json!({"action": action, "input": input}).to_string())
            .await
    }

    #[tokio::test]
    async fn base64_roundtrip() {
        let enc = run("base64_encode", "hello world").await.unwrap();
        assert_eq!(enc, "aGVsbG8gd29ybGQ=");
        assert_eq!(run("base64_decode", &enc).await.unwrap(), "hello world");
    }

    #[tokio::test]
    async fn url_roundtrip() {
        let enc = run("url_encode", "a b&c=d").await.unwrap();
        assert_eq!(enc, "a%20b%26c%3Dd");
        assert_eq!(run("url_decode", &enc).await.unwrap(), "a b&c=d");
    }

    #[tokio::test]
    async fn hex_roundtrip() {
        assert_eq!(run("hex_encode", "AB").await.unwrap(), "4142");
        assert_eq!(run("hex_decode", "4142").await.unwrap(), "AB");
    }

    #[tokio::test]
    async fn jwt_decodes_header_and_payload() {
        // {"alg":"HS256"} . {"sub":"123","admin":true} . sig
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjMiLCJhZG1pbiI6dHJ1ZX0.sig";
        let out = run("jwt_decode", jwt).await.unwrap();
        assert!(out.contains("HS256"), "{out}");
        assert!(out.contains("\"admin\":true"), "{out}");
    }
}
