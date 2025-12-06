//! Request management for Ctab
//!
//! This module provides:
//! - DebounceManager: Concurrent request control and deduplication
//! - SuggestionCache: Cache superseded request results
//! - NextActionManager: Auto-trigger next edit after acceptance
//! - TriggerManager: Smart trigger mechanism with rejection cooldown

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use futures::channel::oneshot;
use parking_lot::Mutex;
use uuid::Uuid;

// ============================================================================
// Constants
// ============================================================================

/// Maximum number of concurrent streams (matches Cursor's jc = 6)
const MAX_CONCURRENT_STREAMS: usize = 6;

/// Default debounce delay in milliseconds
const DEFAULT_DEBOUNCE_MS: u64 = 75;

/// Maximum cached suggestions
const MAX_CACHED_SUGGESTIONS: usize = 5;

/// Maximum version difference for cache hit
const MAX_VERSION_DIFF_FOR_CACHE: i32 = 3;

/// Rejection cooldown in milliseconds
const REJECTION_COOLDOWN_MS: u64 = 1500;

/// Next action expiry in milliseconds
const NEXT_ACTION_EXPIRY_MS: u64 = 30000;

// ============================================================================
// DebounceManager
// ============================================================================

/// Tracks an active request
#[derive(Debug)]
struct ActiveRequest {
    /// Unique request ID
    #[allow(dead_code)]
    id: String,
    /// Timestamp when request was created
    #[allow(dead_code)]
    timestamp: Instant,
    /// Channel to signal cancellation
    cancel_tx: Option<oneshot::Sender<()>>,
    /// Document path this request is for
    document_path: String,
}

/// Manages request debouncing and concurrency control
///
/// Implements the same logic as Cursor's DebounceManager:
/// - Limits concurrent streams to MAX_CONCURRENT_STREAMS
/// - Cancels oldest requests when limit is exceeded
/// - Provides debounce delay before executing requests
pub struct DebounceManager {
    /// Active requests by ID
    requests: Mutex<HashMap<String, ActiveRequest>>,
    /// Request order for FIFO cancellation
    request_order: Mutex<VecDeque<String>>,
    /// Debounce delay in milliseconds
    debounce_ms: AtomicU64,
    /// Counter for generating unique IDs
    request_counter: AtomicU64,
}

impl DebounceManager {
    /// Create a new DebounceManager with default settings
    pub fn new() -> Self {
        Self {
            requests: Mutex::new(HashMap::new()),
            request_order: Mutex::new(VecDeque::new()),
            debounce_ms: AtomicU64::new(DEFAULT_DEBOUNCE_MS),
            request_counter: AtomicU64::new(0),
        }
    }

    /// Create a new DebounceManager with custom debounce delay
    pub fn with_debounce_ms(debounce_ms: u64) -> Self {
        Self {
            requests: Mutex::new(HashMap::new()),
            request_order: Mutex::new(VecDeque::new()),
            debounce_ms: AtomicU64::new(debounce_ms),
            request_counter: AtomicU64::new(0),
        }
    }

    /// Update the debounce delay (can be called when server config changes)
    pub fn set_debounce_ms(&self, ms: u64) {
        self.debounce_ms.store(ms, Ordering::SeqCst);
    }

    /// Get the current debounce delay
    pub fn debounce_ms(&self) -> u64 {
        self.debounce_ms.load(Ordering::SeqCst)
    }

    /// Register a new request and get IDs to cancel
    ///
    /// Returns:
    /// - `request_id`: Unique ID for this request
    /// - `cancel_rx`: Receiver that signals if this request is cancelled
    /// - `requests_to_cancel`: IDs of requests that should be cancelled
    pub fn run_request(&self, document_path: String) -> RunRequestResult {
        let request_id = format!(
            "req-{}-{}",
            Uuid::new_v4().as_simple(),
            self.request_counter.fetch_add(1, Ordering::SeqCst)
        );

        let (cancel_tx, cancel_rx) = oneshot::channel();
        let mut requests_to_cancel = Vec::new();

        {
            let mut requests = self.requests.lock();
            let mut order = self.request_order.lock();

            // Cancel oldest requests if we're at the limit
            while requests.len() >= MAX_CONCURRENT_STREAMS && !order.is_empty() {
                if let Some(oldest_id) = order.pop_front() {
                    if let Some(mut oldest) = requests.remove(&oldest_id) {
                        // Signal cancellation
                        if let Some(tx) = oldest.cancel_tx.take() {
                            let _ = tx.send(());
                        }
                        requests_to_cancel.push(oldest_id);
                    }
                }
            }

            // Also cancel any existing request for the same document
            // This ensures we don't have multiple requests for the same file
            let existing_for_doc: Vec<String> = requests
                .iter()
                .filter(|(_, r)| r.document_path == document_path)
                .map(|(id, _)| id.clone())
                .collect();

            for id in existing_for_doc {
                if let Some(mut req) = requests.remove(&id) {
                    if let Some(tx) = req.cancel_tx.take() {
                        let _ = tx.send(());
                    }
                    requests_to_cancel.push(id.clone());
                    order.retain(|x| x != &id);
                }
            }

            // Add new request
            requests.insert(
                request_id.clone(),
                ActiveRequest {
                    id: request_id.clone(),
                    timestamp: Instant::now(),
                    cancel_tx: Some(cancel_tx),
                    document_path,
                },
            );
            order.push_back(request_id.clone());
        }

        log::debug!(
            "Ctab DebounceManager: Created request {}, cancelled {} old requests",
            &request_id[..16.min(request_id.len())],
            requests_to_cancel.len()
        );

        RunRequestResult {
            request_id,
            cancel_rx,
            requests_to_cancel,
        }
    }

    /// Check if a request should be debounced (i.e., was it cancelled during debounce wait?)
    ///
    /// This should be called after waiting for the debounce delay.
    /// Returns true if the request was cancelled and should not proceed.
    pub fn is_cancelled(&self, request_id: &str) -> bool {
        let requests = self.requests.lock();
        !requests.contains_key(request_id)
    }

    /// Remove a request (call when request completes or is abandoned)
    pub fn remove_request(&self, request_id: &str) {
        let mut requests = self.requests.lock();
        let mut order = self.request_order.lock();

        if requests.remove(request_id).is_some() {
            order.retain(|id| id != request_id);
            log::debug!(
                "Ctab DebounceManager: Removed request {}, {} active",
                &request_id[..16.min(request_id.len())],
                requests.len()
            );
        }
    }

    /// Get the number of active requests
    pub fn active_count(&self) -> usize {
        self.requests.lock().len()
    }

    /// Cancel all requests for a specific document
    pub fn cancel_for_document(&self, document_path: &str) -> Vec<String> {
        let mut requests = self.requests.lock();
        let mut order = self.request_order.lock();
        let mut cancelled = Vec::new();

        let to_cancel: Vec<String> = requests
            .iter()
            .filter(|(_, r)| r.document_path == document_path)
            .map(|(id, _)| id.clone())
            .collect();

        for id in to_cancel {
            if let Some(mut req) = requests.remove(&id) {
                if let Some(tx) = req.cancel_tx.take() {
                    let _ = tx.send(());
                }
                order.retain(|x| x != &id);
                cancelled.push(id);
            }
        }

        cancelled
    }
}

impl Default for DebounceManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Result of `run_request`
pub struct RunRequestResult {
    /// Unique request ID
    pub request_id: String,
    /// Receiver that signals if this request is cancelled
    pub cancel_rx: oneshot::Receiver<()>,
    /// IDs of requests that were cancelled to make room
    pub requests_to_cancel: Vec<String>,
}

// ============================================================================
// SuggestionCache
// ============================================================================

/// A cached suggestion from a superseded request
#[derive(Clone, Debug)]
pub struct CachedSuggestion {
    /// The completion text
    pub text: String,
    /// Binding ID from server
    pub binding_id: Option<String>,
    /// Range to replace (1-indexed line numbers from server)
    pub api_range: Option<(i32, i32)>,
    /// Document path
    pub document_path: String,
    /// Buffer version when cached
    pub buffer_version: usize,
    /// Timestamp when cached
    pub timestamp: Instant,
    /// Whether to retrigger after accept
    pub should_retrigger: bool,
    /// Request ID that produced this suggestion
    pub request_id: String,
}

/// Caches suggestions from superseded requests
///
/// When a newer request supersedes an older one, we cache the result
/// instead of discarding it. The next request can check the cache first.
pub struct SuggestionCache {
    /// Cached suggestions (most recent last)
    cache: Mutex<VecDeque<CachedSuggestion>>,
    /// Maximum cache size
    max_size: usize,
}

impl SuggestionCache {
    /// Create a new SuggestionCache
    pub fn new() -> Self {
        Self {
            cache: Mutex::new(VecDeque::new()),
            max_size: MAX_CACHED_SUGGESTIONS,
        }
    }

    /// Add a suggestion to the cache
    pub fn add(&self, suggestion: CachedSuggestion) {
        let mut cache = self.cache.lock();

        // Remove old entries for the same document to avoid stale cache
        cache.retain(|s| s.document_path != suggestion.document_path);

        cache.push_back(suggestion);

        // Trim to max size
        while cache.len() > self.max_size {
            cache.pop_front();
        }

        log::debug!(
            "Ctab SuggestionCache: Added suggestion, cache size: {}",
            cache.len()
        );
    }

    /// Try to get a cached suggestion for the given document
    ///
    /// Returns and removes the most recent matching suggestion.
    pub fn pop(&self, document_path: &str, buffer_version: usize) -> Option<CachedSuggestion> {
        let mut cache = self.cache.lock();

        // Find most recent matching suggestion (search from end)
        let mut found_idx = None;
        for (idx, cached) in cache.iter().enumerate().rev() {
            if cached.document_path == document_path {
                let version_diff = buffer_version as i32 - cached.buffer_version as i32;
                if version_diff >= 0 && version_diff <= MAX_VERSION_DIFF_FOR_CACHE {
                    found_idx = Some(idx);
                    break;
                }
            }
        }

        if let Some(idx) = found_idx {
            let suggestion = cache.remove(idx);
            log::debug!(
                "Ctab SuggestionCache: Cache hit for {}, {} remaining",
                document_path,
                cache.len()
            );
            suggestion
        } else {
            None
        }
    }

    /// Clear all cached suggestions for a document
    pub fn clear_for_document(&self, document_path: &str) {
        let mut cache = self.cache.lock();
        let before = cache.len();
        cache.retain(|s| s.document_path != document_path);
        let removed = before - cache.len();
        if removed > 0 {
            log::debug!(
                "Ctab SuggestionCache: Cleared {} suggestions for {}",
                removed,
                document_path
            );
        }
    }

    /// Clear all cached suggestions
    pub fn clear_all(&self) {
        let mut cache = self.cache.lock();
        cache.clear();
    }

    /// Get current cache size
    pub fn len(&self) -> usize {
        self.cache.lock().len()
    }

    /// Check if cache is empty
    pub fn is_empty(&self) -> bool {
        self.cache.lock().is_empty()
    }
}

impl Default for SuggestionCache {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// NextActionManager
// ============================================================================

/// Type of next action to perform after acceptance
#[derive(Clone, Debug)]
pub enum NextActionType {
    /// Trigger the next edit in a multidiff sequence
    NextEdit,
    /// Navigate to a cursor prediction target
    CursorPrediction {
        relative_path: String,
        line_number_one_indexed: i32,
        should_retrigger: bool,
    },
}

/// Entry in the next action cache
#[derive(Clone, Debug)]
struct NextActionEntry {
    /// Type of action
    action_type: NextActionType,
    /// Original request ID
    request_id: String,
    /// When this action was registered
    timestamp: Instant,
}

/// Manages next actions after suggestion acceptance
///
/// When a multidiff response has multiple edits, we register a "next action"
/// so that after the user accepts the first edit, we automatically trigger
/// the next one.
pub struct NextActionManager {
    /// Cached next actions by action ID (usually binding_id or request_id)
    actions: Mutex<HashMap<String, NextActionEntry>>,
}

impl NextActionManager {
    /// Create a new NextActionManager
    pub fn new() -> Self {
        Self {
            actions: Mutex::new(HashMap::new()),
        }
    }

    /// Register a next action
    pub fn register(&self, action_id: String, action_type: NextActionType, request_id: String) {
        let mut actions = self.actions.lock();

        // Clean up expired entries
        let now = Instant::now();
        let expiry = Duration::from_millis(NEXT_ACTION_EXPIRY_MS);
        actions.retain(|_, entry| now.duration_since(entry.timestamp) < expiry);

        actions.insert(
            action_id.clone(),
            NextActionEntry {
                action_type: action_type.clone(),
                request_id,
                timestamp: now,
            },
        );

        log::debug!(
            "Ctab NextActionManager: Registered {:?} for {}",
            action_type,
            &action_id[..16.min(action_id.len())]
        );
    }

    /// Get and remove a next action
    pub fn take(&self, action_id: &str) -> Option<NextActionType> {
        let mut actions = self.actions.lock();
        actions.remove(action_id).map(|entry| {
            log::debug!(
                "Ctab NextActionManager: Taking action {:?} for {}",
                entry.action_type,
                &action_id[..16.min(action_id.len())]
            );
            entry.action_type
        })
    }

    /// Check if there's a next action without removing it
    pub fn peek(&self, action_id: &str) -> Option<NextActionType> {
        let actions = self.actions.lock();
        actions
            .get(action_id)
            .map(|entry| entry.action_type.clone())
    }

    /// Remove a next action without returning it
    pub fn remove(&self, action_id: &str) {
        let mut actions = self.actions.lock();
        actions.remove(action_id);
    }

    /// Clear all actions for a request
    pub fn clear_for_request(&self, request_id: &str) {
        let mut actions = self.actions.lock();
        actions.retain(|_, entry| entry.request_id != request_id);
    }

    /// Clear all actions
    pub fn clear_all(&self) {
        let mut actions = self.actions.lock();
        actions.clear();
    }
}

impl Default for NextActionManager {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// TriggerManager (InlineEditTriggerer equivalent)
// ============================================================================

/// Source of a completion trigger
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TriggerSource {
    /// User is typing
    Typing,
    /// Manual trigger (e.g., keyboard shortcut)
    Manual,
    /// Triggered after accepting a previous completion
    PostAccept,
    /// Triggered by cursor prediction
    CursorPrediction,
    /// Triggered by document change event
    DocumentChange,
}

/// Manages smart triggering of completions
///
/// Implements rejection cooldown to avoid triggering completions
/// immediately after the user rejects one.
pub struct TriggerManager {
    /// Last rejection timestamp
    last_rejection: Mutex<Option<Instant>>,
    /// Cooldown duration after rejection
    cooldown_ms: AtomicU64,
    /// Last trigger timestamp per document
    last_trigger: Mutex<HashMap<String, Instant>>,
    /// Minimum interval between triggers for same document
    min_trigger_interval_ms: AtomicU64,
}

impl TriggerManager {
    /// Create a new TriggerManager
    pub fn new() -> Self {
        Self {
            last_rejection: Mutex::new(None),
            cooldown_ms: AtomicU64::new(REJECTION_COOLDOWN_MS),
            last_trigger: Mutex::new(HashMap::new()),
            min_trigger_interval_ms: AtomicU64::new(50),
        }
    }

    /// Record a rejection event
    pub fn record_rejection(&self) {
        let mut last = self.last_rejection.lock();
        *last = Some(Instant::now());
        log::debug!("Ctab TriggerManager: Recorded rejection, cooldown active");
    }

    /// Check if we're in cooldown period
    pub fn is_in_cooldown(&self) -> bool {
        let last = self.last_rejection.lock();
        if let Some(rejection_time) = *last {
            let elapsed = rejection_time.elapsed().as_millis() as u64;
            let cooldown = self.cooldown_ms.load(Ordering::SeqCst);
            elapsed < cooldown
        } else {
            false
        }
    }

    /// Check if a trigger should proceed
    ///
    /// Returns false if:
    /// - We're in rejection cooldown
    /// - We triggered too recently for this document
    pub fn should_trigger(&self, document_path: &str, source: TriggerSource) -> bool {
        // Manual triggers always proceed
        if source == TriggerSource::Manual {
            return true;
        }

        // Check rejection cooldown
        if self.is_in_cooldown() {
            log::debug!("Ctab TriggerManager: In cooldown, skipping trigger");
            return false;
        }

        // Check minimum interval
        let mut last_trigger = self.last_trigger.lock();
        let min_interval =
            Duration::from_millis(self.min_trigger_interval_ms.load(Ordering::SeqCst));

        if let Some(last) = last_trigger.get(document_path) {
            if last.elapsed() < min_interval {
                return false;
            }
        }

        // Update last trigger time
        last_trigger.insert(document_path.to_string(), Instant::now());
        true
    }

    /// Clear cooldown (e.g., when user manually triggers)
    pub fn clear_cooldown(&self) {
        let mut last = self.last_rejection.lock();
        *last = None;
    }

    /// Set cooldown duration
    pub fn set_cooldown_ms(&self, ms: u64) {
        self.cooldown_ms.store(ms, Ordering::SeqCst);
    }

    /// Set minimum trigger interval
    pub fn set_min_trigger_interval_ms(&self, ms: u64) {
        self.min_trigger_interval_ms.store(ms, Ordering::SeqCst);
    }
}

impl Default for TriggerManager {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// RequestStateManager - Combines all managers
// ============================================================================

/// Combines all request management functionality
///
/// This is the main entry point for request management in CtabCompletionProvider.
pub struct RequestStateManager {
    /// Debounce and concurrency control
    pub debounce: DebounceManager,
    /// Suggestion cache for superseded requests
    pub cache: SuggestionCache,
    /// Next action management
    pub next_action: NextActionManager,
    /// Trigger management
    pub trigger: TriggerManager,
    /// Current request ID per document
    current_requests: Mutex<HashMap<String, String>>,
}

impl RequestStateManager {
    /// Create a new RequestStateManager
    pub fn new() -> Self {
        Self {
            debounce: DebounceManager::new(),
            cache: SuggestionCache::new(),
            next_action: NextActionManager::new(),
            trigger: TriggerManager::new(),
            current_requests: Mutex::new(HashMap::new()),
        }
    }

    /// Create with custom debounce delay
    pub fn with_debounce_ms(debounce_ms: u64) -> Self {
        Self {
            debounce: DebounceManager::with_debounce_ms(debounce_ms),
            cache: SuggestionCache::new(),
            next_action: NextActionManager::new(),
            trigger: TriggerManager::new(),
            current_requests: Mutex::new(HashMap::new()),
        }
    }

    /// Set the current request for a document
    pub fn set_current_request(&self, document_path: &str, request_id: &str) {
        let mut current = self.current_requests.lock();
        current.insert(document_path.to_string(), request_id.to_string());
    }

    /// Get the current request for a document
    pub fn get_current_request(&self, document_path: &str) -> Option<String> {
        let current = self.current_requests.lock();
        current.get(document_path).cloned()
    }

    /// Check if a request is still current (not superseded)
    pub fn is_current_request(&self, document_path: &str, request_id: &str) -> bool {
        let current = self.current_requests.lock();
        current
            .get(document_path)
            .map(|id| id == request_id)
            .unwrap_or(false)
    }

    /// Clear current request for a document
    pub fn clear_current_request(&self, document_path: &str) {
        let mut current = self.current_requests.lock();
        current.remove(document_path);
    }

    /// Handle request completion
    ///
    /// If the request was superseded, caches the suggestion.
    /// Returns true if the suggestion should be used, false if it was cached.
    pub fn handle_completion(
        &self,
        document_path: &str,
        request_id: &str,
        suggestion: CachedSuggestion,
    ) -> bool {
        if self.is_current_request(document_path, request_id) {
            // This is the current request, use the suggestion
            true
        } else {
            // This request was superseded, cache the result
            log::debug!(
                "Ctab: Request {} superseded, caching result",
                &request_id[..16.min(request_id.len())]
            );
            self.cache.add(suggestion);
            false
        }
    }

    /// Clean up after a document is closed
    pub fn cleanup_document(&self, document_path: &str) {
        self.debounce.cancel_for_document(document_path);
        self.cache.clear_for_document(document_path);
        self.clear_current_request(document_path);
    }
}

impl Default for RequestStateManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_debounce_manager_basic() {
        let dm = DebounceManager::new();

        let result1 = dm.run_request("file1.rs".to_string());
        assert_eq!(dm.active_count(), 1);
        assert!(result1.requests_to_cancel.is_empty());

        let result2 = dm.run_request("file2.rs".to_string());
        assert_eq!(dm.active_count(), 2);
        assert!(result2.requests_to_cancel.is_empty());

        dm.remove_request(&result1.request_id);
        assert_eq!(dm.active_count(), 1);
    }

    #[test]
    fn test_debounce_manager_cancels_same_document() {
        let dm = DebounceManager::new();

        let result1 = dm.run_request("file.rs".to_string());
        let result2 = dm.run_request("file.rs".to_string());

        // Second request should cancel the first
        assert_eq!(result2.requests_to_cancel.len(), 1);
        assert_eq!(result2.requests_to_cancel[0], result1.request_id);
        assert_eq!(dm.active_count(), 1);
    }

    #[test]
    fn test_debounce_manager_max_concurrent() {
        let dm = DebounceManager::new();

        // Fill up to max
        let mut ids = Vec::new();
        for i in 0..MAX_CONCURRENT_STREAMS {
            let result = dm.run_request(format!("file{}.rs", i));
            ids.push(result.request_id);
        }
        assert_eq!(dm.active_count(), MAX_CONCURRENT_STREAMS);

        // One more should cancel the oldest
        let result = dm.run_request("overflow.rs".to_string());
        assert_eq!(result.requests_to_cancel.len(), 1);
        assert_eq!(result.requests_to_cancel[0], ids[0]);
        assert_eq!(dm.active_count(), MAX_CONCURRENT_STREAMS);
    }

    #[test]
    fn test_suggestion_cache() {
        let cache = SuggestionCache::new();

        cache.add(CachedSuggestion {
            text: "suggestion1".to_string(),
            binding_id: None,
            api_range: None,
            document_path: "file.rs".to_string(),
            buffer_version: 1,
            timestamp: Instant::now(),
            should_retrigger: false,
            request_id: "req1".to_string(),
        });

        // Cache hit with same version
        let result = cache.pop("file.rs", 1);
        assert!(result.is_some());
        assert_eq!(result.unwrap().text, "suggestion1");

        // Cache should be empty now
        assert!(cache.is_empty());
    }

    #[test]
    fn test_suggestion_cache_version_diff() {
        let cache = SuggestionCache::new();

        cache.add(CachedSuggestion {
            text: "suggestion1".to_string(),
            binding_id: None,
            api_range: None,
            document_path: "file.rs".to_string(),
            buffer_version: 1,
            timestamp: Instant::now(),
            should_retrigger: false,
            request_id: "req1".to_string(),
        });

        // Should hit with small version diff
        let result = cache.pop("file.rs", 3);
        assert!(result.is_some());

        // Re-add
        cache.add(CachedSuggestion {
            text: "suggestion2".to_string(),
            binding_id: None,
            api_range: None,
            document_path: "file.rs".to_string(),
            buffer_version: 1,
            timestamp: Instant::now(),
            should_retrigger: false,
            request_id: "req2".to_string(),
        });

        // Should miss with large version diff
        let result = cache.pop("file.rs", 10);
        assert!(result.is_none());
    }

    #[test]
    fn test_next_action_manager() {
        let manager = NextActionManager::new();

        manager.register(
            "action1".to_string(),
            NextActionType::NextEdit,
            "req1".to_string(),
        );

        // Should find the action
        let action = manager.take("action1");
        assert!(matches!(action, Some(NextActionType::NextEdit)));

        // Should be gone now
        let action = manager.take("action1");
        assert!(action.is_none());
    }

    #[test]
    fn test_trigger_manager_cooldown() {
        let tm = TriggerManager::new();
        tm.set_cooldown_ms(100); // Short cooldown for test

        // Should trigger initially
        assert!(tm.should_trigger("file.rs", TriggerSource::Typing));

        // Record rejection
        tm.record_rejection();

        // Should not trigger during cooldown
        assert!(!tm.should_trigger("file.rs", TriggerSource::Typing));

        // Manual trigger should always work
        assert!(tm.should_trigger("file.rs", TriggerSource::Manual));

        // Wait for cooldown
        std::thread::sleep(Duration::from_millis(150));

        // Should trigger again
        assert!(tm.should_trigger("file.rs", TriggerSource::Typing));
    }
}
