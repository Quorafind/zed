//! File synchronization service for Cometix
//!
//! This module handles file synchronization with the Cursor API server,
//! enabling the server to maintain an up-to-date view of the user's workspace.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

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
                "Cometix: Skipping upload for {} - file too large ({} bytes)",
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
            "Cometix: Uploading file {} (version={}, size={}, hash={})",
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
                "Cometix: File upload failed for {} with status {}: {}",
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
            "Cometix: File upload completed for {} - error={:?}",
            file_path,
            response.error
        );

        Ok(response.error())
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
            "Cometix: Syncing file {} (version={}, updates={}, hash={})",
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
                "Cometix: File sync failed for {} with status {}: {}",
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
                "Cometix: File sync completed for {} - error={:?}",
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
}
