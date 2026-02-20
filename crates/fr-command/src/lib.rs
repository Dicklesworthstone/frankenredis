#![forbid(unsafe_code)]

use fr_protocol::RespFrame;
use fr_store::{PttlValue, Store, StoreError, StreamId};

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
        Some(CommandId::Geoadd) => return geoadd(argv, store, now_ms),
        Some(CommandId::Geopos) => return geopos(argv, store, now_ms),
        Some(CommandId::Geodist) => return geodist(argv, store, now_ms),
        Some(CommandId::Geohash) => return geohash(argv, store, now_ms),
        Some(CommandId::Xadd) => return xadd(argv, store, now_ms),
        Some(CommandId::Xlen) => return xlen(argv, store, now_ms),
        Some(CommandId::Xdel) => return xdel(argv, store, now_ms),
        Some(CommandId::Xtrim) => return xtrim(argv, store, now_ms),
        Some(CommandId::Xread) => return xread(argv, store, now_ms),
        Some(CommandId::Xinfo) => return xinfo(argv, store, now_ms),
        Some(CommandId::Xgroup) => return xgroup(argv, store, now_ms),
        Some(CommandId::Xrange) => return xrange(argv, store, now_ms),
        Some(CommandId::Xrevrange) => return xrevrange(argv, store, now_ms),
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
    Geoadd,
    Geopos,
    Geodist,
    Geohash,
    Xadd,
    Xlen,
    Xdel,
    Xtrim,
    Xread,
    Xinfo,
    Xgroup,
    Xrange,
    Xrevrange,
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
            } else if eq_ascii_command(cmd, b"XADD") {
                Some(CommandId::Xadd)
            } else if eq_ascii_command(cmd, b"XLEN") {
                Some(CommandId::Xlen)
            } else if eq_ascii_command(cmd, b"XDEL") {
                Some(CommandId::Xdel)
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
            } else if eq_ascii_command(cmd, b"XREAD") {
                Some(CommandId::Xread)
            } else if eq_ascii_command(cmd, b"XINFO") {
                Some(CommandId::Xinfo)
            } else if eq_ascii_command(cmd, b"XTRIM") {
                Some(CommandId::Xtrim)
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
            } else if eq_ascii_command(cmd, b"XGROUP") {
                Some(CommandId::Xgroup)
            } else if eq_ascii_command(cmd, b"XRANGE") {
                Some(CommandId::Xrange)
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
            } else if eq_ascii_command(cmd, b"GEOADD") {
                Some(CommandId::Geoadd)
            } else if eq_ascii_command(cmd, b"GEOPOS") {
                Some(CommandId::Geopos)
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
            } else if eq_ascii_command(cmd, b"GEODIST") {
                Some(CommandId::Geodist)
            } else if eq_ascii_command(cmd, b"GEOHASH") {
                Some(CommandId::Geohash)
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
            } else if eq_ascii_command(cmd, b"XREVRANGE") {
                Some(CommandId::Xrevrange)
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

const GEO_STEP_MAX: u8 = 26;
const GEO_LONG_MIN: f64 = -180.0;
const GEO_LONG_MAX: f64 = 180.0;
const GEO_LAT_MIN: f64 = -85.051_128_78;
const GEO_LAT_MAX: f64 = 85.051_128_78;
const GEO_STANDARD_LAT_MIN: f64 = -90.0;
const GEO_STANDARD_LAT_MAX: f64 = 90.0;
const GEO_EARTH_RADIUS_IN_METERS: f64 = 6_372_797.560_856;
const GEO_BASE32_ALPHABET: &[u8; 32] = b"0123456789bcdefghjkmnpqrstuvwxyz";

#[inline]
fn geo_interleave64(xlo: u32, ylo: u32) -> u64 {
    const B: [u64; 5] = [
        0x5555_5555_5555_5555,
        0x3333_3333_3333_3333,
        0x0F0F_0F0F_0F0F_0F0F,
        0x00FF_00FF_00FF_00FF,
        0x0000_FFFF_0000_FFFF,
    ];
    let mut x = u64::from(xlo);
    let mut y = u64::from(ylo);

    x = (x | (x << 16)) & B[4];
    y = (y | (y << 16)) & B[4];
    x = (x | (x << 8)) & B[3];
    y = (y | (y << 8)) & B[3];
    x = (x | (x << 4)) & B[2];
    y = (y | (y << 4)) & B[2];
    x = (x | (x << 2)) & B[1];
    y = (y | (y << 2)) & B[1];
    x = (x | (x << 1)) & B[0];
    y = (y | (y << 1)) & B[0];

    x | (y << 1)
}

#[inline]
fn geo_deinterleave64(interleaved: u64) -> u64 {
    const B: [u64; 6] = [
        0x5555_5555_5555_5555,
        0x3333_3333_3333_3333,
        0x0F0F_0F0F_0F0F_0F0F,
        0x00FF_00FF_00FF_00FF,
        0x0000_FFFF_0000_FFFF,
        0x0000_0000_FFFF_FFFF,
    ];
    let mut x = interleaved;
    let mut y = interleaved >> 1;

    x &= B[0];
    y &= B[0];
    x = (x | (x >> 1)) & B[1];
    y = (y | (y >> 1)) & B[1];
    x = (x | (x >> 2)) & B[2];
    y = (y | (y >> 2)) & B[2];
    x = (x | (x >> 4)) & B[3];
    y = (y | (y >> 4)) & B[3];
    x = (x | (x >> 8)) & B[4];
    y = (y | (y >> 8)) & B[4];
    x = (x | (x >> 16)) & B[5];
    y = (y | (y >> 16)) & B[5];

    x | (y << 32)
}

#[inline]
fn geo_encode(
    longitude: f64,
    latitude: f64,
    long_min: f64,
    long_max: f64,
    lat_min: f64,
    lat_max: f64,
    step: u8,
) -> Option<u64> {
    if step == 0 || step > 32 {
        return None;
    }
    if longitude < long_min || longitude > long_max || latitude < lat_min || latitude > lat_max {
        return None;
    }

    let scale = (1_u64 << u32::from(step)) as f64;
    let lat_offset = ((latitude - lat_min) / (lat_max - lat_min) * scale) as u32;
    let long_offset = ((longitude - long_min) / (long_max - long_min) * scale) as u32;
    Some(geo_interleave64(lat_offset, long_offset))
}

#[inline]
fn geo_encode_wgs84(longitude: f64, latitude: f64) -> Option<u64> {
    geo_encode(
        longitude,
        latitude,
        GEO_LONG_MIN,
        GEO_LONG_MAX,
        GEO_LAT_MIN,
        GEO_LAT_MAX,
        GEO_STEP_MAX,
    )
}

#[inline]
fn geo_decode(bits: u64, long_min: f64, long_max: f64, lat_min: f64, lat_max: f64) -> (f64, f64) {
    let step = u32::from(GEO_STEP_MAX);
    let scale = (1_u64 << step) as f64;
    let hash_sep = geo_deinterleave64(bits);

    let ilato = hash_sep as u32;
    let ilono = (hash_sep >> 32) as u32;
    let lat_scale = lat_max - lat_min;
    let long_scale = long_max - long_min;

    let lat_lo = lat_min + (f64::from(ilato) / scale) * lat_scale;
    let lat_hi = lat_min + (f64::from(ilato.saturating_add(1)) / scale) * lat_scale;
    let long_lo = long_min + (f64::from(ilono) / scale) * long_scale;
    let long_hi = long_min + (f64::from(ilono.saturating_add(1)) / scale) * long_scale;

    let longitude = ((long_lo + long_hi) / 2.0).clamp(long_min, long_max);
    let latitude = ((lat_lo + lat_hi) / 2.0).clamp(lat_min, lat_max);
    (longitude, latitude)
}

#[inline]
fn geo_decode_score(score: f64) -> Option<(f64, f64)> {
    if !score.is_finite() {
        return None;
    }
    Some(geo_decode(
        score as u64,
        GEO_LONG_MIN,
        GEO_LONG_MAX,
        GEO_LAT_MIN,
        GEO_LAT_MAX,
    ))
}

#[inline]
fn parse_geo_f64(arg: &[u8]) -> Result<f64, RespFrame> {
    let text = std::str::from_utf8(arg)
        .map_err(|_| RespFrame::Error("ERR value is not a valid float".to_string()))?;
    text.parse::<f64>()
        .map_err(|_| RespFrame::Error("ERR value is not a valid float".to_string()))
}

#[inline]
fn geo_invalid_pair_error(longitude: f64, latitude: f64) -> RespFrame {
    RespFrame::Error(format!(
        "ERR invalid longitude,latitude pair {longitude:.6},{latitude:.6}"
    ))
}

#[inline]
fn geo_unit_to_meters(unit: &[u8]) -> Option<f64> {
    if eq_ascii_command(unit, b"M") {
        Some(1.0)
    } else if eq_ascii_command(unit, b"KM") {
        Some(1000.0)
    } else if eq_ascii_command(unit, b"FT") {
        Some(0.3048)
    } else if eq_ascii_command(unit, b"MI") {
        Some(1609.34)
    } else {
        None
    }
}

#[inline]
fn geo_lat_distance_m(lat1: f64, lat2: f64) -> f64 {
    GEO_EARTH_RADIUS_IN_METERS * (lat2.to_radians() - lat1.to_radians()).abs()
}

#[inline]
fn geo_distance_m(lon1: f64, lat1: f64, lon2: f64, lat2: f64) -> f64 {
    let lon1r = lon1.to_radians();
    let lon2r = lon2.to_radians();
    let v = ((lon2r - lon1r) / 2.0).sin();
    if v == 0.0 {
        return geo_lat_distance_m(lat1, lat2);
    }
    let lat1r = lat1.to_radians();
    let lat2r = lat2.to_radians();
    let u = ((lat2r - lat1r) / 2.0).sin();
    let a = (u * u + lat1r.cos() * lat2r.cos() * v * v).clamp(0.0, 1.0);
    2.0 * GEO_EARTH_RADIUS_IN_METERS * a.sqrt().asin()
}

#[inline]
fn geo_distance_reply(distance: f64) -> RespFrame {
    let normalized = if distance == 0.0 { 0.0 } else { distance };
    RespFrame::BulkString(Some(format!("{normalized:.4}").into_bytes()))
}

#[inline]
fn geo_hash_string_from_score(score: f64) -> Option<Vec<u8>> {
    let (longitude, latitude) = geo_decode_score(score)?;
    let bits = geo_encode(
        longitude,
        latitude,
        GEO_LONG_MIN,
        GEO_LONG_MAX,
        GEO_STANDARD_LAT_MIN,
        GEO_STANDARD_LAT_MAX,
        GEO_STEP_MAX,
    )?;

    let mut buf = [0_u8; 11];
    for (i, slot) in buf.iter_mut().enumerate() {
        let idx = if i == 10 {
            0
        } else {
            ((bits >> (52 - ((i + 1) * 5))) & 0x1f) as usize
        };
        *slot = GEO_BASE32_ALPHABET[idx];
    }
    Some(buf.to_vec())
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

fn geoadd(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() < 5 {
        return Err(CommandError::WrongArity("GEOADD"));
    }

    let mut xx = false;
    let mut nx = false;
    let mut ch = false;
    let mut long_idx = 2;
    while long_idx < argv.len() {
        if eq_ascii_command(&argv[long_idx], b"NX") {
            nx = true;
        } else if eq_ascii_command(&argv[long_idx], b"XX") {
            xx = true;
        } else if eq_ascii_command(&argv[long_idx], b"CH") {
            ch = true;
        } else {
            break;
        }
        long_idx += 1;
    }

    if xx && nx {
        return Err(CommandError::SyntaxError);
    }

    let remaining = argv.len().saturating_sub(long_idx);
    if remaining == 0 {
        return Err(CommandError::WrongArity("GEOADD"));
    }
    if !remaining.is_multiple_of(3) {
        return Err(CommandError::SyntaxError);
    }

    let mut pairs = Vec::with_capacity(remaining / 3);
    let mut idx = long_idx;
    while idx + 2 < argv.len() {
        let longitude = match parse_geo_f64(&argv[idx]) {
            Ok(value) => value,
            Err(reply) => return Ok(reply),
        };
        let latitude = match parse_geo_f64(&argv[idx + 1]) {
            Ok(value) => value,
            Err(reply) => return Ok(reply),
        };
        if !(GEO_LONG_MIN..=GEO_LONG_MAX).contains(&longitude)
            || !(GEO_LAT_MIN..=GEO_LAT_MAX).contains(&latitude)
        {
            return Ok(geo_invalid_pair_error(longitude, latitude));
        }
        let Some(bits) = geo_encode_wgs84(longitude, latitude) else {
            return Ok(geo_invalid_pair_error(longitude, latitude));
        };
        pairs.push((bits as f64, argv[idx + 2].clone()));
        idx += 3;
    }

    let mut added = 0_i64;
    let mut changed = 0_i64;
    for (score, member) in pairs {
        let existing = store.zscore(&argv[1], &member, now_ms)?;
        if nx && existing.is_some() {
            continue;
        }
        if xx && existing.is_none() {
            continue;
        }

        store.zadd(&argv[1], &[(score, member)], now_ms)?;
        match existing {
            Some(old_score) => {
                if old_score != score {
                    changed += 1;
                }
            }
            None => {
                added += 1;
                changed += 1;
            }
        }
    }

    Ok(RespFrame::Integer(if ch { changed } else { added }))
}

fn geohash(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() < 2 {
        return Err(CommandError::WrongArity("GEOHASH"));
    }
    let mut frames = Vec::with_capacity(argv.len().saturating_sub(2));
    for member in &argv[2..] {
        let frame = match store.zscore(&argv[1], member, now_ms)? {
            Some(score) => match geo_hash_string_from_score(score) {
                Some(hash) => RespFrame::BulkString(Some(hash)),
                None => RespFrame::BulkString(None),
            },
            None => RespFrame::BulkString(None),
        };
        frames.push(frame);
    }
    Ok(RespFrame::Array(Some(frames)))
}

fn geopos(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() < 2 {
        return Err(CommandError::WrongArity("GEOPOS"));
    }
    let mut frames = Vec::with_capacity(argv.len().saturating_sub(2));
    for member in &argv[2..] {
        let frame = match store.zscore(&argv[1], member, now_ms)? {
            Some(score) => match geo_decode_score(score) {
                Some((longitude, latitude)) => RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(longitude.to_string().into_bytes())),
                    RespFrame::BulkString(Some(latitude.to_string().into_bytes())),
                ])),
                None => RespFrame::Array(None),
            },
            None => RespFrame::Array(None),
        };
        frames.push(frame);
    }
    Ok(RespFrame::Array(Some(frames)))
}

fn geodist(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 4 && argv.len() != 5 {
        return Err(CommandError::WrongArity("GEODIST"));
    }
    let to_meter = if argv.len() == 5 {
        match geo_unit_to_meters(&argv[4]) {
            Some(unit) => unit,
            None => {
                return Ok(RespFrame::Error(
                    "ERR unsupported unit provided. please use M, KM, FT, MI".to_string(),
                ));
            }
        }
    } else {
        1.0
    };

    let score1 = store.zscore(&argv[1], &argv[2], now_ms)?;
    let score2 = store.zscore(&argv[1], &argv[3], now_ms)?;
    let (Some(score1), Some(score2)) = (score1, score2) else {
        return Ok(RespFrame::BulkString(None));
    };

    let Some((lon1, lat1)) = geo_decode_score(score1) else {
        return Ok(RespFrame::BulkString(None));
    };
    let Some((lon2, lat2)) = geo_decode_score(score2) else {
        return Ok(RespFrame::BulkString(None));
    };

    let distance = geo_distance_m(lon1, lat1, lon2, lat2) / to_meter;
    Ok(geo_distance_reply(distance))
}

fn parse_stream_id(arg: &[u8]) -> Result<StreamId, RespFrame> {
    let text = std::str::from_utf8(arg).map_err(|_| {
        RespFrame::Error("ERR Invalid stream ID specified as stream command argument".to_string())
    })?;
    let Some((ms, seq)) = text.split_once('-') else {
        return Err(RespFrame::Error(
            "ERR Invalid stream ID specified as stream command argument".to_string(),
        ));
    };
    let ms = ms.parse::<u64>().map_err(|_| {
        RespFrame::Error("ERR Invalid stream ID specified as stream command argument".to_string())
    })?;
    let seq = seq.parse::<u64>().map_err(|_| {
        RespFrame::Error("ERR Invalid stream ID specified as stream command argument".to_string())
    })?;
    Ok((ms, seq))
}

#[inline]
fn format_stream_id(id: StreamId) -> Vec<u8> {
    format!("{}-{}", id.0, id.1).into_bytes()
}

#[inline]
fn next_auto_stream_id(last_id: Option<StreamId>, now_ms: u64) -> StreamId {
    let mut id = match last_id {
        Some((last_ms, last_seq)) => {
            if now_ms > last_ms {
                (now_ms, 0)
            } else {
                (last_ms, last_seq.saturating_add(1))
            }
        }
        None => (now_ms, 0),
    };
    if id == (0, 0) {
        id.1 = 1;
    }
    id
}

fn parse_stream_range_bound(arg: &[u8], is_start: bool) -> Result<StreamId, RespFrame> {
    if arg == b"-" {
        return Ok((0, 0));
    }
    if arg == b"+" {
        return Ok((u64::MAX, u64::MAX));
    }

    if let Some((ms, seq)) = std::str::from_utf8(arg)
        .ok()
        .and_then(|text| text.split_once('-'))
    {
        let ms = ms.parse::<u64>().map_err(|_| {
            RespFrame::Error(
                "ERR Invalid stream ID specified as stream command argument".to_string(),
            )
        })?;
        let seq = seq.parse::<u64>().map_err(|_| {
            RespFrame::Error(
                "ERR Invalid stream ID specified as stream command argument".to_string(),
            )
        })?;
        return Ok((ms, seq));
    }

    let ms = parse_u64_arg(arg).map_err(|_| {
        RespFrame::Error("ERR Invalid stream ID specified as stream command argument".to_string())
    })?;
    Ok((ms, if is_start { 0 } else { u64::MAX }))
}

fn parse_xread_id(arg: &[u8]) -> Result<StreamId, RespFrame> {
    if arg == b"-" || arg == b"+" || arg == b"$" {
        return Err(RespFrame::Error(
            "ERR Invalid stream ID specified as stream command argument".to_string(),
        ));
    }
    parse_stream_range_bound(arg, true)
}

fn xadd(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() < 5 || !(argv.len() - 3).is_multiple_of(2) {
        return Err(CommandError::WrongArity("XADD"));
    }

    let mut fields = Vec::with_capacity((argv.len() - 3) / 2);
    let mut idx = 3;
    while idx + 1 < argv.len() {
        fields.push((argv[idx].clone(), argv[idx + 1].clone()));
        idx += 2;
    }

    let last_id = store.xlast_id(&argv[1], now_ms)?;
    let id = if eq_ascii_command(&argv[2], b"*") {
        next_auto_stream_id(last_id, now_ms)
    } else {
        let id = match parse_stream_id(&argv[2]) {
            Ok(id) => id,
            Err(reply) => return Ok(reply),
        };
        if id == (0, 0) {
            return Ok(RespFrame::Error(
                "ERR The ID specified in XADD must be greater than 0-0".to_string(),
            ));
        }
        if let Some(last_id) = last_id
            && id <= last_id
        {
            return Ok(RespFrame::Error(
                "ERR The ID specified in XADD is equal or smaller than the target stream top item"
                    .to_string(),
            ));
        }
        id
    };

    store.xadd(&argv[1], id, &fields, now_ms)?;
    Ok(RespFrame::BulkString(Some(format_stream_id(id))))
}

fn xlen(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 2 {
        return Err(CommandError::WrongArity("XLEN"));
    }
    let len = store.xlen(&argv[1], now_ms)?;
    Ok(RespFrame::Integer(i64::try_from(len).unwrap_or(i64::MAX)))
}

fn xdel(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() < 3 {
        return Err(CommandError::WrongArity("XDEL"));
    }

    let mut ids = Vec::with_capacity(argv.len() - 2);
    for arg in &argv[2..] {
        let id = match parse_stream_id(arg) {
            Ok(id) => id,
            Err(reply) => return Ok(reply),
        };
        ids.push(id);
    }

    let removed = store.xdel(&argv[1], &ids, now_ms)?;
    Ok(RespFrame::Integer(
        i64::try_from(removed).unwrap_or(i64::MAX),
    ))
}

fn xtrim(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 4 && argv.len() != 5 {
        return Err(CommandError::WrongArity("XTRIM"));
    }
    if !eq_ascii_command(&argv[2], b"MAXLEN") {
        return Err(CommandError::SyntaxError);
    }

    let threshold = if argv.len() == 5 {
        if !eq_ascii_command(&argv[3], b"=") {
            return Err(CommandError::SyntaxError);
        }
        &argv[4]
    } else {
        &argv[3]
    };

    let max_len_raw = parse_i64_arg(threshold)?;
    if max_len_raw < 0 {
        return Err(CommandError::InvalidInteger);
    }
    let max_len = usize::try_from(max_len_raw).unwrap_or(usize::MAX);
    let removed = store.xtrim(&argv[1], max_len, now_ms)?;
    Ok(RespFrame::Integer(
        i64::try_from(removed).unwrap_or(i64::MAX),
    ))
}

fn stream_record_to_frame(id: StreamId, fields: Vec<(Vec<u8>, Vec<u8>)>) -> RespFrame {
    let mut field_frames = Vec::with_capacity(fields.len().saturating_mul(2));
    for (field, value) in fields {
        field_frames.push(RespFrame::BulkString(Some(field)));
        field_frames.push(RespFrame::BulkString(Some(value)));
    }
    RespFrame::Array(Some(vec![
        RespFrame::BulkString(Some(format_stream_id(id))),
        RespFrame::Array(Some(field_frames)),
    ]))
}

fn stream_group_info_to_frame(name: Vec<u8>, last_delivered_id: StreamId) -> RespFrame {
    RespFrame::Array(Some(vec![
        RespFrame::BulkString(Some(b"name".to_vec())),
        RespFrame::BulkString(Some(name)),
        RespFrame::BulkString(Some(b"consumers".to_vec())),
        RespFrame::Integer(0),
        RespFrame::BulkString(Some(b"pending".to_vec())),
        RespFrame::Integer(0),
        RespFrame::BulkString(Some(b"last-delivered-id".to_vec())),
        RespFrame::BulkString(Some(format_stream_id(last_delivered_id))),
        RespFrame::BulkString(Some(b"entries-read".to_vec())),
        RespFrame::BulkString(None),
        RespFrame::BulkString(Some(b"lag".to_vec())),
        RespFrame::BulkString(None),
    ]))
}

fn xread(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() < 4 {
        return Err(CommandError::WrongArity("XREAD"));
    }

    let mut idx = 1usize;
    let mut count: Option<usize> = None;
    while idx < argv.len() {
        if eq_ascii_command(&argv[idx], b"COUNT") {
            if idx + 1 >= argv.len() {
                return Err(CommandError::WrongArity("XREAD"));
            }
            let parsed = parse_i64_arg(&argv[idx + 1])?;
            if parsed < 0 {
                return Err(CommandError::InvalidInteger);
            }
            count = Some(usize::try_from(parsed).unwrap_or(usize::MAX));
            idx += 2;
            continue;
        }
        if eq_ascii_command(&argv[idx], b"BLOCK") {
            return Err(CommandError::SyntaxError);
        }
        break;
    }

    if idx >= argv.len() || !eq_ascii_command(&argv[idx], b"STREAMS") {
        return Err(CommandError::SyntaxError);
    }
    idx += 1;

    let tail = argv.len().saturating_sub(idx);
    if tail < 2 || !tail.is_multiple_of(2) {
        return Err(CommandError::WrongArity("XREAD"));
    }
    let stream_count = tail / 2;
    let keys = &argv[idx..idx + stream_count];
    let ids = &argv[idx + stream_count..];

    let mut out = Vec::new();
    for (key, id_arg) in keys.iter().zip(ids.iter()) {
        let cursor = if id_arg.as_slice() == b"$" {
            store.xlast_id(key, now_ms)?.unwrap_or((u64::MAX, u64::MAX))
        } else {
            match parse_xread_id(id_arg) {
                Ok(id) => id,
                Err(reply) => return Ok(reply),
            }
        };

        let records = store.xread(key, cursor, count, now_ms)?;
        if records.is_empty() {
            continue;
        }

        let mut entry_frames = Vec::with_capacity(records.len());
        for (id, fields) in records {
            entry_frames.push(stream_record_to_frame(id, fields));
        }

        out.push(RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(key.clone())),
            RespFrame::Array(Some(entry_frames)),
        ])));
    }

    if out.is_empty() {
        Ok(RespFrame::Array(None))
    } else {
        Ok(RespFrame::Array(Some(out)))
    }
}

fn xgroup(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() < 2 {
        return Err(CommandError::WrongArity("XGROUP"));
    }

    if eq_ascii_command(&argv[1], b"CREATE") {
        if argv.len() != 5 && argv.len() != 6 {
            return Err(CommandError::WrongArity("XGROUP"));
        }

        let mkstream = if argv.len() == 6 {
            if !eq_ascii_command(&argv[5], b"MKSTREAM") {
                return Err(CommandError::SyntaxError);
            }
            true
        } else {
            false
        };

        let start_id = if eq_ascii_command(&argv[4], b"$") {
            store.xlast_id(&argv[2], now_ms)?.unwrap_or((0, 0))
        } else {
            if eq_ascii_command(&argv[4], b"-") || eq_ascii_command(&argv[4], b"+") {
                return Ok(RespFrame::Error(
                    "ERR Invalid stream ID specified as stream command argument".to_string(),
                ));
            }
            match parse_stream_range_bound(&argv[4], true) {
                Ok(id) => id,
                Err(reply) => return Ok(reply),
            }
        };

        return match store.xgroup_create(&argv[2], &argv[3], start_id, mkstream, now_ms) {
            Ok(true) => Ok(RespFrame::SimpleString("OK".to_string())),
            Ok(false) => Ok(RespFrame::Error(
                "BUSYGROUP Consumer Group name already exists".to_string(),
            )),
            Err(StoreError::KeyNotFound) => Err(CommandError::NoSuchKey),
            Err(err) => Err(CommandError::Store(err)),
        };
    }

    if eq_ascii_command(&argv[1], b"DESTROY") {
        if argv.len() != 4 {
            return Err(CommandError::WrongArity("XGROUP"));
        }
        return match store.xgroup_destroy(&argv[2], &argv[3], now_ms) {
            Ok(removed) => Ok(RespFrame::Integer(if removed { 1 } else { 0 })),
            Err(err) => Err(CommandError::Store(err)),
        };
    }

    if eq_ascii_command(&argv[1], b"SETID") {
        if argv.len() != 5 {
            return Err(CommandError::WrongArity("XGROUP"));
        }
        let last_delivered_id = if eq_ascii_command(&argv[4], b"$") {
            store.xlast_id(&argv[2], now_ms)?.unwrap_or((0, 0))
        } else {
            if eq_ascii_command(&argv[4], b"-") || eq_ascii_command(&argv[4], b"+") {
                return Ok(RespFrame::Error(
                    "ERR Invalid stream ID specified as stream command argument".to_string(),
                ));
            }
            match parse_stream_range_bound(&argv[4], true) {
                Ok(id) => id,
                Err(reply) => return Ok(reply),
            }
        };
        return match store.xgroup_setid(&argv[2], &argv[3], last_delivered_id, now_ms) {
            Ok(true) => Ok(RespFrame::SimpleString("OK".to_string())),
            Ok(false) => {
                let key = String::from_utf8_lossy(&argv[2]);
                let group = String::from_utf8_lossy(&argv[3]);
                Ok(RespFrame::Error(format!(
                    "NOGROUP No such key '{key}' or consumer group '{group}' in XGROUP command"
                )))
            }
            Err(StoreError::KeyNotFound) => Err(CommandError::NoSuchKey),
            Err(err) => Err(CommandError::Store(err)),
        };
    }

    Err(CommandError::SyntaxError)
}

fn xinfo(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() < 2 {
        return Err(CommandError::WrongArity("XINFO"));
    }
    if eq_ascii_command(&argv[1], b"GROUPS") {
        if argv.len() != 3 {
            return Err(CommandError::WrongArity("XINFO"));
        }
        let Some(groups) = store.xinfo_groups(&argv[2], now_ms)? else {
            return Err(CommandError::NoSuchKey);
        };
        let mut out = Vec::with_capacity(groups.len());
        for (name, last_delivered_id) in groups {
            out.push(stream_group_info_to_frame(name, last_delivered_id));
        }
        return Ok(RespFrame::Array(Some(out)));
    }
    if !eq_ascii_command(&argv[1], b"STREAM") {
        return Err(CommandError::SyntaxError);
    }
    if argv.len() != 3 {
        return Err(CommandError::WrongArity("XINFO"));
    }

    let Some((len, first, last)) = store.xinfo_stream(&argv[2], now_ms)? else {
        return Err(CommandError::NoSuchKey);
    };

    let last_generated_id = last
        .as_ref()
        .map(|(id, _)| format_stream_id(*id))
        .unwrap_or_else(|| b"0-0".to_vec());
    let recorded_first_entry_id = first
        .as_ref()
        .map(|(id, _)| format_stream_id(*id))
        .unwrap_or_else(|| b"0-0".to_vec());
    let len_i64 = i64::try_from(len).unwrap_or(i64::MAX);
    let group_count = store
        .xinfo_groups(&argv[2], now_ms)?
        .map(|groups| i64::try_from(groups.len()).unwrap_or(i64::MAX))
        .unwrap_or(0);

    let first_entry = first
        .map(|(id, fields)| stream_record_to_frame(id, fields))
        .unwrap_or(RespFrame::BulkString(None));
    let last_entry = last
        .map(|(id, fields)| stream_record_to_frame(id, fields))
        .unwrap_or(RespFrame::BulkString(None));

    Ok(RespFrame::Array(Some(vec![
        RespFrame::BulkString(Some(b"length".to_vec())),
        RespFrame::Integer(len_i64),
        // Placeholder radix/listpack metrics until internal stream-node accounting lands.
        RespFrame::BulkString(Some(b"radix-tree-keys".to_vec())),
        RespFrame::Integer(if len == 0 { 0 } else { 1 }),
        RespFrame::BulkString(Some(b"radix-tree-nodes".to_vec())),
        RespFrame::Integer(if len == 0 { 0 } else { 2 }),
        RespFrame::BulkString(Some(b"last-generated-id".to_vec())),
        RespFrame::BulkString(Some(last_generated_id)),
        RespFrame::BulkString(Some(b"max-deleted-entry-id".to_vec())),
        RespFrame::BulkString(Some(b"0-0".to_vec())),
        RespFrame::BulkString(Some(b"entries-added".to_vec())),
        RespFrame::Integer(len_i64),
        RespFrame::BulkString(Some(b"recorded-first-entry-id".to_vec())),
        RespFrame::BulkString(Some(recorded_first_entry_id)),
        RespFrame::BulkString(Some(b"groups".to_vec())),
        RespFrame::Integer(group_count),
        RespFrame::BulkString(Some(b"first-entry".to_vec())),
        first_entry,
        RespFrame::BulkString(Some(b"last-entry".to_vec())),
        last_entry,
    ])))
}

fn xrange(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 4 && argv.len() != 6 {
        return Err(CommandError::WrongArity("XRANGE"));
    }

    let start = match parse_stream_range_bound(&argv[2], true) {
        Ok(v) => v,
        Err(reply) => return Ok(reply),
    };
    let end = match parse_stream_range_bound(&argv[3], false) {
        Ok(v) => v,
        Err(reply) => return Ok(reply),
    };

    let count = if argv.len() == 6 {
        if !eq_ascii_command(&argv[4], b"COUNT") {
            return Err(CommandError::SyntaxError);
        }
        let parsed = parse_i64_arg(&argv[5])?;
        if parsed < 0 {
            return Err(CommandError::InvalidInteger);
        }
        Some(usize::try_from(parsed).unwrap_or(usize::MAX))
    } else {
        None
    };

    if matches!(count, Some(0)) {
        return Ok(RespFrame::Array(Some(vec![])));
    }

    let records = store.xrange(&argv[1], start, end, count, now_ms)?;
    let mut out = Vec::with_capacity(records.len());
    for (id, fields) in records {
        let mut field_frames = Vec::with_capacity(fields.len().saturating_mul(2));
        for (field, value) in fields {
            field_frames.push(RespFrame::BulkString(Some(field)));
            field_frames.push(RespFrame::BulkString(Some(value)));
        }
        out.push(RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(format_stream_id(id))),
            RespFrame::Array(Some(field_frames)),
        ])));
    }
    Ok(RespFrame::Array(Some(out)))
}

fn xrevrange(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 4 && argv.len() != 6 {
        return Err(CommandError::WrongArity("XREVRANGE"));
    }

    let end = match parse_stream_range_bound(&argv[2], false) {
        Ok(v) => v,
        Err(reply) => return Ok(reply),
    };
    let start = match parse_stream_range_bound(&argv[3], true) {
        Ok(v) => v,
        Err(reply) => return Ok(reply),
    };

    let count = if argv.len() == 6 {
        if !eq_ascii_command(&argv[4], b"COUNT") {
            return Err(CommandError::SyntaxError);
        }
        let parsed = parse_i64_arg(&argv[5])?;
        if parsed < 0 {
            return Err(CommandError::InvalidInteger);
        }
        Some(usize::try_from(parsed).unwrap_or(usize::MAX))
    } else {
        None
    };

    if matches!(count, Some(0)) {
        return Ok(RespFrame::Array(Some(vec![])));
    }

    let records = store.xrevrange(&argv[1], end, start, count, now_ms)?;
    let mut out = Vec::with_capacity(records.len());
    for (id, fields) in records {
        let mut field_frames = Vec::with_capacity(fields.len().saturating_mul(2));
        for (field, value) in fields {
            field_frames.push(RespFrame::BulkString(Some(field)));
            field_frames.push(RespFrame::BulkString(Some(value)));
        }
        out.push(RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(format_stream_id(id))),
            RespFrame::Array(Some(field_frames)),
        ])));
    }
    Ok(RespFrame::Array(Some(out)))
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
    if (!eq_ascii_command(&argv[3], b"LEFT") && !eq_ascii_command(&argv[3], b"RIGHT"))
        || (!eq_ascii_command(&argv[4], b"LEFT") && !eq_ascii_command(&argv[4], b"RIGHT"))
    {
        return Err(CommandError::SyntaxError);
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

fn getex(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
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

fn smismember(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
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

fn bitop(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
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
    let keys: Vec<&[u8]> = argv[3..3 + numkeys].iter().map(|v| v.as_slice()).collect();
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
    let keys: Vec<&[u8]> = argv[3..3 + numkeys].iter().map(|v| v.as_slice()).collect();
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
        Ok(RespFrame::Error("ERR DB index is out of range".to_string()))
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

fn randomkey(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
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

fn scan(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() < 2 {
        return Err(CommandError::WrongArity("SCAN"));
    }
    let cursor = std::str::from_utf8(&argv[1])
        .map_err(|_| CommandError::InvalidInteger)?
        .parse::<u64>()
        .map_err(|_| CommandError::InvalidInteger)?;

    let (pattern, count) = parse_scan_args(argv, 2);
    let (next_cursor, keys) = store.scan(cursor, pattern.as_deref(), count, now_ms);

    let key_frames: Vec<RespFrame> = keys
        .into_iter()
        .map(|k| RespFrame::BulkString(Some(k)))
        .collect();
    Ok(RespFrame::Array(Some(vec![
        RespFrame::BulkString(Some(next_cursor.to_string().into_bytes())),
        RespFrame::Array(Some(key_frames)),
    ])))
}

fn hscan(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
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

fn sscan(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
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

fn zscan(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
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
        items.push(RespFrame::BulkString(Some(score.to_string().into_bytes())));
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

fn touch(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
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

fn copy_cmd(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
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
        if eq_ascii_command(cmd, b"GEOADD") {
            return Some(CommandId::Geoadd);
        }
        if eq_ascii_command(cmd, b"GEOPOS") {
            return Some(CommandId::Geopos);
        }
        if eq_ascii_command(cmd, b"GEODIST") {
            return Some(CommandId::Geodist);
        }
        if eq_ascii_command(cmd, b"GEOHASH") {
            return Some(CommandId::Geohash);
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
        if eq_ascii_command(cmd, b"XADD") {
            return Some(CommandId::Xadd);
        }
        if eq_ascii_command(cmd, b"XLEN") {
            return Some(CommandId::Xlen);
        }
        if eq_ascii_command(cmd, b"XDEL") {
            return Some(CommandId::Xdel);
        }
        if eq_ascii_command(cmd, b"XREAD") {
            return Some(CommandId::Xread);
        }
        if eq_ascii_command(cmd, b"XINFO") {
            return Some(CommandId::Xinfo);
        }
        if eq_ascii_command(cmd, b"XGROUP") {
            return Some(CommandId::Xgroup);
        }
        if eq_ascii_command(cmd, b"XTRIM") {
            return Some(CommandId::Xtrim);
        }
        if eq_ascii_command(cmd, b"XRANGE") {
            return Some(CommandId::Xrange);
        }
        if eq_ascii_command(cmd, b"XREVRANGE") {
            return Some(CommandId::Xrevrange);
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
            b"GEOADD",
            b"GEOPOS",
            b"GEODIST",
            b"GEOHASH",
            b"zadd",
            b"zRangeByScore",
            b"geoadd",
            b"geoPos",
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
            b"XDEL",
            b"xdel",
            b"XREAD",
            b"xread",
            b"XINFO",
            b"xinfo",
            b"XGROUP",
            b"xgroup",
            b"XTRIM",
            b"xtrim",
            b"XRANGE",
            b"xrange",
            b"XREVRANGE",
            b"xrevrange",
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
    fn ltrim_command() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"RPUSH".to_vec(),
                b"l".to_vec(),
                b"a".to_vec(),
                b"b".to_vec(),
                b"c".to_vec(),
                b"d".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("rpush");

        let out = dispatch_argv(
            &[
                b"LTRIM".to_vec(),
                b"l".to_vec(),
                b"1".to_vec(),
                b"2".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("ltrim");
        assert_eq!(out, RespFrame::SimpleString("OK".to_string()));

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
        .expect("lrange");
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"b".to_vec())),
                RespFrame::BulkString(Some(b"c".to_vec())),
            ]))
        );

        let out = dispatch_argv(
            &[
                b"LTRIM".to_vec(),
                b"l".to_vec(),
                b"10".to_vec(),
                b"12".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("ltrim to empty");
        assert_eq!(out, RespFrame::SimpleString("OK".to_string()));

        let out = dispatch_argv(&[b"TYPE".to_vec(), b"l".to_vec()], &mut store, 0).expect("type");
        assert_eq!(out, RespFrame::SimpleString("none".to_string()));
    }

    #[test]
    fn lpushx_rpushx_commands() {
        let mut store = Store::new();

        let out = dispatch_argv(
            &[b"LPUSHX".to_vec(), b"missing".to_vec(), b"x".to_vec()],
            &mut store,
            0,
        )
        .expect("lpushx missing");
        assert_eq!(out, RespFrame::Integer(0));

        dispatch_argv(
            &[b"RPUSH".to_vec(), b"l".to_vec(), b"a".to_vec()],
            &mut store,
            0,
        )
        .expect("rpush");

        let out = dispatch_argv(
            &[
                b"LPUSHX".to_vec(),
                b"l".to_vec(),
                b"b".to_vec(),
                b"c".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("lpushx existing");
        assert_eq!(out, RespFrame::Integer(3));

        let out = dispatch_argv(
            &[
                b"RPUSHX".to_vec(),
                b"l".to_vec(),
                b"d".to_vec(),
                b"e".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("rpushx existing");
        assert_eq!(out, RespFrame::Integer(5));

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
        .expect("lrange");
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"c".to_vec())),
                RespFrame::BulkString(Some(b"b".to_vec())),
                RespFrame::BulkString(Some(b"a".to_vec())),
                RespFrame::BulkString(Some(b"d".to_vec())),
                RespFrame::BulkString(Some(b"e".to_vec())),
            ]))
        );
    }

    #[test]
    fn lmove_command() {
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
        .expect("rpush src");
        dispatch_argv(
            &[b"RPUSH".to_vec(), b"dst".to_vec(), b"x".to_vec()],
            &mut store,
            0,
        )
        .expect("rpush dst");

        let out = dispatch_argv(
            &[
                b"LMOVE".to_vec(),
                b"src".to_vec(),
                b"dst".to_vec(),
                b"LEFT".to_vec(),
                b"RIGHT".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("lmove left-right");
        assert_eq!(out, RespFrame::BulkString(Some(b"a".to_vec())));

        let out = dispatch_argv(
            &[
                b"LMOVE".to_vec(),
                b"src".to_vec(),
                b"dst".to_vec(),
                b"RIGHT".to_vec(),
                b"LEFT".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("lmove right-left");
        assert_eq!(out, RespFrame::BulkString(Some(b"c".to_vec())));

        let out = dispatch_argv(
            &[
                b"LRANGE".to_vec(),
                b"src".to_vec(),
                b"0".to_vec(),
                b"-1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("lrange src");
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![RespFrame::BulkString(Some(b"b".to_vec()))]))
        );

        let out = dispatch_argv(
            &[
                b"LRANGE".to_vec(),
                b"dst".to_vec(),
                b"0".to_vec(),
                b"-1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("lrange dst");
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"c".to_vec())),
                RespFrame::BulkString(Some(b"x".to_vec())),
                RespFrame::BulkString(Some(b"a".to_vec())),
            ]))
        );
    }

    #[test]
    fn lmove_rejects_invalid_directions() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"RPUSH".to_vec(),
                b"src".to_vec(),
                b"a".to_vec(),
                b"b".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("rpush");

        let err = dispatch_argv(
            &[
                b"LMOVE".to_vec(),
                b"src".to_vec(),
                b"dst".to_vec(),
                b"MIDDLE".to_vec(),
                b"LEFT".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect_err("invalid wherefrom");
        assert!(matches!(err, CommandError::SyntaxError));

        let out = dispatch_argv(
            &[
                b"LRANGE".to_vec(),
                b"src".to_vec(),
                b"0".to_vec(),
                b"-1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("lrange after invalid lmove");
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"a".to_vec())),
                RespFrame::BulkString(Some(b"b".to_vec())),
            ]))
        );
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

    #[test]
    fn geoadd_geodist_geohash_and_geopos() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[
                b"GEOADD".to_vec(),
                b"geo".to_vec(),
                b"13.361389".to_vec(),
                b"38.115556".to_vec(),
                b"Palermo".to_vec(),
                b"15.087269".to_vec(),
                b"37.502669".to_vec(),
                b"Catania".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("geoadd");
        assert_eq!(out, RespFrame::Integer(2));

        let dist = dispatch_argv(
            &[
                b"GEODIST".to_vec(),
                b"geo".to_vec(),
                b"Palermo".to_vec(),
                b"Catania".to_vec(),
                b"km".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("geodist");
        assert_eq!(dist, RespFrame::BulkString(Some(b"166.2742".to_vec())));

        let hashes = dispatch_argv(
            &[
                b"GEOHASH".to_vec(),
                b"geo".to_vec(),
                b"Palermo".to_vec(),
                b"Catania".to_vec(),
                b"Missing".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("geohash");
        assert_eq!(
            hashes,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"sqc8b49rny0".to_vec())),
                RespFrame::BulkString(Some(b"sqdtr74hyu0".to_vec())),
                RespFrame::BulkString(None),
            ]))
        );

        let pos = dispatch_argv(
            &[
                b"GEOPOS".to_vec(),
                b"geo".to_vec(),
                b"Palermo".to_vec(),
                b"Catania".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("geopos");
        let RespFrame::Array(Some(items)) = pos else {
            panic!("geopos should return array");
        };
        assert_eq!(items.len(), 2);
        for item in items {
            let RespFrame::Array(Some(coords)) = item else {
                panic!("geopos entry should be coord array");
            };
            assert_eq!(coords.len(), 2);
            let RespFrame::BulkString(Some(longitude_raw)) = &coords[0] else {
                panic!("geopos longitude should be bulk");
            };
            let RespFrame::BulkString(Some(latitude_raw)) = &coords[1] else {
                panic!("geopos latitude should be bulk");
            };
            let longitude = std::str::from_utf8(longitude_raw)
                .expect("longitude utf8")
                .parse::<f64>()
                .expect("longitude float");
            let latitude = std::str::from_utf8(latitude_raw)
                .expect("latitude utf8")
                .parse::<f64>()
                .expect("latitude float");
            assert!((-180.0..=180.0).contains(&longitude));
            assert!((-85.051_128_78..=85.051_128_78).contains(&latitude));
        }
    }

    #[test]
    fn geoadd_options_and_errors() {
        let mut store = Store::new();

        let out = dispatch_argv(
            &[
                b"GEOADD".to_vec(),
                b"geo".to_vec(),
                b"CH".to_vec(),
                b"13.361389".to_vec(),
                b"38.115556".to_vec(),
                b"Palermo".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("geoadd ch initial");
        assert_eq!(out, RespFrame::Integer(1));

        let unchanged = dispatch_argv(
            &[
                b"GEOADD".to_vec(),
                b"geo".to_vec(),
                b"CH".to_vec(),
                b"13.361389".to_vec(),
                b"38.115556".to_vec(),
                b"Palermo".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("geoadd same coords");
        assert_eq!(unchanged, RespFrame::Integer(0));

        let changed = dispatch_argv(
            &[
                b"GEOADD".to_vec(),
                b"geo".to_vec(),
                b"CH".to_vec(),
                b"13.362".to_vec(),
                b"38.115".to_vec(),
                b"Palermo".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("geoadd changed coords");
        assert_eq!(changed, RespFrame::Integer(1));

        let nx_skip = dispatch_argv(
            &[
                b"GEOADD".to_vec(),
                b"geo".to_vec(),
                b"NX".to_vec(),
                b"13.1".to_vec(),
                b"38.1".to_vec(),
                b"Palermo".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("geoadd nx skip");
        assert_eq!(nx_skip, RespFrame::Integer(0));

        let xx_skip = dispatch_argv(
            &[
                b"GEOADD".to_vec(),
                b"geo".to_vec(),
                b"XX".to_vec(),
                b"15.087269".to_vec(),
                b"37.502669".to_vec(),
                b"Catania".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("geoadd xx skip on missing");
        assert_eq!(xx_skip, RespFrame::Integer(0));

        let invalid = dispatch_argv(
            &[
                b"GEOADD".to_vec(),
                b"geo".to_vec(),
                b"181".to_vec(),
                b"0".to_vec(),
                b"bad".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("geoadd invalid pair");
        assert_eq!(
            invalid,
            RespFrame::Error("ERR invalid longitude,latitude pair 181.000000,0.000000".to_string())
        );

        let syntax = dispatch_argv(
            &[
                b"GEOADD".to_vec(),
                b"geo".to_vec(),
                b"NX".to_vec(),
                b"XX".to_vec(),
                b"13.0".to_vec(),
                b"38.0".to_vec(),
                b"x".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect_err("geoadd nx+xx should be syntax error");
        assert!(matches!(syntax, CommandError::SyntaxError));
    }

    #[test]
    fn geodist_units_and_missing_members() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"GEOADD".to_vec(),
                b"geo".to_vec(),
                b"13.361389".to_vec(),
                b"38.115556".to_vec(),
                b"Palermo".to_vec(),
                b"15.087269".to_vec(),
                b"37.502669".to_vec(),
                b"Catania".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("geoadd");

        let meters = dispatch_argv(
            &[
                b"GEODIST".to_vec(),
                b"geo".to_vec(),
                b"Palermo".to_vec(),
                b"Catania".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("geodist meters");
        let RespFrame::BulkString(Some(distance_raw)) = meters else {
            panic!("geodist should return bulk distance");
        };
        let meters_value = std::str::from_utf8(&distance_raw)
            .expect("distance utf8")
            .parse::<f64>()
            .expect("distance float");
        assert!(meters_value > 166_000.0);

        let invalid_unit = dispatch_argv(
            &[
                b"GEODIST".to_vec(),
                b"geo".to_vec(),
                b"Palermo".to_vec(),
                b"Catania".to_vec(),
                b"yards".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("geodist invalid unit");
        assert_eq!(
            invalid_unit,
            RespFrame::Error("ERR unsupported unit provided. please use M, KM, FT, MI".to_string())
        );

        let missing = dispatch_argv(
            &[
                b"GEODIST".to_vec(),
                b"geo".to_vec(),
                b"Palermo".to_vec(),
                b"Missing".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("geodist missing");
        assert_eq!(missing, RespFrame::BulkString(None));
    }

    #[test]
    fn xadd_xlen_and_type_roundtrip() {
        let mut store = Store::new();

        let first = dispatch_argv(
            &[
                b"XADD".to_vec(),
                b"stream".to_vec(),
                b"*".to_vec(),
                b"field1".to_vec(),
                b"value1".to_vec(),
            ],
            &mut store,
            1_000,
        )
        .expect("xadd first");
        assert_eq!(first, RespFrame::BulkString(Some(b"1000-0".to_vec())));

        let second = dispatch_argv(
            &[
                b"XADD".to_vec(),
                b"stream".to_vec(),
                b"*".to_vec(),
                b"field2".to_vec(),
                b"value2".to_vec(),
            ],
            &mut store,
            1_000,
        )
        .expect("xadd second same ms");
        assert_eq!(second, RespFrame::BulkString(Some(b"1000-1".to_vec())));

        let third = dispatch_argv(
            &[
                b"XADD".to_vec(),
                b"stream".to_vec(),
                b"*".to_vec(),
                b"field3".to_vec(),
                b"value3".to_vec(),
            ],
            &mut store,
            1_500,
        )
        .expect("xadd third newer ms");
        assert_eq!(third, RespFrame::BulkString(Some(b"1500-0".to_vec())));

        let len = dispatch_argv(&[b"XLEN".to_vec(), b"stream".to_vec()], &mut store, 1_500)
            .expect("xlen");
        assert_eq!(len, RespFrame::Integer(3));

        let type_out = dispatch_argv(&[b"TYPE".to_vec(), b"stream".to_vec()], &mut store, 1_500)
            .expect("type stream");
        assert_eq!(type_out, RespFrame::SimpleString("stream".to_string()));
    }

    #[test]
    fn xadd_explicit_id_and_validation_errors() {
        let mut store = Store::new();

        let out = dispatch_argv(
            &[
                b"XADD".to_vec(),
                b"stream".to_vec(),
                b"1-1".to_vec(),
                b"field".to_vec(),
                b"value".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xadd explicit");
        assert_eq!(out, RespFrame::BulkString(Some(b"1-1".to_vec())));

        let non_monotonic = dispatch_argv(
            &[
                b"XADD".to_vec(),
                b"stream".to_vec(),
                b"1-1".to_vec(),
                b"field".to_vec(),
                b"value".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xadd non monotonic");
        assert_eq!(
            non_monotonic,
            RespFrame::Error(
                "ERR The ID specified in XADD is equal or smaller than the target stream top item"
                    .to_string()
            )
        );

        let zero_id = dispatch_argv(
            &[
                b"XADD".to_vec(),
                b"stream2".to_vec(),
                b"0-0".to_vec(),
                b"field".to_vec(),
                b"value".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xadd zero id");
        assert_eq!(
            zero_id,
            RespFrame::Error("ERR The ID specified in XADD must be greater than 0-0".to_string())
        );

        let invalid_id = dispatch_argv(
            &[
                b"XADD".to_vec(),
                b"stream2".to_vec(),
                b"bad-id".to_vec(),
                b"field".to_vec(),
                b"value".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xadd invalid id");
        assert_eq!(
            invalid_id,
            RespFrame::Error(
                "ERR Invalid stream ID specified as stream command argument".to_string()
            )
        );
    }

    #[test]
    fn xadd_wrongtype_on_string_key() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), b"v".to_vec(), None, 0);

        let err = dispatch_argv(
            &[
                b"XADD".to_vec(),
                b"k".to_vec(),
                b"*".to_vec(),
                b"field".to_vec(),
                b"value".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect_err("xadd wrongtype");
        assert!(matches!(
            err,
            CommandError::Store(fr_store::StoreError::WrongType)
        ));
    }

    #[test]
    fn xrange_returns_entries_and_supports_count() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"XADD".to_vec(),
                b"stream".to_vec(),
                b"1000-0".to_vec(),
                b"field1".to_vec(),
                b"value1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xadd 1");
        dispatch_argv(
            &[
                b"XADD".to_vec(),
                b"stream".to_vec(),
                b"1000-1".to_vec(),
                b"field2".to_vec(),
                b"value2".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xadd 2");
        dispatch_argv(
            &[
                b"XADD".to_vec(),
                b"stream".to_vec(),
                b"1001-0".to_vec(),
                b"field3".to_vec(),
                b"value3".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xadd 3");

        let all = dispatch_argv(
            &[
                b"XRANGE".to_vec(),
                b"stream".to_vec(),
                b"-".to_vec(),
                b"+".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xrange all");
        assert_eq!(
            all,
            RespFrame::Array(Some(vec![
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"1000-0".to_vec())),
                    RespFrame::Array(Some(vec![
                        RespFrame::BulkString(Some(b"field1".to_vec())),
                        RespFrame::BulkString(Some(b"value1".to_vec())),
                    ])),
                ])),
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"1000-1".to_vec())),
                    RespFrame::Array(Some(vec![
                        RespFrame::BulkString(Some(b"field2".to_vec())),
                        RespFrame::BulkString(Some(b"value2".to_vec())),
                    ])),
                ])),
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"1001-0".to_vec())),
                    RespFrame::Array(Some(vec![
                        RespFrame::BulkString(Some(b"field3".to_vec())),
                        RespFrame::BulkString(Some(b"value3".to_vec())),
                    ])),
                ])),
            ]))
        );

        let limited = dispatch_argv(
            &[
                b"XRANGE".to_vec(),
                b"stream".to_vec(),
                b"1000".to_vec(),
                b"+".to_vec(),
                b"COUNT".to_vec(),
                b"1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xrange count 1");
        assert_eq!(
            limited,
            RespFrame::Array(Some(vec![RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"1000-0".to_vec())),
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"field1".to_vec())),
                    RespFrame::BulkString(Some(b"value1".to_vec())),
                ])),
            ]))]))
        );
    }

    #[test]
    fn xrange_bound_validation_and_empty_cases() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"XADD".to_vec(),
                b"stream".to_vec(),
                b"1000-0".to_vec(),
                b"field".to_vec(),
                b"value".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xadd");

        let invalid_start = dispatch_argv(
            &[
                b"XRANGE".to_vec(),
                b"stream".to_vec(),
                b"bad-id".to_vec(),
                b"+".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xrange invalid start");
        assert_eq!(
            invalid_start,
            RespFrame::Error(
                "ERR Invalid stream ID specified as stream command argument".to_string()
            )
        );

        let syntax = dispatch_argv(
            &[
                b"XRANGE".to_vec(),
                b"stream".to_vec(),
                b"-".to_vec(),
                b"+".to_vec(),
                b"LIMIT".to_vec(),
                b"1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect_err("xrange invalid option");
        assert!(matches!(syntax, CommandError::SyntaxError));

        let empty = dispatch_argv(
            &[
                b"XRANGE".to_vec(),
                b"stream".to_vec(),
                b"2000-0".to_vec(),
                b"1000-0".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xrange inverted bounds");
        assert_eq!(empty, RespFrame::Array(Some(vec![])));

        let missing = dispatch_argv(
            &[
                b"XRANGE".to_vec(),
                b"missing".to_vec(),
                b"-".to_vec(),
                b"+".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xrange missing");
        assert_eq!(missing, RespFrame::Array(Some(vec![])));
    }

    #[test]
    fn xrange_wrongtype_on_string_key() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), b"v".to_vec(), None, 0);
        let err = dispatch_argv(
            &[
                b"XRANGE".to_vec(),
                b"k".to_vec(),
                b"-".to_vec(),
                b"+".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect_err("xrange wrongtype");
        assert!(matches!(
            err,
            CommandError::Store(fr_store::StoreError::WrongType)
        ));
    }

    #[test]
    fn xrevrange_returns_reverse_entries_and_supports_count() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"XADD".to_vec(),
                b"stream".to_vec(),
                b"1000-0".to_vec(),
                b"field1".to_vec(),
                b"value1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xadd 1");
        dispatch_argv(
            &[
                b"XADD".to_vec(),
                b"stream".to_vec(),
                b"1000-1".to_vec(),
                b"field2".to_vec(),
                b"value2".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xadd 2");
        dispatch_argv(
            &[
                b"XADD".to_vec(),
                b"stream".to_vec(),
                b"1001-0".to_vec(),
                b"field3".to_vec(),
                b"value3".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xadd 3");

        let all = dispatch_argv(
            &[
                b"XREVRANGE".to_vec(),
                b"stream".to_vec(),
                b"+".to_vec(),
                b"-".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xrevrange all");
        assert_eq!(
            all,
            RespFrame::Array(Some(vec![
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"1001-0".to_vec())),
                    RespFrame::Array(Some(vec![
                        RespFrame::BulkString(Some(b"field3".to_vec())),
                        RespFrame::BulkString(Some(b"value3".to_vec())),
                    ])),
                ])),
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"1000-1".to_vec())),
                    RespFrame::Array(Some(vec![
                        RespFrame::BulkString(Some(b"field2".to_vec())),
                        RespFrame::BulkString(Some(b"value2".to_vec())),
                    ])),
                ])),
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"1000-0".to_vec())),
                    RespFrame::Array(Some(vec![
                        RespFrame::BulkString(Some(b"field1".to_vec())),
                        RespFrame::BulkString(Some(b"value1".to_vec())),
                    ])),
                ])),
            ]))
        );

        let limited = dispatch_argv(
            &[
                b"XREVRANGE".to_vec(),
                b"stream".to_vec(),
                b"1001-0".to_vec(),
                b"1000-0".to_vec(),
                b"COUNT".to_vec(),
                b"1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xrevrange count 1");
        assert_eq!(
            limited,
            RespFrame::Array(Some(vec![RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"1001-0".to_vec())),
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"field3".to_vec())),
                    RespFrame::BulkString(Some(b"value3".to_vec())),
                ])),
            ]))]))
        );
    }

    #[test]
    fn xrevrange_validation_and_wrongtype() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"XADD".to_vec(),
                b"stream".to_vec(),
                b"1000-0".to_vec(),
                b"field".to_vec(),
                b"value".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xadd");

        let invalid_end = dispatch_argv(
            &[
                b"XREVRANGE".to_vec(),
                b"stream".to_vec(),
                b"bad-id".to_vec(),
                b"-".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xrevrange invalid end");
        assert_eq!(
            invalid_end,
            RespFrame::Error(
                "ERR Invalid stream ID specified as stream command argument".to_string()
            )
        );

        let syntax = dispatch_argv(
            &[
                b"XREVRANGE".to_vec(),
                b"stream".to_vec(),
                b"+".to_vec(),
                b"-".to_vec(),
                b"LIMIT".to_vec(),
                b"1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect_err("xrevrange invalid option");
        assert!(matches!(syntax, CommandError::SyntaxError));

        let empty = dispatch_argv(
            &[
                b"XREVRANGE".to_vec(),
                b"stream".to_vec(),
                b"1000-0".to_vec(),
                b"2000-0".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xrevrange inverted bounds");
        assert_eq!(empty, RespFrame::Array(Some(vec![])));

        store.set(b"k".to_vec(), b"v".to_vec(), None, 0);
        let wrongtype = dispatch_argv(
            &[
                b"XREVRANGE".to_vec(),
                b"k".to_vec(),
                b"+".to_vec(),
                b"-".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect_err("xrevrange wrongtype");
        assert!(matches!(
            wrongtype,
            CommandError::Store(fr_store::StoreError::WrongType)
        ));
    }

    #[test]
    fn xdel_deletes_existing_entries_and_ignores_missing() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"XADD".to_vec(),
                b"stream".to_vec(),
                b"1000-0".to_vec(),
                b"field1".to_vec(),
                b"value1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xadd 1");
        dispatch_argv(
            &[
                b"XADD".to_vec(),
                b"stream".to_vec(),
                b"1000-1".to_vec(),
                b"field2".to_vec(),
                b"value2".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xadd 2");
        dispatch_argv(
            &[
                b"XADD".to_vec(),
                b"stream".to_vec(),
                b"1001-0".to_vec(),
                b"field3".to_vec(),
                b"value3".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xadd 3");

        let removed = dispatch_argv(
            &[
                b"XDEL".to_vec(),
                b"stream".to_vec(),
                b"1000-1".to_vec(),
                b"9999-0".to_vec(),
                b"1000-1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xdel");
        assert_eq!(removed, RespFrame::Integer(1));

        let len = dispatch_argv(&[b"XLEN".to_vec(), b"stream".to_vec()], &mut store, 0)
            .expect("xlen after xdel");
        assert_eq!(len, RespFrame::Integer(2));

        let remaining = dispatch_argv(
            &[
                b"XRANGE".to_vec(),
                b"stream".to_vec(),
                b"-".to_vec(),
                b"+".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xrange remaining");
        assert_eq!(
            remaining,
            RespFrame::Array(Some(vec![
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"1000-0".to_vec())),
                    RespFrame::Array(Some(vec![
                        RespFrame::BulkString(Some(b"field1".to_vec())),
                        RespFrame::BulkString(Some(b"value1".to_vec())),
                    ])),
                ])),
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"1001-0".to_vec())),
                    RespFrame::Array(Some(vec![
                        RespFrame::BulkString(Some(b"field3".to_vec())),
                        RespFrame::BulkString(Some(b"value3".to_vec())),
                    ])),
                ])),
            ]))
        );
    }

    #[test]
    fn xdel_validation_missing_and_wrongtype() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"XADD".to_vec(),
                b"stream".to_vec(),
                b"1000-0".to_vec(),
                b"field".to_vec(),
                b"value".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xadd");

        let invalid = dispatch_argv(
            &[b"XDEL".to_vec(), b"stream".to_vec(), b"bad-id".to_vec()],
            &mut store,
            0,
        )
        .expect("xdel invalid id");
        assert_eq!(
            invalid,
            RespFrame::Error(
                "ERR Invalid stream ID specified as stream command argument".to_string()
            )
        );

        let missing = dispatch_argv(
            &[b"XDEL".to_vec(), b"missing".to_vec(), b"1-0".to_vec()],
            &mut store,
            0,
        )
        .expect("xdel missing key");
        assert_eq!(missing, RespFrame::Integer(0));

        let arity = dispatch_argv(&[b"XDEL".to_vec(), b"stream".to_vec()], &mut store, 0)
            .expect_err("xdel arity");
        assert!(matches!(arity, CommandError::WrongArity("XDEL")));

        store.set(b"k".to_vec(), b"v".to_vec(), None, 0);
        let wrongtype = dispatch_argv(
            &[b"XDEL".to_vec(), b"k".to_vec(), b"1-0".to_vec()],
            &mut store,
            0,
        )
        .expect_err("xdel wrongtype");
        assert!(matches!(
            wrongtype,
            CommandError::Store(fr_store::StoreError::WrongType)
        ));
    }

    #[test]
    fn xtrim_maxlen_removes_oldest_entries_and_supports_equals() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"XADD".to_vec(),
                b"stream".to_vec(),
                b"1000-0".to_vec(),
                b"field1".to_vec(),
                b"value1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xadd 1");
        dispatch_argv(
            &[
                b"XADD".to_vec(),
                b"stream".to_vec(),
                b"1000-1".to_vec(),
                b"field2".to_vec(),
                b"value2".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xadd 2");
        dispatch_argv(
            &[
                b"XADD".to_vec(),
                b"stream".to_vec(),
                b"1001-0".to_vec(),
                b"field3".to_vec(),
                b"value3".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xadd 3");

        let removed = dispatch_argv(
            &[
                b"XTRIM".to_vec(),
                b"stream".to_vec(),
                b"MAXLEN".to_vec(),
                b"2".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xtrim maxlen");
        assert_eq!(removed, RespFrame::Integer(1));

        let removed_again = dispatch_argv(
            &[
                b"XTRIM".to_vec(),
                b"stream".to_vec(),
                b"MAXLEN".to_vec(),
                b"=".to_vec(),
                b"1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xtrim maxlen equals");
        assert_eq!(removed_again, RespFrame::Integer(1));

        let remaining = dispatch_argv(
            &[
                b"XRANGE".to_vec(),
                b"stream".to_vec(),
                b"-".to_vec(),
                b"+".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xrange remaining");
        assert_eq!(
            remaining,
            RespFrame::Array(Some(vec![RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"1001-0".to_vec())),
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"field3".to_vec())),
                    RespFrame::BulkString(Some(b"value3".to_vec())),
                ])),
            ]))]))
        );
    }

    #[test]
    fn xtrim_validation_missing_and_wrongtype() {
        let mut store = Store::new();
        let missing = dispatch_argv(
            &[
                b"XTRIM".to_vec(),
                b"missing".to_vec(),
                b"MAXLEN".to_vec(),
                b"1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xtrim missing key");
        assert_eq!(missing, RespFrame::Integer(0));

        let syntax_mode = dispatch_argv(
            &[
                b"XTRIM".to_vec(),
                b"stream".to_vec(),
                b"MINID".to_vec(),
                b"1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect_err("xtrim invalid mode");
        assert!(matches!(syntax_mode, CommandError::SyntaxError));

        let syntax_option = dispatch_argv(
            &[
                b"XTRIM".to_vec(),
                b"stream".to_vec(),
                b"MAXLEN".to_vec(),
                b"~".to_vec(),
                b"1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect_err("xtrim invalid option");
        assert!(matches!(syntax_option, CommandError::SyntaxError));

        let invalid_integer = dispatch_argv(
            &[
                b"XTRIM".to_vec(),
                b"stream".to_vec(),
                b"MAXLEN".to_vec(),
                b"-1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect_err("xtrim invalid integer");
        assert!(matches!(invalid_integer, CommandError::InvalidInteger));

        let arity = dispatch_argv(
            &[b"XTRIM".to_vec(), b"stream".to_vec(), b"MAXLEN".to_vec()],
            &mut store,
            0,
        )
        .expect_err("xtrim arity");
        assert!(matches!(arity, CommandError::WrongArity("XTRIM")));

        store.set(b"k".to_vec(), b"v".to_vec(), None, 0);
        let wrongtype = dispatch_argv(
            &[
                b"XTRIM".to_vec(),
                b"k".to_vec(),
                b"MAXLEN".to_vec(),
                b"1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect_err("xtrim wrongtype");
        assert!(matches!(
            wrongtype,
            CommandError::Store(fr_store::StoreError::WrongType)
        ));
    }

    #[test]
    fn xread_single_stream_and_count() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"XADD".to_vec(),
                b"s".to_vec(),
                b"1000-0".to_vec(),
                b"field1".to_vec(),
                b"value1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xadd 1");
        dispatch_argv(
            &[
                b"XADD".to_vec(),
                b"s".to_vec(),
                b"1000-1".to_vec(),
                b"field2".to_vec(),
                b"value2".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xadd 2");
        dispatch_argv(
            &[
                b"XADD".to_vec(),
                b"s".to_vec(),
                b"1001-0".to_vec(),
                b"field3".to_vec(),
                b"value3".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xadd 3");

        let from_mid = dispatch_argv(
            &[
                b"XREAD".to_vec(),
                b"STREAMS".to_vec(),
                b"s".to_vec(),
                b"1000-0".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xread from 1000-0");
        assert_eq!(
            from_mid,
            RespFrame::Array(Some(vec![RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"s".to_vec())),
                RespFrame::Array(Some(vec![
                    RespFrame::Array(Some(vec![
                        RespFrame::BulkString(Some(b"1000-1".to_vec())),
                        RespFrame::Array(Some(vec![
                            RespFrame::BulkString(Some(b"field2".to_vec())),
                            RespFrame::BulkString(Some(b"value2".to_vec())),
                        ])),
                    ])),
                    RespFrame::Array(Some(vec![
                        RespFrame::BulkString(Some(b"1001-0".to_vec())),
                        RespFrame::Array(Some(vec![
                            RespFrame::BulkString(Some(b"field3".to_vec())),
                            RespFrame::BulkString(Some(b"value3".to_vec())),
                        ])),
                    ])),
                ])),
            ]))]))
        );

        let limited = dispatch_argv(
            &[
                b"XREAD".to_vec(),
                b"COUNT".to_vec(),
                b"1".to_vec(),
                b"STREAMS".to_vec(),
                b"s".to_vec(),
                b"0".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xread count 1");
        assert_eq!(
            limited,
            RespFrame::Array(Some(vec![RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"s".to_vec())),
                RespFrame::Array(Some(vec![RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"1000-0".to_vec())),
                    RespFrame::Array(Some(vec![
                        RespFrame::BulkString(Some(b"field1".to_vec())),
                        RespFrame::BulkString(Some(b"value1".to_vec())),
                    ])),
                ]))])),
            ]))]))
        );
    }

    #[test]
    fn xread_multiple_streams_and_dollar_nil() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"XADD".to_vec(),
                b"s1".to_vec(),
                b"2000-0".to_vec(),
                b"a".to_vec(),
                b"1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xadd s1");
        dispatch_argv(
            &[
                b"XADD".to_vec(),
                b"s2".to_vec(),
                b"3000-0".to_vec(),
                b"b".to_vec(),
                b"2".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xadd s2");

        let both = dispatch_argv(
            &[
                b"XREAD".to_vec(),
                b"STREAMS".to_vec(),
                b"s1".to_vec(),
                b"s2".to_vec(),
                b"0".to_vec(),
                b"0".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xread both");
        assert_eq!(
            both,
            RespFrame::Array(Some(vec![
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"s1".to_vec())),
                    RespFrame::Array(Some(vec![RespFrame::Array(Some(vec![
                        RespFrame::BulkString(Some(b"2000-0".to_vec())),
                        RespFrame::Array(Some(vec![
                            RespFrame::BulkString(Some(b"a".to_vec())),
                            RespFrame::BulkString(Some(b"1".to_vec())),
                        ])),
                    ]))])),
                ])),
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"s2".to_vec())),
                    RespFrame::Array(Some(vec![RespFrame::Array(Some(vec![
                        RespFrame::BulkString(Some(b"3000-0".to_vec())),
                        RespFrame::Array(Some(vec![
                            RespFrame::BulkString(Some(b"b".to_vec())),
                            RespFrame::BulkString(Some(b"2".to_vec())),
                        ])),
                    ]))])),
                ])),
            ]))
        );

        let none = dispatch_argv(
            &[
                b"XREAD".to_vec(),
                b"STREAMS".to_vec(),
                b"s1".to_vec(),
                b"s2".to_vec(),
                b"$".to_vec(),
                b"$".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xread dollar");
        assert_eq!(none, RespFrame::Array(None));
    }

    #[test]
    fn xread_validation_and_wrongtype() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"XADD".to_vec(),
                b"s".to_vec(),
                b"1000-0".to_vec(),
                b"field".to_vec(),
                b"value".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xadd");

        let invalid_id = dispatch_argv(
            &[
                b"XREAD".to_vec(),
                b"STREAMS".to_vec(),
                b"s".to_vec(),
                b"bad-id".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xread invalid id");
        assert_eq!(
            invalid_id,
            RespFrame::Error(
                "ERR Invalid stream ID specified as stream command argument".to_string()
            )
        );

        let syntax = dispatch_argv(
            &[
                b"XREAD".to_vec(),
                b"BLOCK".to_vec(),
                b"0".to_vec(),
                b"STREAMS".to_vec(),
                b"s".to_vec(),
                b"0".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect_err("xread block unsupported");
        assert!(matches!(syntax, CommandError::SyntaxError));

        let bad_keyword = dispatch_argv(
            &[
                b"XREAD".to_vec(),
                b"COUNT".to_vec(),
                b"1".to_vec(),
                b"NOTSTREAMS".to_vec(),
                b"s".to_vec(),
                b"0".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect_err("xread missing STREAMS keyword");
        assert!(matches!(bad_keyword, CommandError::SyntaxError));

        let arity = dispatch_argv(
            &[
                b"XREAD".to_vec(),
                b"STREAMS".to_vec(),
                b"s".to_vec(),
                b"s2".to_vec(),
                b"0".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect_err("xread mismatched keys/ids");
        assert!(matches!(arity, CommandError::WrongArity("XREAD")));

        store.set(b"k".to_vec(), b"v".to_vec(), None, 0);
        let wrongtype = dispatch_argv(
            &[
                b"XREAD".to_vec(),
                b"STREAMS".to_vec(),
                b"k".to_vec(),
                b"0".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect_err("xread wrongtype");
        assert!(matches!(
            wrongtype,
            CommandError::Store(fr_store::StoreError::WrongType)
        ));
    }

    #[test]
    fn xinfo_stream_reports_bounds_and_metadata_shape() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"XADD".to_vec(),
                b"s".to_vec(),
                b"1000-0".to_vec(),
                b"field1".to_vec(),
                b"value1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xadd 1");
        dispatch_argv(
            &[
                b"XADD".to_vec(),
                b"s".to_vec(),
                b"1001-0".to_vec(),
                b"field2".to_vec(),
                b"value2".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xadd 2");

        let info = dispatch_argv(
            &[b"XINFO".to_vec(), b"STREAM".to_vec(), b"s".to_vec()],
            &mut store,
            0,
        )
        .expect("xinfo stream");

        assert_eq!(
            info,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"length".to_vec())),
                RespFrame::Integer(2),
                RespFrame::BulkString(Some(b"radix-tree-keys".to_vec())),
                RespFrame::Integer(1),
                RespFrame::BulkString(Some(b"radix-tree-nodes".to_vec())),
                RespFrame::Integer(2),
                RespFrame::BulkString(Some(b"last-generated-id".to_vec())),
                RespFrame::BulkString(Some(b"1001-0".to_vec())),
                RespFrame::BulkString(Some(b"max-deleted-entry-id".to_vec())),
                RespFrame::BulkString(Some(b"0-0".to_vec())),
                RespFrame::BulkString(Some(b"entries-added".to_vec())),
                RespFrame::Integer(2),
                RespFrame::BulkString(Some(b"recorded-first-entry-id".to_vec())),
                RespFrame::BulkString(Some(b"1000-0".to_vec())),
                RespFrame::BulkString(Some(b"groups".to_vec())),
                RespFrame::Integer(0),
                RespFrame::BulkString(Some(b"first-entry".to_vec())),
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"1000-0".to_vec())),
                    RespFrame::Array(Some(vec![
                        RespFrame::BulkString(Some(b"field1".to_vec())),
                        RespFrame::BulkString(Some(b"value1".to_vec())),
                    ])),
                ])),
                RespFrame::BulkString(Some(b"last-entry".to_vec())),
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"1001-0".to_vec())),
                    RespFrame::Array(Some(vec![
                        RespFrame::BulkString(Some(b"field2".to_vec())),
                        RespFrame::BulkString(Some(b"value2".to_vec())),
                    ])),
                ])),
            ]))
        );
    }

    #[test]
    fn xinfo_validation_missing_and_wrongtype() {
        let mut store = Store::new();

        let missing = dispatch_argv(
            &[b"XINFO".to_vec(), b"STREAM".to_vec(), b"missing".to_vec()],
            &mut store,
            0,
        )
        .expect_err("xinfo missing");
        assert!(matches!(missing, CommandError::NoSuchKey));

        let syntax = dispatch_argv(
            &[b"XINFO".to_vec(), b"HELP".to_vec(), b"s".to_vec()],
            &mut store,
            0,
        )
        .expect_err("xinfo unsupported subcommand");
        assert!(matches!(syntax, CommandError::SyntaxError));

        let arity = dispatch_argv(&[b"XINFO".to_vec(), b"STREAM".to_vec()], &mut store, 0)
            .expect_err("xinfo arity");
        assert!(matches!(arity, CommandError::WrongArity("XINFO")));

        store.set(b"str".to_vec(), b"value".to_vec(), None, 0);
        let wrongtype = dispatch_argv(
            &[b"XINFO".to_vec(), b"STREAM".to_vec(), b"str".to_vec()],
            &mut store,
            0,
        )
        .expect_err("xinfo wrongtype");
        assert!(matches!(
            wrongtype,
            CommandError::Store(fr_store::StoreError::WrongType)
        ));
    }

    #[test]
    fn xinfo_groups_returns_empty_array_for_stream_without_groups() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"XADD".to_vec(),
                b"s".to_vec(),
                b"1000-0".to_vec(),
                b"field".to_vec(),
                b"value".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xadd");

        let groups = dispatch_argv(
            &[b"XINFO".to_vec(), b"GROUPS".to_vec(), b"s".to_vec()],
            &mut store,
            0,
        )
        .expect("xinfo groups");
        assert_eq!(groups, RespFrame::Array(Some(Vec::new())));

        let missing = dispatch_argv(
            &[b"XINFO".to_vec(), b"GROUPS".to_vec(), b"missing".to_vec()],
            &mut store,
            0,
        )
        .expect_err("xinfo groups missing");
        assert!(matches!(missing, CommandError::NoSuchKey));
    }

    #[test]
    fn xgroup_create_and_xinfo_groups_report_created_group() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"XADD".to_vec(),
                b"s".to_vec(),
                b"1000-0".to_vec(),
                b"field1".to_vec(),
                b"value1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xadd 1");
        dispatch_argv(
            &[
                b"XADD".to_vec(),
                b"s".to_vec(),
                b"1001-0".to_vec(),
                b"field2".to_vec(),
                b"value2".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xadd 2");

        let create = dispatch_argv(
            &[
                b"XGROUP".to_vec(),
                b"CREATE".to_vec(),
                b"s".to_vec(),
                b"g1".to_vec(),
                b"$".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xgroup create");
        assert_eq!(create, RespFrame::SimpleString("OK".to_string()));

        let groups = dispatch_argv(
            &[b"XINFO".to_vec(), b"GROUPS".to_vec(), b"s".to_vec()],
            &mut store,
            0,
        )
        .expect("xinfo groups");
        assert_eq!(
            groups,
            RespFrame::Array(Some(vec![RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"name".to_vec())),
                RespFrame::BulkString(Some(b"g1".to_vec())),
                RespFrame::BulkString(Some(b"consumers".to_vec())),
                RespFrame::Integer(0),
                RespFrame::BulkString(Some(b"pending".to_vec())),
                RespFrame::Integer(0),
                RespFrame::BulkString(Some(b"last-delivered-id".to_vec())),
                RespFrame::BulkString(Some(b"1001-0".to_vec())),
                RespFrame::BulkString(Some(b"entries-read".to_vec())),
                RespFrame::BulkString(None),
                RespFrame::BulkString(Some(b"lag".to_vec())),
                RespFrame::BulkString(None),
            ]))]))
        );

        let duplicate = dispatch_argv(
            &[
                b"XGROUP".to_vec(),
                b"CREATE".to_vec(),
                b"s".to_vec(),
                b"g1".to_vec(),
                b"0".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xgroup duplicate");
        assert_eq!(
            duplicate,
            RespFrame::Error("BUSYGROUP Consumer Group name already exists".to_string())
        );

        let stream_info = dispatch_argv(
            &[b"XINFO".to_vec(), b"STREAM".to_vec(), b"s".to_vec()],
            &mut store,
            0,
        )
        .expect("xinfo stream");
        let RespFrame::Array(Some(items)) = stream_info else {
            panic!("expected array");
        };
        let mut groups_count = None;
        let mut idx = 0usize;
        while idx + 1 < items.len() {
            if items[idx] == RespFrame::BulkString(Some(b"groups".to_vec())) {
                groups_count = Some(items[idx + 1].clone());
                break;
            }
            idx += 2;
        }
        assert_eq!(groups_count, Some(RespFrame::Integer(1)));
    }

    #[test]
    fn xgroup_validation_missing_mkstream_wrongtype_and_syntax() {
        let mut store = Store::new();

        let missing = dispatch_argv(
            &[
                b"XGROUP".to_vec(),
                b"CREATE".to_vec(),
                b"missing".to_vec(),
                b"g1".to_vec(),
                b"0".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect_err("xgroup missing key");
        assert!(matches!(missing, CommandError::NoSuchKey));

        let mkstream = dispatch_argv(
            &[
                b"XGROUP".to_vec(),
                b"CREATE".to_vec(),
                b"missing".to_vec(),
                b"g1".to_vec(),
                b"0".to_vec(),
                b"MKSTREAM".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xgroup mkstream create");
        assert_eq!(mkstream, RespFrame::SimpleString("OK".to_string()));

        let groups = dispatch_argv(
            &[b"XINFO".to_vec(), b"GROUPS".to_vec(), b"missing".to_vec()],
            &mut store,
            0,
        )
        .expect("xinfo groups after mkstream");
        assert_eq!(
            groups,
            RespFrame::Array(Some(vec![RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"name".to_vec())),
                RespFrame::BulkString(Some(b"g1".to_vec())),
                RespFrame::BulkString(Some(b"consumers".to_vec())),
                RespFrame::Integer(0),
                RespFrame::BulkString(Some(b"pending".to_vec())),
                RespFrame::Integer(0),
                RespFrame::BulkString(Some(b"last-delivered-id".to_vec())),
                RespFrame::BulkString(Some(b"0-0".to_vec())),
                RespFrame::BulkString(Some(b"entries-read".to_vec())),
                RespFrame::BulkString(None),
                RespFrame::BulkString(Some(b"lag".to_vec())),
                RespFrame::BulkString(None),
            ]))]))
        );

        let invalid_id = dispatch_argv(
            &[
                b"XGROUP".to_vec(),
                b"CREATE".to_vec(),
                b"missing".to_vec(),
                b"g2".to_vec(),
                b"bad-id".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xgroup invalid id");
        assert_eq!(
            invalid_id,
            RespFrame::Error(
                "ERR Invalid stream ID specified as stream command argument".to_string()
            )
        );

        let syntax = dispatch_argv(
            &[
                b"XGROUP".to_vec(),
                b"HELP".to_vec(),
                b"s".to_vec(),
                b"g1".to_vec(),
                b"0".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect_err("xgroup unsupported subcommand");
        assert!(matches!(syntax, CommandError::SyntaxError));

        let arity = dispatch_argv(
            &[
                b"XGROUP".to_vec(),
                b"CREATE".to_vec(),
                b"s".to_vec(),
                b"g1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect_err("xgroup arity");
        assert!(matches!(arity, CommandError::WrongArity("XGROUP")));

        store.set(b"str".to_vec(), b"value".to_vec(), None, 0);
        let wrongtype = dispatch_argv(
            &[
                b"XGROUP".to_vec(),
                b"CREATE".to_vec(),
                b"str".to_vec(),
                b"g1".to_vec(),
                b"0".to_vec(),
                b"MKSTREAM".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect_err("xgroup wrongtype");
        assert!(matches!(
            wrongtype,
            CommandError::Store(fr_store::StoreError::WrongType)
        ));
    }

    #[test]
    fn xgroup_destroy_removes_group_and_reports_counts() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"XADD".to_vec(),
                b"s".to_vec(),
                b"1000-0".to_vec(),
                b"f".to_vec(),
                b"v".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xadd");
        dispatch_argv(
            &[
                b"XGROUP".to_vec(),
                b"CREATE".to_vec(),
                b"s".to_vec(),
                b"g1".to_vec(),
                b"0".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xgroup create");

        let removed = dispatch_argv(
            &[
                b"XGROUP".to_vec(),
                b"DESTROY".to_vec(),
                b"s".to_vec(),
                b"g1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xgroup destroy");
        assert_eq!(removed, RespFrame::Integer(1));

        let groups = dispatch_argv(
            &[b"XINFO".to_vec(), b"GROUPS".to_vec(), b"s".to_vec()],
            &mut store,
            0,
        )
        .expect("xinfo groups");
        assert_eq!(groups, RespFrame::Array(Some(Vec::new())));

        let removed_missing = dispatch_argv(
            &[
                b"XGROUP".to_vec(),
                b"DESTROY".to_vec(),
                b"s".to_vec(),
                b"g1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xgroup destroy missing group");
        assert_eq!(removed_missing, RespFrame::Integer(0));

        let missing_key = dispatch_argv(
            &[
                b"XGROUP".to_vec(),
                b"DESTROY".to_vec(),
                b"missing".to_vec(),
                b"g1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xgroup destroy missing key");
        assert_eq!(missing_key, RespFrame::Integer(0));
    }

    #[test]
    fn xgroup_destroy_wrongtype_and_arity() {
        let mut store = Store::new();
        store.set(b"str".to_vec(), b"value".to_vec(), None, 0);

        let wrongtype = dispatch_argv(
            &[
                b"XGROUP".to_vec(),
                b"DESTROY".to_vec(),
                b"str".to_vec(),
                b"g1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect_err("xgroup destroy wrongtype");
        assert!(matches!(
            wrongtype,
            CommandError::Store(fr_store::StoreError::WrongType)
        ));

        let arity = dispatch_argv(
            &[b"XGROUP".to_vec(), b"DESTROY".to_vec(), b"str".to_vec()],
            &mut store,
            0,
        )
        .expect_err("xgroup destroy arity");
        assert!(matches!(arity, CommandError::WrongArity("XGROUP")));
    }

    #[test]
    fn xgroup_setid_updates_group_cursor_and_supports_dollar() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"XADD".to_vec(),
                b"s".to_vec(),
                b"1000-0".to_vec(),
                b"f1".to_vec(),
                b"v1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xadd 1");
        dispatch_argv(
            &[
                b"XADD".to_vec(),
                b"s".to_vec(),
                b"1001-0".to_vec(),
                b"f2".to_vec(),
                b"v2".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xadd 2");
        dispatch_argv(
            &[
                b"XGROUP".to_vec(),
                b"CREATE".to_vec(),
                b"s".to_vec(),
                b"g1".to_vec(),
                b"0".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xgroup create");

        let setid = dispatch_argv(
            &[
                b"XGROUP".to_vec(),
                b"SETID".to_vec(),
                b"s".to_vec(),
                b"g1".to_vec(),
                b"1000-0".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xgroup setid");
        assert_eq!(setid, RespFrame::SimpleString("OK".to_string()));

        let groups_after_setid = dispatch_argv(
            &[b"XINFO".to_vec(), b"GROUPS".to_vec(), b"s".to_vec()],
            &mut store,
            0,
        )
        .expect("xinfo groups after setid");
        assert_eq!(
            groups_after_setid,
            RespFrame::Array(Some(vec![RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"name".to_vec())),
                RespFrame::BulkString(Some(b"g1".to_vec())),
                RespFrame::BulkString(Some(b"consumers".to_vec())),
                RespFrame::Integer(0),
                RespFrame::BulkString(Some(b"pending".to_vec())),
                RespFrame::Integer(0),
                RespFrame::BulkString(Some(b"last-delivered-id".to_vec())),
                RespFrame::BulkString(Some(b"1000-0".to_vec())),
                RespFrame::BulkString(Some(b"entries-read".to_vec())),
                RespFrame::BulkString(None),
                RespFrame::BulkString(Some(b"lag".to_vec())),
                RespFrame::BulkString(None),
            ]))]))
        );

        let setid_dollar = dispatch_argv(
            &[
                b"XGROUP".to_vec(),
                b"SETID".to_vec(),
                b"s".to_vec(),
                b"g1".to_vec(),
                b"$".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xgroup setid dollar");
        assert_eq!(setid_dollar, RespFrame::SimpleString("OK".to_string()));

        let groups_after_dollar = dispatch_argv(
            &[b"XINFO".to_vec(), b"GROUPS".to_vec(), b"s".to_vec()],
            &mut store,
            0,
        )
        .expect("xinfo groups after dollar");
        assert_eq!(
            groups_after_dollar,
            RespFrame::Array(Some(vec![RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"name".to_vec())),
                RespFrame::BulkString(Some(b"g1".to_vec())),
                RespFrame::BulkString(Some(b"consumers".to_vec())),
                RespFrame::Integer(0),
                RespFrame::BulkString(Some(b"pending".to_vec())),
                RespFrame::Integer(0),
                RespFrame::BulkString(Some(b"last-delivered-id".to_vec())),
                RespFrame::BulkString(Some(b"1001-0".to_vec())),
                RespFrame::BulkString(Some(b"entries-read".to_vec())),
                RespFrame::BulkString(None),
                RespFrame::BulkString(Some(b"lag".to_vec())),
                RespFrame::BulkString(None),
            ]))]))
        );
    }

    #[test]
    fn xgroup_setid_missing_and_wrongtype_paths() {
        let mut store = Store::new();

        let missing = dispatch_argv(
            &[
                b"XGROUP".to_vec(),
                b"SETID".to_vec(),
                b"missing".to_vec(),
                b"g1".to_vec(),
                b"0".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect_err("xgroup setid missing key");
        assert!(matches!(missing, CommandError::NoSuchKey));

        dispatch_argv(
            &[
                b"XADD".to_vec(),
                b"s".to_vec(),
                b"1000-0".to_vec(),
                b"f".to_vec(),
                b"v".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xadd");
        let nogroup = dispatch_argv(
            &[
                b"XGROUP".to_vec(),
                b"SETID".to_vec(),
                b"s".to_vec(),
                b"g1".to_vec(),
                b"0".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xgroup setid missing group");
        assert_eq!(
            nogroup,
            RespFrame::Error(
                "NOGROUP No such key 's' or consumer group 'g1' in XGROUP command".to_string()
            )
        );

        let invalid_id = dispatch_argv(
            &[
                b"XGROUP".to_vec(),
                b"SETID".to_vec(),
                b"s".to_vec(),
                b"g1".to_vec(),
                b"bad-id".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xgroup setid invalid id");
        assert_eq!(
            invalid_id,
            RespFrame::Error(
                "ERR Invalid stream ID specified as stream command argument".to_string()
            )
        );

        let arity = dispatch_argv(
            &[
                b"XGROUP".to_vec(),
                b"SETID".to_vec(),
                b"s".to_vec(),
                b"g1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect_err("xgroup setid arity");
        assert!(matches!(arity, CommandError::WrongArity("XGROUP")));

        store.set(b"str".to_vec(), b"value".to_vec(), None, 0);
        let wrongtype = dispatch_argv(
            &[
                b"XGROUP".to_vec(),
                b"SETID".to_vec(),
                b"str".to_vec(),
                b"g1".to_vec(),
                b"0".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect_err("xgroup setid wrongtype");
        assert!(matches!(
            wrongtype,
            CommandError::Store(fr_store::StoreError::WrongType)
        ));
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
