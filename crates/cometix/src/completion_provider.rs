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
use language::{Anchor, Buffer, ToOffset};
use parking_lot::Mutex;
use prost::Message;
use settings::Settings;
use sha2::{Digest, Sha256};

use crate::CometixSettings;
use crate::diff_tracker::DiffTracker;
use crate::proto::{
    CppFate, CppIntentInfo, CurrentFileInfo, CursorPosition, RecordCppFateRequest,
    StreamCppRequest, StreamCppResponse,
};

/// Client version to report
const CLIENT_VERSION: &str = "1.6.1-zed";

pub struct CometixCompletionProvider {
    http_client: Arc<dyn HttpClient>,
    diff_tracker: Arc<Mutex<DiffTracker>>,
    buffer_id: Option<EntityId>,
    file_extension: Option<String>,
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
    pub fn new(http_client: Arc<dyn HttpClient>) -> Self {
        Self {
            http_client,
            diff_tracker: Arc::new(Mutex::new(DiffTracker::new())),
            buffer_id: None,
            file_extension: None,
            pending_refresh: None,
            current_completion: None,
            workspace_id: generate_workspace_id(),
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
    ) -> Result<StreamCppResponse> {
        let url = format!("{}{}", base_url, stream_path);

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

        // Read entire response body for LPM parsing
        let mut body_bytes = Vec::new();
        response.body_mut().read_to_end(&mut body_bytes).await?;

        // Parse Length-Prefixed Messages (gRPC-Web format)
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
        let mut offset = 0;

        while offset + HEADER_SIZE <= body.len() {
            // Parse 5-byte header: 1 byte compression flag + 4 bytes length
            let _compression_flag = body[offset];
            let length = u32::from_be_bytes([
                body[offset + 1],
                body[offset + 2],
                body[offset + 3],
                body[offset + 4],
            ]) as usize;

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

            // Decode the protobuf message
            let payload = &body[offset..offset + length];
            match StreamCppResponse::decode(payload) {
                Ok(msg) => {
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

                    // Check for stream termination
                    if msg.done_stream.unwrap_or(false) {
                        log::debug!("Cometix: Stream completed with done_stream flag");
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
        let debounce_ms = settings.debounce_ms;
        let model_name = settings.model.clone().unwrap_or_else(|| "auto".to_string());

        let client_key = settings
            .client_key
            .clone()
            .unwrap_or_else(generate_client_key);

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

        // Build diff history
        let diff_history = {
            let mut tracker = self.diff_tracker.lock();
            tracker.build_diff_history(&file_path, &content)
        };

        let file_version = {
            let tracker = self.diff_tracker.lock();
            tracker.get_file_version(&file_path)
        };

        // Build the request
        let request = StreamCppRequest {
            current_file: Some(CurrentFileInfo {
                relative_workspace_path: file_path.clone(),
                contents: content.clone(),
                rely_on_filesync: false,
                sha_256_hash: Some(Self::compute_sha256(&content)),
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
            additional_files: vec![],
            control_token: None,
            client_time: Some(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0),
            ),
            filesync_updates: vec![],
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

                            // Store the completion
                            this.current_completion = Some(CompletionState {
                                text: response.text,
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
            if let Some(binding_id) = completion.binding_id {
                self.send_fate(binding_id, CppFate::Accept, cx);
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

        let _snapshot = buffer.read(cx).snapshot();

        // Create the edit prediction
        let range = completion.range_start..completion.range_end;
        let text: Arc<str> = Arc::from(completion.text.as_str());

        Some(EditPrediction::Local {
            id: completion
                .binding_id
                .as_ref()
                .map(|id| SharedString::from(id.clone())),
            edits: vec![(range, text)],
            edit_preview: None,
        })
    }
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
