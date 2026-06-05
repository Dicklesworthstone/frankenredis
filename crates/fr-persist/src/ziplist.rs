//! Ziplist + zipmap decoders for loading legacy (redis ≤ 6.2) RDB payloads.
//!
//! Ziplist is the pre-listpack packed encoding used for small lists/hashes/zsets
//! (RDB types `LIST_ZIPLIST`=10, `ZSET_ZIPLIST`=12, `HASH_ZIPLIST`=13) and as the
//! node format of the old quicklist (`LIST_QUICKLIST`=14). Zipmap (`HASH_ZIPMAP`=9)
//! is the even-older small-hash encoding. Modern redis (7.x) transparently
//! upgrades these to listpack on load; we decode them to the same flat entry
//! vector (`Vec<Vec<u8>>`) the listpack decoder produces, so the rest of the RDB
//! path is unchanged. Integer-encoded entries are rendered as their decimal
//! string form, exactly as redis materialises them.

/// Decode a ziplist blob into its flat entry list.
///
/// Layout: `zlbytes:u32le, zltail:u32le, zllen:u16le, <entry>* , 0xFF`. Each
/// entry is `prevlen` (1 byte, or `0xFE` + 4 bytes LE) followed by an encoding:
/// a 6/14/32-bit string length (big-endian for the multi-byte forms) or one of
/// the integer encodings (`0xC0/0xD0/0xE0` int16/32/64 LE, `0xF0` int24 LE,
/// `0xFE` int8, `0xF1..=0xFD` 4-bit immediate `(enc & 0x0F) - 1`).
pub fn decode_ziplist(data: &[u8]) -> Option<Vec<Vec<u8>>> {
    if data.len() < 11 {
        return None;
    }
    let zlbytes = u32::from_le_bytes(data[0..4].try_into().ok()?) as usize;
    // The blob must be self-describing — its header length equals its own size.
    if zlbytes != data.len() {
        return None;
    }
    let zllen = u16::from_le_bytes(data[8..10].try_into().ok()?);

    let mut out: Vec<Vec<u8>> = Vec::new();
    let mut i = 10usize;
    loop {
        let b = *data.get(i)?;
        if b == 0xFF {
            break; // ziplist terminator
        }
        // prevlen: 1 byte if < 0xFE, else 0xFE followed by a 4-byte length.
        if b < 0xFE {
            i += 1;
        } else {
            if i + 5 > data.len() {
                return None;
            }
            i += 5;
        }

        let enc = *data.get(i)?;
        match enc >> 6 {
            0b00 => {
                // 6-bit string length.
                let len = (enc & 0x3F) as usize;
                i += 1;
                let end = i.checked_add(len)?;
                out.push(data.get(i..end)?.to_vec());
                i = end;
            }
            0b01 => {
                // 14-bit string length (big-endian across the 2 header bytes).
                let lo = *data.get(i + 1)?;
                let len = (((enc & 0x3F) as usize) << 8) | lo as usize;
                i += 2;
                let end = i.checked_add(len)?;
                out.push(data.get(i..end)?.to_vec());
                i = end;
            }
            0b10 => {
                // 32-bit string length (big-endian, the 4 bytes after the marker).
                let hdr = data.get(i + 1..i + 5)?;
                let len = ((hdr[0] as usize) << 24)
                    | ((hdr[1] as usize) << 16)
                    | ((hdr[2] as usize) << 8)
                    | (hdr[3] as usize);
                i += 5;
                let end = i.checked_add(len)?;
                out.push(data.get(i..end)?.to_vec());
                i = end;
            }
            _ => {
                // Integer encodings (top two bits are 11).
                let (val, adv): (i64, usize) = match enc {
                    0xC0 => (
                        i16::from_le_bytes([*data.get(i + 1)?, *data.get(i + 2)?]) as i64,
                        3,
                    ),
                    0xD0 => {
                        let h = data.get(i + 1..i + 5)?;
                        (i32::from_le_bytes([h[0], h[1], h[2], h[3]]) as i64, 5)
                    }
                    0xE0 => (
                        i64::from_le_bytes(data.get(i + 1..i + 9)?.try_into().ok()?),
                        9,
                    ),
                    0xF0 => {
                        // 24-bit signed little-endian.
                        let h = data.get(i + 1..i + 4)?;
                        let raw = (h[0] as i32) | ((h[1] as i32) << 8) | ((h[2] as i32) << 16);
                        (((raw << 8) >> 8) as i64, 4)
                    }
                    0xFE => (*data.get(i + 1)? as i8 as i64, 2),
                    0xF1..=0xFD => (((enc & 0x0F) as i64) - 1, 1),
                    _ => return None, // invalid encoding byte
                };
                i += adv;
                out.push(val.to_string().into_bytes());
            }
        }
    }

    // `zllen` saturates at 0xFFFF ("unknown, must count"); otherwise it is exact.
    if zllen != 0xFFFF && out.len() != zllen as usize {
        return None;
    }
    Some(out)
}

/// Decode a zipmap blob (legacy small-hash encoding, RDB type 9) into flat
/// field/value entries.
///
/// Layout: `zmlen:u8, (<keylen><key> <vallen><free:u8><value>)* , 0xFF`. The
/// leading `zmlen` is only a hint (255 = "scan to terminator"), so we always
/// walk to the `0xFF` end marker. Lengths are a single byte when `< 254`, else
/// `0xFE` + a 4-byte little-endian length.
pub fn decode_zipmap(data: &[u8]) -> Option<Vec<Vec<u8>>> {
    if data.is_empty() {
        return None;
    }
    let mut i = 1usize; // skip the zmlen hint
    let mut out: Vec<Vec<u8>> = Vec::new();
    loop {
        let b = *data.get(i)?;
        if b == 0xFF {
            break; // zipmap terminator
        }
        // Field.
        let (klen, adv) = zipmap_decode_len(data, i)?;
        i += adv;
        let kend = i.checked_add(klen)?;
        out.push(data.get(i..kend)?.to_vec());
        i = kend;
        // Value: length, one "free" padding byte, then the value bytes.
        let (vlen, adv) = zipmap_decode_len(data, i)?;
        i += adv;
        let free = *data.get(i)? as usize;
        i += 1;
        let vend = i.checked_add(vlen)?;
        out.push(data.get(i..vend)?.to_vec());
        i = vend.checked_add(free)?;
    }
    if !out.len().is_multiple_of(2) {
        return None;
    }
    Some(out)
}

fn zipmap_decode_len(data: &[u8], i: usize) -> Option<(usize, usize)> {
    let b = *data.get(i)?;
    if b < 254 {
        Some((b as usize, 1))
    } else if b == 254 {
        let h = data.get(i + 1..i + 5)?;
        Some((u32::from_le_bytes([h[0], h[1], h[2], h[3]]) as usize, 5))
    } else {
        None // 255 is the terminator, never a length
    }
}

#[cfg(test)]
mod tests {
    use super::{decode_ziplist, decode_zipmap};

    /// Build a ziplist from already-encoded entry bodies (prevlen is filled in).
    fn assemble_ziplist(entries: &[Vec<u8>]) -> Vec<u8> {
        let mut body = Vec::new();
        let mut prev = 0usize;
        for e in entries {
            // prevlen
            if prev < 254 {
                body.push(prev as u8);
            } else {
                body.push(0xFE);
                body.extend_from_slice(&(prev as u32).to_le_bytes());
            }
            body.extend_from_slice(e);
            prev = if prev < 254 { 1 + e.len() } else { 5 + e.len() };
        }
        let zllen = entries.len() as u16;
        let zlbytes = (10 + body.len() + 1) as u32;
        let mut out = Vec::new();
        out.extend_from_slice(&zlbytes.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // zltail (unused by decoder)
        out.extend_from_slice(&zllen.to_le_bytes());
        out.extend_from_slice(&body);
        out.push(0xFF);
        out
    }

    fn str6(s: &[u8]) -> Vec<u8> {
        let mut v = vec![s.len() as u8]; // 00xxxxxx
        v.extend_from_slice(s);
        v
    }

    #[test]
    fn ziplist_decodes_short_strings_and_immediate_ints() {
        // Mirrors the redis hash-ziplist fixture's entry shapes.
        let zl = assemble_ziplist(&[str6(b"one"), vec![0xF2], str6(b"two"), vec![0xF3]]);
        let entries = decode_ziplist(&zl).expect("decode");
        assert_eq!(
            entries,
            vec![
                b"one".to_vec(),
                b"1".to_vec(),
                b"two".to_vec(),
                b"2".to_vec()
            ]
        );
    }

    #[test]
    fn ziplist_decodes_every_integer_width() {
        let zl = assemble_ziplist(&[
            vec![0xFE, 0xFB],                                           // int8 = -5
            vec![0xC0, 0x2E, 0xFB],                                     // int16 LE = -1234
            vec![0xF0, 0x2E, 0xFB, 0xFF],                               // int24 LE = -1234
            vec![0xD0, 0x15, 0xCD, 0x5B, 0x07],                         // int32 LE = 123456789
            vec![0xE0, 0x15, 0x81, 0xE9, 0x7D, 0xF4, 0x10, 0x22, 0x11], // int64 LE
            vec![0xF1],                                                 // immediate 0
            vec![0xFD],                                                 // immediate 12
        ]);
        let entries = decode_ziplist(&zl).expect("decode");
        assert_eq!(entries[0], b"-5");
        assert_eq!(entries[1], b"-1234");
        assert_eq!(entries[2], b"-1234");
        assert_eq!(entries[3], b"123456789");
        assert_eq!(
            entries[4],
            0x1122_10f4_7de9_8115_i64.to_string().into_bytes()
        );
        assert_eq!(entries[5], b"0");
        assert_eq!(entries[6], b"12");
    }

    #[test]
    fn ziplist_decodes_14bit_and_32bit_strings() {
        let big = vec![b'z'; 300]; // needs 14-bit length
        let mut e14 = vec![0x40 | ((300 >> 8) as u8), (300 & 0xFF) as u8];
        e14.extend_from_slice(&big);
        let huge = vec![b'q'; 70_000]; // needs 32-bit length
        let mut e32 = vec![
            0x80,
            0,
            (70_000 >> 16) as u8,
            (70_000 >> 8) as u8,
            (70_000 & 0xFF) as u8,
        ];
        e32.extend_from_slice(&huge);
        let zl = assemble_ziplist(&[e14, e32]);
        let entries = decode_ziplist(&zl).expect("decode");
        assert_eq!(entries[0], big);
        assert_eq!(entries[1], huge);
    }

    #[test]
    fn ziplist_rejects_truncated_and_unterminated() {
        let zl = assemble_ziplist(&[str6(b"x")]);
        assert!(decode_ziplist(&zl[..zl.len() - 1]).is_none()); // dropped 0xFF + size mismatch
        assert!(decode_ziplist(&[]).is_none());
        assert!(decode_ziplist(&[0; 11]).is_none()); // zlbytes != len
    }

    #[test]
    fn zipmap_decodes_field_value_pairs() {
        // zmlen=2, then (len f1)(len free v1)(len f2)(len free v2) 0xFF.
        let blob = [
            0x02, 0x02, b'f', b'1', 0x02, 0x00, b'v', b'1', 0x02, b'f', b'2', 0x02, 0x00, b'v',
            b'2', 0xFF,
        ];
        let entries = decode_zipmap(&blob).expect("decode");
        assert_eq!(
            entries,
            vec![
                b"f1".to_vec(),
                b"v1".to_vec(),
                b"f2".to_vec(),
                b"v2".to_vec()
            ]
        );
    }

    #[test]
    fn zipmap_honours_free_padding() {
        // value "v" with 2 free padding bytes after it.
        let blob = [0x01, 0x01, b'k', 0x01, 0x02, b'v', 0xAA, 0xBB, 0xFF];
        let entries = decode_zipmap(&blob).expect("decode");
        assert_eq!(entries, vec![b"k".to_vec(), b"v".to_vec()]);
    }
}
