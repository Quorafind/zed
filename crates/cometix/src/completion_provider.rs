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
use language::{Anchor, Buffer, Point, ToOffset};
use parking_lot::Mutex;
use prost::Message;
use settings::Settings;
use sha2::{Digest, Sha256};

use crate::CometixSettings;
use crate::diff_tracker::DiffTracker;
use crate::file_sync::FileSyncManager;
use crate::proto::{
    AdditionalFile, CppAppendRequest, CppConfigRequest, CppConfigResponse, CppFate, CppIntentInfo,
    CurrentFileInfo, CursorPosition, FilesyncUpdateWithModelVersion, FsUploadErrorType,
    RecordCppFateRequest, StreamCppRequest, StreamCppResponse,
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

pub struct CometixCompletionProvider {
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
}

struct CompletionState {
    text: String,
    binding_id: Option<String>,
    range_start: Anchor,
    range_end: Anchor,
    /// Original 1-based line range from API response (for debugging)
    #[allow(dead_code)]
    api_range: Option<(i32, i32)>,
}

impl CometixCompletionProvider {
    pub fn new(http_client: Arc<dyn HttpClient>, project: Option<Entity<Project>>) -> Self {
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
                    log::info!(
                        "Cometix: Decoded message - text='{}', done_stream={:?}, binding_id={:?}",
                        if msg.text.len() > 50 {
                            format!("{}...", &msg.text[..50])
                        } else {
                            msg.text.clone()
                        },
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
        let settings = CometixSettings::get_global(cx);

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
        let settings = CometixSettings::get_global(cx);

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
        let settings = CometixSettings::get_global(cx);

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
    fn collect_additional_files(&self, current_file_path: &str, cx: &App) -> Vec<AdditionalFile> {
        let Some(project) = &self.project else {
            log::debug!("Cometix: No project available for additional files");
            return vec![];
        };

        let project = project.read(cx);

        // Get current file extension for language-based prioritization
        let current_ext = current_file_path
            .rsplit('.')
            .next()
            .map(|s| s.to_lowercase());

        // Collect candidate files, separated by language match
        let mut same_lang_files = Vec::new();
        let mut other_files = Vec::new();

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

            // Build visible range content (full file for now)
            let lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
            let total_lines = lines.len() as i32;

            let additional_file = AdditionalFile {
                relative_workspace_path: file_path.clone(),
                is_open: true,
                visible_range_content: lines,
                // Note: We don't have true last_viewed_at time available here.
                // Setting to None as we cannot accurately track MRU at this level.
                last_viewed_at: None,
                start_line_number_one_indexed: vec![1],
                visible_ranges: vec![crate::proto::LineRange {
                    start_line_number: 1,
                    end_line_number_inclusive: total_lines,
                }],
            };

            // Prioritize files with the same extension
            let file_ext = file_path.rsplit('.').next().map(|s| s.to_lowercase());
            if file_ext == current_ext {
                same_lang_files.push(additional_file);
            } else {
                other_files.push(additional_file);
            }
        }

        // Combine: same-language files first, then others
        let mut additional_files = same_lang_files;
        additional_files.extend(other_files);

        // Limit to MAX_ADDITIONAL_FILES
        additional_files.truncate(MAX_ADDITIONAL_FILES);

        log::info!(
            "Cometix: Collected {} additional files for context",
            additional_files.len()
        );

        additional_files
    }
}

impl EditPredictionProvider for CometixCompletionProvider {
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
        let settings = CometixSettings::get_global(cx);
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

        let settings = CometixSettings::get_global(cx);

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

        // Collect additional files for multi-file context
        let additional_files = self.collect_additional_files(&file_path, cx);

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
            let upload_base_url = base_url.clone();
            let upload_path = settings.fs_upload_path();
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

        // Determine if we can rely on filesync (file has been uploaded and no pending updates)
        let rely_on_filesync = !needs_upload && filesync_updates.is_empty();

        log::info!(
            "Cometix: File sync status - needs_upload={}, rely_on_filesync={}, pending_updates={}",
            needs_upload,
            rely_on_filesync,
            filesync_updates.len()
        );

        // Build the request
        log::info!(
            "Cometix: Building request - file={}, cursor=({},{}), content_len={}, language={}",
            file_path,
            point.row,
            point.column,
            content.len(),
            Self::detect_language(&file_path)
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
            }),
            diff_history: if diff_history.is_empty() {
                vec![]
            } else {
                vec![diff_history]
            },
            model_name: Some(model_name),
            file_diff_histories: vec![],
            immediately_ack: Some(false),
            enable_more_context: Some(true),
            cpp_intent_info: Some(CppIntentInfo {
                source: "typing".to_string(),
            }),
            workspace_id: Some(self.workspace_id.clone()),
            additional_files,
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
                            log::debug!("Cometix: Received completion: {}", response.text);

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

                            // Store the completion
                            this.current_completion = Some(CompletionState {
                                text: completion_text,
                                binding_id: response.binding_id,
                                range_start,
                                range_end,
                                api_range,
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

        // Get the current line content up to the cursor
        let cursor_point = buffer_snapshot.offset_to_point(cursor_offset);
        let line_start_offset = buffer_snapshot.point_to_offset(Point::new(cursor_point.row, 0));
        let text_before_cursor: String = buffer_snapshot
            .chars_for_range(line_start_offset..cursor_offset)
            .collect();

        // Process completion text - strip leading newline if present
        let mut completion_text = completion.text.as_str();
        if completion_text.starts_with('\n') {
            completion_text = &completion_text[1..];
        } else if completion_text.starts_with("\r\n") {
            completion_text = &completion_text[2..];
        }

        // Find common prefix between what user typed and completion
        let prefix_len = common_prefix(text_before_cursor.chars(), completion_text.chars());

        // The text to insert is the completion minus the common prefix
        let insert_text = &completion_text[prefix_len..];

        if insert_text.is_empty() {
            return None;
        }

        log::debug!(
            "Cometix: suggest - text_before_cursor='{}', completion='{}', prefix_len={}, insert_text='{}'",
            text_before_cursor,
            if completion_text.len() > 50 {
                &completion_text[..50]
            } else {
                completion_text
            },
            prefix_len,
            if insert_text.len() > 50 {
                &insert_text[..50]
            } else {
                insert_text
            }
        );

        // Insert at cursor position
        let insert_position = cursor_position.bias_right(buffer_snapshot);
        let text: Arc<str> = Arc::from(insert_text);

        Some(EditPrediction::Local {
            id: completion
                .binding_id
                .as_ref()
                .map(|id| SharedString::from(id.clone())),
            edits: vec![(insert_position..insert_position, text)],
            edit_preview: None,
        })
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
fn generate_workspace_id() -> String {
    use rand::Rng;
    let mut rng = rand::rng();
    (0..7)
        .map(|_| (b'a' + rng.random_range(0..26)) as char)
        .collect::<Vec<_>>()
        .into_iter()
        .collect()
}

/// Generate a client key for checksum validation
fn generate_client_key() -> String {
    use rand::Rng;
    let mut rng = rand::rng();
    let random_bytes: Vec<u8> = (0..32).map(|_| rng.random()).collect();
    hex::encode(random_bytes)
}
