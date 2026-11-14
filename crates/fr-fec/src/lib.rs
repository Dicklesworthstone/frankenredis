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
    let oti = ObjectTransmissionInformation::deserialize(&envelope.oti);
    let symbol_size = usize::from(oti.symbol_size());
    let source_blocks = u64::from(oti.source_blocks());
    if symbol_size == 0 || source_blocks == 0 || oti.transfer_length() == 0 {
        return Err("invalid OTI: zero transfer length, symbol size, or source blocks".to_string());
    }
    let alignment = u16::from(oti.symbol_alignment());
    if alignment == 0
        || oti.symbol_size() % alignment != 0
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
    let envelope = std::path::PathBuf::from(format!("{}.envelope.json", p.display()));
    let symbols = std::path::PathBuf::from(format!("{}.symbols", p.display()));
    (envelope, symbols)
}

/// Path where a corrupted primary artifact is preserved before sidecar recovery overwrites it.
pub fn corrupt_backup_path(artifact_path: impl AsRef<std::path::Path>) -> std::path::PathBuf {
    let p = artifact_path.as_ref();
    std::path::PathBuf::from(format!("{}.corrupt", p.display()))
}

/// Write sidecar envelope and symbols files alongside an artifact.
pub fn write_sidecar(
    artifact_path: impl AsRef<std::path::Path>,
    artifact_type: &str,
    repair_symbols: usize,
    symbol_size: u16,
    now_unix_ms: u64,
) -> Result<(FecEnvelope, std::path::PathBuf, std::path::PathBuf), String> {
    let path = artifact_path.as_ref();
    let metadata =
        std::fs::metadata(path).map_err(|err| format!("stat {}: {err}", path.display()))?;
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
    let (envelope_path, symbols_path) = sidecar_paths(path);
    if let Some(parent) = envelope_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let envelope_json = envelope_to_json(&encoded.envelope);
    std::fs::write(&envelope_path, envelope_json)
        .map_err(|err| format!("write {}: {err}", envelope_path.display()))?;
    let symbols_bytes = serialize_symbol_stream(&encoded.symbols);
    std::fs::write(&symbols_path, symbols_bytes)
        .map_err(|err| format!("write {}: {err}", symbols_path.display()))?;
    Ok((encoded.envelope, envelope_path, symbols_path))
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

/// Scrub an artifact's sidecar, updating status and proofs on disk when recovered.
/// Also verifies that an existing primary file at `artifact_path` matches
/// `envelope.source_hash`; if the primary on disk disagrees with `envelope.source_hash`,
/// the corrupt primary is preserved at `<path>.corrupt` before overwriting with the
/// recovered payload.
pub fn scrub_sidecar(
    artifact_path: impl AsRef<std::path::Path>,
    now_unix_ms: u64,
) -> Result<ScrubOutcome, String> {
    let path = artifact_path.as_ref();
    let (mut envelope, symbols) = read_sidecar(path)?;
    let mut outcome = scrub(&envelope, &symbols, now_unix_ms);
    let primary_mismatch = std::fs::read(path).ok().is_some_and(|disk_bytes| {
        !envelope.source_hash.is_empty() && sha256_hex(&disk_bytes) != envelope.source_hash
    });
    if matches!(outcome, ScrubOutcome::Clean)
        && primary_mismatch
        && let Ok((source, proof)) = decode_artifact(
            &envelope,
            &symbols,
            "primary artifact hash mismatch against envelope.source_hash; recovered from intact RaptorQ sidecar",
            now_unix_ms,
        )
    {
        outcome = ScrubOutcome::Recovered { source, proof };
    }
    match &outcome {
        ScrubOutcome::Clean => {
            if envelope.scrub.last_ok_unix_ms != now_unix_ms || envelope.scrub.status != "ok" {
                envelope.scrub.last_ok_unix_ms = now_unix_ms;
                envelope.scrub.status = "ok".to_string();
                let (envelope_path, _) = sidecar_paths(path);
                let json = envelope_to_json(&envelope);
                std::fs::write(&envelope_path, json)
                    .map_err(|err| format!("update envelope {}: {err}", envelope_path.display()))?;
            }
        }
        ScrubOutcome::Recovered { source, proof } => {
            if path.is_file()
                && std::fs::read(path)
                    .map(|existing| existing != *source)
                    .unwrap_or(false)
            {
                let backup = corrupt_backup_path(path);
                std::fs::copy(path, &backup).map_err(|err| {
                    format!(
                        "preserve corrupt primary {} -> {}: {err}",
                        path.display(),
                        backup.display()
                    )
                })?;
            }
            envelope.decode_proofs.push(proof.clone());
            envelope.scrub.status = "recovered".to_string();
            let (envelope_path, _) = sidecar_paths(path);
            let json = envelope_to_json(&envelope);
            std::fs::write(&envelope_path, json)
                .map_err(|err| format!("update envelope {}: {err}", envelope_path.display()))?;
            std::fs::write(path, source)
                .map_err(|err| format!("write restored artifact {}: {err}", path.display()))?;
        }
        ScrubOutcome::Failed { .. } => {}
    }
    Ok(outcome)
}

/// Verify that a readable primary matches the source reconstructed from its sidecar.
///
/// This gate is read-only: it never repairs the primary or updates sidecar metadata.
/// Recoverable symbol damage is accepted only when the reconstructed bytes match
/// the existing primary, including for legacy envelopes without `source_hash`.
pub fn verify_sidecar_gate(
    artifact_path: impl AsRef<std::path::Path>,
    now_unix_ms: u64,
) -> Result<(), String> {
    let path = artifact_path.as_ref();
    let primary = std::fs::read(path)
        .map_err(|err| format!("read primary artifact {}: {err}", path.display()))?;
    let (envelope, symbols) = read_sidecar(path)?;
    let (source, _) = decode_artifact(
        &envelope,
        &symbols,
        "read-only sidecar gate verification",
        now_unix_ms,
    )
    .map_err(|err| format!("RaptorQ scrub gate failed for {}: {err}", path.display()))?;
    if primary != source {
        return Err(format!(
            "RaptorQ scrub gate failed for {}: primary artifact does not match the sidecar source; \
             refusing to replace it from an unversioned sidecar",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod encoding_tests {
    use super::*;

    const NOW: u64 = 1_788_700_000_000;

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
