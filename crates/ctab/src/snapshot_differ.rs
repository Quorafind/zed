//! Snapshot-based Completion Differ for Ctab
//!
//! This module provides precise diff extraction using BufferSnapshot,
//! similar to Zeta's approach. It uses `language::text_diff` for accurate
//! comparison and supports proper anchor-based edit ranges.

use std::ops::Range;
use std::sync::Arc;

use language::{Anchor, BufferSnapshot, Point, text_diff};

/// Result of snapshot-based diff extraction
#[derive(Debug, Clone)]
pub struct SnapshotDiffResult {
    /// Edits to apply: (range in buffer, new text)
    pub edits: Vec<(Range<Anchor>, Arc<str>)>,
    /// Applied optimizations for debugging
    pub optimizations: Vec<String>,
    /// Confidence score (0.0 - 1.0)
    pub confidence: f64,
}

/// Snapshot-based completion differ
///
/// Uses BufferSnapshot for precise diff calculation, following Zeta's pattern:
/// 1. Extract the relevant text range from buffer
/// 2. Use `text_diff` to compute precise edits
/// 3. Refine edit boundaries using prefix/suffix matching
pub struct SnapshotDiffer;

impl SnapshotDiffer {
    pub fn new() -> Self {
        Self
    }

    /// Extract completion edits using snapshot-based diff
    ///
    /// # Arguments
    /// * `snapshot` - Buffer snapshot for precise text access
    /// * `cursor_offset` - Current cursor position (offset)
    /// * `api_response` - Raw completion text from API
    /// * `api_range` - Optional (start_line, end_line) from API (1-indexed)
    ///
    /// # Returns
    /// Precise edits with anchor-based ranges
    #[allow(dead_code)]
    pub fn extract_edits(
        &self,
        snapshot: &BufferSnapshot,
        cursor_offset: usize,
        api_response: &str,
        api_range: Option<(i32, i32)>,
    ) -> SnapshotDiffResult {
        let mut optimizations = Vec::new();

        // Determine the range to compare
        let (old_range, old_text) = if let Some((start_line_1, end_line_1)) = api_range {
            // API provided a range - extract that range's text
            let start_line_0 = (start_line_1.max(1) - 1) as u32;
            let end_line_0 = (end_line_1.max(1) - 1) as u32;

            let start_offset = snapshot.point_to_offset(Point::new(start_line_0, 0));
            let end_offset =
                snapshot.point_to_offset(Point::new(end_line_0, snapshot.line_len(end_line_0)));

            let text: String = snapshot.text_for_range(start_offset..end_offset).collect();
            optimizations.push(format!("api_range_{}_{}", start_line_0, end_line_0));

            (start_offset..end_offset, text)
        } else {
            // No range - use cursor line to end of current context
            let cursor_point = snapshot.offset_to_point(cursor_offset);

            // Extend to cover potential multi-line completion
            let max_line = snapshot.max_point().row;
            let context_end_line = (cursor_point.row + 10).min(max_line);
            let context_end = snapshot.point_to_offset(Point::new(
                context_end_line,
                snapshot.line_len(context_end_line),
            ));

            let text: String = snapshot
                .text_for_range(cursor_offset..context_end)
                .collect();
            optimizations.push("cursor_context".to_string());

            (cursor_offset..context_end, text)
        };

        // Process API response - handle leading newline
        let mut new_text = api_response.to_string();
        if new_text.starts_with('\n') {
            new_text = new_text[1..].to_string();
            optimizations.push("removed_leading_newline".to_string());
        } else if new_text.starts_with("\r\n") {
            new_text = new_text[2..].to_string();
            optimizations.push("removed_leading_crlf".to_string());
        }

        // Use text_diff for precise comparison
        let raw_edits = text_diff(&old_text, &new_text);

        if raw_edits.is_empty() {
            // No changes needed
            return SnapshotDiffResult {
                edits: Vec::new(),
                optimizations,
                confidence: 1.0,
            };
        }

        // Convert raw edits to anchor-based edits with boundary refinement
        let edits = self.refine_edits(snapshot, old_range.start, raw_edits, &mut optimizations);

        let confidence = if edits.is_empty() { 0.5 } else { 0.9 };

        SnapshotDiffResult {
            edits,
            optimizations,
            confidence,
        }
    }

    /// Refine edits using prefix/suffix matching (Zeta's approach)
    ///
    /// This shrinks edit boundaries to the minimal required change,
    /// avoiding unnecessary replacements of identical content.
    #[allow(dead_code)]
    fn refine_edits(
        &self,
        snapshot: &BufferSnapshot,
        base_offset: usize,
        raw_edits: Vec<(Range<usize>, Arc<str>)>,
        optimizations: &mut Vec<String>,
    ) -> Vec<(Range<Anchor>, Arc<str>)> {
        raw_edits
            .into_iter()
            .filter_map(|(mut range, new_text)| {
                // Adjust range to absolute buffer offsets
                range.start += base_offset;
                range.end += base_offset;

                // Compute common prefix length
                let prefix_len =
                    common_prefix(snapshot.chars_for_range(range.clone()), new_text.chars());
                range.start += prefix_len;

                // Compute common suffix length (from the remaining text)
                let suffix_len = common_prefix(
                    snapshot.reversed_chars_for_range(range.clone()),
                    new_text[prefix_len..].chars().rev(),
                );
                range.end = range.end.saturating_sub(suffix_len);

                // Extract the actual new text (minus matched prefix/suffix)
                let refined_text: Arc<str> = if prefix_len + suffix_len >= new_text.len() {
                    "".into()
                } else {
                    new_text[prefix_len..new_text.len() - suffix_len].into()
                };

                // Skip no-op edits
                if range.is_empty() && refined_text.is_empty() {
                    return None;
                }

                if prefix_len > 0 || suffix_len > 0 {
                    optimizations.push(format!(
                        "refined_prefix_{}_suffix_{}",
                        prefix_len, suffix_len
                    ));
                }

                // Convert to anchor-based range
                let anchor_range = if range.is_empty() {
                    let anchor = snapshot.anchor_after(range.start);
                    anchor..anchor
                } else {
                    snapshot.anchor_after(range.start)..snapshot.anchor_before(range.end)
                };

                Some((anchor_range, refined_text))
            })
            .collect()
    }

    /// Extract edits for inline completion (ghost text)
    ///
    /// This handles two cases:
    /// 1. If api_range is provided: Replace the specified line range with the completion
    /// 2. Otherwise: Insert at cursor position with overlap detection
    pub fn extract_inline_edits(
        &self,
        snapshot: &BufferSnapshot,
        cursor_offset: usize,
        cursor_point: Point,
        api_response: &str,
        api_range: Option<(i32, i32)>,
    ) -> SnapshotDiffResult {
        log::debug!(
            "SnapshotDiffer: extract_inline_edits START - cursor_offset={}, cursor_point=({},{}), api_response_len={}, api_range={:?}",
            cursor_offset,
            cursor_point.row,
            cursor_point.column,
            api_response.len(),
            api_range
        );

        let mut optimizations = Vec::new();

        // Process API response - handle leading newline
        let mut insert_text = api_response.to_string();
        if insert_text.starts_with('\n') {
            insert_text = insert_text[1..].to_string();
            optimizations.push("removed_leading_newline".to_string());
        } else if insert_text.starts_with("\r\n") {
            insert_text = insert_text[2..].to_string();
            optimizations.push("removed_leading_crlf".to_string());
        }

        if insert_text.is_empty() {
            return SnapshotDiffResult {
                edits: Vec::new(),
                optimizations,
                confidence: 0.0,
            };
        }

        // CASE 1: API provided a range_to_replace - use it for line-level replacement
        if let Some((start_line_1, end_line_1)) = api_range {
            // Convert 1-indexed lines to 0-indexed
            let start_line_0 = (start_line_1.max(1) - 1) as u32;
            let end_line_0 = (end_line_1.max(1) - 1) as u32;

            log::info!(
                "SnapshotDiffer: Using api_range for replacement: lines {}-{} (0-indexed: {}-{})",
                start_line_1,
                end_line_1,
                start_line_0,
                end_line_0
            );

            // Validate line numbers
            // Allow start_line to point one past the last line (append mode)
            let max_line = snapshot.max_point().row;
            if start_line_0 > max_line + 1 {
                log::warn!(
                    "SnapshotDiffer: start_line {} exceeds max_line + 1 ({})",
                    start_line_0,
                    max_line
                );
                return SnapshotDiffResult {
                    edits: Vec::new(),
                    optimizations,
                    confidence: 0.0,
                };
            }

            // Handle INSERTION case: start_line > end_line means insert new lines
            // API returns (N, N-1) to indicate "insert before line N"
            // We implement this by inserting at the END of line (end_line - 1) with a leading newline
            if start_line_1 > end_line_1 {
                log::info!(
                    "SnapshotDiffer: Detected INSERTION (start > end): inserting new lines before line {}",
                    start_line_1
                );

                // For insertion, we insert at the end of the line BEFORE where we want the new content
                // If end_line_0 is the target, we insert at the end of end_line_0
                let insert_line = end_line_0.min(max_line);
                let insert_offset = snapshot
                    .point_to_offset(Point::new(insert_line, snapshot.line_len(insert_line)));

                // Prepend newline to the insert text since we're appending to end of previous line
                let text_with_newline = format!("\n{}", insert_text);

                log::info!(
                    "SnapshotDiffer: Inserting at end of line {} (offset {}), text_len={}",
                    insert_line,
                    insert_offset,
                    text_with_newline.len()
                );
                log::info!(
                    "SnapshotDiffer: Insert text preview: {:?}",
                    text_with_newline.chars().take(100).collect::<String>()
                );

                optimizations.push(format!("api_range_insert_after_line_{}", insert_line));

                // Create an insertion edit (empty range)
                let anchor = snapshot.anchor_after(insert_offset);
                let edits = vec![(
                    anchor.clone()..anchor,
                    Arc::from(text_with_newline.as_str()),
                )];

                return SnapshotDiffResult {
                    edits,
                    optimizations,
                    confidence: 0.95,
                };
            }

            let end_line_clamped = end_line_0.min(max_line);

            // Calculate the range to replace - FROM LINE START to LINE END
            // This is a FULL LINE replacement, replacing everything including what user typed
            // If start_line exceeds existing lines, use EOF as the start offset (append mode)
            let replace_start = if start_line_0 > max_line {
                snapshot.len()
            } else {
                snapshot.point_to_offset(Point::new(start_line_0, 0))
            };

            let replace_end = snapshot.point_to_offset(Point::new(
                end_line_clamped,
                snapshot.line_len(end_line_clamped),
            ));

            log::info!(
                "SnapshotDiffer: Replacing FULL line range {}..{} (lines {} to {})",
                replace_start,
                replace_end,
                start_line_0,
                end_line_clamped
            );

            // Get the text that will be replaced for logging
            let replaced_text: String = snapshot
                .text_for_range(replace_start..replace_end)
                .collect();
            log::info!(
                "SnapshotDiffer: Text being replaced (len={}): {:?}",
                replaced_text.len(),
                replaced_text.chars().take(100).collect::<String>()
            );
            log::info!(
                "SnapshotDiffer: Replacement text (len={}): {:?}",
                insert_text.len(),
                insert_text.chars().take(100).collect::<String>()
            );

            optimizations.push(format!(
                "api_range_replace_lines_{}_to_{}",
                start_line_0, end_line_clamped
            ));

            // Create the replacement edit
            let start_anchor = snapshot.anchor_after(replace_start);
            let end_anchor = snapshot.anchor_before(replace_end);
            let edits = vec![(start_anchor..end_anchor, Arc::from(insert_text.as_str()))];

            return SnapshotDiffResult {
                edits,
                optimizations,
                confidence: 0.95, // High confidence when API provides explicit range
            };
        }

        // CASE 2: No api_range - use overlap detection for insertion
        log::debug!("SnapshotDiffer: No api_range, using overlap detection");

        // Get a limited amount of text after cursor for overlap detection
        let max_line = snapshot.max_point().row;
        let context_end_line = (cursor_point.row + 3).min(max_line);
        let context_end = snapshot.point_to_offset(Point::new(
            context_end_line,
            snapshot.line_len(context_end_line),
        ));

        // Limit to 512 bytes max
        let limited_end = (cursor_offset + 512).min(context_end);
        let after_cursor: String = snapshot
            .text_for_range(cursor_offset..limited_end)
            .collect();
        log::debug!(
            "SnapshotDiffer: after_cursor_len={}, content={:?}",
            after_cursor.len(),
            after_cursor.chars().take(50).collect::<String>()
        );

        // Track how much of after_cursor should be replaced (deleted)
        let mut replace_len: usize = 0;

        // Detect and handle trailing overlap with after_cursor
        if !after_cursor.is_empty() {
            log::debug!("SnapshotDiffer: detecting trailing overlap");
            let overlap = self.detect_content_overlap(&insert_text, &after_cursor);
            log::debug!("SnapshotDiffer: trailing overlap={}", overlap);
            if overlap > 0 {
                insert_text.truncate(insert_text.len() - overlap);
                replace_len = replace_len.max(overlap);
                optimizations.push(format!("trailing_overlap_replace_{}", overlap));
            }

            // Detect prefix overlap
            let prefix_overlap = self.detect_prefix_overlap(&insert_text, &after_cursor);
            log::debug!("SnapshotDiffer: prefix overlap={}", prefix_overlap);
            if prefix_overlap > 0 {
                replace_len = replace_len.max(prefix_overlap);
                optimizations.push(format!("prefix_overlap_replace_{}", prefix_overlap));
            }

            // Check for content-level duplicates
            log::debug!("SnapshotDiffer: checking duplicate lines");
            let removed = self.remove_duplicate_lines(&mut insert_text, &after_cursor);
            log::debug!("SnapshotDiffer: removed {} duplicate lines", removed);
            if removed > 0 {
                optimizations.push(format!("removed_{}_duplicate_lines", removed));
            }
        }

        // Detect leading overlap with content before cursor
        let before_cursor: String = if cursor_point.column > 0 {
            let line_start = snapshot.point_to_offset(Point::new(cursor_point.row, 0));
            snapshot.text_for_range(line_start..cursor_offset).collect()
        } else {
            String::new()
        };
        log::debug!("SnapshotDiffer: before_cursor_len={}", before_cursor.len());

        if !before_cursor.is_empty() {
            log::debug!("SnapshotDiffer: detecting leading overlap");
            let leading_overlap = self.detect_leading_overlap(&before_cursor, &insert_text);
            log::debug!("SnapshotDiffer: leading overlap={}", leading_overlap);
            if leading_overlap > 0 {
                insert_text = insert_text[leading_overlap..].to_string();
                optimizations.push(format!("removed_leading_overlap_{}", leading_overlap));
            }
        }

        if insert_text.is_empty() && replace_len == 0 {
            return SnapshotDiffResult {
                edits: Vec::new(),
                optimizations,
                confidence: 0.5,
            };
        }

        // Create edit: either insertion or replacement
        log::debug!(
            "SnapshotDiffer: creating edit at offset={}, replace_len={}",
            cursor_offset,
            replace_len
        );

        let start_anchor = snapshot.anchor_after(cursor_offset);
        let end_anchor = if replace_len > 0 {
            snapshot.anchor_before(cursor_offset + replace_len)
        } else {
            start_anchor.clone()
        };

        let edits = vec![(start_anchor..end_anchor, Arc::from(insert_text.as_str()))];

        log::debug!(
            "SnapshotDiffer: extract_inline_edits END - edits_count={}",
            edits.len()
        );
        SnapshotDiffResult {
            edits,
            optimizations,
            confidence: 0.85,
        }
    }

    /// Detect overlap between end of insert_text and start of after_cursor
    ///
    /// Limited to MAX_OVERLAP_CHECK bytes to prevent O(n²) performance issues.
    fn detect_content_overlap(&self, insert_text: &str, after_cursor: &str) -> usize {
        const MAX_OVERLAP_CHECK: usize = 256; // Limit check to prevent hang

        let insert_bytes = insert_text.as_bytes();
        let after_bytes = after_cursor.as_bytes();
        let max_overlap = insert_bytes
            .len()
            .min(after_bytes.len())
            .min(MAX_OVERLAP_CHECK);

        if max_overlap == 0 {
            return 0;
        }

        // Try exact suffix-prefix match
        for len in (1..=max_overlap).rev() {
            let insert_suffix = &insert_bytes[insert_bytes.len() - len..];
            let after_prefix = &after_bytes[..len];
            if insert_suffix == after_prefix {
                return len;
            }
        }

        // Try trimmed match for newline variations
        let after_trimmed = after_cursor.trim_start_matches(|c| c == '\n' || c == '\r');
        if !after_trimmed.is_empty() && after_trimmed != after_cursor {
            let after_trimmed_bytes = after_trimmed.as_bytes();
            let max_trimmed = insert_bytes
                .len()
                .min(after_trimmed_bytes.len())
                .min(MAX_OVERLAP_CHECK);

            for len in (1..=max_trimmed).rev() {
                let insert_suffix = &insert_bytes[insert_bytes.len() - len..];
                let after_prefix = &after_trimmed_bytes[..len];
                if insert_suffix == after_prefix {
                    return len;
                }
            }
        }

        0
    }

    /// Detect overlap between end of before_cursor and start of insert_text
    ///
    /// Limited to MAX_OVERLAP_CHECK bytes to prevent O(n²) performance issues.
    fn detect_leading_overlap(&self, before_cursor: &str, insert_text: &str) -> usize {
        const MAX_OVERLAP_CHECK: usize = 256; // Limit check to prevent hang

        let before_bytes = before_cursor.as_bytes();
        let insert_bytes = insert_text.as_bytes();
        let max_overlap = before_bytes
            .len()
            .min(insert_bytes.len())
            .min(MAX_OVERLAP_CHECK);

        if max_overlap < 2 {
            return 0;
        }

        for len in (2..=max_overlap).rev() {
            let before_suffix = &before_bytes[before_bytes.len() - len..];
            let insert_prefix = &insert_bytes[..len];
            if before_suffix == insert_prefix {
                // Verify we're not cutting in the middle of a UTF-8 char
                if insert_text.is_char_boundary(len) {
                    return len;
                }
            }
        }

        0
    }

    /// Detect prefix overlap: how much of insert_text's START matches after_cursor's START
    ///
    /// This handles the case where API returns content that partially overlaps
    /// with what's already after the cursor. For example:
    /// - insert_text: "console.log(x);"
    /// - after_cursor: "console"
    /// - Result: 7 (we need to replace "console" with "console.log(x);")
    ///
    /// Limited to MAX_OVERLAP_CHECK bytes to prevent O(n²) performance issues.
    fn detect_prefix_overlap(&self, insert_text: &str, after_cursor: &str) -> usize {
        const MAX_OVERLAP_CHECK: usize = 256;

        let insert_bytes = insert_text.as_bytes();
        let after_bytes = after_cursor.as_bytes();

        // Find the longest common prefix
        let max_len = insert_bytes
            .len()
            .min(after_bytes.len())
            .min(MAX_OVERLAP_CHECK);

        if max_len == 0 {
            return 0;
        }

        let mut common_len = 0;
        for i in 0..max_len {
            if insert_bytes[i] == after_bytes[i] {
                common_len = i + 1;
            } else {
                break;
            }
        }

        // Only return if we have a meaningful overlap (at least 2 chars)
        // and it ends at a reasonable boundary (not in the middle of a word)
        if common_len >= 2 {
            // Verify we're at a UTF-8 char boundary
            if insert_text.is_char_boundary(common_len) {
                return common_len;
            }
        }

        0
    }

    /// Remove lines from insert_text that already exist in after_cursor
    fn remove_duplicate_lines(&self, insert_text: &mut String, after_cursor: &str) -> usize {
        use std::collections::HashSet;

        let after_lines_set: HashSet<&str> = after_cursor
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty() && l.len() > 2) // Skip short structural lines
            .collect();

        if after_lines_set.is_empty() {
            return 0;
        }

        let original_lines: Vec<&str> = insert_text.lines().collect();
        let original_count = original_lines.len();

        let filtered_lines: Vec<&str> = original_lines
            .into_iter()
            .filter(|line| {
                let trimmed = line.trim();
                // Keep empty lines and short structural lines
                if trimmed.is_empty() || trimmed.len() <= 2 {
                    return true;
                }
                // Remove if exact match in after_cursor
                !after_lines_set.contains(trimmed)
            })
            .collect();

        let removed = original_count - filtered_lines.len();
        if removed > 0 {
            *insert_text = filtered_lines.join("\n");
        }

        removed
    }
}

impl Default for SnapshotDiffer {
    fn default() -> Self {
        Self::new()
    }
}

/// Interpolate edits from old snapshot to new snapshot
///
/// This function adjusts pre-computed edits to account for user edits that
/// happened between when the completion was computed and when it's being displayed.
///
/// Following Zeta's approach:
/// - If user typed text that matches the beginning of the predicted text, shrink the prediction
/// - If user's edit conflicts with the prediction, return None to discard the prediction
///
/// # Arguments
/// * `old_snapshot` - The buffer snapshot when edits were originally computed
/// * `new_snapshot` - The current buffer snapshot
/// * `current_edits` - The pre-computed edits to interpolate
///
/// # Returns
/// * `Some(edits)` - Adjusted edits that account for user changes
/// * `None` - If the prediction is no longer valid due to conflicting edits
pub fn interpolate_edits(
    old_snapshot: &BufferSnapshot,
    new_snapshot: &BufferSnapshot,
    current_edits: &[(Range<Anchor>, Arc<str>)],
) -> Option<Vec<(Range<Anchor>, Arc<str>)>> {
    use language::ToOffset;

    // If snapshots are the same version, no interpolation needed
    if old_snapshot.version() == new_snapshot.version() {
        return Some(current_edits.to_vec());
    }

    let mut result_edits = Vec::new();
    let mut model_edits = current_edits.iter().peekable();

    // Process each user edit since the old snapshot
    for user_edit in new_snapshot.edits_since::<usize>(old_snapshot.version()) {
        // First, emit any model edits that come entirely before this user edit
        while let Some((model_range, _)) = model_edits.peek() {
            let model_old_range =
                model_range.start.to_offset(old_snapshot)..model_range.end.to_offset(old_snapshot);

            if model_old_range.end <= user_edit.old.start {
                // This model edit is entirely before the user edit, keep it
                let (range, text) = model_edits.next().unwrap();
                result_edits.push((range.clone(), text.clone()));
            } else {
                break;
            }
        }

        // Check if any model edit overlaps with this user edit
        if let Some((model_range, model_new_text)) = model_edits.peek() {
            let model_old_range =
                model_range.start.to_offset(old_snapshot)..model_range.end.to_offset(old_snapshot);

            // Check if user edit matches the model edit's range
            if user_edit.old == model_old_range {
                // User edited the exact same range - check if they typed a prefix
                let user_new_text: String =
                    new_snapshot.text_for_range(user_edit.new.clone()).collect();

                if let Some(model_suffix) = model_new_text.strip_prefix(&user_new_text) {
                    // User typed a prefix of what we predicted!
                    // Shrink the prediction to just the remaining suffix
                    if !model_suffix.is_empty() {
                        // Create an insertion point at the end of what user typed
                        let anchor = new_snapshot.anchor_after(user_edit.new.end);
                        result_edits.push((anchor.clone()..anchor, model_suffix.into()));
                    }
                    model_edits.next();
                    continue;
                }
            }

            // Check for partial overlap or conflict
            if model_old_range.start < user_edit.old.end
                && model_old_range.end > user_edit.old.start
            {
                // Overlapping edit that doesn't match - prediction is invalid
                log::debug!(
                    "interpolate_edits: Conflict detected - user_edit={:?}, model_range={:?}",
                    user_edit.old,
                    model_old_range
                );
                return None;
            }
        }
    }

    // Add any remaining model edits
    result_edits.extend(model_edits.cloned());

    if result_edits.is_empty() {
        None
    } else {
        Some(result_edits)
    }
}

/// Compute common prefix length in bytes between two char iterators
#[allow(dead_code)]
fn common_prefix<T1, T2>(a: T1, b: T2) -> usize
where
    T1: Iterator<Item = char>,
    T2: Iterator<Item = char>,
{
    a.zip(b)
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a.len_utf8())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_content_overlap() {
        let differ = SnapshotDiffer::new();

        // Exact match
        assert_eq!(differ.detect_content_overlap("hello}", "}world"), 1);

        // Longer overlap
        assert_eq!(differ.detect_content_overlap("foo\nbar", "\nbar\nbaz"), 4);

        // No overlap
        assert_eq!(differ.detect_content_overlap("hello", "world"), 0);

        // Trimmed match with newline
        assert_eq!(differ.detect_content_overlap("}", "\n}"), 1);
    }

    #[test]
    fn test_detect_leading_overlap() {
        let differ = SnapshotDiffer::new();

        // Partial word overlap
        assert_eq!(differ.detect_leading_overlap("console", "console.log"), 7);

        // No overlap (too short)
        assert_eq!(differ.detect_leading_overlap("x", "x + 1"), 0);

        // Statement overlap
        assert_eq!(
            differ.detect_leading_overlap("newTD();", "newTD();\nmore"),
            8
        );
    }

    #[test]
    fn test_remove_duplicate_lines() {
        let differ = SnapshotDiffer::new();

        let mut text = "console.log(x);\n  return result;\n  cleanup();".to_string();
        let after = "\n  return result;\n}\n";
        let removed = differ.remove_duplicate_lines(&mut text, after);

        assert_eq!(removed, 1);
        assert!(!text.contains("return result"));
        assert!(text.contains("console.log"));
        assert!(text.contains("cleanup"));
    }

    #[test]
    fn test_large_text_performance() {
        let differ = SnapshotDiffer::new();

        // Simulate real scenario: large insert_text and after_cursor
        let insert_text = "function merge(a: number[], b: number[]): number[] {\n\
            const result = [];\n\
            let i = 0;\n\
            let j = 0;\n\
            while (i < a.length && j < b.length) {\n\
                if (a[i] < b[j]) {\n\
                    result.push(a[i]);\n\
                    i++;\n\
                } else {\n\
                    result.push(b[j]);\n\
                    j++;\n\
                }\n\
            }\n\
            return result.concat(a.slice(i)).concat(b.slice(j));\n\
        }";

        let after_cursor = "\n\
            return result.concat(a.slice(i)).concat(b.slice(j));\n\
        }\n\
        \n\
        function quickSort(arr: number[]): number[] {\n\
            if (arr.length <= 1) return arr;\n\
            const pivot = arr[0];\n\
        }";

        // This should complete quickly (< 100ms)
        let start = std::time::Instant::now();
        let overlap = differ.detect_content_overlap(insert_text, after_cursor);
        let duration = start.elapsed();

        println!(
            "detect_content_overlap took {:?}, overlap={}",
            duration, overlap
        );
        assert!(
            duration.as_millis() < 100,
            "detect_content_overlap took too long: {:?}",
            duration
        );
    }

    #[test]
    fn test_very_large_text_performance() {
        let differ = SnapshotDiffer::new();

        // Create very large strings (10KB each)
        let large_text: String = (0..500)
            .map(|i| format!("  console.log('line {}');\n", i))
            .collect();

        let after_cursor: String = (0..500)
            .map(|i| format!("  console.log('after {}');\n", i))
            .collect();

        // This should still complete quickly due to MAX_OVERLAP_CHECK limit
        let start = std::time::Instant::now();
        let overlap = differ.detect_content_overlap(&large_text, &after_cursor);
        let duration = start.elapsed();

        println!(
            "Large text detect_content_overlap took {:?}, overlap={}",
            duration, overlap
        );
        assert!(
            duration.as_millis() < 100,
            "detect_content_overlap took too long: {:?}",
            duration
        );

        // Test leading overlap too
        let start = std::time::Instant::now();
        let leading = differ.detect_leading_overlap(&large_text, &after_cursor);
        let duration = start.elapsed();

        println!(
            "Large text detect_leading_overlap took {:?}, leading={}",
            duration, leading
        );
        assert!(
            duration.as_millis() < 100,
            "detect_leading_overlap took too long: {:?}",
            duration
        );

        // Test remove_duplicate_lines
        let mut text_copy = large_text.clone();
        let start = std::time::Instant::now();
        let removed = differ.remove_duplicate_lines(&mut text_copy, &after_cursor);
        let duration = start.elapsed();

        println!(
            "Large text remove_duplicate_lines took {:?}, removed={}",
            duration, removed
        );
        assert!(
            duration.as_millis() < 100,
            "remove_duplicate_lines took too long: {:?}",
            duration
        );
    }
}
