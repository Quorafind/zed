//! Cometix completion provider implementation
//!
//! Implements the EditPredictionProvider trait for Cursor AI completions.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use edit_prediction::{Direction, EditPrediction, EditPredictionProvider};
use futures::AsyncReadExt;
use gpui::{App, Context, Entity, EntityId, SharedString, Task};
use http_client::{AsyncBody, HttpClient, Method};
use language::{Anchor, Buffer, DiagnosticSeverity, Point, ToOffset};
use parking_lot::Mutex;
use prost::Message;
use settings::Settings;
use sha2::{Digest, Sha256};
use unicode_segmentation::UnicodeSegmentation;

use crate::CtabSettings;
use crate::completion_differ::{CompletionContext, SmartCompletionDiffer};
use crate::diff_tracker::DiffTracker;
use crate::file_sync::FileSyncManager;
use crate::proto::{
    AdditionalFile, CppAppendRequest, CppConfigRequest, CppConfigResponse, CppContextItem, CppFate,
    CppFileDiffHistory, CppIntentInfo, CurrentFileInfo, CursorPosition, CursorRange,
    Diagnostic as ProtoDiagnostic, FilesyncUpdateWithModelVersion, FsUploadErrorType,
    RecordCppFateRequest, StreamCppRequest, StreamCppResponse,
    diagnostic::DiagnosticSeverity as ProtoDiagnosticSeverity,
};
use project::Project;

/// Cache TTL for CppConfig (5 minutes)
const CONFIG_CACHE_TTL_SECS: u64 = 300;

/// Client version to report
const CLIENT_VERSION: &str = "1.6.1-zed";

/// Maximum number of additional files to include in context
const MAX_ADDITIONAL_FILES: usize = 10;

/// Maximum content size per additional file (in bytes)
const MAX_ADDITIONAL_FILE_SIZE: usize = 50_000;

/// Cached server configuration
#[derive(Clone, Debug)]
struct CachedConfig {
    config: CppConfigResponse,
    fetched_at: std::time::Instant,
}

impl CachedConfig {
    fn is_valid(&self) -> bool {
        self.fetched_at.elapsed().as_secs() < CONFIG_CACHE_TTL_SECS
    }
}

pub struct CtabCompletionProvider {
    http_client: Arc<dyn HttpClient>,
    project: Option<Entity<Project>>,
    diff_tracker: Arc<Mutex<DiffTracker>>,
    config_cache: Arc<Mutex<Option<CachedConfig>>>,
    file_sync_manager: Arc<FileSyncManager>,
    buffer_id: Option<EntityId>,
    file_extension: Option<String>,
    current_file_path: Option<String>,
    pending_refresh: Option<Task<Result<()>>>,
    current_completion: Option<CompletionState>,
    workspace_id: String,
    /// FileSync client key (64-char hex) - used for x-client-key and x-fs-client-key headers
    filesync_client_key: String,
    /// FileSync cookie value (32-char hex) - used for FilesyncCookie header
    filesync_cookie: String,
    /// Whether to skip the next refresh request (used when completion indicates no retrigger)
    skip_next_refresh: bool,
}

struct CompletionState {
    text: String,
    binding_id: Option<String>,
    #[allow(dead_code)]
    range_start: Anchor,
    #[allow(dead_code)]
    range_end: Anchor,
    #[allow(dead_code)]
    api_range: Option<(i32, i32)>,
    /// Whether to retrigger completion after accepting this one
    should_retrigger: bool,
}

impl CtabCompletionProvider {
    pub fn new(http_client: Arc<dyn HttpClient>, project: Option<Entity<Project>>) -> Self {
        // Generate workspace_id - will be updated when we have project context
        let workspace_id = generate_workspace_id();
        Self {
            http_client: http_client.clone(),
            project,
            diff_tracker: Arc::new(Mutex::new(DiffTracker::new())),
            config_cache: Arc::new(Mutex::new(None)),
            file_sync_manager: Arc::new(FileSyncManager::new(http_client, workspace_id.clone())),
            buffer_id: None,
            file_extension: None,
            current_file_path: None,
            pending_refresh: None,
            current_completion: None,
            workspace_id,
            filesync_client_key: generate_filesync_client_key(),
            filesync_cookie: generate_filesync_cookie(),
            skip_next_refresh: false,
        }
    }

    /// Update workspace_id based on actual workspace path for stability
    fn ensure_stable_workspace_id(&mut self, cx: &App) {
        if let Some(project) = &self.project {
            let project = project.read(cx);
            if let Some(worktree) = project.worktrees(cx).next() {
                let worktree = worktree.read(cx);
                let root_path = worktree.abs_path().to_string_lossy().to_string();
                let stable_id = generate_stable_workspace_id(&root_path);
                if self.workspace_id != stable_id {
                    log::info!(
                        "Cometix: Updating workspace_id from {} to {} (path: {})",
                        self.workspace_id,
                        stable_id,
                        root_path
                    );
                    self.workspace_id = stable_id;
                }
            }
        }
    }

    fn compute_sha256(content: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(content.as_bytes());
        hex::encode(hasher.finalize())
    }

    fn detect_language(file_path: &str) -> String {
        match file_path.rsplit('.').next() {
            Some("rs") => "rust",
            Some("ts") => "typescript",
            Some("tsx") => "typescriptreact",
            Some("js") => "javascript",
            Some("jsx") => "javascriptreact",
            Some("py") => "python",
            Some("go") => "go",
            Some("c") => "c",
            Some("cpp" | "cc" | "cxx") => "cpp",
            Some("h" | "hpp") => "cpp",
            Some("java") => "java",
            Some("rb") => "ruby",
            Some("php") => "php",
            Some("swift") => "swift",
            Some("kt") => "kotlin",
            Some("scala") => "scala",
            Some("cs") => "csharp",
            Some("fs") => "fsharp",
            Some("html") => "html",
            Some("css") => "css",
            Some("scss") => "scss",
            Some("less") => "less",
            Some("json") => "json",
            Some("yaml" | "yml") => "yaml",
            Some("toml") => "toml",
            Some("xml") => "xml",
            Some("md") => "markdown",
            Some("sql") => "sql",
            Some("sh" | "bash") => "shellscript",
            Some("ps1") => "powershell",
            Some("lua") => "lua",
            Some("r") => "r",
            Some("dart") => "dart",
            Some("ex" | "exs") => "elixir",
            Some("erl") => "erlang",
            Some("hs") => "haskell",
            Some("ml") => "ocaml",
            Some("clj") => "clojure",
            Some("vim") => "vim",
            Some("zig") => "zig",
            Some("v") => "vlang",
            Some("nim") => "nim",
            _ => "plaintext",
        }
        .to_string()
    }

    /// Fetches completion from the Cursor API with proper streaming support.
    ///
    /// The API uses gRPC-Web protocol with Length-Prefixed Messages (LPM):
    /// - Each message has a 5-byte header: 1 byte flag + 4 bytes big-endian length
    /// - Messages are streamed until `done_stream` flag is set
    async fn fetch_completion(
        http_client: Arc<dyn HttpClient>,
        request: StreamCppRequest,
        auth_token: String,
        client_key: String,
        client_key_header: String,
        filesync_client_key: String,
        filesync_cookie: String,
        base_url: String,
        stream_path: &'static str,
        uses_connect_rpc: bool,
    ) -> Result<StreamCppResponse> {
        let url = format!("{}{}", base_url, stream_path);

        log::info!("Cometix: Sending request to {}", url);

        let proto_body = request.encode_to_vec();
        log::info!(
            "Cometix: Request proto body size: {} bytes",
            proto_body.len()
        );

        // Connect RPC requires envelope format: [flags(1)][length(4)][payload]
        // flags: 0 = uncompressed
        let body = if uses_connect_rpc {
            let mut envelope = Vec::with_capacity(5 + proto_body.len());
            envelope.push(0u8); // flags: 0 = no compression
            envelope.extend_from_slice(&(proto_body.len() as u32).to_be_bytes()); // length
            envelope.extend_from_slice(&proto_body); // payload
            log::info!(
                "Cometix: Wrapped in Connect envelope, total size: {} bytes",
                envelope.len()
            );
            envelope
        } else {
            proto_body
        };

        // Connect RPC uses different Content-Type header
        let content_type = if uses_connect_rpc {
            "application/connect+proto"
        } else {
            "application/proto"
        };

        let http_request = http_client::Request::builder()
            .method(Method::POST)
            .uri(&url)
            .header("Content-Type", content_type)
            .header("Connect-Protocol-Version", "1")
            .header("Authorization", format!("Bearer {}", auth_token))
            .header(&client_key_header, &client_key)
            .header("x-client-key", &filesync_client_key)
            .header("x-fs-client-key", &filesync_client_key)
            .header("Cookie", format!("FilesyncCookie={}", filesync_cookie))
            .header("x-cursor-client-version", CLIENT_VERSION)
            .header("User-Agent", "connectrpc/1.6.1")
            .body(AsyncBody::from(body))?;

        log::debug!(
            "Cometix: Request headers - Content-Type: {}, {}: {}",
            content_type,
            client_key_header,
            &client_key[..client_key.len().min(20)]
        );

        let mut response = http_client.send(http_request).await?;
        let status = response.status();

        log::info!("Cometix: Response status: {}", status);

        // Handle 204 No Content - server has no suggestion
        if status.as_u16() == 204 {
            log::info!(
                "Cometix: Server returned 204 No Content - no completion suggestion available. \
                This is normal when: (1) input is too short, (2) cursor position doesn't need completion, \
                (3) server is rate limiting, or (4) model decided no suggestion is appropriate."
            );
            return Ok(StreamCppResponse::default());
        }

        // Read entire response body for LPM parsing
        let mut body_bytes = Vec::new();
        response.body_mut().read_to_end(&mut body_bytes).await?;

        log::debug!("Cometix: Response body size: {} bytes", body_bytes.len());

        if !status.is_success() {
            let error_text = String::from_utf8_lossy(&body_bytes);
            log::error!(
                "Cometix: Request failed with status {}: {}",
                status,
                error_text
            );
            anyhow::bail!("Request failed with status {}: {}", status, error_text);
        }

        // Log first few bytes for debugging
        if !body_bytes.is_empty() {
            let preview_len = body_bytes.len().min(200);
            log::info!(
                "Cometix: Response body size={}, preview (first {} bytes): {:?}",
                body_bytes.len(),
                preview_len,
                &body_bytes[..preview_len]
            );
            // Also log as text if possible
            let text_preview = String::from_utf8_lossy(&body_bytes[..preview_len]);
            log::info!("Cometix: Response as text: {}", text_preview);

            // Check if response is JSON (starts with '{')
            if body_bytes.first() == Some(&b'{') {
                let json_text = String::from_utf8_lossy(&body_bytes);
                log::info!("Cometix: Response appears to be JSON: {}", json_text);
                // Try to parse as JSON and extract text field
                if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&body_bytes) {
                    let text = json.get("text").and_then(|v| v.as_str()).unwrap_or("");
                    let binding_id = json
                        .get("bindingId")
                        .or_else(|| json.get("binding_id"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    let done_stream = json
                        .get("doneStream")
                        .or_else(|| json.get("done_stream"))
                        .and_then(|v| v.as_bool());

                    return Ok(StreamCppResponse {
                        text: text.to_string(),
                        binding_id,
                        done_stream,
                        ..Default::default()
                    });
                }
            }
        }

        // Parse Length-Prefixed Messages (gRPC-Web / Connect RPC format)
        Self::parse_streaming_response(&body_bytes)
    }

    /// Parses gRPC-Web Length-Prefixed Messages from response body.
    ///
    /// Format: [1-byte flag][4-byte big-endian length][payload]...
    /// Accumulates text from all messages until `done_stream` is true.
    fn parse_streaming_response(body: &[u8]) -> Result<StreamCppResponse> {
        const HEADER_SIZE: usize = 5;

        let mut accumulated_text = String::new();
        let mut final_binding_id: Option<String> = None;
        let mut final_range_to_replace = None;
        let mut final_cursor_prediction = None;
        let mut final_should_remove_leading_eol: Option<bool> = None;
        let mut offset = 0;
        let mut message_count = 0;

        log::info!(
            "Cometix: Starting to parse {} bytes of response",
            body.len()
        );

        while offset + HEADER_SIZE <= body.len() {
            // Parse 5-byte header: 1 byte flags + 4 bytes length
            let flags = body[offset];
            let length = u32::from_be_bytes([
                body[offset + 1],
                body[offset + 2],
                body[offset + 3],
                body[offset + 4],
            ]) as usize;

            message_count += 1;
            log::info!(
                "Cometix: Message #{}: flags={}, length={}, offset={}",
                message_count,
                flags,
                length,
                offset
            );

            offset += HEADER_SIZE;

            // Validate we have enough bytes for the payload
            if offset + length > body.len() {
                log::warn!(
                    "Cometix: Incomplete message - expected {} bytes, have {}",
                    length,
                    body.len() - offset
                );
                break;
            }

            // flags=2 indicates end-of-stream (trailers)
            if flags == 2 {
                let trailer_data = &body[offset..offset + length];
                let trailer_text = String::from_utf8_lossy(trailer_data);
                log::info!("Cometix: End-of-stream trailer: {}", trailer_text);
                offset += length;
                continue;
            }

            // Decode the protobuf message
            let payload = &body[offset..offset + length];
            log::info!(
                "Cometix: Payload bytes: {:?}",
                &payload[..payload.len().min(50)]
            );
            match StreamCppResponse::decode(payload) {
                Ok(msg) => {
                    // Safely truncate text for logging (respecting UTF-8 char boundaries)
                    let truncated_text = if msg.text.chars().count() > 50 {
                        format!("{}...", msg.text.chars().take(50).collect::<String>())
                    } else {
                        msg.text.clone()
                    };
                    log::info!(
                        "Cometix: Decoded message - text='{}', done_stream={:?}, binding_id={:?}",
                        truncated_text,
                        msg.done_stream,
                        msg.binding_id
                    );

                    // Accumulate completion text
                    accumulated_text.push_str(&msg.text);

                    // Capture binding_id (use the last non-empty one)
                    if msg.binding_id.is_some() {
                        final_binding_id = msg.binding_id;
                    }

                    // Capture range_to_replace if provided
                    if msg.range_to_replace.is_some() {
                        final_range_to_replace = msg.range_to_replace;
                    }

                    // Capture cursor prediction target
                    if msg.cursor_prediction_target.is_some() {
                        final_cursor_prediction = msg.cursor_prediction_target;
                    }

                    // Capture should_remove_leading_eol flag
                    if msg.should_remove_leading_eol.is_some() {
                        final_should_remove_leading_eol = msg.should_remove_leading_eol;
                    }

                    // Check for stream termination
                    if msg.done_stream.unwrap_or(false) {
                        log::info!("Cometix: Stream completed with done_stream flag");
                        break;
                    }
                }
                Err(e) => {
                    log::warn!("Cometix: Failed to decode stream message: {}", e);
                    // Continue to next message - partial failures are acceptable
                }
            }

            offset += length;
        }

        // Construct aggregated response
        Ok(StreamCppResponse {
            text: accumulated_text,
            binding_id: final_binding_id,
            range_to_replace: final_range_to_replace,
            cursor_prediction_target: final_cursor_prediction,
            should_remove_leading_eol: final_should_remove_leading_eol,
            done_stream: Some(true),
            ..Default::default()
        })
    }

    /// Records the fate of a completion (accepted, rejected, or partial).
    ///
    /// This telemetry helps improve the model's predictions.
    async fn record_fate(
        http_client: Arc<dyn HttpClient>,
        binding_id: String,
        fate: CppFate,
        auth_token: String,
        client_key: String,
        client_key_header: String,
        base_url: String,
        fate_path: &'static str,
    ) -> Result<()> {
        let url = format!("{}{}", base_url, fate_path);

        let request = RecordCppFateRequest {
            request_id: binding_id,
            performance_now_time: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as f32)
                .unwrap_or(0.0),
            fate: fate.into(),
            extension: "zed-cometix".to_string(),
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

        let response = http_client.send(http_request).await?;

        if response.status().is_success() {
            log::debug!("Cometix: Successfully recorded fate {:?}", fate);
        } else {
            log::warn!(
                "Cometix: Failed to record fate, status: {}",
                response.status()
            );
        }

        Ok(())
    }

    /// Sends fate recording asynchronously without blocking.
    fn send_fate(&self, binding_id: String, fate: CppFate, cx: &mut Context<Self>) {
        let settings = CtabSettings::get_global(cx);

        let Some(auth_token) = settings.auth_token.clone() else {
            return;
        };

        let base_url = settings.effective_base_url().to_string();
        let fate_path = settings.record_fate_path();
        let client_key_header = settings.client_key_header().to_string();

        let client_key = settings
            .client_key
            .clone()
            .unwrap_or_else(generate_client_key);

        let http_client = self.http_client.clone();

        // Fire and forget - don't block on telemetry
        cx.spawn(async move |_, _| {
            if let Err(e) = Self::record_fate(
                http_client,
                binding_id,
                fate,
                auth_token,
                client_key,
                client_key_header,
                base_url,
                fate_path,
            )
            .await
            {
                log::error!("Cometix: Failed to record fate: {}", e);
            }
        })
        .detach();
    }

    /// Sends a CppAppend request to synchronize incremental changes.
    ///
    /// This is used after partial accepts or significant edits to keep
    /// the server context in sync with the client state.
    async fn send_cpp_append(
        http_client: Arc<dyn HttpClient>,
        changes: Vec<u8>,
        auth_token: String,
        client_key: String,
        client_key_header: String,
        base_url: String,
        append_path: &'static str,
    ) -> Result<bool> {
        let url = format!("{}{}", base_url, append_path);

        log::debug!(
            "Cometix: Sending CppAppend request to {}, changes size: {} bytes",
            url,
            changes.len()
        );

        let request = CppAppendRequest { changes };
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

        if !status.is_success() {
            let mut body_bytes = Vec::new();
            response.body_mut().read_to_end(&mut body_bytes).await?;
            let error_text = String::from_utf8_lossy(&body_bytes);
            log::warn!(
                "Cometix: CppAppend failed with status {}: {}",
                status,
                error_text
            );
            return Ok(false);
        }

        log::debug!("Cometix: CppAppend completed successfully");
        Ok(true)
    }

    /// Triggers a CppAppend request in the background.
    ///
    /// This method is fire-and-forget - it spawns an async task and returns immediately.
    pub fn trigger_append(&self, changes: Vec<u8>, cx: &mut Context<Self>) {
        let settings = CtabSettings::get_global(cx);

        let Some(auth_token) = settings.auth_token.clone() else {
            log::debug!("Cometix: Skipping CppAppend - no auth token");
            return;
        };

        let base_url = settings.effective_base_url().to_string();
        let append_path = settings.cpp_append_path();
        let client_key_header = settings.client_key_header().to_string();

        let client_key = settings
            .client_key
            .clone()
            .unwrap_or_else(generate_client_key);

        let http_client = self.http_client.clone();

        log::debug!(
            "Cometix: Triggering CppAppend with {} bytes of changes",
            changes.len()
        );

        // Fire and forget
        cx.spawn(async move |_, _| {
            if let Err(e) = Self::send_cpp_append(
                http_client,
                changes,
                auth_token,
                client_key,
                client_key_header,
                base_url,
                append_path,
            )
            .await
            {
                log::error!("Cometix: Failed to send CppAppend: {}", e);
            }
        })
        .detach();
    }

    /// Builds the changes bytes from the accepted completion text and file path.
    ///
    /// The changes format follows the diff history format used by Cursor API:
    /// `{line_number}+|{content}\n` for added lines
    fn build_append_changes(&self, completion_text: &str, file_path: &str) -> Vec<u8> {
        let mut tracker = self.diff_tracker.lock();
        let diff = tracker.build_diff_history(file_path, completion_text);

        if diff.is_empty() {
            // If no diff, just send the raw text as UTF-8 bytes
            completion_text.as_bytes().to_vec()
        } else {
            diff.into_bytes()
        }
    }

    /// Fetches CppConfig from the server.
    async fn fetch_cpp_config(
        http_client: Arc<dyn HttpClient>,
        model: String,
        auth_token: String,
        client_key: String,
        client_key_header: String,
        base_url: String,
        config_path: &'static str,
    ) -> Result<CppConfigResponse> {
        let url = format!("{}{}", base_url, config_path);

        log::debug!("Cometix: Fetching CppConfig from {}", url);

        let request = CppConfigRequest {
            is_nightly: Some(false),
            model,
            supports_cpt: Some(true),
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
                "Cometix: CppConfig request failed with status {}: {}",
                status,
                error_text
            );
            anyhow::bail!("CppConfig request failed: {}", status);
        }

        let config = CppConfigResponse::decode(&body_bytes[..])?;
        log::info!(
            "Cometix: Received CppConfig - is_on={:?}, debounce={}ms, geo_url={:?}",
            config.is_on,
            config.client_debounce_duration_millis,
            config.geo_cpp_backend_url
        );

        // Phase 3: Log enhanced config fields if present
        if config.allows_tab_chunks.is_some() || config.tab_context_refresh_debounce_ms.is_some() {
            log::info!(
                "Cometix: Enhanced config - allows_tab_chunks={:?}, tab_refresh_debounce_ms={:?}, editor_change_debounce_ms={:?}",
                config.allows_tab_chunks,
                config.tab_context_refresh_debounce_ms,
                config.tab_context_refresh_editor_change_debounce_ms
            );
        }

        Ok(config)
    }

    /// Gets cached config or fetches new one if expired.
    ///
    /// This is a non-blocking operation that returns cached config if available.
    /// If the cache is expired, it returns the stale config while triggering
    /// a background refresh. Only returns None if no config has ever been fetched.
    fn get_or_refresh_config(&self, cx: &mut Context<Self>) -> Option<CppConfigResponse> {
        let cache = self.config_cache.lock();

        match cache.as_ref() {
            Some(cached) if cached.is_valid() => {
                // Cache is valid, return it directly
                Some(cached.config.clone())
            }
            Some(cached) => {
                // Cache is expired but exists - return stale value and refresh in background
                let stale_config = cached.config.clone();
                drop(cache); // Release lock before spawning
                self.refresh_config_async(cx);
                Some(stale_config)
            }
            None => {
                // No cache at all - trigger fetch and return None
                drop(cache); // Release lock before spawning
                self.refresh_config_async(cx);
                None
            }
        }
    }

    /// Triggers an async config refresh in the background.
    fn refresh_config_async(&self, cx: &mut Context<Self>) {
        let settings = CtabSettings::get_global(cx);

        let Some(auth_token) = settings.auth_token.clone() else {
            return;
        };

        let base_url = settings.effective_base_url().to_string();
        let config_path = settings.cpp_config_path();
        let client_key_header = settings.client_key_header().to_string();
        let model = settings.model.clone().unwrap_or_else(|| "auto".to_string());

        let client_key = settings
            .client_key
            .clone()
            .unwrap_or_else(generate_client_key);

        let http_client = self.http_client.clone();
        let config_cache = self.config_cache.clone();

        cx.spawn(async move |_, _| {
            match Self::fetch_cpp_config(
                http_client,
                model,
                auth_token,
                client_key,
                client_key_header,
                base_url,
                config_path,
            )
            .await
            {
                Ok(config) => {
                    let mut cache = config_cache.lock();
                    *cache = Some(CachedConfig {
                        config,
                        fetched_at: std::time::Instant::now(),
                    });
                    log::debug!("Cometix: Config cache updated");
                }
                Err(e) => {
                    log::error!("Cometix: Failed to fetch CppConfig: {}", e);
                }
            }
        })
        .detach();
    }

    /// Collects additional files from the project for context.
    ///
    /// This method gathers open files to provide multi-file context to the
    /// completion API, improving suggestion quality.
    ///
    /// Strategy:
    /// 1. Get all open buffers from the project
    /// 2. Filter out the current file and files that are too large
    /// 3. Prioritize files with the same language/extension as current file
    /// 4. Limit to MAX_ADDITIONAL_FILES
    ///
    /// Note: True MRU ordering would require tracking buffer access times
    /// at the Workspace/Pane level, which is not currently accessible here.
    /// We use a heuristic that prioritizes same-language files instead.
    /// Collects additional files and context items from the project for context.
    ///
    /// Returns a tuple of (additional_files, context_items):
    /// - additional_files: Used for session state tracking
    /// - context_items: Used for RAG-based code context (primary source for model)
    /// Phase 2: Enhanced context collection with relevance scoring
    ///
    /// Scoring factors:
    /// - Same language extension: +10.0
    /// - Same directory: +5.0
    /// - Parent/sibling directory: +2.0
    /// - File name referenced in current content: +50.0 (import/use detection)
    /// - Token overlap (Jaccard similarity): up to +20.0
    fn collect_additional_files(
        &self,
        current_file_path: &str,
        cx: &App,
    ) -> (Vec<AdditionalFile>, Vec<CppContextItem>) {
        let Some(project) = &self.project else {
            log::debug!("Cometix: No project available for additional files");
            return (vec![], vec![]);
        };

        let project = project.read(cx);

        // Get current file info for scoring
        let current_ext = current_file_path
            .rsplit('.')
            .next()
            .map(|s| s.to_lowercase());

        let current_dir = std::path::Path::new(current_file_path)
            .parent()
            .map(|p| p.to_string_lossy().to_string());

        let current_parent_dir = std::path::Path::new(current_file_path)
            .parent()
            .and_then(|p| p.parent())
            .map(|p| p.to_string_lossy().to_string());

        // Get current file content for reference detection (we'll fetch it from buffer)
        let current_content = project
            .opened_buffers(cx)
            .into_iter()
            .find(|b| {
                b.read(cx)
                    .file()
                    .map(|f| f.path().as_unix_str().to_string() == current_file_path)
                    .unwrap_or(false)
            })
            .map(|b| b.read(cx).text())
            .unwrap_or_default();

        // Extract tokens from current content for Jaccard similarity
        let current_tokens: std::collections::HashSet<&str> = current_content
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|s| s.len() > 2) // Filter out very short tokens
            .collect();

        // Collect scored files
        struct ScoredFile {
            additional_file: AdditionalFile,
            context_item: CppContextItem,
            score: f32,
        }

        let mut scored_files: Vec<ScoredFile> = Vec::new();

        let now_timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);

        for buffer in project.opened_buffers(cx) {
            let buffer = buffer.read(cx);

            // Get file path
            let Some(file) = buffer.file() else {
                continue;
            };

            let file_path = file.path().as_unix_str().to_string();

            // Skip the current file
            if file_path == current_file_path {
                continue;
            }

            // Get buffer content
            let content = buffer.text();

            // Skip empty or too large files
            if content.is_empty() || content.len() > MAX_ADDITIONAL_FILE_SIZE {
                log::debug!(
                    "Cometix: Skipping file {} (empty={}, size={})",
                    file_path,
                    content.is_empty(),
                    content.len()
                );
                continue;
            }

            // Calculate relevance score
            let mut score: f32 = 0.0;

            // 1. Same language extension: +10.0
            let file_ext = file_path.rsplit('.').next().map(|s| s.to_lowercase());
            if file_ext == current_ext {
                score += 10.0;
            }

            // 2. Directory proximity
            let file_dir = std::path::Path::new(&file_path)
                .parent()
                .map(|p| p.to_string_lossy().to_string());
            let file_parent_dir = std::path::Path::new(&file_path)
                .parent()
                .and_then(|p| p.parent())
                .map(|p| p.to_string_lossy().to_string());

            if file_dir == current_dir {
                // Same directory: +5.0
                score += 5.0;
            } else if file_parent_dir == current_parent_dir || file_dir == current_parent_dir {
                // Sibling or parent directory: +2.0
                score += 2.0;
            }

            // 3. File name referenced in current content: +50.0
            // Check if the file stem (e.g., "utils" from "utils.rs") appears in current content
            let file_stem = std::path::Path::new(&file_path)
                .file_stem()
                .and_then(|s| s.to_str());
            if let Some(stem) = file_stem {
                if stem.len() > 2 && current_content.contains(stem) {
                    score += 50.0;
                    log::debug!(
                        "Cometix: File {} referenced in current content (+50)",
                        file_path
                    );
                }
            }

            // 4. Token overlap (Jaccard similarity): up to +20.0
            // Only compute for reasonably-sized files to avoid performance issues
            if content.len() < 15_000 && !current_tokens.is_empty() {
                let file_tokens: std::collections::HashSet<&str> = content
                    .split(|c: char| !c.is_alphanumeric() && c != '_')
                    .filter(|s| s.len() > 2)
                    .collect();

                if !file_tokens.is_empty() {
                    let intersection = current_tokens.intersection(&file_tokens).count();
                    let union = current_tokens.len() + file_tokens.len() - intersection;
                    if union > 0 {
                        let jaccard = intersection as f32 / union as f32;
                        score += jaccard * 20.0;
                    }
                }
            }

            // Build visible range content (full file for now)
            let lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
            let total_lines = lines.len() as i32;

            let additional_file = AdditionalFile {
                relative_workspace_path: file_path.clone(),
                is_open: true,
                visible_range_content: lines,
                last_viewed_at: Some(now_timestamp),
                start_line_number_one_indexed: vec![1],
                visible_ranges: vec![crate::proto::LineRange {
                    start_line_number: 1,
                    end_line_number_inclusive: total_lines,
                }],
            };

            // Build context item for RAG context with computed score
            let context_item = CppContextItem {
                contents: content,
                symbol: None,
                relative_workspace_path: file_path.clone(),
                score,
            };

            scored_files.push(ScoredFile {
                additional_file,
                context_item,
                score,
            });
        }

        // Sort by score descending
        scored_files.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Limit to MAX_ADDITIONAL_FILES
        scored_files.truncate(MAX_ADDITIONAL_FILES);

        log::info!(
            "Cometix: Collected {} additional files (scores: {})",
            scored_files.len(),
            scored_files
                .iter()
                .map(|f| format!("{:.1}", f.score))
                .collect::<Vec<_>>()
                .join(", ")
        );

        // Split into separate vectors
        let (additional_files, context_items): (Vec<_>, Vec<_>) = scored_files
            .into_iter()
            .map(|sf| (sf.additional_file, sf.context_item))
            .unzip();

        (additional_files, context_items)
    }
}

impl EditPredictionProvider for CtabCompletionProvider {
    fn name() -> &'static str {
        "cometix"
    }

    fn display_name() -> &'static str {
        "Cometix"
    }

    fn show_completions_in_menu() -> bool {
        true
    }

    fn show_tab_accept_marker() -> bool {
        true
    }

    fn supports_jump_to_edit() -> bool {
        false
    }

    fn is_enabled(&self, _buffer: &Entity<Buffer>, _cursor_position: Anchor, cx: &App) -> bool {
        let settings = CtabSettings::get_global(cx);
        settings.enabled && settings.auth_token.is_some()
    }

    fn is_refreshing(&self, _cx: &App) -> bool {
        self.pending_refresh.is_some()
    }

    fn refresh(
        &mut self,
        buffer: Entity<Buffer>,
        cursor_position: Anchor,
        debounce: bool,
        cx: &mut Context<Self>,
    ) {
        // Cancel any pending refresh
        self.pending_refresh = None;
        self.current_completion = None;

        // Check if we should skip this refresh (e.g., previous completion indicated no retrigger)
        if self.skip_next_refresh {
            log::debug!("Cometix: Skipping refresh (requested by previous completion)");
            self.skip_next_refresh = false;
            return;
        }

        // Ensure we have a stable workspace_id based on project path
        self.ensure_stable_workspace_id(cx);

        let settings = CtabSettings::get_global(cx);

        // Check if enabled and has auth token
        if !settings.enabled {
            log::debug!("Cometix: Disabled in settings");
            return;
        }

        let Some(auth_token) = settings.auth_token.clone() else {
            log::warn!("Cometix: No auth token configured");
            return;
        };

        let base_url = settings.effective_base_url().to_string();
        let stream_path = settings.stream_cpp_path();
        let client_key_header = settings.client_key_header().to_string();
        let uses_connect_rpc = settings.uses_connect_rpc();
        let default_debounce_ms = settings.debounce_ms;
        let endpoint_type = settings.endpoint_type;
        let model_name = settings.model.clone().unwrap_or_else(|| "auto".to_string());
        let client_key = settings
            .client_key
            .clone()
            .unwrap_or_else(generate_client_key);
        let has_auth_token = settings.auth_token.is_some();
        let fs_upload_path = settings.fs_upload_path();

        // Release immutable borrow before using cx mutably again
        drop(settings);

        // Use server config debounce if available, otherwise use local setting
        let server_config = self.get_or_refresh_config(cx);
        let debounce_ms = server_config
            .as_ref()
            .map(|c| c.client_debounce_duration_millis as u64)
            .filter(|&d| d > 0)
            .unwrap_or(default_debounce_ms);

        log::info!(
            "Cometix: Using endpoint_type={:?}, base_url={}, stream_path={}, uses_connect_rpc={}, has_auth_token={}",
            endpoint_type,
            base_url,
            stream_path,
            uses_connect_rpc,
            has_auth_token
        );

        // Get buffer info
        let snapshot = buffer.read(cx).snapshot();
        let buffer_id = buffer.entity_id();
        self.buffer_id = Some(buffer_id);

        let content = snapshot.text();
        let offset = cursor_position.to_offset(&snapshot);
        let point = snapshot.offset_to_point(offset);

        let file_path = snapshot
            .file()
            .map(|f| f.path().as_unix_str().to_string())
            .unwrap_or_else(|| "untitled".to_string());

        self.file_extension = file_path.rsplit('.').next().map(|s| s.to_string());
        self.current_file_path = Some(file_path.clone());

        // Collect additional files and context items for multi-file context
        let (additional_files, context_items) = self.collect_additional_files(&file_path, cx);

        // Build diff history
        let diff_history = {
            let mut tracker = self.diff_tracker.lock();
            tracker.build_diff_history(&file_path, &content)
        };

        let file_version = {
            let tracker = self.diff_tracker.lock();
            tracker.get_file_version(&file_path)
        };

        // Check file sync status and prepare filesync updates
        let content_hash = Self::compute_sha256(&content);
        let file_sync_manager = self.file_sync_manager.clone();
        let needs_upload = file_sync_manager.needs_upload(&file_path);
        let model_version = file_sync_manager.get_model_version(&file_path);

        // If file needs initial upload, trigger it asynchronously
        if needs_upload {
            let upload_http_client = self.http_client.clone();
            let upload_file_path = file_path.clone();
            let upload_content = content.clone();
            let upload_auth_token = auth_token.clone();
            let upload_client_key = client_key.clone();
            let upload_client_key_header = client_key_header.clone();
            let upload_filesync_client_key = self.filesync_client_key.clone();
            let upload_filesync_cookie = self.filesync_cookie.clone();
            let upload_base_url = base_url.clone();
            let upload_path = fs_upload_path;
            let upload_uuid = self.workspace_id.clone();
            let upload_hash = content_hash.clone();
            let upload_sync_manager = file_sync_manager.clone();

            cx.spawn(async move |_, _| {
                log::info!("Cometix: Uploading file {} for sync", upload_file_path);
                match FileSyncManager::upload_file(
                    upload_http_client,
                    upload_uuid,
                    upload_file_path.clone(),
                    upload_content,
                    1, // Initial model version
                    upload_auth_token,
                    upload_client_key,
                    upload_client_key_header,
                    upload_filesync_client_key,
                    upload_filesync_cookie,
                    upload_base_url,
                    upload_path,
                )
                .await
                {
                    Ok(FsUploadErrorType::Unspecified) => {
                        log::info!("Cometix: File {} uploaded successfully", upload_file_path);
                        upload_sync_manager.mark_uploaded(&upload_file_path, upload_hash);
                    }
                    Ok(error) => {
                        log::warn!(
                            "Cometix: File {} upload returned error: {:?}",
                            upload_file_path,
                            error
                        );
                    }
                    Err(e) => {
                        log::error!("Cometix: Failed to upload file {}: {}", upload_file_path, e);
                    }
                }
            })
            .detach();
        }

        // Collect pending filesync updates and convert to the correct type
        let pending_updates = file_sync_manager.take_pending_updates(&file_path);
        let filesync_updates: Vec<FilesyncUpdateWithModelVersion> = if pending_updates.is_empty() {
            vec![]
        } else {
            vec![FilesyncUpdateWithModelVersion {
                model_version,
                relative_workspace_path: file_path.clone(),
                updates: pending_updates,
                expected_file_length: content.len() as i32,
            }]
        };

        // Force rely_on_filesync to false.
        // Since we haven't hooked edit events to call record_change,
        // the incremental update list is always empty. If we don't force this to false,
        // the client will incorrectly tell the server to use a stale cached version,
        // causing cursor position mismatch or context loss, resulting in 204 responses.
        let rely_on_filesync = false;

        log::info!(
            "Cometix: File sync status - needs_upload={}, rely_on_filesync={}, pending_updates={}",
            needs_upload,
            rely_on_filesync,
            filesync_updates.len()
        );

        // Phase 1: Collect diagnostics from buffer
        // Only collect Error and Warning severity, limit to 20 entries to save tokens
        let diagnostics: Vec<ProtoDiagnostic> = snapshot
            .diagnostics_in_range::<_, Point>(0..snapshot.len(), false)
            .filter(|entry| {
                matches!(
                    entry.diagnostic.severity,
                    DiagnosticSeverity::ERROR | DiagnosticSeverity::WARNING
                )
            })
            .take(20)
            .map(|entry| {
                let severity = match entry.diagnostic.severity {
                    DiagnosticSeverity::ERROR => ProtoDiagnosticSeverity::Error,
                    DiagnosticSeverity::WARNING => ProtoDiagnosticSeverity::Warning,
                    DiagnosticSeverity::INFORMATION => ProtoDiagnosticSeverity::Information,
                    DiagnosticSeverity::HINT => ProtoDiagnosticSeverity::Hint,
                    _ => ProtoDiagnosticSeverity::Unspecified,
                };

                ProtoDiagnostic {
                    message: entry.diagnostic.message.clone(),
                    range: Some(CursorRange {
                        start_position: Some(CursorPosition {
                            line: entry.range.start.row as i32,
                            column: entry.range.start.column as i32,
                        }),
                        end_position: Some(CursorPosition {
                            line: entry.range.end.row as i32,
                            column: entry.range.end.column as i32,
                        }),
                    }),
                    severity: severity.into(),
                    related_information: vec![],
                }
            })
            .collect();

        if !diagnostics.is_empty() {
            log::info!(
                "Cometix: Collected {} diagnostics for {}",
                diagnostics.len(),
                file_path
            );
        }

        // Build the request
        log::info!(
            "Cometix: Building request - file={}, cursor=({},{}), content_len={}, language={}, diagnostics={}",
            file_path,
            point.row,
            point.column,
            content.len(),
            Self::detect_language(&file_path),
            diagnostics.len()
        );

        // Log content preview for debugging
        let content_preview: String = content.chars().take(200).collect();
        log::info!(
            "Cometix: Content preview: {}",
            content_preview.replace('\n', "\\n")
        );

        let request = StreamCppRequest {
            current_file: Some(CurrentFileInfo {
                relative_workspace_path: file_path.clone(),
                contents: content.clone(),
                rely_on_filesync,
                sha_256_hash: Some(content_hash),
                cursor_position: Some(CursorPosition {
                    line: point.row as i32,
                    column: point.column as i32,
                }),
                total_number_of_lines: content.lines().count() as i32,
                language_id: Self::detect_language(&file_path),
                file_version: Some(file_version),
                workspace_root_path: String::new(),
                line_ending: Some("\n".to_string()),
                // Phase 1: Include diagnostics for AI-assisted error fixing
                diagnostics,
            }),
            // Note: diff_history field is deprecated, use file_diff_histories instead
            diff_history: vec![],
            model_name: Some(model_name),
            // Build proper CppFileDiffHistory structure with file name
            file_diff_histories: if diff_history.is_empty() {
                vec![]
            } else {
                let file_name = file_path
                    .rsplit('/')
                    .next()
                    .or_else(|| file_path.rsplit('\\').next())
                    .unwrap_or(&file_path)
                    .to_string();
                vec![CppFileDiffHistory {
                    file_name,
                    diff_history: vec![diff_history],
                    diff_history_timestamps: vec![
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs_f64())
                            .unwrap_or(0.0),
                    ],
                }]
            },
            immediately_ack: Some(false),
            enable_more_context: Some(true),
            cpp_intent_info: Some(CppIntentInfo {
                source: "typing".to_string(),
            }),
            workspace_id: Some(self.workspace_id.clone()),
            additional_files,
            context_items,
            control_token: None,
            client_time: Some(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0),
            ),
            filesync_updates,
            time_since_request_start: 0.0,
            time_at_request_send: 0.0,
            client_timezone_offset: None,
            supports_cpt: Some(true),
            supports_crlf_cpt: Some(false),
        };

        let http_client = self.http_client.clone();
        let filesync_client_key = self.filesync_client_key.clone();
        let filesync_cookie = self.filesync_cookie.clone();

        self.pending_refresh = Some(cx.spawn(async move |this, cx| {
            if debounce {
                gpui::Timer::after(Duration::from_millis(debounce_ms)).await;
            }

            let response = Self::fetch_completion(
                http_client,
                request,
                auth_token,
                client_key,
                client_key_header,
                filesync_client_key,
                filesync_cookie,
                base_url,
                stream_path,
                uses_connect_rpc,
            )
            .await;

            this.update(cx, |this, cx| {
                this.pending_refresh = None;

                match response {
                    Ok(response) => {
                        if !response.text.is_empty() {
                            // DEBUG: Log raw API response with escape sequences visible
                            log::info!(
                                "Cometix: [DEBUG] Raw API completion (len={}): {:?}",
                                response.text.len(),
                                response.text
                            );
                            log::info!(
                                "Cometix: [DEBUG] range_to_replace={:?}, should_remove_leading_eol={:?}",
                                response.range_to_replace,
                                response.should_remove_leading_eol
                            );

                            // Determine range based on API response
                            let (range_start, range_end, api_range) = if let Some(ref range) =
                                response.range_to_replace
                            {
                                // Convert 1-based line numbers to 0-based
                                let start_line = (range.start_line_number.max(1) - 1) as u32;
                                let end_line = (range.end_line_number_inclusive.max(1) - 1) as u32;

                                log::debug!(
                                    "Cometix: Range replacement: lines {}-{} (0-based: {}-{})",
                                    range.start_line_number,
                                    range.end_line_number_inclusive,
                                    start_line,
                                    end_line
                                );

                                // Create anchors for the range
                                // Note: This requires buffer access which we don't have here
                                // For now, fall back to cursor position
                                // TODO: Properly convert line range to anchors
                                (
                                    cursor_position,
                                    cursor_position,
                                    Some((
                                        range.start_line_number,
                                        range.end_line_number_inclusive,
                                    )),
                                )
                            } else {
                                (cursor_position, cursor_position, None)
                            };

                            // Log cursor prediction if available
                            if let Some(ref prediction) = response.cursor_prediction_target {
                                log::debug!(
                                    "Cometix: Cursor prediction: line {} (0-based: {})",
                                    prediction.line_number_one_indexed,
                                    prediction.line_number_one_indexed - 1
                                );
                            }

                            // Process completion text - handle leading EOL removal
                            let mut completion_text = response.text;
                            if response.should_remove_leading_eol.unwrap_or(false) {
                                log::debug!(
                                    "Cometix: Removing leading EOL from completion (should_remove_leading_eol=true)"
                                );
                                completion_text = completion_text
                                    .strip_prefix('\n')
                                    .or_else(|| completion_text.strip_prefix("\r\n"))
                                    .map(|s| s.to_string())
                                    .unwrap_or(completion_text);
                            }

                            // Determine if we should retrigger completion after this one
                            let should_retrigger = response
                                .cursor_prediction_target
                                .as_ref()
                                .map(|t| t.should_retrigger_cpp)
                                .unwrap_or(true); // Default to true to maintain existing behavior

                            // Store the completion
                            this.current_completion = Some(CompletionState {
                                text: completion_text,
                                binding_id: response.binding_id,
                                range_start,
                                range_end,
                                api_range,
                                should_retrigger,
                            });

                            cx.notify();
                        }
                    }
                    Err(e) => {
                        log::error!("Cometix: Failed to fetch completion: {}", e);
                    }
                }
            })?;

            Ok(())
        }));
    }

    fn cycle(
        &mut self,
        _buffer: Entity<Buffer>,
        _cursor_position: Anchor,
        _direction: Direction,
        _cx: &mut Context<Self>,
    ) {
        // Cometix doesn't support cycling through completions
    }

    fn accept(&mut self, cx: &mut Context<Self>) {
        if let Some(completion) = self.current_completion.take() {
            // Check if we should skip the next refresh (prevents completion loop)
            if !completion.should_retrigger {
                log::debug!("Cometix: Disabling next refresh (should_retrigger=false)");
                self.skip_next_refresh = true;
            }

            // Send fate recording
            if let Some(binding_id) = completion.binding_id {
                self.send_fate(binding_id, CppFate::Accept, cx);
            }

            // Trigger CppAppend to sync the accepted completion with the server
            if !completion.text.is_empty() {
                let file_path = self
                    .current_file_path
                    .clone()
                    .unwrap_or_else(|| "untitled".to_string());
                let changes = self.build_append_changes(&completion.text, &file_path);
                self.trigger_append(changes, cx);
            }
        }
    }

    fn discard(&mut self, cx: &mut Context<Self>) {
        if let Some(completion) = self.current_completion.take() {
            if let Some(binding_id) = completion.binding_id {
                self.send_fate(binding_id, CppFate::Reject, cx);
            }
        }
    }

    fn suggest(
        &mut self,
        buffer: &Entity<Buffer>,
        cursor_position: Anchor,
        cx: &mut Context<Self>,
    ) -> Option<EditPrediction> {
        let completion = self.current_completion.as_ref()?;

        // Verify buffer matches
        if self.buffer_id != Some(buffer.entity_id()) {
            return None;
        }

        let buffer_snapshot = buffer.read(cx);
        let cursor_offset = cursor_position.to_offset(buffer_snapshot);
        let cursor_point = buffer_snapshot.offset_to_point(cursor_offset);

        let completion_text = completion.text.as_str();
        if completion_text.is_empty() {
            return None;
        }

        // Build completion context for smart differ
        // When API provides a range, we need to include content from that range
        // to properly detect overlaps
        let context = if let Some((start_line_1, end_line_1)) = completion.api_range {
            // API range is 1-indexed, convert to 0-indexed
            let start_line_0 = (start_line_1.max(1) - 1) as u32;
            let end_line_0 = (end_line_1.max(1) - 1) as u32;

            // Build context that includes the range content
            self.build_completion_context_with_range(
                buffer_snapshot,
                cursor_point,
                cursor_offset,
                start_line_0,
                end_line_0,
            )
        } else {
            self.build_completion_context(buffer_snapshot, cursor_point, cursor_offset)
        };

        // DEBUG: Log context information
        log::info!(
            "Cometix: [DEBUG] suggest() context - cursor=({},{}), before_cursor={:?}, after_cursor={:?}",
            cursor_point.row,
            cursor_point.column,
            &context.before_cursor,
            &context.after_cursor
        );
        log::info!(
            "Cometix: [DEBUG] suggest() completion_text={:?}, api_range={:?}",
            completion_text,
            completion.api_range
        );

        // Use SmartCompletionDiffer to process the completion
        let differ = SmartCompletionDiffer::new();
        let diff_result =
            differ.extract_completion_diff(&context, completion_text, completion.api_range);

        // DEBUG: Log differ result
        log::info!(
            "Cometix: [DEBUG] SmartDiffer result - confidence={:.3}, method={:?}, optimizations={:?}",
            diff_result.confidence,
            diff_result.method,
            diff_result.optimizations
        );
        log::info!(
            "Cometix: [DEBUG] SmartDiffer insert_text={:?}",
            diff_result.insert_text
        );

        let insert_text = &diff_result.insert_text;
        if insert_text.is_empty() {
            log::debug!("Cometix: SmartDiffer returned empty insert_text, skipping");
            return None;
        }

        // For inline completion (ghost text), we use Supermaven's approach:
        // Insert at cursor position with position..position range, combined with
        // a delete_range that covers cursor to end of line for proper diff rendering.
        //
        // This is different from block replacement (Alt+L) which replaces entire lines.
        let insert_text = insert_text.trim_end();
        if insert_text.trim().is_empty() {
            return None;
        }

        // Use completion_from_diff approach like Supermaven for proper inline rendering
        let end_of_line = buffer_snapshot.anchor_after(language::Point::new(
            cursor_point.row,
            buffer_snapshot.line_len(cursor_point.row),
        ));
        let delete_range = cursor_position..end_of_line;

        log::info!(
            "Cometix: [DEBUG] Creating inline completion - cursor_offset={}, insert_len={}",
            cursor_offset,
            insert_text.len()
        );

        // Generate edits using diff-based approach for proper ghost text rendering
        let edits = self.completion_from_diff(
            buffer_snapshot,
            insert_text,
            cursor_position,
            delete_range.clone(),
        );

        // DEBUG: Log the generated edits
        log::info!(
            "Cometix: [DEBUG] completion_from_diff generated {} edits",
            edits.len()
        );
        for (idx, (range, text)) in edits.iter().enumerate() {
            log::info!(
                "Cometix: [DEBUG] edit[{}]: range={}..{}, text={:?}",
                idx,
                range.start.to_offset(buffer_snapshot),
                range.end.to_offset(buffer_snapshot),
                text.as_ref()
            );
        }

        Some(EditPrediction::Local {
            id: completion
                .binding_id
                .as_ref()
                .map(|id| SharedString::from(id.clone())),
            edits,
            edit_preview: None,
        })
    }
}

impl CtabCompletionProvider {
    /// Generate edits from completion text using diff-based approach.
    /// This matches the buffer text against completion text to create proper inlays.
    /// Ported from Supermaven's completion_from_diff function.
    fn completion_from_diff(
        &self,
        snapshot: &language::Buffer,
        completion_text: &str,
        position: Anchor,
        delete_range: std::ops::Range<Anchor>,
    ) -> Vec<(std::ops::Range<Anchor>, Arc<str>)> {
        let buffer_text: String = snapshot
            .text_for_range(
                delete_range.start.to_offset(snapshot)..delete_range.end.to_offset(snapshot),
            )
            .collect();

        // DEBUG: Log inputs to completion_from_diff
        log::info!(
            "Cometix: [DEBUG] completion_from_diff - completion_text={:?}, buffer_text={:?}",
            completion_text,
            buffer_text
        );

        let mut edits: Vec<(std::ops::Range<Anchor>, Arc<str>)> = Vec::new();

        let completion_graphemes: Vec<&str> = completion_text.graphemes(true).collect();
        let buffer_graphemes: Vec<&str> = buffer_text.graphemes(true).collect();

        let mut offset = position.to_offset(snapshot);

        let mut i = 0;
        let mut j = 0;
        while i < completion_graphemes.len() && j < buffer_graphemes.len() {
            // Find the next instance of the buffer text in the completion text
            let k = completion_graphemes[i..]
                .iter()
                .position(|c| *c == buffer_graphemes[j]);
            match k {
                Some(k) => {
                    if k != 0 {
                        let anchor = snapshot.anchor_after(offset);
                        // The range from current position to item is an inlay
                        let edit = (
                            anchor..anchor,
                            Arc::from(completion_graphemes[i..i + k].join("")),
                        );
                        edits.push(edit);
                    }
                    i += k + 1;
                    j += 1;
                    offset += buffer_graphemes[j - 1].len();
                }
                None => {
                    // No more matching completions, drop remaining as inlay
                    break;
                }
            }
        }

        if j == buffer_graphemes.len() && i < completion_graphemes.len() {
            let anchor = snapshot.anchor_after(offset);
            // Leftover completion text becomes an inlay
            let edit_range = anchor..anchor;
            let edit_text = completion_graphemes[i..].join("");
            edits.push((edit_range, Arc::from(edit_text)));
        }

        edits
    }
    /// Build completion context for SmartCompletionDiffer
    fn build_completion_context(
        &self,
        buffer: &language::Buffer,
        cursor_point: Point,
        cursor_offset: usize,
    ) -> CompletionContext {
        // Get text before cursor (current line up to cursor)
        let line_start_offset = buffer.point_to_offset(Point::new(cursor_point.row, 0));
        let before_cursor: String = buffer
            .chars_for_range(line_start_offset..cursor_offset)
            .collect();

        // Get text after cursor (from cursor to end of current line, plus some following lines)
        let line_end_offset = buffer.point_to_offset(Point::new(
            cursor_point.row,
            buffer.line_len(cursor_point.row),
        ));

        // Include up to 10 lines after cursor for overlap detection
        let max_line = buffer.max_point().row;
        let context_end_line = (cursor_point.row + 10).min(max_line);
        let context_end_offset = if context_end_line > cursor_point.row {
            buffer.point_to_offset(Point::new(
                context_end_line,
                buffer.line_len(context_end_line),
            ))
        } else {
            line_end_offset
        };

        let after_cursor: String = buffer
            .chars_for_range(cursor_offset..context_end_offset)
            .collect();

        // Get current line text
        let current_line: String = buffer
            .chars_for_range(line_start_offset..line_end_offset)
            .collect();

        // Calculate indentation
        let indentation: String = current_line
            .chars()
            .take_while(|c| c.is_whitespace())
            .collect();

        // Get language
        let language = buffer
            .language()
            .map(|l| l.name().to_string())
            .unwrap_or_else(|| "unknown".to_string());

        CompletionContext {
            before_cursor,
            after_cursor,
            current_line,
            language,
            indentation,
            cursor_row: cursor_point.row,
            cursor_col: cursor_point.column,
        }
    }

    /// Build completion context with API-specified range for proper overlap detection.
    ///
    /// When API returns a `range_to_replace`, the completion text is meant to replace
    /// that entire range. We need to include content from lines BEFORE the cursor
    /// (within that range) in `before_cursor` so the differ can detect overlaps.
    ///
    /// Example scenario:
    /// - Line 42: `newTD()`  <- cursor is at end of this line or start of next
    /// - Line 43: (empty or next statement)
    /// - API returns: `range_to_replace=(43,43)`, `text="newTD();"`
    /// - Without range context: before_cursor="" (cursor at line start), no overlap detected
    /// - With range context: before_cursor includes line 42 content, overlap detected
    fn build_completion_context_with_range(
        &self,
        buffer: &language::Buffer,
        cursor_point: Point,
        cursor_offset: usize,
        range_start_line: u32,
        range_end_line: u32,
    ) -> CompletionContext {
        let max_line = buffer.max_point().row;

        // Include a few lines before the range start for better context
        let context_start_line = range_start_line.saturating_sub(3);
        let context_start_offset = buffer.point_to_offset(Point::new(context_start_line, 0));

        // Get text before cursor, including previous lines within context
        // This captures content that might overlap with the completion
        let before_cursor: String = buffer
            .chars_for_range(context_start_offset..cursor_offset)
            .collect();

        // Get text after cursor, including lines up to and beyond the range end
        let context_end_line = (range_end_line + 5).min(max_line);
        let context_end_offset = buffer.point_to_offset(Point::new(
            context_end_line,
            buffer.line_len(context_end_line),
        ));

        let after_cursor: String = buffer
            .chars_for_range(cursor_offset..context_end_offset)
            .collect();

        // Get current line text
        let line_start_offset = buffer.point_to_offset(Point::new(cursor_point.row, 0));
        let line_end_offset = buffer.point_to_offset(Point::new(
            cursor_point.row,
            buffer.line_len(cursor_point.row),
        ));
        let current_line: String = buffer
            .chars_for_range(line_start_offset..line_end_offset)
            .collect();

        // Calculate indentation
        let indentation: String = current_line
            .chars()
            .take_while(|c| c.is_whitespace())
            .collect();

        // Get language
        let language = buffer
            .language()
            .map(|l| l.name().to_string())
            .unwrap_or_else(|| "unknown".to_string());

        CompletionContext {
            before_cursor,
            after_cursor,
            current_line,
            language,
            indentation,
            cursor_row: cursor_point.row,
            cursor_col: cursor_point.column,
        }
    }
}

/// Calculates the length in bytes of the common prefix between two character iterators.
fn common_prefix<T1: Iterator<Item = char>, T2: Iterator<Item = char>>(a: T1, b: T2) -> usize {
    a.zip(b)
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a.len_utf8())
        .sum()
}

/// Generate a workspace ID for the session
/// Generate a stable workspace ID based on workspace path
///
/// If no path is provided, falls back to a random ID.
/// The format follows Cursor's convention: "a-b-c-d-e-f-g" (7 lowercase letters separated by dashes)
fn generate_workspace_id() -> String {
    use rand::Rng;
    let mut rng = rand::rng();
    (0..7)
        .map(|_| (b'a' + rng.random_range(0..26)) as char)
        .collect::<Vec<_>>()
        .into_iter()
        .collect()
}

/// Generate a stable workspace ID from a workspace path
///
/// Uses SHA256 hash of the path to generate a deterministic ID.
/// This ensures the same workspace always gets the same ID, which is
/// important for file sync state consistency with the server.
fn generate_stable_workspace_id(workspace_path: &str) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(workspace_path.as_bytes());
    let hash = hex::encode(hasher.finalize());

    // Convert hash to 7 lowercase letters (a-z) separated by dashes
    // Take pairs of hex digits and convert to letters
    let letters: Vec<char> = hash
        .as_bytes()
        .chunks(2)
        .take(7)
        .map(|chunk| {
            let hex_str = std::str::from_utf8(chunk).unwrap_or("00");
            let val = u8::from_str_radix(hex_str, 16).unwrap_or(0);
            (b'a' + (val % 26)) as char
        })
        .collect();

    letters
        .iter()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join("-")
}

/// Generate a cursor checksum for x-cursor-checksum header validation
///
/// Format: timestamp_header(8) + device_hash(64) + '/' + mac_hash(64) = 137 chars
/// - timestamp_header: 6-byte obfuscated timestamp, Base64 encoded to 8 chars
/// - device_hash: 32-byte random value, hex encoded to 64 chars
/// - mac_hash: 32-byte random value, hex encoded to 64 chars
fn generate_client_key() -> String {
    use rand::Rng;
    let mut rng = rand::rng();

    // 1. Generate timestamp header (8 chars)
    let timestamp_header = generate_timestamp_header();

    // 2. Generate device hash (64 hex chars)
    let device_bytes: Vec<u8> = (0..32).map(|_| rng.random()).collect();
    let device_hash = hex::encode(device_bytes);

    // 3. Generate MAC hash (64 hex chars)
    let mac_bytes: Vec<u8> = (0..32).map(|_| rng.random()).collect();
    let mac_hash = hex::encode(mac_bytes);

    format!("{}{}/{}", timestamp_header, device_hash, mac_hash)
}

/// Generate an obfuscated timestamp header (8 chars)
///
/// Uses a custom obfuscation algorithm matching Cursor's implementation.
fn generate_timestamp_header() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // Build 6-byte array from timestamp
    let mut bytes = vec![0u8; 6];
    bytes[0] = ((now >> 8) & 0xFF) as u8;
    bytes[1] = (now & 0xFF) as u8;
    bytes[2] = ((now >> 24) & 0xFF) as u8;
    bytes[3] = ((now >> 16) & 0xFF) as u8;
    bytes[4] = ((now >> 8) & 0xFF) as u8;
    bytes[5] = (now & 0xFF) as u8;

    // Obfuscation algorithm
    let mut prev: u8 = 165;
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = ((*byte ^ prev).wrapping_add(i as u8)) & 0xFF;
        prev = *byte;
    }

    encode_base64_url_safe(&bytes)
}

/// URL-safe Base64 encoding without padding
fn encode_base64_url_safe(input: &[u8]) -> String {
    const B64_CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

    let mut result = String::new();
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = if chunk.len() > 1 { chunk[1] } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] } else { 0 };

        result.push(B64_CHARS[(b0 >> 2) as usize] as char);
        result.push(B64_CHARS[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            result.push(B64_CHARS[(((b1 & 0x0F) << 2) | (b2 >> 6)) as usize] as char);
        }
        if chunk.len() > 2 {
            result.push(B64_CHARS[(b2 & 0x3F) as usize] as char);
        }
    }
    result
}

/// Generate a FileSync client key (64-char hex)
///
/// Used for x-client-key and x-fs-client-key headers in FileSync requests.
fn generate_filesync_client_key() -> String {
    use rand::Rng;
    let mut rng = rand::rng();
    let random_bytes: Vec<u8> = (0..32).map(|_| rng.random()).collect();
    hex::encode(random_bytes)
}

/// Generate a FileSync cookie value (32-char hex)
///
/// Used for FilesyncCookie header in FileSync requests.
fn generate_filesync_cookie() -> String {
    use rand::Rng;
    let mut rng = rand::rng();
    let random_bytes: Vec<u8> = (0..16).map(|_| rng.random()).collect();
    hex::encode(random_bytes)
}
