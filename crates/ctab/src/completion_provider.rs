//! Cometix completion provider implementation
//!
//! Implements the EditPredictionProvider trait for Cursor AI completions.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clock;
use edit_prediction::{Direction, EditPrediction, EditPredictionProvider};
use futures::AsyncReadExt;
use gpui::{App, Context, Entity, EntityId, SharedString, Task};
use http_client::{AsyncBody, HttpClient, Method};
use language::{
    Anchor, Buffer, BufferSnapshot, DiagnosticSeverity, OffsetRangeExt, Point, ToOffset,
};
use parking_lot::Mutex;
use prost::Message;
use settings::Settings;
use sha2::{Digest, Sha256};

use crate::CtabSettings;
use crate::diff_tracker::DiffTracker;
use crate::file_sync::FileSyncManager;
use crate::proto::{
    CodeBlock, CodeResult, CppAppendRequest, CppConfigRequest, CppConfigResponse, CppContextItem,
    CppFate, CppFileDiffHistory, CppIntentInfo, CppParameterHint, CurrentFileInfo, CursorPosition,
    CursorRange, Diagnostic as ProtoDiagnostic, FilesyncUpdateWithModelVersion, FsUploadErrorType,
    LspSubgraphFullContext, LspSuggestedItems, LspSuggestion, RecordCppFateRequest,
    StreamCppRequest, StreamCppResponse, diagnostic::DiagnosticSeverity as ProtoDiagnosticSeverity,
};
use crate::smart_context::SmartContextEngine;
use crate::snapshot_differ::SnapshotDiffer;
use project::Project;

/// Cache TTL for CppConfig (5 minutes)
const CONFIG_CACHE_TTL_SECS: u64 = 300;

/// Client version to report
const CLIENT_VERSION: &str = "1.6.1-zed";

// Note: MAX_ADDITIONAL_FILES and MAX_ADDITIONAL_FILE_SIZE moved to smart_context.rs

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
    /// Smart context engine for intelligent context collection
    smart_context: SmartContextEngine,
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
    /// Followup edits queue for multidiff support
    followup_session: Option<FollowupSession>,
    /// Model info from server (fused cursor prediction, multidiff support)
    is_fused_cursor_prediction_model: bool,
    is_multidiff_model: bool,
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
    /// Pre-computed edits from SnapshotDiffer (computed in refresh, used in suggest)
    /// This avoids expensive computation in the UI thread during suggest()
    precomputed_edits: Option<Vec<(std::ops::Range<Anchor>, Arc<str>)>>,
    /// Original buffer snapshot when edits were computed (for interpolation)
    original_snapshot: Option<BufferSnapshot>,
    /// Cursor prediction target from server response
    cursor_prediction: Option<CursorPredictionTarget>,
    /// Whether this is from a fused cursor prediction model
    is_fused_model: bool,
    /// Whether the model supports multidiff
    is_multidiff_model: bool,
    /// Preloaded jump target for cross-file cursor prediction
    /// Contains (snapshot, target_anchor) if prediction targets a different file
    jump_target: Option<(BufferSnapshot, Anchor)>,
}

/// Cursor prediction target information
#[derive(Clone, Debug)]
pub struct CursorPredictionTarget {
    /// Relative path to the predicted file
    pub relative_path: String,
    /// Line number (1-indexed)
    pub line_number_one_indexed: i32,
    /// Expected content at the target location
    pub expected_content: String,
    /// Whether to retrigger CPP after jumping
    pub should_retrigger_cpp: bool,
}

/// A single edit part from multidiff stream
#[derive(Clone, Debug)]
struct EditPart {
    /// Line range (1-indexed, from server)
    range: (i32, i32),
    /// Edit text content
    text: String,
    /// Binding ID for this edit
    binding_id: Option<String>,
    /// Whether to trim leading EOL
    should_trim_leading: bool,
}

/// Followup session for multidiff edits
struct FollowupSession {
    /// Document path
    document_path: String,
    /// Queued edits
    queue: Vec<EditPart>,
    /// Buffer version when followups were cached
    buffer_version: clock::Global,
}

/// Parsed result from multidiff streaming response
#[derive(Debug)]
struct MultidiffParseResult {
    /// All edit parts from the stream
    edits: Vec<EditPart>,
    /// Cursor prediction target (if any)
    cursor_prediction: Option<CursorPredictionTarget>,
    /// Model info
    is_fused_model: bool,
    is_multidiff_model: bool,
}

impl CtabCompletionProvider {
    pub fn new(http_client: Arc<dyn HttpClient>, project: Option<Entity<Project>>) -> Self {
        // Generate workspace_id - will be updated when we have project context
        let workspace_id = generate_workspace_id();
        Self {
            http_client: http_client.clone(),
            project: project.clone(),
            diff_tracker: Arc::new(Mutex::new(DiffTracker::new())),
            config_cache: Arc::new(Mutex::new(None)),
            file_sync_manager: Arc::new(FileSyncManager::new(http_client, workspace_id.clone())),
            smart_context: SmartContextEngine::new(project),
            buffer_id: None,
            file_extension: None,
            current_file_path: None,
            pending_refresh: None,
            current_completion: None,
            workspace_id,
            filesync_client_key: generate_filesync_client_key(),
            filesync_cookie: generate_filesync_cookie(),
            skip_next_refresh: false,
            followup_session: None,
            is_fused_cursor_prediction_model: true, // Default to true (most models now support this)
            is_multidiff_model: true,               // Default to true
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

    /// Fetches completion with multidiff support.
    ///
    /// Returns a MultidiffParseResult containing all edits and cursor prediction.
    async fn fetch_completion_multidiff(
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
    ) -> Result<MultidiffParseResult> {
        let url = format!("{}{}", base_url, stream_path);

        log::info!("Ctab: [Multidiff] Sending request to {}", url);

        let proto_body = request.encode_to_vec();

        // Connect RPC requires envelope format: [flags(1)][length(4)][payload]
        let body = if uses_connect_rpc {
            let mut envelope = Vec::with_capacity(5 + proto_body.len());
            envelope.push(0u8);
            envelope.extend_from_slice(&(proto_body.len() as u32).to_be_bytes());
            envelope.extend_from_slice(&proto_body);
            envelope
        } else {
            proto_body
        };

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

        let mut response = http_client.send(http_request).await?;
        let status = response.status();

        log::info!("Ctab: [Multidiff] Response status: {}", status);

        // Handle 204 No Content
        if status.as_u16() == 204 {
            log::info!("Ctab: [Multidiff] Server returned 204 No Content");
            return Ok(MultidiffParseResult {
                edits: vec![],
                cursor_prediction: None,
                is_fused_model: true,
                is_multidiff_model: true,
            });
        }

        let mut body_bytes = Vec::new();
        response.body_mut().read_to_end(&mut body_bytes).await?;

        if !status.is_success() {
            let error_text = String::from_utf8_lossy(&body_bytes);
            anyhow::bail!("Request failed with status {}: {}", status, error_text);
        }

        if body_bytes.is_empty() {
            return Ok(MultidiffParseResult {
                edits: vec![],
                cursor_prediction: None,
                is_fused_model: true,
                is_multidiff_model: true,
            });
        }

        // Check if response is JSON (some endpoints return JSON instead of proto)
        if body_bytes.first() == Some(&b'{') {
            if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&body_bytes) {
                let text = json.get("text").and_then(|v| v.as_str()).unwrap_or("");
                let binding_id = json
                    .get("bindingId")
                    .or_else(|| json.get("binding_id"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());

                if !text.is_empty() {
                    return Ok(MultidiffParseResult {
                        edits: vec![EditPart {
                            range: (1, 1), // Fallback range
                            text: text.to_string(),
                            binding_id,
                            should_trim_leading: false,
                        }],
                        cursor_prediction: None,
                        is_fused_model: true,
                        is_multidiff_model: true,
                    });
                }
            }
        }

        // Parse with multidiff support
        Self::parse_streaming_response_multidiff(&body_bytes)
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

    /// Parses gRPC-Web streaming response with multidiff support.
    ///
    /// This enhanced parser handles `begin_edit`/`done_edit` boundaries
    /// to extract multiple edits from a single stream response.
    fn parse_streaming_response_multidiff(body: &[u8]) -> Result<MultidiffParseResult> {
        const HEADER_SIZE: usize = 5;

        let mut edits: Vec<EditPart> = Vec::new();
        let mut current_text = String::new();
        let mut current_range: Option<(i32, i32)> = None;
        let mut current_binding_id: Option<String> = None;
        let mut current_should_trim_leading = false;

        let mut cursor_prediction: Option<CursorPredictionTarget> = None;
        let mut is_fused_model = true;
        let mut is_multidiff_model = true;

        let mut offset = 0;
        let mut message_count = 0;
        let mut in_edit = false;

        log::info!(
            "Ctab: [Multidiff] Starting to parse {} bytes of response",
            body.len()
        );

        // Helper to flush current edit
        let flush_edit = |edits: &mut Vec<EditPart>,
                          text: &mut String,
                          range: &mut Option<(i32, i32)>,
                          binding_id: &mut Option<String>,
                          should_trim: &mut bool| {
            if let Some(r) = range.take() {
                let mut edit_text = std::mem::take(text);
                if *should_trim {
                    edit_text = edit_text
                        .strip_prefix('\n')
                        .or_else(|| edit_text.strip_prefix("\r\n"))
                        .map(|s| s.to_string())
                        .unwrap_or(edit_text);
                }
                if !edit_text.is_empty() || r.0 != r.1 {
                    log::info!(
                        "Ctab: [Multidiff] Flushing edit: lines {}-{}, text_len={}",
                        r.0,
                        r.1,
                        edit_text.len()
                    );
                    edits.push(EditPart {
                        range: r,
                        text: edit_text,
                        binding_id: binding_id.take(),
                        should_trim_leading: *should_trim,
                    });
                }
                *should_trim = false;
            } else {
                text.clear();
            }
        };

        while offset + HEADER_SIZE <= body.len() {
            let flags = body[offset];
            let length = u32::from_be_bytes([
                body[offset + 1],
                body[offset + 2],
                body[offset + 3],
                body[offset + 4],
            ]) as usize;

            message_count += 1;
            offset += HEADER_SIZE;

            if offset + length > body.len() {
                log::warn!(
                    "Ctab: [Multidiff] Incomplete message #{} - expected {} bytes",
                    message_count,
                    length
                );
                break;
            }

            // flags=2 indicates end-of-stream (trailers)
            if flags == 2 {
                offset += length;
                continue;
            }

            let payload = &body[offset..offset + length];
            match StreamCppResponse::decode(payload) {
                Ok(msg) => {
                    // Handle begin_edit: signals start of a new edit
                    if msg.begin_edit.unwrap_or(false) {
                        log::info!(
                            "Ctab: [Multidiff] begin_edit received (edit #{})",
                            edits.len() + 1
                        );
                        // Flush any pending edit before starting new one
                        if in_edit {
                            flush_edit(
                                &mut edits,
                                &mut current_text,
                                &mut current_range,
                                &mut current_binding_id,
                                &mut current_should_trim_leading,
                            );
                        }
                        in_edit = true;
                    }

                    // Accumulate text
                    current_text.push_str(&msg.text);

                    // Update range if provided
                    if let Some(ref range) = msg.range_to_replace {
                        current_range =
                            Some((range.start_line_number, range.end_line_number_inclusive));
                    }

                    // Update binding_id
                    if msg.binding_id.is_some() {
                        current_binding_id = msg.binding_id.clone();
                    }

                    // Update should_remove_leading_eol
                    if msg.should_remove_leading_eol.unwrap_or(false) {
                        current_should_trim_leading = true;
                    }

                    // Handle model_info
                    if let Some(ref info) = msg.model_info {
                        is_fused_model = info.is_fused_cursor_prediction_model;
                        is_multidiff_model = info.is_multidiff_model;
                        log::info!(
                            "Ctab: [Multidiff] Model info: fused={}, multidiff={}",
                            is_fused_model,
                            is_multidiff_model
                        );
                    }

                    // Handle cursor prediction target
                    if let Some(ref target) = msg.cursor_prediction_target {
                        log::info!(
                            "Ctab: [Multidiff] Cursor prediction: {}:{}",
                            target.relative_path,
                            target.line_number_one_indexed
                        );
                        cursor_prediction = Some(CursorPredictionTarget {
                            relative_path: target.relative_path.clone(),
                            line_number_one_indexed: target.line_number_one_indexed,
                            expected_content: target.expected_content.clone(),
                            should_retrigger_cpp: target.should_retrigger_cpp,
                        });
                    }

                    // Handle done_edit: signals end of current edit
                    if msg.done_edit.unwrap_or(false) {
                        log::info!("Ctab: [Multidiff] done_edit received");
                        flush_edit(
                            &mut edits,
                            &mut current_text,
                            &mut current_range,
                            &mut current_binding_id,
                            &mut current_should_trim_leading,
                        );
                        in_edit = false;
                    }

                    // Check for stream termination
                    if msg.done_stream.unwrap_or(false) {
                        log::info!("Ctab: [Multidiff] Stream completed");
                        break;
                    }
                }
                Err(e) => {
                    log::warn!("Ctab: [Multidiff] Failed to decode message: {}", e);
                }
            }

            offset += length;
        }

        // Flush any remaining edit
        if !current_text.is_empty() || current_range.is_some() {
            flush_edit(
                &mut edits,
                &mut current_text,
                &mut current_range,
                &mut current_binding_id,
                &mut current_should_trim_leading,
            );
        }

        log::info!(
            "Ctab: [Multidiff] Parsed {} edits, cursor_prediction={:?}",
            edits.len(),
            cursor_prediction.as_ref().map(|p| &p.relative_path)
        );

        Ok(MultidiffParseResult {
            edits,
            cursor_prediction,
            is_fused_model,
            is_multidiff_model,
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

        log::debug!(
            "Cometix: CppConfig response - status={}, body_len={}, body_preview={:?}",
            status,
            body_bytes.len(),
            String::from_utf8_lossy(&body_bytes[..body_bytes.len().min(200)])
        );

        if !status.is_success() {
            let error_text = String::from_utf8_lossy(&body_bytes);
            log::warn!(
                "Cometix: CppConfig request failed with status {}: {}",
                status,
                error_text
            );
            anyhow::bail!("CppConfig request failed: {}", status);
        }

        // Handle empty response gracefully
        if body_bytes.is_empty() {
            log::warn!("Cometix: CppConfig returned empty response, using defaults");
            return Ok(CppConfigResponse::default());
        }

        let config = match CppConfigResponse::decode(&body_bytes[..]) {
            Ok(c) => c,
            Err(e) => {
                log::warn!(
                    "Cometix: CppConfig decode failed ({}), raw bytes: {:?}",
                    e,
                    &body_bytes[..body_bytes.len().min(50)]
                );
                // Return default config instead of failing
                return Ok(CppConfigResponse::default());
            }
        };
        log::info!(
            "Cometix: Received CppConfig - is_on={:?}, debounce={}ms, geo_url={:?}",
            config.is_on,
            config.client_debounce_duration_millis,
            config.geo_cpp_backend_url
        );

        // Phase 3: Log enhanced config fields if present
        if config.allows_tab_chunks || config.tab_context_refresh_debounce_ms.is_some() {
            log::info!(
                "Cometix: Enhanced config - allows_tab_chunks={}, tab_refresh_debounce_ms={:?}, editor_change_debounce_ms={:?}",
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
    /// Retrieves the enclosing symbol (function/class/method) context using TreeSitter.
    ///
    /// This provides the model with the full scope of the current code block,
    /// improving context awareness for completions within functions or classes.
    ///
    /// Uses `symbols_containing` which is a synchronous TreeSitter-based API,
    /// avoiding the latency of LSP calls.
    fn get_enclosing_context(
        &self,
        snapshot: &BufferSnapshot,
        cursor_offset: usize,
        file_path: &str,
    ) -> Option<CppContextItem> {
        log::info!(
            "Cometix: get_enclosing_context called - offset={}, file={}",
            cursor_offset,
            file_path
        );

        // symbols_containing returns symbols sorted by hierarchy (outermost to innermost).
        // The last element is the most specific enclosing scope (e.g., inner function).
        let symbols = snapshot.symbols_containing(cursor_offset, None);

        log::info!(
            "Cometix: symbols_containing returned {} symbols",
            symbols.len()
        );

        if symbols.is_empty() {
            log::info!(
                "Cometix: No enclosing symbols found at offset {}",
                cursor_offset
            );
            return None;
        }

        // Get the innermost (most specific) enclosing symbol
        let innermost = symbols.last()?;

        // Convert anchor range to offset range and extract the text
        let range = innermost.range.to_offset(snapshot);
        let contents: String = snapshot.text_for_range(range).collect();

        // Skip if the scope is too large (> 10KB) to avoid overwhelming the context
        if contents.len() > 10 * 1024 {
            log::debug!(
                "Cometix: Enclosing scope too large ({} bytes), skipping",
                contents.len()
            );
            return None;
        }

        log::info!(
            "Cometix: Found enclosing scope '{}' ({} bytes)",
            innermost.text,
            contents.len()
        );

        Some(CppContextItem {
            contents,
            symbol: Some(innermost.text.clone()),
            relative_workspace_path: file_path.to_string(),
            score: 1.0, // High relevance for the immediate enclosing scope
        })
    }

    /// Check if there are followup edits available
    pub fn has_followup_edits(&self) -> bool {
        self.followup_session
            .as_ref()
            .map(|s| !s.queue.is_empty())
            .unwrap_or(false)
    }

    /// Get the number of remaining followup edits
    pub fn followup_count(&self) -> usize {
        self.followup_session
            .as_ref()
            .map(|s| s.queue.len())
            .unwrap_or(0)
    }

    /// Get cursor prediction target if available
    pub fn get_cursor_prediction(&self) -> Option<&CursorPredictionTarget> {
        self.current_completion
            .as_ref()
            .and_then(|c| c.cursor_prediction.as_ref())
    }

    /// Try to preload the target buffer for cross-file cursor prediction
    ///
    /// This is called synchronously during refresh when a cross-file prediction is detected.
    /// It attempts to find an already-open buffer for the target file.
    /// If the buffer isn't open, returns None (we don't want to block on file I/O).
    fn try_preload_jump_target(
        project: &Entity<Project>,
        relative_path: &str,
        line_number_one_indexed: i32,
        cx: &mut App,
    ) -> Option<(BufferSnapshot, Anchor)> {
        use project::ProjectPath;
        use util::paths::PathStyle;
        use util::rel_path::RelPath;

        let project_ref = project.read(cx);

        // Try to convert relative_path to RelPath
        let target_rel_path =
            match RelPath::new(std::path::Path::new(relative_path), PathStyle::Posix) {
                Ok(path) => path.into_owned(),
                Err(e) => {
                    log::debug!(
                        "Ctab: [CursorPrediction] Invalid path '{}': {}",
                        relative_path,
                        e
                    );
                    return None;
                }
            };

        // Try to find the target file in any worktree
        for worktree in project_ref.worktrees(cx) {
            let worktree_ref = worktree.read(cx);
            let worktree_id = worktree_ref.id();

            // Check if the file exists in this worktree
            if worktree_ref.entry_for_path(&target_rel_path).is_some() {
                let project_path = ProjectPath {
                    worktree_id,
                    path: target_rel_path.into(),
                };

                // Check if buffer is already open (don't block on file I/O)
                if let Some(buffer) = project_ref.get_open_buffer(&project_path, cx) {
                    let buffer_ref = buffer.read(cx);
                    let snapshot = buffer_ref.snapshot();

                    // Convert 1-indexed line number to 0-indexed and create anchor
                    let line = (line_number_one_indexed - 1).max(0) as u32;
                    let point = Point::new(line, 0);

                    // Ensure point is within buffer bounds
                    let clamped_point = snapshot.clip_point(point, language::Bias::Left);
                    let target_anchor = snapshot.anchor_before(clamped_point);

                    log::info!(
                        "Ctab: [CursorPrediction] Preloaded jump target: {}:{} -> anchor at {:?}",
                        relative_path,
                        line_number_one_indexed,
                        clamped_point
                    );

                    return Some((snapshot, target_anchor));
                } else {
                    log::debug!(
                        "Ctab: [CursorPrediction] Target buffer not open: {}",
                        relative_path
                    );
                }

                // Only check first matching worktree
                break;
            }
        }

        log::debug!(
            "Ctab: [CursorPrediction] Target file not found in worktrees: {}",
            relative_path
        );
        None
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
            log::debug!("Ctab: Skipping refresh (requested by previous completion)");
            self.skip_next_refresh = false;
            return;
        }

        // PRIORITY 1: Check for cached followup edits from previous multidiff stream
        // Followup edits take priority because they are the "next edit" the user expects
        if let Some(ref mut session) = self.followup_session {
            if !session.queue.is_empty() {
                let buffer_ref = buffer.read(cx);
                let current_version = buffer_ref.version();

                // Check if buffer version has changed since followups were cached
                // If the version has changed, invalidate followups to avoid stale edits
                if current_version.changed_since(&session.buffer_version) {
                    // Buffer has changed, invalidate followups
                    log::info!("Ctab: [Multidiff] Clearing stale followup cache (buffer changed)");
                    self.followup_session = None;
                } else {
                    // Use next followup edit
                    let next_edit = session.queue.remove(0);
                    log::info!(
                        "Ctab: [Multidiff] Using cached followup edit ({} remaining)",
                        session.queue.len()
                    );

                    // Pre-compute edits for the followup
                    let buffer_snapshot = buffer_ref.snapshot();
                    let cursor_offset = cursor_position.to_offset(&buffer_snapshot);
                    let cursor_point = buffer_snapshot.offset_to_point(cursor_offset);

                    let snapshot_differ = SnapshotDiffer::new();
                    let diff_result = snapshot_differ.extract_inline_edits(
                        &buffer_snapshot,
                        cursor_offset,
                        cursor_point,
                        &next_edit.text,
                        Some(next_edit.range),
                    );

                    let precomputed_edits = if diff_result.edits.is_empty() {
                        None
                    } else {
                        Some(diff_result.edits)
                    };

                    // Store the followup as current completion
                    self.current_completion = Some(CompletionState {
                        text: next_edit.text,
                        binding_id: next_edit.binding_id,
                        range_start: cursor_position,
                        range_end: cursor_position,
                        api_range: Some(next_edit.range),
                        should_retrigger: !session.queue.is_empty(), // Retrigger if more followups
                        precomputed_edits,
                        original_snapshot: Some(buffer_snapshot),
                        cursor_prediction: None, // Followups don't have cursor prediction
                        is_fused_model: self.is_fused_cursor_prediction_model,
                        is_multidiff_model: self.is_multidiff_model,
                        jump_target: None, // Followups are always in the same file
                    });

                    // Clear session if no more followups
                    if session.queue.is_empty() {
                        self.followup_session = None;
                    }

                    cx.notify();
                    return;
                }
            }
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

        // Record file edit in smart context tracker
        self.smart_context.record_file_edit(&file_path);

        // Collect smart context using the enhanced engine
        let language_id = Self::detect_language(&file_path);
        let smart_items =
            self.smart_context
                .collect_context(&buffer, offset, &file_path, &language_id, cx);

        // Convert to proto formats
        let additional_files = SmartContextEngine::to_additional_files(&smart_items);
        let mut context_items = SmartContextEngine::to_context_items(&smart_items);
        // Convert smart context items to CodeResult format for enhanced context
        let code_results = SmartContextEngine::to_code_results(&smart_items);
        // Convert LSP resolver cache to LspSubgraphFullContext format
        let lsp_contexts = self.smart_context.to_lsp_contexts();

        log::info!(
            "Ctab: SmartContext collected {} items (sources: {})",
            smart_items.len(),
            smart_items
                .iter()
                .map(|s| format!("{:?}", s.source))
                .collect::<Vec<_>>()
                .join(", ")
        );

        // Log new high-priority fields status
        log::info!(
            "Ctab: Enhanced fields - code_results={}, parameter_hints=0 (todo), lsp_contexts={}",
            code_results.len(),
            lsp_contexts.len()
        );

        // Add enclosing scope context (TreeSitter based, synchronous)
        if let Some(enclosing_context) = self.get_enclosing_context(&snapshot, offset, &file_path) {
            context_items.push(enclosing_context);
        }

        // Build and retrieve diff history with timestamps
        let (diff_history, diff_timestamps) = {
            let mut tracker = self.diff_tracker.lock();
            // Update tracker with current content
            tracker.build_diff_history(&file_path, &content);
            // Retrieve full history with timestamps
            tracker.get_diff_history(&file_path)
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

        // Log file sync capability analysis to file
        {
            use crate::file_sync::get_filesync_logger;
            let logger = get_filesync_logger();
            logger.log_capability_analysis(
                &file_path,
                rely_on_filesync,
                filesync_updates.len(),
                content.len(),
            );
        }

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
            // Log each diagnostic detail for debugging
            for (i, diag) in diagnostics.iter().enumerate() {
                if let Some(ref range) = diag.range {
                    let start = range
                        .start_position
                        .as_ref()
                        .map(|p| format!("{}:{}", p.line, p.column))
                        .unwrap_or_default();
                    let end = range
                        .end_position
                        .as_ref()
                        .map(|p| format!("{}:{}", p.line, p.column))
                        .unwrap_or_default();
                    log::info!(
                        "Cometix: Diagnostic[{}]: severity={:?}, range={}..{}, message='{}'",
                        i,
                        diag.severity,
                        start,
                        end,
                        diag.message
                    );
                }
            }
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
                // Cursor API uses 1-indexed line/column numbers
                cursor_position: Some(CursorPosition {
                    line: (point.row + 1) as i32,
                    column: (point.column + 1) as i32,
                }),
                // Selection: use cursor position as both start and end (no selection = cursor at point)
                selection: Some(CursorRange {
                    start_position: Some(CursorPosition {
                        line: (point.row + 1) as i32,
                        column: (point.column + 1) as i32,
                    }),
                    end_position: Some(CursorPosition {
                        line: (point.row + 1) as i32,
                        column: (point.column + 1) as i32,
                    }),
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
            // New fields from unite.proto
            linter_errors: None, // TODO: Convert diagnostics to LinterErrors format
            diff_history_keys: vec![],
            give_debug_output: None,
            // Build proper CppFileDiffHistory structure with complete history
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
                    diff_history,
                    diff_history_timestamps: diff_timestamps,
                }]
            },
            merged_diff_histories: vec![],
            block_diff_patches: vec![],
            is_nightly: Some(false),
            is_debug: None,
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
            // LSP suggested items (autocomplete suggestions)
            lsp_suggested_items: None, // TODO: Integrate with LSP completion provider
            supports_cpt: Some(true),
            supports_crlf_cpt: Some(false),
            // High priority enhancements (Phase 2)
            // Parameter hints: would require LSP signature help integration
            // For now, leave empty as placeholder for future LSP integration
            parameter_hints: vec![],
            // LSP symbol contexts: converted from LspResolver cache
            lsp_contexts,
            // Code results from search/context - populated from smart_context
            code_results,
        };

        let http_client = self.http_client.clone();
        let filesync_client_key = self.filesync_client_key.clone();
        let filesync_cookie = self.filesync_cookie.clone();

        // Capture buffer for diff calculation in the async callback
        let buffer_for_diff = buffer.clone();

        // Capture file path for followup session
        let current_file_path = file_path.clone();

        self.pending_refresh = Some(cx.spawn(async move |this, cx| {
            if debounce {
                gpui::Timer::after(Duration::from_millis(debounce_ms)).await;
            }

            // Use multidiff-aware fetch
            let response = Self::fetch_completion_multidiff(
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
                    Ok(parse_result) => {
                        // Update model info
                        this.is_fused_cursor_prediction_model = parse_result.is_fused_model;
                        this.is_multidiff_model = parse_result.is_multidiff_model;

                        if parse_result.edits.is_empty() {
                            log::debug!("Ctab: [Multidiff] No edits received");
                            return;
                        }

                        log::info!(
                            "Ctab: [Multidiff] Received {} edits, cursor_prediction={:?}",
                            parse_result.edits.len(),
                            parse_result.cursor_prediction.as_ref().map(|p| &p.relative_path)
                        );

                        // Take first edit for immediate display
                        let first_edit = &parse_result.edits[0];
                        let api_range = Some(first_edit.range);

                        log::info!(
                            "Ctab: [Multidiff] First edit: lines {}-{}, text_len={}",
                            first_edit.range.0,
                            first_edit.range.1,
                            first_edit.text.len()
                        );

                        // Store remaining edits in followup queue
                        if parse_result.edits.len() > 1 {
                            let followup_edits: Vec<EditPart> =
                                parse_result.edits[1..].to_vec();
                            log::info!(
                                "Ctab: [Multidiff] Queueing {} followup edits",
                                followup_edits.len()
                            );

                            let buffer_ref = buffer_for_diff.read(cx);
                            this.followup_session = Some(FollowupSession {
                                document_path: current_file_path.clone(),
                                queue: followup_edits,
                                buffer_version: buffer_ref.version(),
                            });
                        } else {
                            this.followup_session = None;
                        }

                        // Pre-compute edits using SnapshotDiffer
                        let buffer_ref = buffer_for_diff.read(cx);
                        let buffer_snapshot = buffer_ref.snapshot();
                        let cursor_offset = cursor_position.to_offset(&buffer_snapshot);
                        let cursor_point = buffer_snapshot.offset_to_point(cursor_offset);

                        let snapshot_differ = SnapshotDiffer::new();
                        let diff_result = snapshot_differ.extract_inline_edits(
                            &buffer_snapshot,
                            cursor_offset,
                            cursor_point,
                            &first_edit.text,
                            api_range,
                        );

                        log::info!(
                            "Ctab: Pre-computed {} edits, confidence={:.3}, optimizations={:?}",
                            diff_result.edits.len(),
                            diff_result.confidence,
                            diff_result.optimizations
                        );

                        let precomputed_edits = if diff_result.edits.is_empty() {
                            None
                        } else {
                            Some(diff_result.edits)
                        };

                        // Determine if we should retrigger
                        let should_retrigger = parse_result
                            .cursor_prediction
                            .as_ref()
                            .map(|p| p.should_retrigger_cpp)
                            .unwrap_or(this.followup_session.is_some()); // Retrigger if there are followups

                        // Check for cross-file cursor prediction and try to preload target buffer
                        let jump_target = if let Some(ref prediction) = parse_result.cursor_prediction {
                            // Check if prediction is for a different file
                            let is_cross_file = prediction.relative_path != current_file_path;
                            if is_cross_file {
                                log::info!(
                                    "Ctab: [CursorPrediction] Cross-file jump detected: {} -> {}:{}",
                                    current_file_path,
                                    prediction.relative_path,
                                    prediction.line_number_one_indexed
                                );
                                // Try to find and open the target buffer
                                if let Some(ref project) = this.project {
                                    Self::try_preload_jump_target(
                                        project,
                                        &prediction.relative_path,
                                        prediction.line_number_one_indexed,
                                        cx,
                                    )
                                } else {
                                    log::debug!("Ctab: [CursorPrediction] No project available for preloading");
                                    None
                                }
                            } else {
                                log::info!(
                                    "Ctab: [CursorPrediction] Same-file prediction: line {}",
                                    prediction.line_number_one_indexed
                                );
                                None
                            }
                        } else {
                            None
                        };

                        // Store the completion
                        this.current_completion = Some(CompletionState {
                            text: first_edit.text.clone(),
                            binding_id: first_edit.binding_id.clone(),
                            range_start: cursor_position,
                            range_end: cursor_position,
                            api_range,
                            should_retrigger,
                            precomputed_edits,
                            original_snapshot: Some(buffer_snapshot),
                            cursor_prediction: parse_result.cursor_prediction,
                            is_fused_model: parse_result.is_fused_model,
                            is_multidiff_model: parse_result.is_multidiff_model,
                            jump_target,
                        });

                        cx.notify();
                    }
                    Err(e) => {
                        log::error!("Ctab: [Multidiff] Failed to fetch completion: {}", e);
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
            if let Some(ref binding_id) = completion.binding_id {
                self.send_fate(binding_id.clone(), CppFate::Accept, cx);
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

            // Check for followup edits (multidiff support)
            if let Some(ref mut session) = self.followup_session {
                if !session.queue.is_empty() {
                    log::info!(
                        "Ctab: [Multidiff] {} followup edits remaining after accept",
                        session.queue.len()
                    );
                    // Don't skip next refresh - we want to show the next edit
                    // The next refresh will pick up from the followup queue
                    return;
                }
            }

            // Clear followup session if empty
            self.followup_session = None;

            // Check if we should skip the next refresh (prevents completion loop)
            if !completion.should_retrigger {
                log::debug!("Ctab: Disabling next refresh (should_retrigger=false)");
                self.skip_next_refresh = true;
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
        _cursor_position: Anchor,
        cx: &mut Context<Self>,
    ) -> Option<EditPrediction> {
        let completion = self.current_completion.as_ref()?;

        // Verify buffer matches
        if self.buffer_id != Some(buffer.entity_id()) {
            return None;
        }

        // PRIORITY: Check for cross-file jump target first
        // If we have a preloaded jump target, return Jump prediction
        if let Some((ref target_snapshot, ref target_anchor)) = completion.jump_target {
            log::info!(
                "Ctab: [CursorPrediction] Returning Jump prediction to {:?}",
                completion
                    .cursor_prediction
                    .as_ref()
                    .map(|p| &p.relative_path)
            );
            return Some(EditPrediction::Jump {
                id: completion
                    .binding_id
                    .as_ref()
                    .map(|id| SharedString::from(id.clone())),
                snapshot: target_snapshot.clone(),
                target: *target_anchor,
            });
        }

        // Get pre-computed edits and original snapshot for interpolation
        let precomputed_edits = completion.precomputed_edits.as_ref()?;
        let original_snapshot = completion.original_snapshot.as_ref()?;

        if precomputed_edits.is_empty() {
            log::debug!("Ctab: No pre-computed edits available, skipping");
            return None;
        }

        // Get current buffer snapshot for interpolation
        let current_snapshot = buffer.read(cx).snapshot();

        // Interpolate edits to account for user typing since completion was computed
        // This is lightweight - just anchor adjustment, no heavy computation
        let edits = crate::snapshot_differ::interpolate_edits(
            original_snapshot,
            &current_snapshot,
            precomputed_edits,
        )?;

        if edits.is_empty() {
            log::debug!("Ctab: Interpolation resulted in empty edits, skipping");
            return None;
        }

        // DEBUG: Log edit info (lightweight)
        log::debug!(
            "Ctab: suggest() returning {} interpolated edits for completion {:?}",
            edits.len(),
            completion.binding_id
        );

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

// NOTE: Old completion_from_diff and build_completion_context functions removed.
// Now using SnapshotDiffer for precise BufferSnapshot-based diff calculation.

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
