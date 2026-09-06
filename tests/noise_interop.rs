//! Live interop against a real HiveMind-core 5.x listener.
//!
//! The unit tests in `src/noise.rs` pin the pre-shared key, canonical JSON and
//! prologue against vectors taken from the reference implementation, which
//! catches every byte-level divergence we know to look for. This catches the
//! ones we do not: the wire type names, the envelope shapes, the order of the
//! exchange, and whether the hub accepts what we encrypt.
//!
//! It skips unless a hub is named, because it needs a running listener:
//!
//! ```text
//! hivemind-core add-client --name rust --access-key <key> --password <password>
//! hivemind-core allow-msg recognizer_loop:utterance <id>
//! hivemind-core listen
//!
//! THALOVANT_INTEROP_WSS=ws://127.0.0.1:5678 \
//! THALOVANT_INTEROP_ACCESS_KEY=<key> \
//! THALOVANT_INTEROP_PASSWORD=<password> \
//! cargo test --test noise_interop -- --nocapture
//! ```
//!
//! A hub pins the client static key on first contact, so a run whose state
//! directory has been discarded needs `hivemind-core reset-noise-pin <key>`.

use std::env;
use std::path::PathBuf;

use serde_json::json;
use thalovant::{Data, Identity, WssTransport};

fn interop_settings() -> Option<(String, String, String)> {
    let endpoint = env::var("THALOVANT_INTEROP_WSS").ok()?;
    let access_key = env::var("THALOVANT_INTEROP_ACCESS_KEY").ok()?;
    let password = env::var("THALOVANT_INTEROP_PASSWORD").ok()?;
    if endpoint.is_empty() || access_key.is_empty() || password.is_empty() {
        return None;
    }
    Some((endpoint, access_key, password))
}

#[tokio::test]
async fn completes_a_v3_handshake_against_a_live_hub() {
    let Some((endpoint, access_key, password)) = interop_settings() else {
        eprintln!(
            "skipping: set THALOVANT_INTEROP_WSS, THALOVANT_INTEROP_ACCESS_KEY and \
             THALOVANT_INTEROP_PASSWORD to run the live interop test"
        );
        return;
    };

    // Part of the opt-in guard, not an assertion: the hub pins the client static
    // key, so the state directory has to outlive the run. Panicking here would
    // fail the suite for anyone who set only three of the four variables.
    let Some(state_dir) = env::var("THALOVANT_INTEROP_STATE_DIR")
        .ok()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
    else {
        eprintln!(
            "skipping: set THALOVANT_INTEROP_STATE_DIR too, so the client static key \
             survives between runs (the hub pins it)"
        );
        return;
    };

    let identity = Identity::from_value(json!({
        "access_key": access_key,
        "password": password,
        "site_id": "rust-interop",
        "default_master": endpoint,
        "default_port": 5678,
        "data_plane_endpoints": {"wss": endpoint},
    }))
    .expect("identity");

    let transport = WssTransport::new(identity);
    transport.set_noise_state_dir(Some(state_dir)).await;

    transport
        .connect()
        .await
        .unwrap_or_else(|err| panic!("the v3 handshake against {endpoint} failed: {err}"));

    let remote = transport.remote_static_key().await;
    assert_eq!(
        remote.as_deref().map(str::len),
        Some(64),
        "the hub's static key came back as {remote:?}; there is nothing to pin"
    );

    // A message the hub has to decrypt and route proves the transport, not just
    // the handshake.
    transport
        .emit_bus(
            "recognizer_loop:utterance",
            Data::from_iter([
                ("utterances".to_string(), json!(["hello from rust"])),
                ("lang".to_string(), json!("en-US")),
            ]),
            Default::default(),
        )
        .await
        .expect("the hub rejected an encrypted bus message");

    // Give the hub a moment to close the socket if it disliked the frame.
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    let health = transport.healthcheck().await;
    assert!(
        health.connected && health.handshake_complete,
        "the session dropped after the first encrypted message: {health:?}"
    );

    transport.disconnect().await.expect("disconnect");
}
