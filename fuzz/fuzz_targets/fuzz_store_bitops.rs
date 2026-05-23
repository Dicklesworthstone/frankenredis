#![no_main]

use arbitrary::{Arbitrary, Unstructured};
use fr_store::{Store, StoreError};
use libfuzzer_sys::fuzz_target;

const MAX_INPUT_LEN: usize = 4_096;
const MAX_OPS: usize = 64;
const MAX_BLOB_LEN: usize = 48;
const MAX_BIT_OFFSET: usize = 768;
const STRING_SLOT_COUNT: usize = 4;
const STRING_KEYS: [&[u8]; STRING_SLOT_COUNT] = [
    b"fuzz:bits:0",
    b"fuzz:bits:1",
    b"fuzz:bits:2",
    b"fuzz:bits:3",
];
const WRONG_TYPE_KEY: &[u8] = b"fuzz:bits:hash";
const MISSING_KEY: &[u8] = b"fuzz:bits:missing";
const ROUNDTRIP_KEY: &[u8] = b"fuzz:bits:roundtrip";

#[derive(Debug, Arbitrary)]
struct FuzzInput {
    ops: Vec<BitOpState>,
}

#[derive(Debug, Arbitrary)]
enum BitOpState {
    Seed {
        key: u8,
        value: Blob,
    },
    SetBit {
        key: u8,
        offset: u16,
        value: bool,
    },
    GetBit {
        key: u8,
        offset: u16,
    },
    BitfieldSet {
        key: u8,
        bit_offset: u16,
        width: Width,
        value: i64,
    },
    BitfieldGet {
        key: u8,
        bit_offset: u16,
        width: Width,
        signed: bool,
    },
    BitOp {
        op: BitOpKind,
        dest: u8,
        source_count: u8,
        first: SourceRef,
        second: SourceRef,
        third: SourceRef,
    },
    WrongType {
        op: WrongTypeOp,
        bit_offset: u16,
        width: Width,
        value: i64,
    },
    RoundTrip {
        key: u8,
    },
}

#[derive(Debug, Clone, Arbitrary)]
struct Blob(Vec<u8>);

#[derive(Debug, Clone, Copy, Arbitrary)]
enum Width {
    Bits1,
    Bits2,
    Bits4,
    Bits5,
    Bits8,
    Bits12,
    Bits16,
    Bits24,
    Bits32,
    Bits48,
    Bits64,
}

#[derive(Debug, Clone, Copy, Arbitrary)]
enum BitOpKind {
    And,
    Or,
    Xor,
    Not,
}

#[derive(Debug, Clone, Copy, Arbitrary)]
enum SourceRef {
    Slot(u8),
    Missing,
    WrongType,
}

#[derive(Debug, Clone, Copy, Arbitrary)]
enum WrongTypeOp {
    GetBit,
    SetBit,
    BitfieldGet,
    BitfieldSet,
}

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_LEN {
        return;
    }

    let mut unstructured = Unstructured::new(data);
    let Ok(input) = FuzzInput::arbitrary(&mut unstructured) else {
        return;
    };

    fuzz_store_bitops(input);
});

fn fuzz_store_bitops(input: FuzzInput) {
    let mut store = Store::new();
    store
        .hset(WRONG_TYPE_KEY, b"field".to_vec(), b"value".to_vec(), 0)
        .expect("wrong-type sentinel must initialize");
    let mut model = vec![None; STRING_SLOT_COUNT];
    let mut now_ms = 1_u64;

    for (step_index, op) in input.ops.into_iter().take(MAX_OPS).enumerate() {
        apply_op(&mut store, &mut model, op, now_ms);
        assert_store_matches_model(&mut store, &model, now_ms);
        if step_index % 8 == 7 {
            assert_round_trip(&mut store, &model, slot_index(step_index as u8), now_ms);
        }
        now_ms = now_ms.saturating_add(1 + (step_index % 5) as u64);
    }

    let _ = store.to_aof_commands(now_ms);
    assert_store_matches_model(&mut store, &model, now_ms);
    for slot in 0..STRING_SLOT_COUNT {
        assert_round_trip(&mut store, &model, slot, now_ms);
    }
}

fn apply_op(store: &mut Store, model: &mut [Option<Vec<u8>>], op: BitOpState, now_ms: u64) {
    match op {
        BitOpState::Seed { key, value } => {
            let slot = slot_index(key);
            let value = normalize_blob(value.0);
            store.set(slot_key(slot).to_vec(), value.clone(), None, now_ms);
            model[slot] = Some(value);
        }
        BitOpState::SetBit { key, offset, value } => {
            let slot = slot_index(key);
            let offset = normalize_bit_offset(offset);
            let before_digest = store.state_digest();
            let bytes = model[slot].get_or_insert_with(Vec::new);
            let (expected_old, changed) = shadow_setbit(bytes, offset, value);
            let actual = store.setbit(slot_key(slot), offset, value, now_ms);
            assert_eq!(actual, Ok(expected_old));
            if !changed {
                assert_eq!(before_digest, store.state_digest());
            }
        }
        BitOpState::GetBit { key, offset } => {
            let slot = slot_index(key);
            let offset = normalize_bit_offset(offset);
            let expected = shadow_getbit(model[slot].as_deref(), offset);
            let actual = store.getbit(slot_key(slot), offset, now_ms);
            assert_eq!(actual, Ok(expected));
        }
        BitOpState::BitfieldSet {
            key,
            bit_offset,
            width,
            value,
        } => {
            let slot = slot_index(key);
            let bit_offset = normalize_bit_offset(bit_offset) as u64;
            let width = width.bits();
            let before_digest = store.state_digest();
            let bytes = model[slot].get_or_insert_with(Vec::new);
            let (expected_old, changed) = shadow_bitfield_set(bytes, bit_offset, width, value);
            let actual = store.bitfield_set(slot_key(slot), bit_offset, width, value, now_ms);
            assert_eq!(actual, Ok(expected_old));
            if !changed {
                assert_eq!(before_digest, store.state_digest());
            }
        }
        BitOpState::BitfieldGet {
            key,
            bit_offset,
            width,
            signed,
        } => {
            let slot = slot_index(key);
            let bit_offset = normalize_bit_offset(bit_offset) as u64;
            let width = width.bits();
            let expected = shadow_bitfield_read(
                model[slot].as_deref().unwrap_or(&[]),
                bit_offset,
                width,
                signed,
            );
            let actual = store.bitfield_get(slot_key(slot), bit_offset, width, signed, now_ms);
            assert_eq!(actual, Ok(expected));
        }
        BitOpState::BitOp {
            op,
            dest,
            source_count,
            first,
            second,
            third,
        } => {
            let dest_slot = slot_index(dest);
            let sources = build_sources(source_count, [first, second, third]);
            let actual_source_keys: Vec<&[u8]> =
                sources.iter().copied().map(resolve_source_key).collect();
            let expected = shadow_bitop(model, op, &sources);
            let actual = store.bitop(
                bitop_name(op),
                slot_key(dest_slot),
                &actual_source_keys,
                now_ms,
            );
            match expected {
                Ok(bytes) => {
                    assert_eq!(actual, Ok(bytes.len()));
                    // Redis BITOP deletes dest when result is empty rather than
                    // storing an empty string (upstream bitops.c::bitopCommand).
                    if bytes.is_empty() {
                        model[dest_slot] = None;
                    } else {
                        model[dest_slot] = Some(bytes);
                    }
                }
                Err(err) => assert_eq!(actual, Err(err)),
            }
        }
        BitOpState::WrongType {
            op,
            bit_offset,
            width,
            value,
        } => {
            let bit_offset = normalize_bit_offset(bit_offset) as u64;
            let width = width.bits();
            let before_digest = store.state_digest();
            match op {
                WrongTypeOp::GetBit => {
                    assert_eq!(
                        store.getbit(WRONG_TYPE_KEY, bit_offset as usize, now_ms),
                        Err(StoreError::WrongType)
                    );
                }
                WrongTypeOp::SetBit => {
                    assert_eq!(
                        store.setbit(
                            WRONG_TYPE_KEY,
                            bit_offset as usize,
                            (value & 1) != 0,
                            now_ms
                        ),
                        Err(StoreError::WrongType)
                    );
                }
                WrongTypeOp::BitfieldGet => {
                    assert_eq!(
                        store.bitfield_get(WRONG_TYPE_KEY, bit_offset, width, false, now_ms),
                        Err(StoreError::WrongType)
                    );
                }
                WrongTypeOp::BitfieldSet => {
                    assert_eq!(
                        store.bitfield_set(WRONG_TYPE_KEY, bit_offset, width, value, now_ms),
                        Err(StoreError::WrongType)
                    );
                }
            }
            assert_eq!(before_digest, store.state_digest());
        }
        BitOpState::RoundTrip { key } => {
            assert_round_trip(store, model, slot_index(key), now_ms);
        }
    }
}

fn assert_store_matches_model(store: &mut Store, model: &[Option<Vec<u8>>], now_ms: u64) {
    for (slot, expected) in model.iter().enumerate() {
        let actual = store.get(slot_key(slot), now_ms);
        match expected {
            Some(bytes) => assert_eq!(actual, Ok(Some(bytes.clone()))),
            None => assert_eq!(actual, Ok(None)),
        }
    }
    assert_eq!(
        store.get(WRONG_TYPE_KEY, now_ms),
        Err(StoreError::WrongType)
    );
    assert_eq!(store.get(MISSING_KEY, now_ms), Ok(None));
}

fn assert_round_trip(store: &mut Store, model: &[Option<Vec<u8>>], slot: usize, now_ms: u64) {
    let payload = store.dump_key(slot_key(slot), now_ms);
    match &model[slot] {
        Some(bytes) => {
            let payload = payload.expect("present string keys must dump");
            let mut restored = Store::new();
            restored
                .restore_key(ROUNDTRIP_KEY, 0, &payload, false, now_ms)
                .expect("self-generated string dump must restore");
            assert_eq!(restored.get(ROUNDTRIP_KEY, now_ms), Ok(Some(bytes.clone())));
        }
        None => assert!(payload.is_none()),
    }
}

fn normalize_blob(mut value: Vec<u8>) -> Vec<u8> {
    value.truncate(MAX_BLOB_LEN);
    value
}

fn slot_index(raw: u8) -> usize {
    usize::from(raw) % STRING_SLOT_COUNT
}

fn slot_key(slot: usize) -> &'static [u8] {
    STRING_KEYS[slot]
}

fn normalize_bit_offset(offset: u16) -> usize {
    usize::from(offset) % MAX_BIT_OFFSET
}

fn build_sources(source_count: u8, candidates: [SourceRef; 3]) -> Vec<SourceRef> {
    let count = usize::from(source_count % 4);
    candidates.into_iter().take(count).collect()
}

fn resolve_source_key(source: SourceRef) -> &'static [u8] {
    match source {
        SourceRef::Slot(slot) => slot_key(slot_index(slot)),
        SourceRef::Missing => MISSING_KEY,
        SourceRef::WrongType => WRONG_TYPE_KEY,
    }
}

fn bitop_name(op: BitOpKind) -> &'static [u8] {
    match op {
        BitOpKind::And => b"AND",
        BitOpKind::Or => b"OR",
        BitOpKind::Xor => b"XOR",
        BitOpKind::Not => b"NOT",
    }
}

fn shadow_bitop(
    model: &[Option<Vec<u8>>],
    op: BitOpKind,
    sources: &[SourceRef],
) -> Result<Vec<u8>, StoreError> {
    let mut values = Vec::with_capacity(sources.len());
    for source in sources {
        match source {
            SourceRef::Slot(slot) => {
                values.push(model[slot_index(*slot)].clone().unwrap_or_default());
            }
            SourceRef::Missing => values.push(Vec::new()),
            SourceRef::WrongType => return Err(StoreError::WrongType),
        }
    }

    let max_len = values.iter().map(Vec::len).max().unwrap_or(0);
    let mut result = vec![0_u8; max_len];
    match op {
        BitOpKind::Not => {
            if values.len() != 1 {
                return Err(StoreError::WrongType);
            }
            for (index, byte) in result.iter_mut().enumerate() {
                *byte = !values[0].get(index).copied().unwrap_or(0);
            }
        }
        BitOpKind::And | BitOpKind::Or | BitOpKind::Xor => {
            if let Some(first) = values.first() {
                for (index, byte) in result.iter_mut().enumerate() {
                    *byte = first.get(index).copied().unwrap_or(0);
                }
            }
            for value in values.iter().skip(1) {
                for (index, byte) in result.iter_mut().enumerate() {
                    let next = value.get(index).copied().unwrap_or(0);
                    match op {
                        BitOpKind::And => *byte &= next,
                        BitOpKind::Or => *byte |= next,
                        BitOpKind::Xor => *byte ^= next,
                        BitOpKind::Not => unreachable!(),
                    }
                }
            }
        }
    }
    Ok(result)
}

// Shadow helpers mirror fr-store's MSB-first bitmap semantics so the harness can
// assert exact bytes and field values after each mutation.
fn shadow_getbit(bytes: Option<&[u8]>, offset: usize) -> bool {
    let Some(bytes) = bytes else {
        return false;
    };
    let byte_idx = offset / 8;
    let bit_idx = 7 - (offset % 8);
    if byte_idx >= bytes.len() {
        false
    } else {
        ((bytes[byte_idx] >> bit_idx) & 1) == 1
    }
}

fn shadow_setbit(bytes: &mut Vec<u8>, offset: usize, value: bool) -> (bool, bool) {
    let byte_idx = offset / 8;
    let bit_idx = 7 - (offset % 8);
    let old_len = bytes.len();
    if bytes.len() <= byte_idx {
        bytes.resize(byte_idx + 1, 0);
    }
    let old_bit = ((bytes[byte_idx] >> bit_idx) & 1) == 1;
    if value {
        bytes[byte_idx] |= 1 << bit_idx;
    } else {
        bytes[byte_idx] &= !(1 << bit_idx);
    }
    let changed = old_len != bytes.len() || old_bit != value;
    (old_bit, changed)
}

fn shadow_bitfield_read(bytes: &[u8], bit_offset: u64, bits: u8, signed: bool) -> i64 {
    if bits == 0 {
        return 0;
    }
    let mut value: u64 = 0;
    for bit in 0..u64::from(bits) {
        let pos = bit_offset.wrapping_add(bit);
        let byte_idx = (pos / 8) as usize;
        let bit_idx = 7 - (pos % 8) as u8;
        let bit_val = if byte_idx < bytes.len() {
            (bytes[byte_idx] >> bit_idx) & 1
        } else {
            0
        };
        value = (value << 1) | u64::from(bit_val);
    }
    if signed && bits < 64 {
        let sign_bit = 1_u64 << (bits - 1);
        if value & sign_bit != 0 {
            value |= u64::MAX << bits;
        }
    }
    value as i64
}

fn shadow_bitfield_set(bytes: &mut Vec<u8>, bit_offset: u64, bits: u8, value: i64) -> (i64, bool) {
    let old_value = shadow_bitfield_read(bytes, bit_offset, bits, false);
    let end_bit = bit_offset.saturating_add(u64::from(bits));
    let needed_bytes = end_bit.div_ceil(8) as usize;
    let old_len = bytes.len();
    if bytes.len() < needed_bytes {
        bytes.resize(needed_bytes, 0);
    }
    shadow_bitfield_write(bytes, bit_offset, bits, value);
    let changed =
        old_len != bytes.len() || old_value != shadow_bitfield_read(bytes, bit_offset, bits, false);
    (old_value, changed)
}

fn shadow_bitfield_write(bytes: &mut [u8], bit_offset: u64, bits: u8, value: i64) {
    let value = value as u64;
    for bit in 0..u64::from(bits) {
        let pos = bit_offset.wrapping_add(bit);
        let byte_idx = (pos / 8) as usize;
        let bit_idx = 7 - (pos % 8) as u8;
        let value_bit = (value >> (u64::from(bits) - 1 - bit)) & 1;
        if value_bit == 1 {
            bytes[byte_idx] |= 1 << bit_idx;
        } else {
            bytes[byte_idx] &= !(1 << bit_idx);
        }
    }
}

impl Width {
    fn bits(self) -> u8 {
        match self {
            Self::Bits1 => 1,
            Self::Bits2 => 2,
            Self::Bits4 => 4,
            Self::Bits5 => 5,
            Self::Bits8 => 8,
            Self::Bits12 => 12,
            Self::Bits16 => 16,
            Self::Bits24 => 24,
            Self::Bits32 => 32,
            Self::Bits48 => 48,
            Self::Bits64 => 64,
        }
    }
}
