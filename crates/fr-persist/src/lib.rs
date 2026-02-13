#![forbid(unsafe_code)]

use fr_protocol::RespFrame;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AofRecord {
    pub argv: Vec<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersistError {
    InvalidFrame,
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

#[cfg(test)]
mod tests {
    use fr_protocol::RespFrame;

    use super::AofRecord;

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
}
