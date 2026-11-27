//! # fr-fec — RaptorQ-Everywhere durability sidecar (spec §9/§19)
//!
//! Systematic forward-error-correction envelopes for FrankenRedis evidence
//! artifacts: conformance fixture bundles, benchmark baselines, migration
//! manifests, reproducibility ledgers, and release-grade state artifacts.
//!
//! The codec is the `raptorq` crate (RFC 6330, systematic): source packets
//! retain the original symbols, and each source block also has repair packets.
//! The FEC layer pays off when symbols are lost or corrupted, which is the
//! durability drill (spec §18 G6).
//!
//! Envelope schema follows spec §19 (`frankenredis_fec_envelope/v1`) with one
//! documented normalization: §19's illustrative `"blake3:..."` hash prefix is
//! emitted as `"sha256:..."` — SHA-256 is the repo-wide hash convention
//! (BENCH_METHODOLOGY, ELF identification) and this crate follows it instead
//! of introducing a second digest family.
#![forbid(unsafe_code)]

use raptorq::Encoder;
use raptorq::EncodingPacket;
use raptorq::ObjectTransmissionInformation;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

pub const ENVELOPE_SCHEMA_VERSION: &str = "frankenredis_fec_envelope/v1";
pub const CHUNKED_ENVELOPE_SCHEMA_VERSION: &str = "frankenredis_fec_envelope/v2";
pub const GENERATION_SCHEMA_VERSION: &str = "frankenredis_fec_generation/v1";
pub const IN_PLACE_RECOVERY_SCHEMA_VERSION: &str = "frankenredis_fec_in_place_recovery/v1";
pub const CODEC_ID_RAPTORQ_1_8_1: &str = "raptorq-1.8.1";
pub const GENERATION_AUTH_SCHEME_HMAC_SHA256: &str = "hmac-sha256-v1";
pub const DEFAULT_CHUNK_SIZE_BYTES: u64 = 16 * 1024 * 1024;
const V2_SYMBOL_STREAM_MAGIC: &[u8; 8] = b"FRFQ2\0\0\0";

/// Authenticated generation metadata embedded inside an `FecEnvelope`, binding
/// the sidecar to a specific committed primary generation.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SidecarGenerationBinding {
    pub generation_id: String,
    pub generation_seq: u64,
    pub primary_len: u64,
    pub auth_scheme: String,
    pub auth_mac: String,
}

/// Per-chunk RaptorQ layout descriptor for bounded-memory streaming `v2` sidecars.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ChunkDescriptor {
    pub index: u32,
    pub byte_offset: u64,
    pub byte_len: u64,
    pub source_hash: String,
    pub padding: u32,
    pub symbol_size: u16,
    pub oti: [u8; 12],
    pub k: u32,
    pub repair_symbols: u32,
    pub symbol_stream_offset: u64,
    pub symbol_stream_bytes: u64,
    pub symbol_hash_start: usize,
    pub symbol_hash_count: usize,
}

/// Chunked streaming layout for artifacts encoded under `frankenredis_fec_envelope/v2`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ChunkedLayout {
    pub chunk_size: u64,
    pub total_source_bytes: u64,
    pub chunks: Vec<ChunkDescriptor>,
}

/// Configuration for bounded-memory chunked encoding (`v2`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkedEncodeOptions {
    pub chunk_size: u64,
    pub repair_symbols: usize,
    pub symbol_size: u16,
}

/// Authenticated primary generation pointer stored atomically alongside a
/// persisted artifact (`<artifact>.generation.json`).
///
/// The single atomic rename of this record commits the primary generation and
/// (in phase 2) binds the `.envelope.json` and `.symbols` sidecars together.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct GenerationRecord {
    pub schema_version: String,
    pub artifact_id: String,
    pub generation_id: String,
    pub generation_seq: u64,
    pub primary_sha256: String,
    pub primary_len: u64,
    pub fec_enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidecar_generation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidecar_envelope_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidecar_symbols_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec_id: Option<String>,
    pub updated_unix_ms: u64,
    pub auth_scheme: String,
    pub auth_mac: String,
}

/// Verified result of authenticating `<artifact>.generation.json` against its
/// sidecar envelope.
#[derive(Clone, Debug, PartialEq)]
pub struct VerifiedGenerationBinding {
    pub record: GenerationRecord,
    pub envelope: FecEnvelope,
    pub symbols_file_intact: bool,
}

/// Auditable proof written after an atomic `fsync`ed in-place recovery.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct InPlaceRecoveryProof {
    pub schema_version: String,
    pub source_artifact: String,
    pub generation_id: String,
    pub generation_seq: u64,
    pub primary_generation_verified: bool,
    pub codec_id: String,
    pub preserved_corrupt_primary_path: String,
    pub preserved_corrupt_primary_sha256: String,
    pub preserved_corrupt_primary_bytes: u64,
    pub recovered_primary_sha256: String,
    pub recovered_primary_bytes: u64,
    pub proof_path: String,
    pub decode_proof: DecodeProof,
}

/// Result of safely retiring superseded sidecar files out of active paths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetiredSidecarPaths {
    pub retired_envelope: Option<std::path::PathBuf>,
    pub retired_symbols: Option<std::path::PathBuf>,
}

/// One recovery event, per spec §19 `decode_proofs[]`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct DecodeProof {
    pub ts_unix_ms: u64,
    /// Why recovery ran (e.g. "scrub: 2 source symbols failed hash check").
    pub reason: String,
    pub recovered_blocks: u32,
    /// SHA-256 over the recovered source, binding the proof to the exact
    /// bytes and envelope that produced it.
    pub proof_hash: String,
}

/// Per spec §19 `scrub{}`: `ok` = all symbols hash-verified; `recovered` = a
/// decode ran and the source hash re-verified; `failed` = verification failed.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ScrubStatus {
    pub last_ok_unix_ms: u64,
    pub status: String,
}

/// Per spec §19 `raptorq{}`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct RaptorQSection {
    /// Source symbol count (RFC 6330 `K`).
    pub k: u32,
    pub repair_symbols: u32,
    /// `repair_symbols / k`.
    pub overhead_ratio: f64,
    /// SHA-256 of each serialized symbol in emitted packet order.
    pub symbol_hashes: Vec<String>,
}

/// Per spec §19, with `padding` and `oti` added: RFC 6330 encodes in whole
/// symbols, so the source is zero-padded to a symbol boundary and the decoder
/// must strip exactly `padding` trailing bytes. `oti` is the RFC 6330 Object
/// Transmission Information exactly as the encoder used it (serialized via
/// `ObjectTransmissionInformation::serialize`, 12 bytes), so symbol size and
/// transfer length can never drift between encode and decode.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct FecEnvelope {
    pub schema_version: String,
    pub artifact_id: String,
    pub artifact_type: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<SidecarGenerationBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunked: Option<ChunkedLayout>,
    pub padding: u32,
    pub symbol_size: u16,
    pub oti: [u8; 12],
    pub raptorq: RaptorQSection,
    pub scrub: ScrubStatus,
    pub decode_proofs: Vec<DecodeProof>,
}

pub type FecSidecarEnvelope = FecEnvelope;

/// The encoded form of an artifact: the envelope plus every encoding packet.
/// Each source block's source packets are followed by its repair packets.
#[derive(Clone, Debug)]
pub struct EncodedArtifact {
    pub envelope: FecEnvelope,
    pub symbols: Vec<EncodingPacket>,
}

#[must_use]
pub fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    format!("sha256:{}", hex::encode(digest))
}

/// Pure-Rust RFC 2104 HMAC-SHA256 formatted as `"hmac-sha256:<hex>"`.
#[must_use]
pub fn hmac_sha256_hex(key: &[u8], message: &[u8]) -> String {
    const BLOCK_SIZE: usize = 64;
    let mut key_block = [0_u8; BLOCK_SIZE];
    if key.len() > BLOCK_SIZE {
        let hashed = Sha256::digest(key);
        key_block[..hashed.len()].copy_from_slice(&hashed);
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }

    let mut ipad = [0x36_u8; BLOCK_SIZE];
    let mut opad = [0x5c_u8; BLOCK_SIZE];
    for idx in 0..BLOCK_SIZE {
        ipad[idx] ^= key_block[idx];
        opad[idx] ^= key_block[idx];
    }

    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(message);
    let inner_digest = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner_digest);
    format!("hmac-sha256:{}", hex::encode(outer.finalize()))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0_u8;
    for (&lhs, &rhs) in a.iter().zip(b.iter()) {
        diff |= lhs ^ rhs;
    }
    diff == 0
}

fn validate_codec_id(codec_id: Option<&str>) -> Result<(), String> {
    match codec_id {
        None | Some(CODEC_ID_RAPTORQ_1_8_1) => Ok(()),
        Some(other) => Err(format!(
            "unsupported FEC codec_id '{other}'; this build supports only '{CODEC_ID_RAPTORQ_1_8_1}' (see docs/RAPTORQ_2X_MIGRATION.md)"
        )),
    }
}

/// Hash a reader with bounded memory, returning the byte count and the same
/// SHA-256 representation as `sha256_hex`. I/O failures are never treated as EOF.
pub fn sha256_reader(mut reader: impl std::io::Read) -> std::io::Result<(u64, String)> {
    let mut digest = Sha256::new();
    let mut length = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        length = length
            .checked_add(count as u64)
            .ok_or_else(|| std::io::Error::other("hashed input length overflows u64"))?;
        digest.update(&buffer[..count]);
    }
    Ok((length, format!("sha256:{}", hex::encode(digest.finalize()))))
}

// RFC 6330 source blocks contain at most K'_max symbols. RaptorQ 1.8.1
// stores the number of source blocks in a u8 and each encoding symbol ID
// in 24 bits. Validate these limits before entering its infallible API.
const MAX_SOURCE_SYMBOLS_PER_BLOCK: u32 = 56_403;
const ENCODING_SYMBOL_ID_LIMIT: u32 = 1 << 24;

#[derive(Debug)]
struct EncodingParameters {
    oti: ObjectTransmissionInformation,
    padded_len: usize,
    padding: u32,
    source_symbols: u32,
    repairs_per_block: u32,
    total_repair_symbols: u32,
}

fn encoding_parameters(
    data_len: usize,
    repair_symbols: usize,
    symbol_size: u16,
) -> Result<EncodingParameters, String> {
    if repair_symbols == 0 {
        return Err("repair_symbols must be >= 1".to_string());
    }
    if symbol_size == 0 {
        return Err("symbol_size must be >= 1".to_string());
    }
    let repairs_per_block = u32::try_from(repair_symbols)
        .map_err(|_| "repair_symbols exceeds the RaptorQ encoding symbol ID limit".to_string())?;

    // RaptorQ treats this argument as a maximum packet size and uses
    // eight-byte symbol alignment for packet sizes of at least 64.
    let symbol_size = if symbol_size >= 64 {
        symbol_size - symbol_size % 8
    } else {
        symbol_size
    };
    let symbol_bytes = usize::from(symbol_size);
    let padding = if data_len == 0 {
        // Encode an empty artifact as one zero symbol, then strip it on
        // decode. A zero-length RaptorQ object has zero source blocks.
        symbol_bytes
    } else {
        (symbol_bytes - data_len % symbol_bytes) % symbol_bytes
    };
    let padded_len = data_len
        .checked_add(padding)
        .ok_or_else(|| "artifact length overflows after symbol padding".to_string())?;
    let source_symbols = u32::try_from(padded_len / symbol_bytes)
        .map_err(|_| "artifact source symbol count exceeds the RaptorQ format limit".to_string())?;
    let max_source_symbols = u32::from(u8::MAX) * MAX_SOURCE_SYMBOLS_PER_BLOCK;
    if source_symbols > max_source_symbols {
        return Err(format!(
            "{source_symbols} source symbols exceed the RaptorQ limit of \
             {max_source_symbols} across 255 source blocks"
        ));
    }

    let transfer_length = u64::try_from(padded_len)
        .map_err(|_| "artifact length exceeds the RaptorQ transfer length limit".to_string())?;
    let oti = ObjectTransmissionInformation::with_defaults(transfer_length, symbol_size);
    let source_blocks = u32::from(oti.source_blocks());
    if source_blocks == 0 || source_symbols.div_ceil(source_blocks) > MAX_SOURCE_SYMBOLS_PER_BLOCK {
        return Err("RaptorQ produced an unsupported source block layout".to_string());
    }

    // Repair IDs start at the extended source-block size, not at K.
    // Check the largest block, since the final ID must remain below 2^24.
    let repair_start =
        raptorq::extended_source_block_symbols(source_symbols.div_ceil(source_blocks));
    let max_repairs_per_block = ENCODING_SYMBOL_ID_LIMIT - repair_start;
    if repairs_per_block > max_repairs_per_block {
        return Err(format!(
            "{repairs_per_block} repair symbols per source block exceed the \
             RaptorQ limit of {max_repairs_per_block} for this artifact"
        ));
    }
    let total_repair_symbols = repairs_per_block
        .checked_mul(source_blocks)
        .ok_or_else(|| "total repair symbol count overflows the envelope".to_string())?;
    source_symbols
        .checked_add(total_repair_symbols)
        .ok_or_else(|| "total symbol count overflows the symbol stream".to_string())?;

    Ok(EncodingParameters {
        oti,
        padded_len,
        padding: padding as u32,
        source_symbols,
        repairs_per_block,
        total_repair_symbols,
    })
}

/// Wrap data in a systematic RaptorQ encoding.
///
/// The repair_symbols argument is the repair budget for each source block
/// and must be >= 1. The envelope records the total source and repair counts
/// across all blocks. RaptorQ rounds packet-size limits of 64 or more down
/// to a multiple of eight; the envelope records the actual symbol size.
///
/// Inputs outside the current codec's source-block or encoding-symbol-ID
/// limits return an error before allocating the padded artifact or entering
/// the encoder. Empty artifacts round-trip through one zero-padded symbol.
pub fn encode_artifact(
    artifact_id: &str,
    artifact_type: &str,
    data: &[u8],
    repair_symbols: usize,
    symbol_size: u16,
    now_unix_ms: u64,
) -> Result<EncodedArtifact, String> {
    let parameters = encoding_parameters(data.len(), repair_symbols, symbol_size)?;
    let mut padded = Vec::new();
    padded
        .try_reserve_exact(parameters.padded_len)
        .map_err(|err| format!("allocate padded artifact: {err}"))?;
    padded.extend_from_slice(data);
    padded.resize(parameters.padded_len, 0);

    let encoder = Encoder::new(&padded, parameters.oti);
    let packets = encoder.get_encoded_packets(parameters.repairs_per_block);
    let expected_packets =
        u64::from(parameters.source_symbols) + u64::from(parameters.total_repair_symbols);
    if u64::try_from(packets.len()).ok() != Some(expected_packets) {
        return Err("RaptorQ returned an unexpected source or repair symbol count".to_string());
    }

    let mut symbol_hashes = Vec::with_capacity(packets.len());
    for packet in &packets {
        symbol_hashes.push(sha256_hex(&packet.serialize()));
    }

    Ok(EncodedArtifact {
        envelope: FecEnvelope {
            schema_version: ENVELOPE_SCHEMA_VERSION.to_string(),
            artifact_id: artifact_id.to_string(),
            artifact_type: artifact_type.to_string(),
            source_hash: sha256_hex(data),
            codec_id: Some(CODEC_ID_RAPTORQ_1_8_1.to_string()),
            generation: None,
            chunked: None,
            padding: parameters.padding,
            symbol_size: parameters.oti.symbol_size(),
            oti: parameters.oti.serialize(),
            raptorq: RaptorQSection {
                k: parameters.source_symbols,
                repair_symbols: parameters.total_repair_symbols,
                overhead_ratio: f64::from(parameters.total_repair_symbols)
                    / f64::from(parameters.source_symbols),
                symbol_hashes,
            },
            scrub: ScrubStatus {
                last_ok_unix_ms: now_unix_ms,
                status: "ok".to_string(),
            },
            decode_proofs: Vec::new(),
        },
        symbols: packets,
    })
}

/// SCRUB (§9 output 2): every retained symbol is hash-checked against the
/// envelope, and the source is re-decoded and re-hashed.
///
/// `Clean` means every expected packet hash is present, no supplied packet
/// fails its hash check, and the reconstructed source passes its source hash
/// when present. `Recovered` means missing or damaged packets were bypassed
/// using intact packets; the caller persists the recovered source and proof.
/// `Failed` means invalid metadata, insufficient intact packets, or a source
/// hash mismatch prevented verified recovery.
#[derive(Debug)]
pub enum ScrubOutcome {
    Clean,
    Recovered { source: Vec<u8>, proof: DecodeProof },
    Failed { reason: String },
}

struct VerifiedDecode {
    source: Vec<u8>,
    source_hash: String,
    missing: usize,
    damaged: usize,
}

fn decode_verified_symbols(
    envelope: &FecEnvelope,
    symbols: &[EncodingPacket],
) -> Result<VerifiedDecode, String> {
    if envelope.schema_version != ENVELOPE_SCHEMA_VERSION {
        return Err(format!(
            "unsupported FEC envelope schema {}",
            envelope.schema_version
        ));
    }
    validate_codec_id(envelope.codec_id.as_deref())?;
    let oti = ObjectTransmissionInformation::deserialize(&envelope.oti);
    let symbol_size = usize::from(oti.symbol_size());
    let source_blocks = u64::from(oti.source_blocks());
    if symbol_size == 0 || source_blocks == 0 || oti.transfer_length() == 0 {
        return Err("invalid OTI: zero transfer length, symbol size, or source blocks".to_string());
    }
    let alignment = u16::from(oti.symbol_alignment());
    if alignment == 0
        || !oti.symbol_size().is_multiple_of(alignment)
        || oti.sub_blocks() == 0
        || oti.sub_blocks() > oti.symbol_size() / alignment
    {
        return Err("invalid OTI: inconsistent symbol alignment or sub-block count".to_string());
    }
    let source_symbols = oti.transfer_length().div_ceil(symbol_size as u64);
    if source_symbols < source_blocks
        || source_symbols.div_ceil(source_blocks) > u64::from(MAX_SOURCE_SYMBOLS_PER_BLOCK)
    {
        return Err("invalid OTI: unsupported source block layout".to_string());
    }
    if source_symbols * symbol_size as u64 > isize::MAX as u64 {
        return Err("decoded artifact exceeds the platform allocation limit".to_string());
    }
    let transfer_length = usize::try_from(oti.transfer_length())
        .map_err(|_| "decoded artifact length exceeds this platform".to_string())?;
    if u64::from(envelope.padding) > oti.transfer_length() {
        return Err("invalid envelope padding: exceeds the transfer length".to_string());
    }
    let padding = usize::try_from(envelope.padding)
        .map_err(|_| "envelope padding exceeds this platform".to_string())?;
    let minimum_packets = usize::try_from(source_symbols)
        .map_err(|_| "source symbol count exceeds this platform".to_string())?;

    // OTI describes the actual layout. Older envelopes can record the
    // requested packet size or incorrect multi-block counters instead.
    let expected: std::collections::HashSet<&str> = envelope
        .raptorq
        .symbol_hashes
        .iter()
        .map(String::as_str)
        .collect();
    let mut present = std::collections::HashSet::new();
    let mut intact = Vec::new();
    let mut damaged = 0_usize;
    for packet in symbols {
        let digest = sha256_hex(&packet.serialize());
        let Some(&expected_hash) = expected.get(digest.as_str()) else {
            damaged += 1;
            continue;
        };
        // Hash failures are erasures, including damaged packet headers.
        // A known packet that contradicts OTI makes the envelope invalid.
        if packet.data().len() != symbol_size {
            return Err("hash-verified packet length does not match OTI symbol size".to_string());
        }
        if u64::from(packet.payload_id().source_block_number()) >= source_blocks {
            return Err("hash-verified packet source block is outside the OTI layout".to_string());
        }
        if present.insert(expected_hash) {
            intact.push(packet);
        }
    }
    let missing = expected.len().saturating_sub(present.len());
    let undecodable = || {
        format!("undecodable: not enough intact symbols; {missing} missing, {damaged} hash-damaged")
    };
    if intact.len() < minimum_packets {
        return Err(undecodable());
    }

    let mut decoder = raptorq::Decoder::new(oti);
    let mut reconstructed = None;
    for packet in intact {
        if let Some(source) = decoder.decode(packet.clone()) {
            reconstructed = Some(source);
            break;
        }
    }
    let Some(mut source) = reconstructed else {
        return Err(undecodable());
    };
    if source.len() != transfer_length {
        return Err("decoded source length does not match OTI transfer length".to_string());
    }
    let source_len = source
        .len()
        .checked_sub(padding)
        .ok_or_else(|| "invalid envelope padding: exceeds the decoded source".to_string())?;
    source.truncate(source_len);
    let source_hash = sha256_hex(&source);
    // Legacy envelopes without a whole-source hash still require every
    // packet admitted to the decoder to match its recorded packet hash.
    if !envelope.source_hash.is_empty() && source_hash != envelope.source_hash {
        return Err(format!(
            "recovered source hash {source_hash} does not match envelope.source_hash {}",
            envelope.source_hash
        ));
    }

    Ok(VerifiedDecode {
        source,
        source_hash,
        missing,
        damaged,
    })
}

pub fn scrub(envelope: &FecEnvelope, symbols: &[EncodingPacket], now_unix_ms: u64) -> ScrubOutcome {
    let decoded = match decode_verified_symbols(envelope, symbols) {
        Ok(decoded) => decoded,
        Err(reason) => return ScrubOutcome::Failed { reason },
    };
    scrub_decoded(envelope, decoded, now_unix_ms)
}

fn scrub_decoded(
    envelope: &FecEnvelope,
    decoded: VerifiedDecode,
    now_unix_ms: u64,
) -> ScrubOutcome {
    if decoded.damaged == 0 && decoded.missing == 0 {
        ScrubOutcome::Clean
    } else {
        let reason = format!(
            "scrub repaired artifact: {} missing intact, {} hash-damaged symbols",
            decoded.missing, decoded.damaged
        );
        let proof = DecodeProof {
            ts_unix_ms: now_unix_ms,
            proof_hash: decoded.source_hash,
            reason,
            recovered_blocks: envelope.raptorq.k,
        };
        ScrubOutcome::Recovered {
            source: decoded.source,
            proof,
        }
    }
}

/// DECODE PROOF (§9 output 3): recover the source using only hash-verified
/// packets, verify it against the envelope, and return the source with a
/// proof ready to append to envelope.decode_proofs before re-persisting it.
pub fn decode_artifact(
    envelope: &FecEnvelope,
    symbols: &[EncodingPacket],
    reason: &str,
    now_unix_ms: u64,
) -> Result<(Vec<u8>, DecodeProof), String> {
    let decoded = decode_verified_symbols(envelope, symbols)?;
    let proof = DecodeProof {
        ts_unix_ms: now_unix_ms,
        reason: reason.to_string(),
        recovered_blocks: envelope.raptorq.k,
        proof_hash: decoded.source_hash,
    };
    Ok((decoded.source, proof))
}

/// Serialize an envelope to the canonical pretty JSON form.
pub fn envelope_to_json(envelope: &FecEnvelope) -> String {
    serde_json::to_string_pretty(envelope).unwrap_or_else(|_| "{}".to_string())
}

/// Parse a canonical envelope JSON.
pub fn envelope_from_json(json: &str) -> Result<FecEnvelope, String> {
    serde_json::from_str(json).map_err(|err| format!("envelope parse: {err}"))
}

/// Serialize one symbol to its on-disk/wire bytes, so artifact classes never
/// touch the `raptorq` types for persistence.
pub fn serialize_symbol(packet: &EncodingPacket) -> Vec<u8> {
    packet.serialize()
}

/// Inverse of [`serialize_symbol`].
pub fn deserialize_symbol(bytes: &[u8]) -> Result<EncodingPacket, String> {
    if bytes.len() < 4 {
        return Err("truncated symbol: cannot read the four-byte payload ID".to_string());
    }
    Ok(raptorq::EncodingPacket::deserialize(bytes))
}

/// Serialize a collection of encoding packets into a self-delimited byte stream.
pub fn serialize_symbol_stream(packets: &[EncodingPacket]) -> Vec<u8> {
    let mut out = Vec::new();
    let count = u32::try_from(packets.len()).unwrap_or(0);
    out.extend_from_slice(&count.to_le_bytes());
    for packet in packets {
        let bytes = packet.serialize();
        let len = u32::try_from(bytes.len()).unwrap_or(0);
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&bytes);
    }
    out
}

/// Deserialize a collection of encoding packets from a self-delimited byte stream.
pub fn deserialize_symbol_stream(mut bytes: &[u8]) -> Result<Vec<EncodingPacket>, String> {
    if bytes.len() < 4 {
        return Err("truncated symbol stream: cannot read count header".to_string());
    }
    let count = u32::from_le_bytes(bytes[..4].try_into().map_err(|e| format!("{e}"))?) as usize;
    bytes = &bytes[4..];
    // Each entry needs a four-byte length and a four-byte payload ID.
    // Check the claimed count before allocating from an untrusted header.
    if count > bytes.len() / 8 {
        return Err("invalid symbol stream count: not enough bytes for packet headers".to_string());
    }
    let mut packets = Vec::new();
    packets
        .try_reserve_exact(count)
        .map_err(|err| format!("allocate symbol stream: {err}"))?;
    for idx in 0..count {
        if bytes.len() < 4 {
            return Err(format!("truncated symbol stream at entry {idx}"));
        }
        let len = u32::from_le_bytes(bytes[..4].try_into().map_err(|e| format!("{e}"))?) as usize;
        bytes = &bytes[4..];
        if bytes.len() < len {
            return Err(format!(
                "truncated symbol data for entry {idx} (need {len}, have {})",
                bytes.len()
            ));
        }
        let packet_bytes = &bytes[..len];
        bytes = &bytes[len..];
        packets.push(
            deserialize_symbol(packet_bytes)
                .map_err(|err| format!("invalid symbol stream entry {idx}: {err}"))?,
        );
    }
    if !bytes.is_empty() {
        return Err("trailing bytes after symbol stream".to_string());
    }
    Ok(packets)
}

/// Sidecar file paths for an artifact.
pub fn sidecar_paths(
    artifact_path: impl AsRef<std::path::Path>,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let p = artifact_path.as_ref();
    let envelope = append_path_suffix(p, ".envelope.json");
    let symbols = append_path_suffix(p, ".symbols");
    (envelope, symbols)
}

fn append_path_suffix(path: &std::path::Path, suffix: &str) -> std::path::PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    std::path::PathBuf::from(name)
}

/// Path of the authenticated primary generation pointer for an artifact.
pub fn generation_path(artifact_path: impl AsRef<std::path::Path>) -> std::path::PathBuf {
    append_path_suffix(artifact_path.as_ref(), ".generation.json")
}

/// Path of the directory-local generation HMAC key (`0o600`).
pub fn generation_auth_key_path(dir: impl AsRef<std::path::Path>) -> std::path::PathBuf {
    dir.as_ref().join(".frankenredis-fec.key")
}

/// True when any active sidecar or generation record exists for `artifact_path`.
#[must_use]
pub fn has_sidecar_or_generation(artifact_path: impl AsRef<std::path::Path>) -> bool {
    let path = artifact_path.as_ref();
    let (env, sym) = sidecar_paths(path);
    let gen_p = generation_path(path);
    std::fs::symlink_metadata(&env).is_ok()
        || std::fs::symlink_metadata(&sym).is_ok()
        || std::fs::symlink_metadata(&gen_p).is_ok()
}

fn artifact_parent_dir(path: &std::path::Path) -> &std::path::Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."))
}

fn resolve_generation_auth_key_path(dir: &std::path::Path) -> std::path::PathBuf {
    if let Some(override_path) = std::env::var_os("FRANKENREDIS_FEC_AUTH_KEY_FILE")
        && !override_path.is_empty()
    {
        return std::path::PathBuf::from(override_path);
    }
    generation_auth_key_path(dir)
}

fn generate_random_32_bytes(seed_context: &str) -> [u8; 32] {
    use std::io::Read;

    static ENTROPY_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let mut os_buf = [0_u8; 32];
    let got_os = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut os_buf))
        .is_ok();
    let mut hasher = Sha256::new();
    hasher.update(b"frankenredis-fec-key-v1:");
    if got_os {
        hasher.update(os_buf);
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    hasher.update(nanos.to_le_bytes());
    hasher.update(std::process::id().to_le_bytes());
    hasher.update(
        ENTROPY_COUNTER
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .to_le_bytes(),
    );
    hasher.update(seed_context.as_bytes());
    let digest = hasher.finalize();
    let mut out = [0_u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// Load an existing generation HMAC key from `dir` without creating one.
pub fn load_existing_generation_auth_key(
    dir: impl AsRef<std::path::Path>,
) -> Result<Vec<u8>, String> {
    let key_path = resolve_generation_auth_key_path(dir.as_ref());
    let meta = std::fs::symlink_metadata(&key_path)
        .map_err(|err| format!("stat generation auth key {}: {err}", key_path.display()))?;
    if meta.file_type().is_symlink() {
        return Err(format!(
            "generation auth key {} is a symlink; refusing to read",
            key_path.display()
        ));
    }
    let bytes = std::fs::read(&key_path)
        .map_err(|err| format!("read generation auth key {}: {err}", key_path.display()))?;
    if bytes.len() < 32 {
        return Err(format!(
            "generation auth key {} is too short ({} bytes, need >= 32)",
            key_path.display(),
            bytes.len()
        ));
    }
    Ok(bytes)
}

/// Load the generation HMAC key for `dir`, atomically provisioning a `0o600`
/// 32-byte key on first use.
pub fn load_or_create_generation_auth_key(
    dir: impl AsRef<std::path::Path>,
) -> Result<Vec<u8>, String> {
    use std::io::Write;

    let dir = dir.as_ref();
    let key_path = resolve_generation_auth_key_path(dir);
    if let Ok(meta) = std::fs::symlink_metadata(&key_path) {
        if meta.file_type().is_symlink() {
            return Err(format!(
                "generation auth key {} is a symlink; refusing to use",
                key_path.display()
            ));
        }
        return load_existing_generation_auth_key(dir);
    }

    if let Some(parent) = key_path.parent()
        && !parent.as_os_str().is_empty()
    {
        let _ = std::fs::create_dir_all(parent);
    }
    let key = generate_random_32_bytes(&key_path.to_string_lossy());
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(&key_path) {
        Ok(mut file) => {
            file.write_all(&key)
                .and_then(|()| file.sync_all())
                .map_err(|err| {
                    format!(
                        "write and sync generation auth key {}: {err}",
                        key_path.display()
                    )
                })?;
            drop(file);
            #[cfg(unix)]
            sync_recovery_directory(artifact_parent_dir(&key_path))?;
            Ok(key.to_vec())
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            load_existing_generation_auth_key(dir)
        }
        Err(err) => Err(format!(
            "create generation auth key {}: {err}",
            key_path.display()
        )),
    }
}

fn generation_record_mac_payload(record: &GenerationRecord) -> String {
    format!(
        "frankenredis-fec-generation-record/v1\n\
         schema={}\n\
         artifact_id={}\n\
         generation_id={}\n\
         generation_seq={}\n\
         primary_sha256={}\n\
         primary_len={}\n\
         fec_enabled={}\n\
         sidecar_generation_id={}\n\
         sidecar_envelope_sha256={}\n\
         sidecar_symbols_sha256={}\n\
         codec_id={}\n\
         updated_unix_ms={}\n\
         auth_scheme={}",
        record.schema_version,
        record.artifact_id,
        record.generation_id,
        record.generation_seq,
        record.primary_sha256,
        record.primary_len,
        record.fec_enabled,
        record.sidecar_generation_id.as_deref().unwrap_or(""),
        record.sidecar_envelope_sha256.as_deref().unwrap_or(""),
        record.sidecar_symbols_sha256.as_deref().unwrap_or(""),
        record.codec_id.as_deref().unwrap_or(""),
        record.updated_unix_ms,
        record.auth_scheme,
    )
}

fn compute_generation_record_mac(record: &GenerationRecord, auth_key: &[u8]) -> String {
    hmac_sha256_hex(auth_key, generation_record_mac_payload(record).as_bytes())
}

fn verify_generation_record_mac(record: &GenerationRecord, auth_key: &[u8]) -> Result<(), String> {
    if record.schema_version != GENERATION_SCHEMA_VERSION {
        return Err(format!(
            "unsupported generation record schema '{}'",
            record.schema_version
        ));
    }
    if record.auth_scheme != GENERATION_AUTH_SCHEME_HMAC_SHA256 {
        return Err(format!(
            "unsupported generation auth scheme '{}'",
            record.auth_scheme
        ));
    }
    let expected = compute_generation_record_mac(record, auth_key);
    if !constant_time_eq(record.auth_mac.as_bytes(), expected.as_bytes()) {
        return Err(format!(
            "generation record authentication failed for {} (generation_id={})",
            record.artifact_id, record.generation_id
        ));
    }
    Ok(())
}

fn envelope_symbol_hashes_digest(envelope: &FecEnvelope) -> String {
    let mut hasher = Sha256::new();
    for h in &envelope.raptorq.symbol_hashes {
        hasher.update(h.as_bytes());
        hasher.update(b"\n");
    }
    if let Some(chunked) = &envelope.chunked {
        hasher.update(b"chunked:");
        hasher.update(chunked.chunk_size.to_le_bytes());
        hasher.update(chunked.total_source_bytes.to_le_bytes());
        for c in &chunked.chunks {
            hasher.update(c.index.to_le_bytes());
            hasher.update(c.byte_offset.to_le_bytes());
            hasher.update(c.byte_len.to_le_bytes());
            hasher.update(c.source_hash.as_bytes());
            hasher.update(c.oti);
            hasher.update(c.padding.to_le_bytes());
            hasher.update(c.symbol_stream_offset.to_le_bytes());
            hasher.update(c.symbol_stream_bytes.to_le_bytes());
        }
    }
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

/// Canonical SHA-256 over all immutable cryptographic/layout fields of an
/// `FecEnvelope`, excluding only operational scrub telemetry (`scrub` and
/// `decode_proofs`) so `scrub_sidecar` updates never invalidate an authentic
/// generation binding while tampering with any decode-critical field fails closed.
fn envelope_immutable_sha256(envelope: &FecEnvelope) -> String {
    let sym_digest = envelope_symbol_hashes_digest(envelope);
    let mut hasher = Sha256::new();
    hasher.update(b"frankenredis-fec-envelope-immutable/v1\n");
    hasher.update(envelope.schema_version.as_bytes());
    hasher.update(b"\n");
    hasher.update(envelope.codec_id.as_deref().unwrap_or("").as_bytes());
    hasher.update(b"\n");
    hasher.update(envelope.artifact_id.as_bytes());
    hasher.update(b"\n");
    hasher.update(envelope.artifact_type.as_bytes());
    hasher.update(b"\n");
    hasher.update(envelope.source_hash.as_bytes());
    hasher.update(b"\n");
    hasher.update(envelope.padding.to_le_bytes());
    hasher.update(envelope.symbol_size.to_le_bytes());
    hasher.update(envelope.oti);
    hasher.update(envelope.raptorq.k.to_le_bytes());
    hasher.update(envelope.raptorq.repair_symbols.to_le_bytes());
    hasher.update(sym_digest.as_bytes());
    hasher.update(b"\n");
    if let Some(binding) = &envelope.generation {
        hasher.update(binding.generation_id.as_bytes());
        hasher.update(b"\n");
        hasher.update(binding.generation_seq.to_le_bytes());
        hasher.update(binding.primary_len.to_le_bytes());
        hasher.update(binding.auth_scheme.as_bytes());
        hasher.update(b"\n");
        hasher.update(binding.auth_mac.as_bytes());
    }
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

fn compute_envelope_generation_mac(
    envelope: &FecEnvelope,
    binding: &SidecarGenerationBinding,
    auth_key: &[u8],
) -> String {
    let sym_digest = envelope_symbol_hashes_digest(envelope);
    let payload = format!(
        "frankenredis-fec-envelope-generation/v1\n\
         schema={}\n\
         codec_id={}\n\
         artifact_id={}\n\
         generation_id={}\n\
         generation_seq={}\n\
         source_hash={}\n\
         primary_len={}\n\
         k={}\n\
         repair={}\n\
         symbols_digest={}\n\
         auth_scheme={}",
        envelope.schema_version,
        envelope.codec_id.as_deref().unwrap_or(""),
        envelope.artifact_id,
        binding.generation_id,
        binding.generation_seq,
        envelope.source_hash,
        binding.primary_len,
        envelope.raptorq.k,
        envelope.raptorq.repair_symbols,
        sym_digest,
        binding.auth_scheme,
    );
    hmac_sha256_hex(auth_key, payload.as_bytes())
}

fn make_generation_id(seq: u64, now_unix_ms: u64, artifact_id: &str) -> String {
    let nonce = generate_random_32_bytes(artifact_id);
    format!(
        "gen-{seq:016x}-{now_unix_ms:016x}-{}",
        hex::encode(&nonce[..8])
    )
}

/// Read `<artifact>.generation.json` if present.
pub fn read_generation_record(
    artifact_path: impl AsRef<std::path::Path>,
) -> Result<GenerationRecord, String> {
    let gen_path = generation_path(artifact_path.as_ref());
    let json = std::fs::read_to_string(&gen_path)
        .map_err(|err| format!("read generation record {}: {err}", gen_path.display()))?;
    serde_json::from_str(&json)
        .map_err(|err| format!("parse generation record {}: {err}", gen_path.display()))
}

/// Atomically advance `<artifact>.generation.json` to a new committed primary
/// generation (`sidecar_generation_id = None`), immediately invalidating any
/// older sidecars on disk.
///
/// Called both as Phase 1 before writing fresh FEC sidecars AND on FEC-disabled
/// saves when prior sidecars or generation metadata exist.
pub fn commit_primary_generation(
    artifact_path: impl AsRef<std::path::Path>,
    primary_sha256: &str,
    primary_len: u64,
    fec_enabled: bool,
    auth_key: &[u8],
    now_unix_ms: u64,
) -> Result<GenerationRecord, String> {
    if auth_key.len() < 32 {
        return Err("generation auth key must be at least 32 bytes".to_string());
    }
    let path = artifact_path.as_ref();
    let gen_path = generation_path(path);
    let prev_seq = read_generation_record(path)
        .map(|r| r.generation_seq)
        .unwrap_or(0);
    let next_seq = prev_seq
        .checked_add(1)
        .ok_or_else(|| "generation sequence overflow".to_string())?;
    let artifact_id = path.to_string_lossy().into_owned();
    let generation_id = make_generation_id(next_seq, now_unix_ms, &artifact_id);
    let mut record = GenerationRecord {
        schema_version: GENERATION_SCHEMA_VERSION.to_string(),
        artifact_id,
        generation_id,
        generation_seq: next_seq,
        primary_sha256: primary_sha256.to_string(),
        primary_len,
        fec_enabled,
        sidecar_generation_id: None,
        sidecar_envelope_sha256: None,
        sidecar_symbols_sha256: None,
        codec_id: None,
        updated_unix_ms: now_unix_ms,
        auth_scheme: GENERATION_AUTH_SCHEME_HMAC_SHA256.to_string(),
        auth_mac: String::new(),
    };
    record.auth_mac = compute_generation_record_mac(&record, auth_key);
    let bytes = serde_json::to_vec_pretty(&record)
        .map_err(|err| format!("serialize generation record: {err}"))?;
    write_sidecar_file_atomically(&gen_path, &bytes)?;
    Ok(record)
}

/// Validate chunked encoding options before reading or allocating.
pub fn validate_chunked_options(options: ChunkedEncodeOptions) -> Result<(), String> {
    if options.chunk_size == 0 {
        return Err("chunk_size must be >= 1".to_string());
    }
    let chunk_len = usize::try_from(options.chunk_size)
        .map_err(|_| "chunk_size exceeds platform addressable memory".to_string())?;
    encoding_parameters(chunk_len, options.repair_symbols, options.symbol_size)?;
    Ok(())
}

fn default_chunk_size(primary_len: u64, symbol_size: u16) -> Option<u64> {
    // Tiny symbols can hit the codec's source-block limit before reaching
    // the usual 16 MiB working-set budget.
    let max_source_symbols = u64::from(u8::MAX) * u64::from(MAX_SOURCE_SYMBOLS_PER_BLOCK);
    let budget = DEFAULT_CHUNK_SIZE_BYTES.min(max_source_symbols * u64::from(symbol_size));
    (primary_len > budget).then_some(budget)
}

/// Encode `artifact_path` using bounded-memory chunked streaming (`v2`),
/// writing `.envelope.json` and `.symbols` atomically. Peak RAM is O(`chunk_size`),
/// enabling artifacts larger than RaptorQ 1.8.1's 255-source-block limit (~6.86 GiB).
pub fn write_sidecar_chunked(
    artifact_path: impl AsRef<std::path::Path>,
    artifact_type: &str,
    options: ChunkedEncodeOptions,
    now_unix_ms: u64,
) -> Result<(FecEnvelope, std::path::PathBuf, std::path::PathBuf), String> {
    write_sidecar_chunked_inner(
        artifact_path.as_ref(),
        artifact_type,
        options,
        None,
        now_unix_ms,
    )
    .map(|(env, env_path, sym_path, _, _)| (env, env_path, sym_path))
}

fn write_sidecar_chunked_inner(
    path: &std::path::Path,
    artifact_type: &str,
    options: ChunkedEncodeOptions,
    generation_ctx: Option<(&GenerationRecord, &[u8])>,
    now_unix_ms: u64,
) -> Result<
    (
        FecEnvelope,
        std::path::PathBuf,
        std::path::PathBuf,
        String,
        String,
    ),
    String,
> {
    use std::io::{BufWriter, Read, Write};

    validate_chunked_options(options)?;
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|err| format!("stat source artifact {}: {err}", path.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "source artifact {} is a symlink; select a regular artifact path",
            path.display()
        ));
    }
    let chunk_size_usize = usize::try_from(options.chunk_size)
        .map_err(|_| "chunk_size exceeds platform addressable memory".to_string())?;
    let (envelope_path, symbols_path) = sidecar_paths(path);
    if let Some(parent) = envelope_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    static NEXT_CHUNK_SYM_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let (sym_staging_path, sym_file) =
        create_sidecar_staging_file(&symbols_path, &NEXT_CHUNK_SYM_ID)?;
    let mut sym_writer = BufWriter::new(sym_file);
    sym_writer
        .write_all(V2_SYMBOL_STREAM_MAGIC)
        .map_err(|err| {
            format!(
                "write chunked symbol magic {}: {err}; staging file retained at {}",
                symbols_path.display(),
                sym_staging_path.display()
            )
        })?;
    let mut sym_offset = V2_SYMBOL_STREAM_MAGIC.len() as u64;
    let mut sym_hasher = Sha256::new();
    sym_hasher.update(V2_SYMBOL_STREAM_MAGIC);

    let mut src_file = std::fs::File::open(path)
        .map_err(|err| format!("open source artifact {}: {err}", path.display()))?;
    let mut whole_hasher = Sha256::new();
    let mut total_source_bytes = 0_u64;
    let mut chunk_buf = Vec::new();
    let mut chunk_index = 0_u32;
    let mut chunks = Vec::new();
    let mut all_symbol_hashes = Vec::new();
    let mut total_k = 0_u32;
    let mut total_repair = 0_u32;
    let mut actual_symbol_size = options.symbol_size;

    loop {
        chunk_buf.clear();
        let mut take_reader = (&mut src_file).take(options.chunk_size);
        let mut read_buf = [0_u8; 64 * 1024];
        loop {
            let remaining = chunk_size_usize.saturating_sub(chunk_buf.len());
            if remaining == 0 {
                break;
            }
            let want = remaining.min(read_buf.len());
            match take_reader.read(&mut read_buf[..want]) {
                Ok(0) => break,
                Ok(n) => chunk_buf.extend_from_slice(&read_buf[..n]),
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(err) => {
                    return Err(format!("read source artifact {}: {err}", path.display()));
                }
            }
        }
        if chunk_buf.is_empty() && chunk_index > 0 {
            break;
        }
        let chunk_byte_offset = total_source_bytes;
        let chunk_byte_len = chunk_buf.len() as u64;
        total_source_bytes = total_source_bytes
            .checked_add(chunk_byte_len)
            .ok_or_else(|| "total artifact bytes overflow u64".to_string())?;
        whole_hasher.update(&chunk_buf);

        let encoded_chunk = encode_artifact(
            &format!("{}#chunk-{chunk_index}", path.to_string_lossy()),
            artifact_type,
            &chunk_buf,
            options.repair_symbols,
            options.symbol_size,
            now_unix_ms,
        )?;
        actual_symbol_size = encoded_chunk.envelope.symbol_size;
        let chunk_stream = serialize_symbol_stream(&encoded_chunk.symbols);
        sym_writer.write_all(&chunk_stream).map_err(|err| {
            format!(
                "write chunk {chunk_index} symbols {}: {err}; staging file retained at {}",
                symbols_path.display(),
                sym_staging_path.display()
            )
        })?;
        sym_hasher.update(&chunk_stream);

        let chunk_stream_len = chunk_stream.len() as u64;
        let sym_hash_start = all_symbol_hashes.len();
        let sym_hash_count = encoded_chunk.envelope.raptorq.symbol_hashes.len();
        all_symbol_hashes.extend(encoded_chunk.envelope.raptorq.symbol_hashes);
        total_k = total_k
            .checked_add(encoded_chunk.envelope.raptorq.k)
            .ok_or_else(|| "chunked source symbol count overflows u32".to_string())?;
        total_repair = total_repair
            .checked_add(encoded_chunk.envelope.raptorq.repair_symbols)
            .ok_or_else(|| "chunked repair symbol count overflows u32".to_string())?;

        chunks.push(ChunkDescriptor {
            index: chunk_index,
            byte_offset: chunk_byte_offset,
            byte_len: chunk_byte_len,
            source_hash: encoded_chunk.envelope.source_hash,
            padding: encoded_chunk.envelope.padding,
            symbol_size: encoded_chunk.envelope.symbol_size,
            oti: encoded_chunk.envelope.oti,
            k: encoded_chunk.envelope.raptorq.k,
            repair_symbols: encoded_chunk.envelope.raptorq.repair_symbols,
            symbol_stream_offset: sym_offset,
            symbol_stream_bytes: chunk_stream_len,
            symbol_hash_start: sym_hash_start,
            symbol_hash_count: sym_hash_count,
        });
        sym_offset = sym_offset
            .checked_add(chunk_stream_len)
            .ok_or_else(|| "chunked symbol stream offset overflows u64".to_string())?;
        chunk_index = chunk_index
            .checked_add(1)
            .ok_or_else(|| "chunk count overflows u32".to_string())?;

        if chunk_buf.is_empty() {
            break;
        }
    }

    let sym_file = sym_writer.into_inner().map_err(|err| {
        format!(
            "flush chunked symbol file {}: {}; staging file retained at {}",
            symbols_path.display(),
            err.error(),
            sym_staging_path.display()
        )
    })?;
    sym_file.sync_all().map_err(|err| {
        format!(
            "sync chunked symbol file {}: {err}; staging file retained at {}",
            symbols_path.display(),
            sym_staging_path.display()
        )
    })?;
    drop(sym_file);

    let symbols_sha256 = format!("sha256:{}", hex::encode(sym_hasher.finalize()));
    let whole_source_hash = format!("sha256:{}", hex::encode(whole_hasher.finalize()));
    let first_padding = chunks[0].padding;
    let first_oti = chunks[0].oti;
    let mut envelope = FecEnvelope {
        schema_version: CHUNKED_ENVELOPE_SCHEMA_VERSION.to_string(),
        artifact_id: path.to_string_lossy().into_owned(),
        artifact_type: artifact_type.to_string(),
        source_hash: whole_source_hash,
        codec_id: Some(CODEC_ID_RAPTORQ_1_8_1.to_string()),
        generation: None,
        chunked: Some(ChunkedLayout {
            chunk_size: options.chunk_size,
            total_source_bytes,
            chunks,
        }),
        padding: first_padding,
        symbol_size: actual_symbol_size,
        oti: first_oti,
        raptorq: RaptorQSection {
            k: total_k,
            repair_symbols: total_repair,
            overhead_ratio: f64::from(total_repair) / f64::from(total_k.max(1)),
            symbol_hashes: all_symbol_hashes,
        },
        scrub: ScrubStatus {
            last_ok_unix_ms: now_unix_ms,
            status: "ok".to_string(),
        },
        decode_proofs: Vec::new(),
    };

    if let Some((record, auth_key)) = generation_ctx {
        if record.primary_sha256 != envelope.source_hash || record.primary_len != total_source_bytes
        {
            return Err(format!(
                "source artifact {} changed during chunked sidecar publication",
                path.display()
            ));
        }
        let mut binding = SidecarGenerationBinding {
            generation_id: record.generation_id.clone(),
            generation_seq: record.generation_seq,
            primary_len: total_source_bytes,
            auth_scheme: GENERATION_AUTH_SCHEME_HMAC_SHA256.to_string(),
            auth_mac: String::new(),
        };
        binding.auth_mac = compute_envelope_generation_mac(&envelope, &binding, auth_key);
        envelope.generation = Some(binding);
    }

    let envelope_json = envelope_to_json(&envelope);
    let envelope_sha256 = envelope_immutable_sha256(&envelope);
    std::fs::rename(&sym_staging_path, &symbols_path).map_err(|err| {
        format!(
            "publish chunked symbols {}: {err}; staging file retained at {}",
            symbols_path.display(),
            sym_staging_path.display()
        )
    })?;
    #[cfg(unix)]
    sync_recovery_directory(artifact_parent_dir(&symbols_path))?;
    write_sidecar_file_atomically(&envelope_path, envelope_json.as_bytes())?;
    Ok((
        envelope,
        envelope_path,
        symbols_path,
        envelope_sha256,
        symbols_sha256,
    ))
}

/// Publish an authenticated, generation-bound RaptorQ sidecar pair using
/// two-phase atomic commit via `<artifact>.generation.json`:
///
/// 1. Phase 1 commits `G_new` with `sidecar_generation_id: None`, immediately
///    invalidating any older sidecars on disk even if publication crashes mid-way.
/// 2. Sidecars (`.envelope.json` + `.symbols`) are encoded (using streaming `v2`
///    chunked mode for explicit chunk sizes and artifacts larger than the
///    default chunk budget) and atomically published.
/// 3. Phase 2 atomically updates `<artifact>.generation.json` binding both
///    sidecar hashes to `G_new` under an HMAC-SHA256 tag.
pub fn write_generation_bound_sidecar(
    artifact_path: impl AsRef<std::path::Path>,
    artifact_type: &str,
    repair_symbols: usize,
    symbol_size: u16,
    chunk_size_override: Option<u64>,
    auth_key: &[u8],
    now_unix_ms: u64,
) -> Result<
    (
        GenerationRecord,
        FecEnvelope,
        std::path::PathBuf,
        std::path::PathBuf,
        std::path::PathBuf,
    ),
    String,
> {
    if auth_key.len() < 32 {
        return Err("generation auth key must be at least 32 bytes".to_string());
    }
    let path = artifact_path.as_ref();
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|err| format!("stat source artifact {}: {err}", path.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "source artifact {} is a symlink; select a regular artifact path",
            path.display()
        ));
    }
    let file = std::fs::File::open(path)
        .map_err(|err| format!("open source artifact {}: {err}", path.display()))?;
    let (primary_len, primary_sha256) = sha256_reader(file)
        .map_err(|err| format!("hash source artifact {}: {err}", path.display()))?;

    // Keep the bulk source data, encoder state and serialized packets for at
    // most one chunk live. The codec's single-object limit can be several GiB
    // above that budget.
    let chunk_size = chunk_size_override.or_else(|| default_chunk_size(primary_len, symbol_size));
    if let Some(chunk_size) = chunk_size {
        validate_chunked_options(ChunkedEncodeOptions {
            chunk_size,
            repair_symbols,
            symbol_size,
        })?;
    } else {
        encoding_parameters(
            usize::try_from(primary_len).map_err(|_| "source exceeds platform size")?,
            repair_symbols,
            symbol_size,
        )?;
    }

    // Phase 1: commit new primary generation with no sidecar bound yet.
    let mut record = commit_primary_generation(
        path,
        &primary_sha256,
        primary_len,
        true,
        auth_key,
        now_unix_ms,
    )?;

    let published = if let Some(chunk_size) = chunk_size {
        write_sidecar_chunked_inner(
            path,
            artifact_type,
            ChunkedEncodeOptions {
                chunk_size,
                repair_symbols,
                symbol_size,
            },
            Some((&record, auth_key)),
            now_unix_ms,
        )?
    } else {
        let (envelope_path, symbols_path) = sidecar_paths(path);
        let data = std::fs::read(path).map_err(|err| format!("read {}: {err}", path.display()))?;
        let data_sha = sha256_hex(&data);
        if data_sha != primary_sha256 || data.len() as u64 != primary_len {
            return Err(format!(
                "source artifact {} changed during sidecar publication",
                path.display()
            ));
        }
        let mut encoded = encode_artifact(
            path.to_str().unwrap_or("artifact"),
            artifact_type,
            &data,
            repair_symbols,
            symbol_size,
            now_unix_ms,
        )?;
        let mut binding = SidecarGenerationBinding {
            generation_id: record.generation_id.clone(),
            generation_seq: record.generation_seq,
            primary_len,
            auth_scheme: GENERATION_AUTH_SCHEME_HMAC_SHA256.to_string(),
            auth_mac: String::new(),
        };
        binding.auth_mac = compute_envelope_generation_mac(&encoded.envelope, &binding, auth_key);
        encoded.envelope.generation = Some(binding);

        if let Some(parent) = envelope_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let envelope_json = envelope_to_json(&encoded.envelope);
        let symbols_bytes = serialize_symbol_stream(&encoded.symbols);
        let envelope_sha256 = envelope_immutable_sha256(&encoded.envelope);
        let symbols_sha256 = sha256_hex(&symbols_bytes);
        write_sidecar_file_atomically(&symbols_path, &symbols_bytes)?;
        write_sidecar_file_atomically(&envelope_path, envelope_json.as_bytes())?;
        (
            encoded.envelope,
            envelope_path,
            symbols_path,
            envelope_sha256,
            symbols_sha256,
        )
    };

    let (envelope, envelope_path, symbols_path, envelope_sha256, symbols_sha256) = published;
    // Phase 2: single atomic commit binding both envelope and symbols to G_new.
    record.sidecar_generation_id = Some(record.generation_id.clone());
    record.sidecar_envelope_sha256 = Some(envelope_sha256);
    record.sidecar_symbols_sha256 = Some(symbols_sha256);
    record.codec_id = Some(CODEC_ID_RAPTORQ_1_8_1.to_string());
    record.auth_mac = compute_generation_record_mac(&record, auth_key);
    let gen_path = generation_path(path);
    let record_bytes = serde_json::to_vec_pretty(&record)
        .map_err(|err| format!("serialize generation record: {err}"))?;
    write_sidecar_file_atomically(&gen_path, &record_bytes)?;

    Ok((record, envelope, gen_path, envelope_path, symbols_path))
}

/// Strictly authenticate `<artifact>.generation.json` against the sidecar
/// envelope on disk. Refuses missing/tampered generation records, FEC-disabled
/// generations, interrupted/superseded sidecar pairs, and unsupported codecs.
pub fn verify_generation_binding(
    artifact_path: impl AsRef<std::path::Path>,
    auth_key: &[u8],
) -> Result<VerifiedGenerationBinding, String> {
    let path = artifact_path.as_ref();
    let record = read_generation_record(path)?;
    verify_generation_record_mac(&record, auth_key)?;
    if !record.fec_enabled {
        return Err(format!(
            "primary generation {} (seq={}) for {} has FEC disabled; refusing stale sidecar recovery",
            record.generation_id,
            record.generation_seq,
            path.display()
        ));
    }
    let Some(bound_gen_id) = record.sidecar_generation_id.as_deref() else {
        return Err(format!(
            "primary generation {} (seq={}) for {} has no committed sidecar (interrupted or pending sidecar update)",
            record.generation_id,
            record.generation_seq,
            path.display()
        ));
    };
    if bound_gen_id != record.generation_id {
        return Err(format!(
            "generation record mismatch for {}: primary generation {} != sidecar generation {}",
            path.display(),
            record.generation_id,
            bound_gen_id
        ));
    }
    validate_codec_id(record.codec_id.as_deref())?;
    let Some(expected_env_sha) = record.sidecar_envelope_sha256.as_deref() else {
        return Err(format!(
            "generation record {} missing sidecar_envelope_sha256",
            record.generation_id
        ));
    };
    let (envelope_path, symbols_path) = sidecar_paths(path);
    let env_raw = std::fs::read(&envelope_path)
        .map_err(|err| format!("read envelope {}: {err}", envelope_path.display()))?;
    let env_str = std::str::from_utf8(&env_raw)
        .map_err(|err| format!("envelope {} is not UTF-8: {err}", envelope_path.display()))?;
    let envelope = envelope_from_json(env_str)?;
    let actual_env_sha = envelope_immutable_sha256(&envelope);
    if !constant_time_eq(actual_env_sha.as_bytes(), expected_env_sha.as_bytes()) {
        return Err(format!(
            "sidecar envelope {} digest {actual_env_sha} does not match committed generation {} ({expected_env_sha})",
            envelope_path.display(),
            record.generation_id
        ));
    }
    validate_codec_id(envelope.codec_id.as_deref())?;
    let Some(binding) = envelope.generation.as_ref() else {
        return Err(format!(
            "envelope {} lacks authenticated generation binding metadata",
            envelope_path.display()
        ));
    };
    if binding.generation_id != record.generation_id
        || binding.generation_seq != record.generation_seq
        || binding.primary_len != record.primary_len
    {
        return Err(format!(
            "sidecar envelope {} belongs to generation {} (seq={}), not committed primary generation {} (seq={})",
            envelope_path.display(),
            binding.generation_id,
            binding.generation_seq,
            record.generation_id,
            record.generation_seq
        ));
    }
    if binding.auth_scheme != GENERATION_AUTH_SCHEME_HMAC_SHA256 {
        return Err(format!(
            "unsupported envelope generation auth scheme '{}'",
            binding.auth_scheme
        ));
    }
    let expected_env_mac = compute_envelope_generation_mac(&envelope, binding, auth_key);
    if !constant_time_eq(binding.auth_mac.as_bytes(), expected_env_mac.as_bytes()) {
        return Err(format!(
            "envelope generation MAC verification failed for {}",
            envelope_path.display()
        ));
    }
    if envelope.source_hash != record.primary_sha256 {
        return Err(format!(
            "envelope source_hash {} does not match committed generation primary_sha256 {}",
            envelope.source_hash, record.primary_sha256
        ));
    }

    let sym_file = std::fs::File::open(&symbols_path)
        .map_err(|err| format!("open symbols {}: {err}", symbols_path.display()))?;
    let (_, actual_sym_sha) = sha256_reader(sym_file)
        .map_err(|err| format!("hash symbols {}: {err}", symbols_path.display()))?;
    let symbols_file_intact = record
        .sidecar_symbols_sha256
        .as_deref()
        .is_some_and(|expected| constant_time_eq(actual_sym_sha.as_bytes(), expected.as_bytes()));

    Ok(VerifiedGenerationBinding {
        record,
        envelope,
        symbols_file_intact,
    })
}

/// Safely move superseded sidecar files out of active paths without deleting
/// forensic evidence. Refuses to retire an active current-generation sidecar.
pub fn retire_superseded_sidecar(
    artifact_path: impl AsRef<std::path::Path>,
    auth_key: &[u8],
) -> Result<RetiredSidecarPaths, String> {
    let path = artifact_path.as_ref();
    let record = read_generation_record(path)?;
    verify_generation_record_mac(&record, auth_key)?;

    if record.fec_enabled
        && record.sidecar_generation_id.as_deref() == Some(&record.generation_id)
        && verify_generation_binding(path, auth_key).is_ok()
    {
        return Err(format!(
            "refusing to retire active current-generation sidecar {} (generation_id={})",
            path.display(),
            record.generation_id
        ));
    }

    let (env_path, sym_path) = sidecar_paths(path);
    let move_aside =
        |src: &std::path::Path, suffix: &str| -> Result<Option<std::path::PathBuf>, String> {
            if std::fs::symlink_metadata(src).is_err() {
                return Ok(None);
            }
            for seq in 0..1024_u32 {
                let dst = append_path_suffix(
                    path,
                    &format!(".superseded.seq{}.{seq}.{suffix}", record.generation_seq),
                );
                if std::fs::symlink_metadata(&dst).is_ok() {
                    continue;
                }
                std::fs::rename(src, &dst).map_err(|err| {
                    format!(
                        "retire superseded sidecar {} -> {}: {err}",
                        src.display(),
                        dst.display()
                    )
                })?;
                return Ok(Some(dst));
            }
            Err(format!(
                "could not allocate collision-free retirement path for {}",
                src.display()
            ))
        };

    let retired_envelope = move_aside(&env_path, "envelope.json")?;
    let retired_symbols = move_aside(&sym_path, "symbols")?;
    #[cfg(unix)]
    if retired_envelope.is_some() || retired_symbols.is_some() {
        sync_recovery_directory(artifact_parent_dir(path))?;
    }
    Ok(RetiredSidecarPaths {
        retired_envelope,
        retired_symbols,
    })
}

/// Conventional path of a forensic backup from earlier recovery tools.
/// Scrubbing and recovery exports never overwrite this backup.
pub fn corrupt_backup_path(artifact_path: impl AsRef<std::path::Path>) -> std::path::PathBuf {
    append_path_suffix(artifact_path.as_ref(), ".corrupt")
}

/// Write sidecar envelope and symbols files alongside an artifact.
///
/// Each file is synced and atomically replaced without following or truncating
/// an existing destination link. The pair is not one atomic generation commit:
/// interrupted publication can leave mismatched sidecars, which verification
/// must reject. Neither sidecar is authority to replace a primary artifact.
/// Source artifact symlinks are rejected: a source link could traverse a
/// sidecar destination that publication is about to replace.
pub fn write_sidecar(
    artifact_path: impl AsRef<std::path::Path>,
    artifact_type: &str,
    repair_symbols: usize,
    symbol_size: u16,
    now_unix_ms: u64,
) -> Result<(FecEnvelope, std::path::PathBuf, std::path::PathBuf), String> {
    let path = artifact_path.as_ref();
    let (envelope_path, symbols_path) = sidecar_paths(path);
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|err| format!("stat source artifact {}: {err}", path.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "source artifact {} is a symlink; select a regular artifact path",
            path.display()
        ));
    }
    let data_len = usize::try_from(metadata.len())
        .map_err(|_| format!("artifact {} is too large for this platform", path.display()))?;
    // Reject known unsupported sizes before reading a multi-GiB artifact.
    // encode_artifact validates the bytes again if the file changed.
    encoding_parameters(data_len, repair_symbols, symbol_size)?;
    let data = std::fs::read(path).map_err(|err| format!("read {}: {err}", path.display()))?;
    let encoded = encode_artifact(
        path.to_str().unwrap_or("artifact"),
        artifact_type,
        &data,
        repair_symbols,
        symbol_size,
        now_unix_ms,
    )?;
    if let Some(parent) = envelope_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let envelope_json = envelope_to_json(&encoded.envelope);
    let symbols_bytes = serialize_symbol_stream(&encoded.symbols);
    write_sidecar_file_atomically(&envelope_path, envelope_json.as_bytes())?;
    write_sidecar_file_atomically(&symbols_path, &symbols_bytes)?;
    Ok((encoded.envelope, envelope_path, symbols_path))
}

fn write_sidecar_file_atomically(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;

    write_sidecar_file_atomically_with(path, |file| {
        file.write_all(bytes)?;
        file.sync_all()
    })
}

/// Retain failed staging files for inspection. A write or sync failure must
/// leave the previous destination intact, including every other hard link to it.
fn write_sidecar_file_atomically_with(
    path: &std::path::Path,
    write_and_sync: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
) -> Result<(), String> {
    static NEXT_TEMP_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    let (staging_path, mut file) = create_sidecar_staging_file(path, &NEXT_TEMP_ID)?;
    write_and_sync(&mut file).map_err(|err| {
        format!(
            "write and sync sidecar {}: {err}; staging file retained at {}",
            path.display(),
            staging_path.display()
        )
    })?;
    drop(file);
    std::fs::rename(&staging_path, path).map_err(|err| {
        format!(
            "publish sidecar {}: {err}; staging file retained at {}",
            path.display(),
            staging_path.display()
        )
    })?;
    #[cfg(unix)]
    {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new("."));
        std::fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|err| format!("sync sidecar directory {}: {err}", parent.display()))?;
    }
    Ok(())
}

fn create_sidecar_staging_file(
    path: &std::path::Path,
    next_id: &std::sync::atomic::AtomicU64,
) -> Result<(std::path::PathBuf, std::fs::File), String> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    for _ in 0..128 {
        let id = next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let staging_path =
            parent.join(format!(".frankenredis-fec-{}-{id}.tmp", std::process::id()));
        if staging_path.file_name() == path.file_name() {
            continue;
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&staging_path) {
            Ok(file) => return Ok((staging_path, file)),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(format!(
                    "create sidecar staging file {}: {err}",
                    staging_path.display()
                ));
            }
        }
    }
    Err(format!(
        "could not create an unused sidecar staging file for {}",
        path.display()
    ))
}

/// Read sidecar envelope and symbols files for an artifact.
pub fn read_sidecar(
    artifact_path: impl AsRef<std::path::Path>,
) -> Result<(FecEnvelope, Vec<EncodingPacket>), String> {
    let path = artifact_path.as_ref();
    let (envelope_path, symbols_path) = sidecar_paths(path);
    let envelope_json = std::fs::read_to_string(&envelope_path)
        .map_err(|err| format!("read envelope {}: {err}", envelope_path.display()))?;
    let envelope = envelope_from_json(&envelope_json)?;
    let symbols_bytes = std::fs::read(&symbols_path)
        .map_err(|err| format!("read symbols {}: {err}", symbols_path.display()))?;
    let symbols = deserialize_symbol_stream(&symbols_bytes)?;
    Ok((envelope, symbols))
}

struct StreamingDecodeStats {
    total_bytes: u64,
    source_hash: String,
    missing: usize,
    damaged: usize,
}

fn decode_chunked_sidecar_to_writer(
    envelope: &FecEnvelope,
    symbols_path: &std::path::Path,
    mut writer: impl std::io::Write,
) -> Result<StreamingDecodeStats, String> {
    use std::io::{Read, Seek, SeekFrom};

    if envelope.schema_version != CHUNKED_ENVELOPE_SCHEMA_VERSION {
        return Err(format!(
            "expected chunked schema {CHUNKED_ENVELOPE_SCHEMA_VERSION}, got {}",
            envelope.schema_version
        ));
    }
    validate_codec_id(envelope.codec_id.as_deref())?;
    let Some(layout) = envelope.chunked.as_ref() else {
        return Err("chunked v2 envelope missing chunked layout metadata".to_string());
    };
    if layout.chunk_size == 0 || layout.chunks.is_empty() {
        return Err("chunked v2 envelope has empty or zero-sized chunk layout".to_string());
    }

    let mut sym_file = std::fs::File::open(symbols_path)
        .map_err(|err| format!("open symbols {}: {err}", symbols_path.display()))?;
    let sym_file_len = sym_file
        .metadata()
        .map_err(|err| format!("stat symbols {}: {err}", symbols_path.display()))?
        .len();
    let mut magic = [0_u8; 8];
    sym_file
        .read_exact(&mut magic)
        .map_err(|err| format!("read v2 symbol header {}: {err}", symbols_path.display()))?;
    if &magic != V2_SYMBOL_STREAM_MAGIC {
        return Err(format!(
            "invalid v2 symbol stream magic in {}",
            symbols_path.display()
        ));
    }

    let mut expected_byte_offset = 0_u64;
    let mut whole_hasher = Sha256::new();
    let mut total_missing = 0_usize;
    let mut total_damaged = 0_usize;

    for (expected_idx, chunk) in layout.chunks.iter().enumerate() {
        if u32::try_from(expected_idx).ok() != Some(chunk.index) {
            return Err(format!("chunked layout index mismatch at {expected_idx}"));
        }
        if chunk.byte_offset != expected_byte_offset {
            return Err(format!(
                "non-contiguous chunk byte_offset at chunk {}: expected {expected_byte_offset}, got {}",
                chunk.index, chunk.byte_offset
            ));
        }
        expected_byte_offset = expected_byte_offset
            .checked_add(chunk.byte_len)
            .ok_or_else(|| "chunk byte_offset overflow".to_string())?;
        let hash_end = chunk
            .symbol_hash_start
            .checked_add(chunk.symbol_hash_count)
            .ok_or_else(|| "chunk symbol hash range overflow".to_string())?;
        if hash_end > envelope.raptorq.symbol_hashes.len() {
            return Err(format!(
                "chunk {} symbol hash range {hash_end} exceeds envelope symbol_hashes ({})",
                chunk.index,
                envelope.raptorq.symbol_hashes.len()
            ));
        }
        let stream_end = chunk
            .symbol_stream_offset
            .checked_add(chunk.symbol_stream_bytes)
            .ok_or_else(|| "chunk symbol stream range overflow".to_string())?;
        if chunk.symbol_stream_offset < V2_SYMBOL_STREAM_MAGIC.len() as u64
            || stream_end > sym_file_len
        {
            return Err(format!(
                "chunk {} symbol stream range {}..{} out of bounds (file len {sym_file_len})",
                chunk.index, chunk.symbol_stream_offset, stream_end
            ));
        }
        let chunk_stream_len = usize::try_from(chunk.symbol_stream_bytes)
            .map_err(|_| format!("chunk {} symbol stream exceeds platform size", chunk.index))?;
        let mut chunk_stream = vec![0_u8; chunk_stream_len];
        sym_file
            .seek(SeekFrom::Start(chunk.symbol_stream_offset))
            .and_then(|_| sym_file.read_exact(&mut chunk_stream))
            .map_err(|err| format!("read chunk {} symbols: {err}", chunk.index))?;
        let packets = deserialize_symbol_stream(&chunk_stream)
            .map_err(|err| format!("chunk {} symbol stream: {err}", chunk.index))?;

        let chunk_envelope = FecEnvelope {
            schema_version: ENVELOPE_SCHEMA_VERSION.to_string(),
            artifact_id: format!("{}#chunk-{}", envelope.artifact_id, chunk.index),
            artifact_type: envelope.artifact_type.clone(),
            source_hash: chunk.source_hash.clone(),
            codec_id: envelope.codec_id.clone(),
            generation: None,
            chunked: None,
            padding: chunk.padding,
            symbol_size: chunk.symbol_size,
            oti: chunk.oti,
            raptorq: RaptorQSection {
                k: chunk.k,
                repair_symbols: chunk.repair_symbols,
                overhead_ratio: f64::from(chunk.repair_symbols) / f64::from(chunk.k.max(1)),
                symbol_hashes: envelope.raptorq.symbol_hashes[chunk.symbol_hash_start..hash_end]
                    .to_vec(),
            },
            scrub: envelope.scrub.clone(),
            decode_proofs: Vec::new(),
        };
        let decoded = decode_verified_symbols(&chunk_envelope, &packets)
            .map_err(|err| format!("chunk {} decode failed: {err}", chunk.index))?;
        if decoded.source.len() as u64 != chunk.byte_len {
            return Err(format!(
                "chunk {} decoded length {} != expected {}",
                chunk.index,
                decoded.source.len(),
                chunk.byte_len
            ));
        }
        writer
            .write_all(&decoded.source)
            .map_err(|err| format!("write decoded chunk {}: {err}", chunk.index))?;
        whole_hasher.update(&decoded.source);
        total_missing = total_missing.saturating_add(decoded.missing);
        total_damaged = total_damaged.saturating_add(decoded.damaged);
    }

    if expected_byte_offset != layout.total_source_bytes {
        return Err(format!(
            "chunked decoded total bytes {expected_byte_offset} != layout.total_source_bytes {}",
            layout.total_source_bytes
        ));
    }
    let whole_hash = format!("sha256:{}", hex::encode(whole_hasher.finalize()));
    if !envelope.source_hash.is_empty() && whole_hash != envelope.source_hash {
        return Err(format!(
            "chunked recovered source hash {whole_hash} does not match envelope.source_hash {}",
            envelope.source_hash
        ));
    }
    Ok(StreamingDecodeStats {
        total_bytes: expected_byte_offset,
        source_hash: whole_hash,
        missing: total_missing,
        damaged: total_damaged,
    })
}

fn decode_sidecar_file_to_writer(
    artifact_path: &std::path::Path,
    envelope: &FecEnvelope,
    mut writer: impl std::io::Write,
) -> Result<StreamingDecodeStats, String> {
    let (_, symbols_path) = sidecar_paths(artifact_path);
    if envelope.schema_version == CHUNKED_ENVELOPE_SCHEMA_VERSION {
        return decode_chunked_sidecar_to_writer(envelope, &symbols_path, writer);
    }
    let symbols_bytes = std::fs::read(&symbols_path)
        .map_err(|err| format!("read symbols {}: {err}", symbols_path.display()))?;
    let symbols = deserialize_symbol_stream(&symbols_bytes)?;
    let decoded = decode_verified_symbols(envelope, &symbols)?;
    writer
        .write_all(&decoded.source)
        .map_err(|err| format!("write decoded artifact: {err}"))?;
    Ok(StreamingDecodeStats {
        total_bytes: decoded.source.len() as u64,
        source_hash: decoded.source_hash,
        missing: decoded.missing,
        damaged: decoded.damaged,
    })
}

fn read_envelope_only(artifact_path: &std::path::Path) -> Result<FecEnvelope, String> {
    let (envelope_path, _) = sidecar_paths(artifact_path);
    let envelope_json = std::fs::read_to_string(&envelope_path)
        .map_err(|err| format!("read envelope {}: {err}", envelope_path.display()))?;
    envelope_from_json(&envelope_json)
}

/// Recover a sidecar into a new, private directory for operator inspection.
///
/// The bundle contains `artifact` and `proof.json`. When an authenticated
/// `<artifact>.generation.json` matches the sidecar, `proof.json` records
/// `"primary_generation_verified": true` along with `generation_id` and
/// `generation_seq`; for unversioned or mismatched sidecars it records `false`.
/// Neither the primary nor its sidecars or existing forensic backups are modified.
pub fn recover_sidecar_to_directory(
    artifact_path: impl AsRef<std::path::Path>,
    output_directory: impl AsRef<std::path::Path>,
    now_unix_ms: u64,
) -> Result<DecodeProof, String> {
    use std::io::BufWriter;

    let path = artifact_path.as_ref();
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(format!(
                "recovery source artifact {} is a symlink; select a regular artifact path",
                path.display()
            ));
        }
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(format!("stat source artifact {}: {err}", path.display())),
    }
    let output = output_directory.as_ref();
    let resolved_output = resolve_new_path(output)?;
    let (envelope_path, symbols_path) = sidecar_paths(path);
    let gen_path = generation_path(path);
    for input_path in [
        path,
        envelope_path.as_path(),
        symbols_path.as_path(),
        gen_path.as_path(),
    ] {
        if resolved_output == resolve_new_path(input_path)? {
            return Err(format!(
                "recovery output {} aliases a source artifact or sidecar",
                output.display()
            ));
        }
    }

    // Use the authenticated envelope for both decodes. Looking it up separately
    // could attach a new generation's proof to bytes from an earlier envelope.
    let verified_gen = load_existing_generation_auth_key(artifact_parent_dir(path))
        .ok()
        .and_then(|key| verify_generation_binding(path, &key).ok());
    let unverified_envelope;
    let envelope = if let Some(binding) = &verified_gen {
        &binding.envelope
    } else {
        unverified_envelope = read_envelope_only(path)?;
        &unverified_envelope
    };
    let primary_generation_verified = verified_gen.is_some();
    let reason = if let Some(vg) = &verified_gen {
        format!(
            "verified-generation recovery export (generation_id={}, seq={})",
            vg.record.generation_id, vg.record.generation_seq
        )
    } else {
        "manual recovery export; primary generation has not been authenticated".to_string()
    };

    // Pre-verify full decode before creating the destination directory so an
    // invalid sidecar never creates an output bundle.
    struct SinkWriter;
    impl std::io::Write for SinkWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let preflight_stats = decode_sidecar_file_to_writer(path, envelope, SinkWriter)?;
    if let Some(binding) = &verified_gen
        && (preflight_stats.source_hash != binding.record.primary_sha256
            || preflight_stats.total_bytes != binding.record.primary_len)
    {
        return Err("decoded export does not match authenticated primary generation".to_string());
    }

    let mut directory = std::fs::DirBuilder::new();
    directory.recursive(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        directory.mode(0o700);
    }
    directory
        .create(&resolved_output)
        .map_err(|err| format!("create new recovery directory {}: {err}", output.display()))?;

    let publish = || -> Result<DecodeProof, String> {
        let artifact_out_path = resolved_output.join("artifact");
        let artifact_file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&artifact_out_path)
            .map_err(|err| {
                format!(
                    "create recovery file {}: {err}",
                    artifact_out_path.display()
                )
            })?;
        let mut buffered = BufWriter::new(artifact_file);
        let stats = decode_sidecar_file_to_writer(path, envelope, &mut buffered)?;
        let artifact_file = buffered
            .into_inner()
            .map_err(|err| format!("flush recovery file: {}", err.error()))?;
        artifact_file
            .sync_all()
            .map_err(|err| format!("sync recovery file {}: {err}", artifact_out_path.display()))?;
        if stats.source_hash != preflight_stats.source_hash
            || stats.total_bytes != preflight_stats.total_bytes
        {
            return Err("sidecar changed during recovery export".to_string());
        }

        let proof = DecodeProof {
            ts_unix_ms: now_unix_ms,
            reason,
            recovered_blocks: envelope.raptorq.k,
            proof_hash: stats.source_hash,
        };
        let mut receipt = serde_json::json!({
            "schema_version": "frankenredis_fec_recovery/v1",
            "source_artifact": path.to_string_lossy(),
            "artifact_filename": "artifact",
            "artifact_bytes": stats.total_bytes,
            "artifact_hash": &proof.proof_hash,
            "envelope_source_hash": &envelope.source_hash,
            "codec_id": envelope.codec_id.as_deref().unwrap_or(CODEC_ID_RAPTORQ_1_8_1),
            "primary_generation_verified": primary_generation_verified,
            "decode_proof": &proof,
        });
        if let Some(vg) = &verified_gen {
            receipt["generation_id"] = serde_json::Value::String(vg.record.generation_id.clone());
            receipt["generation_seq"] = serde_json::Value::Number(vg.record.generation_seq.into());
        }
        let receipt_bytes = serde_json::to_vec_pretty(&receipt)
            .map_err(|err| format!("serialize recovery proof: {err}"))?;
        // A proof is emitted only after the complete artifact has been synced.
        write_recovery_file(&resolved_output.join("proof.json"), &receipt_bytes)?;
        #[cfg(unix)]
        {
            sync_recovery_directory(&resolved_output)?;
            if let Some(parent) = resolved_output.parent() {
                sync_recovery_directory(parent)?;
            }
        }
        Ok(proof)
    };
    publish().map_err(|err| {
        format!(
            "{err}; incomplete recovery bundle retained at {}",
            output.display()
        )
    })
}

/// Atomically repair a damaged/corrupt primary artifact in place ONLY when its
/// sidecar is cryptographically authenticated against `<artifact>.generation.json`.
///
/// Guarantees:
/// 1. Refuses symlinks, missing primaries, unversioned sidecars, FEC-disabled
///    generations, tampered HMACs, superseded sidecars, and unsupported codecs.
/// 2. Decodes and verifies SHA-256 into an exclusive synced staging file first.
/// 3. Preserves the existing bad/corrupt/unsupported primary to a collision-free
///    forensic backup (`<artifact>.corrupt.<ts>.<seq>`) and fsyncs it before
///    touching the primary path. Never overwrites existing backups.
/// 4. Atomically renames the verified replacement over `artifact_path` and fsyncs
///    the parent directory.
/// 5. Writes and fsyncs an auditable JSON recovery proof artifact.
pub fn recover_sidecar_in_place(
    artifact_path: impl AsRef<std::path::Path>,
    auth_key_override: Option<&[u8]>,
    now_unix_ms: u64,
) -> Result<InPlaceRecoveryProof, String> {
    recover_sidecar_in_place_with_validation(artifact_path, auth_key_override, now_unix_ms, |_| {
        Ok(())
    })
}

/// As [`recover_sidecar_in_place`], with application-level validation of the
/// synced candidate before any forensic backup, replacement or proof is made.
pub fn recover_sidecar_in_place_with_validation(
    artifact_path: impl AsRef<std::path::Path>,
    auth_key_override: Option<&[u8]>,
    now_unix_ms: u64,
    validate: impl FnOnce(&std::path::Path) -> Result<(), String>,
) -> Result<InPlaceRecoveryProof, String> {
    use std::io::{BufWriter, Read, Write};

    let path = artifact_path.as_ref();
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!(
                "missing primary artifact {}; refusing to resurrect missing primary in place",
                path.display()
            ));
        }
        Err(err) => return Err(format!("stat primary artifact {}: {err}", path.display())),
    };
    if meta.file_type().is_symlink() {
        return Err(format!(
            "primary artifact {} is a symlink; refusing in-place recovery",
            path.display()
        ));
    }
    if !meta.is_file() {
        return Err(format!(
            "primary artifact {} is not a regular file; refusing in-place recovery",
            path.display()
        ));
    }

    let owned_key;
    let auth_key = match auth_key_override {
        Some(key) => key,
        None => {
            owned_key = load_existing_generation_auth_key(artifact_parent_dir(path))?;
            &owned_key
        }
    };
    let verified = verify_generation_binding(path, auth_key)?;

    // Step 1: Decode and verify into an exclusive staging file before touching anything.
    static NEXT_RECOVER_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let (staging_path, staging_file) = create_sidecar_staging_file(path, &NEXT_RECOVER_ID)?;
    let mut staged_writer = BufWriter::new(staging_file);
    let stats = decode_sidecar_file_to_writer(path, &verified.envelope, &mut staged_writer)
        .map_err(|err| {
            format!(
                "in-place recovery decode failed for {}: {err}; staging file retained at {}",
                path.display(),
                staging_path.display()
            )
        })?;
    let staging_file = staged_writer
        .into_inner()
        .map_err(|err| format!("flush staged recovery {}: {}", path.display(), err.error()))?;
    staging_file
        .sync_all()
        .map_err(|err| format!("sync staged recovery {}: {err}", staging_path.display()))?;
    drop(staging_file);

    if stats.source_hash != verified.record.primary_sha256
        || stats.total_bytes != verified.record.primary_len
    {
        return Err(format!(
            "decoded recovery bytes ({}, {}B) do not match authenticated generation {} ({}, {}B); staging retained at {}",
            stats.source_hash,
            stats.total_bytes,
            verified.record.generation_id,
            verified.record.primary_sha256,
            verified.record.primary_len,
            staging_path.display()
        ));
    }

    validate(&staging_path)?;

    // Step 2: Preserve the existing bad/unsupported primary to a collision-free forensic backup.
    let mut preserved_path = None;
    let mut backup_file = None;
    for seq in 0..1024_u32 {
        let candidate = append_path_suffix(path, &format!(".corrupt.{now_unix_ms}.{seq}"));
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        match opts.open(&candidate) {
            Ok(f) => {
                preserved_path = Some(candidate);
                backup_file = Some(f);
                break;
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(format!(
                    "create corrupt primary backup {}: {err}",
                    candidate.display()
                ));
            }
        }
    }
    let (preserved_path, mut backup_file) = match (preserved_path, backup_file) {
        (Some(p), Some(f)) => (p, f),
        _ => {
            return Err(format!(
                "could not allocate forensic backup path for {}",
                path.display()
            ));
        }
    };

    let mut existing_primary = std::fs::File::open(path).map_err(|err| {
        format!(
            "open existing primary {} for preservation: {err}",
            path.display()
        )
    })?;
    let mut corrupt_hasher = Sha256::new();
    let mut corrupt_bytes = 0_u64;
    let mut copy_buf = [0_u8; 64 * 1024];
    loop {
        let count = match existing_primary.read(&mut copy_buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => {
                return Err(format!(
                    "read existing primary {} during preservation: {err}",
                    path.display()
                ));
            }
        };
        backup_file.write_all(&copy_buf[..count]).map_err(|err| {
            format!(
                "write preserved primary backup {}: {err}",
                preserved_path.display()
            )
        })?;
        corrupt_hasher.update(&copy_buf[..count]);
        corrupt_bytes = corrupt_bytes
            .checked_add(count as u64)
            .ok_or_else(|| "corrupt primary length overflow".to_string())?;
    }
    backup_file.sync_all().map_err(|err| {
        format!(
            "sync preserved primary backup {}: {err}",
            preserved_path.display()
        )
    })?;
    drop(backup_file);
    drop(existing_primary);
    #[cfg(unix)]
    sync_recovery_directory(artifact_parent_dir(&preserved_path))?;
    let preserved_corrupt_sha256 = format!("sha256:{}", hex::encode(corrupt_hasher.finalize()));

    // Step 3: Atomically swap verified replacement into place + parent dir fsync.
    std::fs::rename(&staging_path, path).map_err(|err| {
        format!(
            "atomic replace primary {}: {err}; staging retained at {}, corrupt backup at {}",
            path.display(),
            staging_path.display(),
            preserved_path.display()
        )
    })?;
    #[cfg(unix)]
    sync_recovery_directory(artifact_parent_dir(path))?;

    // Step 4: Write and fsync auditable recovery proof.
    let mut proof_path = None;
    for seq in 0..1024_u32 {
        let candidate =
            append_path_suffix(path, &format!(".recovery-proof.{now_unix_ms}.{seq}.json"));
        if std::fs::symlink_metadata(&candidate).is_err() {
            proof_path = Some(candidate);
            break;
        }
    }
    let proof_path = proof_path.ok_or_else(|| {
        format!(
            "could not allocate recovery proof path for {}",
            path.display()
        )
    })?;

    let decode_proof = DecodeProof {
        ts_unix_ms: now_unix_ms,
        reason: format!(
            "authenticated in-place recovery (generation_id={}, seq={}, missing_symbols={}, damaged_symbols={})",
            verified.record.generation_id,
            verified.record.generation_seq,
            stats.missing,
            stats.damaged
        ),
        recovered_blocks: verified.envelope.raptorq.k,
        proof_hash: stats.source_hash.clone(),
    };
    let proof = InPlaceRecoveryProof {
        schema_version: IN_PLACE_RECOVERY_SCHEMA_VERSION.to_string(),
        source_artifact: path.to_string_lossy().into_owned(),
        generation_id: verified.record.generation_id,
        generation_seq: verified.record.generation_seq,
        primary_generation_verified: true,
        codec_id: verified
            .envelope
            .codec_id
            .unwrap_or_else(|| CODEC_ID_RAPTORQ_1_8_1.to_string()),
        preserved_corrupt_primary_path: preserved_path.to_string_lossy().into_owned(),
        preserved_corrupt_primary_sha256: preserved_corrupt_sha256,
        preserved_corrupt_primary_bytes: corrupt_bytes,
        recovered_primary_sha256: stats.source_hash,
        recovered_primary_bytes: stats.total_bytes,
        proof_path: proof_path.to_string_lossy().into_owned(),
        decode_proof,
    };
    let proof_bytes = serde_json::to_vec_pretty(&proof)
        .map_err(|err| format!("serialize in-place recovery proof: {err}"))?;
    write_recovery_file(&proof_path, &proof_bytes)?;
    #[cfg(unix)]
    sync_recovery_directory(artifact_parent_dir(&proof_path))?;
    Ok(proof)
}

fn resolve_new_path(path: &std::path::Path) -> Result<std::path::PathBuf, String> {
    let name = path
        .file_name()
        .ok_or_else(|| format!("path {} must name a file or new directory", path.display()))?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    let parent = std::fs::canonicalize(parent)
        .map_err(|err| format!("resolve parent of {}: {err}", path.display()))?;
    Ok(parent.join(name))
}

fn write_recovery_file(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|err| format!("create recovery file {}: {err}", path.display()))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|err| format!("write and sync recovery file {}: {err}", path.display()))
}

#[cfg(unix)]
fn sync_recovery_directory(path: &std::path::Path) -> Result<(), String> {
    std::fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|err| format!("sync recovery directory {}: {err}", path.display()))
}

struct PrimaryStreamComparer {
    primary: std::fs::File,
    buf: Vec<u8>,
    matched: bool,
    io_error: Option<String>,
}

impl PrimaryStreamComparer {
    fn new(path: &std::path::Path) -> Result<Self, String> {
        let primary = std::fs::File::open(path)
            .map_err(|err| format!("read primary artifact {}: {err}", path.display()))?;
        let meta = primary
            .metadata()
            .map_err(|err| format!("read primary artifact {}: {err}", path.display()))?;
        if !meta.is_file() {
            return Err(format!(
                "read primary artifact {}: not a regular file",
                path.display()
            ));
        }
        Ok(Self {
            primary,
            buf: Vec::new(),
            matched: true,
            io_error: None,
        })
    }

    fn finish(mut self) -> Result<bool, String> {
        use std::io::Read;

        if let Some(err) = self.io_error {
            return Err(err);
        }
        if !self.matched {
            return Ok(false);
        }
        let mut extra = [0_u8; 1];
        match self.primary.read(&mut extra) {
            Ok(0) => Ok(true),
            Ok(_) => Ok(false),
            Err(err) => Err(format!("read primary artifact tail: {err}")),
        }
    }
}

impl std::io::Write for PrimaryStreamComparer {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        use std::io::Read;

        if !self.matched || self.io_error.is_some() {
            return Ok(data.len());
        }
        self.buf.resize(data.len(), 0);
        let mut read_total = 0_usize;
        while read_total < data.len() {
            match self.primary.read(&mut self.buf[read_total..]) {
                Ok(0) => {
                    self.matched = false;
                    return Ok(data.len());
                }
                Ok(n) => read_total += n,
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(err) => {
                    self.io_error = Some(format!("read primary artifact: {err}"));
                    return Ok(data.len());
                }
            }
        }
        if self.buf != data {
            self.matched = false;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Scrub a sidecar and record verified symbol recovery without replacing the primary.
///
/// Supports both `v1` single-object and `v2` chunked streaming sidecars.
/// The readable primary must equal the decoded source, including for legacy
/// envelopes without a whole-source hash. A mismatch may mean that the sidecar
/// belongs to an older committed generation, so it is never authority to repair
/// the primary. Missing and unreadable primaries fail without changing any file.
pub fn scrub_sidecar(
    artifact_path: impl AsRef<std::path::Path>,
    now_unix_ms: u64,
) -> Result<ScrubOutcome, String> {
    let path = artifact_path.as_ref();
    let mut envelope = read_envelope_only(path)?;
    if envelope.schema_version == CHUNKED_ENVELOPE_SCHEMA_VERSION {
        let mut comparer = PrimaryStreamComparer::new(path)?;
        let stats = match decode_sidecar_file_to_writer(path, &envelope, &mut comparer) {
            Ok(stats) => stats,
            Err(reason) => return Ok(ScrubOutcome::Failed { reason }),
        };
        if !comparer.finish()? {
            return Ok(ScrubOutcome::Failed {
                reason: format!(
                    "primary artifact {} does not match the sidecar source; \
                     refusing to replace it from an unversioned sidecar",
                    path.display()
                ),
            });
        }
        if stats.damaged == 0 && stats.missing == 0 {
            if envelope.scrub.last_ok_unix_ms != now_unix_ms || envelope.scrub.status != "ok" {
                envelope.scrub.last_ok_unix_ms = now_unix_ms;
                envelope.scrub.status = "ok".to_string();
                let (envelope_path, _) = sidecar_paths(path);
                write_sidecar_file_atomically(
                    &envelope_path,
                    envelope_to_json(&envelope).as_bytes(),
                )?;
            }
            return Ok(ScrubOutcome::Clean);
        }
        let proof = DecodeProof {
            ts_unix_ms: now_unix_ms,
            proof_hash: stats.source_hash,
            reason: format!(
                "scrub repaired chunked artifact: {} missing intact, {} hash-damaged symbols",
                stats.missing, stats.damaged
            ),
            recovered_blocks: envelope.raptorq.k,
        };
        envelope.decode_proofs.push(proof.clone());
        envelope.scrub.last_ok_unix_ms = now_unix_ms;
        envelope.scrub.status = "recovered".to_string();
        let (envelope_path, _) = sidecar_paths(path);
        write_sidecar_file_atomically(&envelope_path, envelope_to_json(&envelope).as_bytes())?;
        return Ok(ScrubOutcome::Recovered {
            source: Vec::new(),
            proof,
        });
    }

    let primary = std::fs::read(path)
        .map_err(|err| format!("read primary artifact {}: {err}", path.display()))?;
    let (mut envelope, symbols) = read_sidecar(path)?;
    let decoded = match decode_verified_symbols(&envelope, &symbols) {
        Ok(decoded) => decoded,
        Err(reason) => return Ok(ScrubOutcome::Failed { reason }),
    };
    if primary != decoded.source {
        return Ok(ScrubOutcome::Failed {
            reason: format!(
                "primary artifact {} does not match the sidecar source; \
                 refusing to replace it from an unversioned sidecar",
                path.display()
            ),
        });
    }
    let outcome = scrub_decoded(&envelope, decoded, now_unix_ms);
    match &outcome {
        ScrubOutcome::Clean => {
            if envelope.scrub.last_ok_unix_ms != now_unix_ms || envelope.scrub.status != "ok" {
                envelope.scrub.last_ok_unix_ms = now_unix_ms;
                envelope.scrub.status = "ok".to_string();
                let (envelope_path, _) = sidecar_paths(path);
                let json = envelope_to_json(&envelope);
                write_sidecar_file_atomically(&envelope_path, json.as_bytes())?;
            }
        }
        ScrubOutcome::Recovered { proof, .. } => {
            envelope.decode_proofs.push(proof.clone());
            envelope.scrub.last_ok_unix_ms = now_unix_ms;
            envelope.scrub.status = "recovered".to_string();
            let (envelope_path, _) = sidecar_paths(path);
            let json = envelope_to_json(&envelope);
            write_sidecar_file_atomically(&envelope_path, json.as_bytes())?;
        }
        ScrubOutcome::Failed { .. } => {}
    }
    Ok(outcome)
}

/// Verify that a readable primary matches the source reconstructed from its sidecar.
///
/// Supports both `v1` single-object and `v2` chunked streaming sidecars.
/// This gate is read-only: it never repairs the primary or updates sidecar metadata.
pub fn verify_sidecar_gate(
    artifact_path: impl AsRef<std::path::Path>,
    _now_unix_ms: u64,
) -> Result<(), String> {
    let path = artifact_path.as_ref();
    let envelope = read_envelope_only(path)?;
    verify_primary_against_envelope(path, &envelope).map(|_| ())
}

fn verify_primary_against_envelope(
    path: &std::path::Path,
    envelope: &FecEnvelope,
) -> Result<StreamingDecodeStats, String> {
    let mut comparer = PrimaryStreamComparer::new(path)?;
    let stats = decode_sidecar_file_to_writer(path, envelope, &mut comparer)
        .map_err(|err| format!("RaptorQ scrub gate failed for {}: {err}", path.display()))?;
    if !comparer.finish()? {
        return Err(format!(
            "RaptorQ scrub gate failed for {}: primary artifact does not match the sidecar source; \
             refusing to replace it from an unversioned sidecar",
            path.display()
        ));
    }
    Ok(stats)
}

/// Read-only verification enforcing BOTH authenticated generation binding AND
/// full RaptorQ sidecar decode parity against the primary artifact on disk.
pub fn verify_generation_bound_sidecar_gate(
    artifact_path: impl AsRef<std::path::Path>,
    auth_key_override: Option<&[u8]>,
    _now_unix_ms: u64,
) -> Result<VerifiedGenerationBinding, String> {
    let path = artifact_path.as_ref();
    let owned_key;
    let auth_key = match auth_key_override {
        Some(key) => key,
        None => {
            owned_key = load_existing_generation_auth_key(artifact_parent_dir(path))?;
            &owned_key
        }
    };
    let binding = verify_generation_binding(path, auth_key)?;
    let stats = verify_primary_against_envelope(path, &binding.envelope)?;
    if stats.total_bytes != binding.record.primary_len
        || stats.source_hash != binding.record.primary_sha256
    {
        return Err("decoded sidecar does not match authenticated primary generation".to_string());
    }
    Ok(binding)
}

#[cfg(test)]
mod sidecar_publication_tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    const NOW: u64 = 1_788_700_000_000;

    #[test]
    fn automatic_chunks_respect_working_set_and_codec_limits() {
        let budget = DEFAULT_CHUNK_SIZE_BYTES;
        assert_eq!(default_chunk_size(budget, 512), None);
        assert_eq!(default_chunk_size(budget + 1, 512), Some(budget));
        assert_eq!(default_chunk_size(u64::MAX, 512), Some(budget));
        let codec_limit = u64::from(u8::MAX) * u64::from(MAX_SOURCE_SYMBOLS_PER_BLOCK);
        assert_eq!(default_chunk_size(codec_limit, 1), None);
        assert_eq!(default_chunk_size(codec_limit + 1, 1), Some(codec_limit));
    }

    fn directory(label: &str) -> PathBuf {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "fr-fec-publish-{}-{id}-{label}",
            std::process::id()
        ));
        std::fs::create_dir(&directory).expect("create private test directory");
        directory
    }

    #[test]
    fn sidecar_publication_preserves_primary_through_hard_link_destinations() {
        for target_symbols in [false, true] {
            let dir = directory("hard-link");
            let primary = dir.join("snapshot.rdb");
            let bytes = b"durably committed primary bytes\0\xff";
            std::fs::write(&primary, bytes).expect("write primary");
            let (envelope, symbols) = sidecar_paths(&primary);
            let target = if target_symbols { &symbols } else { &envelope };
            std::fs::hard_link(&primary, target).expect("link sidecar path to primary");

            write_sidecar(&primary, "state", 4, 128, NOW).expect("publish sidecars");

            assert_eq!(std::fs::read(&primary).expect("read primary"), bytes);
            verify_sidecar_gate(&primary, NOW).expect("sidecars match the untouched primary");
            assert_ne!(std::fs::read(target).expect("read sidecar"), bytes);
        }
    }

    #[test]
    #[cfg(unix)]
    fn sidecar_publication_replaces_symlinks_without_following_them() {
        use std::os::unix::fs::symlink;

        for target_symbols in [false, true] {
            let dir = directory("symlink");
            let primary = dir.join("snapshot.rdb");
            let bytes = b"committed primary must survive auxiliary publication";
            std::fs::write(&primary, bytes).expect("write primary");
            let (envelope, symbols) = sidecar_paths(&primary);
            let target = if target_symbols { &symbols } else { &envelope };
            symlink(&primary, target).expect("symlink sidecar path to primary");

            write_sidecar(&primary, "state", 4, 128, NOW).expect("publish sidecars");

            assert_eq!(std::fs::read(&primary).expect("read primary"), bytes);
            assert!(
                std::fs::symlink_metadata(target)
                    .expect("stat replaced sidecar")
                    .is_file()
            );
            verify_sidecar_gate(&primary, NOW).expect("new sidecars verify");
        }
    }

    #[test]
    #[cfg(unix)]
    fn sidecar_publication_rejects_primary_symlinks_through_a_destination() {
        use std::os::unix::fs::symlink;

        for target_symbols in [false, true] {
            for chained in [false, true] {
                let dir = directory("primary-symlink");
                let primary = dir.join("snapshot.rdb");
                let bytes = b"primary content stored at a conflicting destination";
                let (envelope, symbols) = sidecar_paths(&primary);
                let target = if target_symbols { &symbols } else { &envelope };
                let other = if target_symbols { &envelope } else { &symbols };
                if chained {
                    let source = dir.join("source.rdb");
                    std::fs::write(&source, bytes).expect("write terminal primary target");
                    symlink(&source, target).expect("link sidecar destination to source");
                } else {
                    std::fs::write(target, bytes).expect("write primary target");
                }
                symlink(target, &primary).expect("symlink primary to destination");

                let error = write_sidecar(&primary, "state", 4, 128, NOW)
                    .expect_err("publication would replace source bytes");

                assert!(error.contains("is a symlink"), "{error}");
                assert_eq!(std::fs::read(&primary).expect("read primary"), bytes);
                assert_eq!(std::fs::read(target).expect("read primary target"), bytes);
                assert!(
                    !other.exists(),
                    "no sidecar may be published on alias failure"
                );
            }
        }
    }

    #[test]
    #[cfg(unix)]
    fn sidecar_scrub_preserves_linked_envelopes_when_clean_or_recovered() {
        use std::os::unix::fs::symlink;

        for damage in [false, true] {
            for symbolic_link in [false, true] {
                let dir = directory("scrub-links");
                let primary = dir.join("snapshot.rdb");
                let bytes = b"primary data used by the real RaptorQ decoder";
                std::fs::write(&primary, bytes).expect("write primary");
                let (_, envelope, symbols_path) =
                    write_sidecar(&primary, "state", 4, 128, NOW).expect("write sidecars");
                let retained = dir.join("retained-envelope.json");
                if symbolic_link {
                    std::fs::rename(&envelope, &retained).expect("retain original envelope");
                    symlink(&retained, &envelope).expect("link published envelope");
                } else {
                    std::fs::hard_link(&envelope, &retained).expect("retain envelope hard link");
                }
                let original = std::fs::read(&retained).expect("read retained envelope");
                if damage {
                    let (_, mut packets) = read_sidecar(&primary).expect("read symbols");
                    packets.remove(0);
                    std::fs::write(&symbols_path, serialize_symbol_stream(&packets))
                        .expect("erase one source symbol");
                }

                let outcome = scrub_sidecar(&primary, NOW + 1_000).expect("scrub sidecars");

                assert_eq!(
                    matches!(outcome, ScrubOutcome::Recovered { .. }),
                    damage,
                    "unexpected scrub outcome: {outcome:?}"
                );
                assert_eq!(std::fs::read(&primary).expect("read primary"), bytes);
                assert_eq!(std::fs::read(&retained).expect("read retained"), original);
                let (updated, _) = read_sidecar(&primary).expect("read updated sidecars");
                assert_eq!(updated.scrub.last_ok_unix_ms, NOW + 1_000);
                assert_eq!(updated.decode_proofs.len(), usize::from(damage));
                verify_sidecar_gate(&primary, NOW + 2_000).expect("scrubbed sidecars verify");
            }
        }
    }

    #[test]
    fn sidecar_staging_failures_retain_previous_file_and_failed_bytes() {
        for partial in [true, false] {
            let dir = directory("staging-failure");
            let target = dir.join("snapshot.rdb.envelope.json");
            let previous = b"previous complete envelope";
            std::fs::write(&target, previous).expect("write previous destination");
            let attempted = if partial {
                &b"partial"[..]
            } else {
                &b"complete replacement awaiting sync"[..]
            };

            let error = write_sidecar_file_atomically_with(&target, |file| {
                file.write_all(attempted)?;
                Err(std::io::Error::other(if partial {
                    "injected write failure"
                } else {
                    "injected sync failure"
                }))
            })
            .expect_err("failed staging cannot publish");

            assert!(error.contains("staging file retained"), "{error}");
            assert_eq!(std::fs::read(&target).expect("read previous"), previous);
            let staging: Vec<_> = std::fs::read_dir(&dir)
                .expect("list retained files")
                .map(|entry| entry.expect("directory entry").path())
                .filter(|path| path != &target)
                .collect();
            assert_eq!(staging.len(), 1);
            assert_eq!(
                std::fs::read(&staging[0]).expect("read failed staging"),
                attempted
            );

            write_sidecar_file_atomically(&target, b"successful retry").expect("retry publication");
            assert_eq!(
                std::fs::read(&target).expect("read retry"),
                b"successful retry"
            );
            assert_eq!(
                std::fs::read(&staging[0]).expect("retained failure"),
                attempted
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn sidecar_staging_skips_existing_files_links_and_the_destination_name() {
        use std::os::unix::fs::symlink;

        let dir = directory("staging-collisions");
        let candidate = |id| dir.join(format!(".frankenredis-fec-{}-{id}.tmp", std::process::id()));
        let primary = dir.join("snapshot.rdb");
        std::fs::write(&primary, b"committed primary").expect("write primary");
        std::fs::write(candidate(0), b"unrelated staging file").expect("seed collision");
        symlink(&primary, candidate(1)).expect("seed symlink collision");
        let destination = candidate(2);

        let (staging, file) =
            create_sidecar_staging_file(&destination, &AtomicU64::new(0)).expect("unused staging");

        assert_eq!(staging, candidate(3));
        assert!(
            !destination.exists(),
            "staging must not expose the destination"
        );
        assert_eq!(
            std::fs::read(candidate(0)).expect("read collision"),
            b"unrelated staging file"
        );
        assert!(
            std::fs::symlink_metadata(candidate(1))
                .expect("stat collision")
                .is_symlink()
        );
        assert_eq!(
            std::fs::read(&primary).expect("read primary"),
            b"committed primary"
        );
        drop(file);
    }

    #[test]
    fn sidecar_publication_failure_preserves_existing_directory() {
        let dir = directory("rename-failure");
        let target = dir.join("snapshot.rdb.symbols");
        std::fs::create_dir(&target).expect("create existing directory");
        let sentinel = target.join("operator-data");
        std::fs::write(&sentinel, b"retain this file").expect("write sentinel");

        let error = write_sidecar_file_atomically(&target, b"complete symbols")
            .expect_err("cannot replace a directory with a file");

        assert!(error.contains("publish sidecar"), "{error}");
        assert!(error.contains("staging file retained"), "{error}");
        assert_eq!(
            std::fs::read(sentinel).expect("read sentinel"),
            b"retain this file"
        );
        let staging: Vec<_> = std::fs::read_dir(&dir)
            .expect("list retained files")
            .map(|entry| entry.expect("directory entry").path())
            .filter(|path| path != &target)
            .collect();
        assert_eq!(staging.len(), 1);
        assert_eq!(
            std::fs::read(&staging[0]).expect("read staging"),
            b"complete symbols"
        );
    }

    #[test]
    #[cfg(unix)]
    fn sidecar_paths_preserve_non_utf8_artifact_names() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let dir = directory("non-utf8");
        let lossy = dir.join("snapshot-\u{fffd}.rdb");
        let (unrelated_envelope, unrelated_symbols) = sidecar_paths(&lossy);
        std::fs::write(&unrelated_envelope, b"unrelated envelope").expect("write sentinel");
        std::fs::write(&unrelated_symbols, b"unrelated symbols").expect("write sentinel");

        let first = dir.join(OsString::from_vec(b"snapshot-\x80.rdb".to_vec()));
        let second = dir.join(OsString::from_vec(b"snapshot-\x81.rdb".to_vec()));
        for (path, bytes) in [
            (&first, &b"first dataset"[..]),
            (&second, &b"second dataset"[..]),
        ] {
            std::fs::write(path, bytes).expect("write non-UTF8 primary");
            write_sidecar(path, "state", 4, 128, NOW).expect("write independent sidecars");
        }

        assert_ne!(sidecar_paths(&first), sidecar_paths(&second));
        verify_sidecar_gate(&first, NOW).expect("first sidecars verify");
        verify_sidecar_gate(&second, NOW).expect("second sidecars verify");
        assert_eq!(
            std::fs::read(unrelated_envelope).expect("read sentinel"),
            b"unrelated envelope"
        );
        assert_eq!(
            std::fs::read(unrelated_symbols).expect("read sentinel"),
            b"unrelated symbols"
        );
        assert_eq!(
            corrupt_backup_path(&first).file_name(),
            Some(OsString::from_vec(b"snapshot-\x80.rdb.corrupt".to_vec()).as_os_str())
        );
    }
}

#[cfg(test)]
mod encoding_tests {
    use super::*;

    const NOW: u64 = 1_788_700_000_000;

    #[test]
    fn reader_hash_matches_empty_short_and_multi_buffer_sources() {
        for data in [Vec::new(), b"abc".to_vec(), vec![0x5a; 150_000]] {
            let (length, hash) = sha256_reader(data.as_slice()).expect("hash reader");
            assert_eq!(length, data.len() as u64);
            assert_eq!(hash, sha256_hex(&data));
        }
        let (_, hash) = sha256_reader(&b"abc"[..]).expect("known SHA-256 vector");
        assert_eq!(
            hash,
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn reader_hash_retries_interruptions_and_propagates_read_errors() {
        struct InterruptOnce {
            interrupted: bool,
            remaining: &'static [u8],
        }
        impl std::io::Read for InterruptOnce {
            fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
                if !self.interrupted {
                    self.interrupted = true;
                    return Err(std::io::ErrorKind::Interrupted.into());
                }
                std::io::Read::read(&mut self.remaining, output)
            }
        }
        let (length, hash) = sha256_reader(InterruptOnce {
            interrupted: false,
            remaining: b"complete input",
        })
        .expect("retry interrupted read");
        assert_eq!(length, 14);
        assert_eq!(hash, sha256_hex(b"complete input"));

        struct BrokenReader;
        impl std::io::Read for BrokenReader {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::PermissionDenied.into())
            }
        }
        let error = sha256_reader(BrokenReader).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn preflight_checks_both_sides_of_the_255_source_block_limit() {
        let max_len = 14_382_765_usize * 512;
        let parameters = encoding_parameters(max_len, 4, 512).expect("largest supported layout");

        assert_eq!(parameters.oti.source_blocks(), 255);
        assert_eq!(parameters.oti.transfer_length(), max_len as u64);
        assert_eq!(parameters.source_symbols, 14_382_765);
        assert_eq!(parameters.total_repair_symbols, 1_020);
        assert_eq!(parameters.padding, 0);

        let error = encoding_parameters(max_len + 1, 4, 512).unwrap_err();
        assert!(error.contains("255 source blocks"), "{error}");
        assert!(encoding_parameters(max_len + 512, 4, 512).is_err());
    }

    #[test]
    fn preflight_uses_the_aligned_symbol_size_at_the_block_limit() {
        let max_len = 14_382_765_usize * 64;
        let parameters = encoding_parameters(max_len, 4, 65).expect("aligned supported layout");

        assert_eq!(parameters.oti.symbol_size(), 64);
        assert_eq!(parameters.oti.source_blocks(), 255);
        assert_eq!(parameters.source_symbols, 14_382_765);
        assert_eq!(parameters.padding, 0);

        let error = encoding_parameters(max_len + 1, 4, 65).unwrap_err();
        assert!(error.contains("255 source blocks"), "{error}");
    }

    #[test]
    fn preflight_counts_the_repair_budget_for_every_source_block() {
        let parameters =
            encoding_parameters(56_404 * 8, 4, 8).expect("two source blocks without encoding");

        assert_eq!(parameters.oti.source_blocks(), 2);
        assert_eq!(parameters.source_symbols, 56_404);
        assert_eq!(parameters.repairs_per_block, 4);
        assert_eq!(parameters.total_repair_symbols, 8);
        assert_eq!(parameters.padding, 0);
    }

    #[test]
    fn preflight_checks_the_last_24_bit_repair_id() {
        // A one-symbol source block extends to K'=10. Repair IDs start
        // there, so 2^24-10 repairs end exactly at the largest valid ID.
        let max_repairs = (1_usize << 24) - 10;
        let parameters =
            encoding_parameters(1, max_repairs, 1).expect("largest supported repair budget");

        assert_eq!(parameters.source_symbols, 1);
        assert_eq!(parameters.repairs_per_block as usize, max_repairs);
        assert_eq!(parameters.total_repair_symbols as usize, max_repairs);
        assert!(encoding_parameters(1, max_repairs + 1, 1).is_err());
    }

    #[test]
    fn preflight_rejects_padding_overflow_and_invalid_parameters() {
        assert!(encoding_parameters(usize::MAX, 4, 512).is_err());
        assert!(encoding_parameters(1, usize::MAX, 128).is_err());
        assert!(encoding_parameters(1, 0, 128).is_err());
        assert!(encoding_parameters(1, 4, 0).is_err());

        // These public calls must return errors without reaching the
        // codec's assertions or allocating the requested repair budget.
        assert!(encode_artifact("large-budget", "test", b"x", usize::MAX, 128, NOW).is_err());
        assert!(encode_artifact("zero-budget", "test", b"x", 0, 128, NOW).is_err());
        assert!(encode_artifact("zero-symbol", "test", b"x", 4, 0, NOW).is_err());
    }

    #[test]
    fn empty_artifact_round_trips_through_the_real_codec() {
        let encoded = encode_artifact("empty", "test", b"", 4, 128, NOW).expect("encode empty");

        assert_eq!(encoded.envelope.padding, 128);
        assert_eq!(encoded.envelope.raptorq.k, 1);
        assert_eq!(encoded.envelope.raptorq.repair_symbols, 4);
        assert_eq!(encoded.symbols.len(), 5);
        assert!(encoded.envelope.raptorq.overhead_ratio.is_finite());
        assert!(matches!(
            scrub(&encoded.envelope, &encoded.symbols, NOW),
            ScrubOutcome::Clean
        ));

        let (source, proof) = decode_artifact(&encoded.envelope, &encoded.symbols, "empty", NOW)
            .expect("decode empty");
        assert!(source.is_empty());
        assert_eq!(proof.proof_hash, sha256_hex(b""));
    }

    #[test]
    fn unaligned_packet_limit_records_actual_symbol_size_and_round_trips() {
        let data = vec![0xa5; 66];
        let encoded =
            encode_artifact("aligned", "test", &data, 4, 65, NOW).expect("encode aligned symbols");
        let oti = ObjectTransmissionInformation::deserialize(&encoded.envelope.oti);

        assert_eq!(encoded.envelope.symbol_size, 64);
        assert_eq!(oti.symbol_size(), 64);
        assert_eq!(oti.transfer_length(), 128);
        assert_eq!(encoded.envelope.padding, 62);
        assert_eq!(encoded.envelope.raptorq.k, 2);
        assert_eq!(encoded.envelope.raptorq.repair_symbols, 4);
        assert_eq!(encoded.symbols.len(), 6);
        assert!(
            encoded
                .symbols
                .iter()
                .all(|packet| packet.data().len() == 64)
        );

        let (source, _) =
            decode_artifact(&encoded.envelope, &encoded.symbols, "aligned", NOW).expect("decode");
        assert_eq!(source, data);
    }
}
