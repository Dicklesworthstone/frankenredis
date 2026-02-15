#![forbid(unsafe_code)]

use fr_protocol::{RespFrame, RespParseError, parse_frame};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AofRecord {
    pub argv: Vec<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersistError {
    InvalidFrame,
    Parse(RespParseError),
}

impl From<RespParseError> for PersistError {
    fn from(value: RespParseError) -> Self {
        Self::Parse(value)
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
    fn decode_rejects_incomplete_stream() {
        let err = decode_aof_stream(b"*2\r\n$3\r\nGET\r\n$1\r\nk").expect_err("must fail");
        assert_eq!(err, PersistError::Parse(RespParseError::Incomplete));
    }
}
