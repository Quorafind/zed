//! Edit history and diff tracking for Cometix
//!
//! This module tracks file changes and builds diff history strings
//! that the Cursor API expects for context.

use collections::HashMap;
use similar::{ChangeTag, TextDiff};

/// Maximum number of diff entries to keep per file
const MAX_DIFF_HISTORY_ENTRIES: usize = 5;

/// Tracks edit history and generates diff strings for the Cursor API
pub struct DiffTracker {
    file_states: HashMap<String, FileState>,
}

struct FileState {
    last_content: String,
    recent_diffs: Vec<String>,
    version: i32,
}

impl DiffTracker {
    pub fn new() -> Self {
        Self {
            file_states: HashMap::default(),
        }
    }

    /// Get the current file version for a path
    pub fn get_file_version(&self, path: &str) -> i32 {
        self.file_states.get(path).map(|s| s.version).unwrap_or(1)
    }

    /// Build diff history string in Cursor API format
    ///
    /// Format: `{line_number}{+/-}|{content}\n`
    /// - `+` indicates an added line
    /// - `-` indicates a removed line
    pub fn build_diff_history(&mut self, file_path: &str, new_content: &str) -> String {
        let state = self
            .file_states
            .entry(file_path.to_string())
            .or_insert_with(|| FileState {
                last_content: new_content.to_string(),
                recent_diffs: Vec::new(),
                version: 1,
            });

        // If content hasn't changed, return empty string
        if state.last_content == new_content {
            return String::new();
        }

        // Compute line-level diff
        let new_content_string = new_content.to_string();
        let diff = TextDiff::from_lines(&state.last_content, &new_content_string);
        let mut result = String::new();
        let mut old_line = 1u32;
        let mut new_line = 1u32;

        for change in diff.iter_all_changes() {
            match change.tag() {
                ChangeTag::Delete => {
                    // Format: {line_number}-|{content}
                    result.push_str(&format!("{}-|{}\n", old_line, change.value().trim_end()));
                    old_line += 1;
                }
                ChangeTag::Insert => {
                    // Format: {line_number}+|{content}
                    result.push_str(&format!("{}+|{}\n", new_line, change.value().trim_end()));
                    new_line += 1;
                }
                ChangeTag::Equal => {
                    old_line += 1;
                    new_line += 1;
                }
            }
        }

        // Update state
        state.last_content = new_content.to_string();
        state.version += 1;

        if !result.is_empty() {
            state.recent_diffs.push(result.clone());
            if state.recent_diffs.len() > MAX_DIFF_HISTORY_ENTRIES {
                state.recent_diffs.remove(0);
            }
        }

        result
    }

    /// Get all recent diffs for a file
    #[allow(dead_code)]
    pub fn get_recent_diffs(&self, file_path: &str) -> Vec<String> {
        self.file_states
            .get(file_path)
            .map(|s| s.recent_diffs.clone())
            .unwrap_or_default()
    }

    /// Clear tracking state for a file
    #[allow(dead_code)]
    pub fn clear_file(&mut self, file_path: &str) {
        self.file_states.remove(file_path);
    }

    /// Clear all tracking state
    #[allow(dead_code)]
    pub fn clear_all(&mut self) {
        self.file_states.clear();
    }
}

impl Default for DiffTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_diff_tracker_basic() {
        let mut tracker = DiffTracker::new();

        // First content - no diff
        let diff = tracker.build_diff_history("test.rs", "line 1\nline 2\n");
        assert!(diff.is_empty());

        // Add a line
        let diff = tracker.build_diff_history("test.rs", "line 1\nline 2\nline 3\n");
        assert!(diff.contains("3+|line 3"));

        // Remove a line
        let diff = tracker.build_diff_history("test.rs", "line 1\nline 3\n");
        assert!(diff.contains("2-|line 2"));
    }

    #[test]
    fn test_version_tracking() {
        let mut tracker = DiffTracker::new();

        assert_eq!(tracker.get_file_version("test.rs"), 1);

        tracker.build_diff_history("test.rs", "content");
        assert_eq!(tracker.get_file_version("test.rs"), 1);

        tracker.build_diff_history("test.rs", "new content");
        assert_eq!(tracker.get_file_version("test.rs"), 2);
    }
}
