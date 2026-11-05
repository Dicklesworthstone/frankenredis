//! (frankenredis-rc-raptorq-gate-d-contract-71zfo) The §18 G6 durability drill
//! in miniature: generate, damage, scrub, recover — with every §9 output
//! asserted (generation manifest, scrub status, decode proof).

use fr_fec::{
    ScrubOutcome, decode_artifact, deserialize_symbol, encode_artifact, envelope_from_json,
    envelope_to_json, scrub, serialize_symbol,
};

const NOW: u64 = 1_788_600_000_000;

fn fixture_bytes() -> Vec<u8> {
    // Deterministic stand-in for a persisted artifact (e.g. a benchmark
    // baseline JSON): larger than one symbol so k > 1.
    let mut data = Vec::new();
    for i in 0..400_u32 {
        data.extend_from_slice(format!("{{\"entry\":{i},\"value\":\"{i:016x}\"}}\n").as_bytes());
    }
    data
}

fn corrupt(serialized: &[u8]) -> Vec<u8> {
    let mut bytes = serialized.to_vec();
    let at = bytes.len() / 2;
    bytes[at] ^= 0xFF;
    bytes
}

#[test]
fn generation_scrub_and_recovery_drill_round_trips() {
    let data = fixture_bytes();

    // GENERATION (§9 output 1): k source symbols + a repair budget.
    let encoded = encode_artifact("bench/set", "benchmark", &data, 4, 256, NOW).expect("encode");
    let k = encoded.envelope.raptorq.k as usize;
    assert!(k > 1, "fixture must span multiple symbols, k={k}");
    assert_eq!(encoded.symbols.len(), k + 4);
    assert_eq!(encoded.envelope.raptorq.symbol_hashes.len(), k + 4);

    // Round-trip through the serialized symbol form (the persisted form).
    let serialized: Vec<Vec<u8>> = encoded.symbols.iter().map(serialize_symbol).collect();
    let mut symbols: Vec<_> = serialized
        .iter()
        .map(|b| deserialize_symbol(b).unwrap())
        .collect();

    // SCRUB on an intact artifact (§9 output 2): Clean, nothing to repair.
    assert!(matches!(
        scrub(&encoded.envelope, &symbols, NOW),
        ScrubOutcome::Clean
    ));

    // Damage: lose the first source symbol and corrupt one repair symbol.
    symbols.remove(0);
    *symbols.last_mut().unwrap() =
        deserialize_symbol(&corrupt(&serialized[serialized.len() - 1])).unwrap();

    // SCRUB now reports damage; recovery from the remaining symbols works.
    match scrub(&encoded.envelope, &symbols, NOW + 1_000) {
        ScrubOutcome::Recovered { source, proof } => {
            assert_eq!(source, data, "recovered source must equal the original");
            assert_eq!(proof.recovered_blocks as usize, k);
            assert!(proof.reason.contains("hash-damaged"));
        }
        other => panic!("expected Recovered, got {other:?}"),
    }

    // DECODE PROOF (§9 output 3): explicit decode from the surviving set,
    // including the envelope round-trip through canonical JSON.
    let envelope_json = envelope_to_json(&encoded.envelope);
    let envelope = envelope_from_json(&envelope_json).expect("envelope reparse");
    let (source, proof) =
        decode_artifact(&envelope, &symbols, "forced-recovery drill", NOW + 2_000).expect("decode");
    assert_eq!(source, data);
    use sha2::{Digest, Sha256};
    assert_eq!(
        proof.proof_hash,
        format!("sha256:{}", hex::encode(Sha256::digest(&source)))
    );
    assert_eq!(envelope.decode_proofs.len(), 0);

    // Undecodable: below k intact symbols the artifact is honestly failed.
    let too_few: Vec<_> = symbols.iter().take(k - 1).cloned().collect();
    assert!(
        decode_artifact(&envelope, &too_few, "lost", NOW).is_err(),
        "fewer than k intact symbols must not decode"
    );
}

fn assert_both_decoders_fail(
    envelope: &fr_fec::FecEnvelope,
    symbols: &[raptorq::EncodingPacket],
    context: &str,
) {
    assert!(
        decode_artifact(envelope, symbols, context, NOW).is_err(),
        "{context}: unexpected decode success"
    );
    let outcome = scrub(envelope, symbols, NOW);
    assert!(
        matches!(&outcome, ScrubOutcome::Failed { .. }),
        "{context}: unexpected scrub result {outcome:?}"
    );
}

#[test]
fn damaged_first_source_payload_or_header_recovers_from_intact_repairs() {
    let data = fixture_bytes();
    let encoded = encode_artifact("damaged-source", "fixture", &data, 4, 256, NOW).expect("encode");
    let first = serialize_symbol(&encoded.symbols[0]);
    let mut damaged_header = first.clone();
    damaged_header[0] = u8::MAX;

    for (context, damaged_bytes) in [
        ("first source payload", corrupt(&first)),
        ("first source block ID", damaged_header),
    ] {
        let mut symbols = encoded.symbols.clone();
        symbols[0] = deserialize_symbol(&damaged_bytes).expect("deserialize damaged packet");

        match scrub(&encoded.envelope, &symbols, NOW) {
            ScrubOutcome::Recovered { source, proof } => {
                assert_eq!(source, data, "{context}");
                assert_eq!(proof.proof_hash, encoded.envelope.source_hash);
                assert!(proof.reason.contains("1 hash-damaged"));
            }
            outcome => panic!("{context}: expected recovery, got {outcome:?}"),
        }
        let (source, proof) =
            decode_artifact(&encoded.envelope, &symbols, context, NOW).expect("verified recovery");
        assert_eq!(source, data, "{context}");
        assert_eq!(proof.proof_hash, encoded.envelope.source_hash);
    }
}

#[test]
fn legacy_envelope_cannot_decode_hash_damaged_sources_without_enough_intact_packets() {
    let data = fixture_bytes();
    let encoded = encode_artifact("legacy-damage", "fixture", &data, 4, 256, NOW).expect("encode");
    let mut envelope = encoded.envelope.clone();
    envelope.source_hash.clear();
    let mut symbols = encoded.symbols[..envelope.raptorq.k as usize].to_vec();
    let damaged = corrupt(&serialize_symbol(&symbols[0]));
    symbols[0] = deserialize_symbol(&damaged).expect("deserialize damaged packet");

    assert_both_decoders_fail(
        &envelope,
        &symbols,
        "legacy envelope still requires intact packets",
    );
}

#[test]
fn recovered_source_must_match_the_envelope_source_hash() {
    let data = fixture_bytes();
    let mut encoded =
        encode_artifact("source-hash", "fixture", &data, 4, 256, NOW).expect("encode");
    encoded.envelope.source_hash = fr_fec::sha256_hex(b"a different artifact");
    assert_both_decoders_fail(
        &encoded.envelope,
        &encoded.symbols,
        "intact packets, wrong source hash",
    );

    let damaged = corrupt(&serialize_symbol(&encoded.symbols[0]));
    encoded.symbols[0] = deserialize_symbol(&damaged).expect("deserialize damaged packet");
    assert_both_decoders_fail(
        &encoded.envelope,
        &encoded.symbols,
        "repaired packets, wrong source hash",
    );
}

#[test]
fn a_duplicate_packet_cannot_hide_a_missing_repair_packet_from_scrub() {
    let data = fixture_bytes();
    let encoded = encode_artifact("duplicates", "fixture", &data, 4, 256, NOW).expect("encode");
    let mut symbols = encoded.symbols.clone();
    symbols.pop().expect("last repair packet");
    symbols.push(symbols[0].clone());
    assert_eq!(symbols.len(), encoded.symbols.len());

    match scrub(&encoded.envelope, &symbols, NOW) {
        ScrubOutcome::Recovered { source, proof } => {
            assert_eq!(source, data);
            assert!(proof.reason.contains("1 missing"));
        }
        outcome => panic!("duplicate hid a missing repair packet: {outcome:?}"),
    }
    let (source, _) =
        decode_artifact(&encoded.envelope, &symbols, "duplicate packet", NOW).expect("decode");
    assert_eq!(source, data);
}

#[test]
fn malformed_oti_padding_and_schema_fail_without_entering_the_codec() {
    let encoded =
        encode_artifact("invalid-oti", "fixture", b"OTI validation", 4, 256, NOW).expect("encode");

    for (context, offset, value) in [
        ("zero symbol size", 6, 0),
        ("zero source blocks", 8, 0),
        ("zero sub-blocks", 10, 0),
        ("zero alignment", 11, 0),
        ("incompatible alignment", 11, 3),
        ("excess sub-blocks", 9, u8::MAX),
        ("excess source blocks", 8, u8::MAX),
        ("oversized source block", 0, u8::MAX),
    ] {
        let mut envelope = encoded.envelope.clone();
        envelope.oti[offset] = value;
        assert_both_decoders_fail(&envelope, &encoded.symbols, context);
    }

    let mut envelope = encoded.envelope.clone();
    envelope.oti[..5].fill(0);
    assert_both_decoders_fail(&envelope, &encoded.symbols, "zero transfer length");

    let mut envelope = encoded.envelope.clone();
    envelope.padding = u32::MAX;
    assert_both_decoders_fail(
        &envelope,
        &encoded.symbols,
        "padding exceeds transfer length",
    );

    let mut envelope = encoded.envelope.clone();
    envelope.schema_version = "unsupported-fec-schema".to_string();
    assert_both_decoders_fail(&envelope, &encoded.symbols, "unsupported schema");
}

#[test]
fn hash_verified_packets_must_still_match_the_oti_layout() {
    let encoded = encode_artifact(
        "invalid-packet",
        "fixture",
        b"packet validation",
        4,
        64,
        NOW,
    )
    .expect("encode");
    let first = serialize_symbol(&encoded.symbols[0]);
    let mut short_payload = first.clone();
    short_payload.pop().expect("packet payload");
    let mut invalid_block = first;
    invalid_block[0] = u8::MAX;

    for (context, bytes) in [
        ("known hash with a short payload", short_payload),
        ("known hash with an invalid block", invalid_block),
    ] {
        let mut envelope = encoded.envelope.clone();
        envelope.raptorq.symbol_hashes[0] = fr_fec::sha256_hex(&bytes);
        let mut symbols = encoded.symbols.clone();
        symbols[0] = deserialize_symbol(&bytes).expect("deserialize");
        assert_both_decoders_fail(&envelope, &symbols, context);
    }
}

#[test]
fn legacy_unaligned_symbol_size_and_padding_remain_readable() {
    // The old encoder padded to the requested size of 65, while RaptorQ
    // used 64-byte symbols. Its valid padding can equal the actual size.
    let mut padded = vec![0_u8; 65];
    padded[0] = b'x';
    let encoder = raptorq::Encoder::with_defaults(&padded, 65);
    let symbols = encoder.get_encoded_packets(4);
    let mut envelope = encode_artifact("legacy-padding", "fixture", b"x", 4, 64, NOW)
        .expect("envelope template")
        .envelope;
    envelope.oti = encoder.get_config().serialize();
    envelope.symbol_size = 65;
    envelope.padding = 64;
    envelope.raptorq.k = 2;
    envelope.raptorq.overhead_ratio = 2.0;
    envelope.raptorq.symbol_hashes = symbols
        .iter()
        .map(|packet| fr_fec::sha256_hex(&serialize_symbol(packet)))
        .collect();

    for with_source_hash in [true, false] {
        let mut envelope = envelope.clone();
        if !with_source_hash {
            envelope.source_hash.clear();
        }
        let (source, _) =
            decode_artifact(&envelope, &symbols, "legacy padding", NOW).expect("decode legacy");
        assert_eq!(source, b"x");
        assert!(matches!(
            scrub(&envelope, &symbols, NOW),
            ScrubOutcome::Clean
        ));
    }
}

#[test]
fn legacy_multiblock_counter_bug_does_not_make_valid_packets_unreadable() {
    let data = b"abcd";
    let oti = raptorq::ObjectTransmissionInformation::new(4, 1, 2, 1, 1);
    let encoder = raptorq::Encoder::new(data, oti);
    let symbols = encoder.get_encoded_packets(4);
    let mut envelope = encode_artifact("legacy-blocks", "fixture", data, 4, 1, NOW)
        .expect("envelope template")
        .envelope;
    envelope.oti = oti.serialize();
    // The old encoder counted four total repairs instead of four per block.
    envelope.raptorq.k = 8;
    envelope.raptorq.repair_symbols = 4;
    envelope.raptorq.overhead_ratio = 0.5;
    envelope.raptorq.symbol_hashes = symbols
        .iter()
        .map(|packet| fr_fec::sha256_hex(&serialize_symbol(packet)))
        .collect();
    assert_eq!(symbols.len(), 12);

    let (source, _) =
        decode_artifact(&envelope, &symbols, "legacy counters", NOW).expect("decode legacy");
    assert_eq!(source, data);
    assert!(matches!(
        scrub(&envelope, &symbols, NOW),
        ScrubOutcome::Clean
    ));
}
