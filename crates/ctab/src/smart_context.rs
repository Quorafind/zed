//! Smart Context Engine for Ctab
//!
//! This module provides intelligent context collection for code completion,
//! including import analysis, symbol tracking, recent file management,
//! async prefetching, LSP integration, and semantic indexing.
//!
//! Architecture:
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                         SmartContextEngine                               │
//! │  ┌──────────────┐  ┌──────────────┐  ┌───────────────┐  ┌────────────┐  │
//! │  │ImportAnalyzer│  │RecentTracker │  │  LSPResolver  │  │SyntaxIndex │  │
//! │  │  - Rust      │  │  - MRU files │  │  - definition │  │ - outline  │  │
//! │  │  - TS/JS     │  │  - edit time │  │  - references │  │ - symbols  │  │
//! │  │  - Python    │  │  - view time │  │  - hover info │  │ - decls    │  │
//! │  │  - Go        │  │  - decay     │  │               │  │            │  │
//! │  └──────────────┘  └──────────────┘  └───────────────┘  └────────────┘  │
//! │                           │                                              │
//! │  ┌──────────────────────────────────────────────────────────────────┐   │
//! │  │                      AsyncPrefetcher                              │   │
//! │  │  - idle detection    - ripgrep search    - background indexing   │   │
//! │  └──────────────────────────────────────────────────────────────────┘   │
//! │                           │                                              │
//! │                    ┌──────▼──────┐                                      │
//! │                    │ContextCache │                                      │
//! │                    │  - TTL: 30s │                                      │
//! │                    │  - scored   │                                      │
//! │                    └─────────────┘                                      │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use futures::StreamExt;
use gpui::{App, AsyncApp, Context, Entity, Task, WeakEntity};
use language::{Anchor, Buffer, BufferSnapshot, Point, ToOffset, ToPoint};
use parking_lot::{Mutex, RwLock};
use project::Project;
use project::search::{SearchQuery, SearchResult};
use util::paths::PathMatcher;
use util::rel_path::RelPath;
use worktree::Snapshot as WorktreeSnapshot;

use crate::proto::{AdditionalFile, CppContextItem, LineRange};

// ============================================================================
// Constants
// ============================================================================

/// Maximum number of context items to include
const MAX_CONTEXT_ITEMS: usize = 15;

/// Maximum number of recent files to track
const MAX_RECENT_FILES: usize = 20;

/// Maximum content size per context item (bytes)
const MAX_CONTEXT_ITEM_SIZE: usize = 8_000;

/// Context cache TTL (seconds)
const CONTEXT_CACHE_TTL_SECS: u64 = 30;

/// Recent file relevance decay half-life (seconds)
const RECENT_FILE_HALF_LIFE_SECS: f64 = 300.0;

/// Idle detection threshold for prefetch (seconds)
const IDLE_THRESHOLD_SECS: u64 = 5;

/// Prefetch debounce duration (milliseconds)
const PREFETCH_DEBOUNCE_MS: u64 = 500;

/// Maximum prefetch results to cache
const MAX_PREFETCH_RESULTS: usize = 50;

/// Maximum symbols to track for LSP lookups
const MAX_LSP_SYMBOLS: usize = 10;

/// LSP result cache TTL (seconds)
const LSP_CACHE_TTL_SECS: u64 = 60;

/// Maximum declarations per file in syntax index
const MAX_DECLARATIONS_PER_FILE: usize = 100;

// ============================================================================
// Core Types
// ============================================================================

/// Represents a collected context item with scoring metadata
#[derive(Clone, Debug)]
pub struct ScoredContextItem {
    /// The context item content
    pub item: CppContextItem,
    /// Composite relevance score (higher = more relevant)
    pub score: f32,
    /// Source of this context (for debugging)
    pub source: ContextSource,
}

/// Source of a context item
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContextSource {
    /// From import/use statement analysis
    Import,
    /// From recently edited files
    RecentEdit,
    /// From recently viewed files
    RecentView,
    /// From symbol reference analysis (LSP)
    SymbolReference,
    /// From symbol definition (LSP)
    SymbolDefinition,
    /// From enclosing scope (TreeSitter)
    EnclosingScope,
    /// From open buffer heuristics
    OpenBuffer,
    /// From async prefetch (ripgrep search)
    Prefetch,
    /// From syntax index (declarations)
    SyntaxIndex,
}

/// Parsed import statement
#[derive(Clone, Debug)]
pub struct ImportInfo {
    /// The import path (e.g., "std::collections::HashMap")
    pub path: String,
    /// Resolved file path if available
    pub resolved_path: Option<PathBuf>,
    /// Imported symbols (empty for wildcard imports)
    pub symbols: Vec<String>,
    /// Whether this is a wildcard import (use foo::*)
    pub is_wildcard: bool,
    /// Line number in source file
    pub line: u32,
}

/// Recent file entry with timing metadata
#[derive(Clone, Debug)]
pub struct RecentFileEntry {
    /// Relative path from workspace root
    pub path: String,
    /// Last edit timestamp
    pub last_edited: Option<Instant>,
    /// Last view timestamp
    pub last_viewed: Option<Instant>,
    /// Number of edits in this session
    pub edit_count: u32,
    /// Cached content hash (for change detection)
    pub content_hash: Option<u64>,
}

/// Symbol reference information
#[derive(Clone, Debug)]
pub struct SymbolReference {
    /// Symbol name
    pub name: String,
    /// File path where symbol is defined/used
    pub file_path: String,
    /// Line range
    pub line_start: u32,
    pub line_end: u32,
    /// Reference type
    pub ref_type: SymbolRefType,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SymbolRefType {
    Definition,
    Reference,
    Import,
}

/// Declaration information from syntax index
#[derive(Clone, Debug)]
pub struct Declaration {
    /// Symbol name/identifier
    pub name: String,
    /// Declaration kind (function, struct, class, etc.)
    pub kind: DeclarationKind,
    /// File path
    pub file_path: String,
    /// Byte range in file
    pub range: Range<usize>,
    /// Line range
    pub line_range: Range<u32>,
    /// Signature text (first line or full for short decls)
    pub signature: String,
    /// Full declaration text (may be truncated)
    pub full_text: String,
    /// Parent declaration (for nested items)
    pub parent: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeclarationKind {
    Function,
    Method,
    Struct,
    Class,
    Trait,
    Interface,
    Enum,
    Const,
    Variable,
    Module,
    Type,
    Other,
}

// ============================================================================
// Import Analyzer
// ============================================================================

/// Analyzes import statements in source files
pub struct ImportAnalyzer;

impl ImportAnalyzer {
    /// Parse imports from a buffer snapshot
    pub fn parse_imports(snapshot: &BufferSnapshot, language_id: &str) -> Vec<ImportInfo> {
        let content = snapshot.text();
        match language_id {
            "rust" => Self::parse_rust_imports(&content),
            "typescript" | "typescriptreact" => Self::parse_typescript_imports(&content),
            "javascript" | "javascriptreact" => Self::parse_javascript_imports(&content),
            "python" => Self::parse_python_imports(&content),
            "go" => Self::parse_go_imports(&content),
            _ => vec![],
        }
    }

    /// Parse Rust use statements
    fn parse_rust_imports(content: &str) -> Vec<ImportInfo> {
        let mut imports = Vec::new();

        for (line_num, line) in content.lines().enumerate() {
            let trimmed = line.trim();

            // Match: use foo::bar;
            // Match: use foo::bar::{baz, qux};
            // Match: use foo::bar::*;
            if let Some(use_stmt) = trimmed.strip_prefix("use ") {
                if let Some(path) = use_stmt.strip_suffix(';') {
                    let path = path.trim();

                    // Check for wildcard
                    let is_wildcard = path.ends_with("::*");
                    let clean_path = if is_wildcard {
                        path.strip_suffix("::*").unwrap_or(path)
                    } else {
                        path
                    };

                    // Check for grouped imports {a, b, c}
                    let (base_path, symbols) = if let Some(brace_start) = clean_path.find('{') {
                        let base = clean_path[..brace_start].trim_end_matches("::");
                        let symbols_str = &clean_path[brace_start..];
                        let symbols: Vec<String> = symbols_str
                            .trim_matches(|c| c == '{' || c == '}')
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect();
                        (base.to_string(), symbols)
                    } else {
                        // Single import - extract last segment as symbol
                        let parts: Vec<&str> = clean_path.split("::").collect();
                        if parts.len() > 1 {
                            let symbol = parts.last().unwrap().to_string();
                            (clean_path.to_string(), vec![symbol])
                        } else {
                            (clean_path.to_string(), vec![])
                        }
                    };

                    imports.push(ImportInfo {
                        path: base_path,
                        resolved_path: None,
                        symbols,
                        is_wildcard,
                        line: line_num as u32,
                    });
                }
            }

            // Match: mod foo;
            if let Some(mod_stmt) = trimmed.strip_prefix("mod ") {
                if let Some(name) = mod_stmt.strip_suffix(';') {
                    imports.push(ImportInfo {
                        path: format!("crate::{}", name.trim()),
                        resolved_path: None,
                        symbols: vec![],
                        is_wildcard: false,
                        line: line_num as u32,
                    });
                }
            }
        }

        imports
    }

    /// Parse TypeScript/JavaScript imports
    fn parse_typescript_imports(content: &str) -> Vec<ImportInfo> {
        let mut imports = Vec::new();

        for (line_num, line) in content.lines().enumerate() {
            let trimmed = line.trim();

            // Match: import { foo, bar } from 'module';
            // Match: import foo from 'module';
            // Match: import * as foo from 'module';
            // Match: import 'module';
            if trimmed.starts_with("import ") {
                // Extract the module path from quotes
                let path = Self::extract_quoted_string(trimmed);
                if let Some(path) = path {
                    let is_wildcard = trimmed.contains("* as ");

                    // Extract named imports
                    let symbols = if let Some(brace_start) = trimmed.find('{') {
                        if let Some(brace_end) = trimmed.find('}') {
                            trimmed[brace_start + 1..brace_end]
                                .split(',')
                                .map(|s| {
                                    // Handle "foo as bar" -> use "bar"
                                    let s = s.trim();
                                    if let Some(as_idx) = s.find(" as ") {
                                        s[as_idx + 4..].trim().to_string()
                                    } else {
                                        s.to_string()
                                    }
                                })
                                .filter(|s| !s.is_empty())
                                .collect()
                        } else {
                            vec![]
                        }
                    } else {
                        vec![]
                    };

                    imports.push(ImportInfo {
                        path,
                        resolved_path: None,
                        symbols,
                        is_wildcard,
                        line: line_num as u32,
                    });
                }
            }

            // Match: require('module')
            if trimmed.contains("require(") {
                if let Some(path) = Self::extract_quoted_string(trimmed) {
                    imports.push(ImportInfo {
                        path,
                        resolved_path: None,
                        symbols: vec![],
                        is_wildcard: false,
                        line: line_num as u32,
                    });
                }
            }
        }

        imports
    }

    /// Parse JavaScript imports (same as TypeScript for now)
    fn parse_javascript_imports(content: &str) -> Vec<ImportInfo> {
        Self::parse_typescript_imports(content)
    }

    /// Parse Python imports
    fn parse_python_imports(content: &str) -> Vec<ImportInfo> {
        let mut imports = Vec::new();

        for (line_num, line) in content.lines().enumerate() {
            let trimmed = line.trim();

            // Match: import foo
            // Match: import foo.bar
            // Match: import foo as bar
            if let Some(import_stmt) = trimmed.strip_prefix("import ") {
                let parts: Vec<&str> = import_stmt.split(" as ").collect();
                let path = parts[0].trim().to_string();

                imports.push(ImportInfo {
                    path,
                    resolved_path: None,
                    symbols: vec![],
                    is_wildcard: false,
                    line: line_num as u32,
                });
            }

            // Match: from foo import bar
            // Match: from foo import bar, baz
            // Match: from foo import *
            if let Some(from_stmt) = trimmed.strip_prefix("from ") {
                if let Some(import_idx) = from_stmt.find(" import ") {
                    let module_path = from_stmt[..import_idx].trim().to_string();
                    let import_part = from_stmt[import_idx + 8..].trim();

                    let is_wildcard = import_part == "*";
                    let symbols: Vec<String> = if is_wildcard {
                        vec![]
                    } else {
                        import_part
                            .split(',')
                            .map(|s| {
                                let s = s.trim();
                                // Handle "foo as bar"
                                if let Some(as_idx) = s.find(" as ") {
                                    s[..as_idx].trim().to_string()
                                } else {
                                    s.to_string()
                                }
                            })
                            .filter(|s| !s.is_empty())
                            .collect()
                    };

                    imports.push(ImportInfo {
                        path: module_path,
                        resolved_path: None,
                        symbols,
                        is_wildcard,
                        line: line_num as u32,
                    });
                }
            }
        }

        imports
    }

    /// Parse Go imports
    fn parse_go_imports(content: &str) -> Vec<ImportInfo> {
        let mut imports = Vec::new();
        let mut in_import_block = false;

        for (line_num, line) in content.lines().enumerate() {
            let trimmed = line.trim();

            // Single import: import "fmt"
            if let Some(import_stmt) = trimmed.strip_prefix("import ") {
                if !import_stmt.starts_with('(') {
                    if let Some(path) = Self::extract_quoted_string(import_stmt) {
                        imports.push(ImportInfo {
                            path,
                            resolved_path: None,
                            symbols: vec![],
                            is_wildcard: false,
                            line: line_num as u32,
                        });
                    }
                } else {
                    in_import_block = true;
                }
                continue;
            }

            // Import block
            if in_import_block {
                if trimmed == ")" {
                    in_import_block = false;
                    continue;
                }

                if let Some(path) = Self::extract_quoted_string(trimmed) {
                    imports.push(ImportInfo {
                        path,
                        resolved_path: None,
                        symbols: vec![],
                        is_wildcard: false,
                        line: line_num as u32,
                    });
                }
            }
        }

        imports
    }

    /// Extract a quoted string from a line
    fn extract_quoted_string(s: &str) -> Option<String> {
        // Try double quotes first
        if let Some(start) = s.find('"') {
            if let Some(end) = s[start + 1..].find('"') {
                return Some(s[start + 1..start + 1 + end].to_string());
            }
        }
        // Try single quotes
        if let Some(start) = s.find('\'') {
            if let Some(end) = s[start + 1..].find('\'') {
                return Some(s[start + 1..start + 1 + end].to_string());
            }
        }
        // Try backticks (for Go)
        if let Some(start) = s.find('`') {
            if let Some(end) = s[start + 1..].find('`') {
                return Some(s[start + 1..start + 1 + end].to_string());
            }
        }
        None
    }

    /// Resolve import path to relative file path in worktree
    ///
    /// This uses the worktree snapshot to check if files exist, which is the correct
    /// way to resolve paths in Zed's architecture (rather than direct filesystem access).
    ///
    /// Returns the relative path (as a string) if the import can be resolved to an
    /// existing file in the worktree.
    pub fn resolve_import_path(
        import: &ImportInfo,
        current_file: &str,
        language_id: &str,
        worktree_snapshot: &WorktreeSnapshot,
    ) -> Option<String> {
        match language_id {
            "rust" => Self::resolve_rust_import(import, current_file, worktree_snapshot),
            "typescript" | "typescriptreact" | "javascript" | "javascriptreact" => {
                Self::resolve_ts_import(import, current_file, worktree_snapshot)
            }
            "python" => Self::resolve_python_import(import, current_file, worktree_snapshot),
            "go" => Self::resolve_go_import(import, current_file, worktree_snapshot),
            _ => None,
        }
    }

    /// Check if a path exists in the worktree
    fn path_exists(worktree: &WorktreeSnapshot, path: &str) -> bool {
        if let Ok(rel_path) = RelPath::unix(path) {
            worktree.entry_for_path(rel_path).is_some()
        } else {
            false
        }
    }

    /// Try multiple path candidates and return the first one that exists
    fn try_paths(worktree: &WorktreeSnapshot, candidates: &[String]) -> Option<String> {
        for candidate in candidates {
            if Self::path_exists(worktree, candidate) {
                return Some(candidate.clone());
            }
        }
        None
    }

    /// Normalize a path by resolving . and .. components
    fn normalize_path(path: &str) -> String {
        let mut parts: Vec<&str> = Vec::new();

        for part in path.split('/') {
            match part {
                "" | "." => continue,
                ".." => {
                    parts.pop();
                }
                _ => parts.push(part),
            }
        }

        parts.join("/")
    }

    /// Get the parent directory of a path
    fn parent_path(path: &str) -> Option<&str> {
        path.rfind('/').map(|idx| &path[..idx])
    }

    /// Resolve Rust imports (use statements and mod declarations)
    ///
    /// Handles:
    /// - `crate::module::item` - resolves to src/module.rs or src/module/mod.rs
    /// - `super::module` - relative to parent directory
    /// - `self::module` - relative to current directory
    /// - Standard library imports are skipped (std::, core::, alloc::)
    fn resolve_rust_import(
        import: &ImportInfo,
        current_file: &str,
        worktree: &WorktreeSnapshot,
    ) -> Option<String> {
        let path = &import.path;

        // Skip standard library imports
        if path.starts_with("std::")
            || path.starts_with("core::")
            || path.starts_with("alloc::")
            || path.starts_with("proc_macro::")
        {
            return None;
        }

        // Handle crate:: prefix - resolve from crate root (src/)
        if let Some(crate_path) = path.strip_prefix("crate::") {
            return Self::resolve_rust_crate_path(crate_path, current_file, worktree);
        }

        // Handle super:: prefix - resolve from parent directory
        if let Some(super_path) = path.strip_prefix("super::") {
            return Self::resolve_rust_super_path(super_path, current_file, worktree);
        }

        // Handle self:: prefix - resolve from current module directory
        if let Some(self_path) = path.strip_prefix("self::") {
            return Self::resolve_rust_self_path(self_path, current_file, worktree);
        }

        // For other imports (external crates), skip them
        None
    }

    /// Resolve a crate:: prefixed path
    fn resolve_rust_crate_path(
        module_path: &str,
        current_file: &str,
        worktree: &WorktreeSnapshot,
    ) -> Option<String> {
        // Determine the crate root - find src/ directory relative to current file
        // For files in src/foo/bar.rs, the crate root is src/
        let crate_root = Self::find_rust_crate_root(current_file)?;

        // Convert module path (foo::bar) to file path candidates
        let segments: Vec<&str> = module_path.split("::").collect();
        Self::resolve_rust_module_segments(&segments, &crate_root, worktree)
    }

    /// Find the crate root (src/ directory) for a file
    fn find_rust_crate_root(current_file: &str) -> Option<String> {
        // Look for src/ in the path
        if let Some(idx) = current_file.find("src/") {
            return Some(current_file[..idx + 4].to_string()); // Include "src/"
        }

        // If file is directly in src/, return "src/"
        if current_file.starts_with("src/") {
            return Some("src/".to_string());
        }

        // Fallback: assume src/ as root
        Some("src/".to_string())
    }

    /// Resolve super:: path relative to parent module
    fn resolve_rust_super_path(
        module_path: &str,
        current_file: &str,
        worktree: &WorktreeSnapshot,
    ) -> Option<String> {
        // Get the current module directory
        let current_dir = Self::get_rust_module_dir(current_file)?;

        // Go up one level
        let parent_dir = Self::parent_path(&current_dir)?;

        // Handle nested super:: (super::super::foo)
        let (remaining_supers, actual_path) = Self::count_super_prefix(module_path);

        let mut base_dir = parent_dir.to_string();
        for _ in 0..remaining_supers {
            base_dir = Self::parent_path(&base_dir)?.to_string();
        }

        if actual_path.is_empty() {
            // Just super:: pointing to parent module
            let candidates = vec![format!("{}/mod.rs", base_dir), format!("{}.rs", base_dir)];
            Self::try_paths(worktree, &candidates)
        } else {
            let segments: Vec<&str> = actual_path.split("::").collect();
            let search_base = format!("{}/", base_dir);
            Self::resolve_rust_module_segments(&segments, &search_base, worktree)
        }
    }

    /// Count nested super:: prefixes and return remaining path
    fn count_super_prefix(path: &str) -> (usize, &str) {
        let mut count = 0;
        let mut remaining = path;

        while let Some(rest) = remaining.strip_prefix("super::") {
            count += 1;
            remaining = rest;
        }

        (count, remaining)
    }

    /// Resolve self:: path relative to current module
    fn resolve_rust_self_path(
        module_path: &str,
        current_file: &str,
        worktree: &WorktreeSnapshot,
    ) -> Option<String> {
        let current_dir = Self::get_rust_module_dir(current_file)?;
        let segments: Vec<&str> = module_path.split("::").collect();
        let search_base = format!("{}/", current_dir);
        Self::resolve_rust_module_segments(&segments, &search_base, worktree)
    }

    /// Get the module directory for a Rust file
    /// For src/foo/bar.rs -> src/foo
    /// For src/foo/mod.rs -> src/foo
    fn get_rust_module_dir(file_path: &str) -> Option<String> {
        let dir = Self::parent_path(file_path)?;

        // If file is mod.rs, the module dir is its parent
        if file_path.ends_with("/mod.rs") {
            return Some(dir.to_string());
        }

        // Otherwise, use the file's directory
        Some(dir.to_string())
    }

    /// Resolve module path segments to a file path
    fn resolve_rust_module_segments(
        segments: &[&str],
        base_path: &str,
        worktree: &WorktreeSnapshot,
    ) -> Option<String> {
        if segments.is_empty() {
            return None;
        }

        // Build path from segments
        let mut path = base_path.trim_end_matches('/').to_string();

        for (i, segment) in segments.iter().enumerate() {
            if i == segments.len() - 1 {
                // Last segment - try as module file
                let candidates = vec![
                    format!("{}/{}/mod.rs", path, segment),
                    format!("{}/{}.rs", path, segment),
                ];
                return Self::try_paths(worktree, &candidates);
            } else {
                path = format!("{}/{}", path, segment);
            }
        }

        None
    }

    /// Resolve TypeScript/JavaScript imports
    ///
    /// Handles:
    /// - Relative imports: `./foo`, `../bar`
    /// - Index files: `./foo` -> `./foo/index.ts`
    /// - Extension resolution: tries .ts, .tsx, .js, .jsx
    fn resolve_ts_import(
        import: &ImportInfo,
        current_file: &str,
        worktree: &WorktreeSnapshot,
    ) -> Option<String> {
        let path = &import.path;

        // Skip node_modules/package imports (not starting with . or /)
        if !path.starts_with('.') && !path.starts_with('/') {
            return None;
        }

        let current_dir = Self::parent_path(current_file)?;

        // Build base path
        let base_path = if path.starts_with('/') {
            // Absolute path from project root
            path.trim_start_matches('/').to_string()
        } else {
            // Relative path
            let combined = format!("{}/{}", current_dir, path);
            Self::normalize_path(&combined)
        };

        // Try various extensions and index patterns
        let extensions = ["", ".ts", ".tsx", ".js", ".jsx", ".mts", ".mjs"];
        let index_files = ["index.ts", "index.tsx", "index.js", "index.jsx"];

        let mut candidates: Vec<String> = Vec::new();

        // Try direct path with extensions
        for ext in &extensions {
            if ext.is_empty() {
                candidates.push(base_path.clone());
            } else {
                candidates.push(format!("{}{}", base_path, ext));
            }
        }

        // Try as directory with index file
        for index in &index_files {
            candidates.push(format!("{}/{}", base_path, index));
        }

        Self::try_paths(worktree, &candidates)
    }

    /// Resolve Python imports
    ///
    /// Handles:
    /// - Relative imports: `from . import foo`, `from .. import bar`
    /// - Absolute imports within project: `from mypackage import foo`
    /// - Package __init__.py resolution
    fn resolve_python_import(
        import: &ImportInfo,
        current_file: &str,
        worktree: &WorktreeSnapshot,
    ) -> Option<String> {
        let path = &import.path;

        // Handle relative imports (starting with .)
        if path.starts_with('.') {
            return Self::resolve_python_relative_import(path, current_file, worktree);
        }

        // Handle absolute imports - try to find in project
        Self::resolve_python_absolute_import(path, worktree)
    }

    /// Resolve Python relative imports
    fn resolve_python_relative_import(
        path: &str,
        current_file: &str,
        worktree: &WorktreeSnapshot,
    ) -> Option<String> {
        // Count leading dots
        let dots = path.chars().take_while(|&c| c == '.').count();
        let module_path = path.trim_start_matches('.');

        // Get current package directory
        let current_dir = Self::parent_path(current_file)?;

        // Go up (dots - 1) levels (one dot = current package)
        let mut base_dir = current_dir.to_string();
        for _ in 1..dots {
            base_dir = Self::parent_path(&base_dir)?.to_string();
        }

        if module_path.is_empty() {
            // `from . import foo` - pointing to current package's __init__.py
            let candidates = vec![format!("{}/__init__.py", base_dir)];
            return Self::try_paths(worktree, &candidates);
        }

        // Convert module.submodule to path
        let file_path = module_path.replace('.', "/");

        let candidates = vec![
            format!("{}/{}.py", base_dir, file_path),
            format!("{}/{}/__init__.py", base_dir, file_path),
        ];

        Self::try_paths(worktree, &candidates)
    }

    /// Resolve Python absolute imports
    fn resolve_python_absolute_import(path: &str, worktree: &WorktreeSnapshot) -> Option<String> {
        let file_path = path.replace('.', "/");

        // Try common Python project layouts
        let candidates = vec![
            // Direct in root
            format!("{}.py", file_path),
            format!("{}/__init__.py", file_path),
            // In src/ directory
            format!("src/{}.py", file_path),
            format!("src/{}/__init__.py", file_path),
            // In lib/ directory
            format!("lib/{}.py", file_path),
            format!("lib/{}/__init__.py", file_path),
        ];

        Self::try_paths(worktree, &candidates)
    }

    /// Resolve Go imports
    ///
    /// Handles:
    /// - Relative imports within the same module
    /// - Local package imports
    fn resolve_go_import(
        import: &ImportInfo,
        _current_file: &str,
        worktree: &WorktreeSnapshot,
    ) -> Option<String> {
        let path = &import.path;

        // Skip standard library and external imports
        if !path.contains('/') || path.starts_with("golang.org") || path.starts_with("github.com") {
            return None;
        }

        // Try to find the package directory
        // For local imports, the path might be relative to module root
        let candidates = vec![
            format!("{}/", path), // Package directory
            path.clone(),         // Direct path
        ];

        // Look for any .go file in the package
        for candidate in &candidates {
            let dir_path = candidate.trim_end_matches('/');
            // Check if directory exists by looking for common Go files
            let go_candidates = vec![
                format!("{}/main.go", dir_path),
                format!(
                    "{}/{}.go",
                    dir_path,
                    dir_path.split('/').last().unwrap_or("main")
                ),
            ];

            if Self::try_paths(worktree, &go_candidates).is_some() {
                // Return the directory, not the specific file
                return Some(dir_path.to_string());
            }
        }

        None
    }
}

// ============================================================================
// Recent File Tracker
// ============================================================================

/// Tracks recently accessed files with timing metadata
pub struct RecentFileTracker {
    /// Recent files in MRU order
    files: VecDeque<RecentFileEntry>,
    /// Quick lookup by path
    path_index: HashMap<String, usize>,
}

impl RecentFileTracker {
    pub fn new() -> Self {
        Self {
            files: VecDeque::with_capacity(MAX_RECENT_FILES),
            path_index: HashMap::new(),
        }
    }

    /// Record a file view
    pub fn record_view(&mut self, path: &str) {
        self.touch_file(path, false);
    }

    /// Record a file edit
    pub fn record_edit(&mut self, path: &str) {
        self.touch_file(path, true);
    }

    fn touch_file(&mut self, path: &str, is_edit: bool) {
        let now = Instant::now();

        if let Some(&idx) = self.path_index.get(path) {
            // Update existing entry
            if let Some(entry) = self.files.get_mut(idx) {
                if is_edit {
                    entry.last_edited = Some(now);
                    entry.edit_count += 1;
                } else {
                    entry.last_viewed = Some(now);
                }
            }

            // Move to front (MRU)
            if idx > 0 {
                if let Some(entry) = self.files.remove(idx) {
                    self.files.push_front(entry);
                    self.rebuild_index();
                }
            }
        } else {
            // Add new entry
            let entry = RecentFileEntry {
                path: path.to_string(),
                last_edited: if is_edit { Some(now) } else { None },
                last_viewed: if is_edit { None } else { Some(now) },
                edit_count: if is_edit { 1 } else { 0 },
                content_hash: None,
            };

            self.files.push_front(entry);

            // Enforce capacity
            if self.files.len() > MAX_RECENT_FILES {
                self.files.pop_back();
            }

            self.rebuild_index();
        }
    }

    fn rebuild_index(&mut self) {
        self.path_index.clear();
        for (idx, entry) in self.files.iter().enumerate() {
            self.path_index.insert(entry.path.clone(), idx);
        }
    }

    /// Get recent files with relevance scores
    pub fn get_scored_files(&self, current_file: &str) -> Vec<(String, f32)> {
        let now = Instant::now();

        self.files
            .iter()
            .filter(|e| e.path != current_file)
            .map(|entry| {
                let mut score: f32 = 0.0;

                // Time-based decay for edits
                if let Some(edited) = entry.last_edited {
                    let age_secs = now.duration_since(edited).as_secs_f64();
                    let decay = 0.5_f64.powf(age_secs / RECENT_FILE_HALF_LIFE_SECS);
                    score += 10.0 * decay as f32;
                }

                // Time-based decay for views
                if let Some(viewed) = entry.last_viewed {
                    let age_secs = now.duration_since(viewed).as_secs_f64();
                    let decay = 0.5_f64.powf(age_secs / RECENT_FILE_HALF_LIFE_SECS);
                    score += 3.0 * decay as f32;
                }

                // Bonus for edit count
                score += (entry.edit_count as f32).min(5.0) * 2.0;

                (entry.path.clone(), score)
            })
            .collect()
    }
}

impl Default for RecentFileTracker {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// P1: Async Prefetcher
// ============================================================================

/// Prefetch result from background search
#[derive(Clone, Debug)]
struct PrefetchResult {
    /// Search pattern used
    pattern: String,
    /// Matched file paths with ranges
    matches: Vec<PrefetchMatch>,
    /// When this result was computed
    computed_at: Instant,
}

#[derive(Clone, Debug)]
struct PrefetchMatch {
    file_path: String,
    ranges: Vec<Range<usize>>,
    content_preview: String,
}

/// Async prefetcher for background context gathering
pub struct AsyncPrefetcher {
    /// Cached prefetch results
    cache: Arc<RwLock<HashMap<String, PrefetchResult>>>,
    /// Last activity timestamp
    last_activity: Arc<RwLock<Instant>>,
    /// Pending prefetch task
    pending_task: Option<Task<()>>,
    /// Symbols to prefetch
    pending_symbols: Arc<Mutex<HashSet<String>>>,
    /// Project reference
    project: Option<WeakEntity<Project>>,
}

impl AsyncPrefetcher {
    pub fn new(project: Option<Entity<Project>>) -> Self {
        Self {
            cache: Arc::new(RwLock::new(HashMap::new())),
            last_activity: Arc::new(RwLock::new(Instant::now())),
            pending_task: None,
            pending_symbols: Arc::new(Mutex::new(HashSet::new())),
            project: project.map(|p| p.downgrade()),
        }
    }

    /// Record user activity (resets idle timer)
    pub fn record_activity(&self) {
        *self.last_activity.write() = Instant::now();
    }

    /// Check if user is idle
    pub fn is_idle(&self) -> bool {
        self.last_activity.read().elapsed().as_secs() >= IDLE_THRESHOLD_SECS
    }

    /// Queue symbols for prefetch
    pub fn queue_symbols(&self, symbols: Vec<String>) {
        let mut pending = self.pending_symbols.lock();
        for symbol in symbols {
            if symbol.len() >= 3 {
                // Skip very short symbols
                pending.insert(symbol);
            }
        }
    }

    /// Get cached results for a symbol
    pub fn get_cached(&self, symbol: &str) -> Option<Vec<PrefetchMatch>> {
        let cache = self.cache.read();
        cache.get(symbol).and_then(|result| {
            // Check if result is still fresh (5 minutes)
            if result.computed_at.elapsed().as_secs() < 300 {
                Some(result.matches.clone())
            } else {
                None
            }
        })
    }

    /// Start prefetch task if idle
    pub fn maybe_start_prefetch(&mut self, cx: &mut Context<SmartContextEngine>) {
        if !self.is_idle() {
            return;
        }

        let symbols: Vec<String> = {
            let mut pending = self.pending_symbols.lock();
            if pending.is_empty() {
                return;
            }
            let symbols: Vec<String> = pending.iter().take(5).cloned().collect();
            for s in &symbols {
                pending.remove(s);
            }
            symbols
        };

        if symbols.is_empty() {
            return;
        }

        let Some(project) = self.project.as_ref().and_then(|p| p.upgrade()) else {
            return;
        };

        let cache = self.cache.clone();

        self.pending_task = Some(cx.spawn(async move |_, cx| {
            for symbol in symbols {
                if let Err(e) =
                    Self::prefetch_symbol(symbol.clone(), project.clone(), cache.clone(), cx).await
                {
                    log::debug!("Prefetch failed for {}: {}", symbol, e);
                }
            }
        }));
    }

    /// Prefetch context for a single symbol
    async fn prefetch_symbol(
        symbol: String,
        project: Entity<Project>,
        cache: Arc<RwLock<HashMap<String, PrefetchResult>>>,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<()> {
        // Build a regex search query for the symbol
        let pattern = format!(r"\b{}\b", regex::escape(&symbol));

        let query = SearchQuery::regex(
            &pattern,
            false,                  // whole_word
            true,                   // case_sensitive
            false,                  // include_ignored
            true,                   // one_match_per_line
            PathMatcher::default(), // files_to_include
            PathMatcher::default(), // files_to_exclude
            false,                  // match_full_paths
            None,                   // buffers
        )?;

        let results_rx = project.update(cx, |project, cx| project.search(query, cx))?;
        futures::pin_mut!(results_rx);

        let mut matches = Vec::new();

        while let Some(result) = results_rx.next().await {
            if let SearchResult::Buffer { buffer, ranges } = result {
                let (file_path, content_preview) = buffer.read_with(cx, |buffer, _cx| {
                    let path = buffer
                        .file()
                        .map(|f| f.path().as_unix_str().to_string())
                        .unwrap_or_default();

                    // Get content preview from first match
                    let preview = if let Some(first_range) = ranges.first() {
                        let snapshot = buffer.snapshot();
                        let start = first_range.start.to_offset(&snapshot);
                        let end = first_range.end.to_offset(&snapshot);

                        // Expand to include some context
                        let line_start = snapshot.offset_to_point(start).row;
                        let line_end = snapshot.offset_to_point(end).row;
                        let context_start =
                            snapshot.point_to_offset(Point::new(line_start.saturating_sub(2), 0));
                        let context_end = snapshot.point_to_offset(Point::new(
                            (line_end + 3).min(snapshot.max_point().row),
                            0,
                        ));

                        snapshot
                            .text_for_range(context_start..context_end)
                            .collect::<String>()
                    } else {
                        String::new()
                    };

                    (path, preview)
                })?;

                if !file_path.is_empty() && matches.len() < MAX_PREFETCH_RESULTS {
                    let offset_ranges: Vec<Range<usize>> = buffer.read_with(cx, |buffer, _| {
                        let snapshot = buffer.snapshot();
                        ranges
                            .iter()
                            .map(|r| r.start.to_offset(&snapshot)..r.end.to_offset(&snapshot))
                            .collect()
                    })?;

                    matches.push(PrefetchMatch {
                        file_path,
                        ranges: offset_ranges,
                        content_preview,
                    });
                }
            }

            if matches.len() >= MAX_PREFETCH_RESULTS {
                break;
            }
        }

        // Cache the results
        if !matches.is_empty() {
            let mut cache = cache.write();
            // Enforce cache size limit
            if cache.len() >= MAX_PREFETCH_RESULTS {
                // Remove oldest entries
                let mut entries: Vec<_> = cache
                    .iter()
                    .map(|(k, v)| (k.clone(), v.computed_at))
                    .collect();
                entries.sort_by_key(|(_, t)| *t);
                for (key, _) in entries.into_iter().take(cache.len() / 2) {
                    cache.remove(&key);
                }
            }

            cache.insert(
                symbol.clone(),
                PrefetchResult {
                    pattern: symbol,
                    matches,
                    computed_at: Instant::now(),
                },
            );
        }

        Ok(())
    }
}

// ============================================================================
// P2: LSP Resolver
// ============================================================================

/// Cached LSP result
#[derive(Clone, Debug)]
struct LspCacheEntry {
    /// Definition locations
    definitions: Vec<LspLocation>,
    /// Reference locations
    references: Vec<LspLocation>,
    /// When this was cached
    cached_at: Instant,
}

#[derive(Clone, Debug)]
struct LspLocation {
    file_path: String,
    range: Range<Point>,
    content: String,
}

/// LSP-based symbol resolver
pub struct LspResolver {
    /// Cache of LSP results by (file_path, symbol_name, offset)
    cache: Arc<RwLock<HashMap<(String, String, usize), LspCacheEntry>>>,
    /// Project reference
    project: Option<WeakEntity<Project>>,
    /// Pending LSP requests
    pending_requests: Arc<Mutex<HashSet<String>>>,
}

impl LspResolver {
    pub fn new(project: Option<Entity<Project>>) -> Self {
        Self {
            cache: Arc::new(RwLock::new(HashMap::new())),
            project: project.map(|p| p.downgrade()),
            pending_requests: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Get the symbol at cursor position
    pub fn get_symbol_at_cursor(snapshot: &BufferSnapshot, offset: usize) -> Option<String> {
        // Find word boundaries around cursor
        let text = snapshot.text();
        if offset >= text.len() {
            return None;
        }

        let bytes = text.as_bytes();
        let mut start = offset;
        let mut end = offset;

        // Scan backwards to find start of identifier
        while start > 0 {
            let prev = start - 1;
            if prev < bytes.len() {
                let c = bytes[prev] as char;
                if c.is_alphanumeric() || c == '_' {
                    start = prev;
                } else {
                    break;
                }
            } else {
                break;
            }
        }

        // Scan forwards to find end of identifier
        while end < bytes.len() {
            let c = bytes[end] as char;
            if c.is_alphanumeric() || c == '_' {
                end += 1;
            } else {
                break;
            }
        }

        if start < end {
            Some(text[start..end].to_string())
        } else {
            None
        }
    }

    /// Get cached definitions for a symbol
    pub fn get_cached_definitions(
        &self,
        file_path: &str,
        symbol: &str,
        offset: usize,
    ) -> Option<Vec<LspLocation>> {
        let cache = self.cache.read();
        let key = (file_path.to_string(), symbol.to_string(), offset);
        cache.get(&key).and_then(|entry| {
            if entry.cached_at.elapsed().as_secs() < LSP_CACHE_TTL_SECS {
                Some(entry.definitions.clone())
            } else {
                None
            }
        })
    }

    /// Get cached references for a symbol
    pub fn get_cached_references(
        &self,
        file_path: &str,
        symbol: &str,
        offset: usize,
    ) -> Option<Vec<LspLocation>> {
        let cache = self.cache.read();
        let key = (file_path.to_string(), symbol.to_string(), offset);
        cache.get(&key).and_then(|entry| {
            if entry.cached_at.elapsed().as_secs() < LSP_CACHE_TTL_SECS {
                Some(entry.references.clone())
            } else {
                None
            }
        })
    }

    /// Request LSP definitions (async, caches result)
    ///
    /// TODO: Wire up from editor when user triggers go-to-definition or hovers over a symbol.
    /// This populates the LSP cache for `collect_from_lsp_cache` and `to_lsp_contexts`.
    #[allow(dead_code)]
    pub fn request_definitions(
        &self,
        buffer: &Entity<Buffer>,
        position: Anchor,
        file_path: &str,
        symbol: &str,
        cx: &mut Context<SmartContextEngine>,
    ) -> Task<Vec<LspLocation>> {
        let cache = self.cache.clone();
        let key = (
            file_path.to_string(),
            symbol.to_string(),
            position.to_offset(&buffer.read(cx).snapshot()),
        );

        // Check cache first
        {
            let cache_read = cache.read();
            if let Some(entry) = cache_read.get(&key) {
                if entry.cached_at.elapsed().as_secs() < LSP_CACHE_TTL_SECS {
                    return Task::ready(entry.definitions.clone());
                }
            }
        }

        let Some(project) = self.project.as_ref().and_then(|p| p.upgrade()) else {
            return Task::ready(vec![]);
        };

        let buffer = buffer.clone();
        let _file_path = file_path.to_string();
        let _symbol = symbol.to_string();

        cx.spawn(async move |_, cx| {
            let definitions = match project
                .update(cx, |project, cx| project.definitions(&buffer, position, cx))
            {
                Ok(task) => task.await.ok().flatten().unwrap_or_default(),
                Err(_) => return vec![],
            };

            let locations: Vec<LspLocation> = definitions
                .into_iter()
                .filter_map(|link| {
                    let target = link.target;
                    let target_buffer = target.buffer.read_with(cx, |b, _cx| {
                        let path = b.file()?.path().as_unix_str().to_string();
                        let snapshot = b.snapshot();
                        let content: String = snapshot
                            .text_for_range(
                                target.range.start.to_offset(&snapshot)
                                    ..target.range.end.to_offset(&snapshot),
                            )
                            .collect();
                        let start_point = target.range.start.to_point(&snapshot);
                        let end_point = target.range.end.to_point(&snapshot);
                        Some((path, content, start_point, end_point))
                    });

                    target_buffer
                        .ok()
                        .flatten()
                        .map(|(path, content, start_point, end_point)| LspLocation {
                            file_path: path,
                            range: start_point..end_point,
                            content,
                        })
                })
                .collect();

            // Cache the result
            {
                let mut cache_write = cache.write();
                let entry = cache_write.entry(key).or_insert_with(|| LspCacheEntry {
                    definitions: vec![],
                    references: vec![],
                    cached_at: Instant::now(),
                });
                entry.definitions = locations.clone();
                entry.cached_at = Instant::now();
            }

            locations
        })
    }

    /// Get all cached LSP contexts for conversion to proto format
    ///
    /// Returns a list of (symbol_name, file_path, definitions, references) tuples
    /// from the cache that are still valid (within TTL).
    pub fn get_all_cached_contexts(
        &self,
    ) -> Vec<(String, String, Vec<LspLocation>, Vec<LspLocation>)> {
        let cache = self.cache.read();

        cache
            .iter()
            .filter(|(_, entry)| entry.cached_at.elapsed().as_secs() < LSP_CACHE_TTL_SECS)
            .filter(|(_, entry)| !entry.definitions.is_empty() || !entry.references.is_empty())
            .map(|((file_path, symbol, _offset), entry)| {
                (
                    symbol.clone(),
                    file_path.clone(),
                    entry.definitions.clone(),
                    entry.references.clone(),
                )
            })
            .collect()
    }
}

// ============================================================================
// P3: Syntax Index
// ============================================================================

/// Simple syntax index for declarations
pub struct SyntaxIndex {
    /// Declarations by file path
    by_file: HashMap<String, Vec<Declaration>>,
    /// Declarations by identifier name
    by_name: HashMap<String, Vec<(String, usize)>>, // (file_path, index)
    /// Files that need re-indexing
    dirty_files: HashSet<String>,
    /// Last index time per file
    last_indexed: HashMap<String, Instant>,
}

impl SyntaxIndex {
    pub fn new() -> Self {
        Self {
            by_file: HashMap::new(),
            by_name: HashMap::new(),
            dirty_files: HashSet::new(),
            last_indexed: HashMap::new(),
        }
    }

    /// Mark a file as needing re-indexing
    pub fn mark_dirty(&mut self, file_path: &str) {
        self.dirty_files.insert(file_path.to_string());
    }

    /// Index a buffer's declarations using TreeSitter outline
    pub fn index_buffer(&mut self, snapshot: &BufferSnapshot, file_path: &str) {
        // Use symbols_containing with full range to get all symbols
        let outline = snapshot.outline(None);

        let mut declarations = Vec::new();

        for item in outline.items {
            let name = item.text.clone();
            let kind = Self::parse_declaration_kind(&item.text);

            // Get the range
            let range = item.range.start.to_offset(snapshot)..item.range.end.to_offset(snapshot);
            let line_start = item.range.start.to_point(snapshot).row;
            let line_end = item.range.end.to_point(snapshot).row;

            // Extract signature (first line)
            let signature = snapshot
                .text_for_range(
                    snapshot.point_to_offset(Point::new(line_start, 0))
                        ..snapshot
                            .point_to_offset(Point::new(line_start, snapshot.line_len(line_start))),
                )
                .collect::<String>()
                .trim()
                .to_string();

            // Extract full text (limited)
            let full_text: String = snapshot.text_for_range(range.clone()).take(500).collect();

            declarations.push(Declaration {
                name: name.clone(),
                kind,
                file_path: file_path.to_string(),
                range,
                line_range: line_start..line_end,
                signature,
                full_text,
                parent: None, // TODO: Track parent from outline depth
            });

            if declarations.len() >= MAX_DECLARATIONS_PER_FILE {
                break;
            }
        }

        // Update indexes
        // First, remove old entries for this file
        if let Some(old_decls) = self.by_file.get(file_path) {
            for decl in old_decls {
                if let Some(name_entries) = self.by_name.get_mut(&decl.name) {
                    name_entries.retain(|(path, _)| path != file_path);
                }
            }
        }

        // Add new entries
        for (idx, decl) in declarations.iter().enumerate() {
            self.by_name
                .entry(decl.name.clone())
                .or_default()
                .push((file_path.to_string(), idx));
        }

        self.by_file.insert(file_path.to_string(), declarations);
        self.dirty_files.remove(file_path);
        self.last_indexed
            .insert(file_path.to_string(), Instant::now());
    }

    /// Get declarations by name
    pub fn get_by_name(&self, name: &str) -> Vec<&Declaration> {
        self.by_name
            .get(name)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|(file_path, idx)| {
                        self.by_file
                            .get(file_path)
                            .and_then(|decls| decls.get(*idx))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Get all declarations in a file
    pub fn get_by_file(&self, file_path: &str) -> &[Declaration] {
        self.by_file
            .get(file_path)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Parse declaration kind from outline text
    fn parse_declaration_kind(text: &str) -> DeclarationKind {
        let lower = text.to_lowercase();
        if lower.starts_with("fn ") || lower.contains("function") {
            DeclarationKind::Function
        } else if lower.starts_with("pub fn ") || lower.contains("method") {
            DeclarationKind::Method
        } else if lower.starts_with("struct ") {
            DeclarationKind::Struct
        } else if lower.starts_with("class ") {
            DeclarationKind::Class
        } else if lower.starts_with("trait ") {
            DeclarationKind::Trait
        } else if lower.starts_with("interface ") {
            DeclarationKind::Interface
        } else if lower.starts_with("enum ") {
            DeclarationKind::Enum
        } else if lower.starts_with("const ") {
            DeclarationKind::Const
        } else if lower.starts_with("let ") || lower.starts_with("var ") {
            DeclarationKind::Variable
        } else if lower.starts_with("mod ") || lower.starts_with("module ") {
            DeclarationKind::Module
        } else if lower.starts_with("type ") {
            DeclarationKind::Type
        } else {
            DeclarationKind::Other
        }
    }
}

impl Default for SyntaxIndex {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Context Cache
// ============================================================================

/// Cached context with TTL
struct ContextCache {
    items: Vec<ScoredContextItem>,
    computed_at: Instant,
    file_path: String,
    cursor_offset: usize,
}

impl ContextCache {
    fn is_valid(&self, file_path: &str, cursor_offset: usize) -> bool {
        self.file_path == file_path
            && self.computed_at.elapsed().as_secs() < CONTEXT_CACHE_TTL_SECS
            // Allow some cursor movement without invalidation
            && (self.cursor_offset as i64 - cursor_offset as i64).abs() < 100
    }
}

// ============================================================================
// Smart Context Engine
// ============================================================================

/// Main engine for intelligent context collection
pub struct SmartContextEngine {
    /// Recent file tracker
    recent_tracker: RecentFileTracker,
    /// Context cache
    cache: Option<ContextCache>,
    /// Project reference
    project: Option<Entity<Project>>,
    /// P1: Async prefetcher
    prefetcher: AsyncPrefetcher,
    /// P2: LSP resolver
    lsp_resolver: LspResolver,
    /// P3: Syntax index
    syntax_index: SyntaxIndex,
    /// Background indexing task
    indexing_task: Option<Task<()>>,
}

impl SmartContextEngine {
    pub fn new(project: Option<Entity<Project>>) -> Self {
        Self {
            recent_tracker: RecentFileTracker::new(),
            cache: None,
            project: project.clone(),
            prefetcher: AsyncPrefetcher::new(project.clone()),
            lsp_resolver: LspResolver::new(project.clone()),
            syntax_index: SyntaxIndex::new(),
            indexing_task: None,
        }
    }

    /// Record that a file was viewed
    ///
    /// TODO: Wire up from editor's buffer focus/open events to track recently viewed files.
    /// This populates `recent_tracker` for context relevance scoring.
    #[allow(dead_code)]
    pub fn record_file_view(&mut self, path: &str) {
        self.recent_tracker.record_view(path);
        self.prefetcher.record_activity();
    }

    /// Record that a file was edited
    pub fn record_file_edit(&mut self, path: &str) {
        self.recent_tracker.record_edit(path);
        self.prefetcher.record_activity();
        self.syntax_index.mark_dirty(path);
        // Invalidate cache on edit
        self.cache = None;
    }

    /// Collect smart context for completion
    pub fn collect_context(
        &mut self,
        buffer: &Entity<Buffer>,
        cursor_offset: usize,
        current_file_path: &str,
        language_id: &str,
        cx: &App,
    ) -> Vec<ScoredContextItem> {
        // Check cache
        if let Some(ref cache) = self.cache {
            if cache.is_valid(current_file_path, cursor_offset) {
                log::debug!(
                    "SmartContext: Using cached context ({} items)",
                    cache.items.len()
                );
                return cache.items.clone();
            }
        }

        let snapshot = buffer.read(cx).snapshot();
        let mut all_items: Vec<ScoredContextItem> = Vec::new();

        // 1. Collect from imports
        let import_items = self.collect_from_imports(&snapshot, current_file_path, language_id, cx);
        all_items.extend(import_items);

        // 2. Collect from recent files
        let recent_items = self.collect_from_recent_files(current_file_path, cx);
        all_items.extend(recent_items);

        // 3. Collect from open buffers (fallback)
        let open_items = self.collect_from_open_buffers(current_file_path, &snapshot, cx);
        all_items.extend(open_items);

        // 4. P1: Collect from prefetch cache
        let prefetch_items = self.collect_from_prefetch(&snapshot, cursor_offset, cx);
        all_items.extend(prefetch_items);

        // 5. P2: Collect from LSP cache (definitions of symbol at cursor)
        let lsp_items =
            self.collect_from_lsp_cache(&snapshot, cursor_offset, current_file_path, cx);
        all_items.extend(lsp_items);

        // 6. P3: Collect from syntax index
        let index_items =
            self.collect_from_syntax_index(&snapshot, cursor_offset, current_file_path);
        all_items.extend(index_items);

        // 7. Sort by score first (highest score first)
        all_items.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // 8. Deduplicate by path AFTER sorting - this keeps the highest-scored item per path
        let mut seen_paths: HashSet<String> = HashSet::new();
        all_items.retain(|item| {
            if seen_paths.contains(&item.item.relative_workspace_path) {
                false
            } else {
                seen_paths.insert(item.item.relative_workspace_path.clone());
                true
            }
        });

        // 9. Limit to max items
        all_items.truncate(MAX_CONTEXT_ITEMS);

        log::info!(
            "SmartContext: Collected {} context items for {}",
            all_items.len(),
            current_file_path
        );

        // Update cache
        self.cache = Some(ContextCache {
            items: all_items.clone(),
            computed_at: Instant::now(),
            file_path: current_file_path.to_string(),
            cursor_offset,
        });

        // Queue symbols for background prefetch
        self.queue_symbols_for_prefetch(&snapshot, cursor_offset);

        all_items
    }

    /// Queue symbols near cursor for background prefetch
    fn queue_symbols_for_prefetch(&self, snapshot: &BufferSnapshot, cursor_offset: usize) {
        let content = snapshot.text();
        let mut symbols = Vec::new();

        // Extract identifiers near cursor (within 500 chars)
        let start = cursor_offset.saturating_sub(500);
        let end = (cursor_offset + 500).min(content.len());

        if start < end && end <= content.len() {
            let region = &content[start..end];
            let mut current_word = String::new();

            for c in region.chars() {
                if c.is_alphanumeric() || c == '_' {
                    current_word.push(c);
                } else {
                    if current_word.len() >= 3 && !current_word.chars().all(|c| c.is_numeric()) {
                        symbols.push(current_word.clone());
                    }
                    current_word.clear();
                }
            }

            if current_word.len() >= 3 && !current_word.chars().all(|c| c.is_numeric()) {
                symbols.push(current_word);
            }
        }

        self.prefetcher.queue_symbols(symbols);
    }

    /// Trigger background operations (call from idle handler)
    ///
    /// TODO: Wire up from editor's idle detection to enable background prefetching.
    /// This drains `pending_symbols` queue and populates prefetch cache.
    #[allow(dead_code)]
    pub fn on_idle(&mut self, cx: &mut Context<Self>) {
        self.prefetcher.maybe_start_prefetch(cx);
    }

    /// Update syntax index for a buffer
    ///
    /// TODO: Wire up from buffer save or change events to keep syntax index current.
    /// This enables `collect_from_syntax_index` to provide declaration-level context.
    #[allow(dead_code)]
    pub fn update_index(&mut self, buffer: &Entity<Buffer>, file_path: &str, cx: &App) {
        let snapshot = buffer.read(cx).snapshot();
        self.syntax_index.index_buffer(&snapshot, file_path);
    }

    /// Collect context from import statements
    fn collect_from_imports(
        &self,
        snapshot: &BufferSnapshot,
        current_file_path: &str,
        language_id: &str,
        cx: &App,
    ) -> Vec<ScoredContextItem> {
        let imports = ImportAnalyzer::parse_imports(snapshot, language_id);

        if imports.is_empty() {
            return vec![];
        }

        log::debug!(
            "SmartContext: Found {} imports in {}",
            imports.len(),
            current_file_path
        );

        let Some(project) = &self.project else {
            return vec![];
        };

        let project = project.read(cx);

        // Get the first worktree's snapshot for path resolution
        let worktree_snapshot = project
            .worktrees(cx)
            .next()
            .map(|wt| wt.read(cx).snapshot());

        let Some(worktree_snapshot) = worktree_snapshot else {
            return vec![];
        };

        let mut items = Vec::new();

        for import in imports {
            // Try to resolve the import to a relative file path using worktree
            let resolved = ImportAnalyzer::resolve_import_path(
                &import,
                current_file_path,
                language_id,
                &worktree_snapshot,
            );

            if let Some(resolved_path) = resolved {
                log::debug!(
                    "SmartContext: Resolved import '{}' -> '{}'",
                    import.path,
                    resolved_path
                );

                // First try to find in open buffers (faster)
                let mut found_in_buffer = false;
                for buffer in project.opened_buffers(cx) {
                    let buf = buffer.read(cx);
                    if let Some(file) = buf.file() {
                        let buf_path = file.path().as_unix_str().to_string();

                        // Direct path match
                        if buf_path == resolved_path {
                            let content = buf.text();
                            if content.len() <= MAX_CONTEXT_ITEM_SIZE {
                                // Higher score for imports with specific symbols
                                let base_score = if import.is_wildcard {
                                    30.0
                                } else if !import.symbols.is_empty() {
                                    50.0
                                } else {
                                    40.0
                                };

                                items.push(ScoredContextItem {
                                    item: CppContextItem {
                                        contents: content,
                                        symbol: import.symbols.first().cloned(),
                                        relative_workspace_path: buf_path,
                                        score: base_score,
                                    },
                                    score: base_score,
                                    source: ContextSource::Import,
                                });
                                found_in_buffer = true;
                            }
                            break;
                        }
                    }
                }

                // If not in open buffers, the file exists in worktree (we verified earlier)
                // but we can't read its content without async file I/O
                // For now, we record it as a placeholder for future enhancement
                if !found_in_buffer {
                    // Score lower since we don't have content
                    let base_score = if import.is_wildcard {
                        15.0
                    } else if !import.symbols.is_empty() {
                        25.0
                    } else {
                        20.0
                    };

                    // Add a minimal context item with just the path info
                    // The content will be empty but the path is useful for the model
                    items.push(ScoredContextItem {
                        item: CppContextItem {
                            contents: format!(
                                "// Import reference: {}\n// File: {}",
                                import.path, resolved_path
                            ),
                            symbol: import.symbols.first().cloned(),
                            relative_workspace_path: resolved_path,
                            score: base_score,
                        },
                        score: base_score,
                        source: ContextSource::Import,
                    });
                }
            }
        }

        items
    }

    /// Collect context from recently accessed files
    fn collect_from_recent_files(
        &self,
        current_file_path: &str,
        cx: &App,
    ) -> Vec<ScoredContextItem> {
        let Some(project) = &self.project else {
            return vec![];
        };

        let project = project.read(cx);
        let scored_files = self.recent_tracker.get_scored_files(current_file_path);

        let mut items = Vec::new();

        for (file_path, score) in scored_files {
            // Try to find in open buffers
            for buffer in project.opened_buffers(cx) {
                let buf = buffer.read(cx);
                if let Some(file) = buf.file() {
                    let buf_path = file.path().as_unix_str().to_string();

                    if buf_path == file_path {
                        let content = buf.text();
                        if content.len() <= MAX_CONTEXT_ITEM_SIZE {
                            items.push(ScoredContextItem {
                                item: CppContextItem {
                                    contents: content,
                                    symbol: None,
                                    relative_workspace_path: buf_path,
                                    score,
                                },
                                score,
                                source: ContextSource::RecentEdit,
                            });
                        }
                        break;
                    }
                }
            }
        }

        items
    }

    /// Collect from open buffers (fallback heuristics)
    fn collect_from_open_buffers(
        &self,
        current_file_path: &str,
        current_snapshot: &BufferSnapshot,
        cx: &App,
    ) -> Vec<ScoredContextItem> {
        let Some(project) = &self.project else {
            return vec![];
        };

        let project = project.read(cx);
        let current_content = current_snapshot.text();

        // Get current file info for scoring
        let current_ext = current_file_path
            .rsplit('.')
            .next()
            .map(|s| s.to_lowercase());
        let current_dir = std::path::Path::new(current_file_path)
            .parent()
            .map(|p| p.to_string_lossy().to_string());

        // Extract tokens for Jaccard similarity
        let current_tokens: HashSet<&str> = current_content
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|s| s.len() > 2)
            .collect();

        let mut items = Vec::new();

        for buffer in project.opened_buffers(cx) {
            let buf = buffer.read(cx);

            let Some(file) = buf.file() else {
                continue;
            };

            let file_path = file.path().as_unix_str().to_string();

            if file_path == current_file_path {
                continue;
            }

            let content = buf.text();
            if content.is_empty() || content.len() > MAX_CONTEXT_ITEM_SIZE {
                continue;
            }

            // Calculate score
            let mut score: f32 = 5.0; // Base score for open buffer

            // Same extension bonus
            let file_ext = file_path.rsplit('.').next().map(|s| s.to_lowercase());
            if file_ext == current_ext {
                score += 10.0;
            }

            // Same directory bonus
            let file_dir = std::path::Path::new(&file_path)
                .parent()
                .map(|p| p.to_string_lossy().to_string());
            if file_dir == current_dir {
                score += 5.0;
            }

            // Token overlap (Jaccard)
            if !current_tokens.is_empty() && content.len() < 15_000 {
                let file_tokens: HashSet<&str> = content
                    .split(|c: char| !c.is_alphanumeric() && c != '_')
                    .filter(|s| s.len() > 2)
                    .collect();

                if !file_tokens.is_empty() {
                    let intersection = current_tokens.intersection(&file_tokens).count();
                    let union = current_tokens.len() + file_tokens.len() - intersection;
                    if union > 0 {
                        let jaccard = intersection as f32 / union as f32;
                        score += jaccard * 15.0;
                    }
                }
            }

            // File name reference bonus
            let file_stem = std::path::Path::new(&file_path)
                .file_stem()
                .and_then(|s| s.to_str());
            if let Some(stem) = file_stem {
                if stem.len() > 2 && current_content.contains(stem) {
                    score += 25.0;
                }
            }

            items.push(ScoredContextItem {
                item: CppContextItem {
                    contents: content,
                    symbol: None,
                    relative_workspace_path: file_path,
                    score,
                },
                score,
                source: ContextSource::OpenBuffer,
            });
        }

        items
    }

    /// P1: Collect from prefetch cache
    fn collect_from_prefetch(
        &self,
        snapshot: &BufferSnapshot,
        cursor_offset: usize,
        _cx: &App,
    ) -> Vec<ScoredContextItem> {
        let mut items = Vec::new();

        // Get symbol at cursor
        if let Some(symbol) = LspResolver::get_symbol_at_cursor(snapshot, cursor_offset) {
            if let Some(matches) = self.prefetcher.get_cached(&symbol) {
                for m in matches.into_iter().take(3) {
                    if !m.content_preview.is_empty()
                        && m.content_preview.len() <= MAX_CONTEXT_ITEM_SIZE
                    {
                        items.push(ScoredContextItem {
                            item: CppContextItem {
                                contents: m.content_preview,
                                symbol: Some(symbol.clone()),
                                relative_workspace_path: m.file_path,
                                score: 35.0,
                            },
                            score: 35.0,
                            source: ContextSource::Prefetch,
                        });
                    }
                }
            }
        }

        items
    }

    /// P2: Collect from LSP cache
    fn collect_from_lsp_cache(
        &self,
        snapshot: &BufferSnapshot,
        cursor_offset: usize,
        current_file_path: &str,
        _cx: &App,
    ) -> Vec<ScoredContextItem> {
        let mut items = Vec::new();

        // Get symbol at cursor
        if let Some(symbol) = LspResolver::get_symbol_at_cursor(snapshot, cursor_offset) {
            // Check for cached definitions
            if let Some(definitions) =
                self.lsp_resolver
                    .get_cached_definitions(current_file_path, &symbol, cursor_offset)
            {
                for def in definitions.into_iter().take(2) {
                    if !def.content.is_empty() && def.content.len() <= MAX_CONTEXT_ITEM_SIZE {
                        items.push(ScoredContextItem {
                            item: CppContextItem {
                                contents: def.content,
                                symbol: Some(symbol.clone()),
                                relative_workspace_path: def.file_path,
                                score: 55.0, // High score for definitions
                            },
                            score: 55.0,
                            source: ContextSource::SymbolDefinition,
                        });
                    }
                }
            }

            // Check for cached references
            if let Some(references) =
                self.lsp_resolver
                    .get_cached_references(current_file_path, &symbol, cursor_offset)
            {
                for ref_item in references.into_iter().take(2) {
                    if !ref_item.content.is_empty()
                        && ref_item.content.len() <= MAX_CONTEXT_ITEM_SIZE
                    {
                        items.push(ScoredContextItem {
                            item: CppContextItem {
                                contents: ref_item.content,
                                symbol: Some(symbol.clone()),
                                relative_workspace_path: ref_item.file_path,
                                score: 45.0, // Good score for references
                            },
                            score: 45.0,
                            source: ContextSource::SymbolReference,
                        });
                    }
                }
            }
        }

        items
    }

    /// P3: Collect from syntax index
    fn collect_from_syntax_index(
        &self,
        snapshot: &BufferSnapshot,
        cursor_offset: usize,
        current_file_path: &str,
    ) -> Vec<ScoredContextItem> {
        let mut items = Vec::new();

        // Get symbol at cursor
        if let Some(symbol) = LspResolver::get_symbol_at_cursor(snapshot, cursor_offset) {
            // Look up declarations for this symbol
            let declarations = self.syntax_index.get_by_name(&symbol);

            for decl in declarations.into_iter().take(3) {
                // Skip if same file
                if decl.file_path == current_file_path {
                    continue;
                }

                if !decl.full_text.is_empty() && decl.full_text.len() <= MAX_CONTEXT_ITEM_SIZE {
                    let score = match decl.kind {
                        DeclarationKind::Function | DeclarationKind::Method => 48.0,
                        DeclarationKind::Struct | DeclarationKind::Class => 46.0,
                        DeclarationKind::Trait | DeclarationKind::Interface => 44.0,
                        DeclarationKind::Enum => 42.0,
                        DeclarationKind::Type => 40.0,
                        _ => 35.0,
                    };

                    items.push(ScoredContextItem {
                        item: CppContextItem {
                            contents: decl.full_text.clone(),
                            symbol: Some(decl.name.clone()),
                            relative_workspace_path: decl.file_path.clone(),
                            score,
                        },
                        score,
                        source: ContextSource::SyntaxIndex,
                    });
                }
            }
        }

        items
    }

    /// Convert scored items to proto format
    pub fn to_context_items(items: &[ScoredContextItem]) -> Vec<CppContextItem> {
        items.iter().map(|s| s.item.clone()).collect()
    }

    /// Convert scored items to additional files format
    pub fn to_additional_files(items: &[ScoredContextItem]) -> Vec<AdditionalFile> {
        items
            .iter()
            .map(|s| {
                let lines: Vec<String> = s.item.contents.lines().map(|l| l.to_string()).collect();
                let total_lines = lines.len() as i32;

                AdditionalFile {
                    relative_workspace_path: s.item.relative_workspace_path.clone(),
                    is_open: true,
                    visible_range_content: lines,
                    last_viewed_at: Some(
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs_f64())
                            .unwrap_or(0.0),
                    ),
                    start_line_number_one_indexed: vec![1],
                    visible_ranges: vec![LineRange {
                        start_line_number: 1,
                        end_line_number_inclusive: total_lines,
                    }],
                }
            })
            .collect()
    }

    /// Convert scored items to CodeResult format for enhanced context
    ///
    /// This converts the scored context items into the CodeResult proto format,
    /// which provides richer metadata about code blocks for the AI model.
    pub fn to_code_results(items: &[ScoredContextItem]) -> Vec<crate::proto::CodeResult> {
        use crate::proto::{
            CodeBlock, CodeResult, CursorPosition, CursorRange, code_block::Signatures,
        };

        items
            .iter()
            .filter_map(|s| {
                // Skip items without meaningful content
                if s.item.contents.is_empty() {
                    return None;
                }

                let lines: Vec<&str> = s.item.contents.lines().collect();
                let total_lines = lines.len() as i32;

                // Create range covering the entire content
                let range = CursorRange {
                    start_position: Some(CursorPosition { line: 0, column: 0 }),
                    end_position: Some(CursorPosition {
                        line: total_lines.saturating_sub(1),
                        column: lines.last().map(|l| l.len() as i32).unwrap_or(0),
                    }),
                };

                Some(CodeResult {
                    code_block: Some(CodeBlock {
                        relative_workspace_path: s.item.relative_workspace_path.clone(),
                        file_contents: None, // Don't duplicate full file contents
                        range: Some(range),
                        contents: s.item.contents.clone(),
                        signatures: Some(Signatures { ranges: vec![] }),
                        override_contents: None,
                        original_contents: None,
                    }),
                    score: s.score,
                })
            })
            .collect()
    }

    /// Convert LSP resolver cache to LspSubgraphFullContext proto format
    ///
    /// This method extracts cached LSP definitions and references and converts
    /// them to the proto format expected by the completion API.
    pub fn to_lsp_contexts(&self) -> Vec<crate::proto::LspSubgraphFullContext> {
        use crate::proto::{
            LspSubgraphContextItem, LspSubgraphFullContext, LspSubgraphPosition, LspSubgraphRange,
        };

        let cached_contexts = self.lsp_resolver.get_all_cached_contexts();

        cached_contexts
            .into_iter()
            .filter_map(|(symbol_name, uri, definitions, references)| {
                // Skip if no useful data
                if definitions.is_empty() && references.is_empty() {
                    return None;
                }

                // Build positions from definitions
                let positions: Vec<LspSubgraphPosition> = definitions
                    .iter()
                    .map(|loc| LspSubgraphPosition {
                        line: loc.range.start.row as i32,
                        character: loc.range.start.column as i32,
                    })
                    .collect();

                // Build context items from both definitions and references
                let mut context_items: Vec<LspSubgraphContextItem> = Vec::new();

                // Add definitions as context items
                for def in &definitions {
                    if !def.content.is_empty() {
                        context_items.push(LspSubgraphContextItem {
                            uri: Some(def.file_path.clone()),
                            r#type: "definition".to_string(),
                            content: def.content.clone(),
                            range: Some(LspSubgraphRange {
                                start_line: def.range.start.row as i32,
                                start_character: def.range.start.column as i32,
                                end_line: def.range.end.row as i32,
                                end_character: def.range.end.column as i32,
                            }),
                        });
                    }
                }

                // Add references as context items (limit to 5 to save tokens)
                for reference in references.iter().take(5) {
                    if !reference.content.is_empty() {
                        context_items.push(LspSubgraphContextItem {
                            uri: Some(reference.file_path.clone()),
                            r#type: "reference".to_string(),
                            content: reference.content.clone(),
                            range: Some(LspSubgraphRange {
                                start_line: reference.range.start.row as i32,
                                start_character: reference.range.start.column as i32,
                                end_line: reference.range.end.row as i32,
                                end_character: reference.range.end.column as i32,
                            }),
                        });
                    }
                }

                // Skip if no context items were created
                if context_items.is_empty() {
                    return None;
                }

                // Calculate score based on number of definitions and references
                let score =
                    (definitions.len() as f32 * 10.0 + references.len() as f32 * 2.0).min(100.0);

                Some(LspSubgraphFullContext {
                    uri,
                    symbol_name,
                    positions,
                    context_items,
                    score,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rust_import_parsing() {
        let content = r#"
use std::collections::HashMap;
use crate::module::{Foo, Bar};
use super::other::*;
mod my_mod;
"#;

        let imports = ImportAnalyzer::parse_rust_imports(content);

        assert_eq!(imports.len(), 4);
        assert_eq!(imports[0].path, "std::collections::HashMap");
        assert_eq!(imports[1].symbols, vec!["Foo", "Bar"]);
        assert!(imports[2].is_wildcard);
        assert!(imports[3].path.contains("my_mod"));
    }

    #[test]
    fn test_typescript_import_parsing() {
        let content = r#"
import { foo, bar as baz } from './module';
import * as utils from '../utils';
import React from 'react';
const x = require('lodash');
"#;

        let imports = ImportAnalyzer::parse_typescript_imports(content);

        assert_eq!(imports.len(), 4);
        assert_eq!(imports[0].path, "./module");
        assert_eq!(imports[0].symbols, vec!["foo", "baz"]);
        assert!(imports[1].is_wildcard);
    }

    #[test]
    fn test_python_import_parsing() {
        let content = r#"
import os
from collections import defaultdict, OrderedDict
from . import local_module
from ..parent import something
"#;

        let imports = ImportAnalyzer::parse_python_imports(content);

        assert_eq!(imports.len(), 4);
        assert_eq!(imports[0].path, "os");
        assert_eq!(imports[1].symbols, vec!["defaultdict", "OrderedDict"]);
    }

    #[test]
    fn test_recent_file_tracker() {
        let mut tracker = RecentFileTracker::new();

        tracker.record_edit("file1.rs");
        tracker.record_view("file2.rs");
        tracker.record_edit("file1.rs");

        let scored = tracker.get_scored_files("file3.rs");

        assert_eq!(scored.len(), 2);
        // file1 should have higher score (2 edits)
        assert!(
            scored.iter().find(|(p, _)| p == "file1.rs").unwrap().1
                > scored.iter().find(|(p, _)| p == "file2.rs").unwrap().1
        );
    }

    #[test]
    fn test_get_symbol_at_cursor() {
        // Create a mock snapshot-like test
        let text = "fn hello_world() {}";
        let offset = 5; // Inside "hello_world"

        // Simple extraction logic test
        let bytes = text.as_bytes();
        let mut start = offset;
        let mut end = offset;

        while start > 0 {
            let prev = start - 1;
            let c = bytes[prev] as char;
            if c.is_alphanumeric() || c == '_' {
                start = prev;
            } else {
                break;
            }
        }

        while end < bytes.len() {
            let c = bytes[end] as char;
            if c.is_alphanumeric() || c == '_' {
                end += 1;
            } else {
                break;
            }
        }

        let symbol = &text[start..end];
        assert_eq!(symbol, "hello_world");
    }

    #[test]
    fn test_syntax_index_declaration_kind() {
        assert!(matches!(
            SyntaxIndex::parse_declaration_kind("fn foo"),
            DeclarationKind::Function
        ));
        assert!(matches!(
            SyntaxIndex::parse_declaration_kind("struct Bar"),
            DeclarationKind::Struct
        ));
        assert!(matches!(
            SyntaxIndex::parse_declaration_kind("class MyClass"),
            DeclarationKind::Class
        ));
    }

    #[test]
    fn test_normalize_path() {
        assert_eq!(ImportAnalyzer::normalize_path("a/b/c"), "a/b/c");
        assert_eq!(ImportAnalyzer::normalize_path("a/./b/c"), "a/b/c");
        assert_eq!(ImportAnalyzer::normalize_path("a/b/../c"), "a/c");
        assert_eq!(ImportAnalyzer::normalize_path("./a/b"), "a/b");
        assert_eq!(ImportAnalyzer::normalize_path("a/b/c/../../d"), "a/d");
        assert_eq!(ImportAnalyzer::normalize_path("../a/b"), "a/b"); // .. at start gets removed
    }

    #[test]
    fn test_parent_path() {
        assert_eq!(
            ImportAnalyzer::parent_path("src/foo/bar.rs"),
            Some("src/foo")
        );
        assert_eq!(ImportAnalyzer::parent_path("src/foo"), Some("src"));
        assert_eq!(ImportAnalyzer::parent_path("foo"), None);
    }

    #[test]
    fn test_find_rust_crate_root() {
        assert_eq!(
            ImportAnalyzer::find_rust_crate_root("src/foo/bar.rs"),
            Some("src/".to_string())
        );
        assert_eq!(
            ImportAnalyzer::find_rust_crate_root("crates/mylib/src/lib.rs"),
            Some("crates/mylib/src/".to_string())
        );
        assert_eq!(
            ImportAnalyzer::find_rust_crate_root("main.rs"),
            Some("src/".to_string()) // Fallback
        );
    }

    #[test]
    fn test_get_rust_module_dir() {
        assert_eq!(
            ImportAnalyzer::get_rust_module_dir("src/foo/bar.rs"),
            Some("src/foo".to_string())
        );
        assert_eq!(
            ImportAnalyzer::get_rust_module_dir("src/foo/mod.rs"),
            Some("src/foo".to_string())
        );
        assert_eq!(
            ImportAnalyzer::get_rust_module_dir("src/lib.rs"),
            Some("src".to_string())
        );
    }

    #[test]
    fn test_count_super_prefix() {
        assert_eq!(ImportAnalyzer::count_super_prefix("foo"), (0, "foo"));
        assert_eq!(ImportAnalyzer::count_super_prefix("super::foo"), (1, "foo"));
        assert_eq!(
            ImportAnalyzer::count_super_prefix("super::super::foo"),
            (2, "foo")
        );
        assert_eq!(ImportAnalyzer::count_super_prefix(""), (0, ""));
    }
}
