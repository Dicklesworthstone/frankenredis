//! # fr-fec — RaptorQ-Everywhere durability sidecar (spec §9/§19)
//!
//! Systematic forward-error-correction envelopes for FrankenRedis evidence
//! artifacts: conformance fixture bundles, benchmark baselines, migration
//! manifests, reproducibility ledgers, and release-grade state artifacts.
//!
//! The codec is the `raptorq` crate (RFC 6330, systematic): the first `k`
//! emitted packets ARE the source symbols, so a consumer that receives
//! everything needs no decoding at all — the FEC layer pays off only when
//! symbols are lost or corrupted, which is the durability drill (spec §18 G6).
//!
//! Envelope schema follows spec §19 (`frankenredis_fec_envelope/v1`) with one
//! documented normalization: §19's illustrative `"blake3:..."` hash prefix is
//! emitted as `"sha256:..."` — SHA-256 is the repo-wide hash convention
//! (BENCH_METHODOLOGY, ELF identification) and this crate follows it instead
//! of introducing a second digest family.
#![forbid(unsafe_code)]

use raptorq::Encoder;
use raptorq::EncodingPacket;
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
    /// SHA-256 of each serialized symbol, source symbols first.
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

/// The encoded form of an artifact: the envelope plus every serialized symbol
/// (source symbols first, then repair symbols).
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

/// Wrap `data` in a systematic RaptorQ encoding: `k` source symbols plus
/// `repair_symbols` repair symbols of `symbol_size` bytes each.
///
/// `repair_symbols` must be >= 1; with zero repair symbols the envelope is
/// pure checksum and the drill is vacuous. The repair count is the durability
/// budget: up to `repair_symbols` lost symbols keep the artifact recoverable.
pub fn encode_artifact(
    artifact_id: &str,
    artifact_type: &str,
    data: &[u8],
    repair_symbols: usize,
    symbol_size: u16,
    now_unix_ms: u64,
) -> Result<EncodedArtifact, String> {
    if repair_symbols == 0 {
        return Err("repair_symbols must be >= 1".to_string());
    }
    if symbol_size == 0 {
        return Err("symbol_size must be >= 1".to_string());
    }
    let padding = (symbol_size as usize - data.len() % symbol_size as usize) % symbol_size as usize;
    let mut padded = Vec::with_capacity(data.len() + padding);
    padded.extend_from_slice(data);
    padded.resize(data.len() + padding, 0);

    let encoder = Encoder::with_defaults(&padded, symbol_size);
    let oti = encoder.get_config();
    // `get_encoded_packets(n)` returns the INTERNAL source symbol count plus
    // n repair symbols; the source count is the encoder's decision (OTI
    // alignment), never the caller's naive ceil-division.
    let packets = encoder.get_encoded_packets(repair_symbols as u32);
    let k = packets.len() - repair_symbols;

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
            padding: padding as u32,
            symbol_size,
            oti: oti.serialize(),
            raptorq: RaptorQSection {
                k: k as u32,
                repair_symbols: repair_symbols as u32,
                overhead_ratio: repair_symbols as f64 / k as f64,
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
/// `Clean` means all `k` source symbols are present, every hash matches, and
/// the reconstructed source still matches `envelope.source_hash`.
/// `Recovered` means the run found damage but repaired from the retained
/// repair symbols (the caller re-persists the rebuilt symbols and appends the
/// decode proof). `Failed` means the artifact is past repair: hashes disagree
/// or not enough symbols survive.
#[derive(Debug)]
pub enum ScrubOutcome {
    Clean,
    Recovered { source: Vec<u8>, proof: DecodeProof },
    Failed { reason: String },
}

pub fn scrub(envelope: &FecEnvelope, symbols: &[EncodingPacket], now_unix_ms: u64) -> ScrubOutcome {
    let hashes: std::collections::HashSet<&String> =
        envelope.raptorq.symbol_hashes.iter().collect();
    let mut damaged = 0_usize;
    for packet in symbols {
        let digest = sha256_hex(&packet.serialize());
        if !hashes.contains(&digest) {
            damaged += 1;
        }
    }
    let total = envelope.raptorq.k as usize + envelope.raptorq.repair_symbols as usize;
    let missing = total.saturating_sub(symbols.len());

    let mut decoder = raptorq::Decoder::new(raptorq::ObjectTransmissionInformation::deserialize(
        &envelope.oti,
    ));
    let mut reconstructed: Option<Vec<u8>> = None;
    for packet in symbols {
        if reconstructed.is_some() {
            break;
        }
        if let Some(mut padded) = decoder.decode(packet.clone()) {
            padded.truncate(padded.len() - envelope.padding as usize);
            reconstructed = Some(padded);
        }
    }
    let Some(source) = reconstructed else {
        return ScrubOutcome::Failed {
            reason: format!(
                "undecodable: not enough intact symbols; {missing} missing, {damaged} hash-damaged"
            ),
        };
    };
    if !envelope.source_hash.is_empty() && sha256_hex(&source) != envelope.source_hash {
        return ScrubOutcome::Failed {
            reason: "reconstructed source hash does not match envelope.source_hash".to_string(),
        };
    }

    if damaged == 0 && missing == 0 {
        ScrubOutcome::Clean
    } else {
        let reason =
            format!("scrub repaired artifact: {missing} missing, {damaged} hash-damaged symbols");
        let proof = DecodeProof {
            ts_unix_ms: now_unix_ms,
            proof_hash: sha256_hex(&source),
            reason: reason.clone(),
            recovered_blocks: envelope.raptorq.k,
        };
        ScrubOutcome::Recovered { source, proof }
    }
}

/// DECODE PROOF (§9 output 3): recover the source from `symbols`, verify it
/// against the envelope, and return the source with a proof ready to append
/// to `envelope.decode_proofs` before re-persisting the envelope.
pub fn decode_artifact(
    envelope: &FecEnvelope,
    symbols: &[EncodingPacket],
    reason: &str,
    now_unix_ms: u64,
) -> Result<(Vec<u8>, DecodeProof), String> {
    let mut decoder = raptorq::Decoder::new(raptorq::ObjectTransmissionInformation::deserialize(
        &envelope.oti,
    ));
    let mut reconstructed: Option<Vec<u8>> = None;
    for packet in symbols {
        if reconstructed.is_some() {
            break;
        }
        if let Some(mut padded) = decoder.decode(packet.clone()) {
            padded.truncate(padded.len() - envelope.padding as usize);
            reconstructed = Some(padded);
        }
    }
    let Some(source) = reconstructed else {
        return Err(format!(
            "undecodable: not enough intact symbols against k={}",
            envelope.raptorq.k
        ));
    };
    let actual = sha256_hex(&source);
    if !envelope.source_hash.is_empty() && actual != envelope.source_hash {
        return Err(format!(
            "recovered source hash {actual} does not match envelope.source_hash {}",
            envelope.source_hash
        ));
    }
    let proof = DecodeProof {
        ts_unix_ms: now_unix_ms,
        reason: reason.to_string(),
        recovered_blocks: envelope.raptorq.k,
        proof_hash: actual,
    };
    Ok((source, proof))
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
    let mut packets = Vec::with_capacity(count);
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
        packets.push(deserialize_symbol(packet_bytes)?);
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

/// Verify that an artifact's sidecar exists and passes the scrub gate.
pub fn verify_sidecar_gate(
    artifact_path: impl AsRef<std::path::Path>,
    now_unix_ms: u64,
) -> Result<(), String> {
    let path = artifact_path.as_ref();
    let outcome = scrub_sidecar(path, now_unix_ms)?;
    match outcome {
        ScrubOutcome::Clean | ScrubOutcome::Recovered { .. } => Ok(()),
        ScrubOutcome::Failed { reason } => Err(format!(
            "RaptorQ scrub gate failed for {}: {reason}",
            path.display()
        )),
    }
}
