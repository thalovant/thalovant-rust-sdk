//! Rust SDK for direct Thalovant HiveMind HTTPS clients and agents.

pub mod client;
pub mod constants;
pub mod context;
pub mod control;
pub mod errors;
pub mod events;
pub mod identity;
pub mod intents;
pub mod noise;
pub mod noise_store;
pub mod protocols;
mod redact;
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
    DeviceAuthorization, DeviceLoginOptions, DevicePrompt, LoginOptions, MarketplaceSkillsOptions,
    MemoryListOptions, OperationResource, OperationStatus, ReleaseOptions, SkillInstallOptions,
    DEFAULT_CONTROL_API_URL, DEFAULT_DEVICE_POLL_INTERVAL, DEFAULT_SKILL_SOURCE_TYPE,
};
pub use errors::{Result, ThalovantError};
pub use events::{
    context_with_correlation, event_matches_context, merge_context, new_request_id, new_session_id,
    utterance_payload, Context, Data, Event, Reply,
};
pub use identity::{default_config_path, Identity, MqttBrokerCredentials};
pub use intents::{
    HubIntent, HubIntentInventory, HubSkillIntents, IntentDefinition, IntentDescribeOptions,
    IntentInventoryOptions, IntentInventorySource, IntentListOptions, IntentRegistration,
    DEFAULT_INTENT_TIMEOUT, DESCRIBE_BATCH,
};
pub use noise::{
    canonical_json, derive_psk, noise_protocol_name, select_noise_options, NoiseFrame,
    NoiseHandshake, NoiseSession, NOISE_PATTERN_KK, NOISE_PATTERN_XX, NOISE_SUITES, PROTOCOL_V3,
};
pub use noise_store::{
    forget_cached_psk, forget_noise_pin, load_cached_psk, load_noise_pin, load_or_create_noise_key,
    noise_state_dir, pin_hub_key, save_cached_psk, NOISE_KEY_FILENAME, NOISE_PINS_FILENAME,
    NOISE_PSK_FILENAME,
};
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
