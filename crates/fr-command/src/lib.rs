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
    NoSuchKey,
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
    if cmd.eq_ignore_ascii_case("PEXPIRE") {
        return pexpire(argv, store, now_ms);
    }
    if cmd.eq_ignore_ascii_case("EXPIREAT") {
        return expireat(argv, store, now_ms);
    }
    if cmd.eq_ignore_ascii_case("PEXPIREAT") {
        return pexpireat(argv, store, now_ms);
    }
    if cmd.eq_ignore_ascii_case("PTTL") {
        return pttl(argv, store, now_ms);
    }
    if cmd.eq_ignore_ascii_case("APPEND") {
        return append(argv, store, now_ms);
    }
    if cmd.eq_ignore_ascii_case("STRLEN") {
        return strlen(argv, store, now_ms);
    }
    if cmd.eq_ignore_ascii_case("MGET") {
        return mget(argv, store, now_ms);
    }
    if cmd.eq_ignore_ascii_case("MSET") {
        return mset(argv, store, now_ms);
    }
    if cmd.eq_ignore_ascii_case("SETNX") {
        return setnx(argv, store, now_ms);
    }
    if cmd.eq_ignore_ascii_case("GETSET") {
        return getset(argv, store, now_ms);
    }
    if cmd.eq_ignore_ascii_case("INCRBY") {
        return incrby(argv, store, now_ms);
    }
    if cmd.eq_ignore_ascii_case("DECRBY") {
        return decrby(argv, store, now_ms);
    }
    if cmd.eq_ignore_ascii_case("DECR") {
        return decr(argv, store, now_ms);
    }
    if cmd.eq_ignore_ascii_case("EXISTS") {
        return exists(argv, store, now_ms);
    }
    if cmd.eq_ignore_ascii_case("TTL") {
        return ttl(argv, store, now_ms);
    }
    if cmd.eq_ignore_ascii_case("EXPIRETIME") {
        return expiretime(argv, store, now_ms);
    }
    if cmd.eq_ignore_ascii_case("PEXPIRETIME") {
        return pexpiretime(argv, store, now_ms);
    }
    if cmd.eq_ignore_ascii_case("PERSIST") {
        return persist(argv, store, now_ms);
    }
    if cmd.eq_ignore_ascii_case("TYPE") {
        return type_cmd(argv, store, now_ms);
    }
    if cmd.eq_ignore_ascii_case("RENAME") {
        return rename(argv, store, now_ms);
    }
    if cmd.eq_ignore_ascii_case("RENAMENX") {
        return renamenx(argv, store, now_ms);
    }
    if cmd.eq_ignore_ascii_case("KEYS") {
        return keys(argv, store, now_ms);
    }
    if cmd.eq_ignore_ascii_case("DBSIZE") {
        return dbsize(argv, store, now_ms);
    }
    if cmd.eq_ignore_ascii_case("FLUSHDB") || cmd.eq_ignore_ascii_case("FLUSHALL") {
        return flushdb(argv, store);
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
    if argv.len() < 3 {
        return Err(CommandError::WrongArity("SET"));
    }
    let mut px_ttl_ms = None;
    let mut nx = false;
    let mut xx = false;
    let mut get = false;

    let mut i = 3;
    while i < argv.len() {
        let option =
            std::str::from_utf8(&argv[i]).map_err(|_| CommandError::InvalidUtf8Argument)?;
        if option.eq_ignore_ascii_case("PX") {
            i += 1;
            if i >= argv.len() {
                return Err(CommandError::SyntaxError);
            }
            let ttl = parse_u64_arg(&argv[i])?;
            px_ttl_ms = Some(ttl);
        } else if option.eq_ignore_ascii_case("EX") {
            i += 1;
            if i >= argv.len() {
                return Err(CommandError::SyntaxError);
            }
            let seconds = parse_u64_arg(&argv[i])?;
            px_ttl_ms = Some(seconds.saturating_mul(1000));
        } else if option.eq_ignore_ascii_case("NX") {
            nx = true;
        } else if option.eq_ignore_ascii_case("XX") {
            xx = true;
        } else if option.eq_ignore_ascii_case("GET") {
            get = true;
        } else {
            return Err(CommandError::SyntaxError);
        }
        i += 1;
    }

    if nx && xx {
        return Err(CommandError::SyntaxError);
    }

    let old_value = if get {
        store.get(&argv[1], now_ms)
    } else {
        None
    };

    let key_exists = store.exists(&argv[1], now_ms);
    if nx && key_exists {
        return Ok(if get {
            RespFrame::BulkString(old_value)
        } else {
            RespFrame::BulkString(None)
        });
    }
    if xx && !key_exists {
        return Ok(RespFrame::BulkString(None));
    }

    store.set(argv[1].clone(), argv[2].clone(), px_ttl_ms, now_ms);

    if get {
        Ok(RespFrame::BulkString(old_value))
    } else {
        Ok(RespFrame::SimpleString("OK".to_string()))
    }
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

#[derive(Debug, Clone, Copy, Default)]
struct ExpireOptions {
    nx: bool,
    xx: bool,
    gt: bool,
    lt: bool,
}

#[derive(Debug, Clone, Copy)]
enum ExpireCommandKind {
    RelativeSeconds,
    RelativeMilliseconds,
    AbsoluteSeconds,
    AbsoluteMilliseconds,
}

fn expire(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    expire_like(
        argv,
        store,
        now_ms,
        ExpireCommandKind::RelativeSeconds,
        "EXPIRE",
    )
}

fn pexpire(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    expire_like(
        argv,
        store,
        now_ms,
        ExpireCommandKind::RelativeMilliseconds,
        "PEXPIRE",
    )
}

fn expireat(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    expire_like(
        argv,
        store,
        now_ms,
        ExpireCommandKind::AbsoluteSeconds,
        "EXPIREAT",
    )
}

fn pexpireat(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    expire_like(
        argv,
        store,
        now_ms,
        ExpireCommandKind::AbsoluteMilliseconds,
        "PEXPIREAT",
    )
}

fn expire_like(
    argv: &[Vec<u8>],
    store: &mut Store,
    now_ms: u64,
    kind: ExpireCommandKind,
    command_name: &'static str,
) -> Result<RespFrame, CommandError> {
    if argv.len() < 3 {
        return Err(CommandError::WrongArity(command_name));
    }
    let raw_time = parse_i64_arg(&argv[2])?;
    let options = parse_expire_options(&argv[3..])?;
    let when_ms = deadline_from_expire_kind(kind, raw_time, now_ms);
    let applied = apply_expiry_with_options(store, &argv[1], when_ms, now_ms, options);
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

fn append(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 3 {
        return Err(CommandError::WrongArity("APPEND"));
    }
    let new_len = store.append(&argv[1], &argv[2], now_ms);
    let new_len = i64::try_from(new_len).unwrap_or(i64::MAX);
    Ok(RespFrame::Integer(new_len))
}

fn strlen(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 2 {
        return Err(CommandError::WrongArity("STRLEN"));
    }
    let len = store.strlen(&argv[1], now_ms);
    let len = i64::try_from(len).unwrap_or(i64::MAX);
    Ok(RespFrame::Integer(len))
}

fn mget(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() < 2 {
        return Err(CommandError::WrongArity("MGET"));
    }
    let keys: Vec<&[u8]> = argv[1..].iter().map(Vec::as_slice).collect();
    let values = store.mget(&keys, now_ms);
    let frames = values.into_iter().map(RespFrame::BulkString).collect();
    Ok(RespFrame::Array(Some(frames)))
}

fn mset(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() < 3 || !(argv.len() - 1).is_multiple_of(2) {
        return Err(CommandError::WrongArity("MSET"));
    }
    let mut i = 1;
    while i < argv.len() {
        store.set(argv[i].clone(), argv[i + 1].clone(), None, now_ms);
        i += 2;
    }
    Ok(RespFrame::SimpleString("OK".to_string()))
}

fn setnx(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 3 {
        return Err(CommandError::WrongArity("SETNX"));
    }
    let result = store.setnx(argv[1].clone(), argv[2].clone(), now_ms);
    Ok(RespFrame::Integer(if result { 1 } else { 0 }))
}

fn getset(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 3 {
        return Err(CommandError::WrongArity("GETSET"));
    }
    let old = store.getset(argv[1].clone(), argv[2].clone(), now_ms);
    Ok(RespFrame::BulkString(old))
}

fn incrby(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 3 {
        return Err(CommandError::WrongArity("INCRBY"));
    }
    let delta = parse_i64_arg(&argv[2])?;
    let value = store.incrby(&argv[1], delta, now_ms)?;
    Ok(RespFrame::Integer(value))
}

fn decrby(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 3 {
        return Err(CommandError::WrongArity("DECRBY"));
    }
    let delta = parse_i64_arg(&argv[2])?;
    let neg_delta = delta
        .checked_neg()
        .ok_or(CommandError::Store(StoreError::IntegerOverflow))?;
    let value = store.incrby(&argv[1], neg_delta, now_ms)?;
    Ok(RespFrame::Integer(value))
}

fn decr(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 2 {
        return Err(CommandError::WrongArity("DECR"));
    }
    let value = store.incrby(&argv[1], -1, now_ms)?;
    Ok(RespFrame::Integer(value))
}

fn exists(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() < 2 {
        return Err(CommandError::WrongArity("EXISTS"));
    }
    let mut count = 0_i64;
    for key in &argv[1..] {
        if store.exists(key, now_ms) {
            count = count.saturating_add(1);
        }
    }
    Ok(RespFrame::Integer(count))
}

fn ttl(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 2 {
        return Err(CommandError::WrongArity("TTL"));
    }
    let value = match store.pttl(&argv[1], now_ms) {
        PttlValue::KeyMissing => -2,
        PttlValue::NoExpiry => -1,
        PttlValue::Remaining(ms) => ms / 1000,
    };
    Ok(RespFrame::Integer(value))
}

fn expiretime(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 2 {
        return Err(CommandError::WrongArity("EXPIRETIME"));
    }
    let value = match store.pttl(&argv[1], now_ms) {
        PttlValue::KeyMissing => -2,
        PttlValue::NoExpiry => -1,
        PttlValue::Remaining(ms) => {
            let absolute_ms = i128::from(now_ms).saturating_add(i128::from(ms));
            let absolute_ms = clamp_i128_to_i64(absolute_ms);
            absolute_ms.saturating_add(500) / 1000
        }
    };
    Ok(RespFrame::Integer(value))
}

fn pexpiretime(
    argv: &[Vec<u8>],
    store: &mut Store,
    now_ms: u64,
) -> Result<RespFrame, CommandError> {
    if argv.len() != 2 {
        return Err(CommandError::WrongArity("PEXPIRETIME"));
    }
    let value = match store.pttl(&argv[1], now_ms) {
        PttlValue::KeyMissing => -2,
        PttlValue::NoExpiry => -1,
        PttlValue::Remaining(ms) => {
            let absolute_ms = i128::from(now_ms).saturating_add(i128::from(ms));
            clamp_i128_to_i64(absolute_ms)
        }
    };
    Ok(RespFrame::Integer(value))
}

fn persist(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 2 {
        return Err(CommandError::WrongArity("PERSIST"));
    }
    let removed = store.persist(&argv[1], now_ms);
    Ok(RespFrame::Integer(if removed { 1 } else { 0 }))
}

fn type_cmd(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 2 {
        return Err(CommandError::WrongArity("TYPE"));
    }
    let type_str = store.key_type(&argv[1], now_ms).unwrap_or("none");
    Ok(RespFrame::SimpleString(type_str.to_string()))
}

fn rename(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 3 {
        return Err(CommandError::WrongArity("RENAME"));
    }
    store
        .rename(&argv[1], &argv[2], now_ms)
        .map_err(|e| match e {
            StoreError::KeyNotFound => CommandError::NoSuchKey,
            other => CommandError::Store(other),
        })?;
    Ok(RespFrame::SimpleString("OK".to_string()))
}

fn renamenx(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 3 {
        return Err(CommandError::WrongArity("RENAMENX"));
    }
    let result = store
        .renamenx(&argv[1], &argv[2], now_ms)
        .map_err(|e| match e {
            StoreError::KeyNotFound => CommandError::NoSuchKey,
            other => CommandError::Store(other),
        })?;
    Ok(RespFrame::Integer(if result { 1 } else { 0 }))
}

fn keys(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 2 {
        return Err(CommandError::WrongArity("KEYS"));
    }
    let matched = store.keys_matching(&argv[1], now_ms);
    let frames = matched
        .into_iter()
        .map(|k| RespFrame::BulkString(Some(k)))
        .collect();
    Ok(RespFrame::Array(Some(frames)))
}

fn dbsize(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 1 {
        return Err(CommandError::WrongArity("DBSIZE"));
    }
    let size = store.dbsize(now_ms);
    let size = i64::try_from(size).unwrap_or(i64::MAX);
    Ok(RespFrame::Integer(size))
}

fn flushdb(argv: &[Vec<u8>], store: &mut Store) -> Result<RespFrame, CommandError> {
    if argv.len() > 2 {
        return Err(CommandError::WrongArity("FLUSHDB"));
    }
    store.flushdb();
    Ok(RespFrame::SimpleString("OK".to_string()))
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

fn parse_expire_options(extra_args: &[Vec<u8>]) -> Result<ExpireOptions, CommandError> {
    let mut options = ExpireOptions::default();
    for arg in extra_args {
        let option = std::str::from_utf8(arg).map_err(|_| CommandError::InvalidUtf8Argument)?;
        if option.eq_ignore_ascii_case("NX") {
            options.nx = true;
        } else if option.eq_ignore_ascii_case("XX") {
            options.xx = true;
        } else if option.eq_ignore_ascii_case("GT") {
            options.gt = true;
        } else if option.eq_ignore_ascii_case("LT") {
            options.lt = true;
        } else {
            return Err(CommandError::SyntaxError);
        }
    }

    if (options.nx && (options.xx || options.gt || options.lt)) || (options.gt && options.lt) {
        return Err(CommandError::SyntaxError);
    }

    Ok(options)
}

fn deadline_from_expire_kind(kind: ExpireCommandKind, raw_time: i64, now_ms: u64) -> i128 {
    match kind {
        ExpireCommandKind::RelativeSeconds => {
            i128::from(now_ms).saturating_add(i128::from(raw_time).saturating_mul(1000))
        }
        ExpireCommandKind::RelativeMilliseconds => {
            i128::from(now_ms).saturating_add(i128::from(raw_time))
        }
        ExpireCommandKind::AbsoluteSeconds => i128::from(raw_time).saturating_mul(1000),
        ExpireCommandKind::AbsoluteMilliseconds => i128::from(raw_time),
    }
}

fn apply_expiry_with_options(
    store: &mut Store,
    key: &[u8],
    when_ms: i128,
    now_ms: u64,
    options: ExpireOptions,
) -> bool {
    let current_remaining_ms = match store.pttl(key, now_ms) {
        PttlValue::KeyMissing => return false,
        PttlValue::NoExpiry => None,
        PttlValue::Remaining(ms) => Some(ms),
    };

    if options.nx && current_remaining_ms.is_some() {
        return false;
    }
    if options.xx && current_remaining_ms.is_none() {
        return false;
    }
    if options.gt {
        let Some(remaining_ms) = current_remaining_ms else {
            return false;
        };
        let current_when_ms = i128::from(now_ms).saturating_add(i128::from(remaining_ms));
        if when_ms <= current_when_ms {
            return false;
        }
    }
    if options.lt
        && let Some(remaining_ms) = current_remaining_ms
    {
        let current_when_ms = i128::from(now_ms).saturating_add(i128::from(remaining_ms));
        if when_ms >= current_when_ms {
            return false;
        }
    }

    store.expire_at_milliseconds(key, clamp_i128_to_i64(when_ms), now_ms)
}

fn clamp_i128_to_i64(value: i128) -> i64 {
    if value < i128::from(i64::MIN) {
        i64::MIN
    } else if value > i128::from(i64::MAX) {
        i64::MAX
    } else {
        value as i64
    }
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

    #[test]
    fn set_with_ex_option() {
        let mut store = Store::new();
        let argv = vec![
            b"SET".to_vec(),
            b"k".to_vec(),
            b"v".to_vec(),
            b"EX".to_vec(),
            b"10".to_vec(),
        ];
        let out = dispatch_argv(&argv, &mut store, 1000).expect("set with EX");
        assert_eq!(out, RespFrame::SimpleString("OK".to_string()));
        // TTL should be ~10 seconds
        let ttl_argv = vec![b"TTL".to_vec(), b"k".to_vec()];
        let ttl_out = dispatch_argv(&ttl_argv, &mut store, 1000).expect("ttl");
        assert_eq!(ttl_out, RespFrame::Integer(10));
    }

    #[test]
    fn set_with_nx_only_sets_if_absent() {
        let mut store = Store::new();
        let argv = vec![
            b"SET".to_vec(),
            b"k".to_vec(),
            b"v1".to_vec(),
            b"NX".to_vec(),
        ];
        let out = dispatch_argv(&argv, &mut store, 0).expect("set NX");
        assert_eq!(out, RespFrame::SimpleString("OK".to_string()));
        let argv2 = vec![
            b"SET".to_vec(),
            b"k".to_vec(),
            b"v2".to_vec(),
            b"NX".to_vec(),
        ];
        let out2 = dispatch_argv(&argv2, &mut store, 0).expect("set NX again");
        assert_eq!(out2, RespFrame::BulkString(None));
        // Value should still be v1
        let get = vec![b"GET".to_vec(), b"k".to_vec()];
        let val = dispatch_argv(&get, &mut store, 0).expect("get");
        assert_eq!(val, RespFrame::BulkString(Some(b"v1".to_vec())));
    }

    #[test]
    fn set_with_xx_only_sets_if_exists() {
        let mut store = Store::new();
        let argv = vec![
            b"SET".to_vec(),
            b"k".to_vec(),
            b"v1".to_vec(),
            b"XX".to_vec(),
        ];
        let out = dispatch_argv(&argv, &mut store, 0).expect("set XX on missing");
        assert_eq!(out, RespFrame::BulkString(None));
        // Set it first, then XX should work
        store.set(b"k".to_vec(), b"old".to_vec(), None, 0);
        let out2 = dispatch_argv(&argv, &mut store, 0).expect("set XX on existing");
        assert_eq!(out2, RespFrame::SimpleString("OK".to_string()));
    }

    #[test]
    fn set_with_get_returns_old_value() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), b"old".to_vec(), None, 0);
        let argv = vec![
            b"SET".to_vec(),
            b"k".to_vec(),
            b"new".to_vec(),
            b"GET".to_vec(),
        ];
        let out = dispatch_argv(&argv, &mut store, 0).expect("set GET");
        assert_eq!(out, RespFrame::BulkString(Some(b"old".to_vec())));
        let get = vec![b"GET".to_vec(), b"k".to_vec()];
        let val = dispatch_argv(&get, &mut store, 0).expect("get");
        assert_eq!(val, RespFrame::BulkString(Some(b"new".to_vec())));
    }

    #[test]
    fn append_command() {
        let mut store = Store::new();
        let argv = vec![b"APPEND".to_vec(), b"k".to_vec(), b"hello".to_vec()];
        let out = dispatch_argv(&argv, &mut store, 0).expect("append");
        assert_eq!(out, RespFrame::Integer(5));
        let argv2 = vec![b"APPEND".to_vec(), b"k".to_vec(), b" world".to_vec()];
        let out2 = dispatch_argv(&argv2, &mut store, 0).expect("append2");
        assert_eq!(out2, RespFrame::Integer(11));
    }

    #[test]
    fn strlen_command() {
        let mut store = Store::new();
        let argv = vec![b"STRLEN".to_vec(), b"k".to_vec()];
        let out = dispatch_argv(&argv, &mut store, 0).expect("strlen missing");
        assert_eq!(out, RespFrame::Integer(0));
        store.set(b"k".to_vec(), b"hello".to_vec(), None, 0);
        let out2 = dispatch_argv(&argv, &mut store, 0).expect("strlen existing");
        assert_eq!(out2, RespFrame::Integer(5));
    }

    #[test]
    fn mget_command() {
        let mut store = Store::new();
        store.set(b"a".to_vec(), b"1".to_vec(), None, 0);
        store.set(b"c".to_vec(), b"3".to_vec(), None, 0);
        let argv = vec![
            b"MGET".to_vec(),
            b"a".to_vec(),
            b"b".to_vec(),
            b"c".to_vec(),
        ];
        let out = dispatch_argv(&argv, &mut store, 0).expect("mget");
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"1".to_vec())),
                RespFrame::BulkString(None),
                RespFrame::BulkString(Some(b"3".to_vec())),
            ]))
        );
    }

    #[test]
    fn mset_command() {
        let mut store = Store::new();
        let argv = vec![
            b"MSET".to_vec(),
            b"a".to_vec(),
            b"1".to_vec(),
            b"b".to_vec(),
            b"2".to_vec(),
        ];
        let out = dispatch_argv(&argv, &mut store, 0).expect("mset");
        assert_eq!(out, RespFrame::SimpleString("OK".to_string()));
        assert_eq!(store.get(b"a", 0), Some(b"1".to_vec()));
        assert_eq!(store.get(b"b", 0), Some(b"2".to_vec()));
    }

    #[test]
    fn setnx_command() {
        let mut store = Store::new();
        let argv = vec![b"SETNX".to_vec(), b"k".to_vec(), b"v".to_vec()];
        let out = dispatch_argv(&argv, &mut store, 0).expect("setnx");
        assert_eq!(out, RespFrame::Integer(1));
        let out2 = dispatch_argv(&argv, &mut store, 0).expect("setnx again");
        assert_eq!(out2, RespFrame::Integer(0));
    }

    #[test]
    fn getset_command() {
        let mut store = Store::new();
        let argv = vec![b"GETSET".to_vec(), b"k".to_vec(), b"v1".to_vec()];
        let out = dispatch_argv(&argv, &mut store, 0).expect("getset");
        assert_eq!(out, RespFrame::BulkString(None));
        let argv2 = vec![b"GETSET".to_vec(), b"k".to_vec(), b"v2".to_vec()];
        let out2 = dispatch_argv(&argv2, &mut store, 0).expect("getset2");
        assert_eq!(out2, RespFrame::BulkString(Some(b"v1".to_vec())));
    }

    #[test]
    fn incrby_and_decrby_commands() {
        let mut store = Store::new();
        let argv = vec![b"INCRBY".to_vec(), b"n".to_vec(), b"5".to_vec()];
        let out = dispatch_argv(&argv, &mut store, 0).expect("incrby");
        assert_eq!(out, RespFrame::Integer(5));
        let argv2 = vec![b"DECRBY".to_vec(), b"n".to_vec(), b"3".to_vec()];
        let out2 = dispatch_argv(&argv2, &mut store, 0).expect("decrby");
        assert_eq!(out2, RespFrame::Integer(2));
    }

    #[test]
    fn decr_command() {
        let mut store = Store::new();
        store.set(b"n".to_vec(), b"10".to_vec(), None, 0);
        let argv = vec![b"DECR".to_vec(), b"n".to_vec()];
        let out = dispatch_argv(&argv, &mut store, 0).expect("decr");
        assert_eq!(out, RespFrame::Integer(9));
    }

    #[test]
    fn exists_command_multi_key() {
        let mut store = Store::new();
        store.set(b"a".to_vec(), b"1".to_vec(), None, 0);
        store.set(b"b".to_vec(), b"2".to_vec(), None, 0);
        let argv = vec![
            b"EXISTS".to_vec(),
            b"a".to_vec(),
            b"b".to_vec(),
            b"c".to_vec(),
        ];
        let out = dispatch_argv(&argv, &mut store, 0).expect("exists");
        assert_eq!(out, RespFrame::Integer(2));
    }

    #[test]
    fn ttl_command() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), b"v".to_vec(), Some(5500), 1000);
        let argv = vec![b"TTL".to_vec(), b"k".to_vec()];
        let out = dispatch_argv(&argv, &mut store, 1000).expect("ttl");
        assert_eq!(out, RespFrame::Integer(5));
        let argv_missing = vec![b"TTL".to_vec(), b"missing".to_vec()];
        let out2 = dispatch_argv(&argv_missing, &mut store, 1000).expect("ttl missing");
        assert_eq!(out2, RespFrame::Integer(-2));
    }

    #[test]
    fn persist_command() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), b"v".to_vec(), Some(5000), 0);
        let argv = vec![b"PERSIST".to_vec(), b"k".to_vec()];
        let out = dispatch_argv(&argv, &mut store, 0).expect("persist");
        assert_eq!(out, RespFrame::Integer(1));
        let ttl_argv = vec![b"TTL".to_vec(), b"k".to_vec()];
        let ttl = dispatch_argv(&ttl_argv, &mut store, 0).expect("ttl after persist");
        assert_eq!(ttl, RespFrame::Integer(-1));
    }

    #[test]
    fn type_command() {
        let mut store = Store::new();
        let argv = vec![b"TYPE".to_vec(), b"missing".to_vec()];
        let out = dispatch_argv(&argv, &mut store, 0).expect("type missing");
        assert_eq!(out, RespFrame::SimpleString("none".to_string()));
        store.set(b"k".to_vec(), b"v".to_vec(), None, 0);
        let argv2 = vec![b"TYPE".to_vec(), b"k".to_vec()];
        let out2 = dispatch_argv(&argv2, &mut store, 0).expect("type string");
        assert_eq!(out2, RespFrame::SimpleString("string".to_string()));
    }

    #[test]
    fn rename_command() {
        let mut store = Store::new();
        store.set(b"old".to_vec(), b"v".to_vec(), None, 0);
        let argv = vec![b"RENAME".to_vec(), b"old".to_vec(), b"new".to_vec()];
        let out = dispatch_argv(&argv, &mut store, 0).expect("rename");
        assert_eq!(out, RespFrame::SimpleString("OK".to_string()));
        assert_eq!(store.get(b"new", 0), Some(b"v".to_vec()));
    }

    #[test]
    fn rename_missing_key_errors() {
        let mut store = Store::new();
        let argv = vec![b"RENAME".to_vec(), b"missing".to_vec(), b"new".to_vec()];
        let err = dispatch_argv(&argv, &mut store, 0).expect_err("rename missing");
        assert!(matches!(err, super::CommandError::NoSuchKey));
    }

    #[test]
    fn renamenx_command() {
        let mut store = Store::new();
        store.set(b"a".to_vec(), b"1".to_vec(), None, 0);
        store.set(b"b".to_vec(), b"2".to_vec(), None, 0);
        let argv = vec![b"RENAMENX".to_vec(), b"a".to_vec(), b"b".to_vec()];
        let out = dispatch_argv(&argv, &mut store, 0).expect("renamenx existing");
        assert_eq!(out, RespFrame::Integer(0));
        let argv2 = vec![b"RENAMENX".to_vec(), b"a".to_vec(), b"c".to_vec()];
        let out2 = dispatch_argv(&argv2, &mut store, 0).expect("renamenx new");
        assert_eq!(out2, RespFrame::Integer(1));
    }

    #[test]
    fn keys_command() {
        let mut store = Store::new();
        store.set(b"hello".to_vec(), b"1".to_vec(), None, 0);
        store.set(b"hallo".to_vec(), b"2".to_vec(), None, 0);
        store.set(b"world".to_vec(), b"3".to_vec(), None, 0);
        let argv = vec![b"KEYS".to_vec(), b"h*".to_vec()];
        let out = dispatch_argv(&argv, &mut store, 0).expect("keys");
        if let RespFrame::Array(Some(items)) = out {
            assert_eq!(items.len(), 2);
        } else {
            panic!("expected array");
        }
    }

    #[test]
    fn dbsize_command() {
        let mut store = Store::new();
        store.set(b"a".to_vec(), b"1".to_vec(), None, 0);
        store.set(b"b".to_vec(), b"2".to_vec(), None, 0);
        let argv = vec![b"DBSIZE".to_vec()];
        let out = dispatch_argv(&argv, &mut store, 0).expect("dbsize");
        assert_eq!(out, RespFrame::Integer(2));
    }

    #[test]
    fn flushdb_command() {
        let mut store = Store::new();
        store.set(b"a".to_vec(), b"1".to_vec(), None, 0);
        store.set(b"b".to_vec(), b"2".to_vec(), None, 0);
        let argv = vec![b"FLUSHDB".to_vec()];
        let out = dispatch_argv(&argv, &mut store, 0).expect("flushdb");
        assert_eq!(out, RespFrame::SimpleString("OK".to_string()));
        assert!(store.is_empty());
    }

    #[test]
    fn case_insensitive_commands() {
        let mut store = Store::new();
        let argv = vec![b"set".to_vec(), b"k".to_vec(), b"v".to_vec()];
        dispatch_argv(&argv, &mut store, 0).expect("lowercase set");
        let argv2 = vec![b"Get".to_vec(), b"k".to_vec()];
        let out = dispatch_argv(&argv2, &mut store, 0).expect("mixed case get");
        assert_eq!(out, RespFrame::BulkString(Some(b"v".to_vec())));
    }

    #[test]
    fn expire_nx_and_xx_options_follow_contract() {
        let mut store = Store::new();
        dispatch_argv(
            &[b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()],
            &mut store,
            0,
        )
        .expect("set");

        let out = dispatch_argv(
            &[
                b"EXPIRE".to_vec(),
                b"k".to_vec(),
                b"10".to_vec(),
                b"NX".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("expire nx first");
        assert_eq!(out, RespFrame::Integer(1));

        let out = dispatch_argv(
            &[
                b"EXPIRE".to_vec(),
                b"k".to_vec(),
                b"20".to_vec(),
                b"NX".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("expire nx second");
        assert_eq!(out, RespFrame::Integer(0));

        let out = dispatch_argv(
            &[
                b"EXPIRE".to_vec(),
                b"k".to_vec(),
                b"20".to_vec(),
                b"XX".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("expire xx");
        assert_eq!(out, RespFrame::Integer(1));
    }

    #[test]
    fn expire_gt_and_lt_options_follow_contract() {
        let mut store = Store::new();
        dispatch_argv(
            &[b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()],
            &mut store,
            0,
        )
        .expect("set");
        dispatch_argv(
            &[b"EXPIRE".to_vec(), b"k".to_vec(), b"10".to_vec()],
            &mut store,
            0,
        )
        .expect("expire baseline");

        let out = dispatch_argv(
            &[
                b"EXPIRE".to_vec(),
                b"k".to_vec(),
                b"9".to_vec(),
                b"GT".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("gt rejects smaller");
        assert_eq!(out, RespFrame::Integer(0));

        let out = dispatch_argv(
            &[
                b"EXPIRE".to_vec(),
                b"k".to_vec(),
                b"20".to_vec(),
                b"GT".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("gt accepts larger");
        assert_eq!(out, RespFrame::Integer(1));

        let out = dispatch_argv(
            &[
                b"EXPIRE".to_vec(),
                b"k".to_vec(),
                b"30".to_vec(),
                b"LT".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("lt rejects larger");
        assert_eq!(out, RespFrame::Integer(0));

        let out = dispatch_argv(
            &[
                b"EXPIRE".to_vec(),
                b"k".to_vec(),
                b"5".to_vec(),
                b"LT".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("lt accepts smaller");
        assert_eq!(out, RespFrame::Integer(1));
    }

    #[test]
    fn expire_options_on_persistent_key_match_redis_behavior() {
        let mut store = Store::new();
        dispatch_argv(
            &[b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()],
            &mut store,
            0,
        )
        .expect("set");

        let gt = dispatch_argv(
            &[
                b"EXPIRE".to_vec(),
                b"k".to_vec(),
                b"5".to_vec(),
                b"GT".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("gt on persistent key");
        assert_eq!(gt, RespFrame::Integer(0));

        let lt = dispatch_argv(
            &[
                b"EXPIRE".to_vec(),
                b"k".to_vec(),
                b"5".to_vec(),
                b"LT".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("lt on persistent key");
        assert_eq!(lt, RespFrame::Integer(1));
    }

    #[test]
    fn expire_incompatible_or_unknown_options_return_syntax_error() {
        let mut store = Store::new();
        dispatch_argv(
            &[b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()],
            &mut store,
            0,
        )
        .expect("set");

        let nx_xx = dispatch_argv(
            &[
                b"EXPIRE".to_vec(),
                b"k".to_vec(),
                b"5".to_vec(),
                b"NX".to_vec(),
                b"XX".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect_err("nx+xx should fail");
        assert!(matches!(nx_xx, super::CommandError::SyntaxError));

        let gt_lt = dispatch_argv(
            &[
                b"EXPIRE".to_vec(),
                b"k".to_vec(),
                b"5".to_vec(),
                b"GT".to_vec(),
                b"LT".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect_err("gt+lt should fail");
        assert!(matches!(gt_lt, super::CommandError::SyntaxError));

        let unknown = dispatch_argv(
            &[
                b"EXPIRE".to_vec(),
                b"k".to_vec(),
                b"5".to_vec(),
                b"ZZ".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect_err("unknown option should fail");
        assert!(matches!(unknown, super::CommandError::SyntaxError));
    }

    #[test]
    fn pexpire_sets_millisecond_ttl() {
        let mut store = Store::new();
        dispatch_argv(
            &[b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()],
            &mut store,
            1_000,
        )
        .expect("set");

        let out = dispatch_argv(
            &[b"PEXPIRE".to_vec(), b"k".to_vec(), b"1500".to_vec()],
            &mut store,
            1_000,
        )
        .expect("pexpire");
        assert_eq!(out, RespFrame::Integer(1));

        let pttl = dispatch_argv(&[b"PTTL".to_vec(), b"k".to_vec()], &mut store, 1_000)
            .expect("pttl after pexpire");
        assert_eq!(pttl, RespFrame::Integer(1_500));
    }

    #[test]
    fn expireat_and_pexpireat_use_absolute_deadlines() {
        let mut store = Store::new();
        dispatch_argv(
            &[b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()],
            &mut store,
            1_000,
        )
        .expect("set");

        let expireat = dispatch_argv(
            &[b"EXPIREAT".to_vec(), b"k".to_vec(), b"3".to_vec()],
            &mut store,
            1_000,
        )
        .expect("expireat");
        assert_eq!(expireat, RespFrame::Integer(1));

        let pttl = dispatch_argv(&[b"PTTL".to_vec(), b"k".to_vec()], &mut store, 1_000)
            .expect("pttl after expireat");
        assert_eq!(pttl, RespFrame::Integer(2_000));

        let pexpireat = dispatch_argv(
            &[b"PEXPIREAT".to_vec(), b"k".to_vec(), b"4500".to_vec()],
            &mut store,
            1_000,
        )
        .expect("pexpireat");
        assert_eq!(pexpireat, RespFrame::Integer(1));

        let pttl = dispatch_argv(&[b"PTTL".to_vec(), b"k".to_vec()], &mut store, 1_000)
            .expect("pttl after pexpireat");
        assert_eq!(pttl, RespFrame::Integer(3_500));

        let delete_now = dispatch_argv(
            &[b"PEXPIREAT".to_vec(), b"k".to_vec(), b"900".to_vec()],
            &mut store,
            1_000,
        )
        .expect("pexpireat in past");
        assert_eq!(delete_now, RespFrame::Integer(1));

        let missing = dispatch_argv(&[b"GET".to_vec(), b"k".to_vec()], &mut store, 1_000)
            .expect("get missing");
        assert_eq!(missing, RespFrame::BulkString(None));
    }

    #[test]
    fn expiretime_and_pexpiretime_report_absolute_deadlines() {
        let mut store = Store::new();
        let missing = dispatch_argv(
            &[b"EXPIRETIME".to_vec(), b"missing".to_vec()],
            &mut store,
            1_000,
        )
        .expect("expiretime missing");
        assert_eq!(missing, RespFrame::Integer(-2));

        dispatch_argv(
            &[b"SET".to_vec(), b"persistent".to_vec(), b"v".to_vec()],
            &mut store,
            1_000,
        )
        .expect("set persistent");
        let no_expiry = dispatch_argv(
            &[b"PEXPIRETIME".to_vec(), b"persistent".to_vec()],
            &mut store,
            1_000,
        )
        .expect("pexpiretime persistent");
        assert_eq!(no_expiry, RespFrame::Integer(-1));

        dispatch_argv(
            &[b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()],
            &mut store,
            1_000,
        )
        .expect("set key");
        dispatch_argv(
            &[b"PEXPIRE".to_vec(), b"k".to_vec(), b"2500".to_vec()],
            &mut store,
            1_000,
        )
        .expect("pexpire key");

        let expiretime = dispatch_argv(&[b"EXPIRETIME".to_vec(), b"k".to_vec()], &mut store, 1_000)
            .expect("expiretime");
        assert_eq!(expiretime, RespFrame::Integer(4));

        let pexpiretime =
            dispatch_argv(&[b"PEXPIRETIME".to_vec(), b"k".to_vec()], &mut store, 1_000)
                .expect("pexpiretime");
        assert_eq!(pexpiretime, RespFrame::Integer(3_500));
    }

    #[test]
    fn pexpire_supports_nx_xx_gt_lt_options() {
        let mut store = Store::new();
        dispatch_argv(
            &[b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()],
            &mut store,
            0,
        )
        .expect("set");

        let out = dispatch_argv(
            &[
                b"PEXPIRE".to_vec(),
                b"k".to_vec(),
                b"1000".to_vec(),
                b"NX".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("pexpire nx");
        assert_eq!(out, RespFrame::Integer(1));

        let out = dispatch_argv(
            &[
                b"PEXPIRE".to_vec(),
                b"k".to_vec(),
                b"2000".to_vec(),
                b"NX".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("pexpire nx reject");
        assert_eq!(out, RespFrame::Integer(0));

        let out = dispatch_argv(
            &[
                b"PEXPIRE".to_vec(),
                b"k".to_vec(),
                b"2000".to_vec(),
                b"XX".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("pexpire xx");
        assert_eq!(out, RespFrame::Integer(1));

        let out = dispatch_argv(
            &[
                b"PEXPIRE".to_vec(),
                b"k".to_vec(),
                b"1500".to_vec(),
                b"GT".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("pexpire gt reject");
        assert_eq!(out, RespFrame::Integer(0));

        let out = dispatch_argv(
            &[
                b"PEXPIRE".to_vec(),
                b"k".to_vec(),
                b"2500".to_vec(),
                b"GT".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("pexpire gt");
        assert_eq!(out, RespFrame::Integer(1));

        let out = dispatch_argv(
            &[
                b"PEXPIRE".to_vec(),
                b"k".to_vec(),
                b"3000".to_vec(),
                b"LT".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("pexpire lt reject");
        assert_eq!(out, RespFrame::Integer(0));

        let out = dispatch_argv(
            &[
                b"PEXPIRE".to_vec(),
                b"k".to_vec(),
                b"500".to_vec(),
                b"LT".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("pexpire lt");
        assert_eq!(out, RespFrame::Integer(1));
    }
}
