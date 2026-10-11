//! Minimal unsigned LEB128, used by the digital run-length block encoding
//! (SPEC §2.8.1) and the analog codec's outlier escape list (§2.8.2). Small
//! enough, and needed in two places with slightly different element types,
//! that it isn't worth a dependency.

pub fn write_uvarint(buf: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            buf.push(byte);
            return;
        }
        buf.push(byte | 0x80);
    }
}

/// Reads one varint starting at `*pos`, advancing `*pos` past it.
pub fn read_uvarint(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    loop {
        let byte = *buf.get(*pos)?;
        *pos += 1;
        result |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some(result);
        }
        shift += 7;
        if shift >= 64 {
            return None; // malformed: too long for u64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn known_values() {
        let mut buf = Vec::new();
        write_uvarint(&mut buf, 0);
        assert_eq!(buf, vec![0]);

        buf.clear();
        write_uvarint(&mut buf, 300);
        assert_eq!(buf, vec![0xAC, 0x02]);
    }

    proptest! {
        #[test]
        fn round_trips(v: u64) {
            let mut buf = Vec::new();
            write_uvarint(&mut buf, v);
            let mut pos = 0;
            let decoded = read_uvarint(&buf, &mut pos).unwrap();
            prop_assert_eq!(decoded, v);
            prop_assert_eq!(pos, buf.len());
        }
    }
}
