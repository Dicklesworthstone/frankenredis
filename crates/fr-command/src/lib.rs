#![forbid(unsafe_code)]

use fr_protocol::RespFrame;
use fr_store::{PttlValue, Store, StoreError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandError {
    InvalidCommandFrame,
    InvalidUtf8Argument,
    UnknownCommand {
        command: String,
        args_preview: Option<String>,
    },
    WrongArity(&'static str),
    InvalidInteger,
    SyntaxError,
    Store(StoreError),
}

impl From<StoreError> for CommandError {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}

pub fn frame_to_argv(frame: &RespFrame) -> Result<Vec<Vec<u8>>, CommandError> {
    let RespFrame::Array(Some(items)) = frame else {
        return Err(CommandError::InvalidCommandFrame);
    };

    let mut argv = Vec::with_capacity(items.len());
    for item in items {
        match item {
            RespFrame::BulkString(Some(bytes)) => argv.push(bytes.clone()),
            RespFrame::SimpleString(text) => argv.push(text.as_bytes().to_vec()),
            RespFrame::Integer(n) => argv.push(n.to_string().into_bytes()),
            _ => return Err(CommandError::InvalidCommandFrame),
        }
    }
    if argv.is_empty() {
        return Err(CommandError::InvalidCommandFrame);
    }
    Ok(argv)
}

pub fn dispatch_argv(
    argv: &[Vec<u8>],
    store: &mut Store,
    now_ms: u64,
) -> Result<RespFrame, CommandError> {
    let cmd = std::str::from_utf8(&argv[0]).map_err(|_| CommandError::InvalidUtf8Argument)?;
    if cmd.eq_ignore_ascii_case("PING") {
        return ping(argv);
    }
    if cmd.eq_ignore_ascii_case("ECHO") {
        return echo(argv);
    }
    if cmd.eq_ignore_ascii_case("SET") {
        return set(argv, store, now_ms);
    }
    if cmd.eq_ignore_ascii_case("GET") {
        return get(argv, store, now_ms);
    }
    if cmd.eq_ignore_ascii_case("DEL") {
        return del(argv, store, now_ms);
    }
    if cmd.eq_ignore_ascii_case("INCR") {
        return incr(argv, store, now_ms);
    }
    if cmd.eq_ignore_ascii_case("EXPIRE") {
        return expire(argv, store, now_ms);
    }
    if cmd.eq_ignore_ascii_case("PTTL") {
        return pttl(argv, store, now_ms);
    }

    let args_preview = build_unknown_args_preview(argv);
    Err(CommandError::UnknownCommand {
        command: trim_and_cap_string(cmd, 128),
        args_preview,
    })
}

fn ping(argv: &[Vec<u8>]) -> Result<RespFrame, CommandError> {
    match argv.len() {
        1 => Ok(RespFrame::SimpleString("PONG".to_string())),
        2 => Ok(RespFrame::BulkString(Some(argv[1].clone()))),
        _ => Err(CommandError::WrongArity("PING")),
    }
}

fn echo(argv: &[Vec<u8>]) -> Result<RespFrame, CommandError> {
    if argv.len() != 2 {
        return Err(CommandError::WrongArity("ECHO"));
    }
    Ok(RespFrame::BulkString(Some(argv[1].clone())))
}

fn set(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 3 && argv.len() != 5 {
        return Err(CommandError::WrongArity("SET"));
    }
    let mut px_ttl_ms = None;
    if argv.len() == 5 {
        let option =
            std::str::from_utf8(&argv[3]).map_err(|_| CommandError::InvalidUtf8Argument)?;
        if !option.eq_ignore_ascii_case("PX") {
            return Err(CommandError::SyntaxError);
        }
        let ttl = parse_u64_arg(&argv[4])?;
        px_ttl_ms = Some(ttl);
    }
    store.set(argv[1].clone(), argv[2].clone(), px_ttl_ms, now_ms);
    Ok(RespFrame::SimpleString("OK".to_string()))
}

fn get(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 2 {
        return Err(CommandError::WrongArity("GET"));
    }
    Ok(RespFrame::BulkString(store.get(&argv[1], now_ms)))
}

fn del(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() < 2 {
        return Err(CommandError::WrongArity("DEL"));
    }
    let removed = store.del(&argv[1..], now_ms);
    let removed = i64::try_from(removed).unwrap_or(i64::MAX);
    Ok(RespFrame::Integer(removed))
}

fn incr(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 2 {
        return Err(CommandError::WrongArity("INCR"));
    }
    let value = store.incr(&argv[1], now_ms)?;
    Ok(RespFrame::Integer(value))
}

fn expire(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 3 {
        return Err(CommandError::WrongArity("EXPIRE"));
    }
    let seconds = parse_i64_arg(&argv[2])?;
    let applied = store.expire_seconds(&argv[1], seconds, now_ms);
    Ok(RespFrame::Integer(if applied { 1 } else { 0 }))
}

fn pttl(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 2 {
        return Err(CommandError::WrongArity("PTTL"));
    }
    let value = match store.pttl(&argv[1], now_ms) {
        PttlValue::KeyMissing => -2,
        PttlValue::NoExpiry => -1,
        PttlValue::Remaining(ms) => ms,
    };
    Ok(RespFrame::Integer(value))
}

fn parse_i64_arg(arg: &[u8]) -> Result<i64, CommandError> {
    let text = std::str::from_utf8(arg).map_err(|_| CommandError::InvalidUtf8Argument)?;
    text.parse::<i64>()
        .map_err(|_| CommandError::InvalidInteger)
}

fn parse_u64_arg(arg: &[u8]) -> Result<u64, CommandError> {
    let text = std::str::from_utf8(arg).map_err(|_| CommandError::InvalidUtf8Argument)?;
    text.parse::<u64>()
        .map_err(|_| CommandError::InvalidInteger)
}

fn build_unknown_args_preview(argv: &[Vec<u8>]) -> Option<String> {
    if argv.len() < 2 {
        return None;
    }

    let mut out = String::new();
    for arg in &argv[1..] {
        if out.len() >= 128 {
            break;
        }
        let remaining = 128_usize.saturating_sub(out.len());
        if remaining < 3 {
            break;
        }

        let text = String::from_utf8_lossy(arg);
        let sanitized = text.replace(['\r', '\n'], " ");
        let capped = trim_and_cap_string(&sanitized, remaining.saturating_sub(3));
        out.push('\'');
        out.push_str(&capped);
        out.push_str("' ");
    }

    if out.is_empty() { None } else { Some(out) }
}

fn trim_and_cap_string(input: &str, cap: usize) -> String {
    let mut out = String::new();
    for ch in input.chars() {
        if out.len() + ch.len_utf8() > cap {
            break;
        }
        if ch == '\r' || ch == '\n' {
            out.push(' ');
        } else {
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use fr_protocol::RespFrame;
    use fr_store::Store;

    use super::{dispatch_argv, frame_to_argv};

    #[test]
    fn ping_works() {
        let frame = RespFrame::Array(Some(vec![RespFrame::BulkString(Some(b"PING".to_vec()))]));
        let argv = frame_to_argv(&frame).expect("argv");
        let mut store = Store::new();
        let out = dispatch_argv(&argv, &mut store, 0).expect("dispatch");
        assert_eq!(out, RespFrame::SimpleString("PONG".to_string()));
    }

    #[test]
    fn set_get_round_trip() {
        let mut store = Store::new();
        let set = vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()];
        let get = vec![b"GET".to_vec(), b"k".to_vec()];
        dispatch_argv(&set, &mut store, 10).expect("set");
        let out = dispatch_argv(&get, &mut store, 10).expect("get");
        assert_eq!(out, RespFrame::BulkString(Some(b"v".to_vec())));
    }

    #[test]
    fn unknown_command_contains_args_preview() {
        let mut store = Store::new();
        let argv = vec![b"NOPE".to_vec(), b"a".to_vec(), b"b".to_vec()];
        let err = dispatch_argv(&argv, &mut store, 0).expect_err("must fail");
        match err {
            super::CommandError::UnknownCommand {
                command,
                args_preview,
            } => {
                assert_eq!(command, "NOPE");
                assert_eq!(args_preview.as_deref(), Some("'a' 'b' "));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
