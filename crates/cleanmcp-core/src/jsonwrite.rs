//! Byte-level helpers for writing JSON without going through `serde_json`.
//!
//! Snapshot rows (`snapshot.rs`) and `export --format json` write hundreds of
//! thousands of small objects; `serde_json`'s per-value dispatch and one-write-
//! per-token behaviour shows up in profiles at that scale. These helpers append
//! to a `Vec<u8>` so the caller can hand whole chunks to the OS (or to a gzip
//! encoder) at once.
//!
//! The escaping matches `serde_json` byte for byte (which is what the snapshot
//! format and the old export code produced), so output stays compatible.

/// `v` in decimal, no allocation.
pub fn push_u64(buf: &mut Vec<u8>, mut v: u64) {
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    loop {
        i -= 1;
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    buf.extend_from_slice(&tmp[i..]);
}

/// `v` in decimal, with a leading `-` when negative.
pub fn push_i64(buf: &mut Vec<u8>, v: i64) {
    if v < 0 {
        buf.push(b'-');
        push_u64(buf, v.unsigned_abs());
    } else {
        push_u64(buf, v as u64);
    }
}

const HEX: &[u8; 16] = b"0123456789abcdef";

/// Append the body of a JSON string literal (without the surrounding quotes),
/// escaping like `serde_json` does.
pub fn push_escaped(buf: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        // Fast path: the overwhelming majority of bytes need no escape.
        if b >= 0x20 && b != b'"' && b != b'\\' {
            continue;
        }
        buf.extend_from_slice(&bytes[start..i]);
        start = i + 1;
        match b {
            b'"' => buf.extend_from_slice(b"\\\""),
            b'\\' => buf.extend_from_slice(b"\\\\"),
            0x08 => buf.extend_from_slice(b"\\b"),
            0x09 => buf.extend_from_slice(b"\\t"),
            0x0a => buf.extend_from_slice(b"\\n"),
            0x0c => buf.extend_from_slice(b"\\f"),
            0x0d => buf.extend_from_slice(b"\\r"),
            // Other C0 control characters become `\u00XX`.
            _ => {
                buf.extend_from_slice(b"\\u00");
                buf.push(HEX[(b >> 4) as usize]);
                buf.push(HEX[(b & 0x0f) as usize]);
            }
        }
    }
    buf.extend_from_slice(&bytes[start..]);
}

/// Append `s` as a complete quoted JSON string.
pub fn push_str(buf: &mut Vec<u8>, s: &str) {
    buf.push(b'"');
    push_escaped(buf, s);
    buf.push(b'"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integers_match_std() {
        for v in [
            0u64,
            1,
            9,
            10,
            99,
            100,
            12345,
            u32::MAX as u64,
            u64::MAX - 1,
            u64::MAX,
        ] {
            let mut b = Vec::new();
            push_u64(&mut b, v);
            assert_eq!(String::from_utf8(b).unwrap(), v.to_string());
        }
        for v in [0i64, -1, -9, -12345, i64::MIN, i64::MAX] {
            let mut b = Vec::new();
            push_i64(&mut b, v);
            assert_eq!(String::from_utf8(b).unwrap(), v.to_string());
        }
    }

    #[test]
    fn strings_match_serde_json() {
        let cases = [
            "",
            "plain",
            "with space",
            "quote\"inside",
            "back\\slash",
            "tab\there",
            "line\nbreak",
            "cr\rhere",
            "bell\u{7}back\u{8}ff\u{c}",
            "control\u{1}\u{1f}",
            "del\u{7f}",
            "héllo wörld",
            "emoji 🚀 and 日本語",
            "//slashes/not/escaped",
        ];
        for s in cases {
            let mut b = Vec::new();
            push_str(&mut b, s);
            assert_eq!(
                String::from_utf8(b).unwrap(),
                serde_json::to_string(s).unwrap(),
                "mismatch for {s:?}"
            );
        }
    }
}
