//! Deterministic in-memory clipboard for headless UI tests and automation.
//!
//! Prevents headless tests running in parallel from competing for the native
//! operating system clipboard, eliminating concurrency races and OS heap corruption.

use std::path::PathBuf;

/// A deterministic, in-memory mock clipboard for tests.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MockClipboard {
    pub text: Option<String>,
    pub image: Option<(usize, usize, Vec<u8>)>,
    pub files: Option<Vec<PathBuf>>,
}

impl MockClipboard {
    /// Creates an empty mock clipboard.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a mock clipboard containing plain text.
    pub fn with_text(text: impl Into<String>) -> Self {
        Self { text: Some(text.into()), image: None, files: None }
    }

    /// Creates a mock clipboard containing an image (width, height, RGBA bytes).
    pub fn with_image(width: usize, height: usize, rgba: Vec<u8>) -> Self {
        Self { text: None, image: Some((width, height, rgba)), files: None }
    }

    /// Clears all clipboard contents.
    pub fn clear(&mut self) {
        self.text = None;
        self.image = None;
        self.files = None;
    }
}
