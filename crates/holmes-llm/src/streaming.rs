//! Streaming response helpers for LLM completions.
//!
//! Minimal placeholder. Actual SSE / chunked streaming will be wired
//! up once the engine driving streaming is in place.

use serde::{Deserialize, Serialize};

/// A single chunk of a streamed completion.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StreamChunk {
    /// Delta of assistant text since the last chunk.
    pub delta: String,
    /// `true` for the final chunk.
    pub done: bool,
}

/// Trait for consumers of streaming completion chunks.
pub trait StreamSink: Send {
    fn push(&mut self, chunk: StreamChunk);
}

impl StreamSink for Vec<StreamChunk> {
    fn push(&mut self, chunk: StreamChunk) { Vec::push(self, chunk); }
}
