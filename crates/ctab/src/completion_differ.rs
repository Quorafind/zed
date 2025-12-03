//! Smart Completion Differ for Cometix
//!
//! This module provides intelligent diff extraction for code completions,
//! ensuring that API-returned completion text is properly processed to avoid
//! issues like duplicated content, missing newlines, or incorrect range handling.
//!
//! Ported from the VSCode Cometix-Tab extension's SmartCompletionDiffer.

use std::collections::HashSet;

/// Completion context information for diff extraction
#[derive(Debug, Clone)]
pub struct CompletionContext {
    /// Text before the cursor position
    pub before_cursor: String,
    /// Text after the cursor position
    pub after_cursor: String,
    /// Current line's complete text
    pub current_line: String,
    /// Programming language identifier
    pub language: String,
    /// Current line's indentation string
    pub indentation: String,
    /// Cursor row (0-indexed)
    pub cursor_row: u32,
    /// Cursor column (0-indexed)
    pub cursor_col: u32,
}

/// Content type classification for choosing diff strategy
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ContentType {
    /// Partial word completion (e.g., "con" -> "console")
    PartialWord,
    /// Complete word completion
    CompleteWord,
    /// Multi-line structure completion
    MultiLine,
    /// Expression completion
    Expression,
    /// Block structure completion (functions, classes, etc.)
    BlockStructure,
    /// Unknown type
    Unknown,
}

/// Diff extraction method used
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DiffMethod {
    /// Character-level diff
    CharacterDiff,
    /// Word-level diff
    WordDiff,
    /// Line-level diff
    LineDiff,
    /// Hybrid strategy
    Hybrid,
    /// Simple prefix matching (fallback)
    PrefixMatch,
    /// Original content (final fallback)
    Original,
}

/// Result of diff extraction
#[derive(Debug, Clone)]
pub struct DiffExtractionResult {
    /// Text to insert
    pub insert_text: String,
    /// Algorithm confidence (0.0 - 1.0)
    pub confidence: f64,
    /// Method used for extraction
    pub method: DiffMethod,
    /// Applied optimizations
    pub optimizations: Vec<String>,
    /// Optional: start line for range replacement (0-indexed)
    pub start_line: Option<u32>,
    /// Optional: end line for range replacement (0-indexed, inclusive)
    pub end_line: Option<u32>,
}

/// Smart Completion Differ
///
/// Provides intelligent extraction of completion text to ensure correct
/// insertion behavior, handling issues like:
/// - Duplicate content with existing file content
/// - Leading/trailing newline handling
/// - Multi-line completion with single-line API range
pub struct SmartCompletionDiffer;

impl SmartCompletionDiffer {
    /// Create a new SmartCompletionDiffer instance
    pub fn new() -> Self {
        Self
    }

    /// Main entry point: extract completion diff
    ///
    /// # Arguments
    /// * `context` - Completion context with cursor position and surrounding text
    /// * `api_response` - Raw completion text from API
    /// * `api_range` - Optional (start_line, end_line) from API (1-indexed)
    ///
    /// # Returns
    /// Optimized diff extraction result
    pub fn extract_completion_diff(
        &self,
        context: &CompletionContext,
        api_response: &str,
        api_range: Option<(i32, i32)>,
    ) -> DiffExtractionResult {
        // 1. Analyze content type
        let content_type = self.analyze_content_type(context, api_response);
        log::debug!(
            "Cometix differ: content type = {:?}, api_range = {:?}",
            content_type,
            api_range
        );

        // 2. Execute optimal strategy based on content type and API range
        let result = if api_range.is_some() {
            // API provided a range - use range-aware extraction
            self.extract_with_range(context, api_response, api_range.unwrap(), content_type)
        } else {
            // No range - use content-type based extraction
            self.extract_without_range(context, api_response, content_type)
        };

        log::debug!(
            "Cometix differ: result confidence={:.3}, method={:?}, insert_len={}",
            result.confidence,
            result.method,
            result.insert_text.len()
        );

        result
    }

    /// Extract completion when API provides a range
    fn extract_with_range(
        &self,
        context: &CompletionContext,
        api_response: &str,
        api_range: (i32, i32),
        content_type: ContentType,
    ) -> DiffExtractionResult {
        let (start_line_1, end_line_1) = api_range;
        // Ensure valid range: start <= end, both >= 1
        let start_line_1_clamped = start_line_1.max(1);
        let end_line_1_clamped = end_line_1.max(start_line_1_clamped);
        let start_line_0 = (start_line_1_clamped - 1) as u32;
        let end_line_0 = (end_line_1_clamped - 1) as u32;

        let mut insert_text = api_response.to_string();
        let mut optimizations = Vec::new();

        // Strategy 1: Handle leading newline
        // When API returns range for current line but text starts with \n,
        // the \n is likely meant to indicate "insert after current line content"
        if insert_text.starts_with('\n') {
            insert_text = insert_text[1..].to_string();
            optimizations.push("removed_leading_newline".to_string());
        } else if insert_text.starts_with("\r\n") {
            insert_text = insert_text[2..].to_string();
            optimizations.push("removed_leading_crlf".to_string());
        }

        // Strategy 2: Detect and remove LEADING overlap with before_cursor
        // This handles the case where completion text starts with content that
        // already exists before the cursor (e.g., "newTD();" when "newTD()" is already there)
        if !context.before_cursor.is_empty() && !insert_text.is_empty() {
            let leading_overlap = self.detect_leading_overlap(&context.before_cursor, &insert_text);
            if leading_overlap > 0 {
                insert_text = insert_text[leading_overlap..].to_string();
                optimizations.push(format!("removed_leading_overlap_{}", leading_overlap));
                log::info!(
                    "Cometix differ: Removed leading overlap of {} bytes, new insert_text={:?}",
                    leading_overlap,
                    insert_text
                );
            }
        }

        // Strategy 3: Detect and remove trailing overlap with after_cursor
        // This handles cases where API returns content that overlaps with existing file content
        if !context.after_cursor.is_empty() {
            let overlap_result = self.detect_trailing_overlap(&insert_text, &context.after_cursor);
            if overlap_result.overlap_len > 0 {
                let new_len = insert_text.len() - overlap_result.overlap_len;
                insert_text.truncate(new_len);
                optimizations.push(format!(
                    "removed_trailing_overlap_{}",
                    overlap_result.overlap_len
                ));
            }
        }

        // Strategy 4: For multi-line completions with single-line range,
        // calculate the actual range needed
        let actual_lines = insert_text.lines().count() as u32;
        let api_lines = end_line_0 - start_line_0 + 1;

        let (final_start, final_end) =
            if actual_lines > api_lines && content_type == ContentType::MultiLine {
                // Multi-line completion but API only specified single line
                // We need to extend the range to cover all lines being replaced
                let extended_end =
                    self.calculate_extended_range(context, &insert_text, start_line_0, end_line_0);
                optimizations.push(format!("extended_range_to_{}", extended_end));
                (Some(start_line_0), Some(extended_end))
            } else {
                (Some(start_line_0), Some(end_line_0))
            };

        // Strategy 5: Apply syntax-aware optimizations
        insert_text = self.apply_syntax_optimizations(&insert_text, context, &mut optimizations);

        DiffExtractionResult {
            insert_text,
            confidence: 0.8,
            method: DiffMethod::LineDiff,
            optimizations,
            start_line: final_start,
            end_line: final_end,
        }
    }

    /// Calculate extended range for multi-line completions
    fn calculate_extended_range(
        &self,
        context: &CompletionContext,
        insert_text: &str,
        _start_line: u32,
        original_end_line: u32,
    ) -> u32 {
        // Parse lines from after_cursor to detect overlap
        let after_lines: Vec<&str> = context.after_cursor.lines().collect();
        let insert_lines: Vec<&str> = insert_text.lines().collect();

        if after_lines.is_empty() || insert_lines.is_empty() {
            return original_end_line;
        }

        // Find how many lines after cursor overlap with end of insert_text
        let mut overlap_lines = 0u32;
        for (i, insert_line) in insert_lines.iter().rev().enumerate() {
            if i >= after_lines.len() {
                break;
            }
            let after_line = after_lines[i];
            if insert_line.trim() == after_line.trim() && !insert_line.trim().is_empty() {
                overlap_lines += 1;
            } else {
                break;
            }
        }

        // The extended end should include the overlapping lines from original content
        if overlap_lines > 0 {
            original_end_line + overlap_lines
        } else {
            original_end_line
        }
    }

    /// Extract completion when no API range is provided
    fn extract_without_range(
        &self,
        context: &CompletionContext,
        api_response: &str,
        content_type: ContentType,
    ) -> DiffExtractionResult {
        match content_type {
            ContentType::PartialWord => self.extract_partial_word(context, api_response),
            ContentType::CompleteWord | ContentType::Expression => {
                self.extract_word_or_expression(context, api_response)
            }
            ContentType::MultiLine | ContentType::BlockStructure => {
                self.extract_multiline(context, api_response)
            }
            ContentType::Unknown => self.extract_fallback(context, api_response),
        }
    }

    /// Extract partial word completion
    fn extract_partial_word(
        &self,
        context: &CompletionContext,
        api_response: &str,
    ) -> DiffExtractionResult {
        let before = &context.before_cursor;
        let mut optimizations = Vec::new();

        // Find the partial word being typed
        let partial_word = self.get_last_word(before);
        let mut insert_text = api_response.to_string();

        // Remove common prefix with partial word
        if !partial_word.is_empty() && insert_text.starts_with(&partial_word) {
            insert_text = insert_text[partial_word.len()..].to_string();
            optimizations.push(format!("removed_prefix_{}", partial_word.len()));
        }

        // Handle trailing overlap
        if !context.after_cursor.is_empty() {
            let overlap = self.detect_trailing_overlap(&insert_text, &context.after_cursor);
            if overlap.overlap_len > 0 {
                insert_text.truncate(insert_text.len() - overlap.overlap_len);
                optimizations.push(format!("removed_trailing_{}", overlap.overlap_len));
            }
        }

        DiffExtractionResult {
            insert_text,
            confidence: 0.7,
            method: DiffMethod::CharacterDiff,
            optimizations,
            start_line: None,
            end_line: None,
        }
    }

    /// Extract word or expression completion
    fn extract_word_or_expression(
        &self,
        context: &CompletionContext,
        api_response: &str,
    ) -> DiffExtractionResult {
        let mut insert_text = api_response.to_string();
        let mut optimizations = Vec::new();

        // Remove leading whitespace if context already has trailing whitespace
        if context.before_cursor.ends_with(' ') || context.before_cursor.ends_with('\t') {
            insert_text = insert_text.trim_start().to_string();
            optimizations.push("trimmed_leading_whitespace".to_string());
        }

        // Remove trailing overlap
        if !context.after_cursor.is_empty() {
            let overlap = self.detect_trailing_overlap(&insert_text, &context.after_cursor);
            if overlap.overlap_len > 0 {
                insert_text.truncate(insert_text.len() - overlap.overlap_len);
                optimizations.push(format!("removed_trailing_{}", overlap.overlap_len));
            }
        }

        DiffExtractionResult {
            insert_text,
            confidence: 0.6,
            method: DiffMethod::WordDiff,
            optimizations,
            start_line: None,
            end_line: None,
        }
    }

    /// Extract multi-line completion
    fn extract_multiline(
        &self,
        context: &CompletionContext,
        api_response: &str,
    ) -> DiffExtractionResult {
        let mut insert_text = api_response.to_string();
        let mut optimizations = Vec::new();

        // Handle leading newline
        if insert_text.starts_with('\n') {
            insert_text = insert_text[1..].to_string();
            optimizations.push("removed_leading_newline".to_string());
        } else if insert_text.starts_with("\r\n") {
            insert_text = insert_text[2..].to_string();
            optimizations.push("removed_leading_crlf".to_string());
        }

        // Use line-level diff to detect and remove duplicates
        let before_lines: HashSet<&str> = context.before_cursor.lines().map(|l| l.trim()).collect();

        let original_line_count = insert_text.lines().count();
        let filtered_lines: Vec<String> = insert_text
            .lines()
            .filter(|line| {
                let trimmed = line.trim();
                // Keep line if:
                // - It's not empty, OR
                // - It's not a duplicate of existing content
                trimmed.is_empty() || !before_lines.contains(trimmed)
            })
            .map(|s| s.to_string())
            .collect();

        let filtered_line_count = filtered_lines.len();
        insert_text = filtered_lines.join("\n");

        if filtered_line_count < original_line_count {
            optimizations.push(format!(
                "removed_{}_duplicate_lines",
                original_line_count - filtered_line_count
            ));
        }

        // Detect trailing overlap with after_cursor
        if !context.after_cursor.is_empty() {
            let overlap = self.detect_multiline_overlap(&insert_text, &context.after_cursor);
            if overlap > 0 {
                // Remove overlapping lines from end
                let lines: Vec<&str> = insert_text.lines().collect();
                if lines.len() > overlap {
                    insert_text = lines[..lines.len() - overlap].join("\n");
                    optimizations.push(format!("removed_{}_overlapping_lines", overlap));
                }
            }
        }

        DiffExtractionResult {
            insert_text,
            confidence: 0.65,
            method: DiffMethod::LineDiff,
            optimizations,
            start_line: None,
            end_line: None,
        }
    }

    /// Fallback extraction for unknown content types
    fn extract_fallback(
        &self,
        context: &CompletionContext,
        api_response: &str,
    ) -> DiffExtractionResult {
        let mut insert_text = api_response.to_string();
        let mut optimizations = vec!["fallback_strategy".to_string()];

        // Simple prefix matching
        let common_prefix_len = self.common_prefix_len(&context.before_cursor, api_response);
        if common_prefix_len > 0 {
            insert_text = insert_text[common_prefix_len..].to_string();
            optimizations.push(format!("removed_prefix_{}", common_prefix_len));
        }

        DiffExtractionResult {
            insert_text,
            confidence: 0.3,
            method: DiffMethod::PrefixMatch,
            optimizations,
            start_line: None,
            end_line: None,
        }
    }

    /// Analyze content type of the API response
    fn analyze_content_type(&self, context: &CompletionContext, api_response: &str) -> ContentType {
        // Check for multi-line
        if api_response.contains('\n') || api_response.len() > 100 {
            // Check for block structure
            if self.is_block_structure(api_response) {
                return ContentType::BlockStructure;
            }
            return ContentType::MultiLine;
        }

        // Check for partial word
        if self.is_partial_word_completion(context, api_response) {
            return ContentType::PartialWord;
        }

        // Check for expression
        if self.is_expression(api_response) {
            return ContentType::Expression;
        }

        // Check for complete word
        if self.is_complete_word_completion(context, api_response) {
            return ContentType::CompleteWord;
        }

        ContentType::Unknown
    }

    /// Check if response is a block structure (function, class, etc.)
    fn is_block_structure(&self, text: &str) -> bool {
        let open_braces = text.matches('{').count();
        let close_braces = text.matches('}').count();

        if open_braces > 0 && close_braces > 0 {
            return true;
        }

        // Check for multiple indentation levels
        let lines: Vec<&str> = text.lines().collect();
        if lines.len() > 2 {
            let indent_levels: HashSet<usize> = lines
                .iter()
                .filter(|l| !l.trim().is_empty())
                .map(|l| l.len() - l.trim_start().len())
                .collect();
            if indent_levels.len() > 2 {
                return true;
            }
        }

        false
    }

    /// Check if this is a partial word completion
    fn is_partial_word_completion(&self, context: &CompletionContext, api_response: &str) -> bool {
        let before = context.before_cursor.trim_end();

        // Cursor must be at end of a word (alphanumeric)
        if !before
            .chars()
            .last()
            .map_or(false, |c| c.is_alphanumeric() || c == '_')
        {
            return false;
        }

        // Get the partial word
        let partial = self.get_last_word(before);
        if partial.is_empty() {
            return false;
        }

        // API response should start with this partial word (case-insensitive)
        let response_lower = api_response.to_lowercase();
        let partial_lower = partial.to_lowercase();
        response_lower.starts_with(&partial_lower) && api_response.len() > partial.len()
    }

    /// Check if this is a complete word completion
    fn is_complete_word_completion(&self, context: &CompletionContext, api_response: &str) -> bool {
        let before = context.before_cursor.trim_end();

        // Cursor should be after non-word character
        if before
            .chars()
            .last()
            .map_or(false, |c| c.is_alphanumeric() || c == '_')
        {
            return false;
        }

        // Response should start with word character
        if !api_response
            .trim()
            .chars()
            .next()
            .map_or(false, |c| c.is_alphanumeric() || c == '_')
        {
            return false;
        }

        // Should be single line
        !api_response.contains('\n')
    }

    /// Check if response is an expression
    fn is_expression(&self, text: &str) -> bool {
        // Contains operators
        let has_operators = text.contains('+')
            || text.contains('-')
            || text.contains('*')
            || text.contains('/')
            || text.contains('=')
            || text.contains('<')
            || text.contains('>');

        // Contains function call
        let has_function_call = text.contains('(') && text.contains(')');

        // Contains property access
        let has_property_access = text.contains('.') || text.contains('[');

        has_operators || has_function_call || has_property_access
    }

    /// Apply syntax-aware optimizations
    fn apply_syntax_optimizations(
        &self,
        text: &str,
        context: &CompletionContext,
        optimizations: &mut Vec<String>,
    ) -> String {
        let mut result = text.to_string();

        // Align indentation for multi-line content
        if result.contains('\n') {
            result = self.align_indentation(&result, &context.indentation);
            optimizations.push("aligned_indentation".to_string());
        }

        // Language-specific optimizations
        match context.language.to_lowercase().as_str() {
            "javascript" | "typescript" | "tsx" | "jsx" => {
                result = self.optimize_for_javascript(&result, context);
            }
            "python" => {
                result = self.optimize_for_python(&result, context);
            }
            "rust" => {
                result = self.optimize_for_rust(&result, context);
            }
            _ => {}
        }

        result
    }

    /// Align indentation for multi-line text
    ///
    /// Uses smart dedent strategy: strips base_indent from completion lines if present
    /// to avoid "double indentation" when inserting at an indented cursor position.
    ///
    /// Example:
    /// - Cursor at 2-space indent, LLM returns `  return;` (with 2-space)
    /// - Old behavior: would add another 2-space -> `    return;` (4 spaces, wrong!)
    /// - New behavior: strips 2-space -> `return;`, inserted at cursor -> `  return;` (correct)
    fn align_indentation(&self, text: &str, base_indent: &str) -> String {
        if base_indent.is_empty() {
            return text.to_string();
        }

        let lines: Vec<&str> = text.lines().collect();
        let mut result_lines = Vec::new();

        for line in lines.iter() {
            // Try to strip the base indentation from the line.
            // If the line starts with base_indent, we remove it so that when the editor
            // inserts it at the current cursor position (which is already indented),
            // it doesn't get doubled.
            if let Some(stripped) = line.strip_prefix(base_indent) {
                result_lines.push(stripped.to_string());
            } else {
                // If the line has less indentation than base (e.g. closing brace `}`),
                // or different indentation, we leave it as-is.
                // This allows proper dedentation for block closers.
                result_lines.push(line.to_string());
            }
        }

        result_lines.join("\n")
    }

    /// JavaScript/TypeScript specific optimizations
    fn optimize_for_javascript(&self, text: &str, _context: &CompletionContext) -> String {
        let mut result = text.to_string();

        // Balance brackets (simple check)
        result = self.balance_brackets(&result);

        result
    }

    /// Python specific optimizations
    fn optimize_for_python(&self, text: &str, context: &CompletionContext) -> String {
        let mut result = text.to_string();

        // Ensure proper indentation after colons
        if result.contains(':') {
            let lines: Vec<&str> = result.lines().collect();
            let mut new_lines = Vec::new();

            for (i, line) in lines.iter().enumerate() {
                new_lines.push(line.to_string());
                if line.trim_end().ends_with(':') && i + 1 < lines.len() {
                    let next_line = lines[i + 1];
                    let next_indent = next_line.len() - next_line.trim_start().len();
                    let expected_indent = context.indentation.len() + 4;
                    if next_indent <= context.indentation.len() && !next_line.trim().is_empty() {
                        // Need to add indentation
                        let spaces = " ".repeat(expected_indent);
                        new_lines.pop(); // Remove the line we just added
                        new_lines.push(format!("{}{}", spaces, next_line.trim()));
                    }
                }
            }

            result = new_lines.join("\n");
        }

        result
    }

    /// Rust specific optimizations
    fn optimize_for_rust(&self, text: &str, _context: &CompletionContext) -> String {
        let result = text.to_string();
        // Balance brackets
        self.balance_brackets(&result)
    }

    /// Simple bracket balancing (detection only, no auto-fix)
    fn balance_brackets(&self, text: &str) -> String {
        // For now, just return the text as-is
        // We could add bracket balancing logic here if needed
        text.to_string()
    }

    /// Detect leading overlap between before_cursor and insert_text
    ///
    /// Finds the longest suffix of before_cursor that matches a prefix of insert_text.
    /// This handles cases like:
    /// - before_cursor ends with "newTD()" and insert_text is "newTD();"
    /// - before_cursor ends with "console.log" and insert_text is "console.log('hello')"
    ///
    /// Returns the length in bytes of the overlapping portion.
    fn detect_leading_overlap(&self, before_cursor: &str, insert_text: &str) -> usize {
        if before_cursor.is_empty() || insert_text.is_empty() {
            return 0;
        }

        // We want to find the longest suffix of before_cursor that matches
        // a prefix of insert_text.
        //
        // Example:
        //   before_cursor = "  newTD()"
        //   insert_text   = "newTD();"
        //   overlap       = "newTD()" (7 bytes)
        //
        // We iterate from length 1 to min(before_cursor.len(), insert_text.len())
        // and find the longest match.

        let before_bytes = before_cursor.as_bytes();
        let insert_bytes = insert_text.as_bytes();
        let max_overlap = before_bytes.len().min(insert_bytes.len());

        let mut best_overlap = 0;

        for len in 1..=max_overlap {
            // Get the last `len` bytes of before_cursor
            let before_suffix = &before_bytes[before_bytes.len() - len..];
            // Get the first `len` bytes of insert_text
            let insert_prefix = &insert_bytes[..len];

            if before_suffix == insert_prefix {
                best_overlap = len;
            }
        }

        // Only return overlap if it's significant (at least a word-like token)
        // This prevents false positives from single-character matches like "(" or ")"
        if best_overlap >= 2 {
            // Verify we're not cutting in the middle of a UTF-8 character
            if insert_text.is_char_boundary(best_overlap) {
                return best_overlap;
            }
        }

        0
    }

    /// Detect trailing overlap between insert_text and after_cursor
    ///
    /// Uses a hybrid approach:
    /// 1. Line-based trimmed match (robust for multi-line blocks like `}\n}`)
    /// 2. Character-based strict match (safe for inline/partial words)
    fn detect_trailing_overlap(&self, insert_text: &str, after_cursor: &str) -> OverlapResult {
        if insert_text.is_empty() || after_cursor.is_empty() {
            return OverlapResult { overlap_len: 0 };
        }

        let insert_lines: Vec<&str> = insert_text.lines().collect();
        let after_lines: Vec<&str> = after_cursor.lines().collect();

        // Strategy 1: Line-based Trim Match (High confidence for blocks)
        // This handles cases like `}\n}` where indentation might differ but content is identical.
        if !insert_lines.is_empty() && !after_lines.is_empty() {
            for i in 0..insert_lines.len() {
                let suffix_lines = &insert_lines[i..];
                // Optimization: Don't check if suffix is longer than after_cursor lines
                if suffix_lines.len() > after_lines.len() {
                    continue;
                }

                // Check if all lines in this suffix match the beginning of after_cursor.
                // Matching is done on trimmed content to ignore indentation differences.
                // We require !trim().is_empty() to avoid matching blank lines aggressively.
                let matches = suffix_lines
                    .iter()
                    .zip(after_lines.iter())
                    .all(|(a, b)| a.trim() == b.trim() && !a.trim().is_empty());

                if matches && !suffix_lines.is_empty() {
                    // Found a block match!
                    // Calculate the exact byte offset to return the correct overlap length.
                    let mut offset = 0;
                    for (idx, line) in insert_text.lines().enumerate() {
                        if idx == i {
                            return OverlapResult {
                                overlap_len: insert_text.len() - offset,
                            };
                        }
                        offset += line.len();
                        // Add newline length accurately
                        if offset < insert_text.len() {
                            if insert_text[offset..].starts_with("\r\n") {
                                offset += 2;
                            } else {
                                offset += 1;
                            }
                        }
                    }
                }
            }
        }

        // Strategy 2: Strict Character Match (Fallback)
        // Finds longest common suffix of insert_text that matches prefix of after_cursor.
        // This preserves safety for inline completions (e.g. "foo" vs "fooBar").
        let insert_bytes = insert_text.as_bytes();
        let after_bytes = after_cursor.as_bytes();
        let max_overlap = insert_bytes.len().min(after_bytes.len());

        for len in (1..=max_overlap).rev() {
            let insert_suffix = &insert_bytes[insert_bytes.len() - len..];
            let after_prefix = &after_bytes[..len];
            if insert_suffix == after_prefix {
                return OverlapResult { overlap_len: len };
            }
        }

        OverlapResult { overlap_len: 0 }
    }

    /// Detect multi-line overlap (by lines, not characters)
    fn detect_multiline_overlap(&self, insert_text: &str, after_cursor: &str) -> usize {
        let insert_lines: Vec<&str> = insert_text.lines().collect();
        let after_lines: Vec<&str> = after_cursor.lines().collect();

        if insert_lines.is_empty() || after_lines.is_empty() {
            return 0;
        }

        let max_overlap = insert_lines.len().min(after_lines.len());
        let mut overlap_count = 0;

        for len in 1..=max_overlap {
            let insert_suffix: Vec<&str> = insert_lines[insert_lines.len() - len..].to_vec();
            let after_prefix: Vec<&str> = after_lines[..len].to_vec();

            let matches = insert_suffix
                .iter()
                .zip(after_prefix.iter())
                .all(|(a, b)| a.trim() == b.trim());

            if matches {
                overlap_count = len;
            }
        }

        overlap_count
    }

    /// Get the last word from text (alphanumeric + underscore)
    fn get_last_word(&self, text: &str) -> String {
        let mut word = String::new();
        for ch in text.chars().rev() {
            if ch.is_alphanumeric() || ch == '_' {
                word.insert(0, ch);
            } else {
                break;
            }
        }
        word
    }

    /// Calculate common prefix length (matching from end of `before` to start of `response`)
    fn common_prefix_len(&self, before: &str, response: &str) -> usize {
        let before_bytes = before.as_bytes();
        let response_bytes = response.as_bytes();
        let max_len = before_bytes.len().min(response_bytes.len());

        let mut prefix_len = 0;
        for i in 0..max_len {
            // Compare end of before with start of response
            let before_idx = before_bytes.len() - 1 - i;
            if before_bytes[before_idx] == response_bytes[i] {
                prefix_len = i + 1;
            } else {
                break;
            }
        }

        prefix_len
    }
}

impl Default for SmartCompletionDiffer {
    fn default() -> Self {
        Self::new()
    }
}

/// Result of overlap detection
struct OverlapResult {
    overlap_len: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_context(before: &str, after: &str, language: &str) -> CompletionContext {
        let lines: Vec<&str> = before.lines().collect();
        let current_line = lines.last().unwrap_or(&"").to_string();
        let indent_len = current_line.len() - current_line.trim_start().len();
        let indentation = " ".repeat(indent_len);

        CompletionContext {
            before_cursor: before.to_string(),
            after_cursor: after.to_string(),
            current_line,
            language: language.to_string(),
            indentation,
            cursor_row: lines.len().saturating_sub(1) as u32,
            cursor_col: lines.last().map_or(0, |l| l.len()) as u32,
        }
    }

    #[test]
    fn test_leading_newline_removal() {
        let differ = SmartCompletionDiffer::new();
        let context = make_context("  console.log(", ");", "typescript");
        let api_response = "\n  console.log(quickSort(arr2));\n}\nnewTD();";

        let result = differ.extract_completion_diff(&context, api_response, Some((56, 56)));

        assert!(!result.insert_text.starts_with('\n'));
        assert!(
            result
                .optimizations
                .contains(&"removed_leading_newline".to_string())
        );
    }

    #[test]
    fn test_trailing_overlap_detection() {
        let differ = SmartCompletionDiffer::new();
        let context = make_context("let x = ", ";\nlet y = 2;", "typescript");
        let api_response = "1;\nlet y = 2";

        let result = differ.extract_completion_diff(&context, api_response, None);

        // Should detect overlap with ";\nlet y = 2" in after_cursor
        assert!(
            result.insert_text.len() < api_response.len()
                || result.optimizations.iter().any(|o| o.contains("overlap"))
        );
    }

    #[test]
    fn test_partial_word_completion() {
        let differ = SmartCompletionDiffer::new();
        let context = make_context("  con", "", "javascript");
        let api_response = "console.log";

        let result = differ.extract_completion_diff(&context, api_response, None);

        // Should remove "con" prefix
        assert_eq!(result.insert_text, "sole.log");
        assert_eq!(result.method, DiffMethod::CharacterDiff);
    }

    #[test]
    fn test_multiline_duplicate_removal() {
        let differ = SmartCompletionDiffer::new();
        let context = make_context("function test() {\n  let x = 1;\n", "", "javascript");
        let api_response = "  let x = 1;\n  let y = 2;\n}";

        let result = differ.extract_completion_diff(&context, api_response, None);

        // Should remove the duplicate "  let x = 1;" line
        assert!(!result.insert_text.contains("let x = 1"));
        assert!(result.insert_text.contains("let y = 2"));
    }

    #[test]
    fn test_content_type_detection() {
        let differ = SmartCompletionDiffer::new();

        // Multi-line
        let ctx = make_context("", "", "js");
        assert_eq!(
            differ.analyze_content_type(&ctx, "line1\nline2"),
            ContentType::MultiLine
        );

        // Block structure
        assert_eq!(
            differ.analyze_content_type(&ctx, "function test() {\n  return 1;\n}"),
            ContentType::BlockStructure
        );

        // Partial word
        let ctx = make_context("cons", "", "js");
        assert_eq!(
            differ.analyze_content_type(&ctx, "console"),
            ContentType::PartialWord
        );

        // Expression
        let ctx = make_context("let x = ", "", "js");
        assert_eq!(
            differ.analyze_content_type(&ctx, "a + b * c"),
            ContentType::Expression
        );
    }
}
