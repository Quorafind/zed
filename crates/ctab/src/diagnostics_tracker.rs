//! Diagnostics Tracker for Ctab
//!
//! This module provides intelligent collection and tracking of diagnostics
//! from the buffer/LSP to enhance code completion context.
//!
//! Key features:
//! - Collects errors and warnings from buffer diagnostics
//! - Converts to both `Diagnostic` and `LinterErrors` proto formats
//! - Filters by severity and proximity to cursor
//! - Caches recent diagnostics for change detection

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use language::{BufferSnapshot, DiagnosticSeverity, Point};
use parking_lot::RwLock;

use crate::proto::{
    CursorPosition, CursorRange, Diagnostic as ProtoDiagnostic, LinterError, LinterErrors,
    diagnostic::DiagnosticSeverity as ProtoDiagnosticSeverity,
};

// ============================================================================
// Constants
// ============================================================================

/// Maximum diagnostics to include per request
const MAX_DIAGNOSTICS: usize = 20;

/// Maximum diagnostics to include in linter_errors (more detailed format)
const MAX_LINTER_ERRORS: usize = 10;

/// Cache TTL for diagnostics (milliseconds)
const CACHE_TTL_MS: u64 = 1000;

/// Radius around cursor for proximity boost (lines)
const CURSOR_PROXIMITY_RADIUS: u32 = 10;

// ============================================================================
// Types
// ============================================================================

/// A scored diagnostic entry for prioritization
#[derive(Clone, Debug)]
struct ScoredDiagnostic {
    /// The proto diagnostic
    diagnostic: ProtoDiagnostic,
    /// Score for prioritization (higher = more important)
    score: f32,
    /// Line number for sorting
    line: u32,
}

/// Cached diagnostics for a file
#[derive(Clone, Debug)]
struct CachedDiagnostics {
    /// List of diagnostics
    diagnostics: Vec<ProtoDiagnostic>,
    /// When this cache was created
    cached_at: Instant,
    /// Content hash when cached
    content_hash: u64,
}

/// Result of collecting diagnostics
#[derive(Clone, Debug, Default)]
pub struct DiagnosticsResult {
    /// Diagnostics for CurrentFileInfo.diagnostics field
    pub diagnostics: Vec<ProtoDiagnostic>,
    /// LinterErrors for StreamCppRequest.linter_errors field
    pub linter_errors: Option<LinterErrors>,
    /// Number of errors found
    pub error_count: usize,
    /// Number of warnings found
    pub warning_count: usize,
}

// ============================================================================
// DiagnosticsTracker
// ============================================================================

/// Tracks and collects diagnostics for code completion context
pub struct DiagnosticsTracker {
    /// Cache of diagnostics by file path
    cache: Arc<RwLock<HashMap<String, CachedDiagnostics>>>,
}

impl DiagnosticsTracker {
    /// Create a new DiagnosticsTracker
    pub fn new() -> Self {
        Self {
            cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Collect diagnostics from a buffer snapshot
    ///
    /// # Arguments
    /// * `snapshot` - Buffer snapshot to collect from
    /// * `file_path` - Relative file path
    /// * `cursor_point` - Current cursor position for proximity scoring
    /// * `include_content` - Whether to include file content in LinterErrors
    ///
    /// # Returns
    /// DiagnosticsResult with both formats
    pub fn collect(
        &self,
        snapshot: &BufferSnapshot,
        file_path: &str,
        cursor_point: Point,
        include_content: bool,
    ) -> DiagnosticsResult {
        // Check cache first
        if let Some(cached) = self.get_cached(file_path, snapshot) {
            return self.build_result(cached, file_path, snapshot, include_content);
        }

        // Collect and score diagnostics
        let scored: Vec<ScoredDiagnostic> = snapshot
            .diagnostics_in_range::<_, Point>(0..snapshot.len(), false)
            .filter(|entry| {
                matches!(
                    entry.diagnostic.severity,
                    DiagnosticSeverity::ERROR | DiagnosticSeverity::WARNING
                )
            })
            .map(|entry| {
                // entry.range is already Range<Point>, no conversion needed
                let start_point = entry.range.start;
                let end_point = entry.range.end;

                // Calculate score based on severity and proximity
                let severity_score = match entry.diagnostic.severity {
                    DiagnosticSeverity::ERROR => 10.0,
                    DiagnosticSeverity::WARNING => 5.0,
                    _ => 1.0,
                };

                // Proximity boost: diagnostics near cursor get higher priority
                let distance = if start_point.row > cursor_point.row {
                    start_point.row - cursor_point.row
                } else {
                    cursor_point.row - start_point.row
                };
                let proximity_boost = if distance <= CURSOR_PROXIMITY_RADIUS {
                    2.0 - (distance as f32 / CURSOR_PROXIMITY_RADIUS as f32)
                } else {
                    0.0
                };

                let proto_severity = match entry.diagnostic.severity {
                    DiagnosticSeverity::ERROR => ProtoDiagnosticSeverity::Error,
                    DiagnosticSeverity::WARNING => ProtoDiagnosticSeverity::Warning,
                    DiagnosticSeverity::INFORMATION => ProtoDiagnosticSeverity::Information,
                    DiagnosticSeverity::HINT => ProtoDiagnosticSeverity::Hint,
                    _ => ProtoDiagnosticSeverity::Unspecified,
                };

                ScoredDiagnostic {
                    diagnostic: ProtoDiagnostic {
                        message: entry.diagnostic.message.clone(),
                        range: Some(CursorRange {
                            start_position: Some(CursorPosition {
                                line: start_point.row as i32,
                                column: start_point.column as i32,
                            }),
                            end_position: Some(CursorPosition {
                                line: end_point.row as i32,
                                column: end_point.column as i32,
                            }),
                        }),
                        severity: proto_severity.into(),
                        related_information: vec![],
                    },
                    score: severity_score + proximity_boost,
                    line: start_point.row,
                }
            })
            .collect();

        // Sort by score (descending) then by line number
        let mut sorted = scored;
        sorted.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.line.cmp(&b.line))
        });

        // Take top diagnostics
        let diagnostics: Vec<ProtoDiagnostic> = sorted
            .into_iter()
            .take(MAX_DIAGNOSTICS)
            .map(|s| s.diagnostic)
            .collect();

        // Update cache
        self.update_cache(file_path, &diagnostics, snapshot);

        self.build_result(diagnostics, file_path, snapshot, include_content)
    }

    /// Build the result from diagnostics
    fn build_result(
        &self,
        diagnostics: Vec<ProtoDiagnostic>,
        file_path: &str,
        snapshot: &BufferSnapshot,
        include_content: bool,
    ) -> DiagnosticsResult {
        let error_count = diagnostics
            .iter()
            .filter(|d| d.severity == ProtoDiagnosticSeverity::Error as i32)
            .count();
        let warning_count = diagnostics
            .iter()
            .filter(|d| d.severity == ProtoDiagnosticSeverity::Warning as i32)
            .count();

        // Build LinterErrors if there are diagnostics
        let linter_errors = if !diagnostics.is_empty() {
            let errors: Vec<LinterError> = diagnostics
                .iter()
                .take(MAX_LINTER_ERRORS)
                .map(|d| LinterError {
                    message: d.message.clone(),
                    range: d.range.clone(),
                    source: None,
                    related_information: vec![],
                    severity: Some(d.severity),
                })
                .collect();

            Some(LinterErrors {
                relative_workspace_path: file_path.to_string(),
                errors,
                file_contents: if include_content {
                    snapshot.text()
                } else {
                    String::new()
                },
            })
        } else {
            None
        };

        DiagnosticsResult {
            diagnostics,
            linter_errors,
            error_count,
            warning_count,
        }
    }

    /// Get cached diagnostics if valid
    fn get_cached(
        &self,
        file_path: &str,
        snapshot: &BufferSnapshot,
    ) -> Option<Vec<ProtoDiagnostic>> {
        let cache = self.cache.read();
        if let Some(cached) = cache.get(file_path) {
            // Check TTL
            if cached.cached_at.elapsed().as_millis() < CACHE_TTL_MS as u128 {
                // Check content hash (simple length-based for now)
                let current_hash = snapshot.len() as u64;
                if cached.content_hash == current_hash {
                    return Some(cached.diagnostics.clone());
                }
            }
        }
        None
    }

    /// Update the cache
    fn update_cache(
        &self,
        file_path: &str,
        diagnostics: &[ProtoDiagnostic],
        snapshot: &BufferSnapshot,
    ) {
        let mut cache = self.cache.write();
        cache.insert(
            file_path.to_string(),
            CachedDiagnostics {
                diagnostics: diagnostics.to_vec(),
                cached_at: Instant::now(),
                content_hash: snapshot.len() as u64,
            },
        );

        // Limit cache size
        if cache.len() > 50 {
            // Remove oldest entries
            let mut entries: Vec<_> = cache
                .iter()
                .map(|(k, v)| (k.clone(), v.cached_at))
                .collect();
            entries.sort_by_key(|(_, t)| *t);
            for (key, _) in entries.into_iter().take(10) {
                cache.remove(&key);
            }
        }
    }

    /// Clear all cached diagnostics
    #[allow(dead_code)]
    pub fn clear_cache(&self) {
        self.cache.write().clear();
    }

    /// Clear cached diagnostics for a specific file
    #[allow(dead_code)]
    pub fn clear_file_cache(&self, file_path: &str) {
        self.cache.write().remove(file_path);
    }
}

impl Default for DiagnosticsTracker {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_diagnostics_tracker_creation() {
        let tracker = DiagnosticsTracker::new();
        assert!(tracker.cache.read().is_empty());
    }

    #[test]
    fn test_default_result() {
        let result = DiagnosticsResult::default();
        assert!(result.diagnostics.is_empty());
        assert!(result.linter_errors.is_none());
        assert_eq!(result.error_count, 0);
        assert_eq!(result.warning_count, 0);
    }
}
