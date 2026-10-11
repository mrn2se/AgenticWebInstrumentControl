//! Cross-language golden vectors for the binary view frame header (SPEC
//! §3.4), mirroring `tests/frame_vectors.rs` for the §3.2 sample frame
//! header. The header will eventually also be hand-mirrored in
//! `web/src/ws/view-frame.ts` (T18); both sides will test against the same
//! spec-derived fixture, `web/src/ws/view-frame-vectors.json`.

use awic_proto::view_frame::{
    decode_view_frame_header, encode_view_frame_header, ViewFrameFlags, ViewFrameHeader,
    ViewFrameKind,
};
use serde_json::Value;

fn vectors() -> Vec<Value> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../web/src/ws/view-frame-vectors.json"
    );
    let text = std::fs::read_to_string(path).expect("shared view frame vector fixture");
    let doc: Value = serde_json::from_str(&text).expect("fixture is valid JSON");
    doc["vectors"].as_array().expect("vectors array").clone()
}

fn expected(v: &Value) -> ViewFrameHeader {
    let field = |k: &str| v[k].as_u64().unwrap_or_else(|| panic!("{k} missing"));
    let u64_field = |k: &str| -> u64 {
        v[k].as_str()
            .unwrap_or_else(|| panic!("{k} missing"))
            .parse()
            .expect("u64")
    };
    ViewFrameHeader {
        view_id: field("view_id") as u16,
        sequence: field("sequence") as u32,
        kind: ViewFrameKind::from_u8(field("kind") as u8).expect("known kind"),
        channel_count: field("channel_count") as u8,
        sample_bits: field("sample_bits") as u8,
        flags: ViewFrameFlags::from_u8(field("flags") as u8),
        item_count: field("item_count") as u32,
        first_sample: u64_field("first_sample"),
        samples_per_item: u64_field("samples_per_item"),
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
        let bytes = encode_view_frame_header(&expected(&v));
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
        let decoded = decode_view_frame_header(&unhex(v["hex"].as_str().unwrap())).expect(name);
        assert_eq!(decoded, expected(&v), "decode: {name}");
    }
}
