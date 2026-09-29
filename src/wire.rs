use crate::{
    errors::{Result, ThalovantError},
    events::{binary_frame, binary_kind_name},
    transport::HiveMessage,
};
use flate2::read::ZlibDecoder;
use serde_json::{Map, Value};
use std::io::Read;

pub fn encode_hive_binary_frame(message: &HiveMessage) -> Result<Vec<u8>> {
    let type_id = hive_type_to_int(&message.msg_type);
    let metadata = serde_json::to_vec(&message.metadata)?;
    if metadata.len() > 255 {
        return Err(ThalovantError::Runtime(
            "HiveMind binary metadata cannot exceed 255 bytes".to_string(),
        ));
    }
    let payload = serde_json::to_vec(&message.payload)?;
    let mut out = Vec::with_capacity(2 + metadata.len() + payload.len());
    out.push(0x80 | ((type_id & 0x1f) << 1));
    out.push(metadata.len() as u8);
    out.extend(metadata);
    out.extend(payload);
    Ok(out)
}

pub fn decode_hive_binary_frame(payload: &[u8]) -> Result<HiveMessage> {
    let mut reader = BitReader::new(payload);
    reader.skip_left_padding()?;
    let versioned = reader.read_bit()? == 1;
    if versioned {
        let version = reader.read_uint(8)?;
        if version > 1 {
            return Err(ThalovantError::Runtime(format!(
                "unsupported HiveMind binary protocol version: {version}"
            )));
        }
    }
    let type_id = reader.read_uint(5)? as u8;
    let compressed = reader.read_bit()? == 1;
    let metadata_len = reader.read_uint(8)?;
    let metadata = parse_map(&decode_wire_text(
        &reader.read_bytes(metadata_len)?,
        compressed,
    )?)?;
    let msg_type = hive_int_to_type(type_id).to_string();
    if msg_type == "bin" {
        // A BINARY frame does not carry JSON. Four bits name the payload type,
        // and everything after them is the clip itself: raw, misaligned because
        // the padding goes on the front, and never parsed or decompressed.
        let kind = reader.read_uint(4)? as u8;
        let clip = reader.read_remaining_bytes()?;
        return Ok(HiveMessage {
            msg_type,
            payload: Map::new(),
            binary: Some(binary_frame(binary_kind_name(kind), clip, metadata.clone())),
            metadata,
            route: vec![],
            node: None,
            target_site_id: None,
            target_pubkey: None,
            source_peer: None,
        });
    }
    let payload = parse_map(&decode_wire_text(
        &reader.read_remaining_bytes()?,
        compressed,
    )?)?;
    Ok(HiveMessage {
        msg_type,
        payload,
        binary: None,
        metadata,
        route: vec![],
        node: None,
        target_site_id: None,
        target_pubkey: None,
        source_peer: None,
    })
}

fn hive_type_to_int(msg_type: &str) -> u8 {
    match msg_type {
        "shake" | "handshake" => 0,
        "bus" => 1,
        "shared_bus" => 2,
        "broadcast" => 3,
        "propagate" => 4,
        "escalate" => 5,
        "hello" => 6,
        "query" => 7,
        "cascade" => 8,
        "ping" => 9,
        "rendezvous" => 10,
        "3rdparty" => 11,
        "bin" => 12,
        _ => 11,
    }
}

fn hive_int_to_type(type_id: u8) -> &'static str {
    match type_id {
        0 => "shake",
        1 => "bus",
        2 => "shared_bus",
        3 => "broadcast",
        4 => "propagate",
        5 => "escalate",
        6 => "hello",
        7 => "query",
        8 => "cascade",
        9 => "ping",
        10 => "rendezvous",
        12 => "bin",
        _ => "3rdparty",
    }
}

/// The most a compressed part of a binary frame may inflate to: 32 MiB.
///
/// A reassembled Noise message is itself capped at 32 MiB
/// ([`NOISE_MAX_REASSEMBLY`](crate::noise::NOISE_MAX_REASSEMBLY)); without a
/// cap here, a small frame of zeros from a hub could make the client allocate
/// gigabytes.
pub const MAX_INFLATED: usize = 32 * 1024 * 1024;

fn decode_wire_text(payload: &[u8], compressed: bool) -> Result<String> {
    let bytes = if compressed {
        inflate(payload, MAX_INFLATED)?
    } else {
        payload.to_vec()
    };
    String::from_utf8(bytes).map_err(|err| ThalovantError::Runtime(err.to_string()))
}

/// Inflate a zlib stream to at most `limit` bytes. A stream that inflates
/// past it refuses the frame, and so does one that ends before its end
/// marker: flate2 reports that as the [`ThalovantError::Io`] error
/// (`UnexpectedEof`) a corrupt stream has always been.
fn inflate(payload: &[u8], limit: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    ZlibDecoder::new(payload)
        .take(limit as u64 + 1)
        .read_to_end(&mut out)?;
    if out.len() > limit {
        return Err(ThalovantError::Runtime(format!(
            "HiveMind binary frame inflates past the size limit ({limit} bytes)"
        )));
    }
    Ok(out)
}

fn parse_map(raw: &str) -> Result<Map<String, Value>> {
    Ok(serde_json::from_str::<Value>(raw)?
        .as_object()
        .cloned()
        .unwrap_or_default())
}

struct BitReader<'a> {
    payload: &'a [u8],
    offset: usize,
}

impl<'a> BitReader<'a> {
    fn new(payload: &'a [u8]) -> Self {
        Self { payload, offset: 0 }
    }

    fn skip_left_padding(&mut self) -> Result<()> {
        loop {
            if self.read_bit()? == 1 {
                return Ok(());
            }
        }
    }

    fn read_bit(&mut self) -> Result<u8> {
        if self.offset >= self.payload.len() * 8 {
            return Err(ThalovantError::Runtime(
                "unexpected end of HiveMind binary frame".to_string(),
            ));
        }
        let value = (self.payload[self.offset / 8] >> (7 - (self.offset % 8))) & 1;
        self.offset += 1;
        Ok(value)
    }

    fn read_uint(&mut self, width: usize) -> Result<usize> {
        let mut value = 0;
        for _ in 0..width {
            value = (value << 1) | usize::from(self.read_bit()?);
        }
        Ok(value)
    }

    fn read_bytes(&mut self, len: usize) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(len);
        for _ in 0..len {
            out.push(self.read_uint(8)? as u8);
        }
        Ok(out)
    }

    fn read_remaining_bytes(&mut self) -> Result<Vec<u8>> {
        let bits = self.payload.len() * 8 - self.offset;
        self.read_bytes(bits / 8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hive_binary_frame_round_trips() {
        let message = HiveMessage {
            binary: None,
            msg_type: "bus".to_string(),
            payload: json!({
                "type": "test.event",
                "data": {"ok": true},
                "context": {"metadata": {"thalovant_owner_id": "owner-1"}}
            })
            .as_object()
            .unwrap()
            .clone(),
            metadata: Map::new(),
            route: vec![],
            node: None,
            target_site_id: None,
            target_pubkey: None,
            source_peer: None,
        };
        let encoded = encode_hive_binary_frame(&message).unwrap();
        let decoded = decode_hive_binary_frame(&encoded).unwrap();
        assert_eq!(encoded[0], 0x82);
        assert_eq!(decoded.msg_type, "bus");
        assert_eq!(decoded.payload["type"], "test.event");
    }

    fn zlib(bytes: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    /// A compressed bus frame: `{}` metadata, then `payload`.
    fn compressed_frame(payload: &[u8]) -> Vec<u8> {
        let metadata = zlib(b"{}");
        let mut frame = vec![0x80 | (1 << 1) | 1, metadata.len() as u8];
        frame.extend(metadata);
        frame.extend(payload);
        frame
    }

    #[test]
    fn a_compressed_part_is_capped_when_it_inflates() {
        // A small frame of zeros must not inflate to gigabytes (CWE-409).
        let bomb = zlib(&vec![b'0'; MAX_INFLATED + 1]);
        assert!(bomb.len() < 64 * 1024);
        let error = decode_hive_binary_frame(&compressed_frame(&bomb)).unwrap_err();
        assert!(error.to_string().contains("size limit"), "{error}");
    }

    #[test]
    fn a_compressed_part_at_the_cap_still_inflates_and_a_truncated_one_is_refused() {
        let body = serde_json::to_vec(
            &json!({"type": "speak", "data": {"u": "x".repeat(20)}, "context": {}}),
        )
        .unwrap();
        let limit = body.len();
        assert_eq!(inflate(&zlib(&body), limit).unwrap(), body);
        assert!(inflate(&zlib(&body), limit - 1)
            .unwrap_err()
            .to_string()
            .contains("size limit"));
        let decoded = decode_hive_binary_frame(&compressed_frame(&zlib(&body))).unwrap();
        assert_eq!(decoded.payload["type"], "speak");
        let whole = zlib(&body);
        let truncated = &whole[..whole.len() - 4];
        let error = decode_hive_binary_frame(&compressed_frame(truncated)).unwrap_err();
        assert!(
            matches!(&error, ThalovantError::Io(io) if io.kind() == std::io::ErrorKind::UnexpectedEof),
            "{error:?}"
        );
    }
}
