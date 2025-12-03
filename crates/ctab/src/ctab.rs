//! Ctab completion provider for Zed
//!
//! This crate provides an edit prediction provider that integrates with
//! the Cursor AI completion API (api2.cursor.sh) or self-hosted cursor-api servers.

mod completion_differ;
mod completion_provider;
mod diff_tracker;
mod file_sync;
mod snapshot_differ;

pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/aiserver.v1.rs"));
}

pub use completion_provider::CtabCompletionProvider;

use gpui::App;
use settings::{RegisterSetting, Settings, SettingsContent};

/// Default API base URL for official Cursor API
pub const DEFAULT_API_URL: &str = "https://api2.cursor.sh";

/// Default API base URL for self-hosted servers
pub const DEFAULT_SELFHOSTED_URL: &str = "http://localhost:8000";

/// Initialize the Ctab completion provider
pub fn init(cx: &mut App) {
    CtabSettings::register(cx);
}

/// The type of endpoint to use for API calls
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EndpointType {
    /// Official Cursor API (api2.cursor.sh) using Connect RPC / gRPC-Web format
    #[default]
    Official,
    /// Proxy server that forwards to official API (uses official paths with custom base_url)
    SelfhostedProxy,
    /// Self-hosted server with simplified API paths (e.g., cursor-api project)
    Selfhosted,
}

/// Settings for Ctab
#[derive(Clone, Debug, Default, RegisterSetting)]
pub struct CtabSettings {
    /// Whether Ctab is enabled
    pub enabled: bool,
    /// The authentication token for the Cursor API
    pub auth_token: Option<String>,
    /// The base URL for the API
    pub base_url: Option<String>,
    /// The client key for checksum validation
    pub client_key: Option<String>,
    /// The endpoint type (official or selfhosted)
    pub endpoint_type: EndpointType,
    /// The model to use for completions
    pub model: Option<String>,
    /// Debounce delay in milliseconds
    pub debounce_ms: u64,
    /// Maximum completion length
    pub max_completion_length: u32,
}

impl CtabSettings {
    /// Returns the effective base URL based on endpoint type
    pub fn effective_base_url(&self) -> &str {
        if let Some(ref url) = self.base_url {
            if !url.is_empty() {
                return url;
            }
        }
        match self.endpoint_type {
            EndpointType::Official => DEFAULT_API_URL,
            EndpointType::SelfhostedProxy | EndpointType::Selfhosted => DEFAULT_SELFHOSTED_URL,
        }
    }

    /// Returns the API path for streaming completions
    pub fn stream_cpp_path(&self) -> &'static str {
        "/aiserver.v1.AiService/StreamCpp"
    }

    /// Returns the API path for recording fate
    pub fn record_fate_path(&self) -> &'static str {
        "/aiserver.v1.AiService/RecordCppFate"
    }

    /// Returns the API path for CppAppend
    pub fn cpp_append_path(&self) -> &'static str {
        "/aiserver.v1.AiService/CppAppend"
    }

    /// Returns the API path for CppConfig
    pub fn cpp_config_path(&self) -> &'static str {
        "/aiserver.v1.AiService/CppConfig"
    }

    /// Returns the API path for FSUploadFile
    pub fn fs_upload_path(&self) -> &'static str {
        "/aiserver.v1.FileSyncService/FSUploadFile"
    }

    /// Returns the API path for FSSyncFile
    pub fn fs_sync_path(&self) -> &'static str {
        "/aiserver.v1.FileSyncService/FSSyncFile"
    }

    /// Returns the header name for client key
    pub fn client_key_header(&self) -> &'static str {
        "x-cursor-checksum"
    }

    /// Returns true if using Connect RPC protocol (official format)
    ///
    /// Note: Most self-hosted cursor-api servers also expect Connect RPC format
    /// (5-byte envelope header + protobuf payload), so we always return true.
    /// If you have a server that expects raw proto, you may need to add a config option.
    pub fn uses_connect_rpc(&self) -> bool {
        // Always use Connect RPC format - most servers (including self-hosted)
        // expect the standard Connect protocol with envelope headers
        true
    }
}

impl Settings for CtabSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let ctab = content.ctab.as_ref();

        log::info!(
            "Ctab: from_settings called, ctab section present: {}",
            ctab.is_some()
        );

        if let Some(c) = ctab {
            log::info!(
                "Ctab: config values - endpoint_type={:?}, base_url={:?}, auth_token={}",
                c.endpoint_type,
                c.base_url,
                c.auth_token.is_some()
            );
        }

        let endpoint_type = ctab
            .and_then(|c| c.endpoint_type)
            .map(|e| match e {
                settings::CtabEndpointType::Official => EndpointType::Official,
                settings::CtabEndpointType::SelfhostedProxy => EndpointType::SelfhostedProxy,
                settings::CtabEndpointType::Selfhosted => EndpointType::Selfhosted,
            })
            .unwrap_or_default();

        CtabSettings {
            enabled: ctab.and_then(|c| c.enabled).unwrap_or(true),
            auth_token: ctab.and_then(|c| c.auth_token.clone()),
            base_url: ctab.and_then(|c| c.base_url.clone()),
            client_key: ctab.and_then(|c| c.client_key.clone()),
            endpoint_type,
            model: ctab.and_then(|c| c.model.clone()),
            debounce_ms: ctab.and_then(|c| c.debounce_ms).unwrap_or(75),
            max_completion_length: ctab.and_then(|c| c.max_completion_length).unwrap_or(2000),
        }
    }
}
