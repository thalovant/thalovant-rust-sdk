//! Rust SDK for direct Thalovant HiveMind HTTPS clients and agents.

pub mod client;
pub mod constants;
pub mod context;
pub mod control;
pub mod crypto;
pub mod errors;
pub mod events;
pub mod identity;
pub mod protocols;
pub mod rich;
mod tls;
pub mod transport;
pub mod wire;

pub use client::{
    ActionOptions, Client, CodeOptions, Conversation, ConversationOptions, QueryOptions,
    RequestOptions,
};
pub use constants::*;
pub use context::{build_client_context, ClientContextOptions};
pub use control::{
    AnalyticsOverviewOptions, BootstrapIdentityOptions, BootstrapIdentityResult, ControlPlane,
    MemoryListOptions, DEFAULT_CONTROL_API_URL,
};
pub use crypto::{
    decrypt_binary, decrypt_from_json, encrypt_as_binary, encrypt_as_json, runtime_crypto_key,
};
pub use errors::{Result, ThalovantError};
pub use events::{
    context_with_correlation, event_matches_context, merge_context, new_request_id, new_session_id,
    utterance_payload, Context, Data, Event, Reply,
};
pub use identity::{default_config_path, Identity, MqttBrokerCredentials};
pub use protocols::{
    endpoint_from_domain, select_data_plane_endpoint, HubDataPlaneEndpoints, HubProtocol,
    HubProtocolSettings, SelectedHubEndpoint, DEFAULT_PROTOCOL_PREFERENCE,
};
pub use rich::{display_items_from_event_data, rich_media_from_data, strip_ssml, DisplayItem};
pub use transport::{
    mqtt_topics_for_identity, HttpTransport, MqttTopicSet, MqttTransport, RuntimeTransport,
    TransportConnectionInfo, TransportConnectionPhase, TransportHealth, WssTransport,
};
pub use wire::{decode_hive_binary_frame, encode_hive_binary_frame};
