//! File synchronization service for Cometix
//!
//! This module handles file synchronization with the Cursor API server,
//! enabling the server to maintain an up-to-date view of the user's workspace.

use std::sync::Arc;

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
    http_client: Arc<dyn HttpClient>,
    file_states: Arc<Mutex<HashMap<String, FileSyncState>>>,
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
        base_url: String,
        upload_path: &'static str,
    ) -> Result<FsUploadErrorType> {
        // Skip large files
        if contents.len() > MAX_FILE_SIZE_TO_SYNC {
            log::debug!(
                "Cometix: Skipping upload for {} - file too large ({} bytes)",
                file_path,
                contents.len()
            );
            return Ok(FsUploadErrorType::Unspecified);
        }

        let url = format!("{}{}", base_url, upload_path);
        let hash = Self::compute_sha256(&contents);

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
            anyhow::bail!("File upload failed: {}", status);
        }

        let response = FsUploadFileResponse::decode(&body_bytes[..])?;
        log::debug!(
            "Cometix: File upload completed for {} - error={:?}",
            file_path,
            response.error
        );

        Ok(response.error())
    }

    /// Syncs incremental changes to a file
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
        base_url: String,
        sync_path: &'static str,
    ) -> Result<FsSyncErrorType> {
        let url = format!("{}{}", base_url, sync_path);

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
            anyhow::bail!("File sync failed: {}", status);
        }

        // Parse response
        if let Ok(response) = FsSyncFileResponse::decode(&body_bytes[..]) {
            log::debug!(
                "Cometix: File sync completed for {} - error={:?}",
                file_path,
                response.error
            );
            return Ok(response.error());
        }

        Ok(FsSyncErrorType::Unspecified)
    }

    /// Records a text change for incremental sync
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
