use crate::{
    errors::{Result, ThalovantError},
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
    let payload = parse_map(&decode_wire_text(
        &reader.read_remaining_bytes()?,
        compressed,
    )?)?;
    Ok(HiveMessage {
        msg_type: hive_int_to_type(type_id).to_string(),
        payload,
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

fn decode_wire_text(payload: &[u8], compressed: bool) -> Result<String> {
    let bytes = if compressed {
        let mut decoder = ZlibDecoder::new(payload);
        let mut out = Vec::new();
        decoder.read_to_end(&mut out)?;
        out
    } else {
        payload.to_vec()
    };
    String::from_utf8(bytes).map_err(|err| ThalovantError::Runtime(err.to_string()))
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
}
