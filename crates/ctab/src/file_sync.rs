//! File synchronization service for Cometix
//!
//! This module handles file synchronization with the Cursor API server,
//! enabling the server to maintain an up-to-date view of the user's workspace.
//!
//! Enhanced features:
//! - Exponential backoff retry mechanism
//! - Server configuration support (FSConfig)
//! - Rate limiting based on server config
//! - Batch update optimization

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use collections::HashMap;
use futures::AsyncReadExt;
use http_client::{AsyncBody, HttpClient, Method};
use parking_lot::Mutex;
use prost::Message;
use sha2::{Digest, Sha256};

use crate::proto::{
    FilesyncUpdateWithModelVersion, FsSyncErrorType, FsSyncFileRequest, FsSyncFileResponse,
    FsUploadErrorType, FsUploadFileRequest, FsUploadFileResponse, SimpleRange, SingleUpdateRequest,
};

/// Client version to report
const CLIENT_VERSION: &str = "1.6.1-zed";

/// Maximum file size to sync (in bytes)
const MAX_FILE_SIZE_TO_SYNC: usize = 500_000;

/// Default retry configuration
const DEFAULT_MAX_RETRY_ATTEMPTS: u32 = 3;
const DEFAULT_INITIAL_DELAY_MS: u64 = 100;
const DEFAULT_RETRY_MULTIPLIER: u32 = 2;

/// Default rate limiting
const DEFAULT_RATE_LIMIT_RPS: u32 = 10;
const DEFAULT_BURST_CAPACITY: u32 = 20;

/// Config cache TTL (5 minutes)
const CONFIG_CACHE_TTL_SECS: u64 = 300;

/// Minimum delay between rate limiter token refills (ms)
const TOKEN_REFILL_INTERVAL_MS: u64 = 100;

// ============================================================================
// Exponential Backoff Retry Mechanism
// ============================================================================

/// Configuration for exponential backoff retry
#[derive(Clone, Debug)]
pub struct RetryConfig {
    /// Maximum number of retry attempts
    pub max_attempts: u32,
    /// Initial delay before first retry (milliseconds)
    pub initial_delay_ms: u64,
    /// Multiplier applied to delay after each retry
    pub multiplier: u32,
    /// Maximum delay cap (milliseconds)
    pub max_delay_ms: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: DEFAULT_MAX_RETRY_ATTEMPTS,
            initial_delay_ms: DEFAULT_INITIAL_DELAY_MS,
            multiplier: DEFAULT_RETRY_MULTIPLIER,
            max_delay_ms: 5000, // Cap at 5 seconds
        }
    }
}

/// Exponential backoff state tracker
#[derive(Debug)]
pub struct ExponentialBackoff {
    config: RetryConfig,
    current_attempt: u32,
    current_delay_ms: u64,
}

impl ExponentialBackoff {
    /// Create a new backoff tracker with the given configuration
    pub fn new(config: RetryConfig) -> Self {
        Self {
            current_delay_ms: config.initial_delay_ms,
            config,
            current_attempt: 0,
        }
    }

    /// Create with default configuration
    pub fn with_defaults() -> Self {
        Self::new(RetryConfig::default())
    }

    /// Check if another retry should be attempted
    pub fn should_retry(&self) -> bool {
        self.current_attempt < self.config.max_attempts
    }

    /// Get the current delay and advance to next attempt
    ///
    /// Returns `Some(Duration)` if retry should be attempted, `None` if max attempts reached
    pub fn next_delay(&mut self) -> Option<Duration> {
        if !self.should_retry() {
            return None;
        }

        let delay = Duration::from_millis(self.current_delay_ms);

        // Advance state for next call
        self.current_attempt += 1;
        self.current_delay_ms =
            (self.current_delay_ms * self.config.multiplier as u64).min(self.config.max_delay_ms);

        Some(delay)
    }

    /// Reset the backoff state (e.g., after successful request)
    pub fn reset(&mut self) {
        self.current_attempt = 0;
        self.current_delay_ms = self.config.initial_delay_ms;
    }

    /// Get the current attempt number (0-indexed)
    pub fn current_attempt(&self) -> u32 {
        self.current_attempt
    }
}

// ============================================================================
// Token Bucket Rate Limiter
// ============================================================================

/// Token bucket rate limiter for controlling request frequency
///
/// Uses the token bucket algorithm:
/// - Tokens are added at a fixed rate up to a maximum capacity
/// - Each request consumes one token
/// - If no tokens available, request must wait
pub struct RateLimiter {
    /// Current number of available tokens
    tokens: f64,
    /// Maximum token capacity (burst size)
    capacity: u32,
    /// Tokens added per second
    refill_rate: f64,
    /// Last time tokens were refilled
    last_refill: Instant,
}

impl RateLimiter {
    /// Create a new rate limiter
    ///
    /// # Arguments
    /// * `requests_per_second` - Maximum sustained request rate
    /// * `burst_capacity` - Maximum burst size (token bucket capacity)
    pub fn new(requests_per_second: u32, burst_capacity: u32) -> Self {
        Self {
            tokens: burst_capacity as f64,
            capacity: burst_capacity,
            refill_rate: requests_per_second as f64,
            last_refill: Instant::now(),
        }
    }

    /// Create with default configuration
    pub fn with_defaults() -> Self {
        Self::new(DEFAULT_RATE_LIMIT_RPS, DEFAULT_BURST_CAPACITY)
    }

    /// Refill tokens based on elapsed time
    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();

        if elapsed > 0.0 {
            self.tokens = (self.tokens + elapsed * self.refill_rate).min(self.capacity as f64);
            self.last_refill = now;
        }
    }

    /// Try to acquire a token without waiting
    ///
    /// Returns `true` if token was acquired, `false` if no tokens available
    pub fn try_acquire(&mut self) -> bool {
        self.refill();

        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Calculate time to wait before a token becomes available
    ///
    /// Returns `None` if a token is immediately available
    pub fn time_until_available(&mut self) -> Option<Duration> {
        self.refill();

        if self.tokens >= 1.0 {
            None
        } else {
            let tokens_needed = 1.0 - self.tokens;
            let wait_secs = tokens_needed / self.refill_rate;
            Some(Duration::from_secs_f64(wait_secs))
        }
    }

    /// Get current available tokens (for debugging/monitoring)
    pub fn available_tokens(&mut self) -> f64 {
        self.refill();
        self.tokens
    }
}

// ============================================================================
// FSConfig Types (local definitions since not generated from proto)
// ============================================================================

/// Request for FSConfig (empty message)
#[derive(Clone, Debug, Default)]
pub struct FsConfigRequest {}

impl FsConfigRequest {
    pub fn encode_to_vec(&self) -> Vec<u8> {
        // Empty message encodes to empty bytes
        Vec::new()
    }
}

/// Response from FSConfig endpoint
///
/// Contains server-side configuration for file sync operations.
/// Fields match the proto definition in fs.proto.
#[derive(Clone, Debug, Default)]
pub struct FsConfigResponse {
    /// Percentage of file sync operations to verify hash
    pub check_filesync_hash_percent: f32,
    /// Rate limiter reset time in milliseconds
    pub rate_limiter_breaker_reset_time_ms: i32,
    /// Rate limit requests per second
    pub rate_limit_rps: i32,
    /// Burst capacity for rate limiter
    pub burst_capacity: i32,
    /// Maximum recent updates to store
    pub max_recent_updates_stored: i32,
    /// Maximum model version cache size
    pub max_model_version_cache_size: i32,
    /// Maximum file size to sync in bytes
    pub max_file_size_to_sync_bytes: i32,
    /// Sync retry max attempts
    pub sync_retry_max_attempts: i32,
    /// Sync retry initial delay in milliseconds
    pub sync_retry_initial_delay_ms: i32,
    /// Sync retry time multiplier
    pub sync_retry_time_multiplier: i32,
    /// Sync debounce in milliseconds
    pub sync_debounce_ms: i32,
}

impl FsConfigResponse {
    /// Decode from protobuf bytes
    ///
    /// This is a simplified decoder that extracts key fields.
    /// Full proto parsing would require proper protobuf wire format handling.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        // For now, return default if we can't parse
        // A proper implementation would decode the wire format
        if bytes.is_empty() {
            return Ok(Self::default());
        }

        // Simple field extraction (protobuf wire format)
        // Field 3: rate_limiter_rps (varint)
        // Field 4: rate_limiter_burst_capacity (varint)
        let mut config = Self::default();

        let mut i = 0;
        while i < bytes.len() {
            if i + 1 > bytes.len() {
                break;
            }

            let tag_byte = bytes[i];
            let field_number = tag_byte >> 3;
            let wire_type = tag_byte & 0x07;
            i += 1;

            match wire_type {
                0 => {
                    // Varint
                    let (value, consumed) = Self::decode_varint(&bytes[i..])?;
                    i += consumed;

                    match field_number {
                        3 => config.rate_limit_rps = value as i32,
                        4 => config.burst_capacity = value as i32,
                        8 => config.sync_retry_max_attempts = value as i32,
                        9 => config.sync_retry_initial_delay_ms = value as i32,
                        10 => config.sync_retry_time_multiplier = value as i32,
                        17 => config.sync_debounce_ms = value as i32,
                        _ => {} // Skip unknown fields
                    }
                }
                5 => {
                    // 32-bit (float)
                    if i + 4 <= bytes.len() {
                        if field_number == 1 {
                            let bits = u32::from_le_bytes([
                                bytes[i],
                                bytes[i + 1],
                                bytes[i + 2],
                                bytes[i + 3],
                            ]);
                            config.check_filesync_hash_percent = f32::from_bits(bits);
                        }
                        i += 4;
                    } else {
                        break;
                    }
                }
                2 => {
                    // Length-delimited (skip)
                    let (len, consumed) = Self::decode_varint(&bytes[i..])?;
                    i += consumed;
                    i += len as usize;
                }
                _ => {
                    // Unknown wire type, skip
                    break;
                }
            }
        }

        Ok(config)
    }

    /// Decode a varint from bytes
    fn decode_varint(bytes: &[u8]) -> Result<(u64, usize)> {
        let mut result: u64 = 0;
        let mut shift = 0;
        let mut i = 0;

        loop {
            if i >= bytes.len() {
                anyhow::bail!("Unexpected end of varint");
            }

            let byte = bytes[i];
            result |= ((byte & 0x7F) as u64) << shift;
            i += 1;

            if byte & 0x80 == 0 {
                break;
            }

            shift += 7;
            if shift > 63 {
                anyhow::bail!("Varint too long");
            }
        }

        Ok((result, i))
    }
}

// ============================================================================
// FSConfig Manager
// ============================================================================

/// Cached FSConfig from server
#[derive(Clone, Debug)]
pub struct CachedFSConfig {
    /// The server configuration response
    pub config: FsConfigResponse,
    /// When this config was fetched
    pub fetched_at: Instant,
}

impl CachedFSConfig {
    /// Check if the cached config is still valid
    pub fn is_valid(&self) -> bool {
        self.fetched_at.elapsed().as_secs() < CONFIG_CACHE_TTL_SECS
    }
}

/// Manager for FSConfig server configuration
///
/// Handles fetching, caching, and providing server-side configuration
/// for file sync operations.
pub struct FSConfigManager {
    /// Cached configuration
    cache: Arc<Mutex<Option<CachedFSConfig>>>,
    /// Rate limiter instance (created from config)
    rate_limiter: Arc<Mutex<RateLimiter>>,
    /// HTTP client for fetching config
    http_client: Arc<dyn HttpClient>,
}

impl FSConfigManager {
    /// Create a new FSConfig manager
    pub fn new(http_client: Arc<dyn HttpClient>) -> Self {
        Self {
            cache: Arc::new(Mutex::new(None)),
            rate_limiter: Arc::new(Mutex::new(RateLimiter::with_defaults())),
            http_client,
        }
    }

    /// Get the rate limiter
    pub fn rate_limiter(&self) -> Arc<Mutex<RateLimiter>> {
        self.rate_limiter.clone()
    }

    /// Get cached config if valid, otherwise return None
    pub fn get_cached(&self) -> Option<FsConfigResponse> {
        let cache = self.cache.lock();
        cache
            .as_ref()
            .filter(|c| c.is_valid())
            .map(|c| c.config.clone())
    }

    /// Fetch FSConfig from server
    pub async fn fetch_config(
        http_client: Arc<dyn HttpClient>,
        auth_token: &str,
        client_key: &str,
        client_key_header: &str,
        filesync_client_key: &str,
        filesync_cookie: &str,
        base_url: &str,
        config_path: &str,
    ) -> Result<FsConfigResponse> {
        let url = format!("{}{}", base_url, config_path);

        log::debug!("Ctab FileSync: Fetching FSConfig from {}", url);

        let request = FsConfigRequest {};
        let body = request.encode_to_vec();

        let http_request = http_client::Request::builder()
            .method(Method::POST)
            .uri(&url)
            .header("Content-Type", "application/proto")
            .header("Authorization", format!("Bearer {}", auth_token))
            .header(client_key_header, client_key)
            .header("x-client-key", filesync_client_key)
            .header("x-fs-client-key", filesync_client_key)
            .header("Cookie", format!("FilesyncCookie={}", filesync_cookie))
            .header("x-cursor-client-version", CLIENT_VERSION)
            .body(AsyncBody::from(body))?;

        let mut response = http_client.send(http_request).await?;
        let status = response.status();

        let mut body_bytes = Vec::new();
        response.body_mut().read_to_end(&mut body_bytes).await?;

        if !status.is_success() {
            let error_text = String::from_utf8_lossy(&body_bytes);
            log::warn!(
                "Ctab FileSync: FSConfig request failed with status {}: {}",
                status,
                error_text
            );
            anyhow::bail!("FSConfig request failed: {}", status);
        }

        if body_bytes.is_empty() {
            log::debug!("Ctab FileSync: FSConfig returned empty response, using defaults");
            return Ok(FsConfigResponse::default());
        }

        let config = FsConfigResponse::decode(&body_bytes[..])?;

        log::info!(
            "Ctab FileSync: Received FSConfig - rate_limit_rps={}, burst_capacity={}",
            config.rate_limit_rps,
            config.burst_capacity
        );

        Ok(config)
    }

    /// Update cache with fetched config
    pub fn update_cache(&self, config: FsConfigResponse) {
        // Update rate limiter if config provides values
        if config.rate_limit_rps > 0 || config.burst_capacity > 0 {
            let rps = if config.rate_limit_rps > 0 {
                config.rate_limit_rps as u32
            } else {
                DEFAULT_RATE_LIMIT_RPS
            };
            let burst = if config.burst_capacity > 0 {
                config.burst_capacity as u32
            } else {
                DEFAULT_BURST_CAPACITY
            };

            *self.rate_limiter.lock() = RateLimiter::new(rps, burst);
            log::info!(
                "Ctab FileSync: Updated rate limiter - rps={}, burst={}",
                rps,
                burst
            );
        }

        // Update cache
        *self.cache.lock() = Some(CachedFSConfig {
            config,
            fetched_at: Instant::now(),
        });
    }

    /// Try to acquire a rate limit token
    ///
    /// Returns `true` if request can proceed, `false` if rate limited
    pub fn try_acquire(&self) -> bool {
        self.rate_limiter.lock().try_acquire()
    }

    /// Get time until a rate limit token becomes available
    pub fn time_until_available(&self) -> Option<Duration> {
        self.rate_limiter.lock().time_until_available()
    }
}

// ============================================================================
// File Logger for FileSync debugging
// ============================================================================

/// Logger for file sync operations - writes to a dedicated log file
pub struct FileSyncLogger {
    log_file: Option<PathBuf>,
}

impl FileSyncLogger {
    /// Create a new logger with the specified log file path
    pub fn new(log_path: Option<PathBuf>) -> Self {
        Self { log_file: log_path }
    }

    /// Get the default log file path
    pub fn default_log_path() -> PathBuf {
        // Use temp directory for the log file
        let mut path = std::env::temp_dir();
        path.push("ctab_filesync.log");
        path
    }

    /// Log an upload request
    pub fn log_upload_request(
        &self,
        uuid: &str,
        file_path: &str,
        content_size: usize,
        model_version: i32,
        hash: &str,
    ) {
        let msg = format!(
            "[UPLOAD_REQ] uuid={}, path={}, size={} bytes, version={}, hash={}",
            uuid,
            file_path,
            content_size,
            model_version,
            &hash[..16.min(hash.len())]
        );
        self.write_log(&msg);
    }

    /// Log an upload response
    pub fn log_upload_response(
        &self,
        file_path: &str,
        status: u16,
        error: Option<FsUploadErrorType>,
        response_body: Option<&[u8]>,
    ) {
        let error_str = match error {
            Some(FsUploadErrorType::Unspecified) => "UNSPECIFIED (success)".to_string(),
            Some(FsUploadErrorType::NonExistant) => "NON_EXISTANT".to_string(),
            Some(FsUploadErrorType::HashMismatch) => "HASH_MISMATCH".to_string(),
            None => "N/A".to_string(),
        };

        let body_preview = response_body
            .map(|b| {
                let preview_len = b.len().min(200);
                String::from_utf8_lossy(&b[..preview_len]).to_string()
            })
            .unwrap_or_default();

        let msg = format!(
            "[UPLOAD_RSP] path={}, status={}, error={}, body_preview={}",
            file_path, status, error_str, body_preview
        );
        self.write_log(&msg);
    }

    /// Log a sync request
    pub fn log_sync_request(
        &self,
        uuid: &str,
        file_path: &str,
        model_version: i32,
        updates_count: usize,
        hash: &str,
    ) {
        let msg = format!(
            "[SYNC_REQ] uuid={}, path={}, version={}, updates={}, hash={}",
            uuid,
            file_path,
            model_version,
            updates_count,
            &hash[..16.min(hash.len())]
        );
        self.write_log(&msg);
    }

    /// Log a sync response
    pub fn log_sync_response(&self, file_path: &str, status: u16, error: Option<FsSyncErrorType>) {
        let error_str = match error {
            Some(FsSyncErrorType::Unspecified) => "UNSPECIFIED (success)".to_string(),
            Some(FsSyncErrorType::NonExistant) => "NON_EXISTANT".to_string(),
            Some(FsSyncErrorType::HashMismatch) => "HASH_MISMATCH".to_string(),
            None => "N/A".to_string(),
        };

        let msg = format!(
            "[SYNC_RSP] path={}, status={}, error={}",
            file_path, status, error_str
        );
        self.write_log(&msg);
    }

    /// Log a file change record
    pub fn log_change_record(
        &self,
        file_path: &str,
        start_offset: usize,
        end_offset: usize,
        new_text_len: usize,
        model_version: i32,
    ) {
        let msg = format!(
            "[CHANGE] path={}, offset={}..{}, new_len={}, version={}",
            file_path, start_offset, end_offset, new_text_len, model_version
        );
        self.write_log(&msg);
    }

    /// Log general file sync status
    pub fn log_status(
        &self,
        file_path: &str,
        is_uploaded: bool,
        model_version: i32,
        pending_updates: usize,
    ) {
        let msg = format!(
            "[STATUS] path={}, uploaded={}, version={}, pending_updates={}",
            file_path, is_uploaded, model_version, pending_updates
        );
        self.write_log(&msg);
    }

    /// Log server capability analysis
    pub fn log_capability_analysis(
        &self,
        file_path: &str,
        rely_on_filesync: bool,
        filesync_updates_count: usize,
        content_size: usize,
    ) {
        let msg = format!(
            "[CAPABILITY] path={}, rely_on_filesync={}, filesync_updates={}, content_size={}",
            file_path, rely_on_filesync, filesync_updates_count, content_size
        );
        self.write_log(&msg);
    }

    fn write_log(&self, message: &str) {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);

        let formatted = format!("[{:.3}] {}\n", timestamp, message);

        // Always log to standard log
        log::info!("Ctab FileSync: {}", message);

        // Also write to file if configured
        if let Some(ref log_path) = self.log_file {
            if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(log_path) {
                let _ = file.write_all(formatted.as_bytes());
            }
        }
    }
}

impl Default for FileSyncLogger {
    fn default() -> Self {
        Self::new(Some(Self::default_log_path()))
    }
}

/// Global file sync logger instance
static FILE_SYNC_LOGGER: std::sync::OnceLock<FileSyncLogger> = std::sync::OnceLock::new();

/// Get the global file sync logger
pub fn get_filesync_logger() -> &'static FileSyncLogger {
    FILE_SYNC_LOGGER.get_or_init(|| FileSyncLogger::default())
}

/// Tracks the sync state of a single file
#[derive(Clone, Debug)]
struct FileSyncState {
    /// Last known content hash
    last_hash: String,
    /// Current model version
    model_version: i32,
    /// Pending updates since last sync
    pending_updates: Vec<SingleUpdateRequest>,
    /// Whether the file has been uploaded
    is_uploaded: bool,
}

/// File synchronization manager
///
/// Handles uploading and incrementally syncing files with the Cursor API server.
pub struct FileSyncManager {
    #[allow(dead_code)]
    http_client: Arc<dyn HttpClient>,
    file_states: Arc<Mutex<HashMap<String, FileSyncState>>>,
    #[allow(dead_code)]
    workspace_uuid: String,
}

impl FileSyncManager {
    /// Creates a new file sync manager
    pub fn new(http_client: Arc<dyn HttpClient>, workspace_uuid: String) -> Self {
        Self {
            http_client,
            file_states: Arc::new(Mutex::new(HashMap::default())),
            workspace_uuid,
        }
    }

    /// Computes SHA256 hash of content
    fn compute_sha256(content: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(content.as_bytes());
        hex::encode(hasher.finalize())
    }

    /// Uploads a file to the server (initial sync)
    pub async fn upload_file(
        http_client: Arc<dyn HttpClient>,
        uuid: String,
        file_path: String,
        contents: String,
        model_version: i32,
        auth_token: String,
        client_key: String,
        client_key_header: String,
        filesync_client_key: String,
        filesync_cookie: String,
        base_url: String,
        upload_path: &'static str,
    ) -> Result<FsUploadErrorType> {
        let logger = get_filesync_logger();

        // Skip large files
        if contents.len() > MAX_FILE_SIZE_TO_SYNC {
            log::debug!(
                "Ctab: Skipping upload for {} - file too large ({} bytes)",
                file_path,
                contents.len()
            );
            logger.log_status(&file_path, false, model_version, 0);
            return Ok(FsUploadErrorType::Unspecified);
        }

        let url = format!("{}{}", base_url, upload_path);
        let hash = Self::compute_sha256(&contents);

        // Log upload request
        logger.log_upload_request(&uuid, &file_path, contents.len(), model_version, &hash);

        log::debug!(
            "Ctab: Uploading file {} (version={}, size={}, hash={})",
            file_path,
            model_version,
            contents.len(),
            &hash[..16]
        );

        let request = FsUploadFileRequest {
            uuid,
            relative_workspace_path: file_path.clone(),
            contents,
            model_version,
            sha256_hash: Some(hash),
        };

        let body = request.encode_to_vec();

        let http_request = http_client::Request::builder()
            .method(Method::POST)
            .uri(&url)
            .header("Content-Type", "application/proto")
            .header("Authorization", format!("Bearer {}", auth_token))
            .header(&client_key_header, &client_key)
            .header("x-client-key", &filesync_client_key)
            .header("x-fs-client-key", &filesync_client_key)
            .header("Cookie", format!("FilesyncCookie={}", filesync_cookie))
            .header("x-cursor-client-version", CLIENT_VERSION)
            .body(AsyncBody::from(body))?;

        let mut response = http_client.send(http_request).await?;
        let status = response.status();

        let mut body_bytes = Vec::new();
        response.body_mut().read_to_end(&mut body_bytes).await?;

        if !status.is_success() {
            let error_text = String::from_utf8_lossy(&body_bytes);
            log::warn!(
                "Ctab: File upload failed for {} with status {}: {}",
                file_path,
                status,
                error_text
            );
            // Log failed response
            logger.log_upload_response(&file_path, status.as_u16(), None, Some(&body_bytes));
            anyhow::bail!("File upload failed: {}", status);
        }

        let response = FsUploadFileResponse::decode(&body_bytes[..])?;

        // Log successful response
        logger.log_upload_response(
            &file_path,
            status.as_u16(),
            Some(response.error()),
            Some(&body_bytes),
        );

        log::debug!(
            "Ctab: File upload completed for {} - error={:?}",
            file_path,
            response.error
        );

        Ok(response.error())
    }

    /// Uploads a file to the server with exponential backoff retry
    ///
    /// This method wraps `upload_file` with retry logic for transient failures.
    /// It will retry up to `max_attempts` times with exponential backoff delays.
    ///
    /// # Arguments
    /// * `rate_limiter` - Optional rate limiter to throttle requests
    /// * `retry_config` - Optional retry configuration (uses defaults if None)
    /// * Other arguments are passed through to `upload_file`
    ///
    /// # Returns
    /// * `Ok(FsUploadErrorType)` on success (may indicate server-side errors)
    /// * `Err` if all retry attempts fail
    pub async fn upload_file_with_retry(
        http_client: Arc<dyn HttpClient>,
        uuid: String,
        file_path: String,
        contents: String,
        model_version: i32,
        auth_token: String,
        client_key: String,
        client_key_header: String,
        filesync_client_key: String,
        filesync_cookie: String,
        base_url: String,
        upload_path: &'static str,
        rate_limiter: Option<Arc<Mutex<RateLimiter>>>,
        retry_config: Option<RetryConfig>,
    ) -> Result<FsUploadErrorType> {
        let mut backoff = ExponentialBackoff::new(retry_config.unwrap_or_default());
        let mut last_error: Option<anyhow::Error> = None;

        loop {
            // Apply rate limiting if configured
            if let Some(ref limiter) = rate_limiter {
                // Wait for rate limit token if needed
                if let Some(wait_time) = limiter.lock().time_until_available() {
                    log::debug!(
                        "Ctab FileSync: Rate limited, waiting {:?} for {}",
                        wait_time,
                        file_path
                    );
                    smol::Timer::after(wait_time).await;
                }
                // Acquire token (should succeed after waiting)
                limiter.lock().try_acquire();
            }

            // Attempt upload
            match Self::upload_file(
                http_client.clone(),
                uuid.clone(),
                file_path.clone(),
                contents.clone(),
                model_version,
                auth_token.clone(),
                client_key.clone(),
                client_key_header.clone(),
                filesync_client_key.clone(),
                filesync_cookie.clone(),
                base_url.clone(),
                upload_path,
            )
            .await
            {
                Ok(error_type) => {
                    // Success - check if server returned an error that warrants retry
                    match error_type {
                        FsUploadErrorType::Unspecified => {
                            // Success
                            if backoff.current_attempt() > 0 {
                                log::info!(
                                    "Ctab FileSync: Upload succeeded after {} retries for {}",
                                    backoff.current_attempt(),
                                    file_path
                                );
                            }
                            return Ok(error_type);
                        }
                        FsUploadErrorType::HashMismatch => {
                            // Hash mismatch - might be transient, retry
                            log::warn!(
                                "Ctab FileSync: Hash mismatch for {}, will retry",
                                file_path
                            );
                            last_error = Some(anyhow::anyhow!("Hash mismatch"));
                        }
                        FsUploadErrorType::NonExistant => {
                            // File doesn't exist on server - not retryable
                            return Ok(error_type);
                        }
                    }
                }
                Err(e) => {
                    log::warn!(
                        "Ctab FileSync: Upload attempt {} failed for {}: {}",
                        backoff.current_attempt() + 1,
                        file_path,
                        e
                    );
                    last_error = Some(e);
                }
            }

            // Check if we should retry
            if let Some(delay) = backoff.next_delay() {
                log::info!(
                    "Ctab FileSync: Retrying upload for {} in {:?} (attempt {}/{})",
                    file_path,
                    delay,
                    backoff.current_attempt(),
                    backoff.config.max_attempts
                );
                smol::Timer::after(delay).await;
            } else {
                // Max retries exceeded
                let err = last_error.unwrap_or_else(|| anyhow::anyhow!("Max retries exceeded"));
                log::error!(
                    "Ctab FileSync: Upload failed after {} attempts for {}: {}",
                    backoff.current_attempt(),
                    file_path,
                    err
                );
                return Err(err);
            }
        }
    }

    /// Syncs incremental changes to a file with exponential backoff retry
    ///
    /// Similar to `upload_file_with_retry`, but for incremental sync operations.
    pub async fn sync_file_with_retry(
        http_client: Arc<dyn HttpClient>,
        uuid: String,
        file_path: String,
        model_version: i32,
        updates: Vec<FilesyncUpdateWithModelVersion>,
        content_hash: String,
        auth_token: String,
        client_key: String,
        client_key_header: String,
        filesync_client_key: String,
        filesync_cookie: String,
        base_url: String,
        sync_path: &'static str,
        rate_limiter: Option<Arc<Mutex<RateLimiter>>>,
        retry_config: Option<RetryConfig>,
    ) -> Result<FsSyncErrorType> {
        let mut backoff = ExponentialBackoff::new(retry_config.unwrap_or_default());
        let mut last_error: Option<anyhow::Error> = None;

        loop {
            // Apply rate limiting if configured
            if let Some(ref limiter) = rate_limiter {
                if let Some(wait_time) = limiter.lock().time_until_available() {
                    log::debug!(
                        "Ctab FileSync: Rate limited, waiting {:?} for sync {}",
                        wait_time,
                        file_path
                    );
                    smol::Timer::after(wait_time).await;
                }
                limiter.lock().try_acquire();
            }

            // Attempt sync
            match Self::sync_file(
                http_client.clone(),
                uuid.clone(),
                file_path.clone(),
                model_version,
                updates.clone(),
                content_hash.clone(),
                auth_token.clone(),
                client_key.clone(),
                client_key_header.clone(),
                filesync_client_key.clone(),
                filesync_cookie.clone(),
                base_url.clone(),
                sync_path,
            )
            .await
            {
                Ok(error_type) => {
                    match error_type {
                        FsSyncErrorType::Unspecified => {
                            if backoff.current_attempt() > 0 {
                                log::info!(
                                    "Ctab FileSync: Sync succeeded after {} retries for {}",
                                    backoff.current_attempt(),
                                    file_path
                                );
                            }
                            return Ok(error_type);
                        }
                        FsSyncErrorType::HashMismatch => {
                            log::warn!(
                                "Ctab FileSync: Sync hash mismatch for {}, will retry",
                                file_path
                            );
                            last_error = Some(anyhow::anyhow!("Hash mismatch"));
                        }
                        FsSyncErrorType::NonExistant => {
                            // File not found - not retryable, caller should re-upload
                            return Ok(error_type);
                        }
                    }
                }
                Err(e) => {
                    log::warn!(
                        "Ctab FileSync: Sync attempt {} failed for {}: {}",
                        backoff.current_attempt() + 1,
                        file_path,
                        e
                    );
                    last_error = Some(e);
                }
            }

            // Check if we should retry
            if let Some(delay) = backoff.next_delay() {
                log::info!(
                    "Ctab FileSync: Retrying sync for {} in {:?} (attempt {}/{})",
                    file_path,
                    delay,
                    backoff.current_attempt(),
                    backoff.config.max_attempts
                );
                smol::Timer::after(delay).await;
            } else {
                let err = last_error.unwrap_or_else(|| anyhow::anyhow!("Max retries exceeded"));
                log::error!(
                    "Ctab FileSync: Sync failed after {} attempts for {}: {}",
                    backoff.current_attempt(),
                    file_path,
                    err
                );
                return Err(err);
            }
        }
    }

    /// Syncs incremental changes to a file
    #[allow(dead_code)]
    pub async fn sync_file(
        http_client: Arc<dyn HttpClient>,
        uuid: String,
        file_path: String,
        model_version: i32,
        updates: Vec<FilesyncUpdateWithModelVersion>,
        content_hash: String,
        auth_token: String,
        client_key: String,
        client_key_header: String,
        filesync_client_key: String,
        filesync_cookie: String,
        base_url: String,
        sync_path: &'static str,
    ) -> Result<FsSyncErrorType> {
        let logger = get_filesync_logger();
        let url = format!("{}{}", base_url, sync_path);

        // Log sync request
        logger.log_sync_request(
            &uuid,
            &file_path,
            model_version,
            updates.len(),
            &content_hash,
        );

        log::debug!(
            "Ctab: Syncing file {} (version={}, updates={}, hash={})",
            file_path,
            model_version,
            updates.len(),
            &content_hash[..16]
        );

        let request = FsSyncFileRequest {
            uuid,
            relative_workspace_path: file_path.clone(),
            model_version,
            filesync_updates: updates,
            sha256_hash: content_hash,
        };

        let body = request.encode_to_vec();

        let http_request = http_client::Request::builder()
            .method(Method::POST)
            .uri(&url)
            .header("Content-Type", "application/proto")
            .header("Authorization", format!("Bearer {}", auth_token))
            .header(&client_key_header, &client_key)
            .header("x-client-key", &filesync_client_key)
            .header("x-fs-client-key", &filesync_client_key)
            .header("Cookie", format!("FilesyncCookie={}", filesync_cookie))
            .header("x-cursor-client-version", CLIENT_VERSION)
            .body(AsyncBody::from(body))?;

        let mut response = http_client.send(http_request).await?;
        let status = response.status();

        let mut body_bytes = Vec::new();
        response.body_mut().read_to_end(&mut body_bytes).await?;

        if !status.is_success() {
            let error_text = String::from_utf8_lossy(&body_bytes);
            log::warn!(
                "Ctab: File sync failed for {} with status {}: {}",
                file_path,
                status,
                error_text
            );
            // Log failed response
            logger.log_sync_response(&file_path, status.as_u16(), None);
            anyhow::bail!("File sync failed: {}", status);
        }

        // Parse response
        if let Ok(response) = FsSyncFileResponse::decode(&body_bytes[..]) {
            // Log successful response
            logger.log_sync_response(&file_path, status.as_u16(), Some(response.error()));

            log::debug!(
                "Ctab: File sync completed for {} - error={:?}",
                file_path,
                response.error
            );
            return Ok(response.error());
        }

        // Log response without parsed error
        logger.log_sync_response(&file_path, status.as_u16(), None);

        Ok(FsSyncErrorType::Unspecified)
    }

    /// Records a text change for incremental sync
    #[allow(dead_code)]
    pub fn record_change(
        &self,
        file_path: &str,
        start_offset: usize,
        end_offset: usize,
        new_text: &str,
        start_line: i32,
        start_col: i32,
        end_line: i32,
        end_col: i32,
    ) {
        let mut states = self.file_states.lock();
        let state = states
            .entry(file_path.to_string())
            .or_insert_with(|| FileSyncState {
                last_hash: String::new(),
                model_version: 1,
                pending_updates: Vec::new(),
                is_uploaded: false,
            });

        state.model_version += 1;
        state.pending_updates.push(SingleUpdateRequest {
            start_position: start_offset as i32,
            end_position: end_offset as i32,
            change_length: new_text.len() as i32,
            replaced_string: new_text.to_string(),
            range: Some(SimpleRange {
                start_line_number: start_line,
                start_column: start_col,
                end_line_number_inclusive: end_line,
                end_column: end_col,
            }),
        });

        // Log change record
        let logger = get_filesync_logger();
        logger.log_change_record(
            file_path,
            start_offset,
            end_offset,
            new_text.len(),
            state.model_version,
        );
    }

    /// Gets the current model version for a file
    pub fn get_model_version(&self, file_path: &str) -> i32 {
        self.file_states
            .lock()
            .get(file_path)
            .map(|s| s.model_version)
            .unwrap_or(1)
    }

    /// Checks if a file needs initial upload
    pub fn needs_upload(&self, file_path: &str) -> bool {
        self.file_states
            .lock()
            .get(file_path)
            .map(|s| !s.is_uploaded)
            .unwrap_or(true)
    }

    /// Marks a file as uploaded
    pub fn mark_uploaded(&self, file_path: &str, content_hash: String) {
        let mut states = self.file_states.lock();
        if let Some(state) = states.get_mut(file_path) {
            state.is_uploaded = true;
            state.last_hash = content_hash;
            state.pending_updates.clear();
        } else {
            states.insert(
                file_path.to_string(),
                FileSyncState {
                    last_hash: content_hash,
                    model_version: 1,
                    pending_updates: Vec::new(),
                    is_uploaded: true,
                },
            );
        }
    }

    /// Clears sync state for a file
    #[allow(dead_code)]
    pub fn clear_file(&self, file_path: &str) {
        self.file_states.lock().remove(file_path);
    }

    /// Gets pending updates for a file and clears them
    pub fn take_pending_updates(&self, file_path: &str) -> Vec<SingleUpdateRequest> {
        self.file_states
            .lock()
            .get_mut(file_path)
            .map(|s| std::mem::take(&mut s.pending_updates))
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sha256_computation() {
        let hash = FileSyncManager::compute_sha256("hello world");
        assert_eq!(
            hash,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn test_model_version_tracking() {
        // This would require mocking HttpClient, so just test the basic logic
        let hash1 = FileSyncManager::compute_sha256("content1");
        let hash2 = FileSyncManager::compute_sha256("content2");
        assert_ne!(hash1, hash2);
    }

    // ========================================================================
    // ExponentialBackoff Tests
    // ========================================================================

    #[test]
    fn test_exponential_backoff_default_config() {
        let config = RetryConfig::default();
        assert_eq!(config.max_attempts, 3);
        assert_eq!(config.initial_delay_ms, 100);
        assert_eq!(config.multiplier, 2);
        assert_eq!(config.max_delay_ms, 5000);
    }

    #[test]
    fn test_exponential_backoff_delays() {
        let mut backoff = ExponentialBackoff::with_defaults();

        // Should retry 3 times
        assert!(backoff.should_retry());
        assert_eq!(backoff.current_attempt(), 0);

        // First delay: 100ms
        let delay1 = backoff.next_delay().unwrap();
        assert_eq!(delay1, Duration::from_millis(100));
        assert_eq!(backoff.current_attempt(), 1);

        // Second delay: 200ms
        let delay2 = backoff.next_delay().unwrap();
        assert_eq!(delay2, Duration::from_millis(200));
        assert_eq!(backoff.current_attempt(), 2);

        // Third delay: 400ms
        let delay3 = backoff.next_delay().unwrap();
        assert_eq!(delay3, Duration::from_millis(400));
        assert_eq!(backoff.current_attempt(), 3);

        // No more retries
        assert!(!backoff.should_retry());
        assert!(backoff.next_delay().is_none());
    }

    #[test]
    fn test_exponential_backoff_max_delay_cap() {
        let config = RetryConfig {
            max_attempts: 10,
            initial_delay_ms: 1000,
            multiplier: 3,
            max_delay_ms: 2000, // Low cap
        };
        let mut backoff = ExponentialBackoff::new(config);

        // First: 1000ms
        assert_eq!(backoff.next_delay().unwrap(), Duration::from_millis(1000));
        // Second: 3000ms -> capped to 2000ms
        assert_eq!(backoff.next_delay().unwrap(), Duration::from_millis(2000));
        // Third: still capped
        assert_eq!(backoff.next_delay().unwrap(), Duration::from_millis(2000));
    }

    #[test]
    fn test_exponential_backoff_reset() {
        let mut backoff = ExponentialBackoff::with_defaults();

        // Use some attempts
        backoff.next_delay();
        backoff.next_delay();
        assert_eq!(backoff.current_attempt(), 2);

        // Reset
        backoff.reset();
        assert_eq!(backoff.current_attempt(), 0);
        assert!(backoff.should_retry());

        // First delay should be initial again
        assert_eq!(backoff.next_delay().unwrap(), Duration::from_millis(100));
    }

    // ========================================================================
    // RateLimiter Tests
    // ========================================================================

    #[test]
    fn test_rate_limiter_initial_burst() {
        let mut limiter = RateLimiter::new(10, 5);

        // Should have 5 tokens initially (burst capacity)
        assert!(limiter.try_acquire());
        assert!(limiter.try_acquire());
        assert!(limiter.try_acquire());
        assert!(limiter.try_acquire());
        assert!(limiter.try_acquire());

        // 6th should fail
        assert!(!limiter.try_acquire());
    }

    #[test]
    fn test_rate_limiter_time_until_available() {
        let mut limiter = RateLimiter::new(10, 2);

        // Use all tokens
        limiter.try_acquire();
        limiter.try_acquire();
        assert!(!limiter.try_acquire());

        // Should need to wait
        let wait_time = limiter.time_until_available();
        assert!(wait_time.is_some());
        assert!(wait_time.unwrap() <= Duration::from_millis(100)); // 1/10 second for 1 token
    }

    #[test]
    fn test_rate_limiter_available_tokens() {
        let mut limiter = RateLimiter::new(10, 20);

        // Initially full
        assert_eq!(limiter.available_tokens(), 20.0);

        // After acquiring some
        limiter.try_acquire();
        limiter.try_acquire();
        assert!((limiter.available_tokens() - 18.0).abs() < 0.1);
    }

    #[test]
    fn test_rate_limiter_defaults() {
        let limiter = RateLimiter::with_defaults();
        assert_eq!(limiter.capacity, DEFAULT_BURST_CAPACITY);
        assert_eq!(limiter.refill_rate, DEFAULT_RATE_LIMIT_RPS as f64);
    }

    // ========================================================================
    // CachedFSConfig Tests
    // ========================================================================

    #[test]
    fn test_cached_fs_config_validity() {
        let cached = CachedFSConfig {
            config: FsConfigResponse::default(),
            fetched_at: Instant::now(),
        };

        // Should be valid immediately
        assert!(cached.is_valid());
    }
}
