//! Binary frames, against the vectors and frames every SDK shares.
//!
//! A hub answers `speak:synth` by rendering the utterance and sending the audio
//! back, so a client with no synthesiser of its own can still speak; a file
//! arrives the same way. The expectations are `binary-vectors.json` and the
//! frames themselves are `binary-frames.json` -- hivemind-bus-client's own
//! encoder output, so this is tested against the wire a hub actually puts out
//! rather than against a reading of the specification.

use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::Value;
use thalovant::events::{binary_kind_name, BINARY_PAYLOAD_KINDS};
use thalovant::wire::decode_hive_binary_frame;

fn vectors(name: &str) -> Value {
    let raw = std::fs::read_to_string(format!("tests/conformance/{name}"))
        .unwrap_or_else(|error| panic!("read {name}: {error}"));
    serde_json::from_str(&raw).unwrap_or_else(|error| panic!("parse {name}: {error}"))
}

fn frame(name: &str) -> Vec<u8> {
    let frames = vectors("binary-frames.json");
    let case = frames["cases"]
        .as_array()
        .expect("cases")
        .iter()
        .find(|case| case["name"] == name)
        .unwrap_or_else(|| panic!("no frame for {name}"))
        .clone();
    STANDARD
        .decode(case["frame"].as_str().expect("frame"))
        .expect("base64")
}

#[test]
fn the_payload_kinds_are_the_ones_the_vectors_name() {
    let spec = vectors("binary-vectors.json");
    let named = spec["payload_kinds"].as_object().expect("payload_kinds");
    assert_eq!(named.len(), BINARY_PAYLOAD_KINDS.len());
    for (wire, name) in BINARY_PAYLOAD_KINDS {
        assert_eq!(named[&wire.to_string()], name, "payload type {wire}");
    }
}

#[test]
fn a_payload_type_nobody_named_arrives_under_its_number() {
    // Only 0-15 can travel: the wire field is four bits. The naming has to hold
    // for every number all the same -- it is the last thing between a payload
    // type nobody has named yet and a frame that disappears.
    let spec = vectors("binary-vectors.json");
    for (wire, name) in spec["unnamed_kind_names"]
        .as_object()
        .expect("unnamed_kind_names")
    {
        let number: u8 = wire.parse().expect("wire number");
        assert_eq!(binary_kind_name(number), name.as_str().expect("name"));
    }
}

#[test]
fn the_reference_encoders_frames_decode_here() {
    let frames = vectors("binary-frames.json");
    for case in frames["cases"].as_array().expect("cases") {
        let name = case["name"].as_str().expect("name");
        let raw = STANDARD
            .decode(case["frame"].as_str().expect("frame"))
            .expect("base64");
        let message = decode_hive_binary_frame(&raw).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(message.msg_type, "bin", "{name}");
        let binary = message
            .binary
            .as_ref()
            .unwrap_or_else(|| panic!("{name}: no binary"));
        assert_eq!(
            binary.kind,
            case["expected_kind"].as_str().expect("kind"),
            "{name}"
        );
        let clip = STANDARD
            .decode(case["expected_payload"].as_str().expect("payload"))
            .expect("base64");
        assert_eq!(
            binary.data, clip,
            "{name}: the clip did not survive the decode"
        );
        for (key, want) in case["expected_metadata"].as_object().expect("metadata") {
            assert_eq!(&binary.metadata[key], want, "{name}: metadata {key}");
        }
    }
}

#[test]
fn a_binarized_bus_frame_is_still_text() {
    // Only BINARY carries bytes; every other type binarized on the wire is JSON
    // and has to keep decoding as it always did.
    let frames = vectors("binary-frames.json");
    let raw = STANDARD
        .decode(frames["bus_frame"].as_str().expect("bus_frame"))
        .expect("base64");
    let message = decode_hive_binary_frame(&raw).expect("decode");
    assert_eq!(message.msg_type, "bus");
    assert!(message.binary.is_none());
    assert_eq!(message.payload["type"], "speak");
}

#[test]
fn every_case_the_vectors_describe_decodes_as_it_says() {
    let spec = vectors("binary-vectors.json");
    for case in spec["cases"].as_array().expect("cases") {
        let name = case["name"].as_str().expect("name");
        let message =
            decode_hive_binary_frame(&frame(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        let binary = message
            .binary
            .as_ref()
            .unwrap_or_else(|| panic!("{name}: no binary"));
        let expected = &case["expected"];
        assert_eq!(
            binary.kind,
            expected["kind"].as_str().expect("kind"),
            "{name}"
        );
        // An empty name is no name: rendering "" would put a blank filename in
        // front of somebody as though the hub had chosen it.
        for (field, got) in [
            ("utterance", &binary.utterance),
            ("lang", &binary.lang),
            ("file_name", &binary.file_name),
        ] {
            let want = expected[field].as_str().map(str::to_string);
            assert_eq!(got, &want, "{name}: {field}");
        }
    }
}
