//! Cross-language golden vectors for the terminal output frame header
//! (SPEC §3.5, §11.2). Mirrors `tests/frame_vectors.rs`'s approach for the
//! `0xA5` sample frame header, against `web/src/ws/term-frame-vectors.json`.

use awic_proto::term_frame::{decode_header, encode_header, TermFrameHeader};
use serde_json::Value;

fn vectors() -> Vec<Value> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../web/src/ws/term-frame-vectors.json"
    );
    let text = std::fs::read_to_string(path).expect("shared terminal frame vector fixture");
    let doc: Value = serde_json::from_str(&text).expect("fixture is valid JSON");
    doc["vectors"].as_array().expect("vectors array").clone()
}

fn expected(v: &Value) -> TermFrameHeader {
    let field = |k: &str| v[k].as_u64().unwrap_or_else(|| panic!("{k} missing"));
    TermFrameHeader {
        session_id: field("session_id") as u16,
        sequence: field("sequence") as u32,
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
