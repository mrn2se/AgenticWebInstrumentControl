//! Cross-language golden vectors for the binary frame header (SPEC §3.2).
//!
//! The header is hand-mirrored in `web/src/ws/frame.ts` because raw bytes are
//! not something `ts-rs` can export — so unlike the JSON control types, drift
//! between the two codecs would not be a compile error. Both sides therefore
//! test against the same spec-derived fixture, `web/src/ws/frame-vectors.json`.

use awic_proto::frame::{decode_header, encode_header, Encoding, FrameHeader};
use serde_json::Value;

fn vectors() -> Vec<Value> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../web/src/ws/frame-vectors.json"
    );
    let text = std::fs::read_to_string(path).expect("shared frame vector fixture");
    let doc: Value = serde_json::from_str(&text).expect("fixture is valid JSON");
    doc["vectors"].as_array().expect("vectors array").clone()
}

fn expected(v: &Value) -> FrameHeader {
    let field = |k: &str| v[k].as_u64().unwrap_or_else(|| panic!("{k} missing"));
    FrameHeader {
        stream_id: field("stream_id") as u16,
        sequence: field("sequence") as u32,
        encoding: Encoding::from_u8(field("encoding") as u8).expect("known encoding"),
        channel_count: field("channel_count") as u8,
        sample_count: field("sample_count") as u32,
        t0_ns: v["t0_ns"]
            .as_str()
            .expect("t0_ns string")
            .parse()
            .expect("u64"),
    }
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
        .collect()
}

#[test]
fn encoder_produces_the_spec_bytes() {
    for v in vectors() {
        let name = v["name"].as_str().unwrap();
        let bytes = encode_header(&expected(&v));
        assert_eq!(
            bytes.to_vec(),
            unhex(v["hex"].as_str().unwrap()),
            "encode: {name}"
        );
    }
}

#[test]
fn decoder_recovers_the_spec_fields() {
    for v in vectors() {
        let name = v["name"].as_str().unwrap();
        let decoded = decode_header(&unhex(v["hex"].as_str().unwrap())).expect(name);
        assert_eq!(decoded, expected(&v), "decode: {name}");
    }
}
