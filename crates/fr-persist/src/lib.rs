#![forbid(unsafe_code)]

use std::io::Write;
use std::path::Path;

use fr_protocol::{RespFrame, RespParseError, parse_frame};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AofRecord {
    pub argv: Vec<Vec<u8>>,
}

#[derive(Debug)]
pub enum PersistError {
    InvalidFrame,
    Parse(RespParseError),
    Io(std::io::Error),
}

impl PartialEq for PersistError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::InvalidFrame, Self::InvalidFrame) => true,
            (Self::Parse(a), Self::Parse(b)) => a == b,
            (Self::Io(_), Self::Io(_)) => false, // I/O errors are not structurally comparable
            _ => false,
        }
    }
}

impl Eq for PersistError {}

impl From<RespParseError> for PersistError {
    fn from(value: RespParseError) -> Self {
        Self::Parse(value)
    }
}

impl From<std::io::Error> for PersistError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl AofRecord {
    #[must_use]
    pub fn to_resp_frame(&self) -> RespFrame {
        let args = self
            .argv
            .iter()
            .map(|arg| RespFrame::BulkString(Some(arg.clone())))
            .collect();
        RespFrame::Array(Some(args))
    }

    pub fn from_resp_frame(frame: &RespFrame) -> Result<Self, PersistError> {
        let RespFrame::Array(Some(items)) = frame else {
            return Err(PersistError::InvalidFrame);
        };
        if items.is_empty() {
            return Err(PersistError::InvalidFrame);
        }
        let mut argv = Vec::with_capacity(items.len());
        for item in items {
            match item {
                RespFrame::BulkString(Some(bytes)) => argv.push(bytes.clone()),
                RespFrame::SimpleString(text) => argv.push(text.as_bytes().to_vec()),
                RespFrame::Integer(n) => argv.push(n.to_string().as_bytes().to_vec()),
                _ => return Err(PersistError::InvalidFrame),
            }
        }
        Ok(Self { argv })
    }
}

#[must_use]
pub fn encode_aof_stream(records: &[AofRecord]) -> Vec<u8> {
    let mut out = Vec::new();
    for record in records {
        out.extend_from_slice(&record.to_resp_frame().to_bytes());
    }
    out
}

pub fn decode_aof_stream(input: &[u8]) -> Result<Vec<AofRecord>, PersistError> {
    let mut cursor = 0usize;
    let mut out = Vec::new();
    while cursor < input.len() {
        let parsed = parse_frame(&input[cursor..])?;
        let record = AofRecord::from_resp_frame(&parsed.frame)?;
        out.push(record);
        cursor = cursor.saturating_add(parsed.consumed);
    }
    Ok(out)
}

/// Convert a list of command argv vectors (from `Store::to_aof_commands()`)
/// into `AofRecord` entries suitable for encoding.
#[must_use]
pub fn argv_to_aof_records(commands: Vec<Vec<Vec<u8>>>) -> Vec<AofRecord> {
    commands
        .into_iter()
        .map(|argv| AofRecord { argv })
        .collect()
}

/// Write AOF records to a file at the given path.
///
/// Writes atomically by first writing to a temporary file, then renaming.
/// This prevents corruption if the process crashes mid-write.
pub fn write_aof_file(path: &Path, records: &[AofRecord]) -> Result<(), PersistError> {
    let encoded = encode_aof_stream(records);
    let tmp_path = path.with_extension("tmp");
    let mut file = std::fs::File::create(&tmp_path)?;
    file.write_all(&encoded)?;
    file.sync_all()?;
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

/// Read and decode AOF records from a file at the given path.
///
/// Returns an empty vector if the file does not exist.
pub fn read_aof_file(path: &Path) -> Result<Vec<AofRecord>, PersistError> {
    match std::fs::read(path) {
        Ok(data) => {
            if data.is_empty() {
                return Ok(Vec::new());
            }
            decode_aof_stream(&data)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(PersistError::Io(e)),
    }
}

#[cfg(test)]
mod tests {
    use fr_protocol::{RespFrame, RespParseError};

    use super::{AofRecord, PersistError, decode_aof_stream, encode_aof_stream};

    #[test]
    fn round_trip_aof_record() {
        let record = AofRecord {
            argv: vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()],
        };
        let frame = record.to_resp_frame();
        let decoded = AofRecord::from_resp_frame(&frame).expect("decode");
        assert_eq!(decoded, record);
    }

    #[test]
    fn invalid_frame_rejected() {
        let frame = RespFrame::BulkString(Some(b"x".to_vec()));
        assert!(AofRecord::from_resp_frame(&frame).is_err());
    }

    #[test]
    fn empty_array_record_rejected() {
        let frame = RespFrame::Array(Some(Vec::new()));
        let err = AofRecord::from_resp_frame(&frame).expect_err("must fail");
        assert_eq!(err, PersistError::InvalidFrame);
    }

    #[test]
    fn round_trip_multi_record_stream() {
        let records = vec![
            AofRecord {
                argv: vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()],
            },
            AofRecord {
                argv: vec![b"INCR".to_vec(), b"counter".to_vec()],
            },
        ];
        let encoded = encode_aof_stream(&records);
        let decoded = decode_aof_stream(&encoded).expect("decode stream");
        assert_eq!(decoded, records);
    }

    #[test]
    fn decode_rejects_invalid_stream_frame() {
        let err = decode_aof_stream(b"$3\r\nbad\r\n").expect_err("must fail");
        assert_eq!(err, PersistError::InvalidFrame);
    }

    #[test]
    fn decode_rejects_empty_command_array_record() {
        let err = decode_aof_stream(b"*0\r\n").expect_err("must fail");
        assert_eq!(err, PersistError::InvalidFrame);
    }

    #[test]
    fn decode_rejects_incomplete_stream() {
        let err = decode_aof_stream(b"*2\r\n$3\r\nGET\r\n$1\r\nk").expect_err("must fail");
        assert_eq!(err, PersistError::Parse(RespParseError::Incomplete));
    }

    #[test]
    fn argv_to_aof_records_converts() {
        let commands = vec![
            vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()],
            vec![
                b"HSET".to_vec(),
                b"h".to_vec(),
                b"f".to_vec(),
                b"v".to_vec(),
            ],
        ];
        let records = super::argv_to_aof_records(commands);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].argv[0], b"SET");
        assert_eq!(records[1].argv[0], b"HSET");
    }

    #[test]
    fn write_and_read_aof_file_round_trip() {
        let dir = std::env::temp_dir().join("fr_persist_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test.aof");

        let records = vec![
            AofRecord {
                argv: vec![b"SET".to_vec(), b"key1".to_vec(), b"val1".to_vec()],
            },
            AofRecord {
                argv: vec![
                    b"RPUSH".to_vec(),
                    b"list1".to_vec(),
                    b"a".to_vec(),
                    b"b".to_vec(),
                ],
            },
        ];

        super::write_aof_file(&path, &records).expect("write");
        let loaded = super::read_aof_file(&path).expect("read");
        assert_eq!(loaded, records);

        // Cleanup
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_aof_file_missing_returns_empty() {
        let path = std::path::Path::new("/tmp/fr_persist_nonexistent_test_file.aof");
        let loaded = super::read_aof_file(path).expect("read missing");
        assert!(loaded.is_empty());
    }
}
