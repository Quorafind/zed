//! Cometix completion provider for Zed
//!
//! This crate provides an edit prediction provider that integrates with
//! the Cursor AI completion API (api2.cursor.sh) or self-hosted cursor-api servers.

mod completion_provider;
mod diff_tracker;
mod file_sync;

pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/aiserver.v1.rs"));
}

pub use completion_provider::CometixCompletionProvider;

use gpui::App;
use settings::{RegisterSetting, Settings, SettingsContent};

/// Default API base URL for official Cursor API
pub const DEFAULT_API_URL: &str = "https://api2.cursor.sh";

/// Default API base URL for self-hosted servers
pub const DEFAULT_SELFHOSTED_URL: &str = "http://localhost:8000";

/// Initialize the Cometix completion provider
pub fn init(cx: &mut App) {
    CometixSettings::register(cx);
}

/// The type of endpoint to use for API calls
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EndpointType {
    /// Official Cursor API (api2.cursor.sh) using Connect RPC / gRPC-Web format
    #[default]
    Official,
    /// Self-hosted server with custom base_url
    Selfhosted,
}

/// Settings for Cometix
#[derive(Clone, Debug, Default, RegisterSetting)]
pub struct CometixSettings {
    /// Whether Cometix is enabled
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

impl CometixSettings {
    /// Returns the effective base URL based on endpoint type
    pub fn effective_base_url(&self) -> &str {
        if let Some(ref url) = self.base_url {
            if !url.is_empty() {
                return url;
            }
        }
        match self.endpoint_type {
            EndpointType::Official => DEFAULT_API_URL,
            EndpointType::Selfhosted => DEFAULT_SELFHOSTED_URL,
        }
    }

    /// Returns the API path for streaming completions based on endpoint type
    pub fn stream_cpp_path(&self) -> &'static str {
        match self.endpoint_type {
            EndpointType::Official => "/aiserver.v1.AiService/StreamCpp",
            EndpointType::Selfhosted => "/cpp/stream",
        }
    }

    /// Returns the API path for recording fate based on endpoint type
    pub fn record_fate_path(&self) -> &'static str {
        match self.endpoint_type {
            EndpointType::Official => "/aiserver.v1.AiService/RecordCppFate",
            EndpointType::Selfhosted => "/cpp/fate",
        }
    }

    /// Returns the API path for CppAppend based on endpoint type
    pub fn cpp_append_path(&self) -> &'static str {
        match self.endpoint_type {
            EndpointType::Official => "/aiserver.v1.AiService/CppAppend",
            EndpointType::Selfhosted => "/cpp/append",
        }
    }

    /// Returns the API path for CppConfig based on endpoint type
    pub fn cpp_config_path(&self) -> &'static str {
        match self.endpoint_type {
            EndpointType::Official => "/aiserver.v1.AiService/CppConfig",
            EndpointType::Selfhosted => "/cpp/config",
        }
    }

    /// Returns the API path for FSUploadFile based on endpoint type
    pub fn fs_upload_path(&self) -> &'static str {
        match self.endpoint_type {
            EndpointType::Official => "/aiserver.v1.FileSyncService/FSUploadFile",
            EndpointType::Selfhosted => "/fs/upload",
        }
    }

    /// Returns the API path for FSSyncFile based on endpoint type
    pub fn fs_sync_path(&self) -> &'static str {
        match self.endpoint_type {
            EndpointType::Official => "/aiserver.v1.FileSyncService/FSSyncFile",
            EndpointType::Selfhosted => "/fs/sync",
        }
    }

    /// Returns the header name for client key based on endpoint type
    pub fn client_key_header(&self) -> &'static str {
        match self.endpoint_type {
            EndpointType::Official => "x-cursor-checksum",
            EndpointType::Selfhosted => "x-client-key",
        }
    }

    /// Returns true if using Connect RPC protocol (official format)
    pub fn uses_connect_rpc(&self) -> bool {
        matches!(self.endpoint_type, EndpointType::Official)
    }
}

impl Settings for CometixSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let cometix = content.cometix.as_ref();

        log::info!(
            "Cometix: from_settings called, cometix section present: {}",
            cometix.is_some()
        );

        if let Some(c) = cometix {
            log::info!(
                "Cometix: config values - endpoint_type={:?}, base_url={:?}, auth_token={}",
                c.endpoint_type,
                c.base_url,
                c.auth_token.is_some()
            );
        }

        let endpoint_type = cometix
            .and_then(|c| c.endpoint_type)
            .map(|e| match e {
                settings::CometixEndpointType::Official => EndpointType::Official,
                settings::CometixEndpointType::Selfhosted => EndpointType::Selfhosted,
            })
            .unwrap_or_default();

        CometixSettings {
            enabled: cometix.and_then(|c| c.enabled).unwrap_or(true),
            auth_token: cometix.and_then(|c| c.auth_token.clone()),
            base_url: cometix.and_then(|c| c.base_url.clone()),
            client_key: cometix.and_then(|c| c.client_key.clone()),
            endpoint_type,
            model: cometix.and_then(|c| c.model.clone()),
            debounce_ms: cometix.and_then(|c| c.debounce_ms).unwrap_or(75),
            max_completion_length: cometix
                .and_then(|c| c.max_completion_length)
                .unwrap_or(2000),
        }
    }
}
