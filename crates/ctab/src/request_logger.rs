//! Request context logger for debugging Ctab completions
//!
//! Logs each request's full context to rotating files (keeps last 3).

use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::SystemTime;

use crate::proto::StreamCppRequest;

/// Request logger that writes to rotating files
pub struct RequestLogger {
    /// Base directory for log files
    log_dir: PathBuf,
    /// Counter for rotation (0, 1, 2)
    counter: AtomicU32,
}

impl RequestLogger {
    /// Create a new request logger
    ///
    /// Logs will be written to `{zed_data_dir}/ctab_requests/`
    pub fn new() -> Self {
        // Try to find the Zed data directory
        let log_dir = Self::get_zed_data_dir().join("ctab_requests");

        // Create directory if it doesn't exist
        let _ = fs::create_dir_all(&log_dir);

        Self {
            log_dir,
            counter: AtomicU32::new(0),
        }
    }

    /// Get the Zed data directory
    fn get_zed_data_dir() -> PathBuf {
        // Windows: %LOCALAPPDATA%\Zed
        // macOS: ~/Library/Application Support/Zed
        // Linux: ~/.local/share/Zed
        #[cfg(target_os = "windows")]
        {
            std::env::var("LOCALAPPDATA")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("."))
                .join("Zed")
        }

        #[cfg(target_os = "macos")]
        {
            std::env::var("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("."))
                .join("Library")
                .join("Application Support")
                .join("Zed")
        }

        #[cfg(target_os = "linux")]
        {
            std::env::var("XDG_DATA_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| {
                    std::env::var("HOME")
                        .map(PathBuf::from)
                        .unwrap_or_else(|_| PathBuf::from("."))
                        .join(".local")
                        .join("share")
                })
                .join("Zed")
        }

        #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
        {
            PathBuf::from(".").join("Zed")
        }
    }

    /// Format current time as a string
    fn format_timestamp() -> String {
        let now = SystemTime::now();
        let duration = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default();
        let secs = duration.as_secs();

        // Convert to readable format (UTC)
        let days_since_epoch = secs / 86400;
        let time_of_day = secs % 86400;
        let hours = time_of_day / 3600;
        let minutes = (time_of_day % 3600) / 60;
        let seconds = time_of_day % 60;
        let millis = duration.subsec_millis();

        // Simple date calculation (approximate, good enough for logging)
        let year = 1970 + (days_since_epoch / 365) as u32;
        let day_of_year = days_since_epoch % 365;
        let month = (day_of_year / 30) + 1;
        let day = (day_of_year % 30) + 1;

        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03} UTC",
            year, month, day, hours, minutes, seconds, millis
        )
    }

    /// Log a request to file
    ///
    /// Files are named `request_0.log`, `request_1.log`, `request_2.log`
    /// and rotate every 3 requests.
    pub fn log_request(&self, request: &StreamCppRequest, extra_info: &str) {
        let index = self.counter.fetch_add(1, Ordering::SeqCst) % 3;
        let file_path = self.log_dir.join(format!("request_{}.log", index));

        if let Err(e) = self.write_request_to_file(&file_path, request, extra_info) {
            log::warn!("Ctab RequestLogger: Failed to write request log: {}", e);
        } else {
            log::debug!("Ctab RequestLogger: Wrote request to {:?}", file_path);
        }
    }

    fn write_request_to_file(
        &self,
        path: &PathBuf,
        request: &StreamCppRequest,
        extra_info: &str,
    ) -> std::io::Result<()> {
        let mut file = File::create(path)?;

        let timestamp = Self::format_timestamp();

        writeln!(
            file,
            "╔══════════════════════════════════════════════════════════════════╗"
        )?;
        writeln!(
            file,
            "║  CTAB REQUEST CONTEXT LOG                                        ║"
        )?;
        writeln!(file, "║  Timestamp: {:50} ║", timestamp)?;
        writeln!(
            file,
            "╚══════════════════════════════════════════════════════════════════╝"
        )?;
        writeln!(file)?;

        // Extra info
        if !extra_info.is_empty() {
            writeln!(file, "═══ EXTRA INFO ═══")?;
            writeln!(file, "{}", extra_info)?;
            writeln!(file)?;
        }

        // Current file info
        if let Some(current_file) = &request.current_file {
            writeln!(file, "═══ CURRENT FILE ═══")?;
            writeln!(file, "Path: {}", current_file.relative_workspace_path)?;
            writeln!(file, "Language: {}", current_file.language_id)?;
            writeln!(file, "Total lines: {}", current_file.total_number_of_lines)?;
            writeln!(
                file,
                "Content length: {} bytes",
                current_file.contents.len()
            )?;
            writeln!(file, "Rely on filesync: {}", current_file.rely_on_filesync)?;

            if let Some(cursor) = &current_file.cursor_position {
                writeln!(
                    file,
                    "Cursor position: line {}, column {}",
                    cursor.line, cursor.column
                )?;
            }

            if let Some(selection) = &current_file.selection {
                if let (Some(start), Some(end)) =
                    (&selection.start_position, &selection.end_position)
                {
                    if start.line != end.line || start.column != end.column {
                        writeln!(
                            file,
                            "Selection: ({},{}) to ({},{})",
                            start.line, start.column, end.line, end.column
                        )?;
                    }
                }
            }

            writeln!(file)?;
            writeln!(file, "─── FILE CONTENTS ───")?;
            writeln!(file, "{}", current_file.contents)?;
            writeln!(file)?;

            // Diagnostics
            if !current_file.diagnostics.is_empty() {
                writeln!(
                    file,
                    "─── DIAGNOSTICS ({}) ───",
                    current_file.diagnostics.len()
                )?;
                for (i, diag) in current_file.diagnostics.iter().enumerate() {
                    writeln!(file, "[{}] {}", i + 1, diag.message)?;
                    if let Some(range) = &diag.range {
                        if let (Some(start), Some(end)) =
                            (&range.start_position, &range.end_position)
                        {
                            writeln!(
                                file,
                                "    Range: ({},{}) to ({},{})",
                                start.line, start.column, end.line, end.column
                            )?;
                        }
                    }
                }
                writeln!(file)?;
            }
        }

        // Model name
        if let Some(model) = &request.model_name {
            writeln!(file, "═══ MODEL ═══")?;
            writeln!(file, "Name: {}", model)?;
            writeln!(file)?;
        }

        // Workspace ID
        if let Some(ws_id) = &request.workspace_id {
            writeln!(file, "═══ WORKSPACE ═══")?;
            writeln!(file, "ID: {}", ws_id)?;
            writeln!(file)?;
        }

        // Diff history
        if !request.file_diff_histories.is_empty() {
            writeln!(file, "═══ DIFF HISTORY ═══")?;
            for fdh in &request.file_diff_histories {
                writeln!(file, "File: {}", fdh.file_name)?;
                writeln!(file, "Diffs: {} entries", fdh.diff_history.len())?;
                for (i, diff) in fdh.diff_history.iter().enumerate() {
                    let ts = fdh.diff_history_timestamps.get(i).copied().unwrap_or(0.0);
                    writeln!(file, "  [{}] (ts: {:.3})", i + 1, ts)?;
                    // Indent diff content
                    for line in diff.lines() {
                        writeln!(file, "    {}", line)?;
                    }
                }
            }
            writeln!(file)?;
        }

        // Context items
        if !request.context_items.is_empty() {
            writeln!(
                file,
                "═══ CONTEXT ITEMS ({}) ═══",
                request.context_items.len()
            )?;
            for (i, item) in request.context_items.iter().enumerate() {
                writeln!(
                    file,
                    "[{}] Path: {}, Score: {:.3}",
                    i + 1,
                    item.relative_workspace_path,
                    item.score
                )?;
                if let Some(symbol) = &item.symbol {
                    writeln!(file, "    Symbol: {}", symbol)?;
                }
                writeln!(file, "    Content ({} chars):", item.contents.len())?;
                // Show first 500 chars of content
                let preview: String = item.contents.chars().take(500).collect();
                for line in preview.lines() {
                    writeln!(file, "      {}", line)?;
                }
                if item.contents.len() > 500 {
                    writeln!(file, "      ... ({} more chars)", item.contents.len() - 500)?;
                }
            }
            writeln!(file)?;
        }

        // Additional files
        if !request.additional_files.is_empty() {
            writeln!(
                file,
                "═══ ADDITIONAL FILES ({}) ═══",
                request.additional_files.len()
            )?;
            for (i, af) in request.additional_files.iter().enumerate() {
                writeln!(
                    file,
                    "[{}] Path: {}, Open: {}",
                    i + 1,
                    af.relative_workspace_path,
                    af.is_open
                )?;
                if let Some(last_viewed) = af.last_viewed_at {
                    writeln!(file, "    Last viewed: {:.3}", last_viewed)?;
                }
                if !af.visible_ranges.is_empty() {
                    writeln!(
                        file,
                        "    Visible ranges: {} range(s)",
                        af.visible_ranges.len()
                    )?;
                }
                if !af.visible_range_content.is_empty() {
                    writeln!(
                        file,
                        "    Visible content ({} lines):",
                        af.visible_range_content.len()
                    )?;
                    for (j, content) in af.visible_range_content.iter().take(10).enumerate() {
                        writeln!(file, "      {}: {}", j + 1, content)?;
                    }
                    if af.visible_range_content.len() > 10 {
                        writeln!(
                            file,
                            "      ... ({} more lines)",
                            af.visible_range_content.len() - 10
                        )?;
                    }
                }
            }
            writeln!(file)?;
        }

        // Linter errors
        if let Some(linter_errors) = &request.linter_errors {
            if !linter_errors.errors.is_empty() {
                writeln!(
                    file,
                    "═══ LINTER ERRORS ({}) ═══",
                    linter_errors.errors.len()
                )?;
                writeln!(file, "File: {}", linter_errors.relative_workspace_path)?;
                for (i, err) in linter_errors.errors.iter().enumerate() {
                    writeln!(file, "[{}] {}", i + 1, err.message)?;
                    if let Some(range) = &err.range {
                        if let (Some(start), Some(end)) =
                            (&range.start_position, &range.end_position)
                        {
                            writeln!(
                                file,
                                "    Range: ({},{}) to ({},{})",
                                start.line, start.column, end.line, end.column
                            )?;
                        }
                    }
                }
                writeln!(file)?;
            }
        }

        // LSP contexts
        if !request.lsp_contexts.is_empty() {
            writeln!(
                file,
                "═══ LSP CONTEXTS ({}) ═══",
                request.lsp_contexts.len()
            )?;
            for (i, ctx) in request.lsp_contexts.iter().enumerate() {
                writeln!(
                    file,
                    "[{}] URI: {}, Symbol: {}, Score: {:.3}",
                    i + 1,
                    ctx.uri,
                    ctx.symbol_name,
                    ctx.score
                )?;
                if !ctx.context_items.is_empty() {
                    writeln!(file, "    Context items: {}", ctx.context_items.len())?;
                    for item in &ctx.context_items {
                        writeln!(
                            file,
                            "      - Type: {}, Content: {} chars",
                            item.r#type,
                            item.content.len()
                        )?;
                    }
                }
            }
            writeln!(file)?;
        }

        // Code results
        if !request.code_results.is_empty() {
            writeln!(
                file,
                "═══ CODE RESULTS ({}) ═══",
                request.code_results.len()
            )?;
            for (i, result) in request.code_results.iter().enumerate() {
                writeln!(file, "[{}] Score: {:.3}", i + 1, result.score)?;
                if let Some(block) = &result.code_block {
                    writeln!(file, "    Path: {}", block.relative_workspace_path)?;
                    writeln!(file, "    Contents ({} chars):", block.contents.len())?;
                    let preview: String = block.contents.chars().take(300).collect();
                    for line in preview.lines() {
                        writeln!(file, "      {}", line)?;
                    }
                    if block.contents.len() > 300 {
                        writeln!(
                            file,
                            "      ... ({} more chars)",
                            block.contents.len() - 300
                        )?;
                    }
                }
            }
            writeln!(file)?;
        }

        // Parameter hints
        if !request.parameter_hints.is_empty() {
            writeln!(
                file,
                "═══ PARAMETER HINTS ({}) ═══",
                request.parameter_hints.len()
            )?;
            for (i, hint) in request.parameter_hints.iter().enumerate() {
                writeln!(file, "[{}] Label: {}", i + 1, hint.label)?;
                if let Some(doc) = &hint.documentation {
                    writeln!(file, "    Doc: {}", doc)?;
                }
            }
            writeln!(file)?;
        }

        // Filesync updates
        if !request.filesync_updates.is_empty() {
            writeln!(
                file,
                "═══ FILESYNC UPDATES ({}) ═══",
                request.filesync_updates.len()
            )?;
            for (i, update) in request.filesync_updates.iter().enumerate() {
                writeln!(
                    file,
                    "[{}] Path: {}, Version: {}, Updates: {}",
                    i + 1,
                    update.relative_workspace_path,
                    update.model_version,
                    update.updates.len()
                )?;
            }
            writeln!(file)?;
        }

        // Other flags
        writeln!(file, "═══ FLAGS ═══")?;
        writeln!(file, "is_nightly: {:?}", request.is_nightly)?;
        writeln!(file, "is_debug: {:?}", request.is_debug)?;
        writeln!(
            file,
            "enable_more_context: {:?}",
            request.enable_more_context
        )?;
        writeln!(file, "supports_cpt: {:?}", request.supports_cpt)?;
        writeln!(file, "supports_crlf_cpt: {:?}", request.supports_crlf_cpt)?;
        if let Some(intent) = &request.cpp_intent_info {
            writeln!(file, "intent_source: {}", intent.source)?;
        }
        writeln!(file)?;

        writeln!(file, "═══ END OF REQUEST LOG ═══")?;

        Ok(())
    }

    /// Get the log directory path
    pub fn log_dir(&self) -> &PathBuf {
        &self.log_dir
    }
}

impl Default for RequestLogger {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_logger_rotation() {
        let logger = RequestLogger::new();

        // First request should be index 0
        let idx0 = logger.counter.load(Ordering::SeqCst);
        assert_eq!(idx0 % 3, 0);

        // After 3 increments, should wrap back to 0
        logger.counter.fetch_add(1, Ordering::SeqCst);
        logger.counter.fetch_add(1, Ordering::SeqCst);
        logger.counter.fetch_add(1, Ordering::SeqCst);
        let idx3 = logger.counter.load(Ordering::SeqCst);
        assert_eq!(idx3 % 3, 0);
    }
}
