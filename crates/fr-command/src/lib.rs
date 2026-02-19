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
    let Some(raw_cmd) = argv.first() else {
        return Err(CommandError::InvalidCommandFrame);
    };
    match classify_command(raw_cmd) {
        Some(CommandId::Ping) => return ping(argv),
        Some(CommandId::Echo) => return echo(argv),
        Some(CommandId::Set) => return set(argv, store, now_ms),
        Some(CommandId::Get) => return get(argv, store, now_ms),
        Some(CommandId::Del) => return del(argv, store, now_ms),
        Some(CommandId::Incr) => return incr(argv, store, now_ms),
        Some(CommandId::Expire) => return expire(argv, store, now_ms),
        Some(CommandId::Pexpire) => return pexpire(argv, store, now_ms),
        Some(CommandId::Expireat) => return expireat(argv, store, now_ms),
        Some(CommandId::Pexpireat) => return pexpireat(argv, store, now_ms),
        Some(CommandId::Pttl) => return pttl(argv, store, now_ms),
        Some(CommandId::Append) => return append(argv, store, now_ms),
        Some(CommandId::Strlen) => return strlen(argv, store, now_ms),
        Some(CommandId::Mget) => return mget(argv, store, now_ms),
        Some(CommandId::Mset) => return mset(argv, store, now_ms),
        Some(CommandId::Setnx) => return setnx(argv, store, now_ms),
        Some(CommandId::Getset) => return getset(argv, store, now_ms),
        Some(CommandId::Incrby) => return incrby(argv, store, now_ms),
        Some(CommandId::Decrby) => return decrby(argv, store, now_ms),
        Some(CommandId::Decr) => return decr(argv, store, now_ms),
        Some(CommandId::Exists) => return exists(argv, store, now_ms),
        Some(CommandId::Ttl) => return ttl(argv, store, now_ms),
        Some(CommandId::Expiretime) => return expiretime(argv, store, now_ms),
        Some(CommandId::Pexpiretime) => return pexpiretime(argv, store, now_ms),
        Some(CommandId::Persist) => return persist(argv, store, now_ms),
        Some(CommandId::Type) => return type_cmd(argv, store, now_ms),
        Some(CommandId::Rename) => return rename(argv, store, now_ms),
        Some(CommandId::Renamenx) => return renamenx(argv, store, now_ms),
        Some(CommandId::Keys) => return keys(argv, store, now_ms),
        Some(CommandId::Dbsize) => return dbsize(argv, store, now_ms),
        Some(CommandId::Flushdb) => return flushdb(argv, store),
        Some(CommandId::Hset) => return hset(argv, store, now_ms),
        Some(CommandId::Hget) => return hget(argv, store, now_ms),
        Some(CommandId::Hdel) => return hdel(argv, store, now_ms),
        Some(CommandId::Hexists) => return hexists(argv, store, now_ms),
        Some(CommandId::Hlen) => return hlen(argv, store, now_ms),
        Some(CommandId::Hgetall) => return hgetall(argv, store, now_ms),
        Some(CommandId::Hkeys) => return hkeys(argv, store, now_ms),
        Some(CommandId::Hvals) => return hvals(argv, store, now_ms),
        Some(CommandId::Hmget) => return hmget(argv, store, now_ms),
        Some(CommandId::Hmset) => return hmset(argv, store, now_ms),
        Some(CommandId::Hincrby) => return hincrby(argv, store, now_ms),
        Some(CommandId::Hsetnx) => return hsetnx_cmd(argv, store, now_ms),
        Some(CommandId::Hstrlen) => return hstrlen(argv, store, now_ms),
        Some(CommandId::Lpush) => return lpush(argv, store, now_ms),
        Some(CommandId::Rpush) => return rpush(argv, store, now_ms),
        Some(CommandId::Lpop) => return lpop(argv, store, now_ms),
        Some(CommandId::Rpop) => return rpop(argv, store, now_ms),
        Some(CommandId::Llen) => return llen(argv, store, now_ms),
        Some(CommandId::Lrange) => return lrange(argv, store, now_ms),
        Some(CommandId::Lindex) => return lindex(argv, store, now_ms),
        Some(CommandId::Lset) => return lset_cmd(argv, store, now_ms),
        Some(CommandId::Sadd) => return sadd(argv, store, now_ms),
        Some(CommandId::Srem) => return srem(argv, store, now_ms),
        Some(CommandId::Smembers) => return smembers(argv, store, now_ms),
        Some(CommandId::Scard) => return scard(argv, store, now_ms),
        Some(CommandId::Sismember) => return sismember(argv, store, now_ms),
        Some(CommandId::Zadd) => return zadd(argv, store, now_ms),
        Some(CommandId::Zrem) => return zrem(argv, store, now_ms),
        Some(CommandId::Zscore) => return zscore(argv, store, now_ms),
        Some(CommandId::Zcard) => return zcard(argv, store, now_ms),
        Some(CommandId::Zrank) => return zrank(argv, store, now_ms),
        Some(CommandId::Zrevrank) => return zrevrank(argv, store, now_ms),
        Some(CommandId::Zrange) => return zrange(argv, store, now_ms),
        Some(CommandId::Zrevrange) => return zrevrange(argv, store, now_ms),
        Some(CommandId::Zrangebyscore) => return zrangebyscore(argv, store, now_ms),
        Some(CommandId::Zcount) => return zcount(argv, store, now_ms),
        Some(CommandId::Zincrby) => return zincrby(argv, store, now_ms),
        Some(CommandId::Zpopmin) => return zpopmin(argv, store, now_ms),
        Some(CommandId::Zpopmax) => return zpopmax(argv, store, now_ms),
        Some(CommandId::Setex) => return setex(argv, store, now_ms),
        Some(CommandId::Psetex) => return psetex(argv, store, now_ms),
        Some(CommandId::Getdel) => return getdel(argv, store, now_ms),
        Some(CommandId::Getrange) => return getrange(argv, store, now_ms),
        Some(CommandId::Setrange) => return setrange(argv, store, now_ms),
        Some(CommandId::Incrbyfloat) => return incrbyfloat(argv, store, now_ms),
        Some(CommandId::Sinter) => return sinter(argv, store, now_ms),
        Some(CommandId::Sunion) => return sunion(argv, store, now_ms),
        Some(CommandId::Sdiff) => return sdiff(argv, store, now_ms),
        Some(CommandId::Spop) => return spop(argv, store, now_ms),
        Some(CommandId::Srandmember) => return srandmember(argv, store, now_ms),
        Some(CommandId::Setbit) => return setbit(argv, store, now_ms),
        Some(CommandId::Getbit) => return getbit(argv, store, now_ms),
        Some(CommandId::Bitcount) => return bitcount(argv, store, now_ms),
        Some(CommandId::Bitpos) => return bitpos(argv, store, now_ms),
        Some(CommandId::Lpos) => return lpos(argv, store, now_ms),
        Some(CommandId::Linsert) => return linsert(argv, store, now_ms),
        Some(CommandId::Lrem) => return lrem(argv, store, now_ms),
        Some(CommandId::Rpoplpush) => return rpoplpush(argv, store, now_ms),
        Some(CommandId::Hincrbyfloat) => return hincrbyfloat(argv, store, now_ms),
        Some(CommandId::Hrandfield) => return hrandfield(argv, store, now_ms),
        Some(CommandId::Zrevrangebyscore) => return zrevrangebyscore(argv, store, now_ms),
        Some(CommandId::Zrangebylex) => return zrangebylex(argv, store, now_ms),
        Some(CommandId::Zrevrangebylex) => return zrevrangebylex(argv, store, now_ms),
        Some(CommandId::Zlexcount) => return zlexcount(argv, store, now_ms),
        Some(CommandId::Ltrim) => return ltrim(argv, store, now_ms),
        Some(CommandId::Lpushx) => return lpushx(argv, store, now_ms),
        Some(CommandId::Rpushx) => return rpushx(argv, store, now_ms),
        Some(CommandId::Lmove) => return lmove(argv, store, now_ms),
        Some(CommandId::Smove) => return smove(argv, store, now_ms),
        Some(CommandId::Sinterstore) => return sinterstore(argv, store, now_ms),
        Some(CommandId::Sunionstore) => return sunionstore(argv, store, now_ms),
        Some(CommandId::Sdiffstore) => return sdiffstore(argv, store, now_ms),
        Some(CommandId::Zremrangebyrank) => return zremrangebyrank(argv, store, now_ms),
        Some(CommandId::Zremrangebyscore) => return zremrangebyscore(argv, store, now_ms),
        Some(CommandId::Zremrangebylex) => return zremrangebylex(argv, store, now_ms),
        Some(CommandId::Zrandmember) => return zrandmember(argv, store, now_ms),
        Some(CommandId::Zmscore) => return zmscore(argv, store, now_ms),
        Some(CommandId::Pfadd) => return pfadd(argv, store, now_ms),
        Some(CommandId::Pfcount) => return pfcount(argv, store, now_ms),
        Some(CommandId::Pfmerge) => return pfmerge(argv, store, now_ms),
        Some(CommandId::Getex) => return getex(argv, store, now_ms),
        Some(CommandId::Smismember) => return smismember(argv, store, now_ms),
        Some(CommandId::Substr) => return getrange(argv, store, now_ms),
        Some(CommandId::Bitop) => return bitop(argv, store, now_ms),
        Some(CommandId::Zunionstore) => return zunionstore(argv, store, now_ms),
        Some(CommandId::Zinterstore) => return zinterstore(argv, store, now_ms),
        Some(CommandId::Quit) => return quit(argv),
        Some(CommandId::Select) => return select(argv),
        Some(CommandId::Info) => return info(argv, store, now_ms),
        Some(CommandId::Command) => return command_cmd(argv),
        Some(CommandId::Config) => return config_cmd(argv),
        Some(CommandId::Client) => return client_cmd(argv),
        Some(CommandId::Time) => return time_cmd(argv, now_ms),
        Some(CommandId::Randomkey) => return randomkey(argv, store, now_ms),
        Some(CommandId::Scan) => return scan(argv, store, now_ms),
        Some(CommandId::Hscan) => return hscan(argv, store, now_ms),
        Some(CommandId::Sscan) => return sscan(argv, store, now_ms),
        Some(CommandId::Zscan) => return zscan(argv, store, now_ms),
        Some(CommandId::Object) => return object_cmd(argv),
        Some(CommandId::Wait) => return wait_cmd(argv),
        Some(CommandId::Reset) => return reset_cmd(argv),
        Some(CommandId::Unlink) => return del(argv, store, now_ms),
        Some(CommandId::Touch) => return touch(argv, store, now_ms),
        Some(CommandId::Dump) => return dump_cmd(argv),
        Some(CommandId::Restore) => return restore_cmd(argv),
        Some(CommandId::Sort) => return sort_cmd(argv),
        Some(CommandId::Copy) => return copy_cmd(argv, store, now_ms),
        None => {}
    }

    let cmd = std::str::from_utf8(raw_cmd).map_err(|_| CommandError::InvalidUtf8Argument)?;
    let args_preview = build_unknown_args_preview(argv);
    Err(CommandError::UnknownCommand {
        command: trim_and_cap_string(cmd, 128),
        args_preview,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommandId {
    Ping,
    Echo,
    Set,
    Get,
    Del,
    Incr,
    Expire,
    Pexpire,
    Expireat,
    Pexpireat,
    Pttl,
    Append,
    Strlen,
    Mget,
    Mset,
    Setnx,
    Getset,
    Incrby,
    Decrby,
    Decr,
    Exists,
    Ttl,
    Expiretime,
    Pexpiretime,
    Persist,
    Type,
    Rename,
    Renamenx,
    Keys,
    Dbsize,
    Flushdb,
    Hset,
    Hget,
    Hdel,
    Hexists,
    Hlen,
    Hgetall,
    Hkeys,
    Hvals,
    Hmget,
    Hmset,
    Hincrby,
    Hsetnx,
    Hstrlen,
    Lpush,
    Rpush,
    Lpop,
    Rpop,
    Llen,
    Lrange,
    Lindex,
    Lset,
    Sadd,
    Srem,
    Smembers,
    Scard,
    Sismember,
    Zadd,
    Zrem,
    Zscore,
    Zcard,
    Zrank,
    Zrevrank,
    Zrange,
    Zrevrange,
    Zrangebyscore,
    Zcount,
    Zincrby,
    Zpopmin,
    Zpopmax,
    Setex,
    Psetex,
    Getdel,
    Getrange,
    Setrange,
    Incrbyfloat,
    Sinter,
    Sunion,
    Sdiff,
    Spop,
    Srandmember,
    Setbit,
    Getbit,
    Bitcount,
    Bitpos,
    Lpos,
    Linsert,
    Lrem,
    Rpoplpush,
    Hincrbyfloat,
    Hrandfield,
    Zrevrangebyscore,
    Zrangebylex,
    Zrevrangebylex,
    Zlexcount,
    Ltrim,
    Lpushx,
    Rpushx,
    Lmove,
    Smove,
    Sinterstore,
    Sunionstore,
    Sdiffstore,
    Zremrangebyrank,
    Zremrangebyscore,
    Zremrangebylex,
    Zrandmember,
    Zmscore,
    Pfadd,
    Pfcount,
    Pfmerge,
    Getex,
    Smismember,
    Substr,
    Bitop,
    Zunionstore,
    Zinterstore,
    Quit,
    Select,
    Info,
    Command,
    Config,
    Client,
    Time,
    Randomkey,
    Scan,
    Hscan,
    Sscan,
    Zscan,
    Object,
    Wait,
    Reset,
    Unlink,
    Touch,
    Dump,
    Restore,
    Sort,
    Copy,
}

#[inline]
fn classify_command(cmd: &[u8]) -> Option<CommandId> {
    match cmd.len() {
        3 => {
            if eq_ascii_command(cmd, b"GET") {
                Some(CommandId::Get)
            } else if eq_ascii_command(cmd, b"SET") {
                Some(CommandId::Set)
            } else if eq_ascii_command(cmd, b"DEL") {
                Some(CommandId::Del)
            } else if eq_ascii_command(cmd, b"TTL") {
                Some(CommandId::Ttl)
            } else {
                None
            }
        }
        4 => {
            if eq_ascii_command(cmd, b"PING") {
                Some(CommandId::Ping)
            } else if eq_ascii_command(cmd, b"ECHO") {
                Some(CommandId::Echo)
            } else if eq_ascii_command(cmd, b"INCR") {
                Some(CommandId::Incr)
            } else if eq_ascii_command(cmd, b"PTTL") {
                Some(CommandId::Pttl)
            } else if eq_ascii_command(cmd, b"MGET") {
                Some(CommandId::Mget)
            } else if eq_ascii_command(cmd, b"MSET") {
                Some(CommandId::Mset)
            } else if eq_ascii_command(cmd, b"DECR") {
                Some(CommandId::Decr)
            } else if eq_ascii_command(cmd, b"TYPE") {
                Some(CommandId::Type)
            } else if eq_ascii_command(cmd, b"KEYS") {
                Some(CommandId::Keys)
            } else if eq_ascii_command(cmd, b"HGET") {
                Some(CommandId::Hget)
            } else if eq_ascii_command(cmd, b"HSET") {
                Some(CommandId::Hset)
            } else if eq_ascii_command(cmd, b"HDEL") {
                Some(CommandId::Hdel)
            } else if eq_ascii_command(cmd, b"HLEN") {
                Some(CommandId::Hlen)
            } else if eq_ascii_command(cmd, b"LPOP") {
                Some(CommandId::Lpop)
            } else if eq_ascii_command(cmd, b"RPOP") {
                Some(CommandId::Rpop)
            } else if eq_ascii_command(cmd, b"LLEN") {
                Some(CommandId::Llen)
            } else if eq_ascii_command(cmd, b"LSET") {
                Some(CommandId::Lset)
            } else if eq_ascii_command(cmd, b"SADD") {
                Some(CommandId::Sadd)
            } else if eq_ascii_command(cmd, b"SREM") {
                Some(CommandId::Srem)
            } else if eq_ascii_command(cmd, b"ZADD") {
                Some(CommandId::Zadd)
            } else if eq_ascii_command(cmd, b"ZREM") {
                Some(CommandId::Zrem)
            } else if eq_ascii_command(cmd, b"SPOP") {
                Some(CommandId::Spop)
            } else if eq_ascii_command(cmd, b"LPOS") {
                Some(CommandId::Lpos)
            } else if eq_ascii_command(cmd, b"LREM") {
                Some(CommandId::Lrem)
            } else if eq_ascii_command(cmd, b"QUIT") {
                Some(CommandId::Quit)
            } else if eq_ascii_command(cmd, b"INFO") {
                Some(CommandId::Info)
            } else if eq_ascii_command(cmd, b"TIME") {
                Some(CommandId::Time)
            } else if eq_ascii_command(cmd, b"WAIT") {
                Some(CommandId::Wait)
            } else if eq_ascii_command(cmd, b"DUMP") {
                Some(CommandId::Dump)
            } else if eq_ascii_command(cmd, b"SORT") {
                Some(CommandId::Sort)
            } else if eq_ascii_command(cmd, b"COPY") {
                Some(CommandId::Copy)
            } else if eq_ascii_command(cmd, b"SCAN") {
                Some(CommandId::Scan)
            } else {
                None
            }
        }
        5 => {
            if eq_ascii_command(cmd, b"SETNX") {
                Some(CommandId::Setnx)
            } else if eq_ascii_command(cmd, b"HKEYS") {
                Some(CommandId::Hkeys)
            } else if eq_ascii_command(cmd, b"HVALS") {
                Some(CommandId::Hvals)
            } else if eq_ascii_command(cmd, b"HMGET") {
                Some(CommandId::Hmget)
            } else if eq_ascii_command(cmd, b"HMSET") {
                Some(CommandId::Hmset)
            } else if eq_ascii_command(cmd, b"LPUSH") {
                Some(CommandId::Lpush)
            } else if eq_ascii_command(cmd, b"RPUSH") {
                Some(CommandId::Rpush)
            } else if eq_ascii_command(cmd, b"SCARD") {
                Some(CommandId::Scard)
            } else if eq_ascii_command(cmd, b"ZRANK") {
                Some(CommandId::Zrank)
            } else if eq_ascii_command(cmd, b"ZCARD") {
                Some(CommandId::Zcard)
            } else if eq_ascii_command(cmd, b"SETEX") {
                Some(CommandId::Setex)
            } else if eq_ascii_command(cmd, b"SDIFF") {
                Some(CommandId::Sdiff)
            } else if eq_ascii_command(cmd, b"PFADD") {
                Some(CommandId::Pfadd)
            } else if eq_ascii_command(cmd, b"LTRIM") {
                Some(CommandId::Ltrim)
            } else if eq_ascii_command(cmd, b"LMOVE") {
                Some(CommandId::Lmove)
            } else if eq_ascii_command(cmd, b"SMOVE") {
                Some(CommandId::Smove)
            } else if eq_ascii_command(cmd, b"GETEX") {
                Some(CommandId::Getex)
            } else if eq_ascii_command(cmd, b"BITOP") {
                Some(CommandId::Bitop)
            } else if eq_ascii_command(cmd, b"HSCAN") {
                Some(CommandId::Hscan)
            } else if eq_ascii_command(cmd, b"SSCAN") {
                Some(CommandId::Sscan)
            } else if eq_ascii_command(cmd, b"ZSCAN") {
                Some(CommandId::Zscan)
            } else if eq_ascii_command(cmd, b"TOUCH") {
                Some(CommandId::Touch)
            } else if eq_ascii_command(cmd, b"RESET") {
                Some(CommandId::Reset)
            } else {
                None
            }
        }
        6 => {
            if eq_ascii_command(cmd, b"EXPIRE") {
                Some(CommandId::Expire)
            } else if eq_ascii_command(cmd, b"STRLEN") {
                Some(CommandId::Strlen)
            } else if eq_ascii_command(cmd, b"GETSET") {
                Some(CommandId::Getset)
            } else if eq_ascii_command(cmd, b"INCRBY") {
                Some(CommandId::Incrby)
            } else if eq_ascii_command(cmd, b"DECRBY") {
                Some(CommandId::Decrby)
            } else if eq_ascii_command(cmd, b"EXISTS") {
                Some(CommandId::Exists)
            } else if eq_ascii_command(cmd, b"RENAME") {
                Some(CommandId::Rename)
            } else if eq_ascii_command(cmd, b"DBSIZE") {
                Some(CommandId::Dbsize)
            } else if eq_ascii_command(cmd, b"APPEND") {
                Some(CommandId::Append)
            } else if eq_ascii_command(cmd, b"HSETNX") {
                Some(CommandId::Hsetnx)
            } else if eq_ascii_command(cmd, b"LRANGE") {
                Some(CommandId::Lrange)
            } else if eq_ascii_command(cmd, b"LINDEX") {
                Some(CommandId::Lindex)
            } else if eq_ascii_command(cmd, b"ZSCORE") {
                Some(CommandId::Zscore)
            } else if eq_ascii_command(cmd, b"ZRANGE") {
                Some(CommandId::Zrange)
            } else if eq_ascii_command(cmd, b"ZCOUNT") {
                Some(CommandId::Zcount)
            } else if eq_ascii_command(cmd, b"PSETEX") {
                Some(CommandId::Psetex)
            } else if eq_ascii_command(cmd, b"GETDEL") {
                Some(CommandId::Getdel)
            } else if eq_ascii_command(cmd, b"SINTER") {
                Some(CommandId::Sinter)
            } else if eq_ascii_command(cmd, b"SUNION") {
                Some(CommandId::Sunion)
            } else if eq_ascii_command(cmd, b"SETBIT") {
                Some(CommandId::Setbit)
            } else if eq_ascii_command(cmd, b"GETBIT") {
                Some(CommandId::Getbit)
            } else if eq_ascii_command(cmd, b"BITPOS") {
                Some(CommandId::Bitpos)
            } else if eq_ascii_command(cmd, b"LPUSHX") {
                Some(CommandId::Lpushx)
            } else if eq_ascii_command(cmd, b"RPUSHX") {
                Some(CommandId::Rpushx)
            } else if eq_ascii_command(cmd, b"SELECT") {
                Some(CommandId::Select)
            } else if eq_ascii_command(cmd, b"CONFIG") {
                Some(CommandId::Config)
            } else if eq_ascii_command(cmd, b"CLIENT") {
                Some(CommandId::Client)
            } else if eq_ascii_command(cmd, b"OBJECT") {
                Some(CommandId::Object)
            } else if eq_ascii_command(cmd, b"UNLINK") {
                Some(CommandId::Unlink)
            } else if eq_ascii_command(cmd, b"SUBSTR") {
                Some(CommandId::Substr)
            } else {
                None
            }
        }
        7 => {
            if eq_ascii_command(cmd, b"PEXPIRE") {
                Some(CommandId::Pexpire)
            } else if eq_ascii_command(cmd, b"PERSIST") {
                Some(CommandId::Persist)
            } else if eq_ascii_command(cmd, b"FLUSHDB") {
                Some(CommandId::Flushdb)
            } else if eq_ascii_command(cmd, b"HGETALL") {
                Some(CommandId::Hgetall)
            } else if eq_ascii_command(cmd, b"HEXISTS") {
                Some(CommandId::Hexists)
            } else if eq_ascii_command(cmd, b"HINCRBY") {
                Some(CommandId::Hincrby)
            } else if eq_ascii_command(cmd, b"HSTRLEN") {
                Some(CommandId::Hstrlen)
            } else if eq_ascii_command(cmd, b"ZINCRBY") {
                Some(CommandId::Zincrby)
            } else if eq_ascii_command(cmd, b"ZPOPMIN") {
                Some(CommandId::Zpopmin)
            } else if eq_ascii_command(cmd, b"ZPOPMAX") {
                Some(CommandId::Zpopmax)
            } else if eq_ascii_command(cmd, b"LINSERT") {
                Some(CommandId::Linsert)
            } else if eq_ascii_command(cmd, b"PFCOUNT") {
                Some(CommandId::Pfcount)
            } else if eq_ascii_command(cmd, b"PFMERGE") {
                Some(CommandId::Pfmerge)
            } else if eq_ascii_command(cmd, b"ZMSCORE") {
                Some(CommandId::Zmscore)
            } else if eq_ascii_command(cmd, b"COMMAND") {
                Some(CommandId::Command)
            } else if eq_ascii_command(cmd, b"RESTORE") {
                Some(CommandId::Restore)
            } else {
                None
            }
        }
        8 => {
            if eq_ascii_command(cmd, b"EXPIREAT") {
                Some(CommandId::Expireat)
            } else if eq_ascii_command(cmd, b"RENAMENX") {
                Some(CommandId::Renamenx)
            } else if eq_ascii_command(cmd, b"FLUSHALL") {
                Some(CommandId::Flushdb)
            } else if eq_ascii_command(cmd, b"SMEMBERS") {
                Some(CommandId::Smembers)
            } else if eq_ascii_command(cmd, b"ZREVRANK") {
                Some(CommandId::Zrevrank)
            } else if eq_ascii_command(cmd, b"GETRANGE") {
                Some(CommandId::Getrange)
            } else if eq_ascii_command(cmd, b"SETRANGE") {
                Some(CommandId::Setrange)
            } else if eq_ascii_command(cmd, b"BITCOUNT") {
                Some(CommandId::Bitcount)
            } else {
                None
            }
        }
        9 => {
            if eq_ascii_command(cmd, b"PEXPIREAT") {
                Some(CommandId::Pexpireat)
            } else if eq_ascii_command(cmd, b"SISMEMBER") {
                Some(CommandId::Sismember)
            } else if eq_ascii_command(cmd, b"ZREVRANGE") {
                Some(CommandId::Zrevrange)
            } else if eq_ascii_command(cmd, b"RPOPLPUSH") {
                Some(CommandId::Rpoplpush)
            } else if eq_ascii_command(cmd, b"ZLEXCOUNT") {
                Some(CommandId::Zlexcount)
            } else if eq_ascii_command(cmd, b"RANDOMKEY") {
                Some(CommandId::Randomkey)
            } else {
                None
            }
        }
        10 => {
            if eq_ascii_command(cmd, b"EXPIRETIME") {
                Some(CommandId::Expiretime)
            } else if eq_ascii_command(cmd, b"HRANDFIELD") {
                Some(CommandId::Hrandfield)
            } else if eq_ascii_command(cmd, b"SDIFFSTORE") {
                Some(CommandId::Sdiffstore)
            } else if eq_ascii_command(cmd, b"SMISMEMBER") {
                Some(CommandId::Smismember)
            } else {
                None
            }
        }
        11 => {
            if eq_ascii_command(cmd, b"PEXPIRETIME") {
                Some(CommandId::Pexpiretime)
            } else if eq_ascii_command(cmd, b"INCRBYFLOAT") {
                Some(CommandId::Incrbyfloat)
            } else if eq_ascii_command(cmd, b"SRANDMEMBER") {
                Some(CommandId::Srandmember)
            } else if eq_ascii_command(cmd, b"ZRANGEBYLEX") {
                Some(CommandId::Zrangebylex)
            } else if eq_ascii_command(cmd, b"SINTERSTORE") {
                Some(CommandId::Sinterstore)
            } else if eq_ascii_command(cmd, b"SUNIONSTORE") {
                Some(CommandId::Sunionstore)
            } else if eq_ascii_command(cmd, b"ZRANDMEMBER") {
                Some(CommandId::Zrandmember)
            } else if eq_ascii_command(cmd, b"ZUNIONSTORE") {
                Some(CommandId::Zunionstore)
            } else if eq_ascii_command(cmd, b"ZINTERSTORE") {
                Some(CommandId::Zinterstore)
            } else {
                None
            }
        }
        12 => {
            if eq_ascii_command(cmd, b"HINCRBYFLOAT") {
                Some(CommandId::Hincrbyfloat)
            } else {
                None
            }
        }
        13 => {
            if eq_ascii_command(cmd, b"ZRANGEBYSCORE") {
                Some(CommandId::Zrangebyscore)
            } else {
                None
            }
        }
        14 => {
            if eq_ascii_command(cmd, b"ZREVRANGEBYLEX") {
                Some(CommandId::Zrevrangebylex)
            } else if eq_ascii_command(cmd, b"ZREMRANGEBYLEX") {
                Some(CommandId::Zremrangebylex)
            } else {
                None
            }
        }
        15 => {
            if eq_ascii_command(cmd, b"ZREMRANGEBYRANK") {
                Some(CommandId::Zremrangebyrank)
            } else {
                None
            }
        }
        16 => {
            if eq_ascii_command(cmd, b"ZREVRANGEBYSCORE") {
                Some(CommandId::Zrevrangebyscore)
            } else if eq_ascii_command(cmd, b"ZREMRANGEBYSCORE") {
                Some(CommandId::Zremrangebyscore)
            } else {
                None
            }
        }
        _ => None,
    }
}

#[inline]
fn eq_ascii_command(lhs: &[u8], rhs: &[u8]) -> bool {
    lhs.len() == rhs.len()
        && lhs
            .iter()
            .zip(rhs.iter())
            .all(|(left, right)| left.to_ascii_uppercase() == *right)
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

    let mut options = argv[3..].iter();
    while let Some(option_arg) = options.next() {
        let option =
            std::str::from_utf8(option_arg).map_err(|_| CommandError::InvalidUtf8Argument)?;
        if option.eq_ignore_ascii_case("PX") {
            let Some(ttl_arg) = options.next() else {
                return Err(CommandError::SyntaxError);
            };
            let ttl = parse_u64_arg(ttl_arg)?;
            px_ttl_ms = Some(ttl);
        } else if option.eq_ignore_ascii_case("EX") {
            let Some(seconds_arg) = options.next() else {
                return Err(CommandError::SyntaxError);
            };
            let seconds = parse_u64_arg(seconds_arg)?;
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
    }

    if nx && xx {
        return Err(CommandError::SyntaxError);
    }

    let old_value = if get {
        store.get(&argv[1], now_ms)?
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
    Ok(RespFrame::BulkString(store.get(&argv[1], now_ms)?))
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
    let new_len = store.append(&argv[1], &argv[2], now_ms)?;
    let new_len = i64::try_from(new_len).unwrap_or(i64::MAX);
    Ok(RespFrame::Integer(new_len))
}

fn strlen(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 2 {
        return Err(CommandError::WrongArity("STRLEN"));
    }
    let len = store.strlen(&argv[1], now_ms)?;
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
    let old = store.getset(argv[1].clone(), argv[2].clone(), now_ms)?;
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

fn hset(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() < 4 || !(argv.len() - 2).is_multiple_of(2) {
        return Err(CommandError::WrongArity("HSET"));
    }
    let mut added = 0_usize;
    let mut i = 2;
    while i + 1 < argv.len() {
        if store.hset(&argv[1], argv[i].clone(), argv[i + 1].clone(), now_ms)? {
            added += 1;
        }
        i += 2;
    }
    Ok(RespFrame::Integer(i64::try_from(added).unwrap_or(i64::MAX)))
}

fn hget(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 3 {
        return Err(CommandError::WrongArity("HGET"));
    }
    let value = store.hget(&argv[1], &argv[2], now_ms)?;
    Ok(RespFrame::BulkString(value))
}

fn hdel(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() < 3 {
        return Err(CommandError::WrongArity("HDEL"));
    }
    let fields: Vec<&[u8]> = argv[2..].iter().map(Vec::as_slice).collect();
    let removed = store.hdel(&argv[1], &fields, now_ms)?;
    Ok(RespFrame::Integer(
        i64::try_from(removed).unwrap_or(i64::MAX),
    ))
}

fn hexists(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 3 {
        return Err(CommandError::WrongArity("HEXISTS"));
    }
    let exists = store.hexists(&argv[1], &argv[2], now_ms)?;
    Ok(RespFrame::Integer(if exists { 1 } else { 0 }))
}

fn hlen(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 2 {
        return Err(CommandError::WrongArity("HLEN"));
    }
    let len = store.hlen(&argv[1], now_ms)?;
    Ok(RespFrame::Integer(i64::try_from(len).unwrap_or(i64::MAX)))
}

#[allow(clippy::type_complexity)]
fn hgetall(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 2 {
        return Err(CommandError::WrongArity("HGETALL"));
    }
    let pairs = store.hgetall(&argv[1], now_ms)?;
    let mut frames = Vec::with_capacity(pairs.len() * 2);
    for (field, value) in pairs {
        frames.push(RespFrame::BulkString(Some(field)));
        frames.push(RespFrame::BulkString(Some(value)));
    }
    Ok(RespFrame::Array(Some(frames)))
}

fn hkeys(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 2 {
        return Err(CommandError::WrongArity("HKEYS"));
    }
    let keys = store.hkeys(&argv[1], now_ms)?;
    let frames = keys
        .into_iter()
        .map(|k| RespFrame::BulkString(Some(k)))
        .collect();
    Ok(RespFrame::Array(Some(frames)))
}

fn hvals(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 2 {
        return Err(CommandError::WrongArity("HVALS"));
    }
    let vals = store.hvals(&argv[1], now_ms)?;
    let frames = vals
        .into_iter()
        .map(|v| RespFrame::BulkString(Some(v)))
        .collect();
    Ok(RespFrame::Array(Some(frames)))
}

fn hmget(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() < 3 {
        return Err(CommandError::WrongArity("HMGET"));
    }
    let fields: Vec<&[u8]> = argv[2..].iter().map(Vec::as_slice).collect();
    let values = store.hmget(&argv[1], &fields, now_ms)?;
    let frames = values.into_iter().map(RespFrame::BulkString).collect();
    Ok(RespFrame::Array(Some(frames)))
}

fn hmset(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() < 4 || !(argv.len() - 2).is_multiple_of(2) {
        return Err(CommandError::WrongArity("HMSET"));
    }
    let mut i = 2;
    while i + 1 < argv.len() {
        store.hset(&argv[1], argv[i].clone(), argv[i + 1].clone(), now_ms)?;
        i += 2;
    }
    Ok(RespFrame::SimpleString("OK".to_string()))
}

fn hincrby(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 4 {
        return Err(CommandError::WrongArity("HINCRBY"));
    }
    let delta = parse_i64_arg(&argv[3])?;
    let value = store.hincrby(&argv[1], &argv[2], delta, now_ms)?;
    Ok(RespFrame::Integer(value))
}

fn hsetnx_cmd(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 4 {
        return Err(CommandError::WrongArity("HSETNX"));
    }
    let set = store.hsetnx(&argv[1], argv[2].clone(), argv[3].clone(), now_ms)?;
    Ok(RespFrame::Integer(if set { 1 } else { 0 }))
}

fn hstrlen(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 3 {
        return Err(CommandError::WrongArity("HSTRLEN"));
    }
    let len = store.hstrlen(&argv[1], &argv[2], now_ms)?;
    Ok(RespFrame::Integer(i64::try_from(len).unwrap_or(i64::MAX)))
}

fn lpush(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() < 3 {
        return Err(CommandError::WrongArity("LPUSH"));
    }
    let len = store.lpush(&argv[1], &argv[2..], now_ms)?;
    Ok(RespFrame::Integer(i64::try_from(len).unwrap_or(i64::MAX)))
}

fn rpush(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() < 3 {
        return Err(CommandError::WrongArity("RPUSH"));
    }
    let len = store.rpush(&argv[1], &argv[2..], now_ms)?;
    Ok(RespFrame::Integer(i64::try_from(len).unwrap_or(i64::MAX)))
}

fn lpop(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 2 {
        return Err(CommandError::WrongArity("LPOP"));
    }
    let value = store.lpop(&argv[1], now_ms)?;
    Ok(RespFrame::BulkString(value))
}

fn rpop(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 2 {
        return Err(CommandError::WrongArity("RPOP"));
    }
    let value = store.rpop(&argv[1], now_ms)?;
    Ok(RespFrame::BulkString(value))
}

fn llen(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 2 {
        return Err(CommandError::WrongArity("LLEN"));
    }
    let len = store.llen(&argv[1], now_ms)?;
    Ok(RespFrame::Integer(i64::try_from(len).unwrap_or(i64::MAX)))
}

fn lrange(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 4 {
        return Err(CommandError::WrongArity("LRANGE"));
    }
    let start = parse_i64_arg(&argv[2])?;
    let stop = parse_i64_arg(&argv[3])?;
    let values = store.lrange(&argv[1], start, stop, now_ms)?;
    let frames = values
        .into_iter()
        .map(|v| RespFrame::BulkString(Some(v)))
        .collect();
    Ok(RespFrame::Array(Some(frames)))
}

fn lindex(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 3 {
        return Err(CommandError::WrongArity("LINDEX"));
    }
    let index = parse_i64_arg(&argv[2])?;
    let value = store.lindex(&argv[1], index, now_ms)?;
    Ok(RespFrame::BulkString(value))
}

fn lset_cmd(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 4 {
        return Err(CommandError::WrongArity("LSET"));
    }
    let index = parse_i64_arg(&argv[2])?;
    store.lset(&argv[1], index, argv[3].clone(), now_ms)?;
    Ok(RespFrame::SimpleString("OK".to_string()))
}

fn sadd(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() < 3 {
        return Err(CommandError::WrongArity("SADD"));
    }
    let added = store.sadd(&argv[1], &argv[2..], now_ms)?;
    Ok(RespFrame::Integer(i64::try_from(added).unwrap_or(i64::MAX)))
}

fn srem(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() < 3 {
        return Err(CommandError::WrongArity("SREM"));
    }
    let members: Vec<&[u8]> = argv[2..].iter().map(Vec::as_slice).collect();
    let removed = store.srem(&argv[1], &members, now_ms)?;
    Ok(RespFrame::Integer(
        i64::try_from(removed).unwrap_or(i64::MAX),
    ))
}

fn smembers(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 2 {
        return Err(CommandError::WrongArity("SMEMBERS"));
    }
    let members = store.smembers(&argv[1], now_ms)?;
    let frames = members
        .into_iter()
        .map(|m| RespFrame::BulkString(Some(m)))
        .collect();
    Ok(RespFrame::Array(Some(frames)))
}

fn scard(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 2 {
        return Err(CommandError::WrongArity("SCARD"));
    }
    let len = store.scard(&argv[1], now_ms)?;
    Ok(RespFrame::Integer(i64::try_from(len).unwrap_or(i64::MAX)))
}

fn sismember(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 3 {
        return Err(CommandError::WrongArity("SISMEMBER"));
    }
    let is_member = store.sismember(&argv[1], &argv[2], now_ms)?;
    Ok(RespFrame::Integer(if is_member { 1 } else { 0 }))
}

fn parse_f64_arg(arg: &[u8]) -> Result<f64, CommandError> {
    let text = std::str::from_utf8(arg).map_err(|_| CommandError::InvalidUtf8Argument)?;
    text.parse::<f64>()
        .map_err(|_| CommandError::InvalidInteger)
}

fn zadd(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    // ZADD key score member [score member ...]
    if argv.len() < 4 || !(argv.len() - 2).is_multiple_of(2) {
        return Err(CommandError::WrongArity("ZADD"));
    }
    let mut pairs = Vec::with_capacity((argv.len() - 2) / 2);
    let mut i = 2;
    while i + 1 < argv.len() {
        let score = parse_f64_arg(&argv[i])?;
        pairs.push((score, argv[i + 1].clone()));
        i += 2;
    }
    let added = store.zadd(&argv[1], &pairs, now_ms)?;
    Ok(RespFrame::Integer(i64::try_from(added).unwrap_or(i64::MAX)))
}

fn zrem(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() < 3 {
        return Err(CommandError::WrongArity("ZREM"));
    }
    let members: Vec<&[u8]> = argv[2..].iter().map(Vec::as_slice).collect();
    let removed = store.zrem(&argv[1], &members, now_ms)?;
    Ok(RespFrame::Integer(
        i64::try_from(removed).unwrap_or(i64::MAX),
    ))
}

fn zscore(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 3 {
        return Err(CommandError::WrongArity("ZSCORE"));
    }
    match store.zscore(&argv[1], &argv[2], now_ms)? {
        Some(score) => Ok(RespFrame::BulkString(Some(score.to_string().into_bytes()))),
        None => Ok(RespFrame::BulkString(None)),
    }
}

fn zcard(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 2 {
        return Err(CommandError::WrongArity("ZCARD"));
    }
    let len = store.zcard(&argv[1], now_ms)?;
    Ok(RespFrame::Integer(i64::try_from(len).unwrap_or(i64::MAX)))
}

fn zrank(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 3 {
        return Err(CommandError::WrongArity("ZRANK"));
    }
    match store.zrank(&argv[1], &argv[2], now_ms)? {
        Some(rank) => Ok(RespFrame::Integer(i64::try_from(rank).unwrap_or(i64::MAX))),
        None => Ok(RespFrame::BulkString(None)),
    }
}

fn zrevrank(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 3 {
        return Err(CommandError::WrongArity("ZREVRANK"));
    }
    match store.zrevrank(&argv[1], &argv[2], now_ms)? {
        Some(rank) => Ok(RespFrame::Integer(i64::try_from(rank).unwrap_or(i64::MAX))),
        None => Ok(RespFrame::BulkString(None)),
    }
}

fn zrange(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    // ZRANGE key start stop [WITHSCORES]
    if argv.len() < 4 || argv.len() > 5 {
        return Err(CommandError::WrongArity("ZRANGE"));
    }
    let start = parse_i64_arg(&argv[2])?;
    let stop = parse_i64_arg(&argv[3])?;
    let withscores = argv.len() == 5
        && std::str::from_utf8(&argv[4])
            .map(|s| s.eq_ignore_ascii_case("WITHSCORES"))
            .unwrap_or(false);
    if argv.len() == 5 && !withscores {
        return Err(CommandError::SyntaxError);
    }
    if withscores {
        let pairs = store.zrange_withscores(&argv[1], start, stop, now_ms)?;
        let mut frames = Vec::with_capacity(pairs.len() * 2);
        for (member, score) in pairs {
            frames.push(RespFrame::BulkString(Some(member)));
            frames.push(RespFrame::BulkString(Some(score.to_string().into_bytes())));
        }
        Ok(RespFrame::Array(Some(frames)))
    } else {
        let members = store.zrange(&argv[1], start, stop, now_ms)?;
        let frames = members
            .into_iter()
            .map(|m| RespFrame::BulkString(Some(m)))
            .collect();
        Ok(RespFrame::Array(Some(frames)))
    }
}

fn zrevrange(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() < 4 || argv.len() > 5 {
        return Err(CommandError::WrongArity("ZREVRANGE"));
    }
    let start = parse_i64_arg(&argv[2])?;
    let stop = parse_i64_arg(&argv[3])?;
    let withscores = argv.len() == 5
        && std::str::from_utf8(&argv[4])
            .map(|s| s.eq_ignore_ascii_case("WITHSCORES"))
            .unwrap_or(false);
    if argv.len() == 5 && !withscores {
        return Err(CommandError::SyntaxError);
    }
    let members = store.zrevrange(&argv[1], start, stop, now_ms)?;
    if withscores {
        // Need scores too - get them individually
        let mut frames = Vec::with_capacity(members.len() * 2);
        for m in &members {
            frames.push(RespFrame::BulkString(Some(m.clone())));
            let score = store.zscore(&argv[1], m, now_ms)?.unwrap_or(0.0);
            frames.push(RespFrame::BulkString(Some(score.to_string().into_bytes())));
        }
        Ok(RespFrame::Array(Some(frames)))
    } else {
        let frames = members
            .into_iter()
            .map(|m| RespFrame::BulkString(Some(m)))
            .collect();
        Ok(RespFrame::Array(Some(frames)))
    }
}

fn zrangebyscore(
    argv: &[Vec<u8>],
    store: &mut Store,
    now_ms: u64,
) -> Result<RespFrame, CommandError> {
    if argv.len() != 4 {
        return Err(CommandError::WrongArity("ZRANGEBYSCORE"));
    }
    let min = parse_score_bound(&argv[2])?;
    let max = parse_score_bound(&argv[3])?;
    let members = store.zrangebyscore(&argv[1], min, max, now_ms)?;
    let frames = members
        .into_iter()
        .map(|m| RespFrame::BulkString(Some(m)))
        .collect();
    Ok(RespFrame::Array(Some(frames)))
}

fn zcount(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 4 {
        return Err(CommandError::WrongArity("ZCOUNT"));
    }
    let min = parse_score_bound(&argv[2])?;
    let max = parse_score_bound(&argv[3])?;
    let count = store.zcount(&argv[1], min, max, now_ms)?;
    Ok(RespFrame::Integer(i64::try_from(count).unwrap_or(i64::MAX)))
}

fn zincrby(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 4 {
        return Err(CommandError::WrongArity("ZINCRBY"));
    }
    let delta = parse_f64_arg(&argv[2])?;
    let new_score = store.zincrby(&argv[1], argv[3].clone(), delta, now_ms)?;
    Ok(RespFrame::BulkString(Some(
        new_score.to_string().into_bytes(),
    )))
}

fn zpopmin(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 2 {
        return Err(CommandError::WrongArity("ZPOPMIN"));
    }
    match store.zpopmin(&argv[1], now_ms)? {
        Some((member, score)) => Ok(RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(member)),
            RespFrame::BulkString(Some(score.to_string().into_bytes())),
        ]))),
        None => Ok(RespFrame::Array(Some(vec![]))),
    }
}

fn zpopmax(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 2 {
        return Err(CommandError::WrongArity("ZPOPMAX"));
    }
    match store.zpopmax(&argv[1], now_ms)? {
        Some((member, score)) => Ok(RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(member)),
            RespFrame::BulkString(Some(score.to_string().into_bytes())),
        ]))),
        None => Ok(RespFrame::Array(Some(vec![]))),
    }
}

fn parse_score_bound(arg: &[u8]) -> Result<f64, CommandError> {
    let text = std::str::from_utf8(arg).map_err(|_| CommandError::InvalidUtf8Argument)?;
    if text == "-inf" {
        Ok(f64::NEG_INFINITY)
    } else if text == "+inf" || text == "inf" {
        Ok(f64::INFINITY)
    } else {
        text.parse::<f64>()
            .map_err(|_| CommandError::InvalidInteger)
    }
}

fn setex(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    // SETEX key seconds value
    if argv.len() != 4 {
        return Err(CommandError::WrongArity("SETEX"));
    }
    let seconds = parse_i64_arg(&argv[2])?;
    if seconds <= 0 {
        return Err(CommandError::InvalidInteger);
    }
    let px = u64::try_from(seconds)
        .unwrap_or(u64::MAX)
        .saturating_mul(1000);
    store.set(argv[1].clone(), argv[3].clone(), Some(px), now_ms);
    Ok(RespFrame::SimpleString("OK".to_string()))
}

fn psetex(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    // PSETEX key milliseconds value
    if argv.len() != 4 {
        return Err(CommandError::WrongArity("PSETEX"));
    }
    let ms = parse_i64_arg(&argv[2])?;
    if ms <= 0 {
        return Err(CommandError::InvalidInteger);
    }
    let px = u64::try_from(ms).unwrap_or(u64::MAX);
    store.set(argv[1].clone(), argv[3].clone(), Some(px), now_ms);
    Ok(RespFrame::SimpleString("OK".to_string()))
}

fn getdel(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 2 {
        return Err(CommandError::WrongArity("GETDEL"));
    }
    match store.getdel(&argv[1], now_ms)? {
        Some(v) => Ok(RespFrame::BulkString(Some(v))),
        None => Ok(RespFrame::BulkString(None)),
    }
}

fn getrange(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 4 {
        return Err(CommandError::WrongArity("GETRANGE"));
    }
    let start = parse_i64_arg(&argv[2])?;
    let end = parse_i64_arg(&argv[3])?;
    let result = store.getrange(&argv[1], start, end, now_ms)?;
    Ok(RespFrame::BulkString(Some(result)))
}

fn setrange(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 4 {
        return Err(CommandError::WrongArity("SETRANGE"));
    }
    let offset = parse_i64_arg(&argv[2])?;
    if offset < 0 {
        return Err(CommandError::InvalidInteger);
    }
    let new_len = store.setrange(&argv[1], offset as usize, &argv[3], now_ms)?;
    Ok(RespFrame::Integer(
        i64::try_from(new_len).unwrap_or(i64::MAX),
    ))
}

fn incrbyfloat(
    argv: &[Vec<u8>],
    store: &mut Store,
    now_ms: u64,
) -> Result<RespFrame, CommandError> {
    if argv.len() != 3 {
        return Err(CommandError::WrongArity("INCRBYFLOAT"));
    }
    let delta = parse_f64_arg(&argv[2])?;
    let new_val = store.incrbyfloat(&argv[1], delta, now_ms)?;
    Ok(RespFrame::BulkString(Some(
        new_val.to_string().into_bytes(),
    )))
}

fn sinter(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() < 2 {
        return Err(CommandError::WrongArity("SINTER"));
    }
    let keys: Vec<&[u8]> = argv[1..].iter().map(Vec::as_slice).collect();
    let members = store.sinter(&keys, now_ms)?;
    let frames = members
        .into_iter()
        .map(|m| RespFrame::BulkString(Some(m)))
        .collect();
    Ok(RespFrame::Array(Some(frames)))
}

fn sunion(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() < 2 {
        return Err(CommandError::WrongArity("SUNION"));
    }
    let keys: Vec<&[u8]> = argv[1..].iter().map(Vec::as_slice).collect();
    let members = store.sunion(&keys, now_ms)?;
    let frames = members
        .into_iter()
        .map(|m| RespFrame::BulkString(Some(m)))
        .collect();
    Ok(RespFrame::Array(Some(frames)))
}

fn sdiff(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() < 2 {
        return Err(CommandError::WrongArity("SDIFF"));
    }
    let keys: Vec<&[u8]> = argv[1..].iter().map(Vec::as_slice).collect();
    let members = store.sdiff(&keys, now_ms)?;
    let frames = members
        .into_iter()
        .map(|m| RespFrame::BulkString(Some(m)))
        .collect();
    Ok(RespFrame::Array(Some(frames)))
}

fn spop(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 2 {
        return Err(CommandError::WrongArity("SPOP"));
    }
    match store.spop(&argv[1], now_ms)? {
        Some(m) => Ok(RespFrame::BulkString(Some(m))),
        None => Ok(RespFrame::BulkString(None)),
    }
}

fn srandmember(
    argv: &[Vec<u8>],
    store: &mut Store,
    now_ms: u64,
) -> Result<RespFrame, CommandError> {
    if argv.len() != 2 {
        return Err(CommandError::WrongArity("SRANDMEMBER"));
    }
    match store.srandmember(&argv[1], now_ms)? {
        Some(m) => Ok(RespFrame::BulkString(Some(m))),
        None => Ok(RespFrame::BulkString(None)),
    }
}

fn smove(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 4 {
        return Err(CommandError::WrongArity("SMOVE"));
    }
    let moved = store.smove(&argv[1], &argv[2], &argv[3], now_ms)?;
    Ok(RespFrame::Integer(if moved { 1 } else { 0 }))
}

fn sinterstore(
    argv: &[Vec<u8>],
    store: &mut Store,
    now_ms: u64,
) -> Result<RespFrame, CommandError> {
    if argv.len() < 3 {
        return Err(CommandError::WrongArity("SINTERSTORE"));
    }
    let keys: Vec<&[u8]> = argv[2..].iter().map(|a| a.as_slice()).collect();
    let count = store.sinterstore(&argv[1], &keys, now_ms)?;
    Ok(RespFrame::Integer(i64::try_from(count).unwrap_or(i64::MAX)))
}

fn sunionstore(
    argv: &[Vec<u8>],
    store: &mut Store,
    now_ms: u64,
) -> Result<RespFrame, CommandError> {
    if argv.len() < 3 {
        return Err(CommandError::WrongArity("SUNIONSTORE"));
    }
    let keys: Vec<&[u8]> = argv[2..].iter().map(|a| a.as_slice()).collect();
    let count = store.sunionstore(&argv[1], &keys, now_ms)?;
    Ok(RespFrame::Integer(i64::try_from(count).unwrap_or(i64::MAX)))
}

fn sdiffstore(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() < 3 {
        return Err(CommandError::WrongArity("SDIFFSTORE"));
    }
    let keys: Vec<&[u8]> = argv[2..].iter().map(|a| a.as_slice()).collect();
    let count = store.sdiffstore(&argv[1], &keys, now_ms)?;
    Ok(RespFrame::Integer(i64::try_from(count).unwrap_or(i64::MAX)))
}

fn ltrim(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 4 {
        return Err(CommandError::WrongArity("LTRIM"));
    }
    let start = parse_i64_arg(&argv[2])?;
    let stop = parse_i64_arg(&argv[3])?;
    store.ltrim(&argv[1], start, stop, now_ms)?;
    Ok(RespFrame::SimpleString("OK".to_string()))
}

fn lpushx(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() < 3 {
        return Err(CommandError::WrongArity("LPUSHX"));
    }
    let values: Vec<Vec<u8>> = argv[2..].to_vec();
    let len = store.lpushx(&argv[1], &values, now_ms)?;
    Ok(RespFrame::Integer(i64::try_from(len).unwrap_or(i64::MAX)))
}

fn rpushx(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() < 3 {
        return Err(CommandError::WrongArity("RPUSHX"));
    }
    let values: Vec<Vec<u8>> = argv[2..].to_vec();
    let len = store.rpushx(&argv[1], &values, now_ms)?;
    Ok(RespFrame::Integer(i64::try_from(len).unwrap_or(i64::MAX)))
}

fn lmove(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 5 {
        return Err(CommandError::WrongArity("LMOVE"));
    }
    match store.lmove(&argv[1], &argv[2], &argv[3], &argv[4], now_ms)? {
        Some(v) => Ok(RespFrame::BulkString(Some(v))),
        None => Ok(RespFrame::BulkString(None)),
    }
}

fn zremrangebyrank(
    argv: &[Vec<u8>],
    store: &mut Store,
    now_ms: u64,
) -> Result<RespFrame, CommandError> {
    if argv.len() != 4 {
        return Err(CommandError::WrongArity("ZREMRANGEBYRANK"));
    }
    let start = parse_i64_arg(&argv[2])?;
    let stop = parse_i64_arg(&argv[3])?;
    let removed = store.zremrangebyrank(&argv[1], start, stop, now_ms)?;
    Ok(RespFrame::Integer(
        i64::try_from(removed).unwrap_or(i64::MAX),
    ))
}

fn zremrangebyscore(
    argv: &[Vec<u8>],
    store: &mut Store,
    now_ms: u64,
) -> Result<RespFrame, CommandError> {
    if argv.len() != 4 {
        return Err(CommandError::WrongArity("ZREMRANGEBYSCORE"));
    }
    let min = parse_score_bound(&argv[2])?;
    let max = parse_score_bound(&argv[3])?;
    let removed = store.zremrangebyscore(&argv[1], min, max, now_ms)?;
    Ok(RespFrame::Integer(
        i64::try_from(removed).unwrap_or(i64::MAX),
    ))
}

fn zremrangebylex(
    argv: &[Vec<u8>],
    store: &mut Store,
    now_ms: u64,
) -> Result<RespFrame, CommandError> {
    if argv.len() != 4 {
        return Err(CommandError::WrongArity("ZREMRANGEBYLEX"));
    }
    let removed = store.zremrangebylex(&argv[1], &argv[2], &argv[3], now_ms)?;
    Ok(RespFrame::Integer(
        i64::try_from(removed).unwrap_or(i64::MAX),
    ))
}

fn zrandmember(
    argv: &[Vec<u8>],
    store: &mut Store,
    now_ms: u64,
) -> Result<RespFrame, CommandError> {
    if argv.len() != 2 {
        return Err(CommandError::WrongArity("ZRANDMEMBER"));
    }
    match store.zrandmember(&argv[1], now_ms)? {
        Some(m) => Ok(RespFrame::BulkString(Some(m))),
        None => Ok(RespFrame::BulkString(None)),
    }
}

fn zmscore(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() < 3 {
        return Err(CommandError::WrongArity("ZMSCORE"));
    }
    let members: Vec<&[u8]> = argv[2..].iter().map(|a| a.as_slice()).collect();
    let scores = store.zmscore(&argv[1], &members, now_ms)?;
    let frames = scores
        .into_iter()
        .map(|s| match s {
            Some(score) => RespFrame::BulkString(Some(score.to_string().into_bytes())),
            None => RespFrame::BulkString(None),
        })
        .collect();
    Ok(RespFrame::Array(Some(frames)))
}

fn setbit(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 4 {
        return Err(CommandError::WrongArity("SETBIT"));
    }
    let offset = parse_i64_arg(&argv[2])?;
    if offset < 0 {
        return Err(CommandError::InvalidInteger);
    }
    let bit_val = parse_i64_arg(&argv[3])?;
    if bit_val != 0 && bit_val != 1 {
        return Err(CommandError::InvalidInteger);
    }
    let old = store.setbit(&argv[1], offset as usize, bit_val == 1, now_ms)?;
    Ok(RespFrame::Integer(if old { 1 } else { 0 }))
}

fn getbit(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 3 {
        return Err(CommandError::WrongArity("GETBIT"));
    }
    let offset = parse_i64_arg(&argv[2])?;
    if offset < 0 {
        return Err(CommandError::InvalidInteger);
    }
    let bit = store.getbit(&argv[1], offset as usize, now_ms)?;
    Ok(RespFrame::Integer(if bit { 1 } else { 0 }))
}

fn bitcount(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() < 2 || argv.len() > 4 {
        return Err(CommandError::WrongArity("BITCOUNT"));
    }
    let (start, end) = if argv.len() >= 4 {
        (
            Some(parse_i64_arg(&argv[2])?),
            Some(parse_i64_arg(&argv[3])?),
        )
    } else {
        (None, None)
    };
    let count = store.bitcount(&argv[1], start, end, now_ms)?;
    Ok(RespFrame::Integer(i64::try_from(count).unwrap_or(i64::MAX)))
}

fn bitpos(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() < 3 || argv.len() > 5 {
        return Err(CommandError::WrongArity("BITPOS"));
    }
    let bit_val = parse_i64_arg(&argv[2])?;
    if bit_val != 0 && bit_val != 1 {
        return Err(CommandError::InvalidInteger);
    }
    let start = if argv.len() >= 4 {
        Some(parse_i64_arg(&argv[3])?)
    } else {
        None
    };
    let end = if argv.len() >= 5 {
        Some(parse_i64_arg(&argv[4])?)
    } else {
        None
    };
    let pos = store.bitpos(&argv[1], bit_val == 1, start, end, now_ms)?;
    Ok(RespFrame::Integer(pos))
}

fn lpos(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 3 {
        return Err(CommandError::WrongArity("LPOS"));
    }
    match store.lpos(&argv[1], &argv[2], now_ms)? {
        Some(pos) => Ok(RespFrame::Integer(i64::try_from(pos).unwrap_or(i64::MAX))),
        None => Ok(RespFrame::BulkString(None)),
    }
}

fn linsert(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    // LINSERT key BEFORE|AFTER pivot element
    if argv.len() != 5 {
        return Err(CommandError::WrongArity("LINSERT"));
    }
    let direction = std::str::from_utf8(&argv[2]).map_err(|_| CommandError::InvalidUtf8Argument)?;
    if direction.eq_ignore_ascii_case("BEFORE") {
        let len = store.linsert_before(&argv[1], &argv[3], argv[4].clone(), now_ms)?;
        Ok(RespFrame::Integer(len))
    } else if direction.eq_ignore_ascii_case("AFTER") {
        let len = store.linsert_after(&argv[1], &argv[3], argv[4].clone(), now_ms)?;
        Ok(RespFrame::Integer(len))
    } else {
        Err(CommandError::SyntaxError)
    }
}

fn lrem(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 4 {
        return Err(CommandError::WrongArity("LREM"));
    }
    let count = parse_i64_arg(&argv[2])?;
    let removed = store.lrem(&argv[1], count, &argv[3], now_ms)?;
    Ok(RespFrame::Integer(
        i64::try_from(removed).unwrap_or(i64::MAX),
    ))
}

fn rpoplpush(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 3 {
        return Err(CommandError::WrongArity("RPOPLPUSH"));
    }
    match store.rpoplpush(&argv[1], &argv[2], now_ms)? {
        Some(v) => Ok(RespFrame::BulkString(Some(v))),
        None => Ok(RespFrame::BulkString(None)),
    }
}

fn hincrbyfloat(
    argv: &[Vec<u8>],
    store: &mut Store,
    now_ms: u64,
) -> Result<RespFrame, CommandError> {
    if argv.len() != 4 {
        return Err(CommandError::WrongArity("HINCRBYFLOAT"));
    }
    let delta = parse_f64_arg(&argv[3])?;
    let new_val = store.hincrbyfloat(&argv[1], &argv[2], delta, now_ms)?;
    Ok(RespFrame::BulkString(Some(
        new_val.to_string().into_bytes(),
    )))
}

fn hrandfield(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 2 {
        return Err(CommandError::WrongArity("HRANDFIELD"));
    }
    match store.hrandfield(&argv[1], now_ms)? {
        Some(field) => Ok(RespFrame::BulkString(Some(field))),
        None => Ok(RespFrame::BulkString(None)),
    }
}

fn zrevrangebyscore(
    argv: &[Vec<u8>],
    store: &mut Store,
    now_ms: u64,
) -> Result<RespFrame, CommandError> {
    // ZREVRANGEBYSCORE key max min
    if argv.len() != 4 {
        return Err(CommandError::WrongArity("ZREVRANGEBYSCORE"));
    }
    let max = parse_score_bound(&argv[2])?;
    let min = parse_score_bound(&argv[3])?;
    let members = store.zrevrangebyscore(&argv[1], max, min, now_ms)?;
    let frames = members
        .into_iter()
        .map(|m| RespFrame::BulkString(Some(m)))
        .collect();
    Ok(RespFrame::Array(Some(frames)))
}

fn zrangebylex(
    argv: &[Vec<u8>],
    store: &mut Store,
    now_ms: u64,
) -> Result<RespFrame, CommandError> {
    if argv.len() != 4 {
        return Err(CommandError::WrongArity("ZRANGEBYLEX"));
    }
    let members = store.zrangebylex(&argv[1], &argv[2], &argv[3], now_ms)?;
    let frames = members
        .into_iter()
        .map(|m| RespFrame::BulkString(Some(m)))
        .collect();
    Ok(RespFrame::Array(Some(frames)))
}

fn zrevrangebylex(
    argv: &[Vec<u8>],
    store: &mut Store,
    now_ms: u64,
) -> Result<RespFrame, CommandError> {
    if argv.len() != 4 {
        return Err(CommandError::WrongArity("ZREVRANGEBYLEX"));
    }
    // ZREVRANGEBYLEX key max min (note: max before min, reversed from ZRANGEBYLEX)
    let members = store.zrevrangebylex(&argv[1], &argv[2], &argv[3], now_ms)?;
    let frames = members
        .into_iter()
        .map(|m| RespFrame::BulkString(Some(m)))
        .collect();
    Ok(RespFrame::Array(Some(frames)))
}

fn zlexcount(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 4 {
        return Err(CommandError::WrongArity("ZLEXCOUNT"));
    }
    let count = store.zlexcount(&argv[1], &argv[2], &argv[3], now_ms)?;
    Ok(RespFrame::Integer(i64::try_from(count).unwrap_or(i64::MAX)))
}

// ── HyperLogLog command handlers ──────────────────────────────────────

fn pfadd(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() < 2 {
        return Err(CommandError::WrongArity("PFADD"));
    }
    let elements: Vec<Vec<u8>> = argv[2..].to_vec();
    let modified = store.pfadd(&argv[1], &elements, now_ms)?;
    Ok(RespFrame::Integer(i64::from(modified)))
}

fn pfcount(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() < 2 {
        return Err(CommandError::WrongArity("PFCOUNT"));
    }
    let keys: Vec<&[u8]> = argv[1..].iter().map(|k| k.as_slice()).collect();
    let count = store.pfcount(&keys, now_ms)?;
    Ok(RespFrame::Integer(i64::try_from(count).unwrap_or(i64::MAX)))
}

fn pfmerge(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() < 2 {
        return Err(CommandError::WrongArity("PFMERGE"));
    }
    let sources: Vec<&[u8]> = argv[2..].iter().map(|k| k.as_slice()).collect();
    store.pfmerge(&argv[1], &sources, now_ms)?;
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

fn getex(
    argv: &[Vec<u8>],
    store: &mut Store,
    now_ms: u64,
) -> Result<RespFrame, CommandError> {
    if argv.len() < 2 {
        return Err(CommandError::WrongArity("GETEX"));
    }
    let key = &argv[1];

    // Parse expiration options
    let new_expires: Option<Option<u64>> = if argv.len() == 2 {
        None // No expiration change
    } else {
        let opt = std::str::from_utf8(&argv[2]).map_err(|_| CommandError::SyntaxError)?;
        if opt.eq_ignore_ascii_case("EX") {
            if argv.len() != 4 {
                return Err(CommandError::SyntaxError);
            }
            let secs = std::str::from_utf8(&argv[3])
                .map_err(|_| CommandError::InvalidInteger)?
                .parse::<u64>()
                .map_err(|_| CommandError::InvalidInteger)?;
            Some(Some(now_ms.saturating_add(secs * 1000)))
        } else if opt.eq_ignore_ascii_case("PX") {
            if argv.len() != 4 {
                return Err(CommandError::SyntaxError);
            }
            let ms = std::str::from_utf8(&argv[3])
                .map_err(|_| CommandError::InvalidInteger)?
                .parse::<u64>()
                .map_err(|_| CommandError::InvalidInteger)?;
            Some(Some(now_ms.saturating_add(ms)))
        } else if opt.eq_ignore_ascii_case("EXAT") {
            if argv.len() != 4 {
                return Err(CommandError::SyntaxError);
            }
            let ts = std::str::from_utf8(&argv[3])
                .map_err(|_| CommandError::InvalidInteger)?
                .parse::<u64>()
                .map_err(|_| CommandError::InvalidInteger)?;
            Some(Some(ts * 1000))
        } else if opt.eq_ignore_ascii_case("PXAT") {
            if argv.len() != 4 {
                return Err(CommandError::SyntaxError);
            }
            let ts_ms = std::str::from_utf8(&argv[3])
                .map_err(|_| CommandError::InvalidInteger)?
                .parse::<u64>()
                .map_err(|_| CommandError::InvalidInteger)?;
            Some(Some(ts_ms))
        } else if opt.eq_ignore_ascii_case("PERSIST") {
            Some(None)
        } else {
            return Err(CommandError::SyntaxError);
        }
    };

    match store.getex(key, new_expires, now_ms)? {
        Some(v) => Ok(RespFrame::BulkString(Some(v))),
        None => Ok(RespFrame::BulkString(None)),
    }
}

fn smismember(
    argv: &[Vec<u8>],
    store: &mut Store,
    now_ms: u64,
) -> Result<RespFrame, CommandError> {
    if argv.len() < 3 {
        return Err(CommandError::WrongArity("SMISMEMBER"));
    }
    let key = &argv[1];
    let members: Vec<&[u8]> = argv[2..].iter().map(|v| v.as_slice()).collect();
    let results = store
        .smismember(key, &members, now_ms)
        .map_err(CommandError::Store)?;
    let frames: Vec<RespFrame> = results
        .into_iter()
        .map(|b| RespFrame::Integer(i64::from(b)))
        .collect();
    Ok(RespFrame::Array(Some(frames)))
}

fn bitop(
    argv: &[Vec<u8>],
    store: &mut Store,
    now_ms: u64,
) -> Result<RespFrame, CommandError> {
    if argv.len() < 4 {
        return Err(CommandError::WrongArity("BITOP"));
    }
    let op = &argv[1];
    let dest = &argv[2];
    let keys: Vec<&[u8]> = argv[3..].iter().map(|v| v.as_slice()).collect();
    let len = store
        .bitop(op, dest, &keys, now_ms)
        .map_err(CommandError::Store)?;
    Ok(RespFrame::Integer(i64::try_from(len).unwrap_or(i64::MAX)))
}

fn parse_zstore_args(argv: &[Vec<u8>], start: usize) -> (Vec<f64>, Vec<u8>) {
    let mut weights: Vec<f64> = Vec::new();
    let mut aggregate: Vec<u8> = b"SUM".to_vec();
    let mut i = start;
    while i < argv.len() {
        let kw = std::str::from_utf8(&argv[i]).unwrap_or("");
        if kw.eq_ignore_ascii_case("WEIGHTS") {
            i += 1;
            while i < argv.len() {
                if let Ok(w) = std::str::from_utf8(&argv[i])
                    .ok()
                    .and_then(|s| s.parse::<f64>().ok())
                    .ok_or(())
                {
                    weights.push(w);
                    i += 1;
                } else {
                    break;
                }
            }
        } else if kw.eq_ignore_ascii_case("AGGREGATE") {
            i += 1;
            if i < argv.len() {
                aggregate = argv[i].clone();
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    (weights, aggregate)
}

fn zunionstore(
    argv: &[Vec<u8>],
    store: &mut Store,
    now_ms: u64,
) -> Result<RespFrame, CommandError> {
    if argv.len() < 4 {
        return Err(CommandError::WrongArity("ZUNIONSTORE"));
    }
    let dest = &argv[1];
    let numkeys = std::str::from_utf8(&argv[2])
        .map_err(|_| CommandError::InvalidInteger)?
        .parse::<usize>()
        .map_err(|_| CommandError::InvalidInteger)?;
    if argv.len() < 3 + numkeys {
        return Err(CommandError::SyntaxError);
    }
    let keys: Vec<&[u8]> = argv[3..3 + numkeys]
        .iter()
        .map(|v| v.as_slice())
        .collect();
    let (weights, aggregate) = parse_zstore_args(argv, 3 + numkeys);
    let count = store
        .zunionstore(dest, &keys, &weights, &aggregate, now_ms)
        .map_err(CommandError::Store)?;
    Ok(RespFrame::Integer(i64::try_from(count).unwrap_or(i64::MAX)))
}

fn zinterstore(
    argv: &[Vec<u8>],
    store: &mut Store,
    now_ms: u64,
) -> Result<RespFrame, CommandError> {
    if argv.len() < 4 {
        return Err(CommandError::WrongArity("ZINTERSTORE"));
    }
    let dest = &argv[1];
    let numkeys = std::str::from_utf8(&argv[2])
        .map_err(|_| CommandError::InvalidInteger)?
        .parse::<usize>()
        .map_err(|_| CommandError::InvalidInteger)?;
    if argv.len() < 3 + numkeys {
        return Err(CommandError::SyntaxError);
    }
    let keys: Vec<&[u8]> = argv[3..3 + numkeys]
        .iter()
        .map(|v| v.as_slice())
        .collect();
    let (weights, aggregate) = parse_zstore_args(argv, 3 + numkeys);
    let count = store
        .zinterstore(dest, &keys, &weights, &aggregate, now_ms)
        .map_err(CommandError::Store)?;
    Ok(RespFrame::Integer(i64::try_from(count).unwrap_or(i64::MAX)))
}

// ── Server / connection commands ──────────────────────────────────────

fn quit(argv: &[Vec<u8>]) -> Result<RespFrame, CommandError> {
    if argv.len() != 1 {
        return Err(CommandError::WrongArity("QUIT"));
    }
    Ok(RespFrame::SimpleString("OK".to_string()))
}

fn select(argv: &[Vec<u8>]) -> Result<RespFrame, CommandError> {
    if argv.len() != 2 {
        return Err(CommandError::WrongArity("SELECT"));
    }
    let db = std::str::from_utf8(&argv[1])
        .map_err(|_| CommandError::InvalidInteger)?
        .parse::<i64>()
        .map_err(|_| CommandError::InvalidInteger)?;
    if db == 0 {
        Ok(RespFrame::SimpleString("OK".to_string()))
    } else {
        Ok(RespFrame::Error(
            "ERR DB index is out of range".to_string(),
        ))
    }
}

fn info(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() > 2 {
        // Redis allows INFO [section] but we accept 0 or 1 extra args
    }
    let keyspace_size = store.dbsize(now_ms);
    let section = if argv.len() >= 2 {
        std::str::from_utf8(&argv[1]).unwrap_or("all")
    } else {
        "all"
    };

    let mut info = String::new();
    // Server section
    if section == "all" || section.eq_ignore_ascii_case("server") {
        info.push_str("# Server\r\n");
        info.push_str("redis_version:7.0.0-frankenredis\r\n");
        info.push_str("redis_mode:standalone\r\n");
        info.push_str("arch_bits:64\r\n");
        info.push_str("tcp_port:6379\r\n");
        info.push_str("\r\n");
    }
    // Keyspace section
    if section == "all" || section.eq_ignore_ascii_case("keyspace") {
        info.push_str("# Keyspace\r\n");
        info.push_str(&format!("db0:keys={keyspace_size},expires=0,avg_ttl=0\r\n"));
        info.push_str("\r\n");
    }
    // Memory section
    if section == "all" || section.eq_ignore_ascii_case("memory") {
        info.push_str("# Memory\r\n");
        info.push_str("used_memory:0\r\n");
        info.push_str("used_memory_human:0B\r\n");
        info.push_str("\r\n");
    }
    // Replication section
    if section == "all" || section.eq_ignore_ascii_case("replication") {
        info.push_str("# Replication\r\n");
        info.push_str("role:master\r\n");
        info.push_str("connected_slaves:0\r\n");
        info.push_str("\r\n");
    }

    Ok(RespFrame::BulkString(Some(info.into_bytes())))
}

fn command_cmd(argv: &[Vec<u8>]) -> Result<RespFrame, CommandError> {
    // COMMAND, COMMAND COUNT, COMMAND DOCS, etc.
    if argv.len() == 1 {
        // COMMAND with no sub-command: return empty array (stub)
        return Ok(RespFrame::Array(Some(Vec::new())));
    }
    let sub = std::str::from_utf8(&argv[1]).map_err(|_| CommandError::InvalidUtf8Argument)?;
    if sub.eq_ignore_ascii_case("COUNT") {
        // Return approximate command count
        Ok(RespFrame::Integer(100))
    } else {
        // DOCS, INFO, and other subcommands return empty array
        Ok(RespFrame::Array(Some(Vec::new())))
    }
}

fn config_cmd(argv: &[Vec<u8>]) -> Result<RespFrame, CommandError> {
    if argv.len() < 2 {
        return Err(CommandError::WrongArity("CONFIG"));
    }
    let sub = std::str::from_utf8(&argv[1]).map_err(|_| CommandError::InvalidUtf8Argument)?;
    if sub.eq_ignore_ascii_case("GET") {
        if argv.len() < 3 {
            return Err(CommandError::WrongArity("CONFIG"));
        }
        // Return empty array for all CONFIG GET queries (stub)
        Ok(RespFrame::Array(Some(Vec::new())))
    } else if sub.eq_ignore_ascii_case("SET")
        || sub.eq_ignore_ascii_case("RESETSTAT")
        || sub.eq_ignore_ascii_case("REWRITE")
    {
        Ok(RespFrame::SimpleString("OK".to_string()))
    } else {
        Ok(RespFrame::Error(format!(
            "ERR Unknown subcommand or wrong number of arguments for CONFIG {sub}"
        )))
    }
}

fn client_cmd(argv: &[Vec<u8>]) -> Result<RespFrame, CommandError> {
    if argv.len() < 2 {
        return Err(CommandError::WrongArity("CLIENT"));
    }
    let sub = std::str::from_utf8(&argv[1]).map_err(|_| CommandError::InvalidUtf8Argument)?;
    if sub.eq_ignore_ascii_case("SETNAME") {
        Ok(RespFrame::SimpleString("OK".to_string()))
    } else if sub.eq_ignore_ascii_case("GETNAME") {
        Ok(RespFrame::BulkString(None))
    } else if sub.eq_ignore_ascii_case("ID") {
        Ok(RespFrame::Integer(1))
    } else if sub.eq_ignore_ascii_case("LIST") || sub.eq_ignore_ascii_case("INFO") {
        Ok(RespFrame::BulkString(Some(
            b"id=1 addr=127.0.0.1:0 fd=0 name= db=0\r\n".to_vec(),
        )))
    } else if sub.eq_ignore_ascii_case("NO-EVICT")
        || sub.eq_ignore_ascii_case("NO-TOUCH")
        || sub.eq_ignore_ascii_case("REPLY")
    {
        Ok(RespFrame::SimpleString("OK".to_string()))
    } else {
        Ok(RespFrame::Error(format!(
            "ERR Unknown subcommand or wrong number of arguments for CLIENT {sub}"
        )))
    }
}

fn time_cmd(argv: &[Vec<u8>], now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 1 {
        return Err(CommandError::WrongArity("TIME"));
    }
    let secs = now_ms / 1000;
    let usecs = (now_ms % 1000) * 1000;
    Ok(RespFrame::Array(Some(vec![
        RespFrame::BulkString(Some(secs.to_string().into_bytes())),
        RespFrame::BulkString(Some(usecs.to_string().into_bytes())),
    ])))
}

fn randomkey(
    argv: &[Vec<u8>],
    store: &mut Store,
    now_ms: u64,
) -> Result<RespFrame, CommandError> {
    if argv.len() != 1 {
        return Err(CommandError::WrongArity("RANDOMKEY"));
    }
    match store.randomkey(now_ms) {
        Some(key) => Ok(RespFrame::BulkString(Some(key))),
        None => Ok(RespFrame::BulkString(None)),
    }
}

// ── SCAN family ──────────────────────────────────────────────────────

fn parse_scan_args(argv: &[Vec<u8>], start_idx: usize) -> (Option<Vec<u8>>, usize) {
    let mut pattern: Option<Vec<u8>> = None;
    let mut count: usize = 10;
    let mut i = start_idx;
    while i + 1 < argv.len() {
        let kw = std::str::from_utf8(&argv[i]).unwrap_or("");
        if kw.eq_ignore_ascii_case("MATCH") {
            pattern = Some(argv[i + 1].clone());
            i += 2;
        } else if kw.eq_ignore_ascii_case("COUNT") {
            count = std::str::from_utf8(&argv[i + 1])
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(10);
            i += 2;
        } else {
            i += 1;
        }
    }
    (pattern, count)
}

fn scan(
    argv: &[Vec<u8>],
    store: &mut Store,
    now_ms: u64,
) -> Result<RespFrame, CommandError> {
    if argv.len() < 2 {
        return Err(CommandError::WrongArity("SCAN"));
    }
    let cursor = std::str::from_utf8(&argv[1])
        .map_err(|_| CommandError::InvalidInteger)?
        .parse::<u64>()
        .map_err(|_| CommandError::InvalidInteger)?;

    let (pattern, count) = parse_scan_args(argv, 2);
    let (next_cursor, keys) =
        store.scan(cursor, pattern.as_deref(), count, now_ms);

    let key_frames: Vec<RespFrame> = keys
        .into_iter()
        .map(|k| RespFrame::BulkString(Some(k)))
        .collect();
    Ok(RespFrame::Array(Some(vec![
        RespFrame::BulkString(Some(next_cursor.to_string().into_bytes())),
        RespFrame::Array(Some(key_frames)),
    ])))
}

fn hscan(
    argv: &[Vec<u8>],
    store: &mut Store,
    now_ms: u64,
) -> Result<RespFrame, CommandError> {
    if argv.len() < 3 {
        return Err(CommandError::WrongArity("HSCAN"));
    }
    let key = &argv[1];
    let cursor = std::str::from_utf8(&argv[2])
        .map_err(|_| CommandError::InvalidInteger)?
        .parse::<u64>()
        .map_err(|_| CommandError::InvalidInteger)?;

    let (pattern, count) = parse_scan_args(argv, 3);
    let (next_cursor, pairs) = store
        .hscan(key, cursor, pattern.as_deref(), count, now_ms)
        .map_err(CommandError::Store)?;

    let mut items = Vec::with_capacity(pairs.len() * 2);
    for (field, value) in pairs {
        items.push(RespFrame::BulkString(Some(field)));
        items.push(RespFrame::BulkString(Some(value)));
    }
    Ok(RespFrame::Array(Some(vec![
        RespFrame::BulkString(Some(next_cursor.to_string().into_bytes())),
        RespFrame::Array(Some(items)),
    ])))
}

fn sscan(
    argv: &[Vec<u8>],
    store: &mut Store,
    now_ms: u64,
) -> Result<RespFrame, CommandError> {
    if argv.len() < 3 {
        return Err(CommandError::WrongArity("SSCAN"));
    }
    let key = &argv[1];
    let cursor = std::str::from_utf8(&argv[2])
        .map_err(|_| CommandError::InvalidInteger)?
        .parse::<u64>()
        .map_err(|_| CommandError::InvalidInteger)?;

    let (pattern, count) = parse_scan_args(argv, 3);
    let (next_cursor, members) = store
        .sscan(key, cursor, pattern.as_deref(), count, now_ms)
        .map_err(CommandError::Store)?;

    let member_frames: Vec<RespFrame> = members
        .into_iter()
        .map(|m| RespFrame::BulkString(Some(m)))
        .collect();
    Ok(RespFrame::Array(Some(vec![
        RespFrame::BulkString(Some(next_cursor.to_string().into_bytes())),
        RespFrame::Array(Some(member_frames)),
    ])))
}

fn zscan(
    argv: &[Vec<u8>],
    store: &mut Store,
    now_ms: u64,
) -> Result<RespFrame, CommandError> {
    if argv.len() < 3 {
        return Err(CommandError::WrongArity("ZSCAN"));
    }
    let key = &argv[1];
    let cursor = std::str::from_utf8(&argv[2])
        .map_err(|_| CommandError::InvalidInteger)?
        .parse::<u64>()
        .map_err(|_| CommandError::InvalidInteger)?;

    let (pattern, count) = parse_scan_args(argv, 3);
    let (next_cursor, pairs) = store
        .zscan(key, cursor, pattern.as_deref(), count, now_ms)
        .map_err(CommandError::Store)?;

    let mut items = Vec::with_capacity(pairs.len() * 2);
    for (member, score) in pairs {
        items.push(RespFrame::BulkString(Some(member)));
        items.push(RespFrame::BulkString(Some(
            score.to_string().into_bytes(),
        )));
    }
    Ok(RespFrame::Array(Some(vec![
        RespFrame::BulkString(Some(next_cursor.to_string().into_bytes())),
        RespFrame::Array(Some(items)),
    ])))
}

fn object_cmd(argv: &[Vec<u8>]) -> Result<RespFrame, CommandError> {
    if argv.len() < 2 {
        return Err(CommandError::WrongArity("OBJECT"));
    }
    let sub = std::str::from_utf8(&argv[1]).map_err(|_| CommandError::InvalidUtf8Argument)?;
    if sub.eq_ignore_ascii_case("ENCODING") {
        if argv.len() < 3 {
            return Err(CommandError::WrongArity("OBJECT"));
        }
        // Stub: always return "raw" encoding
        Ok(RespFrame::BulkString(Some(b"raw".to_vec())))
    } else if sub.eq_ignore_ascii_case("REFCOUNT") {
        Ok(RespFrame::Integer(1))
    } else if sub.eq_ignore_ascii_case("IDLETIME") || sub.eq_ignore_ascii_case("FREQ") {
        Ok(RespFrame::Integer(0))
    } else if sub.eq_ignore_ascii_case("HELP") {
        Ok(RespFrame::Array(Some(Vec::new())))
    } else {
        Ok(RespFrame::Error(format!(
            "ERR Unknown subcommand or wrong number of arguments for OBJECT {sub}"
        )))
    }
}

fn wait_cmd(argv: &[Vec<u8>]) -> Result<RespFrame, CommandError> {
    if argv.len() < 3 {
        return Err(CommandError::WrongArity("WAIT"));
    }
    // WAIT numreplicas timeout - in standalone mode, return 0 replicas
    Ok(RespFrame::Integer(0))
}

fn reset_cmd(argv: &[Vec<u8>]) -> Result<RespFrame, CommandError> {
    if argv.len() != 1 {
        return Err(CommandError::WrongArity("RESET"));
    }
    Ok(RespFrame::SimpleString("RESET".to_string()))
}

fn touch(
    argv: &[Vec<u8>],
    store: &mut Store,
    now_ms: u64,
) -> Result<RespFrame, CommandError> {
    if argv.len() < 2 {
        return Err(CommandError::WrongArity("TOUCH"));
    }
    let keys: Vec<&[u8]> = argv[1..].iter().map(|v| v.as_slice()).collect();
    let count = store.touch(&keys, now_ms);
    Ok(RespFrame::Integer(count))
}

fn dump_cmd(argv: &[Vec<u8>]) -> Result<RespFrame, CommandError> {
    if argv.len() != 2 {
        return Err(CommandError::WrongArity("DUMP"));
    }
    // Stub: DUMP is not fully supported
    Ok(RespFrame::BulkString(None))
}

fn restore_cmd(argv: &[Vec<u8>]) -> Result<RespFrame, CommandError> {
    if argv.len() < 4 {
        return Err(CommandError::WrongArity("RESTORE"));
    }
    // Stub: RESTORE is not fully supported
    Ok(RespFrame::Error(
        "ERR DUMP/RESTORE serialization not supported".to_string(),
    ))
}

fn sort_cmd(argv: &[Vec<u8>]) -> Result<RespFrame, CommandError> {
    if argv.len() < 2 {
        return Err(CommandError::WrongArity("SORT"));
    }
    // Stub: basic SORT returning empty array
    Ok(RespFrame::Array(Some(Vec::new())))
}

fn copy_cmd(
    argv: &[Vec<u8>],
    store: &mut Store,
    now_ms: u64,
) -> Result<RespFrame, CommandError> {
    if argv.len() < 3 {
        return Err(CommandError::WrongArity("COPY"));
    }
    let source = &argv[1];
    let destination = &argv[2];

    // Parse optional REPLACE flag
    let mut replace = false;
    let mut i = 3;
    while i < argv.len() {
        let arg = std::str::from_utf8(&argv[i]).unwrap_or("");
        if arg.eq_ignore_ascii_case("REPLACE") {
            replace = true;
        } else if arg.eq_ignore_ascii_case("DESTINATION") {
            // COPY source destination DESTINATION db - ignore DB (single-db mode)
            i += 1;
        }
        i += 1;
    }

    let copied = store
        .copy(source, destination, replace, now_ms)
        .map_err(CommandError::Store)?;
    Ok(RespFrame::Integer(i64::from(copied)))
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use fr_protocol::RespFrame;
    use fr_store::Store;

    use super::{
        CommandError, CommandId, classify_command, dispatch_argv, eq_ascii_command, frame_to_argv,
    };

    fn classify_command_linear(cmd: &[u8]) -> Option<CommandId> {
        if eq_ascii_command(cmd, b"PING") {
            return Some(CommandId::Ping);
        }
        if eq_ascii_command(cmd, b"ECHO") {
            return Some(CommandId::Echo);
        }
        if eq_ascii_command(cmd, b"SET") {
            return Some(CommandId::Set);
        }
        if eq_ascii_command(cmd, b"GET") {
            return Some(CommandId::Get);
        }
        if eq_ascii_command(cmd, b"DEL") {
            return Some(CommandId::Del);
        }
        if eq_ascii_command(cmd, b"INCR") {
            return Some(CommandId::Incr);
        }
        if eq_ascii_command(cmd, b"EXPIRE") {
            return Some(CommandId::Expire);
        }
        if eq_ascii_command(cmd, b"PEXPIRE") {
            return Some(CommandId::Pexpire);
        }
        if eq_ascii_command(cmd, b"EXPIREAT") {
            return Some(CommandId::Expireat);
        }
        if eq_ascii_command(cmd, b"PEXPIREAT") {
            return Some(CommandId::Pexpireat);
        }
        if eq_ascii_command(cmd, b"PTTL") {
            return Some(CommandId::Pttl);
        }
        if eq_ascii_command(cmd, b"APPEND") {
            return Some(CommandId::Append);
        }
        if eq_ascii_command(cmd, b"STRLEN") {
            return Some(CommandId::Strlen);
        }
        if eq_ascii_command(cmd, b"MGET") {
            return Some(CommandId::Mget);
        }
        if eq_ascii_command(cmd, b"MSET") {
            return Some(CommandId::Mset);
        }
        if eq_ascii_command(cmd, b"SETNX") {
            return Some(CommandId::Setnx);
        }
        if eq_ascii_command(cmd, b"GETSET") {
            return Some(CommandId::Getset);
        }
        if eq_ascii_command(cmd, b"INCRBY") {
            return Some(CommandId::Incrby);
        }
        if eq_ascii_command(cmd, b"DECRBY") {
            return Some(CommandId::Decrby);
        }
        if eq_ascii_command(cmd, b"DECR") {
            return Some(CommandId::Decr);
        }
        if eq_ascii_command(cmd, b"EXISTS") {
            return Some(CommandId::Exists);
        }
        if eq_ascii_command(cmd, b"TTL") {
            return Some(CommandId::Ttl);
        }
        if eq_ascii_command(cmd, b"EXPIRETIME") {
            return Some(CommandId::Expiretime);
        }
        if eq_ascii_command(cmd, b"PEXPIRETIME") {
            return Some(CommandId::Pexpiretime);
        }
        if eq_ascii_command(cmd, b"PERSIST") {
            return Some(CommandId::Persist);
        }
        if eq_ascii_command(cmd, b"TYPE") {
            return Some(CommandId::Type);
        }
        if eq_ascii_command(cmd, b"RENAME") {
            return Some(CommandId::Rename);
        }
        if eq_ascii_command(cmd, b"RENAMENX") {
            return Some(CommandId::Renamenx);
        }
        if eq_ascii_command(cmd, b"KEYS") {
            return Some(CommandId::Keys);
        }
        if eq_ascii_command(cmd, b"DBSIZE") {
            return Some(CommandId::Dbsize);
        }
        if eq_ascii_command(cmd, b"FLUSHDB") || eq_ascii_command(cmd, b"FLUSHALL") {
            return Some(CommandId::Flushdb);
        }
        if eq_ascii_command(cmd, b"HSET") {
            return Some(CommandId::Hset);
        }
        if eq_ascii_command(cmd, b"HGET") {
            return Some(CommandId::Hget);
        }
        if eq_ascii_command(cmd, b"HDEL") {
            return Some(CommandId::Hdel);
        }
        if eq_ascii_command(cmd, b"HLEN") {
            return Some(CommandId::Hlen);
        }
        if eq_ascii_command(cmd, b"HKEYS") {
            return Some(CommandId::Hkeys);
        }
        if eq_ascii_command(cmd, b"HVALS") {
            return Some(CommandId::Hvals);
        }
        if eq_ascii_command(cmd, b"HMGET") {
            return Some(CommandId::Hmget);
        }
        if eq_ascii_command(cmd, b"HMSET") {
            return Some(CommandId::Hmset);
        }
        if eq_ascii_command(cmd, b"HGETALL") {
            return Some(CommandId::Hgetall);
        }
        if eq_ascii_command(cmd, b"HEXISTS") {
            return Some(CommandId::Hexists);
        }
        if eq_ascii_command(cmd, b"HINCRBY") {
            return Some(CommandId::Hincrby);
        }
        if eq_ascii_command(cmd, b"HSETNX") {
            return Some(CommandId::Hsetnx);
        }
        if eq_ascii_command(cmd, b"HSTRLEN") {
            return Some(CommandId::Hstrlen);
        }
        if eq_ascii_command(cmd, b"LPUSH") {
            return Some(CommandId::Lpush);
        }
        if eq_ascii_command(cmd, b"RPUSH") {
            return Some(CommandId::Rpush);
        }
        if eq_ascii_command(cmd, b"LPOP") {
            return Some(CommandId::Lpop);
        }
        if eq_ascii_command(cmd, b"RPOP") {
            return Some(CommandId::Rpop);
        }
        if eq_ascii_command(cmd, b"LLEN") {
            return Some(CommandId::Llen);
        }
        if eq_ascii_command(cmd, b"LRANGE") {
            return Some(CommandId::Lrange);
        }
        if eq_ascii_command(cmd, b"LINDEX") {
            return Some(CommandId::Lindex);
        }
        if eq_ascii_command(cmd, b"LSET") {
            return Some(CommandId::Lset);
        }
        if eq_ascii_command(cmd, b"SADD") {
            return Some(CommandId::Sadd);
        }
        if eq_ascii_command(cmd, b"SREM") {
            return Some(CommandId::Srem);
        }
        if eq_ascii_command(cmd, b"SMEMBERS") {
            return Some(CommandId::Smembers);
        }
        if eq_ascii_command(cmd, b"SCARD") {
            return Some(CommandId::Scard);
        }
        if eq_ascii_command(cmd, b"SISMEMBER") {
            return Some(CommandId::Sismember);
        }
        if eq_ascii_command(cmd, b"ZADD") {
            return Some(CommandId::Zadd);
        }
        if eq_ascii_command(cmd, b"ZREM") {
            return Some(CommandId::Zrem);
        }
        if eq_ascii_command(cmd, b"ZSCORE") {
            return Some(CommandId::Zscore);
        }
        if eq_ascii_command(cmd, b"ZCARD") {
            return Some(CommandId::Zcard);
        }
        if eq_ascii_command(cmd, b"ZRANK") {
            return Some(CommandId::Zrank);
        }
        if eq_ascii_command(cmd, b"ZREVRANK") {
            return Some(CommandId::Zrevrank);
        }
        if eq_ascii_command(cmd, b"ZRANGE") {
            return Some(CommandId::Zrange);
        }
        if eq_ascii_command(cmd, b"ZREVRANGE") {
            return Some(CommandId::Zrevrange);
        }
        if eq_ascii_command(cmd, b"ZRANGEBYSCORE") {
            return Some(CommandId::Zrangebyscore);
        }
        if eq_ascii_command(cmd, b"ZCOUNT") {
            return Some(CommandId::Zcount);
        }
        if eq_ascii_command(cmd, b"ZINCRBY") {
            return Some(CommandId::Zincrby);
        }
        if eq_ascii_command(cmd, b"ZPOPMIN") {
            return Some(CommandId::Zpopmin);
        }
        if eq_ascii_command(cmd, b"ZPOPMAX") {
            return Some(CommandId::Zpopmax);
        }
        if eq_ascii_command(cmd, b"SETEX") {
            return Some(CommandId::Setex);
        }
        if eq_ascii_command(cmd, b"PSETEX") {
            return Some(CommandId::Psetex);
        }
        if eq_ascii_command(cmd, b"GETDEL") {
            return Some(CommandId::Getdel);
        }
        if eq_ascii_command(cmd, b"GETRANGE") {
            return Some(CommandId::Getrange);
        }
        if eq_ascii_command(cmd, b"SETRANGE") {
            return Some(CommandId::Setrange);
        }
        if eq_ascii_command(cmd, b"INCRBYFLOAT") {
            return Some(CommandId::Incrbyfloat);
        }
        if eq_ascii_command(cmd, b"SINTER") {
            return Some(CommandId::Sinter);
        }
        if eq_ascii_command(cmd, b"SUNION") {
            return Some(CommandId::Sunion);
        }
        if eq_ascii_command(cmd, b"SDIFF") {
            return Some(CommandId::Sdiff);
        }
        if eq_ascii_command(cmd, b"SPOP") {
            return Some(CommandId::Spop);
        }
        if eq_ascii_command(cmd, b"SRANDMEMBER") {
            return Some(CommandId::Srandmember);
        }
        if eq_ascii_command(cmd, b"SETBIT") {
            return Some(CommandId::Setbit);
        }
        if eq_ascii_command(cmd, b"GETBIT") {
            return Some(CommandId::Getbit);
        }
        if eq_ascii_command(cmd, b"BITCOUNT") {
            return Some(CommandId::Bitcount);
        }
        if eq_ascii_command(cmd, b"BITPOS") {
            return Some(CommandId::Bitpos);
        }
        if eq_ascii_command(cmd, b"LPOS") {
            return Some(CommandId::Lpos);
        }
        if eq_ascii_command(cmd, b"LINSERT") {
            return Some(CommandId::Linsert);
        }
        if eq_ascii_command(cmd, b"LREM") {
            return Some(CommandId::Lrem);
        }
        if eq_ascii_command(cmd, b"RPOPLPUSH") {
            return Some(CommandId::Rpoplpush);
        }
        if eq_ascii_command(cmd, b"HINCRBYFLOAT") {
            return Some(CommandId::Hincrbyfloat);
        }
        if eq_ascii_command(cmd, b"HRANDFIELD") {
            return Some(CommandId::Hrandfield);
        }
        if eq_ascii_command(cmd, b"ZREVRANGEBYSCORE") {
            return Some(CommandId::Zrevrangebyscore);
        }
        if eq_ascii_command(cmd, b"ZRANGEBYLEX") {
            return Some(CommandId::Zrangebylex);
        }
        if eq_ascii_command(cmd, b"ZREVRANGEBYLEX") {
            return Some(CommandId::Zrevrangebylex);
        }
        if eq_ascii_command(cmd, b"ZLEXCOUNT") {
            return Some(CommandId::Zlexcount);
        }
        if eq_ascii_command(cmd, b"PFADD") {
            return Some(CommandId::Pfadd);
        }
        if eq_ascii_command(cmd, b"PFCOUNT") {
            return Some(CommandId::Pfcount);
        }
        if eq_ascii_command(cmd, b"PFMERGE") {
            return Some(CommandId::Pfmerge);
        }
        if eq_ascii_command(cmd, b"LTRIM") {
            return Some(CommandId::Ltrim);
        }
        if eq_ascii_command(cmd, b"LPUSHX") {
            return Some(CommandId::Lpushx);
        }
        if eq_ascii_command(cmd, b"RPUSHX") {
            return Some(CommandId::Rpushx);
        }
        if eq_ascii_command(cmd, b"LMOVE") {
            return Some(CommandId::Lmove);
        }
        if eq_ascii_command(cmd, b"SMOVE") {
            return Some(CommandId::Smove);
        }
        if eq_ascii_command(cmd, b"SINTERSTORE") {
            return Some(CommandId::Sinterstore);
        }
        if eq_ascii_command(cmd, b"SUNIONSTORE") {
            return Some(CommandId::Sunionstore);
        }
        if eq_ascii_command(cmd, b"SDIFFSTORE") {
            return Some(CommandId::Sdiffstore);
        }
        if eq_ascii_command(cmd, b"ZREMRANGEBYRANK") {
            return Some(CommandId::Zremrangebyrank);
        }
        if eq_ascii_command(cmd, b"ZREMRANGEBYSCORE") {
            return Some(CommandId::Zremrangebyscore);
        }
        if eq_ascii_command(cmd, b"ZREMRANGEBYLEX") {
            return Some(CommandId::Zremrangebylex);
        }
        if eq_ascii_command(cmd, b"ZRANDMEMBER") {
            return Some(CommandId::Zrandmember);
        }
        if eq_ascii_command(cmd, b"ZMSCORE") {
            return Some(CommandId::Zmscore);
        }
        None
    }

    fn classify_packet_008_dispatch_linear(cmd: &[u8]) -> Option<CommandId> {
        let text = std::str::from_utf8(cmd).ok()?;
        classify_command(text.as_bytes())
    }

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
    fn unknown_command_preview_sanitizes_and_caps_output() {
        let mut store = Store::new();
        let argv = vec![vec![b'X'; 200], b"line1\r\nline2".to_vec(), vec![b'a'; 200]];
        let err = dispatch_argv(&argv, &mut store, 0).expect_err("must fail");
        match err {
            CommandError::UnknownCommand {
                command,
                args_preview,
            } => {
                assert_eq!(command.len(), 128);
                assert!(command.chars().all(|ch| ch == 'X'));
                let preview = args_preview.expect("args preview");
                assert!(preview.len() <= 128);
                assert!(!preview.contains('\r'));
                assert!(!preview.contains('\n'));
                assert!(preview.starts_with("'line1  line2' "));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn dispatch_invalid_utf8_command_name_errors_invalid_utf8_argument() {
        let mut store = Store::new();
        let argv = vec![vec![0xFF], b"k".to_vec()];
        let err = dispatch_argv(&argv, &mut store, 0).expect_err("must fail");
        assert!(matches!(err, super::CommandError::InvalidUtf8Argument));
    }

    #[test]
    fn dispatch_empty_argv_returns_invalid_command_frame() {
        let mut store = Store::new();
        let err = dispatch_argv(&[], &mut store, 0).expect_err("must fail");
        assert!(matches!(err, CommandError::InvalidCommandFrame));
    }

    #[test]
    fn frame_to_argv_rejects_non_array_and_null_array_frames() {
        let invalid = [
            RespFrame::SimpleString("PING".to_string()),
            RespFrame::BulkString(Some(b"PING".to_vec())),
            RespFrame::Array(None),
        ];

        for frame in invalid {
            let err = frame_to_argv(&frame).expect_err("must fail");
            assert!(matches!(err, CommandError::InvalidCommandFrame));
        }
    }

    #[test]
    fn frame_to_argv_rejects_empty_or_unsupported_array_items() {
        let empty = RespFrame::Array(Some(vec![]));
        let err = frame_to_argv(&empty).expect_err("must fail");
        assert!(matches!(err, CommandError::InvalidCommandFrame));

        let invalid_items = RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"SET".to_vec())),
            RespFrame::BulkString(None),
        ]));
        let err = frame_to_argv(&invalid_items).expect_err("must fail");
        assert!(matches!(err, CommandError::InvalidCommandFrame));
    }

    #[test]
    fn classify_command_matches_linear_reference() {
        let samples: &[&[u8]] = &[
            b"PING",
            b"ping",
            b"PiNg",
            b"ECHO",
            b"SET",
            b"GET",
            b"DEL",
            b"INCR",
            b"EXPIRE",
            b"PEXPIRE",
            b"EXPIREAT",
            b"PEXPIREAT",
            b"PTTL",
            b"APPEND",
            b"STRLEN",
            b"MGET",
            b"MSET",
            b"SETNX",
            b"GETSET",
            b"INCRBY",
            b"DECRBY",
            b"DECR",
            b"EXISTS",
            b"TTL",
            b"EXPIRETIME",
            b"PEXPIRETIME",
            b"PERSIST",
            b"TYPE",
            b"RENAME",
            b"RENAMENX",
            b"KEYS",
            b"DBSIZE",
            b"FLUSHDB",
            b"flushall",
            b"UNKNOWN",
            b"POST",
            b"host:",
            b"HSET",
            b"HGET",
            b"HDEL",
            b"HLEN",
            b"HKEYS",
            b"HVALS",
            b"HMGET",
            b"HMSET",
            b"HGETALL",
            b"HEXISTS",
            b"HINCRBY",
            b"HSETNX",
            b"HSTRLEN",
            b"hset",
            b"hGetAll",
            b"LPUSH",
            b"RPUSH",
            b"LPOP",
            b"RPOP",
            b"LLEN",
            b"LRANGE",
            b"LINDEX",
            b"LSET",
            b"SADD",
            b"SREM",
            b"SMEMBERS",
            b"SCARD",
            b"SISMEMBER",
            b"lpush",
            b"sIsMember",
            b"ZADD",
            b"ZREM",
            b"ZSCORE",
            b"ZCARD",
            b"ZRANK",
            b"ZREVRANK",
            b"ZRANGE",
            b"ZREVRANGE",
            b"ZRANGEBYSCORE",
            b"ZCOUNT",
            b"ZINCRBY",
            b"ZPOPMIN",
            b"ZPOPMAX",
            b"zadd",
            b"zRangeByScore",
            b"SETEX",
            b"PSETEX",
            b"GETDEL",
            b"GETRANGE",
            b"SETRANGE",
            b"INCRBYFLOAT",
            b"setex",
            b"getRange",
            b"SINTER",
            b"SUNION",
            b"SDIFF",
            b"SPOP",
            b"SRANDMEMBER",
            b"sinter",
            b"sDiff",
            b"SETBIT",
            b"GETBIT",
            b"BITCOUNT",
            b"BITPOS",
            b"setBit",
            b"LPOS",
            b"LINSERT",
            b"LREM",
            b"RPOPLPUSH",
            b"linsert",
            b"HINCRBYFLOAT",
            b"HRANDFIELD",
            b"ZREVRANGEBYSCORE",
            b"ZRANGEBYLEX",
            b"ZLEXCOUNT",
            b"hIncRByFloat",
            b"PFADD",
            b"pfadd",
            b"PFCOUNT",
            b"pfcount",
            b"PFMERGE",
            b"pfmerge",
        ];
        for sample in samples {
            let optimized = classify_command(sample);
            let linear = classify_command_linear(sample);
            assert_eq!(
                optimized,
                linear,
                "lookup mismatch for {:?}",
                String::from_utf8_lossy(sample)
            );
        }
    }

    #[test]
    fn fr_p2c_008_dispatch_lookup_matches_linear_utf8_gate() {
        let samples: &[&[u8]] = &[
            b"EXPIRE",
            b"PEXPIRE",
            b"EXPIREAT",
            b"PEXPIREAT",
            b"TTL",
            b"PTTL",
            b"EXPIRETIME",
            b"PEXPIRETIME",
            b"PERSIST",
            b"set",
            b"get",
            b"DeL",
            b"UNKNOWN",
            b"host:",
            &[0xFF, 0xFE],
            &[0xC3, 0x91, b'X'],
        ];

        for sample in samples {
            let optimized = classify_command(sample);
            let linear = classify_packet_008_dispatch_linear(sample);
            assert_eq!(
                optimized,
                linear,
                "dispatch lookup mismatch for {:?}",
                String::from_utf8_lossy(sample)
            );
        }
    }

    #[test]
    #[ignore = "profiling helper for FR-P2C-008-H"]
    fn fr_p2c_008_dispatch_lookup_profile_snapshot() {
        let workload: &[&[u8]] = &[
            b"EXPIRE",
            b"PEXPIRE",
            b"EXPIREAT",
            b"PEXPIREAT",
            b"TTL",
            b"PTTL",
            b"EXPIRETIME",
            b"PEXPIRETIME",
            b"PERSIST",
            b"SET",
            b"GET",
            b"DEL",
            b"EXISTS",
            b"UNKNOWN",
            b"host:",
            &[0xFF, 0xFE],
            &[0xC3, 0x91, b'X'],
        ];

        let rounds = 300_000usize;
        let total_lookups = rounds.saturating_mul(workload.len());

        let mut linear_hits = 0usize;
        let linear_start = Instant::now();
        for _ in 0..rounds {
            for sample in workload {
                if classify_packet_008_dispatch_linear(sample).is_some() {
                    linear_hits = linear_hits.saturating_add(1);
                }
            }
        }
        let linear_ns = linear_start.elapsed().as_nanos();

        let mut optimized_hits = 0usize;
        let optimized_start = Instant::now();
        for _ in 0..rounds {
            for sample in workload {
                if classify_command(sample).is_some() {
                    optimized_hits = optimized_hits.saturating_add(1);
                }
            }
        }
        let optimized_ns = optimized_start.elapsed().as_nanos();

        assert_eq!(linear_hits, optimized_hits);
        assert!(total_lookups > 0);

        let linear_ns_per_lookup = linear_ns as f64 / total_lookups as f64;
        let optimized_ns_per_lookup = optimized_ns as f64 / total_lookups as f64;
        let speedup_ratio = if optimized_ns > 0 {
            linear_ns as f64 / optimized_ns as f64
        } else {
            0.0
        };

        println!("profile.packet_id=FR-P2C-008");
        println!("profile.benchmark=dispatch_utf8_gate");
        println!("profile.total_lookups={total_lookups}");
        println!("profile.linear_total_ns={linear_ns}");
        println!("profile.optimized_total_ns={optimized_ns}");
        println!("profile.linear_ns_per_lookup={linear_ns_per_lookup:.6}");
        println!("profile.optimized_ns_per_lookup={optimized_ns_per_lookup:.6}");
        println!("profile.speedup_ratio={speedup_ratio:.6}");
        println!("profile.linear_hits={linear_hits}");
        println!("profile.optimized_hits={optimized_hits}");
    }

    #[test]
    #[ignore = "profiling helper for FR-P2C-003-H"]
    fn fr_p2c_003_dispatch_lookup_profile_snapshot() {
        let workload: &[&[u8]] = &[
            b"PING",
            b"ECHO",
            b"SET",
            b"GET",
            b"DEL",
            b"INCR",
            b"EXPIRE",
            b"PEXPIRE",
            b"EXPIREAT",
            b"PEXPIREAT",
            b"PTTL",
            b"APPEND",
            b"STRLEN",
            b"MGET",
            b"MSET",
            b"SETNX",
            b"GETSET",
            b"INCRBY",
            b"DECRBY",
            b"DECR",
            b"EXISTS",
            b"TTL",
            b"EXPIRETIME",
            b"PEXPIRETIME",
            b"PERSIST",
            b"TYPE",
            b"RENAME",
            b"RENAMENX",
            b"KEYS",
            b"DBSIZE",
            b"FLUSHDB",
            b"FLUSHALL",
            b"UNKNOWN",
            b"NOPE",
            b"host:",
            b"post",
        ];

        let rounds = 200_000usize;
        let total_lookups = rounds.saturating_mul(workload.len());

        let mut linear_hits = 0usize;
        let linear_start = Instant::now();
        for _ in 0..rounds {
            for cmd in workload {
                if classify_command_linear(cmd).is_some() {
                    linear_hits = linear_hits.saturating_add(1);
                }
            }
        }
        let linear_ns = linear_start.elapsed().as_nanos();

        let mut optimized_hits = 0usize;
        let optimized_start = Instant::now();
        for _ in 0..rounds {
            for cmd in workload {
                if classify_command(cmd).is_some() {
                    optimized_hits = optimized_hits.saturating_add(1);
                }
            }
        }
        let optimized_ns = optimized_start.elapsed().as_nanos();

        assert_eq!(linear_hits, optimized_hits);
        assert!(total_lookups > 0);

        let linear_ns_per_lookup = linear_ns as f64 / total_lookups as f64;
        let optimized_ns_per_lookup = optimized_ns as f64 / total_lookups as f64;
        let speedup_ratio = if optimized_ns > 0 {
            linear_ns as f64 / optimized_ns as f64
        } else {
            0.0
        };

        println!("profile.packet_id=FR-P2C-003");
        println!("profile.benchmark=dispatch_lookup_classifier");
        println!("profile.total_lookups={total_lookups}");
        println!("profile.linear_total_ns={linear_ns}");
        println!("profile.optimized_total_ns={optimized_ns}");
        println!("profile.linear_ns_per_lookup={linear_ns_per_lookup:.6}");
        println!("profile.optimized_ns_per_lookup={optimized_ns_per_lookup:.6}");
        println!("profile.speedup_ratio={speedup_ratio:.6}");
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
    fn set_with_px_missing_ttl_returns_syntax_error() {
        let mut store = Store::new();
        let argv = vec![
            b"SET".to_vec(),
            b"k".to_vec(),
            b"v".to_vec(),
            b"PX".to_vec(),
        ];
        let err = dispatch_argv(&argv, &mut store, 0).expect_err("set should fail");
        assert!(matches!(err, CommandError::SyntaxError));
    }

    #[test]
    fn set_with_ex_missing_seconds_returns_syntax_error() {
        let mut store = Store::new();
        let argv = vec![
            b"SET".to_vec(),
            b"k".to_vec(),
            b"v".to_vec(),
            b"EX".to_vec(),
        ];
        let err = dispatch_argv(&argv, &mut store, 0).expect_err("set should fail");
        assert!(matches!(err, CommandError::SyntaxError));
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
    fn set_with_mixed_xx_get_px_options_returns_old_and_sets_ttl() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), b"old".to_vec(), None, 1_000);

        let argv = vec![
            b"SET".to_vec(),
            b"k".to_vec(),
            b"new".to_vec(),
            b"XX".to_vec(),
            b"GET".to_vec(),
            b"PX".to_vec(),
            b"500".to_vec(),
        ];
        let out = dispatch_argv(&argv, &mut store, 1_000).expect("set XX GET PX");
        assert_eq!(out, RespFrame::BulkString(Some(b"old".to_vec())));

        let get = vec![b"GET".to_vec(), b"k".to_vec()];
        let val = dispatch_argv(&get, &mut store, 1_000).expect("get");
        assert_eq!(val, RespFrame::BulkString(Some(b"new".to_vec())));

        let pttl = vec![b"PTTL".to_vec(), b"k".to_vec()];
        let ttl_out = dispatch_argv(&pttl, &mut store, 1_000).expect("pttl");
        assert_eq!(ttl_out, RespFrame::Integer(500));
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
        assert_eq!(store.get(b"a", 0).unwrap(), Some(b"1".to_vec()));
        assert_eq!(store.get(b"b", 0).unwrap(), Some(b"2".to_vec()));
    }

    #[test]
    fn mset_odd_arg_count_errors_wrong_arity() {
        let mut store = Store::new();
        store.set(b"sentinel".to_vec(), b"keep".to_vec(), None, 0);
        let argv = vec![
            b"MSET".to_vec(),
            b"a".to_vec(),
            b"1".to_vec(),
            b"b".to_vec(),
        ];
        let err = dispatch_argv(&argv, &mut store, 0).expect_err("must fail");
        assert!(matches!(err, super::CommandError::WrongArity("MSET")));
        assert_eq!(store.get(b"sentinel", 0).unwrap(), Some(b"keep".to_vec()));
        assert_eq!(store.get(b"a", 0).unwrap(), None);
        assert_eq!(store.get(b"b", 0).unwrap(), None);
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
        assert_eq!(store.get(b"new", 0).unwrap(), Some(b"v".to_vec()));
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
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"hallo".to_vec())),
                RespFrame::BulkString(Some(b"hello".to_vec())),
            ]))
        );
    }

    #[test]
    fn keys_command_glob_class_edge_semantics() {
        let mut store = Store::new();
        store.set(b"!".to_vec(), b"0".to_vec(), None, 0);
        store.set(b"a".to_vec(), b"1".to_vec(), None, 0);
        store.set(b"b".to_vec(), b"2".to_vec(), None, 0);
        store.set(b"m".to_vec(), b"3".to_vec(), None, 0);
        store.set(b"z".to_vec(), b"4".to_vec(), None, 0);
        store.set(b"-".to_vec(), b"5".to_vec(), None, 0);
        store.set(b"]".to_vec(), b"6".to_vec(), None, 0);
        store.set(b"[abc".to_vec(), b"7".to_vec(), None, 0);

        let range_out =
            dispatch_argv(&[b"KEYS".to_vec(), b"[z-a]".to_vec()], &mut store, 0).expect("keys");
        assert_eq!(
            range_out,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"a".to_vec())),
                RespFrame::BulkString(Some(b"b".to_vec())),
                RespFrame::BulkString(Some(b"m".to_vec())),
                RespFrame::BulkString(Some(b"z".to_vec())),
            ]))
        );

        let escaped_out =
            dispatch_argv(&[b"KEYS".to_vec(), b"[\\-]".to_vec()], &mut store, 0).expect("keys");
        assert_eq!(
            escaped_out,
            RespFrame::Array(Some(vec![RespFrame::BulkString(Some(b"-".to_vec()))]))
        );

        let trailing_dash_out =
            dispatch_argv(&[b"KEYS".to_vec(), b"[a-]".to_vec()], &mut store, 0).expect("keys");
        assert_eq!(
            trailing_dash_out,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"]".to_vec())),
                RespFrame::BulkString(Some(b"a".to_vec())),
            ]))
        );

        let literal_bang_out =
            dispatch_argv(&[b"KEYS".to_vec(), b"[!a]".to_vec()], &mut store, 0).expect("keys");
        assert_eq!(
            literal_bang_out,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"!".to_vec())),
                RespFrame::BulkString(Some(b"a".to_vec())),
            ]))
        );

        let malformed_out =
            dispatch_argv(&[b"KEYS".to_vec(), b"[abc".to_vec()], &mut store, 0).expect("keys");
        assert_eq!(
            malformed_out,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"a".to_vec())),
                RespFrame::BulkString(Some(b"b".to_vec())),
            ]))
        );
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
    fn expired_keys_become_invisible_to_get_keys_dbsize_and_ttl() {
        let mut store = Store::new();
        dispatch_argv(
            &[b"SET".to_vec(), b"live".to_vec(), b"1".to_vec()],
            &mut store,
            0,
        )
        .expect("set live");
        dispatch_argv(
            &[
                b"SET".to_vec(),
                b"soon".to_vec(),
                b"2".to_vec(),
                b"PX".to_vec(),
                b"100".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("set soon");

        let keys_before =
            dispatch_argv(&[b"KEYS".to_vec(), b"*".to_vec()], &mut store, 0).expect("keys before");
        assert_eq!(
            keys_before,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"live".to_vec())),
                RespFrame::BulkString(Some(b"soon".to_vec())),
            ]))
        );

        let get_expired =
            dispatch_argv(&[b"GET".to_vec(), b"soon".to_vec()], &mut store, 150).expect("get");
        assert_eq!(get_expired, RespFrame::BulkString(None));

        let ttl_expired =
            dispatch_argv(&[b"TTL".to_vec(), b"soon".to_vec()], &mut store, 150).expect("ttl");
        assert_eq!(ttl_expired, RespFrame::Integer(-2));

        let keys_after =
            dispatch_argv(&[b"KEYS".to_vec(), b"*".to_vec()], &mut store, 150).expect("keys after");
        assert_eq!(
            keys_after,
            RespFrame::Array(Some(vec![RespFrame::BulkString(Some(b"live".to_vec()))]))
        );

        let dbsize_after =
            dispatch_argv(&[b"DBSIZE".to_vec()], &mut store, 150).expect("dbsize after");
        assert_eq!(dbsize_after, RespFrame::Integer(1));
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
        let command_variants = [
            (b"set".to_vec(), b"get".to_vec()),
            (b"SET".to_vec(), b"GET".to_vec()),
            (b"SeT".to_vec(), b"gEt".to_vec()),
            (b"sEt".to_vec(), b"GeT".to_vec()),
        ];
        for (set_cmd, get_cmd) in command_variants {
            let mut store = Store::new();
            let set = vec![set_cmd, b"k".to_vec(), b"v".to_vec()];
            dispatch_argv(&set, &mut store, 0).expect("set variant");
            let get = vec![get_cmd, b"k".to_vec()];
            let out = dispatch_argv(&get, &mut store, 0).expect("get variant");
            assert_eq!(out, RespFrame::BulkString(Some(b"v".to_vec())));
        }
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
    fn expire_invalid_integer_argument_errors_invalid_integer_property() {
        let invalid_values = ["", "not-a-number", "1.5", "+-2", "999999999999999999999"];
        for invalid in invalid_values {
            let mut store = Store::new();
            dispatch_argv(
                &[b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()],
                &mut store,
                0,
            )
            .expect("set");

            let err = dispatch_argv(
                &[
                    b"EXPIRE".to_vec(),
                    b"k".to_vec(),
                    invalid.as_bytes().to_vec(),
                ],
                &mut store,
                0,
            )
            .expect_err("must fail");
            assert!(matches!(err, super::CommandError::InvalidInteger));
            assert_eq!(store.get(b"k", 0).unwrap(), Some(b"v".to_vec()));
        }
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
    fn expire_option_compatibility_rules_match_redis() {
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

        let xx_gt = dispatch_argv(
            &[
                b"EXPIRE".to_vec(),
                b"k".to_vec(),
                b"10".to_vec(),
                b"XX".to_vec(),
                b"GT".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xx+gt should be accepted");
        assert_eq!(xx_gt, RespFrame::Integer(0));

        let nx_xx_gt = dispatch_argv(
            &[
                b"EXPIRE".to_vec(),
                b"k".to_vec(),
                b"10".to_vec(),
                b"NX".to_vec(),
                b"XX".to_vec(),
                b"GT".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect_err("nx cannot combine with xx/gt");
        assert!(matches!(nx_xx_gt, super::CommandError::SyntaxError));
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
                b"2500".to_vec(),
                b"XX".to_vec(),
                b"GT".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("pexpire xx gt");
        assert_eq!(out, RespFrame::Integer(1));

        let out = dispatch_argv(
            &[
                b"PEXPIRE".to_vec(),
                b"k".to_vec(),
                b"2400".to_vec(),
                b"XX".to_vec(),
                b"GT".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("pexpire xx gt reject");
        assert_eq!(out, RespFrame::Integer(0));

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
                b"2600".to_vec(),
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
                b"2400".to_vec(),
                b"XX".to_vec(),
                b"LT".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("pexpire xx lt");
        assert_eq!(out, RespFrame::Integer(1));

        let out = dispatch_argv(
            &[
                b"PEXPIRE".to_vec(),
                b"k".to_vec(),
                b"2600".to_vec(),
                b"XX".to_vec(),
                b"LT".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("pexpire xx lt reject");
        assert_eq!(out, RespFrame::Integer(0));

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

    #[test]
    fn hset_and_hget_round_trip() {
        let mut store = Store::new();
        let set = vec![
            b"HSET".to_vec(),
            b"myhash".to_vec(),
            b"f1".to_vec(),
            b"v1".to_vec(),
        ];
        let out = dispatch_argv(&set, &mut store, 0).expect("hset");
        assert_eq!(out, RespFrame::Integer(1));

        let get = vec![b"HGET".to_vec(), b"myhash".to_vec(), b"f1".to_vec()];
        let out = dispatch_argv(&get, &mut store, 0).expect("hget");
        assert_eq!(out, RespFrame::BulkString(Some(b"v1".to_vec())));
    }

    #[test]
    fn hset_multiple_fields() {
        let mut store = Store::new();
        let argv = vec![
            b"HSET".to_vec(),
            b"h".to_vec(),
            b"a".to_vec(),
            b"1".to_vec(),
            b"b".to_vec(),
            b"2".to_vec(),
        ];
        let out = dispatch_argv(&argv, &mut store, 0).expect("hset multi");
        assert_eq!(out, RespFrame::Integer(2));
    }

    #[test]
    fn hget_missing_field_returns_nil() {
        let mut store = Store::new();
        let get = vec![b"HGET".to_vec(), b"h".to_vec(), b"f1".to_vec()];
        let out = dispatch_argv(&get, &mut store, 0).expect("hget missing");
        assert_eq!(out, RespFrame::BulkString(None));
    }

    #[test]
    fn hdel_command() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"HSET".to_vec(),
                b"h".to_vec(),
                b"a".to_vec(),
                b"1".to_vec(),
                b"b".to_vec(),
                b"2".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("hset");
        let out = dispatch_argv(
            &[
                b"HDEL".to_vec(),
                b"h".to_vec(),
                b"a".to_vec(),
                b"c".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("hdel");
        assert_eq!(out, RespFrame::Integer(1));
    }

    #[test]
    fn hexists_command() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"HSET".to_vec(),
                b"h".to_vec(),
                b"f".to_vec(),
                b"v".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("hset");
        let out = dispatch_argv(
            &[b"HEXISTS".to_vec(), b"h".to_vec(), b"f".to_vec()],
            &mut store,
            0,
        )
        .expect("hexists");
        assert_eq!(out, RespFrame::Integer(1));
        let out2 = dispatch_argv(
            &[b"HEXISTS".to_vec(), b"h".to_vec(), b"missing".to_vec()],
            &mut store,
            0,
        )
        .expect("hexists missing");
        assert_eq!(out2, RespFrame::Integer(0));
    }

    #[test]
    fn hlen_command() {
        let mut store = Store::new();
        let out =
            dispatch_argv(&[b"HLEN".to_vec(), b"h".to_vec()], &mut store, 0).expect("hlen empty");
        assert_eq!(out, RespFrame::Integer(0));
        dispatch_argv(
            &[
                b"HSET".to_vec(),
                b"h".to_vec(),
                b"a".to_vec(),
                b"1".to_vec(),
                b"b".to_vec(),
                b"2".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("hset");
        let out2 = dispatch_argv(&[b"HLEN".to_vec(), b"h".to_vec()], &mut store, 0).expect("hlen");
        assert_eq!(out2, RespFrame::Integer(2));
    }

    #[test]
    fn hgetall_command() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"HSET".to_vec(),
                b"h".to_vec(),
                b"b".to_vec(),
                b"2".to_vec(),
                b"a".to_vec(),
                b"1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("hset");
        let out =
            dispatch_argv(&[b"HGETALL".to_vec(), b"h".to_vec()], &mut store, 0).expect("hgetall");
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"a".to_vec())),
                RespFrame::BulkString(Some(b"1".to_vec())),
                RespFrame::BulkString(Some(b"b".to_vec())),
                RespFrame::BulkString(Some(b"2".to_vec())),
            ]))
        );
    }

    #[test]
    fn hkeys_and_hvals_commands() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"HSET".to_vec(),
                b"h".to_vec(),
                b"b".to_vec(),
                b"2".to_vec(),
                b"a".to_vec(),
                b"1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("hset");
        let keys_out =
            dispatch_argv(&[b"HKEYS".to_vec(), b"h".to_vec()], &mut store, 0).expect("hkeys");
        assert_eq!(
            keys_out,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"a".to_vec())),
                RespFrame::BulkString(Some(b"b".to_vec())),
            ]))
        );
        let vals_out =
            dispatch_argv(&[b"HVALS".to_vec(), b"h".to_vec()], &mut store, 0).expect("hvals");
        assert_eq!(
            vals_out,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"1".to_vec())),
                RespFrame::BulkString(Some(b"2".to_vec())),
            ]))
        );
    }

    #[test]
    fn hmget_command() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"HSET".to_vec(),
                b"h".to_vec(),
                b"a".to_vec(),
                b"1".to_vec(),
                b"c".to_vec(),
                b"3".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("hset");
        let out = dispatch_argv(
            &[
                b"HMGET".to_vec(),
                b"h".to_vec(),
                b"a".to_vec(),
                b"b".to_vec(),
                b"c".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("hmget");
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
    fn hmset_command() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[
                b"HMSET".to_vec(),
                b"h".to_vec(),
                b"a".to_vec(),
                b"1".to_vec(),
                b"b".to_vec(),
                b"2".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("hmset");
        assert_eq!(out, RespFrame::SimpleString("OK".to_string()));
        let get = dispatch_argv(
            &[b"HGET".to_vec(), b"h".to_vec(), b"b".to_vec()],
            &mut store,
            0,
        )
        .expect("hget");
        assert_eq!(get, RespFrame::BulkString(Some(b"2".to_vec())));
    }

    #[test]
    fn hincrby_command() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[
                b"HINCRBY".to_vec(),
                b"h".to_vec(),
                b"counter".to_vec(),
                b"5".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("hincrby");
        assert_eq!(out, RespFrame::Integer(5));
        let out2 = dispatch_argv(
            &[
                b"HINCRBY".to_vec(),
                b"h".to_vec(),
                b"counter".to_vec(),
                b"-2".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("hincrby neg");
        assert_eq!(out2, RespFrame::Integer(3));
    }

    #[test]
    fn hsetnx_command() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[
                b"HSETNX".to_vec(),
                b"h".to_vec(),
                b"f".to_vec(),
                b"v1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("hsetnx");
        assert_eq!(out, RespFrame::Integer(1));
        let out2 = dispatch_argv(
            &[
                b"HSETNX".to_vec(),
                b"h".to_vec(),
                b"f".to_vec(),
                b"v2".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("hsetnx again");
        assert_eq!(out2, RespFrame::Integer(0));
        let val = dispatch_argv(
            &[b"HGET".to_vec(), b"h".to_vec(), b"f".to_vec()],
            &mut store,
            0,
        )
        .expect("hget");
        assert_eq!(val, RespFrame::BulkString(Some(b"v1".to_vec())));
    }

    #[test]
    fn hstrlen_command() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"HSET".to_vec(),
                b"h".to_vec(),
                b"f".to_vec(),
                b"hello".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("hset");
        let out = dispatch_argv(
            &[b"HSTRLEN".to_vec(), b"h".to_vec(), b"f".to_vec()],
            &mut store,
            0,
        )
        .expect("hstrlen");
        assert_eq!(out, RespFrame::Integer(5));
        let out2 = dispatch_argv(
            &[b"HSTRLEN".to_vec(), b"h".to_vec(), b"missing".to_vec()],
            &mut store,
            0,
        )
        .expect("hstrlen missing");
        assert_eq!(out2, RespFrame::Integer(0));
    }

    #[test]
    fn hash_wrongtype_on_string_key() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), b"v".to_vec(), None, 0);
        let err = dispatch_argv(
            &[
                b"HSET".to_vec(),
                b"k".to_vec(),
                b"f".to_vec(),
                b"v".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect_err("wrongtype");
        assert!(matches!(
            err,
            CommandError::Store(fr_store::StoreError::WrongType)
        ));
    }

    #[test]
    fn type_command_reports_hash() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"HSET".to_vec(),
                b"h".to_vec(),
                b"f".to_vec(),
                b"v".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("hset");
        let out = dispatch_argv(&[b"TYPE".to_vec(), b"h".to_vec()], &mut store, 0).expect("type");
        assert_eq!(out, RespFrame::SimpleString("hash".to_string()));
    }

    #[test]
    fn lpush_rpush_lpop_rpop_round_trip() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[
                b"LPUSH".to_vec(),
                b"list".to_vec(),
                b"a".to_vec(),
                b"b".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("lpush");
        assert_eq!(out, RespFrame::Integer(2));

        let out = dispatch_argv(
            &[b"RPUSH".to_vec(), b"list".to_vec(), b"c".to_vec()],
            &mut store,
            0,
        )
        .expect("rpush");
        assert_eq!(out, RespFrame::Integer(3));

        let out =
            dispatch_argv(&[b"LPOP".to_vec(), b"list".to_vec()], &mut store, 0).expect("lpop");
        assert_eq!(out, RespFrame::BulkString(Some(b"b".to_vec())));

        let out =
            dispatch_argv(&[b"RPOP".to_vec(), b"list".to_vec()], &mut store, 0).expect("rpop");
        assert_eq!(out, RespFrame::BulkString(Some(b"c".to_vec())));
    }

    #[test]
    fn lpop_rpop_on_missing_key_returns_nil() {
        let mut store = Store::new();
        let out = dispatch_argv(&[b"LPOP".to_vec(), b"missing".to_vec()], &mut store, 0)
            .expect("lpop missing");
        assert_eq!(out, RespFrame::BulkString(None));
        let out = dispatch_argv(&[b"RPOP".to_vec(), b"missing".to_vec()], &mut store, 0)
            .expect("rpop missing");
        assert_eq!(out, RespFrame::BulkString(None));
    }

    #[test]
    fn llen_command() {
        let mut store = Store::new();
        let out =
            dispatch_argv(&[b"LLEN".to_vec(), b"l".to_vec()], &mut store, 0).expect("llen empty");
        assert_eq!(out, RespFrame::Integer(0));
        dispatch_argv(
            &[
                b"RPUSH".to_vec(),
                b"l".to_vec(),
                b"a".to_vec(),
                b"b".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("rpush");
        let out = dispatch_argv(&[b"LLEN".to_vec(), b"l".to_vec()], &mut store, 0).expect("llen");
        assert_eq!(out, RespFrame::Integer(2));
    }

    #[test]
    fn lrange_command() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"RPUSH".to_vec(),
                b"l".to_vec(),
                b"a".to_vec(),
                b"b".to_vec(),
                b"c".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("rpush");
        let out = dispatch_argv(
            &[
                b"LRANGE".to_vec(),
                b"l".to_vec(),
                b"0".to_vec(),
                b"-1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("lrange all");
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"a".to_vec())),
                RespFrame::BulkString(Some(b"b".to_vec())),
                RespFrame::BulkString(Some(b"c".to_vec())),
            ]))
        );

        let out = dispatch_argv(
            &[
                b"LRANGE".to_vec(),
                b"l".to_vec(),
                b"1".to_vec(),
                b"1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("lrange single");
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![RespFrame::BulkString(Some(b"b".to_vec()))]))
        );
    }

    #[test]
    fn lindex_command() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"RPUSH".to_vec(),
                b"l".to_vec(),
                b"a".to_vec(),
                b"b".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("rpush");
        let out = dispatch_argv(
            &[b"LINDEX".to_vec(), b"l".to_vec(), b"0".to_vec()],
            &mut store,
            0,
        )
        .expect("lindex 0");
        assert_eq!(out, RespFrame::BulkString(Some(b"a".to_vec())));
        let out = dispatch_argv(
            &[b"LINDEX".to_vec(), b"l".to_vec(), b"-1".to_vec()],
            &mut store,
            0,
        )
        .expect("lindex -1");
        assert_eq!(out, RespFrame::BulkString(Some(b"b".to_vec())));
        let out = dispatch_argv(
            &[b"LINDEX".to_vec(), b"l".to_vec(), b"5".to_vec()],
            &mut store,
            0,
        )
        .expect("lindex oob");
        assert_eq!(out, RespFrame::BulkString(None));
    }

    #[test]
    fn lset_command() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"RPUSH".to_vec(),
                b"l".to_vec(),
                b"a".to_vec(),
                b"b".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("rpush");
        let out = dispatch_argv(
            &[
                b"LSET".to_vec(),
                b"l".to_vec(),
                b"0".to_vec(),
                b"x".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("lset");
        assert_eq!(out, RespFrame::SimpleString("OK".to_string()));
        let val = dispatch_argv(
            &[b"LINDEX".to_vec(), b"l".to_vec(), b"0".to_vec()],
            &mut store,
            0,
        )
        .expect("lindex");
        assert_eq!(val, RespFrame::BulkString(Some(b"x".to_vec())));
    }

    #[test]
    fn list_wrongtype_on_string_key() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), b"v".to_vec(), None, 0);
        let err = dispatch_argv(
            &[b"LPUSH".to_vec(), b"k".to_vec(), b"a".to_vec()],
            &mut store,
            0,
        )
        .expect_err("wrongtype");
        assert!(matches!(
            err,
            CommandError::Store(fr_store::StoreError::WrongType)
        ));
    }

    #[test]
    fn type_command_reports_list() {
        let mut store = Store::new();
        dispatch_argv(
            &[b"RPUSH".to_vec(), b"l".to_vec(), b"a".to_vec()],
            &mut store,
            0,
        )
        .expect("rpush");
        let out = dispatch_argv(&[b"TYPE".to_vec(), b"l".to_vec()], &mut store, 0).expect("type");
        assert_eq!(out, RespFrame::SimpleString("list".to_string()));
    }

    #[test]
    fn sadd_srem_scard_sismember_round_trip() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[
                b"SADD".to_vec(),
                b"s".to_vec(),
                b"a".to_vec(),
                b"b".to_vec(),
                b"a".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("sadd");
        assert_eq!(out, RespFrame::Integer(2));

        let out = dispatch_argv(&[b"SCARD".to_vec(), b"s".to_vec()], &mut store, 0).expect("scard");
        assert_eq!(out, RespFrame::Integer(2));

        let out = dispatch_argv(
            &[b"SISMEMBER".to_vec(), b"s".to_vec(), b"a".to_vec()],
            &mut store,
            0,
        )
        .expect("sismember yes");
        assert_eq!(out, RespFrame::Integer(1));

        let out = dispatch_argv(
            &[b"SISMEMBER".to_vec(), b"s".to_vec(), b"c".to_vec()],
            &mut store,
            0,
        )
        .expect("sismember no");
        assert_eq!(out, RespFrame::Integer(0));

        let out = dispatch_argv(
            &[
                b"SREM".to_vec(),
                b"s".to_vec(),
                b"a".to_vec(),
                b"c".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("srem");
        assert_eq!(out, RespFrame::Integer(1));
    }

    #[test]
    fn smembers_command() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"SADD".to_vec(),
                b"s".to_vec(),
                b"b".to_vec(),
                b"a".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("sadd");
        let out =
            dispatch_argv(&[b"SMEMBERS".to_vec(), b"s".to_vec()], &mut store, 0).expect("smembers");
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"a".to_vec())),
                RespFrame::BulkString(Some(b"b".to_vec())),
            ]))
        );
    }

    #[test]
    fn set_wrongtype_on_string_key() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), b"v".to_vec(), None, 0);
        let err = dispatch_argv(
            &[b"SADD".to_vec(), b"k".to_vec(), b"a".to_vec()],
            &mut store,
            0,
        )
        .expect_err("wrongtype");
        assert!(matches!(
            err,
            CommandError::Store(fr_store::StoreError::WrongType)
        ));
    }

    #[test]
    fn type_command_reports_set() {
        let mut store = Store::new();
        dispatch_argv(
            &[b"SADD".to_vec(), b"s".to_vec(), b"a".to_vec()],
            &mut store,
            0,
        )
        .expect("sadd");
        let out = dispatch_argv(&[b"TYPE".to_vec(), b"s".to_vec()], &mut store, 0).expect("type");
        assert_eq!(out, RespFrame::SimpleString("set".to_string()));
    }

    #[test]
    fn scard_missing_key_returns_zero() {
        let mut store = Store::new();
        let out = dispatch_argv(&[b"SCARD".to_vec(), b"missing".to_vec()], &mut store, 0)
            .expect("scard missing");
        assert_eq!(out, RespFrame::Integer(0));
    }

    // ── Sorted Set command tests ────────────────────────────────────────

    #[test]
    fn zadd_and_zscore() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[
                b"ZADD".to_vec(),
                b"zs".to_vec(),
                b"1.5".to_vec(),
                b"a".to_vec(),
                b"2.5".to_vec(),
                b"b".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("zadd");
        assert_eq!(out, RespFrame::Integer(2));

        let score = dispatch_argv(
            &[b"ZSCORE".to_vec(), b"zs".to_vec(), b"a".to_vec()],
            &mut store,
            0,
        )
        .expect("zscore");
        assert_eq!(score, RespFrame::BulkString(Some(b"1.5".to_vec())));
    }

    #[test]
    fn zscore_missing_member() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[b"ZSCORE".to_vec(), b"zs".to_vec(), b"none".to_vec()],
            &mut store,
            0,
        )
        .expect("zscore missing");
        assert_eq!(out, RespFrame::BulkString(None));
    }

    #[test]
    fn zcard_and_zrem() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"ZADD".to_vec(),
                b"zs".to_vec(),
                b"1".to_vec(),
                b"a".to_vec(),
                b"2".to_vec(),
                b"b".to_vec(),
                b"3".to_vec(),
                b"c".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("zadd");
        let card =
            dispatch_argv(&[b"ZCARD".to_vec(), b"zs".to_vec()], &mut store, 0).expect("zcard");
        assert_eq!(card, RespFrame::Integer(3));

        let removed = dispatch_argv(
            &[b"ZREM".to_vec(), b"zs".to_vec(), b"b".to_vec()],
            &mut store,
            0,
        )
        .expect("zrem");
        assert_eq!(removed, RespFrame::Integer(1));

        let card2 = dispatch_argv(&[b"ZCARD".to_vec(), b"zs".to_vec()], &mut store, 0)
            .expect("zcard after rem");
        assert_eq!(card2, RespFrame::Integer(2));
    }

    #[test]
    fn zrank_and_zrevrank() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"ZADD".to_vec(),
                b"zs".to_vec(),
                b"1".to_vec(),
                b"a".to_vec(),
                b"2".to_vec(),
                b"b".to_vec(),
                b"3".to_vec(),
                b"c".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("zadd");

        let rank = dispatch_argv(
            &[b"ZRANK".to_vec(), b"zs".to_vec(), b"b".to_vec()],
            &mut store,
            0,
        )
        .expect("zrank");
        assert_eq!(rank, RespFrame::Integer(1));

        let revrank = dispatch_argv(
            &[b"ZREVRANK".to_vec(), b"zs".to_vec(), b"b".to_vec()],
            &mut store,
            0,
        )
        .expect("zrevrank");
        assert_eq!(revrank, RespFrame::Integer(1));

        let missing = dispatch_argv(
            &[b"ZRANK".to_vec(), b"zs".to_vec(), b"x".to_vec()],
            &mut store,
            0,
        )
        .expect("zrank missing");
        assert_eq!(missing, RespFrame::BulkString(None));
    }

    #[test]
    fn zrange_basic() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"ZADD".to_vec(),
                b"zs".to_vec(),
                b"1".to_vec(),
                b"a".to_vec(),
                b"2".to_vec(),
                b"b".to_vec(),
                b"3".to_vec(),
                b"c".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("zadd");

        let out = dispatch_argv(
            &[
                b"ZRANGE".to_vec(),
                b"zs".to_vec(),
                b"0".to_vec(),
                b"-1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("zrange");
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"a".to_vec())),
                RespFrame::BulkString(Some(b"b".to_vec())),
                RespFrame::BulkString(Some(b"c".to_vec())),
            ]))
        );
    }

    #[test]
    fn zrange_withscores() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"ZADD".to_vec(),
                b"zs".to_vec(),
                b"1".to_vec(),
                b"a".to_vec(),
                b"2".to_vec(),
                b"b".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("zadd");

        let out = dispatch_argv(
            &[
                b"ZRANGE".to_vec(),
                b"zs".to_vec(),
                b"0".to_vec(),
                b"-1".to_vec(),
                b"WITHSCORES".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("zrange withscores");
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"a".to_vec())),
                RespFrame::BulkString(Some(b"1".to_vec())),
                RespFrame::BulkString(Some(b"b".to_vec())),
                RespFrame::BulkString(Some(b"2".to_vec())),
            ]))
        );
    }

    #[test]
    fn zrevrange_basic() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"ZADD".to_vec(),
                b"zs".to_vec(),
                b"1".to_vec(),
                b"a".to_vec(),
                b"2".to_vec(),
                b"b".to_vec(),
                b"3".to_vec(),
                b"c".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("zadd");

        let out = dispatch_argv(
            &[
                b"ZREVRANGE".to_vec(),
                b"zs".to_vec(),
                b"0".to_vec(),
                b"-1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("zrevrange");
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"c".to_vec())),
                RespFrame::BulkString(Some(b"b".to_vec())),
                RespFrame::BulkString(Some(b"a".to_vec())),
            ]))
        );
    }

    #[test]
    fn zrangebyscore_basic() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"ZADD".to_vec(),
                b"zs".to_vec(),
                b"1".to_vec(),
                b"a".to_vec(),
                b"2".to_vec(),
                b"b".to_vec(),
                b"3".to_vec(),
                b"c".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("zadd");

        let out = dispatch_argv(
            &[
                b"ZRANGEBYSCORE".to_vec(),
                b"zs".to_vec(),
                b"1".to_vec(),
                b"2".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("zrangebyscore");
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"a".to_vec())),
                RespFrame::BulkString(Some(b"b".to_vec())),
            ]))
        );
    }

    #[test]
    fn zrangebyscore_inf_bounds() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"ZADD".to_vec(),
                b"zs".to_vec(),
                b"1".to_vec(),
                b"a".to_vec(),
                b"2".to_vec(),
                b"b".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("zadd");

        let out = dispatch_argv(
            &[
                b"ZRANGEBYSCORE".to_vec(),
                b"zs".to_vec(),
                b"-inf".to_vec(),
                b"+inf".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("zrangebyscore -inf +inf");
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"a".to_vec())),
                RespFrame::BulkString(Some(b"b".to_vec())),
            ]))
        );
    }

    #[test]
    fn zcount_basic() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"ZADD".to_vec(),
                b"zs".to_vec(),
                b"1".to_vec(),
                b"a".to_vec(),
                b"2".to_vec(),
                b"b".to_vec(),
                b"3".to_vec(),
                b"c".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("zadd");

        let out = dispatch_argv(
            &[
                b"ZCOUNT".to_vec(),
                b"zs".to_vec(),
                b"1".to_vec(),
                b"2".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("zcount");
        assert_eq!(out, RespFrame::Integer(2));
    }

    #[test]
    fn zincrby_basic() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"ZADD".to_vec(),
                b"zs".to_vec(),
                b"1".to_vec(),
                b"a".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("zadd");

        let out = dispatch_argv(
            &[
                b"ZINCRBY".to_vec(),
                b"zs".to_vec(),
                b"2.5".to_vec(),
                b"a".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("zincrby");
        assert_eq!(out, RespFrame::BulkString(Some(b"3.5".to_vec())));
    }

    #[test]
    fn zpopmin_and_zpopmax() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"ZADD".to_vec(),
                b"zs".to_vec(),
                b"1".to_vec(),
                b"a".to_vec(),
                b"2".to_vec(),
                b"b".to_vec(),
                b"3".to_vec(),
                b"c".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("zadd");

        let min =
            dispatch_argv(&[b"ZPOPMIN".to_vec(), b"zs".to_vec()], &mut store, 0).expect("zpopmin");
        assert_eq!(
            min,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"a".to_vec())),
                RespFrame::BulkString(Some(b"1".to_vec())),
            ]))
        );

        let max =
            dispatch_argv(&[b"ZPOPMAX".to_vec(), b"zs".to_vec()], &mut store, 0).expect("zpopmax");
        assert_eq!(
            max,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"c".to_vec())),
                RespFrame::BulkString(Some(b"3".to_vec())),
            ]))
        );

        let card =
            dispatch_argv(&[b"ZCARD".to_vec(), b"zs".to_vec()], &mut store, 0).expect("zcard");
        assert_eq!(card, RespFrame::Integer(1));
    }

    #[test]
    fn zpopmin_empty() {
        let mut store = Store::new();
        let out = dispatch_argv(&[b"ZPOPMIN".to_vec(), b"zs".to_vec()], &mut store, 0)
            .expect("zpopmin empty");
        assert_eq!(out, RespFrame::Array(Some(vec![])));
    }

    #[test]
    fn zcard_missing_key() {
        let mut store = Store::new();
        let out = dispatch_argv(&[b"ZCARD".to_vec(), b"missing".to_vec()], &mut store, 0)
            .expect("zcard missing");
        assert_eq!(out, RespFrame::Integer(0));
    }

    #[test]
    fn zadd_wrongtype() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), b"v".to_vec(), None, 0);
        let err = dispatch_argv(
            &[
                b"ZADD".to_vec(),
                b"k".to_vec(),
                b"1".to_vec(),
                b"m".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect_err("wrongtype");
        assert!(matches!(
            err,
            CommandError::Store(fr_store::StoreError::WrongType)
        ));
    }

    #[test]
    fn type_command_reports_zset() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"ZADD".to_vec(),
                b"zs".to_vec(),
                b"1".to_vec(),
                b"a".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("zadd");
        let out = dispatch_argv(&[b"TYPE".to_vec(), b"zs".to_vec()], &mut store, 0).expect("type");
        assert_eq!(out, RespFrame::SimpleString("zset".to_string()));
    }

    #[test]
    fn zadd_wrong_arity() {
        let mut store = Store::new();
        let err = dispatch_argv(
            &[b"ZADD".to_vec(), b"zs".to_vec(), b"1".to_vec()],
            &mut store,
            0,
        )
        .expect_err("wrong arity");
        assert!(matches!(err, CommandError::WrongArity("ZADD")));
    }

    // ── String extension command tests ──────────────────────────────────

    #[test]
    fn setex_sets_with_expiry() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[
                b"SETEX".to_vec(),
                b"k".to_vec(),
                b"10".to_vec(),
                b"v".to_vec(),
            ],
            &mut store,
            1000,
        )
        .expect("setex");
        assert_eq!(out, RespFrame::SimpleString("OK".to_string()));

        let val = dispatch_argv(&[b"GET".to_vec(), b"k".to_vec()], &mut store, 1000).expect("get");
        assert_eq!(val, RespFrame::BulkString(Some(b"v".to_vec())));

        // Should be expired after 10 seconds
        let val2 = dispatch_argv(&[b"GET".to_vec(), b"k".to_vec()], &mut store, 12000)
            .expect("get expired");
        assert_eq!(val2, RespFrame::BulkString(None));
    }

    #[test]
    fn psetex_sets_with_ms_expiry() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[
                b"PSETEX".to_vec(),
                b"k".to_vec(),
                b"500".to_vec(),
                b"v".to_vec(),
            ],
            &mut store,
            1000,
        )
        .expect("psetex");
        assert_eq!(out, RespFrame::SimpleString("OK".to_string()));

        let val = dispatch_argv(&[b"GET".to_vec(), b"k".to_vec()], &mut store, 1400)
            .expect("get within ttl");
        assert_eq!(val, RespFrame::BulkString(Some(b"v".to_vec())));

        let val2 = dispatch_argv(&[b"GET".to_vec(), b"k".to_vec()], &mut store, 1600)
            .expect("get expired");
        assert_eq!(val2, RespFrame::BulkString(None));
    }

    #[test]
    fn getdel_returns_and_deletes() {
        let mut store = Store::new();
        dispatch_argv(
            &[b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()],
            &mut store,
            0,
        )
        .expect("set");

        let out =
            dispatch_argv(&[b"GETDEL".to_vec(), b"k".to_vec()], &mut store, 0).expect("getdel");
        assert_eq!(out, RespFrame::BulkString(Some(b"v".to_vec())));

        let out2 = dispatch_argv(&[b"GET".to_vec(), b"k".to_vec()], &mut store, 0)
            .expect("get after getdel");
        assert_eq!(out2, RespFrame::BulkString(None));
    }

    #[test]
    fn getdel_missing_key() {
        let mut store = Store::new();
        let out = dispatch_argv(&[b"GETDEL".to_vec(), b"k".to_vec()], &mut store, 0)
            .expect("getdel missing");
        assert_eq!(out, RespFrame::BulkString(None));
    }

    #[test]
    fn getrange_command() {
        let mut store = Store::new();
        dispatch_argv(
            &[b"SET".to_vec(), b"k".to_vec(), b"Hello".to_vec()],
            &mut store,
            0,
        )
        .expect("set");

        let out = dispatch_argv(
            &[
                b"GETRANGE".to_vec(),
                b"k".to_vec(),
                b"0".to_vec(),
                b"2".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("getrange");
        assert_eq!(out, RespFrame::BulkString(Some(b"Hel".to_vec())));
    }

    #[test]
    fn setrange_command() {
        let mut store = Store::new();
        dispatch_argv(
            &[b"SET".to_vec(), b"k".to_vec(), b"Hello World".to_vec()],
            &mut store,
            0,
        )
        .expect("set");

        let out = dispatch_argv(
            &[
                b"SETRANGE".to_vec(),
                b"k".to_vec(),
                b"6".to_vec(),
                b"Redis".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("setrange");
        assert_eq!(out, RespFrame::Integer(11));

        let val = dispatch_argv(&[b"GET".to_vec(), b"k".to_vec()], &mut store, 0).expect("get");
        assert_eq!(val, RespFrame::BulkString(Some(b"Hello Redis".to_vec())));
    }

    #[test]
    fn incrbyfloat_command() {
        let mut store = Store::new();
        dispatch_argv(
            &[b"SET".to_vec(), b"k".to_vec(), b"10.5".to_vec()],
            &mut store,
            0,
        )
        .expect("set");

        let out = dispatch_argv(
            &[b"INCRBYFLOAT".to_vec(), b"k".to_vec(), b"0.1".to_vec()],
            &mut store,
            0,
        )
        .expect("incrbyfloat");
        // Result is a bulk string of the new float value
        if let RespFrame::BulkString(Some(v)) = &out {
            let val: f64 = std::str::from_utf8(v).unwrap().parse().unwrap();
            assert!((val - 10.6).abs() < 1e-10);
        } else {
            panic!("expected bulk string, got {:?}", out);
        }
    }

    #[test]
    fn incrbyfloat_missing_key() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[b"INCRBYFLOAT".to_vec(), b"k".to_vec(), b"3.5".to_vec()],
            &mut store,
            0,
        )
        .expect("incrbyfloat missing");
        assert_eq!(out, RespFrame::BulkString(Some(b"3.5".to_vec())));
    }

    #[test]
    fn setex_rejects_zero_ttl() {
        let mut store = Store::new();
        let err = dispatch_argv(
            &[
                b"SETEX".to_vec(),
                b"k".to_vec(),
                b"0".to_vec(),
                b"v".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect_err("zero ttl");
        assert!(matches!(err, CommandError::InvalidInteger));
    }

    // ── Set algebra command tests ───────────────────────────────────────

    #[test]
    fn sinter_command() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"SADD".to_vec(),
                b"s1".to_vec(),
                b"a".to_vec(),
                b"b".to_vec(),
                b"c".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("sadd s1");
        dispatch_argv(
            &[
                b"SADD".to_vec(),
                b"s2".to_vec(),
                b"b".to_vec(),
                b"c".to_vec(),
                b"d".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("sadd s2");

        let out = dispatch_argv(
            &[b"SINTER".to_vec(), b"s1".to_vec(), b"s2".to_vec()],
            &mut store,
            0,
        )
        .expect("sinter");
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"b".to_vec())),
                RespFrame::BulkString(Some(b"c".to_vec())),
            ]))
        );
    }

    #[test]
    fn sunion_command() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"SADD".to_vec(),
                b"s1".to_vec(),
                b"a".to_vec(),
                b"b".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("sadd s1");
        dispatch_argv(
            &[
                b"SADD".to_vec(),
                b"s2".to_vec(),
                b"b".to_vec(),
                b"c".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("sadd s2");

        let out = dispatch_argv(
            &[b"SUNION".to_vec(), b"s1".to_vec(), b"s2".to_vec()],
            &mut store,
            0,
        )
        .expect("sunion");
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"a".to_vec())),
                RespFrame::BulkString(Some(b"b".to_vec())),
                RespFrame::BulkString(Some(b"c".to_vec())),
            ]))
        );
    }

    #[test]
    fn sdiff_command() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"SADD".to_vec(),
                b"s1".to_vec(),
                b"a".to_vec(),
                b"b".to_vec(),
                b"c".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("sadd s1");
        dispatch_argv(
            &[b"SADD".to_vec(), b"s2".to_vec(), b"b".to_vec()],
            &mut store,
            0,
        )
        .expect("sadd s2");

        let out = dispatch_argv(
            &[b"SDIFF".to_vec(), b"s1".to_vec(), b"s2".to_vec()],
            &mut store,
            0,
        )
        .expect("sdiff");
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"a".to_vec())),
                RespFrame::BulkString(Some(b"c".to_vec())),
            ]))
        );
    }

    #[test]
    fn spop_command() {
        let mut store = Store::new();
        dispatch_argv(
            &[b"SADD".to_vec(), b"s".to_vec(), b"a".to_vec()],
            &mut store,
            0,
        )
        .expect("sadd");

        let out = dispatch_argv(&[b"SPOP".to_vec(), b"s".to_vec()], &mut store, 0).expect("spop");
        assert_eq!(out, RespFrame::BulkString(Some(b"a".to_vec())));

        let card =
            dispatch_argv(&[b"SCARD".to_vec(), b"s".to_vec()], &mut store, 0).expect("scard");
        assert_eq!(card, RespFrame::Integer(0));
    }

    #[test]
    fn spop_empty() {
        let mut store = Store::new();
        let out =
            dispatch_argv(&[b"SPOP".to_vec(), b"s".to_vec()], &mut store, 0).expect("spop empty");
        assert_eq!(out, RespFrame::BulkString(None));
    }

    #[test]
    fn srandmember_command() {
        let mut store = Store::new();
        dispatch_argv(
            &[b"SADD".to_vec(), b"s".to_vec(), b"a".to_vec()],
            &mut store,
            0,
        )
        .expect("sadd");

        let out = dispatch_argv(&[b"SRANDMEMBER".to_vec(), b"s".to_vec()], &mut store, 0)
            .expect("srandmember");
        assert_eq!(out, RespFrame::BulkString(Some(b"a".to_vec())));

        // srandmember should NOT remove the member
        let card =
            dispatch_argv(&[b"SCARD".to_vec(), b"s".to_vec()], &mut store, 0).expect("scard");
        assert_eq!(card, RespFrame::Integer(1));
    }

    #[test]
    fn srandmember_empty() {
        let mut store = Store::new();
        let out = dispatch_argv(&[b"SRANDMEMBER".to_vec(), b"s".to_vec()], &mut store, 0)
            .expect("srandmember empty");
        assert_eq!(out, RespFrame::BulkString(None));
    }

    // ── Bitmap command tests ────────────────────────────────────────────

    #[test]
    fn setbit_and_getbit() {
        let mut store = Store::new();
        let old = dispatch_argv(
            &[
                b"SETBIT".to_vec(),
                b"bm".to_vec(),
                b"7".to_vec(),
                b"1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("setbit");
        assert_eq!(old, RespFrame::Integer(0));

        let bit = dispatch_argv(
            &[b"GETBIT".to_vec(), b"bm".to_vec(), b"7".to_vec()],
            &mut store,
            0,
        )
        .expect("getbit");
        assert_eq!(bit, RespFrame::Integer(1));

        let bit0 = dispatch_argv(
            &[b"GETBIT".to_vec(), b"bm".to_vec(), b"0".to_vec()],
            &mut store,
            0,
        )
        .expect("getbit 0");
        assert_eq!(bit0, RespFrame::Integer(0));
    }

    #[test]
    fn bitcount_command() {
        let mut store = Store::new();
        // Set bits 0 and 7 -> byte 0 = 0b10000001 = 0x81 (2 bits set)
        dispatch_argv(
            &[
                b"SETBIT".to_vec(),
                b"bm".to_vec(),
                b"0".to_vec(),
                b"1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("setbit 0");
        dispatch_argv(
            &[
                b"SETBIT".to_vec(),
                b"bm".to_vec(),
                b"7".to_vec(),
                b"1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("setbit 7");

        let count = dispatch_argv(&[b"BITCOUNT".to_vec(), b"bm".to_vec()], &mut store, 0)
            .expect("bitcount");
        assert_eq!(count, RespFrame::Integer(2));
    }

    #[test]
    fn bitcount_with_range() {
        let mut store = Store::new();
        // Set "foobar" which has a known bitcount
        dispatch_argv(
            &[b"SET".to_vec(), b"k".to_vec(), b"foobar".to_vec()],
            &mut store,
            0,
        )
        .expect("set");

        let count = dispatch_argv(
            &[
                b"BITCOUNT".to_vec(),
                b"k".to_vec(),
                b"0".to_vec(),
                b"0".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("bitcount range");
        // 'f' = 0x66 = 0b01100110 = 4 bits set
        assert_eq!(count, RespFrame::Integer(4));
    }

    #[test]
    fn bitpos_command() {
        let mut store = Store::new();
        // Set byte 0 = 0x00, byte 1 = 0x01 (bit 15 set)
        dispatch_argv(
            &[
                b"SETBIT".to_vec(),
                b"bm".to_vec(),
                b"15".to_vec(),
                b"1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("setbit 15");

        let pos = dispatch_argv(
            &[b"BITPOS".to_vec(), b"bm".to_vec(), b"1".to_vec()],
            &mut store,
            0,
        )
        .expect("bitpos 1");
        assert_eq!(pos, RespFrame::Integer(15));
    }

    #[test]
    fn getbit_missing_key() {
        let mut store = Store::new();
        let bit = dispatch_argv(
            &[b"GETBIT".to_vec(), b"missing".to_vec(), b"10".to_vec()],
            &mut store,
            0,
        )
        .expect("getbit missing");
        assert_eq!(bit, RespFrame::Integer(0));
    }

    #[test]
    fn bitcount_missing_key() {
        let mut store = Store::new();
        let count = dispatch_argv(&[b"BITCOUNT".to_vec(), b"missing".to_vec()], &mut store, 0)
            .expect("bitcount missing");
        assert_eq!(count, RespFrame::Integer(0));
    }

    #[test]
    fn setbit_wrongtype() {
        let mut store = Store::new();
        dispatch_argv(
            &[b"SADD".to_vec(), b"s".to_vec(), b"a".to_vec()],
            &mut store,
            0,
        )
        .expect("sadd");
        let err = dispatch_argv(
            &[
                b"SETBIT".to_vec(),
                b"s".to_vec(),
                b"0".to_vec(),
                b"1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect_err("wrongtype");
        assert!(matches!(
            err,
            CommandError::Store(fr_store::StoreError::WrongType)
        ));
    }

    // ── Extended List command tests ──────────────────────────────────────

    #[test]
    fn lpos_command() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"RPUSH".to_vec(),
                b"l".to_vec(),
                b"a".to_vec(),
                b"b".to_vec(),
                b"c".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("rpush");

        let pos = dispatch_argv(
            &[b"LPOS".to_vec(), b"l".to_vec(), b"b".to_vec()],
            &mut store,
            0,
        )
        .expect("lpos");
        assert_eq!(pos, RespFrame::Integer(1));

        let missing = dispatch_argv(
            &[b"LPOS".to_vec(), b"l".to_vec(), b"x".to_vec()],
            &mut store,
            0,
        )
        .expect("lpos missing");
        assert_eq!(missing, RespFrame::BulkString(None));
    }

    #[test]
    fn linsert_before_and_after() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"RPUSH".to_vec(),
                b"l".to_vec(),
                b"a".to_vec(),
                b"c".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("rpush");

        let len = dispatch_argv(
            &[
                b"LINSERT".to_vec(),
                b"l".to_vec(),
                b"BEFORE".to_vec(),
                b"c".to_vec(),
                b"b".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("linsert before");
        assert_eq!(len, RespFrame::Integer(3));

        let range = dispatch_argv(
            &[
                b"LRANGE".to_vec(),
                b"l".to_vec(),
                b"0".to_vec(),
                b"-1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("lrange");
        assert_eq!(
            range,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"a".to_vec())),
                RespFrame::BulkString(Some(b"b".to_vec())),
                RespFrame::BulkString(Some(b"c".to_vec())),
            ]))
        );
    }

    #[test]
    fn lrem_command() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"RPUSH".to_vec(),
                b"l".to_vec(),
                b"a".to_vec(),
                b"b".to_vec(),
                b"a".to_vec(),
                b"c".to_vec(),
                b"a".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("rpush");

        let removed = dispatch_argv(
            &[
                b"LREM".to_vec(),
                b"l".to_vec(),
                b"2".to_vec(),
                b"a".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("lrem");
        assert_eq!(removed, RespFrame::Integer(2));

        let len = dispatch_argv(&[b"LLEN".to_vec(), b"l".to_vec()], &mut store, 0).expect("llen");
        assert_eq!(len, RespFrame::Integer(3));
    }

    #[test]
    fn rpoplpush_command() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"RPUSH".to_vec(),
                b"src".to_vec(),
                b"a".to_vec(),
                b"b".to_vec(),
                b"c".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("rpush");

        let val = dispatch_argv(
            &[b"RPOPLPUSH".to_vec(), b"src".to_vec(), b"dst".to_vec()],
            &mut store,
            0,
        )
        .expect("rpoplpush");
        assert_eq!(val, RespFrame::BulkString(Some(b"c".to_vec())));

        // src should now be [a, b]
        let src_len =
            dispatch_argv(&[b"LLEN".to_vec(), b"src".to_vec()], &mut store, 0).expect("src llen");
        assert_eq!(src_len, RespFrame::Integer(2));

        // dst should be [c]
        let dst = dispatch_argv(
            &[
                b"LRANGE".to_vec(),
                b"dst".to_vec(),
                b"0".to_vec(),
                b"-1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("dst lrange");
        assert_eq!(
            dst,
            RespFrame::Array(Some(vec![RespFrame::BulkString(Some(b"c".to_vec()))]))
        );
    }

    #[test]
    fn rpoplpush_empty_source() {
        let mut store = Store::new();
        let val = dispatch_argv(
            &[b"RPOPLPUSH".to_vec(), b"empty".to_vec(), b"dst".to_vec()],
            &mut store,
            0,
        )
        .expect("rpoplpush empty");
        assert_eq!(val, RespFrame::BulkString(None));
    }

    // ── Extended Hash/ZSet command tests ─────────────────────────────────

    #[test]
    fn hincrbyfloat_command() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"HSET".to_vec(),
                b"h".to_vec(),
                b"f".to_vec(),
                b"10.5".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("hset");

        let out = dispatch_argv(
            &[
                b"HINCRBYFLOAT".to_vec(),
                b"h".to_vec(),
                b"f".to_vec(),
                b"0.1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("hincrbyfloat");
        if let RespFrame::BulkString(Some(v)) = &out {
            let val: f64 = std::str::from_utf8(v).unwrap().parse().unwrap();
            assert!((val - 10.6).abs() < 1e-10);
        } else {
            panic!("expected bulk string");
        }
    }

    #[test]
    fn hrandfield_command() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"HSET".to_vec(),
                b"h".to_vec(),
                b"f".to_vec(),
                b"v".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("hset");

        let out = dispatch_argv(&[b"HRANDFIELD".to_vec(), b"h".to_vec()], &mut store, 0)
            .expect("hrandfield");
        assert_eq!(out, RespFrame::BulkString(Some(b"f".to_vec())));
    }

    #[test]
    fn hrandfield_empty() {
        let mut store = Store::new();
        let out = dispatch_argv(&[b"HRANDFIELD".to_vec(), b"h".to_vec()], &mut store, 0)
            .expect("hrandfield empty");
        assert_eq!(out, RespFrame::BulkString(None));
    }

    #[test]
    fn zrevrangebyscore_command() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"ZADD".to_vec(),
                b"zs".to_vec(),
                b"1".to_vec(),
                b"a".to_vec(),
                b"2".to_vec(),
                b"b".to_vec(),
                b"3".to_vec(),
                b"c".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("zadd");

        let out = dispatch_argv(
            &[
                b"ZREVRANGEBYSCORE".to_vec(),
                b"zs".to_vec(),
                b"3".to_vec(),
                b"1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("zrevrangebyscore");
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"c".to_vec())),
                RespFrame::BulkString(Some(b"b".to_vec())),
                RespFrame::BulkString(Some(b"a".to_vec())),
            ]))
        );
    }

    #[test]
    fn zrangebylex_command() {
        let mut store = Store::new();
        // All same score so lex ordering applies
        dispatch_argv(
            &[
                b"ZADD".to_vec(),
                b"zs".to_vec(),
                b"0".to_vec(),
                b"a".to_vec(),
                b"0".to_vec(),
                b"b".to_vec(),
                b"0".to_vec(),
                b"c".to_vec(),
                b"0".to_vec(),
                b"d".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("zadd");

        let out = dispatch_argv(
            &[
                b"ZRANGEBYLEX".to_vec(),
                b"zs".to_vec(),
                b"[b".to_vec(),
                b"[c".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("zrangebylex");
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"b".to_vec())),
                RespFrame::BulkString(Some(b"c".to_vec())),
            ]))
        );
    }

    #[test]
    fn zlexcount_command() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"ZADD".to_vec(),
                b"zs".to_vec(),
                b"0".to_vec(),
                b"a".to_vec(),
                b"0".to_vec(),
                b"b".to_vec(),
                b"0".to_vec(),
                b"c".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("zadd");

        let out = dispatch_argv(
            &[
                b"ZLEXCOUNT".to_vec(),
                b"zs".to_vec(),
                b"-".to_vec(),
                b"+".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("zlexcount");
        assert_eq!(out, RespFrame::Integer(3));
    }

    // ── HyperLogLog command tests ─────────────────────────────────────────

    #[test]
    fn pfadd_command() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[
                b"PFADD".to_vec(),
                b"hll".to_vec(),
                b"a".to_vec(),
                b"b".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("pfadd");
        assert_eq!(out, RespFrame::Integer(1));

        // Adding same elements again
        let out2 = dispatch_argv(
            &[
                b"PFADD".to_vec(),
                b"hll".to_vec(),
                b"a".to_vec(),
                b"b".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("pfadd again");
        assert_eq!(out2, RespFrame::Integer(0));
    }

    #[test]
    fn pfadd_no_elements_creates_key() {
        let mut store = Store::new();
        let out = dispatch_argv(&[b"PFADD".to_vec(), b"hll".to_vec()], &mut store, 0)
            .expect("pfadd empty");
        assert_eq!(out, RespFrame::Integer(1));
    }

    #[test]
    fn pfcount_command() {
        let mut store = Store::new();
        let elements: Vec<Vec<u8>> = (0..100).map(|i| format!("e{i}").into_bytes()).collect();
        let mut argv = vec![b"PFADD".to_vec(), b"hll".to_vec()];
        argv.extend(elements);
        dispatch_argv(&argv, &mut store, 0).expect("pfadd batch");

        let out =
            dispatch_argv(&[b"PFCOUNT".to_vec(), b"hll".to_vec()], &mut store, 0).expect("pfcount");
        let RespFrame::Integer(count) = out else {
            panic!("expected integer, got {out:?}");
        };
        assert!((90..=110).contains(&count), "count={count}, expected ~100");
    }

    #[test]
    fn pfcount_missing_key() {
        let mut store = Store::new();
        let out = dispatch_argv(&[b"PFCOUNT".to_vec(), b"missing".to_vec()], &mut store, 0)
            .expect("pfcount missing");
        assert_eq!(out, RespFrame::Integer(0));
    }

    #[test]
    fn pfmerge_command() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"PFADD".to_vec(),
                b"h1".to_vec(),
                b"a".to_vec(),
                b"b".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("pfadd h1");
        dispatch_argv(
            &[
                b"PFADD".to_vec(),
                b"h2".to_vec(),
                b"c".to_vec(),
                b"d".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("pfadd h2");

        let out = dispatch_argv(
            &[
                b"PFMERGE".to_vec(),
                b"merged".to_vec(),
                b"h1".to_vec(),
                b"h2".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("pfmerge");
        assert_eq!(out, RespFrame::SimpleString("OK".to_string()));

        let count_out = dispatch_argv(&[b"PFCOUNT".to_vec(), b"merged".to_vec()], &mut store, 0)
            .expect("pfcount merged");
        let RespFrame::Integer(count) = count_out else {
            panic!("expected integer");
        };
        assert!((3..=5).contains(&count), "count={count}, expected ~4");
    }

    #[test]
    fn pfcount_multiple_keys() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"PFADD".to_vec(),
                b"h1".to_vec(),
                b"x".to_vec(),
                b"y".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("pfadd h1");
        dispatch_argv(
            &[
                b"PFADD".to_vec(),
                b"h2".to_vec(),
                b"y".to_vec(),
                b"z".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("pfadd h2");

        let out = dispatch_argv(
            &[b"PFCOUNT".to_vec(), b"h1".to_vec(), b"h2".to_vec()],
            &mut store,
            0,
        )
        .expect("pfcount multi");
        let RespFrame::Integer(count) = out else {
            panic!("expected integer");
        };
        assert!((2..=4).contains(&count), "count={count}, expected ~3");
    }
}
