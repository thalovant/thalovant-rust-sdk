//! Rust SDK for direct Thalovant HiveMind HTTPS clients and agents.

pub mod client;
pub mod constants;
pub mod crypto;
pub mod errors;
pub mod events;
pub mod identity;
pub mod transport;

pub use client::{Client, Conversation, ConversationOptions, RequestOptions};
pub use constants::*;
pub use crypto::{decrypt_from_json, encrypt_as_json, runtime_crypto_key};
pub use errors::{Result, ThalovantError};
pub use events::{
    context_with_correlation, event_matches_context, merge_context, new_request_id, new_session_id,
    utterance_payload, Context, Data, Event, Reply,
};
pub use identity::Identity;
pub use transport::{HttpTransport, TransportHealth};
