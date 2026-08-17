//! UI support: semantic theming with color/glyph degradation, style-preserving
//! line wrapping for scrollback commits, a single keyboard-input thread, key
//! ownership, dynamic live-region height, the Ask-mode permission card,
//! diff bodies (syntax-highlighted) for file-writing tool blocks,
//! streaming markdown rendering with checkpoint freezing, the segment-based
//! input buffer with atomic paste chips, and `@path` fuzzy file completion.

pub mod blocks;
pub mod buffer;
pub mod cmdsplit;
pub mod completion;
pub mod diff;
pub mod highlight;
pub mod input;
pub mod keys;
pub mod markdown;
pub mod permission;
pub mod theme;
pub mod viewport;
pub mod wrap;
