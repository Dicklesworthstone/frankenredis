#![forbid(unsafe_code)]

mod lua_eval;

use fr_protocol::RespFrame;
use fr_store::{
    PttlValue, ScoreBound, Store, StoreError, StreamAutoClaimOptions, StreamAutoClaimReply,
    StreamClaimOptions, StreamClaimReply, StreamGroupReadCursor, StreamGroupReadOptions, StreamId,
    ValueType,
};

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
        Some(CommandId::Georadius) => return georadius(argv, store, now_ms),
        Some(CommandId::Georadiusbymember) => return georadiusbymember(argv, store, now_ms),
        Some(CommandId::Geosearch) => return geosearch(argv, store, now_ms),
        Some(CommandId::Geosearchstore) => return geosearchstore(argv, store, now_ms),
        Some(CommandId::Xadd) => return xadd(argv, store, now_ms),
        Some(CommandId::Xlen) => return xlen(argv, store, now_ms),
        Some(CommandId::Xdel) => return xdel(argv, store, now_ms),
        Some(CommandId::Xtrim) => return xtrim(argv, store, now_ms),
        Some(CommandId::Xread) => return xread(argv, store, now_ms),
        Some(CommandId::Xreadgroup) => return xreadgroup(argv, store, now_ms),
        Some(CommandId::Xclaim) => return xclaim(argv, store, now_ms),
        Some(CommandId::Xautoclaim) => return xautoclaim(argv, store, now_ms),
        Some(CommandId::Xpending) => return xpending(argv, store, now_ms),
        Some(CommandId::Xack) => return xack_cmd(argv, store, now_ms),
        Some(CommandId::Xsetid) => return xsetid_cmd(argv, store, now_ms),
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
        Some(CommandId::Sintercard) => return sintercard(argv, store, now_ms),
        Some(CommandId::Lcs) => return lcs(argv, store, now_ms),
        Some(CommandId::Lmpop) => return lmpop(argv, store, now_ms),
        Some(CommandId::Zmpop) => return zmpop(argv, store, now_ms),
        Some(CommandId::Slowlog) => return slowlog_cmd(argv),
        Some(CommandId::Save) => return save_cmd(argv),
        Some(CommandId::Bgsave) => return bgsave_cmd(argv),
        Some(CommandId::Bgrewriteaof) => return bgrewriteaof_cmd(argv),
        Some(CommandId::Lastsave) => return lastsave_cmd(argv, now_ms),
        Some(CommandId::Swapdb) => return swapdb_cmd(argv),
        Some(CommandId::Blpop) => return blpop(argv, store, now_ms),
        Some(CommandId::Brpop) => return brpop(argv, store, now_ms),
        Some(CommandId::Blmove) => return blmove(argv, store, now_ms),
        Some(CommandId::Blmpop) => return blmpop(argv, store, now_ms),
        Some(CommandId::Subscribe) => return subscribe_cmd(argv, store),
        Some(CommandId::Unsubscribe) => return unsubscribe_cmd(argv, store),
        Some(CommandId::Psubscribe) => return psubscribe_cmd(argv, store),
        Some(CommandId::Punsubscribe) => return punsubscribe_cmd(argv, store),
        Some(CommandId::Publish) => return publish_cmd(argv, store),
        Some(CommandId::Pubsub) => return pubsub_cmd(argv, store),
        Some(CommandId::Msetnx) => return msetnx(argv, store, now_ms),
        Some(CommandId::Brpoplpush) => return brpoplpush(argv, store, now_ms),
        Some(CommandId::Zdiff) => return zdiff(argv, store, now_ms),
        Some(CommandId::Zdiffstore) => return zdiffstore(argv, store, now_ms),
        Some(CommandId::Zinter) => return zinter(argv, store, now_ms),
        Some(CommandId::Zunion) => return zunion_cmd(argv, store, now_ms),
        Some(CommandId::Zintercard) => return zintercard(argv, store, now_ms),
        Some(CommandId::Eval) => return eval_cmd(argv, store, now_ms),
        Some(CommandId::Evalsha) => return evalsha_cmd(argv, store, now_ms),
        Some(CommandId::EvalRo) => return eval_cmd(argv, store, now_ms),
        Some(CommandId::EvalshaRo) => return evalsha_cmd(argv, store, now_ms),
        Some(CommandId::Script) => return script_cmd(argv, store),
        Some(CommandId::Debug) => return debug_cmd(argv),
        Some(CommandId::Role) => return role_cmd(argv),
        Some(CommandId::Shutdown) => return shutdown_cmd(argv),
        Some(CommandId::Move) => return move_cmd(argv),
        Some(CommandId::Latency) => return latency_cmd(argv),
        Some(CommandId::Bitfield) => return bitfield_cmd(argv, store, now_ms),
        Some(CommandId::Memory) => return memory_cmd(argv, store, now_ms),
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
        Some(CommandId::Object) => return object_cmd(argv, store, now_ms),
        Some(CommandId::Wait) => return wait_cmd(argv),
        Some(CommandId::Reset) => return reset_cmd(argv),
        Some(CommandId::Unlink) => return del(argv, store, now_ms),
        Some(CommandId::Touch) => return touch(argv, store, now_ms),
        Some(CommandId::Dump) => return dump_cmd(argv, store, now_ms),
        Some(CommandId::Restore) => return restore_cmd(argv, store, now_ms),
        Some(CommandId::Sort) => return sort_cmd(argv, store, now_ms),
        Some(CommandId::Copy) => return copy_cmd(argv, store, now_ms),
        Some(CommandId::Lolwut) => return lolwut_cmd(argv),
        Some(CommandId::Waitaof) => return waitaof_cmd(argv),
        Some(CommandId::Cluster) => return cluster_cmd(argv),
        Some(CommandId::Replicaof) => return replicaof_cmd(argv),
        Some(CommandId::Function) => return function_cmd(argv, store, now_ms),
        Some(CommandId::Fcall) => return fcall_cmd(argv, store, now_ms),
        Some(CommandId::FcallRo) => return fcall_cmd(argv, store, now_ms),
        Some(CommandId::Ssubscribe) => return ssubscribe_cmd(argv),
        Some(CommandId::Sunsubscribe) => return sunsubscribe_cmd(argv),
        Some(CommandId::Spublish) => return spublish_cmd(argv),
        Some(CommandId::SortRo) => return sort_cmd(argv, store, now_ms),
        Some(CommandId::Readonly) => return readonly_cmd(argv),
        Some(CommandId::Readwrite) => return readwrite_cmd(argv),
        Some(CommandId::Zrangestore) => return zrangestore_cmd(argv, store, now_ms),
        None => {}
    }

    let cmd = std::str::from_utf8(raw_cmd).map_err(|_| CommandError::InvalidUtf8Argument)?;
    let args_preview = build_unknown_args_preview(argv);
    Err(CommandError::UnknownCommand {
        command: trim_and_cap_string(cmd, 128),
        args_preview,
    })
}

pub fn is_write_command(cmd: &[u8]) -> bool {
    let Some(id) = classify_command(cmd) else {
        return false;
    };
    matches!(
        id,
        CommandId::Set
            | CommandId::Del
            | CommandId::Incr
            | CommandId::Expire
            | CommandId::Pexpire
            | CommandId::Expireat
            | CommandId::Pexpireat
            | CommandId::Append
            | CommandId::Mset
            | CommandId::Setnx
            | CommandId::Getset
            | CommandId::Incrby
            | CommandId::Decrby
            | CommandId::Decr
            | CommandId::Persist
            | CommandId::Rename
            | CommandId::Renamenx
            | CommandId::Flushdb
            | CommandId::Hset
            | CommandId::Hdel
            | CommandId::Hmset
            | CommandId::Hincrby
            | CommandId::Hsetnx
            | CommandId::Lpush
            | CommandId::Rpush
            | CommandId::Lpop
            | CommandId::Rpop
            | CommandId::Lset
            | CommandId::Sadd
            | CommandId::Srem
            | CommandId::Zadd
            | CommandId::Zrem
            | CommandId::Zincrby
            | CommandId::Zpopmin
            | CommandId::Zpopmax
            | CommandId::Geoadd
            | CommandId::Xadd
            | CommandId::Xdel
            | CommandId::Xtrim
            | CommandId::Xgroup
            | CommandId::Xclaim
            | CommandId::Xautoclaim
            | CommandId::Setex
            | CommandId::Psetex
            | CommandId::Getdel
            | CommandId::Setrange
            | CommandId::Incrbyfloat
            | CommandId::Spop
            | CommandId::Setbit
            | CommandId::Linsert
            | CommandId::Lrem
            | CommandId::Rpoplpush
            | CommandId::Hincrbyfloat
            | CommandId::Ltrim
            | CommandId::Lpushx
            | CommandId::Rpushx
            | CommandId::Lmove
            | CommandId::Smove
            | CommandId::Sinterstore
            | CommandId::Sunionstore
            | CommandId::Sdiffstore
            | CommandId::Zremrangebyrank
            | CommandId::Zremrangebyscore
            | CommandId::Zremrangebylex
            | CommandId::Pfadd
            | CommandId::Pfmerge
            | CommandId::Getex
            | CommandId::Bitop
            | CommandId::Zunionstore
            | CommandId::Zinterstore
            | CommandId::Restore
            | CommandId::Sort
            | CommandId::Copy
            | CommandId::Unlink
            | CommandId::Xack
            | CommandId::Xsetid
            | CommandId::Zrangestore
    )
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
    Xreadgroup,
    Xclaim,
    Xautoclaim,
    Xpending,
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
    Sintercard,
    Lcs,
    Lmpop,
    Zmpop,
    Slowlog,
    Memory,
    Substr,
    Bitop,
    Zunionstore,
    Zinterstore,
    Save,
    Bgsave,
    Bgrewriteaof,
    Lastsave,
    Swapdb,
    Blpop,
    Brpop,
    Blmove,
    Blmpop,
    Subscribe,
    Unsubscribe,
    Psubscribe,
    Punsubscribe,
    Publish,
    Pubsub,
    Msetnx,
    Brpoplpush,
    Zdiff,
    Zdiffstore,
    Zinter,
    Zunion,
    Zintercard,
    Eval,
    Evalsha,
    EvalRo,
    EvalshaRo,
    Fcall,
    FcallRo,
    Script,
    Debug,
    Role,
    Shutdown,
    Move,
    Latency,
    Bitfield,
    Georadius,
    Georadiusbymember,
    Geosearch,
    Geosearchstore,
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
    Xack,
    Xsetid,
    Lolwut,
    Waitaof,
    Cluster,
    Replicaof,
    Function,
    Ssubscribe,
    Sunsubscribe,
    Spublish,
    SortRo,
    Readonly,
    Readwrite,
    Zrangestore,
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
            } else if eq_ascii_command(cmd, b"LCS") {
                Some(CommandId::Lcs)
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
            } else if eq_ascii_command(cmd, b"SAVE") {
                Some(CommandId::Save)
            } else if eq_ascii_command(cmd, b"SCAN") {
                Some(CommandId::Scan)
            } else if eq_ascii_command(cmd, b"XADD") {
                Some(CommandId::Xadd)
            } else if eq_ascii_command(cmd, b"XLEN") {
                Some(CommandId::Xlen)
            } else if eq_ascii_command(cmd, b"XDEL") {
                Some(CommandId::Xdel)
            } else if eq_ascii_command(cmd, b"XACK") {
                Some(CommandId::Xack)
            } else if eq_ascii_command(cmd, b"EVAL") {
                Some(CommandId::Eval)
            } else if eq_ascii_command(cmd, b"ROLE") {
                Some(CommandId::Role)
            } else if eq_ascii_command(cmd, b"MOVE") {
                Some(CommandId::Move)
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
            } else if eq_ascii_command(cmd, b"LMPOP") {
                Some(CommandId::Lmpop)
            } else if eq_ascii_command(cmd, b"ZMPOP") {
                Some(CommandId::Zmpop)
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
            } else if eq_ascii_command(cmd, b"ZDIFF") {
                Some(CommandId::Zdiff)
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
            } else if eq_ascii_command(cmd, b"BLPOP") {
                Some(CommandId::Blpop)
            } else if eq_ascii_command(cmd, b"BRPOP") {
                Some(CommandId::Brpop)
            } else if eq_ascii_command(cmd, b"DEBUG") {
                Some(CommandId::Debug)
            } else if eq_ascii_command(cmd, b"FCALL") {
                Some(CommandId::Fcall)
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
            } else if eq_ascii_command(cmd, b"XCLAIM") {
                Some(CommandId::Xclaim)
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
            } else if eq_ascii_command(cmd, b"MEMORY") {
                Some(CommandId::Memory)
            } else if eq_ascii_command(cmd, b"BGSAVE") {
                Some(CommandId::Bgsave)
            } else if eq_ascii_command(cmd, b"MSETNX") {
                Some(CommandId::Msetnx)
            } else if eq_ascii_command(cmd, b"ZINTER") {
                Some(CommandId::Zinter)
            } else if eq_ascii_command(cmd, b"ZUNION") {
                Some(CommandId::Zunion)
            } else if eq_ascii_command(cmd, b"SWAPDB") {
                Some(CommandId::Swapdb)
            } else if eq_ascii_command(cmd, b"BLMOVE") {
                Some(CommandId::Blmove)
            } else if eq_ascii_command(cmd, b"BLMPOP") {
                Some(CommandId::Blmpop)
            } else if eq_ascii_command(cmd, b"PUBSUB") {
                Some(CommandId::Pubsub)
            } else if eq_ascii_command(cmd, b"SCRIPT") {
                Some(CommandId::Script)
            } else if eq_ascii_command(cmd, b"XSETID") {
                Some(CommandId::Xsetid)
            } else if eq_ascii_command(cmd, b"LOLWUT") {
                Some(CommandId::Lolwut)
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
            } else if eq_ascii_command(cmd, b"SLOWLOG") {
                Some(CommandId::Slowlog)
            } else if eq_ascii_command(cmd, b"EVALSHA") {
                Some(CommandId::Evalsha)
            } else if eq_ascii_command(cmd, b"EVAL_RO") {
                Some(CommandId::EvalRo)
            } else if eq_ascii_command(cmd, b"LATENCY") {
                Some(CommandId::Latency)
            } else if eq_ascii_command(cmd, b"PUBLISH") {
                Some(CommandId::Publish)
            } else if eq_ascii_command(cmd, b"WAITAOF") {
                Some(CommandId::Waitaof)
            } else if eq_ascii_command(cmd, b"CLUSTER") {
                Some(CommandId::Cluster)
            } else if eq_ascii_command(cmd, b"SLAVEOF") {
                Some(CommandId::Replicaof)
            } else if eq_ascii_command(cmd, b"SORT_RO") {
                Some(CommandId::SortRo)
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
            } else if eq_ascii_command(cmd, b"XPENDING") {
                Some(CommandId::Xpending)
            } else if eq_ascii_command(cmd, b"LASTSAVE") {
                Some(CommandId::Lastsave)
            } else if eq_ascii_command(cmd, b"SHUTDOWN") {
                Some(CommandId::Shutdown)
            } else if eq_ascii_command(cmd, b"BITFIELD") {
                Some(CommandId::Bitfield)
            } else if eq_ascii_command(cmd, b"FUNCTION") {
                Some(CommandId::Function)
            } else if eq_ascii_command(cmd, b"SPUBLISH") {
                Some(CommandId::Spublish)
            } else if eq_ascii_command(cmd, b"READONLY") {
                Some(CommandId::Readonly)
            } else if eq_ascii_command(cmd, b"FCALL_RO") {
                Some(CommandId::FcallRo)
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
            } else if eq_ascii_command(cmd, b"SUBSCRIBE") {
                Some(CommandId::Subscribe)
            } else if eq_ascii_command(cmd, b"GEORADIUS") {
                Some(CommandId::Georadius)
            } else if eq_ascii_command(cmd, b"GEOSEARCH") {
                Some(CommandId::Geosearch)
            } else if eq_ascii_command(cmd, b"REPLICAOF") {
                Some(CommandId::Replicaof)
            } else if eq_ascii_command(cmd, b"READWRITE") {
                Some(CommandId::Readwrite)
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
            } else if eq_ascii_command(cmd, b"XAUTOCLAIM") {
                Some(CommandId::Xautoclaim)
            } else if eq_ascii_command(cmd, b"XREADGROUP") {
                Some(CommandId::Xreadgroup)
            } else if eq_ascii_command(cmd, b"SINTERCARD") {
                Some(CommandId::Sintercard)
            } else if eq_ascii_command(cmd, b"PSUBSCRIBE") {
                Some(CommandId::Psubscribe)
            } else if eq_ascii_command(cmd, b"BRPOPLPUSH") {
                Some(CommandId::Brpoplpush)
            } else if eq_ascii_command(cmd, b"EVALSHA_RO") {
                Some(CommandId::EvalshaRo)
            } else if eq_ascii_command(cmd, b"ZDIFFSTORE") {
                Some(CommandId::Zdiffstore)
            } else if eq_ascii_command(cmd, b"ZINTERCARD") {
                Some(CommandId::Zintercard)
            } else if eq_ascii_command(cmd, b"SSUBSCRIBE") {
                Some(CommandId::Ssubscribe)
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
            } else if eq_ascii_command(cmd, b"UNSUBSCRIBE") {
                Some(CommandId::Unsubscribe)
            } else if eq_ascii_command(cmd, b"ZRANGESTORE") {
                Some(CommandId::Zrangestore)
            } else {
                None
            }
        }
        12 => {
            if eq_ascii_command(cmd, b"HINCRBYFLOAT") {
                Some(CommandId::Hincrbyfloat)
            } else if eq_ascii_command(cmd, b"BGREWRITEAOF") {
                Some(CommandId::Bgrewriteaof)
            } else if eq_ascii_command(cmd, b"PUNSUBSCRIBE") {
                Some(CommandId::Punsubscribe)
            } else if eq_ascii_command(cmd, b"SUNSUBSCRIBE") {
                Some(CommandId::Sunsubscribe)
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
            } else if eq_ascii_command(cmd, b"GEOSEARCHSTORE") {
                Some(CommandId::Geosearchstore)
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
        17 => {
            if eq_ascii_command(cmd, b"GEORADIUSBYMEMBER") {
                Some(CommandId::Georadiusbymember)
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

    // Expiry mode: at most one of EX/PX/EXAT/PXAT/KEEPTTL
    enum ExpiryMode {
        None,
        Px(u64),
        Ex(u64),
        Pxat(u64),
        Exat(u64),
        KeepTtl,
    }
    let mut expiry_mode = ExpiryMode::None;
    let mut nx = false;
    let mut xx = false;
    let mut get = false;

    let mut options = argv[3..].iter();
    while let Some(option_arg) = options.next() {
        let option =
            std::str::from_utf8(option_arg).map_err(|_| CommandError::InvalidUtf8Argument)?;
        if option.eq_ignore_ascii_case("PX") {
            if !matches!(expiry_mode, ExpiryMode::None) {
                return Err(CommandError::SyntaxError);
            }
            let Some(ttl_arg) = options.next() else {
                return Err(CommandError::SyntaxError);
            };
            expiry_mode = ExpiryMode::Px(parse_u64_arg(ttl_arg)?);
        } else if option.eq_ignore_ascii_case("EX") {
            if !matches!(expiry_mode, ExpiryMode::None) {
                return Err(CommandError::SyntaxError);
            }
            let Some(seconds_arg) = options.next() else {
                return Err(CommandError::SyntaxError);
            };
            expiry_mode = ExpiryMode::Ex(parse_u64_arg(seconds_arg)?);
        } else if option.eq_ignore_ascii_case("PXAT") {
            if !matches!(expiry_mode, ExpiryMode::None) {
                return Err(CommandError::SyntaxError);
            }
            let Some(ts_arg) = options.next() else {
                return Err(CommandError::SyntaxError);
            };
            expiry_mode = ExpiryMode::Pxat(parse_u64_arg(ts_arg)?);
        } else if option.eq_ignore_ascii_case("EXAT") {
            if !matches!(expiry_mode, ExpiryMode::None) {
                return Err(CommandError::SyntaxError);
            }
            let Some(ts_arg) = options.next() else {
                return Err(CommandError::SyntaxError);
            };
            expiry_mode = ExpiryMode::Exat(parse_u64_arg(ts_arg)?);
        } else if option.eq_ignore_ascii_case("KEEPTTL") {
            if !matches!(expiry_mode, ExpiryMode::None) {
                return Err(CommandError::SyntaxError);
            }
            expiry_mode = ExpiryMode::KeepTtl;
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

    match expiry_mode {
        ExpiryMode::None => {
            store.set(argv[1].clone(), argv[2].clone(), None, now_ms);
        }
        ExpiryMode::Px(ms) => {
            store.set(argv[1].clone(), argv[2].clone(), Some(ms), now_ms);
        }
        ExpiryMode::Ex(sec) => {
            store.set(
                argv[1].clone(),
                argv[2].clone(),
                Some(sec.saturating_mul(1000)),
                now_ms,
            );
        }
        ExpiryMode::Pxat(abs_ms) => {
            store.set_with_abs_expiry(argv[1].clone(), argv[2].clone(), Some(abs_ms));
        }
        ExpiryMode::Exat(abs_sec) => {
            store.set_with_abs_expiry(
                argv[1].clone(),
                argv[2].clone(),
                Some(abs_sec.saturating_mul(1000)),
            );
        }
        ExpiryMode::KeepTtl => {
            let existing_expiry = store.get_expires_at_ms(&argv[1], now_ms);
            store.set_with_abs_expiry(argv[1].clone(), argv[2].clone(), existing_expiry);
        }
    }

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
        PttlValue::Remaining(ms) => ms.saturating_add(500) / 1000,
    };
    Ok(RespFrame::Integer(value))
}

fn expiretime(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 2 {
        return Err(CommandError::WrongArity("EXPIRETIME"));
    }
    if !store.exists(&argv[1], now_ms) {
        return Ok(RespFrame::Integer(-2));
    }
    let value = match store.get_expires_at_ms(&argv[1], now_ms) {
        Some(abs_ms) => (abs_ms / 1000) as i64,
        None => -1,
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
    if !store.exists(&argv[1], now_ms) {
        return Ok(RespFrame::Integer(-2));
    }
    let value = match store.get_expires_at_ms(&argv[1], now_ms) {
        Some(abs_ms) => abs_ms as i64,
        None => -1,
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
    if argv.len() < 2 || argv.len() > 3 {
        return Err(CommandError::WrongArity("LPOP"));
    }
    if argv.len() == 3 {
        let count = parse_u64_arg(&argv[2])? as usize;
        // Redis returns nil (not empty array) when key doesn't exist with COUNT
        if !store.exists(&argv[1], now_ms) {
            return Ok(RespFrame::BulkString(None));
        }
        let mut result = Vec::new();
        for _ in 0..count {
            match store.lpop(&argv[1], now_ms)? {
                Some(v) => result.push(RespFrame::BulkString(Some(v))),
                None => break,
            }
        }
        return Ok(RespFrame::Array(Some(result)));
    }
    let value = store.lpop(&argv[1], now_ms)?;
    Ok(RespFrame::BulkString(value))
}

fn rpop(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() < 2 || argv.len() > 3 {
        return Err(CommandError::WrongArity("RPOP"));
    }
    if argv.len() == 3 {
        let count = parse_u64_arg(&argv[2])? as usize;
        // Redis returns nil (not empty array) when key doesn't exist with COUNT
        if !store.exists(&argv[1], now_ms) {
            return Ok(RespFrame::BulkString(None));
        }
        let mut result = Vec::new();
        for _ in 0..count {
            match store.rpop(&argv[1], now_ms)? {
                Some(v) => result.push(RespFrame::BulkString(Some(v))),
                None => break,
            }
        }
        return Ok(RespFrame::Array(Some(result)));
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
    let val = text
        .parse::<f64>()
        .map_err(|_| CommandError::Store(StoreError::ValueNotFloat))?;
    if val.is_nan() {
        return Err(CommandError::Store(StoreError::ValueNotFloat));
    }
    Ok(val)
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
    // ZADD key [NX|XX] [GT|LT] [CH] score member [score member ...]
    if argv.len() < 4 {
        return Err(CommandError::WrongArity("ZADD"));
    }

    let mut nx = false;
    let mut xx = false;
    let mut gt = false;
    let mut lt = false;
    let mut ch = false;
    let mut i = 2;

    // Parse option flags
    while i < argv.len() {
        let opt = std::str::from_utf8(&argv[i]).unwrap_or("");
        if opt.eq_ignore_ascii_case("NX") {
            nx = true;
        } else if opt.eq_ignore_ascii_case("XX") {
            xx = true;
        } else if opt.eq_ignore_ascii_case("GT") {
            gt = true;
        } else if opt.eq_ignore_ascii_case("LT") {
            lt = true;
        } else if opt.eq_ignore_ascii_case("CH") {
            ch = true;
        } else {
            break; // Start of score-member pairs
        }
        i += 1;
    }

    // NX and XX are mutually exclusive
    if nx && xx {
        return Ok(RespFrame::Error(
            "ERR XX and NX options at the same time are not compatible".to_string(),
        ));
    }
    // NX and GT/LT are mutually exclusive
    if nx && (gt || lt) {
        return Ok(RespFrame::Error(
            "ERR GT, LT, and NX options at the same time are not compatible".to_string(),
        ));
    }

    let remaining = argv.len() - i;
    if remaining < 2 || !remaining.is_multiple_of(2) {
        return Err(CommandError::WrongArity("ZADD"));
    }

    let mut pairs = Vec::with_capacity(remaining / 2);
    while i + 1 < argv.len() {
        let score = parse_f64_arg(&argv[i])?;
        pairs.push((score, argv[i + 1].clone()));
        i += 2;
    }

    let opts = fr_store::ZaddOptions { nx, xx, gt, lt, ch };
    let (count, _changed) = store.zadd_with_options(&argv[1], &pairs, opts, now_ms)?;
    Ok(RespFrame::Integer(i64::try_from(count).unwrap_or(i64::MAX)))
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
    // ZRANGE key min max [BYSCORE | BYLEX] [REV] [LIMIT offset count] [WITHSCORES]
    if argv.len() < 4 {
        return Err(CommandError::WrongArity("ZRANGE"));
    }

    let mut byscore = false;
    let mut bylex = false;
    let mut rev = false;
    let mut withscores = false;
    let mut limit_offset: Option<usize> = None;
    let mut limit_count: Option<usize> = None;

    let mut i = 4;
    while i < argv.len() {
        let opt = std::str::from_utf8(&argv[i]).map_err(|_| CommandError::InvalidUtf8Argument)?;
        if opt.eq_ignore_ascii_case("BYSCORE") {
            byscore = true;
            i += 1;
        } else if opt.eq_ignore_ascii_case("BYLEX") {
            bylex = true;
            i += 1;
        } else if opt.eq_ignore_ascii_case("REV") {
            rev = true;
            i += 1;
        } else if opt.eq_ignore_ascii_case("WITHSCORES") {
            withscores = true;
            i += 1;
        } else if opt.eq_ignore_ascii_case("LIMIT") {
            if i + 2 >= argv.len() {
                return Err(CommandError::SyntaxError);
            }
            limit_offset = Some(parse_u64_arg(&argv[i + 1])? as usize);
            limit_count = Some(parse_u64_arg(&argv[i + 2])? as usize);
            i += 3;
        } else {
            return Err(CommandError::SyntaxError);
        }
    }

    if byscore && bylex {
        return Err(CommandError::SyntaxError);
    }

    // LIMIT only valid with BYSCORE or BYLEX
    if limit_offset.is_some() && !byscore && !bylex {
        return Err(CommandError::SyntaxError);
    }

    if byscore {
        let min = parse_score_bound(&argv[2])?;
        let max = parse_score_bound(&argv[3])?;
        let (lo, hi) = if rev { (max, min) } else { (min, max) };
        let mut pairs = store.zrangebyscore_withscores(&argv[1], lo, hi, now_ms)?;
        if rev {
            pairs.reverse();
        }
        if let (Some(offset), Some(count)) = (limit_offset, limit_count) {
            pairs = pairs.into_iter().skip(offset).take(count).collect();
        }
        zrange_emit(pairs, withscores)
    } else if bylex {
        let min_lex = &argv[2];
        let max_lex = &argv[3];
        let (lo, hi) = if rev {
            (max_lex.as_slice(), min_lex.as_slice())
        } else {
            (min_lex.as_slice(), max_lex.as_slice())
        };
        let mut members = store.zrangebylex(&argv[1], lo, hi, now_ms)?;
        if rev {
            members.reverse();
        }
        if let (Some(offset), Some(count)) = (limit_offset, limit_count) {
            members = members.into_iter().skip(offset).take(count).collect();
        }
        if withscores {
            // For lex ranges, get scores individually
            let mut frames = Vec::with_capacity(members.len() * 2);
            for m in &members {
                let score = store.zscore(&argv[1], m, now_ms)?.unwrap_or(0.0);
                frames.push(RespFrame::BulkString(Some(m.clone())));
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
    } else {
        // Default: by rank (index)
        let start = parse_i64_arg(&argv[2])?;
        let stop = parse_i64_arg(&argv[3])?;
        if rev {
            let mut pairs = store.zrange_withscores(&argv[1], start, stop, now_ms)?;
            pairs.reverse();
            zrange_emit(pairs, withscores)
        } else if withscores {
            let pairs = store.zrange_withscores(&argv[1], start, stop, now_ms)?;
            zrange_emit(pairs, true)
        } else {
            let members = store.zrange(&argv[1], start, stop, now_ms)?;
            let frames = members
                .into_iter()
                .map(|m| RespFrame::BulkString(Some(m)))
                .collect();
            Ok(RespFrame::Array(Some(frames)))
        }
    }
}

/// Helper to emit ZRANGE results with optional WITHSCORES.
fn zrange_emit(pairs: Vec<(Vec<u8>, f64)>, withscores: bool) -> Result<RespFrame, CommandError> {
    if withscores {
        let mut frames = Vec::with_capacity(pairs.len() * 2);
        for (member, score) in pairs {
            frames.push(RespFrame::BulkString(Some(member)));
            frames.push(RespFrame::BulkString(Some(score.to_string().into_bytes())));
        }
        Ok(RespFrame::Array(Some(frames)))
    } else {
        let frames = pairs
            .into_iter()
            .map(|(m, _)| RespFrame::BulkString(Some(m)))
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

/// Parse optional WITHSCORES / LIMIT offset count from argv starting at `start_idx`.
fn parse_zrangebyscore_opts(
    argv: &[Vec<u8>],
    start_idx: usize,
) -> Result<(bool, Option<usize>, Option<usize>), CommandError> {
    let mut withscores = false;
    let mut limit_offset = None;
    let mut limit_count = None;
    let mut i = start_idx;
    while i < argv.len() {
        let opt = std::str::from_utf8(&argv[i]).map_err(|_| CommandError::InvalidUtf8Argument)?;
        if opt.eq_ignore_ascii_case("WITHSCORES") {
            withscores = true;
            i += 1;
        } else if opt.eq_ignore_ascii_case("LIMIT") {
            if i + 2 >= argv.len() {
                return Err(CommandError::SyntaxError);
            }
            limit_offset = Some(parse_u64_arg(&argv[i + 1])? as usize);
            limit_count = Some(parse_u64_arg(&argv[i + 2])? as usize);
            i += 3;
        } else {
            return Err(CommandError::SyntaxError);
        }
    }
    Ok((withscores, limit_offset, limit_count))
}

fn zrangebyscore(
    argv: &[Vec<u8>],
    store: &mut Store,
    now_ms: u64,
) -> Result<RespFrame, CommandError> {
    // ZRANGEBYSCORE key min max [WITHSCORES] [LIMIT offset count]
    if argv.len() < 4 {
        return Err(CommandError::WrongArity("ZRANGEBYSCORE"));
    }
    let min = parse_score_bound(&argv[2])?;
    let max = parse_score_bound(&argv[3])?;
    let (withscores, limit_offset, limit_count) = parse_zrangebyscore_opts(argv, 4)?;
    let mut pairs = store.zrangebyscore_withscores(&argv[1], min, max, now_ms)?;
    if let (Some(offset), Some(count)) = (limit_offset, limit_count) {
        pairs = pairs.into_iter().skip(offset).take(count).collect();
    }
    zrange_emit(pairs, withscores)
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
    if argv.len() < 2 || argv.len() > 3 {
        return Err(CommandError::WrongArity("ZPOPMIN"));
    }
    if argv.len() == 3 {
        let count = parse_u64_arg(&argv[2])? as usize;
        let pairs = store.zpopmin_count(&argv[1], count, now_ms)?;
        let mut frames = Vec::with_capacity(pairs.len() * 2);
        for (member, score) in pairs {
            frames.push(RespFrame::BulkString(Some(member)));
            frames.push(RespFrame::BulkString(Some(score.to_string().into_bytes())));
        }
        return Ok(RespFrame::Array(Some(frames)));
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
    if argv.len() < 2 || argv.len() > 3 {
        return Err(CommandError::WrongArity("ZPOPMAX"));
    }
    if argv.len() == 3 {
        let count = parse_u64_arg(&argv[2])? as usize;
        let pairs = store.zpopmax_count(&argv[1], count, now_ms)?;
        let mut frames = Vec::with_capacity(pairs.len() * 2);
        for (member, score) in pairs {
            frames.push(RespFrame::BulkString(Some(member)));
            frames.push(RespFrame::BulkString(Some(score.to_string().into_bytes())));
        }
        return Ok(RespFrame::Array(Some(frames)));
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

/// Shared core for GEORADIUS, GEORADIUSBYMEMBER, GEOSEARCH, GEOSEARCHSTORE.
/// Returns filtered (member, score, distance, lon, lat) tuples sorted by distance.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn geo_search_core(
    store: &mut Store,
    key: &[u8],
    center_lon: f64,
    center_lat: f64,
    radius_m: f64,
    count: Option<usize>,
    asc: bool,
    now_ms: u64,
) -> Result<Vec<(Vec<u8>, f64, f64, f64, f64)>, CommandError> {
    let members = store.zrange_withscores(key, 0, -1, now_ms)?;
    let mut results: Vec<(Vec<u8>, f64, f64, f64, f64)> = Vec::new();
    for (member, score) in members {
        let Some((lon, lat)) = geo_decode_score(score) else {
            continue;
        };
        let dist = geo_distance_m(center_lon, center_lat, lon, lat);
        if dist <= radius_m {
            results.push((member, score, dist, lon, lat));
        }
    }
    if asc {
        results.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));
    } else {
        results.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
    }
    if let Some(limit) = count {
        results.truncate(limit);
    }
    Ok(results)
}

/// Formats GEOSEARCH-family results as RESP frames.
fn geo_search_reply(
    results: &[(Vec<u8>, f64, f64, f64, f64)],
    withcoord: bool,
    withdist: bool,
    withhash: bool,
    to_meter: f64,
) -> RespFrame {
    let frames: Vec<RespFrame> = results
        .iter()
        .map(|(member, score, dist, lon, lat)| {
            if !withcoord && !withdist && !withhash {
                return RespFrame::BulkString(Some(member.clone()));
            }
            let mut parts = vec![RespFrame::BulkString(Some(member.clone()))];
            if withdist {
                let d = dist / to_meter;
                let normalized = if d == 0.0 { 0.0 } else { d };
                parts.push(RespFrame::BulkString(Some(
                    format!("{normalized:.4}").into_bytes(),
                )));
            }
            if withhash {
                parts.push(RespFrame::Integer(*score as i64));
            }
            if withcoord {
                parts.push(RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(lon.to_string().into_bytes())),
                    RespFrame::BulkString(Some(lat.to_string().into_bytes())),
                ])));
            }
            RespFrame::Array(Some(parts))
        })
        .collect();
    RespFrame::Array(Some(frames))
}

/// Parse optional flags WITHCOORD, WITHDIST, WITHHASH, COUNT N [ANY], ASC, DESC from argv starting
/// at `start`. Returns (withcoord, withdist, withhash, count, any, asc, to_meter, store_key).
#[allow(clippy::type_complexity)]
fn parse_geo_search_flags(
    argv: &[Vec<u8>],
    start: usize,
    default_unit: f64,
) -> Result<(bool, bool, bool, Option<usize>, bool, bool, f64), CommandError> {
    let mut withcoord = false;
    let mut withdist = false;
    let mut withhash = false;
    let mut count: Option<usize> = None;
    let mut any = false;
    let mut asc = true;
    let mut to_meter = default_unit;
    let mut i = start;
    while i < argv.len() {
        if eq_ascii_command(&argv[i], b"WITHCOORD") {
            withcoord = true;
        } else if eq_ascii_command(&argv[i], b"WITHDIST") {
            withdist = true;
        } else if eq_ascii_command(&argv[i], b"WITHHASH") {
            withhash = true;
        } else if eq_ascii_command(&argv[i], b"COUNT") {
            i += 1;
            if i >= argv.len() {
                return Err(CommandError::SyntaxError);
            }
            let n = parse_i64_arg(&argv[i])?;
            if n <= 0 {
                return Err(CommandError::SyntaxError);
            }
            count = Some(n as usize);
        } else if eq_ascii_command(&argv[i], b"ANY") {
            any = true;
        } else if eq_ascii_command(&argv[i], b"ASC") {
            asc = true;
        } else if eq_ascii_command(&argv[i], b"DESC") {
            asc = false;
        } else if eq_ascii_command(&argv[i], b"STORE") || eq_ascii_command(&argv[i], b"STOREDIST") {
            // Skip STORE/STOREDIST key for GEORADIUS (not supported in read-only mode)
            i += 1;
        } else if let Some(m) = geo_unit_to_meters(&argv[i]) {
            to_meter = m;
        } else {
            // unknown flag, ignore for forward compatibility
        }
        i += 1;
    }
    Ok((withcoord, withdist, withhash, count, any, asc, to_meter))
}

fn georadius(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    // GEORADIUS key longitude latitude radius m|km|ft|mi [WITHCOORD] [WITHDIST] [WITHHASH] [COUNT count [ANY]] [ASC|DESC]
    if argv.len() < 6 {
        return Err(CommandError::WrongArity("GEORADIUS"));
    }
    let center_lon = match parse_geo_f64(&argv[2]) {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let center_lat = match parse_geo_f64(&argv[3]) {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let radius = match parse_geo_f64(&argv[4]) {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let unit_mult = match geo_unit_to_meters(&argv[5]) {
        Some(m) => m,
        None => {
            return Ok(RespFrame::Error(
                "ERR unsupported unit provided. please use M, KM, FT, MI".to_string(),
            ));
        }
    };
    let radius_m = radius * unit_mult;
    let (withcoord, withdist, withhash, count, _any, asc, _) =
        parse_geo_search_flags(argv, 6, unit_mult)?;
    let results = geo_search_core(
        store, &argv[1], center_lon, center_lat, radius_m, count, asc, now_ms,
    )?;
    Ok(geo_search_reply(
        &results, withcoord, withdist, withhash, unit_mult,
    ))
}

fn georadiusbymember(
    argv: &[Vec<u8>],
    store: &mut Store,
    now_ms: u64,
) -> Result<RespFrame, CommandError> {
    // GEORADIUSBYMEMBER key member radius m|km|ft|mi [WITHCOORD] [WITHDIST] [WITHHASH] [COUNT count [ANY]] [ASC|DESC]
    if argv.len() < 5 {
        return Err(CommandError::WrongArity("GEORADIUSBYMEMBER"));
    }
    let score = store.zscore(&argv[1], &argv[2], now_ms)?;
    let Some(score) = score else {
        return Ok(RespFrame::Array(Some(Vec::new())));
    };
    let Some((center_lon, center_lat)) = geo_decode_score(score) else {
        return Ok(RespFrame::Array(Some(Vec::new())));
    };
    let radius = match parse_geo_f64(&argv[3]) {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let unit_mult = match geo_unit_to_meters(&argv[4]) {
        Some(m) => m,
        None => {
            return Ok(RespFrame::Error(
                "ERR unsupported unit provided. please use M, KM, FT, MI".to_string(),
            ));
        }
    };
    let radius_m = radius * unit_mult;
    let (withcoord, withdist, withhash, count, _any, asc, _) =
        parse_geo_search_flags(argv, 5, unit_mult)?;
    let results = geo_search_core(
        store, &argv[1], center_lon, center_lat, radius_m, count, asc, now_ms,
    )?;
    Ok(geo_search_reply(
        &results, withcoord, withdist, withhash, unit_mult,
    ))
}

fn geosearch(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    // GEOSEARCH key FROMMEMBER member | FROMLONLAT lon lat BYRADIUS radius m|km|ft|mi | BYBOX width height m|km|ft|mi
    //   [ASC|DESC] [COUNT count [ANY]] [WITHCOORD] [WITHDIST] [WITHHASH]
    if argv.len() < 4 {
        return Err(CommandError::WrongArity("GEOSEARCH"));
    }
    let mut i = 2;
    let mut center_lon: Option<f64> = None;
    let mut center_lat: Option<f64> = None;
    let mut radius_m: Option<f64> = None;
    let mut box_width_m: Option<f64> = None;
    let mut box_height_m: Option<f64> = None;
    let mut unit_mult = 1.0_f64;

    // Parse FROMMEMBER/FROMLONLAT and BYRADIUS/BYBOX
    while i < argv.len() {
        if eq_ascii_command(&argv[i], b"FROMMEMBER") {
            i += 1;
            if i >= argv.len() {
                return Err(CommandError::SyntaxError);
            }
            let score = store.zscore(&argv[1], &argv[i], now_ms)?;
            if let Some(score) = score
                && let Some((lon, lat)) = geo_decode_score(score)
            {
                center_lon = Some(lon);
                center_lat = Some(lat);
            }
            if center_lon.is_none() {
                return Ok(RespFrame::Error(
                    "ERR could not decode requested zset member".to_string(),
                ));
            }
        } else if eq_ascii_command(&argv[i], b"FROMLONLAT") {
            if i + 2 >= argv.len() {
                return Err(CommandError::SyntaxError);
            }
            center_lon = Some(match parse_geo_f64(&argv[i + 1]) {
                Ok(v) => v,
                Err(e) => return Ok(e),
            });
            center_lat = Some(match parse_geo_f64(&argv[i + 2]) {
                Ok(v) => v,
                Err(e) => return Ok(e),
            });
            i += 2;
        } else if eq_ascii_command(&argv[i], b"BYRADIUS") {
            if i + 2 >= argv.len() {
                return Err(CommandError::SyntaxError);
            }
            let r = match parse_geo_f64(&argv[i + 1]) {
                Ok(v) => v,
                Err(e) => return Ok(e),
            };
            let um = match geo_unit_to_meters(&argv[i + 2]) {
                Some(m) => m,
                None => {
                    return Ok(RespFrame::Error(
                        "ERR unsupported unit provided. please use M, KM, FT, MI".to_string(),
                    ));
                }
            };
            unit_mult = um;
            radius_m = Some(r * um);
            i += 2;
        } else if eq_ascii_command(&argv[i], b"BYBOX") {
            if i + 3 >= argv.len() {
                return Err(CommandError::SyntaxError);
            }
            let w = match parse_geo_f64(&argv[i + 1]) {
                Ok(v) => v,
                Err(e) => return Ok(e),
            };
            let h = match parse_geo_f64(&argv[i + 2]) {
                Ok(v) => v,
                Err(e) => return Ok(e),
            };
            let um = match geo_unit_to_meters(&argv[i + 3]) {
                Some(m) => m,
                None => {
                    return Ok(RespFrame::Error(
                        "ERR unsupported unit provided. please use M, KM, FT, MI".to_string(),
                    ));
                }
            };
            unit_mult = um;
            box_width_m = Some(w * um);
            box_height_m = Some(h * um);
            i += 3;
        } else {
            break;
        }
        i += 1;
    }

    let (Some(cx), Some(cy)) = (center_lon, center_lat) else {
        return Ok(RespFrame::Error(
            "ERR exactly one of FROMMEMBER or FROMLONLAT must be provided".to_string(),
        ));
    };

    // Parse remaining flags
    let (withcoord, withdist, withhash, count, _any, asc, _) =
        parse_geo_search_flags(argv, i, unit_mult)?;

    if let Some(rm) = radius_m {
        let results = geo_search_core(store, &argv[1], cx, cy, rm, count, asc, now_ms)?;
        Ok(geo_search_reply(
            &results, withcoord, withdist, withhash, unit_mult,
        ))
    } else if let (Some(w), Some(h)) = (box_width_m, box_height_m) {
        // BYBOX: filter by bounding box
        let members = store.zrange_withscores(&argv[1], 0, -1, now_ms)?;
        let half_w = w / 2.0;
        let half_h = h / 2.0;
        let mut results: Vec<(Vec<u8>, f64, f64, f64, f64)> = Vec::new();
        for (member, score) in members {
            let Some((lon, lat)) = geo_decode_score(score) else {
                continue;
            };
            let dist = geo_distance_m(cx, cy, lon, lat);
            // Approximate box check using lat/lon distance components
            let lat_dist = geo_distance_m(cx, cy, cx, lat);
            let lon_dist = geo_distance_m(cx, cy, lon, cy);
            if lat_dist <= half_h && lon_dist <= half_w {
                results.push((member, score, dist, lon, lat));
            }
        }
        if asc {
            results.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));
        } else {
            results.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
        }
        if let Some(limit) = count {
            results.truncate(limit);
        }
        Ok(geo_search_reply(
            &results, withcoord, withdist, withhash, unit_mult,
        ))
    } else {
        Ok(RespFrame::Error(
            "ERR exactly one of BYRADIUS or BYBOX must be provided".to_string(),
        ))
    }
}

fn geosearchstore(
    argv: &[Vec<u8>],
    store: &mut Store,
    now_ms: u64,
) -> Result<RespFrame, CommandError> {
    // GEOSEARCHSTORE destination source FROMMEMBER member | FROMLONLAT lon lat BYRADIUS radius m|km|ft|mi | BYBOX width height m|km|ft|mi
    //   [ASC|DESC] [COUNT count [ANY]] [STOREDIST]
    if argv.len() < 5 {
        return Err(CommandError::WrongArity("GEOSEARCHSTORE"));
    }
    let dest = argv[1].clone();
    let source = argv[2].clone();
    let mut storedist = false;

    // Check for STOREDIST flag
    for arg in &argv[3..] {
        if eq_ascii_command(arg, b"STOREDIST") {
            storedist = true;
        }
    }

    // Build a synthetic GEOSEARCH argv: [GEOSEARCH, source, ...rest]
    let mut synth = vec![b"GEOSEARCH".to_vec(), source];
    synth.extend_from_slice(&argv[3..]);

    // Parse the geosearch args to get center and radius/box
    let mut i = 2;
    let mut center_lon: Option<f64> = None;
    let mut center_lat: Option<f64> = None;
    let mut radius_m: Option<f64> = None;
    let mut box_width_m: Option<f64> = None;
    let mut box_height_m: Option<f64> = None;
    let mut unit_mult = 1.0_f64;

    while i < synth.len() {
        if eq_ascii_command(&synth[i], b"FROMMEMBER") {
            i += 1;
            if i >= synth.len() {
                return Err(CommandError::SyntaxError);
            }
            let score = store.zscore(&synth[1], &synth[i], now_ms)?;
            if let Some(score) = score
                && let Some((lon, lat)) = geo_decode_score(score)
            {
                center_lon = Some(lon);
                center_lat = Some(lat);
            }
            if center_lon.is_none() {
                return Ok(RespFrame::Error(
                    "ERR could not decode requested zset member".to_string(),
                ));
            }
        } else if eq_ascii_command(&synth[i], b"FROMLONLAT") {
            if i + 2 >= synth.len() {
                return Err(CommandError::SyntaxError);
            }
            center_lon = Some(match parse_geo_f64(&synth[i + 1]) {
                Ok(v) => v,
                Err(e) => return Ok(e),
            });
            center_lat = Some(match parse_geo_f64(&synth[i + 2]) {
                Ok(v) => v,
                Err(e) => return Ok(e),
            });
            i += 2;
        } else if eq_ascii_command(&synth[i], b"BYRADIUS") {
            if i + 2 >= synth.len() {
                return Err(CommandError::SyntaxError);
            }
            let r = match parse_geo_f64(&synth[i + 1]) {
                Ok(v) => v,
                Err(e) => return Ok(e),
            };
            let um = match geo_unit_to_meters(&synth[i + 2]) {
                Some(m) => m,
                None => {
                    return Ok(RespFrame::Error(
                        "ERR unsupported unit provided. please use M, KM, FT, MI".to_string(),
                    ));
                }
            };
            unit_mult = um;
            radius_m = Some(r * um);
            i += 2;
        } else if eq_ascii_command(&synth[i], b"BYBOX") {
            if i + 3 >= synth.len() {
                return Err(CommandError::SyntaxError);
            }
            let w = match parse_geo_f64(&synth[i + 1]) {
                Ok(v) => v,
                Err(e) => return Ok(e),
            };
            let h = match parse_geo_f64(&synth[i + 2]) {
                Ok(v) => v,
                Err(e) => return Ok(e),
            };
            let um = match geo_unit_to_meters(&synth[i + 3]) {
                Some(m) => m,
                None => {
                    return Ok(RespFrame::Error(
                        "ERR unsupported unit provided. please use M, KM, FT, MI".to_string(),
                    ));
                }
            };
            unit_mult = um;
            box_width_m = Some(w * um);
            box_height_m = Some(h * um);
            i += 3;
        } else {
            break;
        }
        i += 1;
    }

    let (Some(cx), Some(cy)) = (center_lon, center_lat) else {
        return Ok(RespFrame::Error(
            "ERR exactly one of FROMMEMBER or FROMLONLAT must be provided".to_string(),
        ));
    };

    let (_, _, _, count, _any, asc, _) = parse_geo_search_flags(&synth, i, unit_mult)?;

    let results = if let Some(rm) = radius_m {
        geo_search_core(store, &synth[1], cx, cy, rm, count, asc, now_ms)?
    } else if let (Some(w), Some(h)) = (box_width_m, box_height_m) {
        let members = store.zrange_withscores(&synth[1], 0, -1, now_ms)?;
        let half_w = w / 2.0;
        let half_h = h / 2.0;
        let mut res: Vec<(Vec<u8>, f64, f64, f64, f64)> = Vec::new();
        for (member, score) in members {
            let Some((lon, lat)) = geo_decode_score(score) else {
                continue;
            };
            let dist = geo_distance_m(cx, cy, lon, lat);
            let lat_dist = geo_distance_m(cx, cy, cx, lat);
            let lon_dist = geo_distance_m(cx, cy, lon, cy);
            if lat_dist <= half_h && lon_dist <= half_w {
                res.push((member, score, dist, lon, lat));
            }
        }
        if asc {
            res.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));
        } else {
            res.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
        }
        if let Some(limit) = count {
            res.truncate(limit);
        }
        res
    } else {
        return Ok(RespFrame::Error(
            "ERR exactly one of BYRADIUS or BYBOX must be provided".to_string(),
        ));
    };

    // Store results in destination key as sorted set
    let count_result = results.len() as i64;
    if results.is_empty() {
        store.del(std::slice::from_ref(&dest), now_ms);
    } else {
        let pairs: Vec<(f64, Vec<u8>)> = results
            .iter()
            .map(|(member, score, dist, _, _)| {
                if storedist {
                    (*dist / unit_mult, member.clone())
                } else {
                    (*score, member.clone())
                }
            })
            .collect();
        // Delete existing key and re-add
        store.del(std::slice::from_ref(&dest), now_ms);
        store.zadd(&dest, &pairs, now_ms)?;
    }
    Ok(RespFrame::Integer(count_result))
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

fn stream_group_info_to_frame(
    name: Vec<u8>,
    consumers: usize,
    pending: usize,
    last_delivered_id: StreamId,
) -> RespFrame {
    RespFrame::Array(Some(vec![
        RespFrame::BulkString(Some(b"name".to_vec())),
        RespFrame::BulkString(Some(name)),
        RespFrame::BulkString(Some(b"consumers".to_vec())),
        RespFrame::Integer(i64::try_from(consumers).unwrap_or(i64::MAX)),
        RespFrame::BulkString(Some(b"pending".to_vec())),
        RespFrame::Integer(i64::try_from(pending).unwrap_or(i64::MAX)),
        RespFrame::BulkString(Some(b"last-delivered-id".to_vec())),
        RespFrame::BulkString(Some(format_stream_id(last_delivered_id))),
        RespFrame::BulkString(Some(b"entries-read".to_vec())),
        RespFrame::BulkString(None),
        RespFrame::BulkString(Some(b"lag".to_vec())),
        RespFrame::BulkString(None),
    ]))
}

fn stream_consumer_info_to_frame(name: Vec<u8>) -> RespFrame {
    RespFrame::Array(Some(vec![
        RespFrame::BulkString(Some(b"name".to_vec())),
        RespFrame::BulkString(Some(name)),
        RespFrame::BulkString(Some(b"pending".to_vec())),
        RespFrame::Integer(0),
        RespFrame::BulkString(Some(b"idle".to_vec())),
        RespFrame::Integer(0),
        RespFrame::BulkString(Some(b"inactive".to_vec())),
        RespFrame::Integer(0),
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

fn xreadgroup(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() < 7 {
        return Err(CommandError::WrongArity("XREADGROUP"));
    }
    if !eq_ascii_command(&argv[1], b"GROUP") {
        return Err(CommandError::SyntaxError);
    }

    let group = &argv[2];
    let consumer = &argv[3];

    let mut idx = 4usize;
    let mut count: Option<usize> = None;
    let mut noack = false;
    while idx < argv.len() {
        if eq_ascii_command(&argv[idx], b"COUNT") {
            if idx + 1 >= argv.len() {
                return Err(CommandError::WrongArity("XREADGROUP"));
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
        if eq_ascii_command(&argv[idx], b"NOACK") {
            noack = true;
            idx += 1;
            continue;
        }
        break;
    }

    if idx >= argv.len() || !eq_ascii_command(&argv[idx], b"STREAMS") {
        return Err(CommandError::SyntaxError);
    }
    idx += 1;

    let tail = argv.len().saturating_sub(idx);
    if tail < 2 || !tail.is_multiple_of(2) {
        return Err(CommandError::WrongArity("XREADGROUP"));
    }
    let stream_count = tail / 2;
    let keys = &argv[idx..idx + stream_count];
    let ids = &argv[idx + stream_count..];

    let mut out = Vec::new();
    for (key, id_arg) in keys.iter().zip(ids.iter()) {
        let cursor = if id_arg.as_slice() == b">" {
            StreamGroupReadCursor::NewEntries
        } else {
            let parsed = match parse_xread_id(id_arg) {
                Ok(id) => id,
                Err(reply) => return Ok(reply),
            };
            StreamGroupReadCursor::Id(parsed)
        };

        let read_options = StreamGroupReadOptions {
            cursor,
            noack,
            count,
        };
        let Some(records) = store.xreadgroup(key, group, consumer, read_options, now_ms)? else {
            return Ok(xreadgroup_nogroup_error(key, group));
        };
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

fn xclaim(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() < 6 {
        return Err(CommandError::WrongArity("XCLAIM"));
    }

    let min_idle_raw = parse_i64_arg(&argv[4])?;
    if min_idle_raw < 0 {
        return Err(CommandError::InvalidInteger);
    }
    let min_idle_time_ms = u64::try_from(min_idle_raw).unwrap_or(u64::MAX);

    let mut ids = Vec::new();
    let mut idx = 5usize;
    while idx < argv.len() {
        let arg = &argv[idx];
        if eq_ascii_command(arg, b"IDLE")
            || eq_ascii_command(arg, b"TIME")
            || eq_ascii_command(arg, b"RETRYCOUNT")
            || eq_ascii_command(arg, b"FORCE")
            || eq_ascii_command(arg, b"JUSTID")
            || eq_ascii_command(arg, b"LASTID")
        {
            break;
        }
        let id = match parse_stream_id(arg) {
            Ok(id) => id,
            Err(reply) => return Ok(reply),
        };
        ids.push(id);
        idx += 1;
    }
    if ids.is_empty() {
        return Err(CommandError::WrongArity("XCLAIM"));
    }

    let mut idle_ms: Option<u64> = None;
    let mut time_ms: Option<u64> = None;
    let mut retry_count: Option<u64> = None;
    let mut force = false;
    let mut justid = false;
    let mut last_id: Option<StreamId> = None;

    while idx < argv.len() {
        if eq_ascii_command(&argv[idx], b"IDLE") {
            if idx + 1 >= argv.len() {
                return Err(CommandError::WrongArity("XCLAIM"));
            }
            let parsed = parse_i64_arg(&argv[idx + 1])?;
            if parsed < 0 {
                return Err(CommandError::InvalidInteger);
            }
            idle_ms = Some(u64::try_from(parsed).unwrap_or(u64::MAX));
            idx += 2;
            continue;
        }
        if eq_ascii_command(&argv[idx], b"TIME") {
            if idx + 1 >= argv.len() {
                return Err(CommandError::WrongArity("XCLAIM"));
            }
            let parsed = parse_i64_arg(&argv[idx + 1])?;
            if parsed < 0 {
                return Err(CommandError::InvalidInteger);
            }
            time_ms = Some(u64::try_from(parsed).unwrap_or(u64::MAX));
            idx += 2;
            continue;
        }
        if eq_ascii_command(&argv[idx], b"RETRYCOUNT") {
            if idx + 1 >= argv.len() {
                return Err(CommandError::WrongArity("XCLAIM"));
            }
            let parsed = parse_i64_arg(&argv[idx + 1])?;
            if parsed < 0 {
                return Err(CommandError::InvalidInteger);
            }
            retry_count = Some(u64::try_from(parsed).unwrap_or(u64::MAX));
            idx += 2;
            continue;
        }
        if eq_ascii_command(&argv[idx], b"FORCE") {
            force = true;
            idx += 1;
            continue;
        }
        if eq_ascii_command(&argv[idx], b"JUSTID") {
            justid = true;
            idx += 1;
            continue;
        }
        if eq_ascii_command(&argv[idx], b"LASTID") {
            if idx + 1 >= argv.len() {
                return Err(CommandError::WrongArity("XCLAIM"));
            }
            let parsed = match parse_stream_id(&argv[idx + 1]) {
                Ok(id) => id,
                Err(reply) => return Ok(reply),
            };
            last_id = Some(parsed);
            idx += 2;
            continue;
        }
        return Err(CommandError::SyntaxError);
    }

    if idle_ms.is_some() && time_ms.is_some() {
        return Err(CommandError::SyntaxError);
    }

    let Some(reply) = store.xclaim(
        &argv[1],
        &argv[2],
        &argv[3],
        &ids,
        StreamClaimOptions {
            min_idle_time_ms,
            idle_ms,
            time_ms,
            retry_count,
            force,
            justid,
            last_id,
        },
        now_ms,
    )?
    else {
        return Ok(xclaim_nogroup_error(&argv[1], &argv[2]));
    };

    match reply {
        StreamClaimReply::Entries(entries) => Ok(RespFrame::Array(Some(
            entries
                .into_iter()
                .map(|(id, fields)| stream_record_to_frame(id, fields))
                .collect(),
        ))),
        StreamClaimReply::Ids(ids) => Ok(RespFrame::Array(Some(
            ids.into_iter()
                .map(|id| RespFrame::BulkString(Some(format_stream_id(id))))
                .collect(),
        ))),
    }
}

fn xautoclaim(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() < 6 {
        return Err(CommandError::WrongArity("XAUTOCLAIM"));
    }

    let min_idle_raw = parse_i64_arg(&argv[4])?;
    if min_idle_raw < 0 {
        return Err(CommandError::InvalidInteger);
    }
    let min_idle_time_ms = u64::try_from(min_idle_raw).unwrap_or(u64::MAX);

    let start = match parse_stream_id(&argv[5]) {
        Ok(id) => id,
        Err(reply) => return Ok(reply),
    };

    let mut count = 100usize;
    let mut justid = false;
    let mut idx = 6usize;
    while idx < argv.len() {
        if eq_ascii_command(&argv[idx], b"COUNT") {
            if idx + 1 >= argv.len() {
                return Err(CommandError::WrongArity("XAUTOCLAIM"));
            }
            let parsed = parse_i64_arg(&argv[idx + 1])?;
            if parsed <= 0 {
                return Err(CommandError::InvalidInteger);
            }
            count = usize::try_from(parsed).unwrap_or(usize::MAX);
            idx += 2;
            continue;
        }
        if eq_ascii_command(&argv[idx], b"JUSTID") {
            justid = true;
            idx += 1;
            continue;
        }
        return Err(CommandError::SyntaxError);
    }

    let Some(reply) = store.xautoclaim(
        &argv[1],
        &argv[2],
        &argv[3],
        start,
        StreamAutoClaimOptions {
            min_idle_time_ms,
            count,
            justid,
        },
        now_ms,
    )?
    else {
        return Ok(xautoclaim_nogroup_error(&argv[1], &argv[2]));
    };

    match reply {
        StreamAutoClaimReply::Entries {
            next_start,
            entries,
            deleted_ids,
        } => {
            let mut entry_frames = Vec::with_capacity(entries.len());
            for (id, fields) in entries {
                entry_frames.push(stream_record_to_frame(id, fields));
            }
            let deleted_frames = deleted_ids
                .into_iter()
                .map(|id| RespFrame::BulkString(Some(format_stream_id(id))))
                .collect();
            Ok(RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(format_stream_id(next_start))),
                RespFrame::Array(Some(entry_frames)),
                RespFrame::Array(Some(deleted_frames)),
            ])))
        }
        StreamAutoClaimReply::Ids {
            next_start,
            ids,
            deleted_ids,
        } => {
            let id_frames = ids
                .into_iter()
                .map(|id| RespFrame::BulkString(Some(format_stream_id(id))))
                .collect();
            let deleted_frames = deleted_ids
                .into_iter()
                .map(|id| RespFrame::BulkString(Some(format_stream_id(id))))
                .collect();
            Ok(RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(format_stream_id(next_start))),
                RespFrame::Array(Some(id_frames)),
                RespFrame::Array(Some(deleted_frames)),
            ])))
        }
    }
}

fn xpending(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() == 3 {
        let Some((total, min_id, max_id, per_consumer)) =
            store.xpending_summary(&argv[1], &argv[2], now_ms)?
        else {
            return Ok(xpending_nogroup_error(&argv[1], &argv[2]));
        };

        let mut consumer_frames = Vec::with_capacity(per_consumer.len());
        for (consumer, pending_count) in per_consumer {
            consumer_frames.push(RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(consumer)),
                RespFrame::Integer(i64::try_from(pending_count).unwrap_or(i64::MAX)),
            ])));
        }

        return Ok(RespFrame::Array(Some(vec![
            RespFrame::Integer(i64::try_from(total).unwrap_or(i64::MAX)),
            min_id
                .map(|id| RespFrame::BulkString(Some(format_stream_id(id))))
                .unwrap_or(RespFrame::BulkString(None)),
            max_id
                .map(|id| RespFrame::BulkString(Some(format_stream_id(id))))
                .unwrap_or(RespFrame::BulkString(None)),
            RespFrame::Array(Some(consumer_frames)),
        ])));
    }

    if argv.len() != 6 && argv.len() != 7 {
        return Err(CommandError::WrongArity("XPENDING"));
    }

    let start = match parse_stream_range_bound(&argv[3], true) {
        Ok(id) => id,
        Err(reply) => return Ok(reply),
    };
    let end = match parse_stream_range_bound(&argv[4], false) {
        Ok(id) => id,
        Err(reply) => return Ok(reply),
    };

    let count_raw = parse_i64_arg(&argv[5])?;
    if count_raw < 0 {
        return Err(CommandError::InvalidInteger);
    }
    let count = usize::try_from(count_raw).unwrap_or(usize::MAX);
    let consumer = if argv.len() == 7 {
        Some(argv[6].as_slice())
    } else {
        None
    };

    let Some(entries) =
        store.xpending_entries(&argv[1], &argv[2], (start, end), count, consumer, now_ms)?
    else {
        return Ok(xpending_nogroup_error(&argv[1], &argv[2]));
    };

    let mut out = Vec::with_capacity(entries.len());
    for (id, owner, idle_ms, deliveries) in entries {
        out.push(RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(format_stream_id(id))),
            RespFrame::BulkString(Some(owner)),
            RespFrame::Integer(i64::try_from(idle_ms).unwrap_or(i64::MAX)),
            RespFrame::Integer(i64::try_from(deliveries).unwrap_or(i64::MAX)),
        ])));
    }
    Ok(RespFrame::Array(Some(out)))
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
            Ok(false) => Ok(xgroup_nogroup_error(&argv[2], &argv[3])),
            Err(StoreError::KeyNotFound) => Err(CommandError::NoSuchKey),
            Err(err) => Err(CommandError::Store(err)),
        };
    }

    if eq_ascii_command(&argv[1], b"CREATECONSUMER") {
        if argv.len() != 5 {
            return Err(CommandError::WrongArity("XGROUP"));
        }
        return match store.xgroup_createconsumer(&argv[2], &argv[3], &argv[4], now_ms) {
            Ok(Some(created)) => Ok(RespFrame::Integer(if created { 1 } else { 0 })),
            Ok(None) => Ok(xgroup_nogroup_error(&argv[2], &argv[3])),
            Err(err) => Err(CommandError::Store(err)),
        };
    }

    if eq_ascii_command(&argv[1], b"DELCONSUMER") {
        if argv.len() != 5 {
            return Err(CommandError::WrongArity("XGROUP"));
        }
        return match store.xgroup_delconsumer(&argv[2], &argv[3], &argv[4], now_ms) {
            Ok(Some(deleted_pending)) => Ok(RespFrame::Integer(
                i64::try_from(deleted_pending).unwrap_or(i64::MAX),
            )),
            Ok(None) => Ok(xgroup_nogroup_error(&argv[2], &argv[3])),
            Err(err) => Err(CommandError::Store(err)),
        };
    }

    Err(CommandError::SyntaxError)
}

fn xreadgroup_nogroup_error(key: &[u8], group: &[u8]) -> RespFrame {
    let key = String::from_utf8_lossy(key);
    let group = String::from_utf8_lossy(group);
    RespFrame::Error(format!(
        "NOGROUP No such key '{key}' or consumer group '{group}' in XREADGROUP with GROUP option"
    ))
}

fn xgroup_nogroup_error(key: &[u8], group: &[u8]) -> RespFrame {
    let key = String::from_utf8_lossy(key);
    let group = String::from_utf8_lossy(group);
    RespFrame::Error(format!(
        "NOGROUP No such key '{key}' or consumer group '{group}' in XGROUP command"
    ))
}

fn xclaim_nogroup_error(key: &[u8], group: &[u8]) -> RespFrame {
    let key = String::from_utf8_lossy(key);
    let group = String::from_utf8_lossy(group);
    RespFrame::Error(format!(
        "NOGROUP No such key '{key}' or consumer group '{group}' in XCLAIM command"
    ))
}

fn xautoclaim_nogroup_error(key: &[u8], group: &[u8]) -> RespFrame {
    let key = String::from_utf8_lossy(key);
    let group = String::from_utf8_lossy(group);
    RespFrame::Error(format!(
        "NOGROUP No such key '{key}' or consumer group '{group}' in XAUTOCLAIM command"
    ))
}

fn xpending_nogroup_error(key: &[u8], group: &[u8]) -> RespFrame {
    let key = String::from_utf8_lossy(key);
    let group = String::from_utf8_lossy(group);
    RespFrame::Error(format!(
        "NOGROUP No such key '{key}' or consumer group '{group}' in XPENDING command"
    ))
}

fn xinfo_nogroup_consumers_error(key: &[u8], group: &[u8]) -> RespFrame {
    let key = String::from_utf8_lossy(key);
    let group = String::from_utf8_lossy(group);
    RespFrame::Error(format!(
        "NOGROUP No such consumer group '{group}' for key name '{key}'"
    ))
}

fn xinfo(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() < 2 {
        return Err(CommandError::WrongArity("XINFO"));
    }
    if eq_ascii_command(&argv[1], b"CONSUMERS") {
        if argv.len() != 4 {
            return Err(CommandError::WrongArity("XINFO"));
        }
        let consumers = match store.xinfo_consumers(&argv[2], &argv[3], now_ms) {
            Ok(Some(consumers)) => consumers,
            Ok(None) => return Ok(xinfo_nogroup_consumers_error(&argv[2], &argv[3])),
            Err(StoreError::KeyNotFound) => return Err(CommandError::NoSuchKey),
            Err(err) => return Err(CommandError::Store(err)),
        };
        let out = consumers
            .into_iter()
            .map(stream_consumer_info_to_frame)
            .collect();
        return Ok(RespFrame::Array(Some(out)));
    }
    if eq_ascii_command(&argv[1], b"GROUPS") {
        if argv.len() != 3 {
            return Err(CommandError::WrongArity("XINFO"));
        }
        let Some(groups) = store.xinfo_groups(&argv[2], now_ms)? else {
            return Err(CommandError::NoSuchKey);
        };
        let mut out = Vec::with_capacity(groups.len());
        for (name, consumers, pending, last_delivered_id) in groups {
            out.push(stream_group_info_to_frame(
                name,
                consumers,
                pending,
                last_delivered_id,
            ));
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

// ── XACK ────────────────────────────────────────────────────────────

fn xack_cmd(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() < 4 {
        return Err(CommandError::WrongArity("XACK"));
    }
    let key = &argv[1];
    let group = &argv[2];
    let mut ids = Vec::with_capacity(argv.len() - 3);
    for arg in &argv[3..] {
        let id = match parse_stream_id(arg) {
            Ok(id) => id,
            Err(reply) => return Ok(reply),
        };
        ids.push(id);
    }
    let acked = store.xack(key, group, &ids, now_ms)?;
    Ok(RespFrame::Integer(acked as i64))
}

// ── XSETID ──────────────────────────────────────────────────────────

fn xsetid_cmd(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    // XSETID key last-id [ENTRIESADDED entries-added] [MAXDELETEDID max-deleted-id]
    if argv.len() < 3 {
        return Err(CommandError::WrongArity("XSETID"));
    }
    let key = &argv[1];
    let last_id = match parse_stream_id(&argv[2]) {
        Ok(id) => id,
        Err(reply) => return Ok(reply),
    };
    // parse optional ENTRIESADDED / MAXDELETEDID (accept but ignore)
    let mut i = 3;
    while i < argv.len() {
        let kw = std::str::from_utf8(&argv[i]).unwrap_or("");
        if kw.eq_ignore_ascii_case("ENTRIESADDED") {
            i += 2; // skip arg
        } else if kw.eq_ignore_ascii_case("MAXDELETEDID") {
            i += 2;
        } else {
            return Ok(RespFrame::Error("ERR syntax error".to_string()));
        }
    }
    match store.xsetid(key, last_id, now_ms) {
        Ok(true) => Ok(RespFrame::SimpleString("OK".to_string())),
        Ok(false) => Ok(RespFrame::Error(
            "ERR The ID specified in XSETID is not present in the target stream".to_string(),
        )),
        Err(e) => Err(e.into()),
    }
}

// ── LOLWUT ──────────────────────────────────────────────────────────

fn lolwut_cmd(argv: &[Vec<u8>]) -> Result<RespFrame, CommandError> {
    // LOLWUT [VERSION version]
    let version = if argv.len() >= 3 {
        let ver_kw = std::str::from_utf8(&argv[1]).unwrap_or("");
        if ver_kw.eq_ignore_ascii_case("VERSION") {
            parse_i64_arg(&argv[2]).unwrap_or(1) as u32
        } else {
            1
        }
    } else {
        1
    };
    let art = generate_lolwut_art(version);
    Ok(RespFrame::BulkString(Some(art.into_bytes())))
}

fn generate_lolwut_art(version: u32) -> String {
    // Generate deterministic ASCII art based on version number
    let width = 40;
    let height = 12;
    let mut lines = Vec::with_capacity(height + 2);
    lines.push(String::new());
    let seed = version.wrapping_mul(2654435761);
    for row in 0..height {
        let mut line = String::with_capacity(width);
        for col in 0..width {
            let v = seed
                .wrapping_add((row as u32).wrapping_mul(31))
                .wrapping_add((col as u32).wrapping_mul(17));
            let ch = match (v >> 4) % 8 {
                0 => ' ',
                1 => '.',
                2 => 'o',
                3 => 'O',
                4 => '#',
                5 => '*',
                6 => '+',
                _ => '-',
            };
            line.push(ch);
        }
        lines.push(line);
    }
    lines.push(String::new());
    lines.push(format!("FrankenRedis ver. 0.1.0 -- LOLWUT v{version}"));
    lines.push(String::new());
    lines.join("\n")
}

// ── WAITAOF ─────────────────────────────────────────────────────────

fn waitaof_cmd(argv: &[Vec<u8>]) -> Result<RespFrame, CommandError> {
    // WAITAOF numlocal numreplicas timeout
    if argv.len() < 4 {
        return Err(CommandError::WrongArity("WAITAOF"));
    }
    // In standalone mode: local=0, replicas=0
    Ok(RespFrame::Array(Some(vec![
        RespFrame::Integer(0),
        RespFrame::Integer(0),
    ])))
}

// ── CLUSTER ─────────────────────────────────────────────────────────

fn cluster_cmd(argv: &[Vec<u8>]) -> Result<RespFrame, CommandError> {
    if argv.len() < 2 {
        return Err(CommandError::WrongArity("CLUSTER"));
    }
    let sub = std::str::from_utf8(&argv[1]).map_err(|_| CommandError::InvalidUtf8Argument)?;
    if sub.eq_ignore_ascii_case("INFO") {
        Ok(RespFrame::BulkString(Some(
            b"cluster_enabled:0\r\n\
              cluster_state:ok\r\n\
              cluster_slots_assigned:0\r\n\
              cluster_slots_ok:0\r\n\
              cluster_slots_pfail:0\r\n\
              cluster_slots_fail:0\r\n\
              cluster_known_nodes:0\r\n\
              cluster_size:0\r\n\
              cluster_current_epoch:0\r\n\
              cluster_my_epoch:0\r\n\
              cluster_stats_messages_sent:0\r\n\
              cluster_stats_messages_received:0\r\n\
              total_cluster_links_buffer_limit_exceeded:0\r\n"
                .to_vec(),
        )))
    } else if sub.eq_ignore_ascii_case("MYID") {
        Ok(RespFrame::BulkString(Some(
            b"0000000000000000000000000000000000000000".to_vec(),
        )))
    } else if sub.eq_ignore_ascii_case("SLOTS")
        || sub.eq_ignore_ascii_case("SHARDS")
        || sub.eq_ignore_ascii_case("GETKEYSINSLOT")
    {
        Ok(RespFrame::Array(Some(Vec::new())))
    } else if sub.eq_ignore_ascii_case("NODES") {
        Ok(RespFrame::BulkString(Some(Vec::new())))
    } else if sub.eq_ignore_ascii_case("RESET") {
        Ok(RespFrame::SimpleString("OK".to_string()))
    } else if sub.eq_ignore_ascii_case("KEYSLOT") {
        if argv.len() < 3 {
            return Err(CommandError::WrongArity("CLUSTER"));
        }
        // CRC16 slot computation
        let slot = crc16_slot(&argv[2]);
        Ok(RespFrame::Integer(slot as i64))
    } else if sub.eq_ignore_ascii_case("COUNTKEYSINSLOT") {
        Ok(RespFrame::Integer(0))
    } else if sub.eq_ignore_ascii_case("HELP") {
        Ok(RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"CLUSTER INFO".to_vec())),
            RespFrame::BulkString(Some(b"CLUSTER MYID".to_vec())),
            RespFrame::BulkString(Some(b"CLUSTER KEYSLOT <key>".to_vec())),
            RespFrame::BulkString(Some(b"CLUSTER SLOTS".to_vec())),
            RespFrame::BulkString(Some(b"CLUSTER SHARDS".to_vec())),
            RespFrame::BulkString(Some(b"CLUSTER NODES".to_vec())),
            RespFrame::BulkString(Some(b"CLUSTER RESET".to_vec())),
        ])))
    } else {
        Ok(RespFrame::Error(format!(
            "ERR Unknown subcommand or wrong number of arguments for CLUSTER {sub}"
        )))
    }
}

// ── REPLICAOF / SLAVEOF ─────────────────────────────────────────────

fn replicaof_cmd(argv: &[Vec<u8>]) -> Result<RespFrame, CommandError> {
    // REPLICAOF host port / REPLICAOF NO ONE
    if argv.len() < 3 {
        return Err(CommandError::WrongArity("REPLICAOF"));
    }
    let host = std::str::from_utf8(&argv[1]).unwrap_or("");
    let port = std::str::from_utf8(&argv[2]).unwrap_or("");
    if host.eq_ignore_ascii_case("NO") && port.eq_ignore_ascii_case("ONE") {
        Ok(RespFrame::SimpleString("OK Already a master".to_string()))
    } else {
        // Accept but don't actually replicate - standalone mode
        Ok(RespFrame::SimpleString("OK".to_string()))
    }
}

// ── READONLY / READWRITE ────────────────────────────────────────────

fn readonly_cmd(argv: &[Vec<u8>]) -> Result<RespFrame, CommandError> {
    if argv.len() != 1 {
        return Err(CommandError::WrongArity("READONLY"));
    }
    Ok(RespFrame::SimpleString("OK".to_string()))
}

fn readwrite_cmd(argv: &[Vec<u8>]) -> Result<RespFrame, CommandError> {
    if argv.len() != 1 {
        return Err(CommandError::WrongArity("READWRITE"));
    }
    Ok(RespFrame::SimpleString("OK".to_string()))
}

// ── ZRANGESTORE ────────────────────────────────────────────────────

fn zrangestore_cmd(
    argv: &[Vec<u8>],
    store: &mut Store,
    now_ms: u64,
) -> Result<RespFrame, CommandError> {
    // ZRANGESTORE dst src min max [BYSCORE | BYLEX] [REV] [LIMIT offset count]
    if argv.len() < 5 {
        return Err(CommandError::WrongArity("ZRANGESTORE"));
    }
    let dst = &argv[1];
    let src = &argv[2];

    let mut byscore = false;
    let mut bylex = false;
    let mut rev = false;
    let mut limit_offset: Option<usize> = None;
    let mut limit_count: Option<usize> = None;

    let mut i = 5;
    while i < argv.len() {
        let opt = std::str::from_utf8(&argv[i]).map_err(|_| CommandError::InvalidUtf8Argument)?;
        if opt.eq_ignore_ascii_case("BYSCORE") {
            byscore = true;
            i += 1;
        } else if opt.eq_ignore_ascii_case("BYLEX") {
            bylex = true;
            i += 1;
        } else if opt.eq_ignore_ascii_case("REV") {
            rev = true;
            i += 1;
        } else if opt.eq_ignore_ascii_case("LIMIT") {
            if i + 2 >= argv.len() {
                return Err(CommandError::SyntaxError);
            }
            limit_offset = Some(parse_u64_arg(&argv[i + 1])? as usize);
            limit_count = Some(parse_u64_arg(&argv[i + 2])? as usize);
            i += 3;
        } else {
            return Err(CommandError::SyntaxError);
        }
    }

    if byscore && bylex {
        return Err(CommandError::SyntaxError);
    }

    let pairs: Vec<(Vec<u8>, f64)> = if byscore {
        let min = parse_score_bound(&argv[3])?;
        let max = parse_score_bound(&argv[4])?;
        let (lo, hi) = if rev { (max, min) } else { (min, max) };
        let mut result = store.zrangebyscore_withscores(src, lo, hi, now_ms)?;
        if rev {
            result.reverse();
        }
        result
    } else if bylex {
        let min_lex = &argv[3];
        let max_lex = &argv[4];
        let (lo, hi) = if rev {
            (max_lex.as_slice(), min_lex.as_slice())
        } else {
            (min_lex.as_slice(), max_lex.as_slice())
        };
        let mut members = store.zrangebylex(src, lo, hi, now_ms)?;
        if rev {
            members.reverse();
        }
        // Look up scores for each member
        let mut result = Vec::with_capacity(members.len());
        for m in members {
            let score = store.zscore(src, &m, now_ms)?.unwrap_or(0.0);
            result.push((m, score));
        }
        result
    } else {
        // Default: by rank (index)
        let start = parse_i64_arg(&argv[3])?;
        let stop = parse_i64_arg(&argv[4])?;
        if rev {
            let mut result = store.zrange_withscores(src, start, stop, now_ms)?;
            result.reverse();
            result
        } else {
            store.zrange_withscores(src, start, stop, now_ms)?
        }
    };

    // Apply LIMIT
    let pairs = if let (Some(offset), Some(count)) = (limit_offset, limit_count) {
        pairs.into_iter().skip(offset).take(count).collect()
    } else {
        pairs
    };

    let count = pairs.len() as i64;
    if pairs.is_empty() {
        // Delete dst if it exists
        store.del(std::slice::from_ref(dst), now_ms);
    } else {
        store.zstore_from_pairs(dst.clone(), pairs, now_ms);
    }
    Ok(RespFrame::Integer(count))
}

// ── FUNCTION ────────────────────────────────────────────────────────

fn function_cmd(
    argv: &[Vec<u8>],
    store: &mut Store,
    _now_ms: u64,
) -> Result<RespFrame, CommandError> {
    if argv.len() < 2 {
        return Err(CommandError::WrongArity("FUNCTION"));
    }
    let sub = std::str::from_utf8(&argv[1]).map_err(|_| CommandError::InvalidUtf8Argument)?;
    if sub.eq_ignore_ascii_case("LOAD") {
        // FUNCTION LOAD [REPLACE] function-code
        if argv.len() < 3 {
            return Err(CommandError::WrongArity("FUNCTION"));
        }
        let mut replace = false;
        let code_idx;
        if argv.len() >= 4 {
            let flag = std::str::from_utf8(&argv[2]).unwrap_or("");
            if flag.eq_ignore_ascii_case("REPLACE") {
                replace = true;
                code_idx = 3;
            } else {
                code_idx = 2;
            }
        } else {
            code_idx = 2;
        }
        if code_idx >= argv.len() {
            return Err(CommandError::WrongArity("FUNCTION"));
        }
        match store.function_load(&argv[code_idx], replace) {
            Ok(name) => Ok(RespFrame::BulkString(Some(name.into_bytes()))),
            Err(e) => Err(CommandError::Store(e)),
        }
    } else if sub.eq_ignore_ascii_case("LIST") {
        // FUNCTION LIST [LIBRARYNAME pattern] [WITHCODE]
        let mut pattern = None;
        let mut with_code = false;
        let mut i = 2;
        while i < argv.len() {
            let arg = std::str::from_utf8(&argv[i]).unwrap_or("");
            if arg.eq_ignore_ascii_case("LIBRARYNAME") {
                i += 1;
                if i < argv.len() {
                    pattern = Some(std::str::from_utf8(&argv[i]).unwrap_or("*").to_string());
                }
            } else if arg.eq_ignore_ascii_case("WITHCODE") {
                with_code = true;
            }
            i += 1;
        }
        let libs = store.function_list(pattern.as_deref());
        let mut result = Vec::new();
        for lib in libs {
            let mut entries = vec![
                RespFrame::BulkString(Some(b"library_name".to_vec())),
                RespFrame::BulkString(Some(lib.name.as_bytes().to_vec())),
                RespFrame::BulkString(Some(b"engine".to_vec())),
                RespFrame::BulkString(Some(lib.engine.as_bytes().to_vec())),
                RespFrame::BulkString(Some(b"functions".to_vec())),
            ];
            let funcs: Vec<RespFrame> = lib
                .functions
                .iter()
                .map(|f| {
                    let func_entries = vec![
                        RespFrame::BulkString(Some(b"name".to_vec())),
                        RespFrame::BulkString(Some(f.name.as_bytes().to_vec())),
                        RespFrame::BulkString(Some(b"description".to_vec())),
                        match &f.description {
                            Some(d) => RespFrame::BulkString(Some(d.as_bytes().to_vec())),
                            None => RespFrame::BulkString(None),
                        },
                        RespFrame::BulkString(Some(b"flags".to_vec())),
                        RespFrame::Array(Some(
                            f.flags
                                .iter()
                                .map(|fl| RespFrame::BulkString(Some(fl.as_bytes().to_vec())))
                                .collect(),
                        )),
                    ];
                    RespFrame::Array(Some(func_entries))
                })
                .collect();
            entries.push(RespFrame::Array(Some(funcs)));
            if with_code {
                entries.push(RespFrame::BulkString(Some(b"library_code".to_vec())));
                entries.push(RespFrame::BulkString(Some(lib.code.clone())));
            }
            result.push(RespFrame::Array(Some(entries)));
        }
        Ok(RespFrame::Array(Some(result)))
    } else if sub.eq_ignore_ascii_case("STATS") {
        let (lib_count, func_count) = store.function_stats();
        Ok(RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"running_script".to_vec())),
            RespFrame::Integer(0),
            RespFrame::BulkString(Some(b"engines".to_vec())),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"LUA".to_vec())),
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"libraries_count".to_vec())),
                    RespFrame::Integer(lib_count as i64),
                    RespFrame::BulkString(Some(b"functions_count".to_vec())),
                    RespFrame::Integer(func_count as i64),
                ])),
            ])),
        ])))
    } else if sub.eq_ignore_ascii_case("DUMP") {
        let data = store.function_dump();
        Ok(RespFrame::BulkString(Some(data)))
    } else if sub.eq_ignore_ascii_case("RESTORE") {
        // FUNCTION RESTORE serialized-value [FLUSH|APPEND|REPLACE]
        if argv.len() < 3 {
            return Err(CommandError::WrongArity("FUNCTION"));
        }
        let policy = if argv.len() >= 4 {
            std::str::from_utf8(&argv[3]).unwrap_or("")
        } else {
            ""
        };
        match store.function_restore(&argv[2], policy) {
            Ok(()) => Ok(RespFrame::SimpleString("OK".to_string())),
            Err(e) => Err(CommandError::Store(e)),
        }
    } else if sub.eq_ignore_ascii_case("FLUSH") {
        store.function_flush();
        Ok(RespFrame::SimpleString("OK".to_string()))
    } else if sub.eq_ignore_ascii_case("DELETE") {
        if argv.len() < 3 {
            return Err(CommandError::WrongArity("FUNCTION"));
        }
        let name = std::str::from_utf8(&argv[2]).map_err(|_| CommandError::InvalidUtf8Argument)?;
        match store.function_delete(name) {
            Ok(()) => Ok(RespFrame::SimpleString("OK".to_string())),
            Err(e) => Err(CommandError::Store(e)),
        }
    } else if sub.eq_ignore_ascii_case("HELP") {
        Ok(RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(
                b"FUNCTION LOAD [REPLACE] function-code - Load a library.".to_vec(),
            )),
            RespFrame::BulkString(Some(
                b"FUNCTION LIST [LIBRARYNAME pattern] [WITHCODE] - List libraries.".to_vec(),
            )),
            RespFrame::BulkString(Some(b"FUNCTION STATS - Return engine stats.".to_vec())),
            RespFrame::BulkString(Some(b"FUNCTION DUMP - Serialize all libraries.".to_vec())),
            RespFrame::BulkString(Some(
                b"FUNCTION RESTORE data [FLUSH|APPEND|REPLACE] - Restore libraries.".to_vec(),
            )),
            RespFrame::BulkString(Some(
                b"FUNCTION FLUSH [ASYNC|SYNC] - Delete all libraries.".to_vec(),
            )),
            RespFrame::BulkString(Some(
                b"FUNCTION DELETE library-name - Delete a library.".to_vec(),
            )),
            RespFrame::BulkString(Some(b"FUNCTION HELP - Return this help.".to_vec())),
        ])))
    } else {
        Ok(RespFrame::Error(format!(
            "ERR Unknown subcommand or wrong number of arguments for FUNCTION {sub}"
        )))
    }
}

fn fcall_cmd(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    // FCALL function numkeys [key ...] [arg ...]
    if argv.len() < 3 {
        return Err(CommandError::WrongArity("FCALL"));
    }
    let func_name = std::str::from_utf8(&argv[1]).map_err(|_| CommandError::InvalidUtf8Argument)?;
    let (_numkeys, keys, args) = parse_eval_args(argv)?;

    // Validate function name contains only safe identifier characters to prevent
    // Lua code injection when constructing the wrapper script.
    if func_name.is_empty()
        || !func_name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Ok(RespFrame::Error(
            "ERR Function not found".to_string(),
        ));
    }

    // Look up the function by name
    let script = match store.function_get(func_name) {
        Some((lib, _func)) => lib.code.clone(),
        None => {
            return Ok(RespFrame::Error("ERR Function not found".to_string()));
        }
    };

    // Execute the library code, which should define the function, then call it
    // For Redis 7.0+, the library registers functions via redis.register_function
    // The FCALL mechanism calls the registered function directly
    // Since we have the Lua evaluator, we execute the full library code
    // and then call the target function
    let wrapper_script = format!(
        "{}\nreturn {}(KEYS, ARGV)",
        String::from_utf8_lossy(&script),
        func_name
    );

    let keys_vec: Vec<Vec<u8>> = keys.to_vec();
    let args_vec: Vec<Vec<u8>> = args.to_vec();
    match lua_eval::eval_script(
        wrapper_script.as_bytes(),
        &keys_vec,
        &args_vec,
        store,
        now_ms,
    ) {
        Ok(frame) => Ok(frame),
        Err(e) => Ok(RespFrame::Error(format!("ERR Error running function: {e}"))),
    }
}

// ── SSUBSCRIBE / SUNSUBSCRIBE / SPUBLISH (shard Pub/Sub) ───────────

fn ssubscribe_cmd(argv: &[Vec<u8>]) -> Result<RespFrame, CommandError> {
    // SSUBSCRIBE shardchannel [shardchannel ...]
    if argv.len() < 2 {
        return Err(CommandError::WrongArity("SSUBSCRIBE"));
    }
    let last = &argv[argv.len() - 1];
    Ok(RespFrame::Array(Some(vec![
        RespFrame::BulkString(Some(b"ssubscribe".to_vec())),
        RespFrame::BulkString(Some(last.clone())),
        RespFrame::Integer((argv.len() - 1) as i64),
    ])))
}

fn sunsubscribe_cmd(argv: &[Vec<u8>]) -> Result<RespFrame, CommandError> {
    // SUNSUBSCRIBE [shardchannel ...]
    let count = if argv.len() > 1 { argv.len() - 1 } else { 0 };
    Ok(RespFrame::Array(Some(vec![
        RespFrame::BulkString(Some(b"sunsubscribe".to_vec())),
        RespFrame::BulkString(None),
        RespFrame::Integer(count as i64),
    ])))
}

fn spublish_cmd(argv: &[Vec<u8>]) -> Result<RespFrame, CommandError> {
    // SPUBLISH shardchannel message
    if argv.len() != 3 {
        return Err(CommandError::WrongArity("SPUBLISH"));
    }
    // No subscribers in standalone mode
    Ok(RespFrame::Integer(0))
}

/// CRC16/CCITT for Redis cluster hash slot computation.
fn crc16_slot(key: &[u8]) -> u16 {
    // If key contains {hashtag}, use only the content between first { and next }
    let data = if let Some(start) = key.iter().position(|&b| b == b'{') {
        if let Some(end) = key[start + 1..].iter().position(|&b| b == b'}') {
            if end > 0 {
                &key[start + 1..start + 1 + end]
            } else {
                key
            }
        } else {
            key
        }
    } else {
        key
    };
    let mut crc: u16 = 0;
    for &byte in data {
        crc = ((crc << 8) & 0xFF00) ^ CRC16_TAB[((crc >> 8) as u8 ^ byte) as usize];
    }
    crc % 16384
}

/// CRC16 lookup table from Redis source (CRC-16/XMODEM, poly 0x1021).
#[rustfmt::skip]
const CRC16_TAB: [u16; 256] = [
    0x0000, 0x1021, 0x2042, 0x3063, 0x4084, 0x50a5, 0x60c6, 0x70e7,
    0x8108, 0x9129, 0xa14a, 0xb16b, 0xc18c, 0xd1ad, 0xe1ce, 0xf1ef,
    0x1231, 0x0210, 0x3273, 0x2252, 0x52b5, 0x4294, 0x72f7, 0x62d6,
    0x9339, 0x8318, 0xb37b, 0xa35a, 0xd3bd, 0xc39c, 0xf3ff, 0xe3de,
    0x2462, 0x3443, 0x0420, 0x1401, 0x64e6, 0x74c7, 0x44a4, 0x5485,
    0xa56a, 0xb54b, 0x8528, 0x9509, 0xe5ee, 0xf5cf, 0xc5ac, 0xd58d,
    0x3653, 0x2672, 0x1611, 0x0630, 0x76d7, 0x66f6, 0x5695, 0x46b4,
    0xb75b, 0xa77a, 0x9719, 0x8738, 0xf7df, 0xe7fe, 0xd79d, 0xc7bc,
    0x48c4, 0x58e5, 0x6886, 0x78a7, 0x0840, 0x1861, 0x2802, 0x3823,
    0xc9cc, 0xd9ed, 0xe98e, 0xf9af, 0x8948, 0x9969, 0xa90a, 0xb92b,
    0x5af5, 0x4ad4, 0x7ab7, 0x6a96, 0x1a71, 0x0a50, 0x3a33, 0x2a12,
    0xdbfd, 0xcbdc, 0xfbbf, 0xeb9e, 0x9b79, 0x8b58, 0xbb3b, 0xab1a,
    0x6ca6, 0x7c87, 0x4ce4, 0x5cc5, 0x2c22, 0x3c03, 0x0c60, 0x1c41,
    0xedae, 0xfd8f, 0xcdec, 0xddcd, 0xad2a, 0xbd0b, 0x8d68, 0x9d49,
    0x7e97, 0x6eb6, 0x5ed5, 0x4ef4, 0x3e13, 0x2e32, 0x1e51, 0x0e70,
    0xff9f, 0xefbe, 0xdfdd, 0xcffc, 0xbf1b, 0xaf3a, 0x9f59, 0x8f78,
    0x9188, 0x81a9, 0xb1ca, 0xa1eb, 0xd10c, 0xc12d, 0xf14e, 0xe16f,
    0x1080, 0x00a1, 0x30c2, 0x20e3, 0x5004, 0x4025, 0x7046, 0x6067,
    0x83b9, 0x9398, 0xa3fb, 0xb3da, 0xc33d, 0xd31c, 0xe37f, 0xf35e,
    0x02b1, 0x1290, 0x22f3, 0x32d2, 0x4235, 0x5214, 0x6277, 0x7256,
    0xb5ea, 0xa5cb, 0x95a8, 0x8589, 0xf56e, 0xe54f, 0xd52c, 0xc50d,
    0x34e2, 0x24c3, 0x14a0, 0x0481, 0x7466, 0x6447, 0x5424, 0x4405,
    0xa7db, 0xb7fa, 0x8799, 0x97b8, 0xe75f, 0xf77e, 0xc71d, 0xd73c,
    0x26d3, 0x36f2, 0x0691, 0x16b0, 0x6657, 0x7676, 0x4615, 0x5634,
    0xd94c, 0xc96d, 0xf90e, 0xe92f, 0x99c8, 0x89e9, 0xb98a, 0xa9ab,
    0x5844, 0x4865, 0x7806, 0x6827, 0x18c0, 0x08e1, 0x3882, 0x28a3,
    0xcb7d, 0xdb5c, 0xeb3f, 0xfb1e, 0x8bf9, 0x9bd8, 0xabbb, 0xbb9a,
    0x4a75, 0x5a54, 0x6a37, 0x7a16, 0x0af1, 0x1ad0, 0x2ab3, 0x3a92,
    0xfd2e, 0xed0f, 0xdd6c, 0xcd4d, 0xbdaa, 0xad8b, 0x9de8, 0x8dc9,
    0x7c26, 0x6c07, 0x5c64, 0x4c45, 0x3ca2, 0x2c83, 0x1ce0, 0x0cc1,
    0xef1f, 0xff3e, 0xcf5d, 0xdf7c, 0xaf9b, 0xbfba, 0x8fd9, 0x9ff8,
    0x6e17, 0x7e36, 0x4e55, 0x5e74, 0x2e93, 0x3eb2, 0x0ed1, 0x1ef0,
];

fn parse_score_bound(arg: &[u8]) -> Result<ScoreBound, CommandError> {
    let text = std::str::from_utf8(arg).map_err(|_| CommandError::InvalidUtf8Argument)?;
    if text == "-inf" {
        Ok(ScoreBound::Inclusive(f64::NEG_INFINITY))
    } else if text == "+inf" || text == "inf" {
        Ok(ScoreBound::Inclusive(f64::INFINITY))
    } else if let Some(rest) = text.strip_prefix('(') {
        let val = rest
            .parse::<f64>()
            .map_err(|_| CommandError::InvalidInteger)?;
        Ok(ScoreBound::Exclusive(val))
    } else {
        let val = text
            .parse::<f64>()
            .map_err(|_| CommandError::InvalidInteger)?;
        Ok(ScoreBound::Inclusive(val))
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
    let max_len = offset as usize + argv[3].len();
    if max_len > 536_870_912 {
        return Ok(RespFrame::Error(
            "ERR string exceeds maximum allowed size (512MB)".to_string(),
        ));
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
    if argv.len() < 2 || argv.len() > 3 {
        return Err(CommandError::WrongArity("SPOP"));
    }
    if argv.len() == 3 {
        let count = parse_u64_arg(&argv[2])? as usize;
        let members = store.spop_count(&argv[1], count, now_ms)?;
        let arr = members
            .into_iter()
            .map(|m| RespFrame::BulkString(Some(m)))
            .collect();
        return Ok(RespFrame::Array(Some(arr)));
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
    if argv.len() < 2 || argv.len() > 3 {
        return Err(CommandError::WrongArity("SRANDMEMBER"));
    }
    if argv.len() == 2 {
        // No count: return single element or nil
        match store.srandmember(&argv[1], now_ms)? {
            Some(m) => Ok(RespFrame::BulkString(Some(m))),
            None => Ok(RespFrame::BulkString(None)),
        }
    } else {
        // With count: return array
        let count = parse_i64_arg(&argv[2])?;
        let members = store.srandmember_count(&argv[1], count, now_ms)?;
        let arr: Vec<RespFrame> = members
            .into_iter()
            .map(|m| RespFrame::BulkString(Some(m)))
            .collect();
        Ok(RespFrame::Array(Some(arr)))
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
    // ZRANDMEMBER key [count [WITHSCORES]]
    if argv.len() < 2 || argv.len() > 4 {
        return Err(CommandError::WrongArity("ZRANDMEMBER"));
    }
    if argv.len() == 2 {
        // No count: return single element or nil
        return match store.zrandmember(&argv[1], now_ms)? {
            Some(m) => Ok(RespFrame::BulkString(Some(m))),
            None => Ok(RespFrame::BulkString(None)),
        };
    }
    let count = parse_i64_arg(&argv[2])?;
    let withscores = argv.len() == 4
        && std::str::from_utf8(&argv[3]).is_ok_and(|s| s.eq_ignore_ascii_case("WITHSCORES"));
    if argv.len() == 4 && !withscores {
        return Err(CommandError::SyntaxError);
    }
    let pairs = store.zrandmember_count(&argv[1], count, now_ms)?;
    if withscores {
        let mut frames = Vec::with_capacity(pairs.len() * 2);
        for (member, score) in pairs {
            frames.push(RespFrame::BulkString(Some(member)));
            frames.push(RespFrame::BulkString(Some(score.to_string().into_bytes())));
        }
        Ok(RespFrame::Array(Some(frames)))
    } else {
        let frames = pairs
            .into_iter()
            .map(|(m, _)| RespFrame::BulkString(Some(m)))
            .collect();
        Ok(RespFrame::Array(Some(frames)))
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
    if !(0..4_294_967_296).contains(&offset) {
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
    // LPOS key element [RANK rank] [COUNT count] [MAXLEN maxlen]
    if argv.len() < 3 {
        return Err(CommandError::WrongArity("LPOS"));
    }
    let mut rank: i64 = 1;
    let mut count: Option<u64> = None;
    let mut maxlen: usize = 0;
    let mut i = 3;
    while i < argv.len() {
        let opt = std::str::from_utf8(&argv[i]).map_err(|_| CommandError::InvalidUtf8Argument)?;
        if opt.eq_ignore_ascii_case("RANK") {
            i += 1;
            if i >= argv.len() {
                return Err(CommandError::SyntaxError);
            }
            rank = parse_i64_arg(&argv[i])?;
            if rank == 0 {
                return Ok(RespFrame::Error(
                    "ERR RANK can't be zero: use 1 to start from the first match, 2 from the second ... or use negative to start from the end of the list"
                        .to_string(),
                ));
            }
        } else if opt.eq_ignore_ascii_case("COUNT") {
            i += 1;
            if i >= argv.len() {
                return Err(CommandError::SyntaxError);
            }
            let c = parse_i64_arg(&argv[i])?;
            if c < 0 {
                return Ok(RespFrame::Error("ERR COUNT can't be negative".to_string()));
            }
            count = Some(c as u64);
        } else if opt.eq_ignore_ascii_case("MAXLEN") {
            i += 1;
            if i >= argv.len() {
                return Err(CommandError::SyntaxError);
            }
            let m = parse_i64_arg(&argv[i])?;
            if m < 0 {
                return Ok(RespFrame::Error("ERR MAXLEN can't be negative".to_string()));
            }
            maxlen = m as usize;
        } else {
            return Err(CommandError::SyntaxError);
        }
        i += 1;
    }

    let positions = store.lpos_full(&argv[1], &argv[2], rank, count, maxlen, now_ms)?;

    if count.is_some() {
        // COUNT mode: always return array
        let arr: Vec<RespFrame> = positions
            .into_iter()
            .map(|p| RespFrame::Integer(i64::try_from(p).unwrap_or(i64::MAX)))
            .collect();
        Ok(RespFrame::Array(Some(arr)))
    } else {
        // No COUNT: return single integer or nil
        match positions.first() {
            Some(&pos) => Ok(RespFrame::Integer(i64::try_from(pos).unwrap_or(i64::MAX))),
            None => Ok(RespFrame::BulkString(None)),
        }
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
    // HRANDFIELD key [count [WITHVALUES]]
    if argv.len() < 2 || argv.len() > 4 {
        return Err(CommandError::WrongArity("HRANDFIELD"));
    }
    if argv.len() == 2 {
        // No count: return single field or nil
        return match store.hrandfield(&argv[1], now_ms)? {
            Some(field) => Ok(RespFrame::BulkString(Some(field))),
            None => Ok(RespFrame::BulkString(None)),
        };
    }
    let count = parse_i64_arg(&argv[2])?;
    let withvalues = argv.len() == 4
        && std::str::from_utf8(&argv[3]).is_ok_and(|s| s.eq_ignore_ascii_case("WITHVALUES"));
    if argv.len() == 4 && !withvalues {
        return Err(CommandError::SyntaxError);
    }
    let pairs = store.hrandfield_count(&argv[1], count, now_ms)?;
    if withvalues {
        let mut frames = Vec::with_capacity(pairs.len() * 2);
        for (field, value) in pairs {
            frames.push(RespFrame::BulkString(Some(field)));
            frames.push(RespFrame::BulkString(Some(value)));
        }
        Ok(RespFrame::Array(Some(frames)))
    } else {
        let frames = pairs
            .into_iter()
            .map(|(f, _)| RespFrame::BulkString(Some(f)))
            .collect();
        Ok(RespFrame::Array(Some(frames)))
    }
}

fn zrevrangebyscore(
    argv: &[Vec<u8>],
    store: &mut Store,
    now_ms: u64,
) -> Result<RespFrame, CommandError> {
    // ZREVRANGEBYSCORE key max min [WITHSCORES] [LIMIT offset count]
    if argv.len() < 4 {
        return Err(CommandError::WrongArity("ZREVRANGEBYSCORE"));
    }
    let max = parse_score_bound(&argv[2])?;
    let min = parse_score_bound(&argv[3])?;
    let (withscores, limit_offset, limit_count) = parse_zrangebyscore_opts(argv, 4)?;
    let mut pairs = store.zrangebyscore_withscores(&argv[1], min, max, now_ms)?;
    pairs.reverse();
    if let (Some(offset), Some(count)) = (limit_offset, limit_count) {
        pairs = pairs.into_iter().skip(offset).take(count).collect();
    }
    zrange_emit(pairs, withscores)
}

fn zrangebylex(
    argv: &[Vec<u8>],
    store: &mut Store,
    now_ms: u64,
) -> Result<RespFrame, CommandError> {
    // ZRANGEBYLEX key min max [LIMIT offset count]
    if argv.len() < 4 {
        return Err(CommandError::WrongArity("ZRANGEBYLEX"));
    }
    let (_, limit_offset, limit_count) = parse_zrangebyscore_opts(argv, 4)?;
    let mut members = store.zrangebylex(&argv[1], &argv[2], &argv[3], now_ms)?;
    if let (Some(offset), Some(count)) = (limit_offset, limit_count) {
        members = members.into_iter().skip(offset).take(count).collect();
    }
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
    // ZREVRANGEBYLEX key max min [LIMIT offset count]
    if argv.len() < 4 {
        return Err(CommandError::WrongArity("ZREVRANGEBYLEX"));
    }
    let (_, limit_offset, limit_count) = parse_zrangebyscore_opts(argv, 4)?;
    let mut members = store.zrevrangebylex(&argv[1], &argv[2], &argv[3], now_ms)?;
    if let (Some(offset), Some(count)) = (limit_offset, limit_count) {
        members = members.into_iter().skip(offset).take(count).collect();
    }
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
            if argv.len() != 3 {
                return Err(CommandError::SyntaxError);
            }
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

fn sintercard(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    // SINTERCARD numkeys key [key ...] [LIMIT limit]
    if argv.len() < 3 {
        return Err(CommandError::WrongArity("SINTERCARD"));
    }
    let numkeys_val = parse_i64_arg(&argv[1])?;
    if numkeys_val <= 0 {
        return Err(CommandError::SyntaxError);
    }
    let numkeys = numkeys_val as usize;
    let keys_end = 2 + numkeys;
    if keys_end > argv.len() {
        return Err(CommandError::SyntaxError);
    }
    let keys: Vec<&[u8]> = argv[2..keys_end].iter().map(Vec::as_slice).collect();
    let mut limit: u64 = 0;
    let mut idx = keys_end;
    while idx < argv.len() {
        if argv[idx].eq_ignore_ascii_case(b"LIMIT") {
            idx += 1;
            if idx >= argv.len() {
                return Err(CommandError::SyntaxError);
            }
            let val = parse_i64_arg(&argv[idx])?;
            if val < 0 {
                return Err(CommandError::SyntaxError);
            }
            limit = val as u64;
            idx += 1;
        } else {
            return Err(CommandError::SyntaxError);
        }
    }
    let count = store.sintercard(&keys, limit, now_ms)?;
    #[allow(clippy::cast_possible_wrap)]
    Ok(RespFrame::Integer(count as i64))
}

fn lcs(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    // LCS key1 key2 [LEN] [IDX] [MINMATCHLEN min] [WITHMATCHLEN]
    if argv.len() < 3 {
        return Err(CommandError::WrongArity("LCS"));
    }
    let a = store.get(&argv[1], now_ms)?.unwrap_or_default();
    let b = store.get(&argv[2], now_ms)?.unwrap_or_default();

    let mut len_only = false;
    let mut idx_mode = false;
    let mut min_match_len: usize = 0;
    let mut with_match_len = false;
    let mut i = 3;
    while i < argv.len() {
        if argv[i].eq_ignore_ascii_case(b"LEN") {
            len_only = true;
            i += 1;
        } else if argv[i].eq_ignore_ascii_case(b"IDX") {
            idx_mode = true;
            i += 1;
        } else if argv[i].eq_ignore_ascii_case(b"MINMATCHLEN") {
            i += 1;
            if i >= argv.len() {
                return Err(CommandError::SyntaxError);
            }
            let val = parse_i64_arg(&argv[i])?;
            if val < 0 {
                return Err(CommandError::SyntaxError);
            }
            min_match_len = val as usize;
            i += 1;
        } else if argv[i].eq_ignore_ascii_case(b"WITHMATCHLEN") {
            with_match_len = true;
            i += 1;
        } else {
            return Err(CommandError::SyntaxError);
        }
    }

    let lcs_result = compute_lcs(&a, &b);

    if len_only {
        return Ok(RespFrame::Integer(lcs_result.len() as i64));
    }

    if idx_mode {
        let matches = compute_lcs_matches(&a, &b, min_match_len);
        let mut match_frames = Vec::new();
        for m in &matches {
            let a_range = RespFrame::Array(Some(vec![
                RespFrame::Integer(m.a_start as i64),
                RespFrame::Integer(m.a_end as i64),
            ]));
            let b_range = RespFrame::Array(Some(vec![
                RespFrame::Integer(m.b_start as i64),
                RespFrame::Integer(m.b_end as i64),
            ]));
            if with_match_len {
                match_frames.push(RespFrame::Array(Some(vec![
                    a_range,
                    b_range,
                    RespFrame::Integer(m.len as i64),
                ])));
            } else {
                match_frames.push(RespFrame::Array(Some(vec![a_range, b_range])));
            }
        }
        return Ok(RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"matches".to_vec())),
            RespFrame::Array(Some(match_frames)),
            RespFrame::BulkString(Some(b"len".to_vec())),
            RespFrame::Integer(lcs_result.len() as i64),
        ])));
    }

    Ok(RespFrame::BulkString(Some(lcs_result)))
}

struct LcsMatch {
    a_start: usize,
    a_end: usize,
    b_start: usize,
    b_end: usize,
    len: usize,
}

fn compute_lcs(a: &[u8], b: &[u8]) -> Vec<u8> {
    let m = a.len();
    let n = b.len();
    if m == 0 || n == 0 {
        return Vec::new();
    }
    // DP table
    let mut dp = vec![vec![0u32; n + 1]; m + 1];
    for i in 1..=m {
        for j in 1..=n {
            if a[i - 1] == b[j - 1] {
                dp[i][j] = dp[i - 1][j - 1] + 1;
            } else {
                dp[i][j] = dp[i - 1][j].max(dp[i][j - 1]);
            }
        }
    }
    // Backtrack
    let mut result = Vec::with_capacity(dp[m][n] as usize);
    let (mut i, mut j) = (m, n);
    while i > 0 && j > 0 {
        if a[i - 1] == b[j - 1] {
            result.push(a[i - 1]);
            i -= 1;
            j -= 1;
        } else if dp[i - 1][j] > dp[i][j - 1] {
            i -= 1;
        } else {
            j -= 1;
        }
    }
    result.reverse();
    result
}

fn compute_lcs_matches(a: &[u8], b: &[u8], min_match_len: usize) -> Vec<LcsMatch> {
    let m = a.len();
    let n = b.len();
    if m == 0 || n == 0 {
        return Vec::new();
    }
    let mut dp = vec![vec![0u32; n + 1]; m + 1];
    for i in 1..=m {
        for j in 1..=n {
            if a[i - 1] == b[j - 1] {
                dp[i][j] = dp[i - 1][j - 1] + 1;
            } else {
                dp[i][j] = dp[i - 1][j].max(dp[i][j - 1]);
            }
        }
    }
    // Backtrack to find matching segments
    let mut segments = Vec::new();
    let (mut i, mut j) = (m, n);
    while i > 0 && j > 0 {
        if a[i - 1] == b[j - 1] {
            let end_a = i - 1;
            let end_b = j - 1;
            while i > 0 && j > 0 && a[i - 1] == b[j - 1] {
                i -= 1;
                j -= 1;
            }
            let start_a = i;
            let start_b = j;
            let len = end_a - start_a + 1;
            if len >= min_match_len {
                segments.push(LcsMatch {
                    a_start: start_a,
                    a_end: end_a,
                    b_start: start_b,
                    b_end: end_b,
                    len,
                });
            }
        } else if dp[i - 1][j] > dp[i][j - 1] {
            i -= 1;
        } else {
            j -= 1;
        }
    }
    segments
}

fn lmpop(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    // LMPOP numkeys key [key ...] LEFT|RIGHT [COUNT count]
    if argv.len() < 4 {
        return Err(CommandError::WrongArity("LMPOP"));
    }
    let numkeys_val = parse_i64_arg(&argv[1])?;
    if numkeys_val <= 0 {
        return Err(CommandError::SyntaxError);
    }
    let numkeys = numkeys_val as usize;
    let keys_end = 2 + numkeys;
    if keys_end >= argv.len() {
        return Err(CommandError::SyntaxError);
    }
    let direction = &argv[keys_end];
    let left = if direction.eq_ignore_ascii_case(b"LEFT") {
        true
    } else if direction.eq_ignore_ascii_case(b"RIGHT") {
        false
    } else {
        return Err(CommandError::SyntaxError);
    };
    let mut count: usize = 1;
    let mut idx = keys_end + 1;
    if idx < argv.len() {
        if argv[idx].eq_ignore_ascii_case(b"COUNT") {
            idx += 1;
            if idx >= argv.len() {
                return Err(CommandError::SyntaxError);
            }
            let val = parse_i64_arg(&argv[idx])?;
            if val <= 0 {
                return Err(CommandError::SyntaxError);
            }
            count = val as usize;
        } else {
            return Err(CommandError::SyntaxError);
        }
    }
    for key in &argv[2..keys_end] {
        let len = store.llen(key, now_ms);
        match len {
            Ok(n) if n > 0 => {
                let mut popped = Vec::new();
                for _ in 0..count {
                    let val = if left {
                        store.lpop(key, now_ms)?
                    } else {
                        store.rpop(key, now_ms)?
                    };
                    match val {
                        Some(v) => popped.push(RespFrame::BulkString(Some(v))),
                        None => break,
                    }
                }
                return Ok(RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(key.clone())),
                    RespFrame::Array(Some(popped)),
                ])));
            }
            Ok(_) => continue,
            Err(StoreError::WrongType) => {
                return Err(CommandError::Store(StoreError::WrongType));
            }
            Err(e) => return Err(CommandError::Store(e)),
        }
    }
    Ok(RespFrame::Array(None))
}

fn zmpop(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    // ZMPOP numkeys key [key ...] MIN|MAX [COUNT count]
    if argv.len() < 4 {
        return Err(CommandError::WrongArity("ZMPOP"));
    }
    let numkeys_val = parse_i64_arg(&argv[1])?;
    if numkeys_val <= 0 {
        return Err(CommandError::SyntaxError);
    }
    let numkeys = numkeys_val as usize;
    let keys_end = 2 + numkeys;
    if keys_end >= argv.len() {
        return Err(CommandError::SyntaxError);
    }
    let direction = &argv[keys_end];
    let use_min = if direction.eq_ignore_ascii_case(b"MIN") {
        true
    } else if direction.eq_ignore_ascii_case(b"MAX") {
        false
    } else {
        return Err(CommandError::SyntaxError);
    };
    let mut count: usize = 1;
    let mut idx = keys_end + 1;
    if idx < argv.len() {
        if argv[idx].eq_ignore_ascii_case(b"COUNT") {
            idx += 1;
            if idx >= argv.len() {
                return Err(CommandError::SyntaxError);
            }
            let val = parse_i64_arg(&argv[idx])?;
            if val <= 0 {
                return Err(CommandError::SyntaxError);
            }
            count = val as usize;
        } else {
            return Err(CommandError::SyntaxError);
        }
    }
    for key in &argv[2..keys_end] {
        let card = store.zcard(key, now_ms);
        match card {
            Ok(n) if n > 0 => {
                let mut popped = Vec::new();
                for _ in 0..count {
                    let result = if use_min {
                        store.zpopmin(key, now_ms)?
                    } else {
                        store.zpopmax(key, now_ms)?
                    };
                    match result {
                        Some((member, score)) => {
                            popped.push(RespFrame::BulkString(Some(member)));
                            popped
                                .push(RespFrame::BulkString(Some(score.to_string().into_bytes())));
                        }
                        None => break,
                    }
                }
                return Ok(RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(key.clone())),
                    RespFrame::Array(Some(popped)),
                ])));
            }
            Ok(_) => continue,
            Err(StoreError::WrongType) => {
                return Err(CommandError::Store(StoreError::WrongType));
            }
            Err(e) => return Err(CommandError::Store(e)),
        }
    }
    Ok(RespFrame::Array(None))
}

fn bitop(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() < 4 {
        return Err(CommandError::WrongArity("BITOP"));
    }
    let op = &argv[1];
    let dest = &argv[2];
    let keys: Vec<&[u8]> = argv[3..].iter().map(|v| v.as_slice()).collect();

    if eq_ascii_command(op, b"NOT") && keys.len() != 1 {
        return Ok(RespFrame::Error(
            "ERR BITOP NOT must be called with a single source key.".to_string(),
        ));
    }

    let len = store
        .bitop(op, dest, &keys, now_ms)
        .map_err(CommandError::Store)?;
    Ok(RespFrame::Integer(i64::try_from(len).unwrap_or(i64::MAX)))
}

fn parse_zstore_args(
    argv: &[Vec<u8>],
    start: usize,
    numkeys: usize,
) -> Result<(Vec<f64>, Vec<u8>), CommandError> {
    let mut weights: Vec<f64> = Vec::new();
    let mut aggregate: Vec<u8> = b"SUM".to_vec();
    let mut i = start;
    while i < argv.len() {
        let kw = std::str::from_utf8(&argv[i]).unwrap_or("");
        if kw.eq_ignore_ascii_case("WEIGHTS") {
            i += 1;
            if i + numkeys > argv.len() {
                return Err(CommandError::SyntaxError);
            }
            for _ in 0..numkeys {
                let w = std::str::from_utf8(&argv[i])
                    .map_err(|_| CommandError::InvalidInteger)?
                    .parse::<f64>()
                    .map_err(|_| CommandError::InvalidInteger)?;
                weights.push(w);
                i += 1;
            }
        } else if kw.eq_ignore_ascii_case("AGGREGATE") {
            i += 1;
            if i < argv.len() {
                aggregate = argv[i].clone();
                i += 1;
            } else {
                return Err(CommandError::SyntaxError);
            }
        } else {
            return Err(CommandError::SyntaxError);
        }
    }
    Ok((weights, aggregate))
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
    if numkeys == 0 {
        return Ok(RespFrame::Error(
            "ERR at least 1 input key is needed for ZUNIONSTORE/ZINTERSTORE".to_string(),
        ));
    }
    if argv.len() < 3 + numkeys {
        return Err(CommandError::SyntaxError);
    }
    let keys: Vec<&[u8]> = argv[3..3 + numkeys].iter().map(|v| v.as_slice()).collect();
    let (weights, aggregate) = parse_zstore_args(argv, 3 + numkeys, numkeys)?;
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
    if numkeys == 0 {
        return Ok(RespFrame::Error(
            "ERR at least 1 input key is needed for ZUNIONSTORE/ZINTERSTORE".to_string(),
        ));
    }
    if argv.len() < 3 + numkeys {
        return Err(CommandError::SyntaxError);
    }
    let keys: Vec<&[u8]> = argv[3..3 + numkeys].iter().map(|v| v.as_slice()).collect();
    let (weights, aggregate) = parse_zstore_args(argv, 3 + numkeys, numkeys)?;
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
    let keyspace_size = store.dbsize(now_ms);
    let section = if argv.len() >= 2 {
        std::str::from_utf8(&argv[1]).unwrap_or("all")
    } else {
        "all"
    };
    let is_all = section.eq_ignore_ascii_case("all")
        || section.eq_ignore_ascii_case("everything")
        || section.eq_ignore_ascii_case("default");

    let mut info = String::new();

    // Server section
    if is_all || section.eq_ignore_ascii_case("server") {
        info.push_str("# Server\r\n");
        info.push_str("redis_version:7.2.0-frankenredis\r\n");
        info.push_str("redis_git_sha1:00000000\r\n");
        info.push_str("redis_git_dirty:0\r\n");
        info.push_str("redis_build_id:0\r\n");
        info.push_str("redis_mode:standalone\r\n");
        info.push_str("os:Linux\r\n");
        info.push_str("arch_bits:64\r\n");
        info.push_str("monotonic_clock:POSIX clock_gettime\r\n");
        info.push_str("multiplexing_api:epoll\r\n");
        info.push_str("gcc_version:0.0.0\r\n");
        info.push_str("process_id:1\r\n");
        info.push_str("run_id:00000000000000000000000000000000\r\n");
        info.push_str("tcp_port:6379\r\n");
        let uptime_s = now_ms / 1000;
        info.push_str(&format!("uptime_in_seconds:{uptime_s}\r\n"));
        info.push_str(&format!("uptime_in_days:{}\r\n", uptime_s / 86400));
        info.push_str("hz:10\r\n");
        info.push_str("configured_hz:10\r\n");
        info.push_str("lru_clock:0\r\n");
        info.push_str("executable:/usr/local/bin/frankenredis\r\n");
        info.push_str("config_file:\r\n");
        info.push_str("\r\n");
    }

    // Clients section
    if is_all || section.eq_ignore_ascii_case("clients") {
        info.push_str("# Clients\r\n");
        info.push_str("connected_clients:1\r\n");
        info.push_str("cluster_connections:0\r\n");
        info.push_str("maxclients:10000\r\n");
        info.push_str("client_recent_max_input_buffer:0\r\n");
        info.push_str("client_recent_max_output_buffer:0\r\n");
        info.push_str("total_clients_connected_including_replicas:1\r\n");
        info.push_str("blocked_clients:0\r\n");
        info.push_str("tracking_clients:0\r\n");
        info.push_str("clients_in_timeout_table:0\r\n");
        info.push_str("total_blocking_clients:0\r\n");
        info.push_str("total_blocking_clients_on_nokey:0\r\n");
        info.push_str("\r\n");
    }

    // Memory section
    if is_all || section.eq_ignore_ascii_case("memory") {
        info.push_str("# Memory\r\n");
        info.push_str("used_memory:0\r\n");
        info.push_str("used_memory_human:0B\r\n");
        info.push_str("used_memory_rss:0\r\n");
        info.push_str("used_memory_rss_human:0B\r\n");
        info.push_str("used_memory_peak:0\r\n");
        info.push_str("used_memory_peak_human:0B\r\n");
        info.push_str("used_memory_peak_perc:0.00%\r\n");
        info.push_str("used_memory_overhead:0\r\n");
        info.push_str("used_memory_startup:0\r\n");
        info.push_str("used_memory_dataset:0\r\n");
        info.push_str("used_memory_dataset_perc:0.00%\r\n");
        info.push_str("maxmemory:0\r\n");
        info.push_str("maxmemory_human:0B\r\n");
        info.push_str("maxmemory_policy:noeviction\r\n");
        info.push_str("mem_fragmentation_ratio:1.00\r\n");
        info.push_str("mem_allocator:rust-alloc\r\n");
        info.push_str("active_defrag_running:0\r\n");
        info.push_str("lazyfree_pending_objects:0\r\n");
        info.push_str("lazyfreed_objects:0\r\n");
        info.push_str("\r\n");
    }

    // Persistence section
    if is_all || section.eq_ignore_ascii_case("persistence") {
        info.push_str("# Persistence\r\n");
        info.push_str("loading:0\r\n");
        info.push_str("async_loading:0\r\n");
        info.push_str("current_cow_peak:0\r\n");
        info.push_str("current_cow_size:0\r\n");
        info.push_str("current_cow_size_age:0\r\n");
        info.push_str("current_fork_perc:0.00\r\n");
        info.push_str("current_save_keys_processed:0\r\n");
        info.push_str("current_save_keys_total:0\r\n");
        info.push_str("rdb_changes_since_last_save:0\r\n");
        info.push_str("rdb_bgsave_in_progress:0\r\n");
        info.push_str("rdb_last_save_time:0\r\n");
        info.push_str("rdb_last_bgsave_status:ok\r\n");
        info.push_str("rdb_last_bgsave_time_sec:-1\r\n");
        info.push_str("rdb_current_bgsave_time_sec:-1\r\n");
        info.push_str("rdb_saves:0\r\n");
        info.push_str("rdb_last_cow_size:0\r\n");
        info.push_str("aof_enabled:0\r\n");
        info.push_str("aof_rewrite_in_progress:0\r\n");
        info.push_str("aof_rewrite_scheduled:0\r\n");
        info.push_str("aof_last_rewrite_time_sec:-1\r\n");
        info.push_str("aof_current_rewrite_time_sec:-1\r\n");
        info.push_str("aof_last_bgrewrite_status:ok\r\n");
        info.push_str("aof_last_write_status:ok\r\n");
        info.push_str("aof_last_cow_size:0\r\n");
        info.push_str("\r\n");
    }

    // Stats section
    if is_all || section.eq_ignore_ascii_case("stats") {
        info.push_str("# Stats\r\n");
        info.push_str("total_connections_received:1\r\n");
        info.push_str("total_commands_processed:0\r\n");
        info.push_str("instantaneous_ops_per_sec:0\r\n");
        info.push_str("total_net_input_bytes:0\r\n");
        info.push_str("total_net_output_bytes:0\r\n");
        info.push_str("total_net_repl_input_bytes:0\r\n");
        info.push_str("total_net_repl_output_bytes:0\r\n");
        info.push_str("instantaneous_input_kbps:0.00\r\n");
        info.push_str("instantaneous_output_kbps:0.00\r\n");
        info.push_str("instantaneous_input_repl_kbps:0.00\r\n");
        info.push_str("instantaneous_output_repl_kbps:0.00\r\n");
        info.push_str("rejected_connections:0\r\n");
        info.push_str("sync_full:0\r\n");
        info.push_str("sync_partial_ok:0\r\n");
        info.push_str("sync_partial_err:0\r\n");
        info.push_str("expired_keys:0\r\n");
        info.push_str("expired_stale_perc:0.00\r\n");
        info.push_str("expired_time_cap_reached_count:0\r\n");
        info.push_str("expire_cycle_cpu_milliseconds:0\r\n");
        info.push_str("evicted_keys:0\r\n");
        info.push_str("evicted_clients:0\r\n");
        info.push_str("total_keys_expired:0\r\n");
        info.push_str("total_keys_evicted:0\r\n");
        info.push_str("keyspace_hits:0\r\n");
        info.push_str("keyspace_misses:0\r\n");
        info.push_str("pubsub_channels:0\r\n");
        info.push_str("pubsub_patterns:0\r\n");
        info.push_str("pubsub_shardchannels:0\r\n");
        info.push_str("latest_fork_usec:0\r\n");
        info.push_str("total_forks:0\r\n");
        info.push_str("migrate_cached_sockets:0\r\n");
        info.push_str("slave_expires_tracked_keys:0\r\n");
        info.push_str("active_defrag_hits:0\r\n");
        info.push_str("active_defrag_misses:0\r\n");
        info.push_str("active_defrag_key_hits:0\r\n");
        info.push_str("active_defrag_key_misses:0\r\n");
        info.push_str("tracking_total_keys:0\r\n");
        info.push_str("tracking_total_items:0\r\n");
        info.push_str("tracking_total_prefixes:0\r\n");
        info.push_str("unexpected_error_replies:0\r\n");
        info.push_str("total_error_replies:0\r\n");
        info.push_str("dump_payload_sanitizations:0\r\n");
        info.push_str("total_reads_processed:0\r\n");
        info.push_str("total_writes_processed:0\r\n");
        info.push_str("io_threaded_reads_processed:0\r\n");
        info.push_str("io_threaded_writes_processed:0\r\n");
        info.push_str("reply_buffer_shrinks:0\r\n");
        info.push_str("reply_buffer_expands:0\r\n");
        info.push_str("current_eviction_exceeded_time:0\r\n");
        info.push_str("current_active_defrag_time:0\r\n");
        info.push_str("\r\n");
    }

    // Replication section
    if is_all || section.eq_ignore_ascii_case("replication") {
        info.push_str("# Replication\r\n");
        info.push_str("role:master\r\n");
        info.push_str("connected_slaves:0\r\n");
        info.push_str("master_failover_state:no-failover\r\n");
        info.push_str("master_replid:0000000000000000000000000000000000000000\r\n");
        info.push_str("master_replid2:0000000000000000000000000000000000000000\r\n");
        info.push_str("master_repl_offset:0\r\n");
        info.push_str("second_repl_offset:-1\r\n");
        info.push_str("repl_backlog_active:0\r\n");
        info.push_str("repl_backlog_size:1048576\r\n");
        info.push_str("repl_backlog_first_byte_offset:0\r\n");
        info.push_str("repl_backlog_histlen:0\r\n");
        info.push_str("\r\n");
    }

    // CPU section
    if is_all || section.eq_ignore_ascii_case("cpu") {
        info.push_str("# CPU\r\n");
        info.push_str("used_cpu_sys:0.000000\r\n");
        info.push_str("used_cpu_user:0.000000\r\n");
        info.push_str("used_cpu_sys_children:0.000000\r\n");
        info.push_str("used_cpu_user_children:0.000000\r\n");
        info.push_str("used_cpu_sys_main_thread:0.000000\r\n");
        info.push_str("used_cpu_user_main_thread:0.000000\r\n");
        info.push_str("\r\n");
    }

    // Modules section
    if is_all || section.eq_ignore_ascii_case("modules") {
        info.push_str("# Modules\r\n");
        info.push_str("\r\n");
    }

    // Errorstats section
    if is_all || section.eq_ignore_ascii_case("errorstats") {
        info.push_str("# Errorstats\r\n");
        info.push_str("\r\n");
    }

    // Cluster section
    if is_all || section.eq_ignore_ascii_case("cluster") {
        info.push_str("# Cluster\r\n");
        info.push_str("cluster_enabled:0\r\n");
        info.push_str("\r\n");
    }

    // Keyspace section
    if is_all || section.eq_ignore_ascii_case("keyspace") {
        info.push_str("# Keyspace\r\n");
        if keyspace_size > 0 {
            info.push_str(&format!("db0:keys={keyspace_size},expires=0,avg_ttl=0\r\n"));
        }
        info.push_str("\r\n");
    }

    Ok(RespFrame::BulkString(Some(info.into_bytes())))
}

/// Static command metadata table: (name, arity, flags, first_key, last_key, step)
/// Arity: positive = exact arg count incl. command, negative = minimum args
const COMMAND_TABLE: &[(&str, i64, &str, i64, i64, i64)] = &[
    ("ping", 1, "fast", 0, 0, 0),
    ("echo", 2, "fast", 0, 0, 0),
    ("set", -3, "write denyoom", 1, 1, 1),
    ("get", 2, "readonly fast", 1, 1, 1),
    ("del", -2, "write", 1, -1, 1),
    ("incr", 2, "write denyoom fast", 1, 1, 1),
    ("decr", 2, "write denyoom fast", 1, 1, 1),
    ("incrby", 3, "write denyoom fast", 1, 1, 1),
    ("decrby", 3, "write denyoom fast", 1, 1, 1),
    ("incrbyfloat", 3, "write denyoom fast", 1, 1, 1),
    ("mget", -2, "readonly fast", 1, -1, 1),
    ("mset", -3, "write denyoom", 1, -1, 2),
    ("msetnx", -3, "write denyoom", 1, -1, 2),
    ("append", 3, "write denyoom fast", 1, 1, 1),
    ("strlen", 2, "readonly fast", 1, 1, 1),
    ("getrange", 4, "readonly", 1, 1, 1),
    ("setrange", 4, "write denyoom", 1, 1, 1),
    ("setnx", 3, "write denyoom fast", 1, 1, 1),
    ("setex", 4, "write denyoom", 1, 1, 1),
    ("psetex", 4, "write denyoom", 1, 1, 1),
    ("getset", 3, "write denyoom fast", 1, 1, 1),
    ("getdel", 2, "write fast", 1, 1, 1),
    ("getex", -2, "write fast", 1, 1, 1),
    ("substr", 4, "readonly", 1, 1, 1),
    ("lcs", -3, "readonly", 1, 2, 1),
    ("expire", 3, "write fast", 1, 1, 1),
    ("pexpire", 3, "write fast", 1, 1, 1),
    ("expireat", 3, "write fast", 1, 1, 1),
    ("pexpireat", 3, "write fast", 1, 1, 1),
    ("persist", 2, "write fast", 1, 1, 1),
    ("ttl", 2, "readonly fast", 1, 1, 1),
    ("pttl", 2, "readonly fast", 1, 1, 1),
    ("expiretime", 2, "readonly fast", 1, 1, 1),
    ("pexpiretime", 2, "readonly fast", 1, 1, 1),
    ("exists", -2, "readonly fast", 1, -1, 1),
    ("type", 2, "readonly fast", 1, 1, 1),
    ("rename", 3, "write", 1, 2, 1),
    ("renamenx", 3, "write fast", 1, 2, 1),
    ("keys", 2, "readonly sort_for_script", 0, 0, 0),
    ("randomkey", 1, "readonly", 0, 0, 0),
    ("scan", -2, "readonly", 0, 0, 0),
    ("dbsize", 1, "readonly fast", 0, 0, 0),
    ("flushdb", -1, "write", 0, 0, 0),
    ("select", 2, "fast", 0, 0, 0),
    ("move", 3, "write fast", 1, 1, 1),
    ("copy", -3, "write", 1, 2, 1),
    ("sort", -2, "write denyoom", 1, 1, 1),
    ("dump", 2, "readonly", 1, 1, 1),
    ("restore", -4, "write denyoom", 1, 1, 1),
    ("unlink", -2, "write fast", 1, -1, 1),
    ("touch", -2, "readonly fast", 1, -1, 1),
    ("hset", -4, "write denyoom fast", 1, 1, 1),
    ("hget", 3, "readonly fast", 1, 1, 1),
    ("hdel", -3, "write fast", 1, 1, 1),
    ("hexists", 3, "readonly fast", 1, 1, 1),
    ("hlen", 2, "readonly fast", 1, 1, 1),
    ("hgetall", 2, "readonly", 1, 1, 1),
    ("hkeys", 2, "readonly sort_for_script", 1, 1, 1),
    ("hvals", 2, "readonly sort_for_script", 1, 1, 1),
    ("hmget", -3, "readonly fast", 1, 1, 1),
    ("hmset", -4, "write denyoom fast", 1, 1, 1),
    ("hincrby", 4, "write denyoom fast", 1, 1, 1),
    ("hincrbyfloat", 4, "write denyoom fast", 1, 1, 1),
    ("hsetnx", 4, "write denyoom fast", 1, 1, 1),
    ("hstrlen", 3, "readonly fast", 1, 1, 1),
    ("hrandfield", -2, "readonly", 1, 1, 1),
    ("hscan", -3, "readonly", 1, 1, 1),
    ("lpush", -3, "write denyoom fast", 1, 1, 1),
    ("rpush", -3, "write denyoom fast", 1, 1, 1),
    ("lpushx", -3, "write denyoom fast", 1, 1, 1),
    ("rpushx", -3, "write denyoom fast", 1, 1, 1),
    ("lpop", -2, "write fast", 1, 1, 1),
    ("rpop", -2, "write fast", 1, 1, 1),
    ("llen", 2, "readonly fast", 1, 1, 1),
    ("lrange", 4, "readonly", 1, 1, 1),
    ("lindex", 3, "readonly", 1, 1, 1),
    ("lset", 4, "write denyoom", 1, 1, 1),
    ("linsert", 5, "write denyoom", 1, 1, 1),
    ("lrem", 4, "write", 1, 1, 1),
    ("ltrim", 4, "write", 1, 1, 1),
    ("lpos", -3, "readonly", 1, 1, 1),
    ("lmove", 5, "write denyoom", 1, 2, 1),
    ("lmpop", -4, "write fast", 0, 0, 0),
    ("rpoplpush", 3, "write denyoom", 1, 2, 1),
    ("blpop", -3, "write denyoom", 1, -2, 1),
    ("brpop", -3, "write denyoom", 1, -2, 1),
    ("blmove", 6, "write denyoom", 1, 2, 1),
    ("blmpop", -5, "write denyoom", 0, 0, 0),
    ("brpoplpush", 4, "write denyoom", 1, 2, 1),
    ("sadd", -3, "write denyoom fast", 1, 1, 1),
    ("srem", -3, "write fast", 1, 1, 1),
    ("smembers", 2, "readonly sort_for_script", 1, 1, 1),
    ("scard", 2, "readonly fast", 1, 1, 1),
    ("sismember", 3, "readonly fast", 1, 1, 1),
    ("smismember", -3, "readonly fast", 1, 1, 1),
    ("sinter", -2, "readonly sort_for_script", 1, -1, 1),
    ("sinterstore", -3, "write denyoom", 1, -1, 1),
    ("sintercard", -3, "readonly", 0, 0, 0),
    ("sunion", -2, "readonly sort_for_script", 1, -1, 1),
    ("sunionstore", -3, "write denyoom", 1, -1, 1),
    ("sdiff", -2, "readonly sort_for_script", 1, -1, 1),
    ("sdiffstore", -3, "write denyoom", 1, -1, 1),
    ("spop", -2, "write fast", 1, 1, 1),
    ("srandmember", -2, "readonly", 1, 1, 1),
    ("smove", 4, "write fast", 1, 2, 1),
    ("sscan", -3, "readonly", 1, 1, 1),
    ("zadd", -4, "write denyoom fast", 1, 1, 1),
    ("zrem", -3, "write fast", 1, 1, 1),
    ("zscore", 3, "readonly fast", 1, 1, 1),
    ("zmscore", -3, "readonly fast", 1, 1, 1),
    ("zcard", 2, "readonly fast", 1, 1, 1),
    ("zrank", 3, "readonly fast", 1, 1, 1),
    ("zrevrank", 3, "readonly fast", 1, 1, 1),
    ("zrange", -4, "readonly", 1, 1, 1),
    ("zrevrange", 4, "readonly", 1, 1, 1),
    ("zrangebyscore", -4, "readonly", 1, 1, 1),
    ("zrevrangebyscore", -4, "readonly", 1, 1, 1),
    ("zrangebylex", -4, "readonly", 1, 1, 1),
    ("zrevrangebylex", -4, "readonly", 1, 1, 1),
    ("zcount", 4, "readonly fast", 1, 1, 1),
    ("zlexcount", 4, "readonly fast", 1, 1, 1),
    ("zincrby", 4, "write denyoom fast", 1, 1, 1),
    ("zpopmin", -2, "write fast", 1, 1, 1),
    ("zpopmax", -2, "write fast", 1, 1, 1),
    ("zrandmember", -2, "readonly", 1, 1, 1),
    ("zunionstore", -4, "write denyoom", 1, 1, 1),
    ("zinterstore", -4, "write denyoom", 1, 1, 1),
    ("zdiff", -3, "readonly sort_for_script", 0, 0, 0),
    ("zdiffstore", -4, "write denyoom", 1, 1, 1),
    ("zinter", -3, "readonly sort_for_script", 0, 0, 0),
    ("zunion", -3, "readonly sort_for_script", 0, 0, 0),
    ("zintercard", -3, "readonly", 0, 0, 0),
    ("zmpop", -4, "write fast", 0, 0, 0),
    ("zremrangebyrank", 4, "write", 1, 1, 1),
    ("zremrangebyscore", 4, "write", 1, 1, 1),
    ("zremrangebylex", 4, "write", 1, 1, 1),
    ("zscan", -3, "readonly", 1, 1, 1),
    ("pfadd", -2, "write denyoom fast", 1, 1, 1),
    ("pfcount", -2, "readonly", 1, -1, 1),
    ("pfmerge", -2, "write denyoom", 1, -1, 1),
    ("setbit", 4, "write denyoom", 1, 1, 1),
    ("getbit", 3, "readonly fast", 1, 1, 1),
    ("bitcount", -2, "readonly", 1, 1, 1),
    ("bitpos", -3, "readonly", 1, 1, 1),
    ("bitop", -4, "write denyoom", 2, -1, 1),
    ("bitfield", -2, "write denyoom", 1, 1, 1),
    ("geoadd", -5, "write denyoom", 1, 1, 1),
    ("geopos", -2, "readonly", 1, 1, 1),
    ("geodist", -4, "readonly", 1, 1, 1),
    ("geohash", -2, "readonly", 1, 1, 1),
    ("georadius", -6, "write", 1, 1, 1),
    ("georadiusbymember", -5, "write", 1, 1, 1),
    ("geosearch", -7, "readonly", 1, 1, 1),
    ("geosearchstore", -8, "write denyoom", 1, 2, 1),
    ("xadd", -5, "write denyoom fast", 1, 1, 1),
    ("xlen", 2, "readonly fast", 1, 1, 1),
    ("xdel", -3, "write fast", 1, 1, 1),
    ("xtrim", -4, "write", 1, 1, 1),
    ("xread", -4, "readonly", 0, 0, 0),
    ("xreadgroup", -7, "write", 0, 0, 0),
    ("xclaim", -6, "write fast", 1, 1, 1),
    ("xautoclaim", -7, "write fast", 1, 1, 1),
    ("xpending", -3, "readonly", 1, 1, 1),
    ("xack", -4, "write fast", 1, 1, 1),
    ("xsetid", -3, "write", 1, 1, 1),
    ("xinfo", -2, "readonly", 2, 2, 1),
    ("xgroup", -2, "write", 2, 2, 1),
    ("xrange", -4, "readonly", 1, 1, 1),
    ("xrevrange", -4, "readonly", 1, 1, 1),
    ("subscribe", -2, "pubsub", 0, 0, 0),
    ("unsubscribe", -1, "pubsub", 0, 0, 0),
    ("psubscribe", -2, "pubsub", 0, 0, 0),
    ("punsubscribe", -1, "pubsub", 0, 0, 0),
    ("publish", 3, "pubsub fast", 0, 0, 0),
    ("pubsub", -2, "pubsub", 0, 0, 0),
    ("eval", -3, "scripting", 0, 0, 0),
    ("evalsha", -3, "scripting", 0, 0, 0),
    ("eval_ro", -3, "scripting readonly", 0, 0, 0),
    ("evalsha_ro", -3, "scripting readonly", 0, 0, 0),
    ("fcall", -3, "scripting", 0, 0, 0),
    ("fcall_ro", -3, "scripting readonly", 0, 0, 0),
    ("script", -2, "scripting", 0, 0, 0),
    ("multi", 1, "fast", 0, 0, 0),
    ("exec", 1, "slow", 0, 0, 0),
    ("discard", 1, "fast", 0, 0, 0),
    ("watch", -2, "fast", 1, -1, 1),
    ("unwatch", 1, "fast", 0, 0, 0),
    ("auth", -2, "fast", 0, 0, 0),
    ("hello", -1, "fast", 0, 0, 0),
    ("quit", 1, "fast", 0, 0, 0),
    ("reset", 1, "fast", 0, 0, 0),
    ("info", -1, "fast", 0, 0, 0),
    ("config", -2, "admin", 0, 0, 0),
    ("acl", -2, "admin", 0, 0, 0),
    ("command", -1, "fast", 0, 0, 0),
    ("client", -2, "admin", 0, 0, 0),
    ("time", 1, "fast", 0, 0, 0),
    ("save", 1, "admin", 0, 0, 0),
    ("bgsave", -1, "admin", 0, 0, 0),
    ("bgrewriteaof", 1, "admin", 0, 0, 0),
    ("lastsave", 1, "fast", 0, 0, 0),
    ("swapdb", 3, "write fast", 0, 0, 0),
    ("object", -2, "readonly", 2, 2, 1),
    ("memory", -2, "readonly", 0, 0, 0),
    ("slowlog", -2, "admin", 0, 0, 0),
    ("debug", -2, "admin", 0, 0, 0),
    ("role", 1, "fast", 0, 0, 0),
    ("shutdown", -1, "admin", 0, 0, 0),
    ("latency", -2, "admin", 0, 0, 0),
    ("wait", 3, "slow", 0, 0, 0),
    ("waitaof", 4, "slow", 0, 0, 0),
    ("lolwut", -1, "fast", 0, 0, 0),
    ("cluster", -2, "admin", 0, 0, 0),
    ("asking", 1, "fast", 0, 0, 0),
    ("readonly", 1, "fast", 0, 0, 0),
    ("readwrite", 1, "fast", 0, 0, 0),
    ("replicaof", 3, "admin", 0, 0, 0),
    ("slaveof", 3, "admin", 0, 0, 0),
    ("function", -2, "scripting", 0, 0, 0),
    ("ssubscribe", -2, "pubsub", 0, 0, 0),
    ("sunsubscribe", -1, "pubsub", 0, 0, 0),
    ("spublish", 3, "pubsub fast", 0, 0, 0),
    ("sort_ro", -2, "readonly fast", 1, 1, 1),
    ("zrangestore", -5, "write denyoom", 1, 2, 1),
];

/// Return commands that belong to the given ACL category.
/// Redis maps commands to categories based on data type (string, hash, list, set,
/// sortedset, geo, stream, bitmap, hyperloglog), access type (read, write),
/// and command group (admin, fast, slow, pubsub, scripting, transaction, connection,
/// keyspace, generic, blocking, dangerous, server).
pub fn commands_in_acl_category(category: &str) -> Vec<&'static str> {
    // Build the category → command mapping from COMMAND_TABLE flags and command names.
    let cat_lower = category.to_ascii_lowercase();
    COMMAND_TABLE
        .iter()
        .filter(|&&(name, _arity, flags, _first, _last, _step)| {
            command_matches_acl_category(name, flags, &cat_lower)
        })
        .map(|&(name, ..)| name)
        .collect()
}

fn command_matches_acl_category(name: &str, flags: &str, category: &str) -> bool {
    // Direct flag match (e.g., "write", "fast", "admin", "scripting", "pubsub")
    let flag_list: Vec<&str> = flags.split_whitespace().collect();

    match category {
        // Access-type categories from flags
        "read" => flag_list.contains(&"readonly"),
        "write" => flag_list.contains(&"write"),
        "fast" => flag_list.contains(&"fast"),
        "slow" => flag_list.contains(&"slow"),
        "admin" => flag_list.contains(&"admin"),
        "pubsub" => flag_list.contains(&"pubsub"),
        "scripting" => flag_list.contains(&"scripting"),
        "dangerous" => flag_list.contains(&"admin") || flag_list.contains(&"dangerous"),
        "blocking" => matches!(
            name,
            "blpop" | "brpop" | "blmove" | "blmpop" | "brpoplpush" | "wait" | "waitaof"
        ),
        "connection" => matches!(
            name,
            "auth" | "hello" | "ping" | "echo" | "quit" | "reset" | "select" | "client"
        ),
        "transaction" => matches!(name, "multi" | "exec" | "discard" | "watch" | "unwatch"),
        "server" => matches!(
            name,
            "info"
                | "config"
                | "acl"
                | "command"
                | "time"
                | "dbsize"
                | "flushdb"
                | "save"
                | "bgsave"
                | "bgrewriteaof"
                | "lastsave"
                | "slowlog"
                | "debug"
                | "role"
                | "shutdown"
                | "latency"
                | "lolwut"
                | "memory"
                | "replicaof"
                | "slaveof"
                | "cluster"
                | "swapdb"
                | "object"
        ),
        "generic" | "keyspace" => matches!(
            name,
            "del"
                | "exists"
                | "type"
                | "keys"
                | "randomkey"
                | "scan"
                | "rename"
                | "renamenx"
                | "expire"
                | "pexpire"
                | "expireat"
                | "pexpireat"
                | "persist"
                | "ttl"
                | "pttl"
                | "expiretime"
                | "pexpiretime"
                | "dump"
                | "restore"
                | "unlink"
                | "touch"
                | "sort"
                | "sort_ro"
                | "copy"
                | "move"
                | "object"
                | "wait"
                | "waitaof"
        ),
        // Data-type categories
        "string" => matches!(
            name,
            "set"
                | "get"
                | "append"
                | "strlen"
                | "getrange"
                | "setrange"
                | "incr"
                | "decr"
                | "incrby"
                | "decrby"
                | "incrbyfloat"
                | "mget"
                | "mset"
                | "msetnx"
                | "setnx"
                | "setex"
                | "psetex"
                | "getset"
                | "getdel"
                | "getex"
                | "substr"
                | "lcs"
        ),
        "hash" => matches!(
            name,
            "hset"
                | "hget"
                | "hdel"
                | "hexists"
                | "hlen"
                | "hgetall"
                | "hkeys"
                | "hvals"
                | "hmget"
                | "hmset"
                | "hincrby"
                | "hincrbyfloat"
                | "hsetnx"
                | "hstrlen"
                | "hrandfield"
                | "hscan"
        ),
        "list" => matches!(
            name,
            "lpush"
                | "rpush"
                | "lpushx"
                | "rpushx"
                | "lpop"
                | "rpop"
                | "llen"
                | "lrange"
                | "lindex"
                | "lset"
                | "linsert"
                | "lrem"
                | "ltrim"
                | "lpos"
                | "lmove"
                | "lmpop"
                | "rpoplpush"
                | "blpop"
                | "brpop"
                | "blmove"
                | "blmpop"
                | "brpoplpush"
        ),
        "set" => matches!(
            name,
            "sadd"
                | "srem"
                | "smembers"
                | "scard"
                | "sismember"
                | "smismember"
                | "sinter"
                | "sinterstore"
                | "sintercard"
                | "sunion"
                | "sunionstore"
                | "sdiff"
                | "sdiffstore"
                | "spop"
                | "srandmember"
                | "smove"
                | "sscan"
        ),
        "sortedset" => matches!(
            name,
            "zadd"
                | "zrem"
                | "zscore"
                | "zmscore"
                | "zcard"
                | "zrank"
                | "zrevrank"
                | "zrange"
                | "zrevrange"
                | "zrangebyscore"
                | "zrevrangebyscore"
                | "zrangebylex"
                | "zrevrangebylex"
                | "zcount"
                | "zlexcount"
                | "zincrby"
                | "zpopmin"
                | "zpopmax"
                | "zrandmember"
                | "zunionstore"
                | "zinterstore"
                | "zdiff"
                | "zdiffstore"
                | "zinter"
                | "zunion"
                | "zintercard"
                | "zmpop"
                | "zremrangebyrank"
                | "zremrangebyscore"
                | "zremrangebylex"
                | "zscan"
                | "zrangestore"
        ),
        "bitmap" => matches!(
            name,
            "setbit" | "getbit" | "bitcount" | "bitpos" | "bitop" | "bitfield"
        ),
        "hyperloglog" => matches!(name, "pfadd" | "pfcount" | "pfmerge"),
        "geo" => matches!(
            name,
            "geoadd"
                | "geopos"
                | "geodist"
                | "geohash"
                | "georadius"
                | "georadiusbymember"
                | "geosearch"
                | "geosearchstore"
        ),
        "stream" => matches!(
            name,
            "xadd"
                | "xlen"
                | "xdel"
                | "xtrim"
                | "xread"
                | "xreadgroup"
                | "xclaim"
                | "xautoclaim"
                | "xpending"
                | "xack"
                | "xsetid"
                | "xinfo"
                | "xgroup"
                | "xrange"
                | "xrevrange"
        ),
        _ => false,
    }
}

fn command_cmd(argv: &[Vec<u8>]) -> Result<RespFrame, CommandError> {
    if argv.len() == 1 {
        // COMMAND with no sub-command: return full command info for all commands
        let entries: Vec<RespFrame> = COMMAND_TABLE
            .iter()
            .map(|&(name, arity, flags, first_key, last_key, step)| {
                command_info_entry(name, arity, flags, first_key, last_key, step)
            })
            .collect();
        return Ok(RespFrame::Array(Some(entries)));
    }
    let sub = std::str::from_utf8(&argv[1]).map_err(|_| CommandError::InvalidUtf8Argument)?;
    if sub.eq_ignore_ascii_case("COUNT") {
        Ok(RespFrame::Integer(COMMAND_TABLE.len() as i64))
    } else if sub.eq_ignore_ascii_case("LIST") {
        let names: Vec<RespFrame> = COMMAND_TABLE
            .iter()
            .map(|&(name, ..)| RespFrame::BulkString(Some(name.as_bytes().to_vec())))
            .collect();
        Ok(RespFrame::Array(Some(names)))
    } else if sub.eq_ignore_ascii_case("INFO") {
        if argv.len() < 3 {
            let entries: Vec<RespFrame> = COMMAND_TABLE
                .iter()
                .map(|&(name, arity, flags, first_key, last_key, step)| {
                    command_info_entry(name, arity, flags, first_key, last_key, step)
                })
                .collect();
            return Ok(RespFrame::Array(Some(entries)));
        }
        let mut entries = Vec::new();
        for arg in &argv[2..] {
            let cmd_name =
                std::str::from_utf8(arg).map_err(|_| CommandError::InvalidUtf8Argument)?;
            let found = COMMAND_TABLE
                .iter()
                .find(|&&(name, ..)| name.eq_ignore_ascii_case(cmd_name));
            match found {
                Some(&(name, arity, flags, first_key, last_key, step)) => {
                    entries.push(command_info_entry(
                        name, arity, flags, first_key, last_key, step,
                    ));
                }
                None => entries.push(RespFrame::BulkString(None)),
            }
        }
        Ok(RespFrame::Array(Some(entries)))
    } else if sub.eq_ignore_ascii_case("DOCS") || sub.eq_ignore_ascii_case("GETKEYS") {
        Ok(RespFrame::Array(Some(Vec::new())))
    } else {
        Ok(RespFrame::Error(format!(
            "ERR unknown subcommand or wrong number of arguments for 'command|{sub}'"
        )))
    }
}

/// Build a Redis COMMAND INFO entry: [name, arity, flags, first_key, last_key, step]
fn command_info_entry(
    name: &str,
    arity: i64,
    flags: &str,
    first_key: i64,
    last_key: i64,
    step: i64,
) -> RespFrame {
    let flag_list: Vec<RespFrame> = flags
        .split_whitespace()
        .map(|f| RespFrame::SimpleString(f.to_string()))
        .collect();
    RespFrame::Array(Some(vec![
        RespFrame::BulkString(Some(name.as_bytes().to_vec())),
        RespFrame::Integer(arity),
        RespFrame::Array(Some(flag_list)),
        RespFrame::Integer(first_key),
        RespFrame::Integer(last_key),
        RespFrame::Integer(step),
    ]))
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
    } else if sub.eq_ignore_ascii_case("KILL") {
        // CLIENT KILL [ip:port | ID client-id | ...]
        // In standalone single-connection mode, always return 0 (no clients killed)
        Ok(RespFrame::Integer(0))
    } else if sub.eq_ignore_ascii_case("PAUSE") {
        // CLIENT PAUSE timeout [WRITE|ALL]
        if argv.len() < 3 {
            return Err(CommandError::WrongArity("CLIENT"));
        }
        Ok(RespFrame::SimpleString("OK".to_string()))
    } else if sub.eq_ignore_ascii_case("UNPAUSE") {
        Ok(RespFrame::SimpleString("OK".to_string()))
    } else if sub.eq_ignore_ascii_case("TRACKING") {
        // CLIENT TRACKING ON|OFF [REDIRECT id] [PREFIX prefix ...] [BCAST] [OPTIN] [OPTOUT] [NOLOOP]
        if argv.len() < 3 {
            return Err(CommandError::WrongArity("CLIENT"));
        }
        Ok(RespFrame::SimpleString("OK".to_string()))
    } else if sub.eq_ignore_ascii_case("CACHING") {
        // CLIENT CACHING YES|NO
        if argv.len() < 3 {
            return Err(CommandError::WrongArity("CLIENT"));
        }
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

#[allow(clippy::type_complexity)]
fn parse_scan_args(
    argv: &[Vec<u8>],
    start_idx: usize,
) -> Result<(Option<Vec<u8>>, usize, Option<Vec<u8>>), CommandError> {
    let mut pattern: Option<Vec<u8>> = None;
    let mut count: usize = 10;
    let mut type_filter: Option<Vec<u8>> = None;
    let mut i = start_idx;
    while i < argv.len() {
        let kw = std::str::from_utf8(&argv[i]).unwrap_or("");
        if kw.eq_ignore_ascii_case("MATCH") {
            if i + 1 >= argv.len() {
                return Err(CommandError::SyntaxError);
            }
            pattern = Some(argv[i + 1].clone());
            i += 2;
        } else if kw.eq_ignore_ascii_case("COUNT") {
            if i + 1 >= argv.len() {
                return Err(CommandError::SyntaxError);
            }
            let c = std::str::from_utf8(&argv[i + 1])
                .ok()
                .and_then(|s| s.parse::<i64>().ok())
                .ok_or(CommandError::InvalidInteger)?;
            if c <= 0 {
                return Err(CommandError::InvalidInteger);
            }
            count = c as usize;
            i += 2;
        } else if kw.eq_ignore_ascii_case("TYPE") {
            if i + 1 >= argv.len() {
                return Err(CommandError::SyntaxError);
            }
            type_filter = Some(argv[i + 1].clone());
            i += 2;
        } else {
            return Err(CommandError::SyntaxError);
        }
    }
    Ok((pattern, count, type_filter))
}

fn scan(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() < 2 {
        return Err(CommandError::WrongArity("SCAN"));
    }
    let cursor = std::str::from_utf8(&argv[1])
        .map_err(|_| CommandError::InvalidInteger)?
        .parse::<u64>()
        .map_err(|_| CommandError::InvalidInteger)?;

    let (pattern, count, type_filter) = parse_scan_args(argv, 2)?;
    let (next_cursor, keys) = store.scan(cursor, pattern.as_deref(), count, now_ms);

    // Apply TYPE filter if specified
    let keys: Vec<Vec<u8>> = if let Some(ref tf) = type_filter {
        let tf_str = std::str::from_utf8(tf).unwrap_or("");
        keys.into_iter()
            .filter(|k| {
                store
                    .key_type(k, now_ms)
                    .is_some_and(|t| t.eq_ignore_ascii_case(tf_str))
            })
            .collect()
    } else {
        keys
    };

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

    let (pattern, count, _type_filter) = parse_scan_args(argv, 3)?;
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

    let (pattern, count, _type_filter) = parse_scan_args(argv, 3)?;
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

    let (pattern, count, _type_filter) = parse_scan_args(argv, 3)?;
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

fn object_cmd(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() < 2 {
        return Err(CommandError::WrongArity("OBJECT"));
    }
    let sub = std::str::from_utf8(&argv[1]).map_err(|_| CommandError::InvalidUtf8Argument)?;
    if sub.eq_ignore_ascii_case("ENCODING") {
        if argv.len() < 3 {
            return Err(CommandError::WrongArity("OBJECT"));
        }
        let encoding = store.object_encoding(&argv[2], now_ms);
        match encoding {
            Some(enc) => Ok(RespFrame::BulkString(Some(enc.as_bytes().to_vec()))),
            None => Ok(RespFrame::Error("ERR no such key".to_string())),
        }
    } else if sub.eq_ignore_ascii_case("REFCOUNT") {
        if argv.len() < 3 {
            return Err(CommandError::WrongArity("OBJECT"));
        }
        if store.exists(&argv[2], now_ms) {
            Ok(RespFrame::Integer(1))
        } else {
            Ok(RespFrame::Error("ERR no such key".to_string()))
        }
    } else if sub.eq_ignore_ascii_case("IDLETIME") {
        if argv.len() < 3 {
            return Err(CommandError::WrongArity("OBJECT"));
        }
        match store.object_idletime(&argv[2], now_ms) {
            Some(idle_secs) => Ok(RespFrame::Integer(idle_secs as i64)),
            None => Ok(RespFrame::Error("ERR no such key".to_string())),
        }
    } else if sub.eq_ignore_ascii_case("FREQ") {
        // LFU frequency counter - returns 0 since we don't track frequency
        if argv.len() < 3 {
            return Err(CommandError::WrongArity("OBJECT"));
        }
        if store.exists(&argv[2], now_ms) {
            Ok(RespFrame::Integer(0))
        } else {
            Ok(RespFrame::Error("ERR no such key".to_string()))
        }
    } else if sub.eq_ignore_ascii_case("HELP") {
        Ok(RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(
                b"OBJECT <subcommand> [<arg> [value] [opt] ...]. Subcommands are:".to_vec(),
            )),
            RespFrame::BulkString(Some(
                b"ENCODING <key> - Return the encoding of the object at <key>.".to_vec(),
            )),
            RespFrame::BulkString(Some(
                b"FREQ <key> - Return the access frequency of <key>.".to_vec(),
            )),
            RespFrame::BulkString(Some(b"HELP - Return subcommand help summary.".to_vec())),
            RespFrame::BulkString(Some(
                b"IDLETIME <key> - Return the idle time of <key>.".to_vec(),
            )),
            RespFrame::BulkString(Some(
                b"REFCOUNT <key> - Return the reference count of <key>.".to_vec(),
            )),
        ])))
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

fn slowlog_cmd(argv: &[Vec<u8>]) -> Result<RespFrame, CommandError> {
    if argv.len() < 2 {
        return Err(CommandError::WrongArity("SLOWLOG"));
    }
    let sub = std::str::from_utf8(&argv[1]).map_err(|_| CommandError::InvalidUtf8Argument)?;
    if sub.eq_ignore_ascii_case("GET") {
        Ok(RespFrame::Array(Some(Vec::new())))
    } else if sub.eq_ignore_ascii_case("LEN") {
        Ok(RespFrame::Integer(0))
    } else if sub.eq_ignore_ascii_case("RESET") {
        Ok(RespFrame::SimpleString("OK".to_string()))
    } else if sub.eq_ignore_ascii_case("HELP") {
        Ok(RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(
                b"SLOWLOG <subcommand> [<arg> [value] ...]. Subcommands are:".to_vec(),
            )),
            RespFrame::BulkString(Some(
                b"GET [<count>] - Return the slow log entries.".to_vec(),
            )),
            RespFrame::BulkString(Some(
                b"LEN - Return the number of entries in the slow log.".to_vec(),
            )),
            RespFrame::BulkString(Some(b"RESET - Reset the slow log.".to_vec())),
            RespFrame::BulkString(Some(b"HELP - Return subcommand help summary.".to_vec())),
        ])))
    } else {
        Ok(RespFrame::Error(format!(
            "ERR Unknown subcommand or wrong number of arguments for SLOWLOG {sub}"
        )))
    }
}

fn memory_cmd(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() < 2 {
        return Err(CommandError::WrongArity("MEMORY"));
    }
    let sub = std::str::from_utf8(&argv[1]).map_err(|_| CommandError::InvalidUtf8Argument)?;
    if sub.eq_ignore_ascii_case("USAGE") {
        if argv.len() < 3 || argv.len() > 5 {
            return Err(CommandError::WrongArity("MEMORY|USAGE"));
        }
        // Optional SAMPLES count (ignored, we do full scan)
        if argv.len() > 3 {
            let opt =
                std::str::from_utf8(&argv[3]).map_err(|_| CommandError::InvalidUtf8Argument)?;
            if !opt.eq_ignore_ascii_case("SAMPLES") {
                return Ok(RespFrame::Error(format!("ERR unknown option '{opt}'")));
            }
            if argv.len() != 5 {
                return Ok(RespFrame::Error(
                    "ERR value is not an integer or out of range".to_string(),
                ));
            }
            // Parse the samples count to validate it, but we ignore it
            let _samples: i64 = parse_i64_arg(&argv[4])?;
        }
        match store.memory_usage_for_key(&argv[2], now_ms) {
            Some(bytes) => Ok(RespFrame::Integer(bytes as i64)),
            None => Ok(RespFrame::BulkString(None)),
        }
    } else if sub.eq_ignore_ascii_case("DOCTOR") {
        Ok(RespFrame::BulkString(Some(
            b"Sam, I have no memory problems".to_vec(),
        )))
    } else if sub.eq_ignore_ascii_case("MALLOC-STATS") {
        Ok(RespFrame::BulkString(Some(
            b"Memory allocator stats not available".to_vec(),
        )))
    } else if sub.eq_ignore_ascii_case("PURGE") {
        Ok(RespFrame::SimpleString("OK".to_string()))
    } else if sub.eq_ignore_ascii_case("STATS") {
        Ok(RespFrame::BulkString(Some(
            b"peak.allocated:0\r\ntotal.allocated:0\r\nstartup.allocated:0\r\nreplication.backlog:0\r\nclients.slaves:0\r\nclients.normal:0\r\naof.buffer:0\r\n".to_vec(),
        )))
    } else if sub.eq_ignore_ascii_case("HELP") {
        Ok(RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(
                b"MEMORY <subcommand> [<arg> [value] ...]. Subcommands are:".to_vec(),
            )),
            RespFrame::BulkString(Some(
                b"DOCTOR - Return memory problems reports.".to_vec(),
            )),
            RespFrame::BulkString(Some(
                b"MALLOC-STATS - Return internal statistics report from the memory allocator."
                    .to_vec(),
            )),
            RespFrame::BulkString(Some(
                b"PURGE - Ask the allocator to release memory.".to_vec(),
            )),
            RespFrame::BulkString(Some(
                b"STATS - Return information about the memory usage of the server.".to_vec(),
            )),
            RespFrame::BulkString(Some(
                b"USAGE <key> [SAMPLES <count>] - Return memory in bytes used by <key> and its value."
                    .to_vec(),
            )),
            RespFrame::BulkString(Some(b"HELP - Return subcommand help summary.".to_vec())),
        ])))
    } else {
        Ok(RespFrame::Error(format!(
            "ERR unknown subcommand or wrong number of arguments for 'memory|{sub}'"
        )))
    }
}

fn save_cmd(argv: &[Vec<u8>]) -> Result<RespFrame, CommandError> {
    if argv.len() != 1 {
        return Err(CommandError::WrongArity("SAVE"));
    }
    Ok(RespFrame::SimpleString("OK".to_string()))
}

fn bgsave_cmd(argv: &[Vec<u8>]) -> Result<RespFrame, CommandError> {
    if argv.len() > 2 {
        return Err(CommandError::WrongArity("BGSAVE"));
    }
    // Optional SCHEDULE argument — accepted but ignored
    Ok(RespFrame::SimpleString(
        "Background saving started".to_string(),
    ))
}

fn bgrewriteaof_cmd(argv: &[Vec<u8>]) -> Result<RespFrame, CommandError> {
    if argv.len() != 1 {
        return Err(CommandError::WrongArity("BGREWRITEAOF"));
    }
    Ok(RespFrame::SimpleString(
        "Background append only file rewriting started".to_string(),
    ))
}

fn lastsave_cmd(argv: &[Vec<u8>], now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 1 {
        return Err(CommandError::WrongArity("LASTSAVE"));
    }
    // Return current time in seconds as a stub (no real save has occurred)
    Ok(RespFrame::Integer((now_ms / 1000) as i64))
}

fn subscribe_cmd(argv: &[Vec<u8>], store: &mut Store) -> Result<RespFrame, CommandError> {
    // SUBSCRIBE channel [channel ...]
    if argv.len() < 2 {
        return Err(CommandError::WrongArity("SUBSCRIBE"));
    }
    // Subscribe to each channel and return confirmation for the last
    let mut count = 0i64;
    for channel in &argv[1..] {
        count = store.subscribe(channel.clone()) as i64;
    }
    let last_channel = &argv[argv.len() - 1];
    Ok(RespFrame::Array(Some(vec![
        RespFrame::BulkString(Some(b"subscribe".to_vec())),
        RespFrame::BulkString(Some(last_channel.clone())),
        RespFrame::Integer(count),
    ])))
}

fn unsubscribe_cmd(argv: &[Vec<u8>], store: &mut Store) -> Result<RespFrame, CommandError> {
    // UNSUBSCRIBE [channel [channel ...]]
    if argv.len() < 2 {
        // Unsubscribe from all channels
        let channels: Vec<Vec<u8>> = store.subscribed_channels.iter().cloned().collect();
        for ch in &channels {
            store.unsubscribe(ch);
        }
        return Ok(RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"unsubscribe".to_vec())),
            RespFrame::BulkString(None),
            RespFrame::Integer(0),
        ])));
    }
    let mut count = 0i64;
    for channel in &argv[1..] {
        count = store.unsubscribe(channel) as i64;
    }
    Ok(RespFrame::Array(Some(vec![
        RespFrame::BulkString(Some(b"unsubscribe".to_vec())),
        RespFrame::BulkString(Some(argv[argv.len() - 1].clone())),
        RespFrame::Integer(count),
    ])))
}

fn psubscribe_cmd(argv: &[Vec<u8>], store: &mut Store) -> Result<RespFrame, CommandError> {
    if argv.len() < 2 {
        return Err(CommandError::WrongArity("PSUBSCRIBE"));
    }
    let mut count = 0i64;
    for pattern in &argv[1..] {
        count = store.psubscribe(pattern.clone()) as i64;
    }
    let last_pattern = &argv[argv.len() - 1];
    Ok(RespFrame::Array(Some(vec![
        RespFrame::BulkString(Some(b"psubscribe".to_vec())),
        RespFrame::BulkString(Some(last_pattern.clone())),
        RespFrame::Integer(count),
    ])))
}

fn punsubscribe_cmd(argv: &[Vec<u8>], store: &mut Store) -> Result<RespFrame, CommandError> {
    if argv.len() < 2 {
        let patterns: Vec<Vec<u8>> = store.subscribed_patterns.iter().cloned().collect();
        for p in &patterns {
            store.punsubscribe(p);
        }
        return Ok(RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"punsubscribe".to_vec())),
            RespFrame::BulkString(None),
            RespFrame::Integer(0),
        ])));
    }
    let mut count = 0i64;
    for pattern in &argv[1..] {
        count = store.punsubscribe(pattern) as i64;
    }
    Ok(RespFrame::Array(Some(vec![
        RespFrame::BulkString(Some(b"punsubscribe".to_vec())),
        RespFrame::BulkString(Some(argv[argv.len() - 1].clone())),
        RespFrame::Integer(count),
    ])))
}

fn publish_cmd(argv: &[Vec<u8>], store: &mut Store) -> Result<RespFrame, CommandError> {
    // PUBLISH channel message
    if argv.len() != 3 {
        return Err(CommandError::WrongArity("PUBLISH"));
    }
    let receivers = store.publish(&argv[1], &argv[2]);
    Ok(RespFrame::Integer(receivers as i64))
}

fn pubsub_cmd(argv: &[Vec<u8>], store: &mut Store) -> Result<RespFrame, CommandError> {
    if argv.len() < 2 {
        return Err(CommandError::WrongArity("PUBSUB"));
    }
    let sub = std::str::from_utf8(&argv[1]).map_err(|_| CommandError::InvalidUtf8Argument)?;
    if sub.eq_ignore_ascii_case("CHANNELS") {
        let channels = store.pubsub_channels();
        // Optionally filter by pattern
        let filtered: Vec<RespFrame> = if argv.len() >= 3 {
            let pattern = &argv[2];
            channels
                .into_iter()
                .filter(|ch| {
                    // Simple glob match on channel name
                    pattern == b"*"
                        || ch == pattern
                        || String::from_utf8_lossy(pattern)
                            .contains(String::from_utf8_lossy(ch).as_ref())
                })
                .map(|ch| RespFrame::BulkString(Some(ch)))
                .collect()
        } else {
            channels
                .into_iter()
                .map(|ch| RespFrame::BulkString(Some(ch)))
                .collect()
        };
        Ok(RespFrame::Array(Some(filtered)))
    } else if sub.eq_ignore_ascii_case("NUMSUB") {
        let mut result = Vec::new();
        for ch in &argv[2..] {
            result.push(RespFrame::BulkString(Some(ch.clone())));
            result.push(RespFrame::Integer(store.pubsub_numsub_count(ch) as i64));
        }
        Ok(RespFrame::Array(Some(result)))
    } else if sub.eq_ignore_ascii_case("NUMPAT") {
        Ok(RespFrame::Integer(store.pubsub_numpat() as i64))
    } else if sub.eq_ignore_ascii_case("SHARDCHANNELS") {
        // Shard channels not tracked in standalone mode
        Ok(RespFrame::Array(Some(Vec::new())))
    } else if sub.eq_ignore_ascii_case("SHARDNUMSUB") {
        let mut result = Vec::new();
        for ch in &argv[2..] {
            result.push(RespFrame::BulkString(Some(ch.clone())));
            result.push(RespFrame::Integer(0));
        }
        Ok(RespFrame::Array(Some(result)))
    } else if sub.eq_ignore_ascii_case("HELP") {
        Ok(RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(
                b"PUBSUB <subcommand> [<arg> [value] ...]. Subcommands are:".to_vec(),
            )),
            RespFrame::BulkString(Some(
                b"CHANNELS [<pattern>] - Return channels with active subscribers matching pattern."
                    .to_vec(),
            )),
            RespFrame::BulkString(Some(
                b"NUMSUB [<channel> ...] - Return subscriber counts for channels.".to_vec(),
            )),
            RespFrame::BulkString(Some(
                b"NUMPAT - Return number of pattern subscriptions.".to_vec(),
            )),
            RespFrame::BulkString(Some(b"HELP - Return subcommand help summary.".to_vec())),
        ])))
    } else {
        Ok(RespFrame::Error(format!(
            "ERR unknown subcommand or wrong number of arguments for 'pubsub|{sub}'"
        )))
    }
}

fn msetnx(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    // MSETNX key value [key value ...]
    if argv.len() < 3 || argv.len().is_multiple_of(2) {
        return Err(CommandError::WrongArity("MSETNX"));
    }
    // Check if any key already exists (any type, not just string)
    for i in (1..argv.len()).step_by(2) {
        if store.exists(&argv[i], now_ms) {
            return Ok(RespFrame::Integer(0));
        }
    }
    // All keys are new — set them all
    for i in (1..argv.len()).step_by(2) {
        store.set(argv[i].clone(), argv[i + 1].clone(), None, now_ms);
    }
    Ok(RespFrame::Integer(1))
}

fn brpoplpush(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    // BRPOPLPUSH source destination timeout
    if argv.len() != 4 {
        return Err(CommandError::WrongArity("BRPOPLPUSH"));
    }
    let _timeout = parse_f64_arg(&argv[3])?;
    match store.rpoplpush(&argv[1], &argv[2], now_ms) {
        Ok(Some(val)) => Ok(RespFrame::BulkString(Some(val))),
        Ok(None) => Ok(RespFrame::BulkString(None)),
        Err(e) => Err(CommandError::Store(e)),
    }
}

fn zdiff(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    // ZDIFF numkeys key [key ...] [WITHSCORES]
    if argv.len() < 3 {
        return Err(CommandError::WrongArity("ZDIFF"));
    }
    let numkeys = parse_i64_arg(&argv[1])? as usize;
    if numkeys == 0 || argv.len() < 2 + numkeys {
        return Ok(RespFrame::Error("ERR syntax error".to_string()));
    }
    let withscores =
        argv.len() > 2 + numkeys && argv[2 + numkeys].eq_ignore_ascii_case(b"WITHSCORES");
    let keys: Vec<&[u8]> = (0..numkeys).map(|i| argv[2 + i].as_slice()).collect();
    // Compute difference: members in first set not in any other
    let first_members = store.zrange_withscores(keys[0], 0, -1, now_ms)?;
    let mut result: Vec<(Vec<u8>, f64)> = Vec::new();
    for (member, score) in first_members {
        let mut in_other = false;
        for &key in &keys[1..] {
            if store.zscore(key, &member, now_ms)?.is_some() {
                in_other = true;
                break;
            }
        }
        if !in_other {
            result.push((member, score));
        }
    }
    let mut frames = Vec::new();
    for (member, score) in result {
        frames.push(RespFrame::BulkString(Some(member)));
        if withscores {
            frames.push(RespFrame::BulkString(Some(score.to_string().into_bytes())));
        }
    }
    Ok(RespFrame::Array(Some(frames)))
}

fn zdiffstore(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    // ZDIFFSTORE destination numkeys key [key ...]
    if argv.len() < 4 {
        return Err(CommandError::WrongArity("ZDIFFSTORE"));
    }
    let dest = &argv[1];
    let numkeys = parse_i64_arg(&argv[2])? as usize;
    if numkeys == 0 || argv.len() < 3 + numkeys {
        return Ok(RespFrame::Error("ERR syntax error".to_string()));
    }
    let keys: Vec<&[u8]> = (0..numkeys).map(|i| argv[3 + i].as_slice()).collect();
    let first_members = store.zrange_withscores(keys[0], 0, -1, now_ms)?;
    let mut result: std::collections::HashMap<Vec<u8>, f64> = std::collections::HashMap::new();
    for (member, score) in first_members {
        let mut in_other = false;
        for &key in &keys[1..] {
            if store.zscore(key, &member, now_ms)?.is_some() {
                in_other = true;
                break;
            }
        }
        if !in_other {
            result.insert(member, score);
        }
    }
    let count = result.len();
    store.store_sorted_set(dest, result);
    Ok(RespFrame::Integer(count as i64))
}

fn zinter(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    // ZINTER numkeys key [key ...] [WEIGHTS w ...] [AGGREGATE SUM|MIN|MAX] [WITHSCORES]
    if argv.len() < 3 {
        return Err(CommandError::WrongArity("ZINTER"));
    }
    let numkeys = parse_i64_arg(&argv[1])? as usize;
    if numkeys == 0 || argv.len() < 2 + numkeys {
        return Ok(RespFrame::Error("ERR syntax error".to_string()));
    }
    let keys: Vec<&[u8]> = (0..numkeys).map(|i| argv[2 + i].as_slice()).collect();
    // Find WITHSCORES before parse_zstore_args
    let withscores_pos = argv[2 + numkeys..]
        .iter()
        .position(|a| a.eq_ignore_ascii_case(b"WITHSCORES"));
    // Filter out WITHSCORES before passing to parse_zstore_args
    let args_end = if let Some(pos) = withscores_pos {
        let abs = 2 + numkeys + pos;
        // Create temporary argv without WITHSCORES for parsing
        abs
    } else {
        argv.len()
    };
    let filtered: Vec<Vec<u8>> = argv[..args_end].to_vec();
    let (weights, aggregate) = parse_zstore_args(&filtered, 2 + numkeys, numkeys)?;
    let withscores = withscores_pos.is_some();
    let first_members = store.zrange_withscores(keys[0], 0, -1, now_ms)?;
    let mut result: Vec<(Vec<u8>, f64)> = Vec::new();
    let w0 = weights.first().copied().unwrap_or(1.0);
    for (member, score) in first_members {
        let mut combined = score * w0;
        let mut in_all = true;
        for (i, &key) in keys[1..].iter().enumerate() {
            match store.zscore(key, &member, now_ms)? {
                Some(s) => {
                    let w = weights.get(i + 1).copied().unwrap_or(1.0);
                    combined = aggregate_scores_for_cmd(combined, s * w, &aggregate);
                }
                None => {
                    in_all = false;
                    break;
                }
            }
        }
        if in_all {
            result.push((member, combined));
        }
    }
    let mut frames = Vec::new();
    for (member, score) in result {
        frames.push(RespFrame::BulkString(Some(member)));
        if withscores {
            frames.push(RespFrame::BulkString(Some(score.to_string().into_bytes())));
        }
    }
    Ok(RespFrame::Array(Some(frames)))
}

fn zunion_cmd(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    // ZUNION numkeys key [key ...] [WEIGHTS w ...] [AGGREGATE SUM|MIN|MAX] [WITHSCORES]
    if argv.len() < 3 {
        return Err(CommandError::WrongArity("ZUNION"));
    }
    let numkeys = parse_i64_arg(&argv[1])? as usize;
    if numkeys == 0 || argv.len() < 2 + numkeys {
        return Ok(RespFrame::Error("ERR syntax error".to_string()));
    }
    let keys: Vec<&[u8]> = (0..numkeys).map(|i| argv[2 + i].as_slice()).collect();
    let withscores_pos = argv[2 + numkeys..]
        .iter()
        .position(|a| a.eq_ignore_ascii_case(b"WITHSCORES"));
    let args_end = if let Some(pos) = withscores_pos {
        2 + numkeys + pos
    } else {
        argv.len()
    };
    let filtered: Vec<Vec<u8>> = argv[..args_end].to_vec();
    let (weights, aggregate) = parse_zstore_args(&filtered, 2 + numkeys, numkeys)?;
    let withscores = withscores_pos.is_some();
    let mut combined: std::collections::HashMap<Vec<u8>, f64> = std::collections::HashMap::new();
    for (i, &key) in keys.iter().enumerate() {
        let w = weights.get(i).copied().unwrap_or(1.0);
        let members = store.zrange_withscores(key, 0, -1, now_ms)?;
        for (member, score) in members {
            let weighted = score * w;
            use std::collections::hash_map::Entry as HEntry;
            match combined.entry(member) {
                HEntry::Vacant(e) => {
                    e.insert(weighted);
                }
                HEntry::Occupied(mut e) => {
                    let current = e.get_mut();
                    *current = aggregate_scores_for_cmd(*current, weighted, &aggregate);
                }
            }
        }
    }
    let mut entries: Vec<(Vec<u8>, f64)> = combined.into_iter().collect();
    entries.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    let mut frames = Vec::new();
    for (member, score) in entries {
        frames.push(RespFrame::BulkString(Some(member)));
        if withscores {
            frames.push(RespFrame::BulkString(Some(score.to_string().into_bytes())));
        }
    }
    Ok(RespFrame::Array(Some(frames)))
}

fn zintercard(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    // ZINTERCARD numkeys key [key ...] [LIMIT limit]
    if argv.len() < 3 {
        return Err(CommandError::WrongArity("ZINTERCARD"));
    }
    let numkeys = parse_i64_arg(&argv[1])? as usize;
    if numkeys == 0 || argv.len() < 2 + numkeys {
        return Ok(RespFrame::Error("ERR syntax error".to_string()));
    }
    let mut limit: u64 = 0;
    let mut idx = 2 + numkeys;
    while idx < argv.len() {
        let opt = std::str::from_utf8(&argv[idx]).map_err(|_| CommandError::InvalidUtf8Argument)?;
        if opt.eq_ignore_ascii_case("LIMIT") {
            idx += 1;
            if idx >= argv.len() {
                return Ok(RespFrame::Error("ERR syntax error".to_string()));
            }
            limit = parse_u64_arg(&argv[idx])?;
        } else {
            return Ok(RespFrame::Error("ERR syntax error".to_string()));
        }
        idx += 1;
    }
    let keys: Vec<&[u8]> = (0..numkeys).map(|i| argv[2 + i].as_slice()).collect();
    // Compute intersection count
    if keys.is_empty() {
        return Ok(RespFrame::Integer(0));
    }
    let first_members = store.zrange_withscores(keys[0], 0, -1, now_ms)?;
    let mut count: u64 = 0;
    for (member, _) in first_members {
        let mut in_all = true;
        for &key in &keys[1..] {
            if store.zscore(key, &member, now_ms)?.is_none() {
                in_all = false;
                break;
            }
        }
        if in_all {
            count += 1;
            if limit > 0 && count >= limit {
                break;
            }
        }
    }
    Ok(RespFrame::Integer(count as i64))
}

#[allow(clippy::type_complexity)]
fn parse_eval_args(argv: &[Vec<u8>]) -> Result<(usize, &[Vec<u8>], &[Vec<u8>]), CommandError> {
    // Parse numkeys
    let numkeys_str =
        std::str::from_utf8(&argv[2]).map_err(|_| CommandError::InvalidUtf8Argument)?;
    let numkeys: usize = numkeys_str
        .parse()
        .map_err(|_| CommandError::InvalidInteger)?;
    let total_after = argv.len() - 3;
    if numkeys > total_after {
        return Err(CommandError::InvalidInteger);
    }
    let keys = &argv[3..3 + numkeys];
    let args = &argv[3 + numkeys..];
    Ok((numkeys, keys, args))
}

fn eval_cmd(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    // EVAL script numkeys [key ...] [arg ...]
    if argv.len() < 3 {
        return Err(CommandError::WrongArity("EVAL"));
    }
    let script = &argv[1];
    let (_numkeys, keys, args) = parse_eval_args(argv)?;

    // Cache the script (Redis caches on EVAL too)
    store.script_load(script);

    // Execute
    let keys_vec: Vec<Vec<u8>> = keys.to_vec();
    let args_vec: Vec<Vec<u8>> = args.to_vec();
    match lua_eval::eval_script(script, &keys_vec, &args_vec, store, now_ms) {
        Ok(frame) => Ok(frame),
        Err(e) => Ok(RespFrame::Error(format!("ERR Error running script: {e}"))),
    }
}

fn evalsha_cmd(
    argv: &[Vec<u8>],
    store: &mut Store,
    now_ms: u64,
) -> Result<RespFrame, CommandError> {
    // EVALSHA sha1 numkeys [key ...] [arg ...]
    if argv.len() < 3 {
        return Err(CommandError::WrongArity("EVALSHA"));
    }
    let sha1 = &argv[1];
    let (_numkeys, keys, args) = parse_eval_args(argv)?;

    // Look up script by SHA1
    let script = match store.script_get(sha1) {
        Some(s) => s.to_vec(),
        None => {
            return Ok(RespFrame::Error(
                "NOSCRIPT No matching script. Please use EVAL.".to_string(),
            ));
        }
    };

    let keys_vec: Vec<Vec<u8>> = keys.to_vec();
    let args_vec: Vec<Vec<u8>> = args.to_vec();
    match lua_eval::eval_script(&script, &keys_vec, &args_vec, store, now_ms) {
        Ok(frame) => Ok(frame),
        Err(e) => Ok(RespFrame::Error(format!("ERR Error running script: {e}"))),
    }
}

fn script_cmd(argv: &[Vec<u8>], store: &mut Store) -> Result<RespFrame, CommandError> {
    if argv.len() < 2 {
        return Err(CommandError::WrongArity("SCRIPT"));
    }
    let sub = std::str::from_utf8(&argv[1]).map_err(|_| CommandError::InvalidUtf8Argument)?;
    if sub.eq_ignore_ascii_case("LOAD") {
        if argv.len() != 3 {
            return Err(CommandError::WrongArity("SCRIPT|LOAD"));
        }
        let sha1 = store.script_load(&argv[2]);
        Ok(RespFrame::BulkString(Some(sha1.into_bytes())))
    } else if sub.eq_ignore_ascii_case("EXISTS") {
        if argv.len() < 3 {
            return Err(CommandError::WrongArity("SCRIPT|EXISTS"));
        }
        let sha1s: Vec<&[u8]> = argv[2..].iter().map(Vec::as_slice).collect();
        let results: Vec<RespFrame> = store
            .script_exists(&sha1s)
            .into_iter()
            .map(|exists| RespFrame::Integer(i64::from(exists)))
            .collect();
        Ok(RespFrame::Array(Some(results)))
    } else if sub.eq_ignore_ascii_case("FLUSH") {
        store.script_flush();
        Ok(RespFrame::SimpleString("OK".to_string()))
    } else if sub.eq_ignore_ascii_case("KILL") {
        Ok(RespFrame::Error(
            "NOTBUSY No scripts in execution right now.".to_string(),
        ))
    } else if sub.eq_ignore_ascii_case("HELP") {
        Ok(RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(
                b"SCRIPT <subcommand> [<arg> [value] ...]. Subcommands are:".to_vec(),
            )),
            RespFrame::BulkString(Some(
                b"EXISTS <sha1> [<sha1> ...] - Check existence of scripts by SHA1.".to_vec(),
            )),
            RespFrame::BulkString(Some(
                b"FLUSH [ASYNC|SYNC] - Remove all scripts from the cache.".to_vec(),
            )),
            RespFrame::BulkString(Some(
                b"KILL - Kill the currently executing script.".to_vec(),
            )),
            RespFrame::BulkString(Some(
                b"LOAD <script> - Load a script into the cache.".to_vec(),
            )),
            RespFrame::BulkString(Some(b"HELP - Return subcommand help summary.".to_vec())),
        ])))
    } else {
        Ok(RespFrame::Error(format!(
            "ERR unknown subcommand or wrong number of arguments for 'script|{sub}'"
        )))
    }
}

fn debug_cmd(argv: &[Vec<u8>]) -> Result<RespFrame, CommandError> {
    if argv.len() < 2 {
        return Err(CommandError::WrongArity("DEBUG"));
    }
    let sub = std::str::from_utf8(&argv[1]).map_err(|_| CommandError::InvalidUtf8Argument)?;
    if sub.eq_ignore_ascii_case("SLEEP")
        || sub.eq_ignore_ascii_case("SET-ACTIVE-EXPIRE")
        || sub.eq_ignore_ascii_case("JMAP")
        || sub.eq_ignore_ascii_case("RELOAD")
        || sub.eq_ignore_ascii_case("OBJECT")
    {
        Ok(RespFrame::SimpleString("OK".to_string()))
    } else {
        Ok(RespFrame::Error(format!(
            "ERR Unknown subcommand or wrong number of arguments for DEBUG {sub}"
        )))
    }
}

fn role_cmd(argv: &[Vec<u8>]) -> Result<RespFrame, CommandError> {
    if argv.len() != 1 {
        return Err(CommandError::WrongArity("ROLE"));
    }
    // Return master role with empty replica list
    Ok(RespFrame::Array(Some(vec![
        RespFrame::BulkString(Some(b"master".to_vec())),
        RespFrame::Integer(0),
        RespFrame::Array(Some(Vec::new())),
    ])))
}

fn shutdown_cmd(argv: &[Vec<u8>]) -> Result<RespFrame, CommandError> {
    if argv.len() > 2 {
        return Err(CommandError::WrongArity("SHUTDOWN"));
    }
    // Stub — in production this would trigger graceful shutdown
    Ok(RespFrame::SimpleString("OK".to_string()))
}

fn move_cmd(argv: &[Vec<u8>]) -> Result<RespFrame, CommandError> {
    // MOVE key db
    if argv.len() != 3 {
        return Err(CommandError::WrongArity("MOVE"));
    }
    let db = parse_i64_arg(&argv[2])?;
    if db == 0 {
        // Same DB — no-op, return 0 (key already in this DB)
        Ok(RespFrame::Integer(0))
    } else {
        Ok(RespFrame::Error("ERR index out of range".to_string()))
    }
}

fn latency_cmd(argv: &[Vec<u8>]) -> Result<RespFrame, CommandError> {
    if argv.len() < 2 {
        return Err(CommandError::WrongArity("LATENCY"));
    }
    let sub = std::str::from_utf8(&argv[1]).map_err(|_| CommandError::InvalidUtf8Argument)?;
    if sub.eq_ignore_ascii_case("LATEST") || sub.eq_ignore_ascii_case("HISTORY") {
        Ok(RespFrame::Array(Some(Vec::new())))
    } else if sub.eq_ignore_ascii_case("RESET") {
        Ok(RespFrame::SimpleString("OK".to_string()))
    } else if sub.eq_ignore_ascii_case("GRAPH") {
        Ok(RespFrame::BulkString(None))
    } else if sub.eq_ignore_ascii_case("HELP") {
        Ok(RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(
                b"LATENCY <subcommand> [<arg> ...]. Subcommands are:".to_vec(),
            )),
            RespFrame::BulkString(Some(
                b"LATEST - Return the latest latency samples.".to_vec(),
            )),
            RespFrame::BulkString(Some(
                b"HISTORY <event> - Return latency history for an event.".to_vec(),
            )),
            RespFrame::BulkString(Some(b"RESET [<event> ...] - Reset latency data.".to_vec())),
            RespFrame::BulkString(Some(
                b"GRAPH <event> - Return an ASCII graph of latency.".to_vec(),
            )),
            RespFrame::BulkString(Some(b"HELP - Return subcommand help summary.".to_vec())),
        ])))
    } else {
        Ok(RespFrame::Error(format!(
            "ERR unknown subcommand or wrong number of arguments for 'latency|{sub}'"
        )))
    }
}

fn bitfield_cmd(
    argv: &[Vec<u8>],
    store: &mut Store,
    now_ms: u64,
) -> Result<RespFrame, CommandError> {
    // BITFIELD key [GET encoding offset] [SET encoding offset value]
    //              [INCRBY encoding offset increment] [OVERFLOW WRAP|SAT|FAIL]
    if argv.len() < 2 {
        return Err(CommandError::WrongArity("BITFIELD"));
    }
    let key = &argv[1];
    let mut results: Vec<RespFrame> = Vec::new();
    let mut overflow_mode = BitfieldOverflow::Wrap;
    let mut i = 2;

    while i < argv.len() {
        let sub = std::str::from_utf8(&argv[i]).map_err(|_| CommandError::InvalidUtf8Argument)?;
        if sub.eq_ignore_ascii_case("GET") {
            if i + 2 >= argv.len() {
                return Ok(RespFrame::Error("ERR syntax error".to_string()));
            }
            let (signed, bits) = match bitfield_parse_encoding(&argv[i + 1]) {
                Some(v) => v,
                None => {
                    return Ok(RespFrame::Error(
                        "ERR Invalid bitfield type. Use something like i8 u8 i16 u16 i32 u32 i64 u63"
                            .to_string(),
                    ));
                }
            };
            let bit_offset = match bitfield_parse_offset(&argv[i + 2], bits) {
                Some(v) => v,
                None => {
                    return Ok(RespFrame::Error(
                        "ERR bit offset is not an integer or out of range".to_string(),
                    ));
                }
            };
            let val = store
                .bitfield_get(key, bit_offset, bits, signed, now_ms)
                .map_err(CommandError::Store)?;
            results.push(RespFrame::Integer(val));
            i += 3;
        } else if sub.eq_ignore_ascii_case("SET") {
            if i + 3 >= argv.len() {
                return Ok(RespFrame::Error("ERR syntax error".to_string()));
            }
            let (signed, bits) = match bitfield_parse_encoding(&argv[i + 1]) {
                Some(v) => v,
                None => {
                    return Ok(RespFrame::Error(
                        "ERR Invalid bitfield type. Use something like i8 u8 i16 u16 i32 u32 i64 u63"
                            .to_string(),
                    ));
                }
            };
            let bit_offset = match bitfield_parse_offset(&argv[i + 2], bits) {
                Some(v) => v,
                None => {
                    return Ok(RespFrame::Error(
                        "ERR bit offset is not an integer or out of range".to_string(),
                    ));
                }
            };
            let value = parse_i64_arg(&argv[i + 3])?;
            let clamped = bitfield_clamp(value, bits, signed, overflow_mode);
            // For SET, overflow FAIL still writes (SET always succeeds per Redis behavior),
            // but we truncate/wrap as needed
            let old = store
                .bitfield_set(key, bit_offset, bits, clamped, now_ms)
                .map_err(CommandError::Store)?;
            // Return old value, sign-extended if signed
            let old_result = if signed {
                bitfield_sign_extend(old, bits)
            } else {
                old
            };
            results.push(RespFrame::Integer(old_result));
            i += 4;
        } else if sub.eq_ignore_ascii_case("INCRBY") {
            if i + 3 >= argv.len() {
                return Ok(RespFrame::Error("ERR syntax error".to_string()));
            }
            let (signed, bits) = match bitfield_parse_encoding(&argv[i + 1]) {
                Some(v) => v,
                None => {
                    return Ok(RespFrame::Error(
                        "ERR Invalid bitfield type. Use something like i8 u8 i16 u16 i32 u32 i64 u63"
                            .to_string(),
                    ));
                }
            };
            let bit_offset = match bitfield_parse_offset(&argv[i + 2], bits) {
                Some(v) => v,
                None => {
                    return Ok(RespFrame::Error(
                        "ERR bit offset is not an integer or out of range".to_string(),
                    ));
                }
            };
            let increment = parse_i64_arg(&argv[i + 3])?;

            // Read current value
            let current = store
                .bitfield_get(key, bit_offset, bits, signed, now_ms)
                .map_err(CommandError::Store)?;

            let new_val = current.wrapping_add(increment);
            let (clamped, overflowed) =
                bitfield_clamp_with_overflow(new_val, bits, signed, overflow_mode);

            if overflowed && overflow_mode == BitfieldOverflow::Fail {
                results.push(RespFrame::BulkString(None));
            } else {
                store
                    .bitfield_set(key, bit_offset, bits, clamped, now_ms)
                    .map_err(CommandError::Store)?;
                results.push(RespFrame::Integer(clamped));
            }
            i += 4;
        } else if sub.eq_ignore_ascii_case("OVERFLOW") {
            if i + 1 >= argv.len() {
                return Ok(RespFrame::Error("ERR syntax error".to_string()));
            }
            let mode =
                std::str::from_utf8(&argv[i + 1]).map_err(|_| CommandError::InvalidUtf8Argument)?;
            if mode.eq_ignore_ascii_case("WRAP") {
                overflow_mode = BitfieldOverflow::Wrap;
            } else if mode.eq_ignore_ascii_case("SAT") {
                overflow_mode = BitfieldOverflow::Sat;
            } else if mode.eq_ignore_ascii_case("FAIL") {
                overflow_mode = BitfieldOverflow::Fail;
            } else {
                return Ok(RespFrame::Error(
                    "ERR Invalid OVERFLOW type (should be one of WRAP, SAT, FAIL)".to_string(),
                ));
            }
            i += 2;
        } else {
            return Ok(RespFrame::Error("ERR syntax error".to_string()));
        }
    }
    Ok(RespFrame::Array(Some(results)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BitfieldOverflow {
    Wrap,
    Sat,
    Fail,
}

/// Parse a BITFIELD type encoding like "i8", "u16", "i64", "u63".
/// Returns (signed, bits). Max 64 for signed, 63 for unsigned.
fn bitfield_parse_encoding(arg: &[u8]) -> Option<(bool, u8)> {
    let s = std::str::from_utf8(arg).ok()?;
    if s.is_empty() {
        return None;
    }
    let (signed, rest) = if s.starts_with('i') || s.starts_with('I') {
        (true, &s[1..])
    } else if s.starts_with('u') || s.starts_with('U') {
        (false, &s[1..])
    } else {
        return None;
    };
    let bits: u8 = rest.parse().ok()?;
    if bits == 0 {
        return None;
    }
    if signed && bits > 64 {
        return None;
    }
    if !signed && bits > 63 {
        return None;
    }
    Some((signed, bits))
}

/// Parse a BITFIELD offset like "100" (plain bit offset) or "#5" (type-aligned offset).
fn bitfield_parse_offset(arg: &[u8], bits: u8) -> Option<u64> {
    let s = std::str::from_utf8(arg).ok()?;
    if let Some(rest) = s.strip_prefix('#') {
        let n: u64 = rest.parse().ok()?;
        Some(n.checked_mul(u64::from(bits))?)
    } else {
        s.parse::<u64>().ok()
    }
}

/// Sign-extend a value from `bits` width to i64.
fn bitfield_sign_extend(value: i64, bits: u8) -> i64 {
    if bits >= 64 {
        return value;
    }
    let mask = 1i64 << (bits - 1);
    if value & mask != 0 {
        value | (i64::MAX << bits << 1) // sign extend
    } else {
        value & ((1i64 << bits) - 1) // mask to width
    }
}

/// Clamp a value to the range of the specified type encoding.
fn bitfield_clamp(value: i64, bits: u8, signed: bool, overflow: BitfieldOverflow) -> i64 {
    bitfield_clamp_with_overflow(value, bits, signed, overflow).0
}

/// Clamp a value and report whether overflow occurred.
fn bitfield_clamp_with_overflow(
    value: i64,
    bits: u8,
    signed: bool,
    overflow: BitfieldOverflow,
) -> (i64, bool) {
    if signed {
        let min = if bits >= 64 {
            i64::MIN
        } else {
            -(1i64 << (bits - 1))
        };
        let max = if bits >= 64 {
            i64::MAX
        } else {
            (1i64 << (bits - 1)) - 1
        };
        if value >= min && value <= max {
            return (value, false);
        }
        match overflow {
            BitfieldOverflow::Wrap => {
                if bits >= 64 {
                    (value, true)
                } else {
                    let range = 1i64 << bits;
                    let mut wrapped = value % range;
                    if wrapped > max {
                        wrapped -= range;
                    } else if wrapped < min {
                        wrapped += range;
                    }
                    (wrapped, true)
                }
            }
            BitfieldOverflow::Sat => {
                let clamped = value.clamp(min, max);
                (clamped, true)
            }
            BitfieldOverflow::Fail => (value, true),
        }
    } else {
        let max = if bits >= 64 {
            u64::MAX
        } else {
            (1u64 << bits) - 1
        };
        let uval = value as u64;
        // Check if value fits in the unsigned range
        if value >= 0 && uval <= max {
            return (value, false);
        }
        match overflow {
            BitfieldOverflow::Wrap => {
                let wrapped = uval & max;
                (wrapped as i64, true)
            }
            BitfieldOverflow::Sat => {
                if value < 0 {
                    (0, true)
                } else {
                    (max as i64, true)
                }
            }
            BitfieldOverflow::Fail => (value, true),
        }
    }
}

fn aggregate_scores_for_cmd(a: f64, b: f64, aggregate: &[u8]) -> f64 {
    if aggregate.eq_ignore_ascii_case(b"MIN") {
        a.min(b)
    } else if aggregate.eq_ignore_ascii_case(b"MAX") {
        a.max(b)
    } else {
        a + b
    }
}

fn swapdb_cmd(argv: &[Vec<u8>]) -> Result<RespFrame, CommandError> {
    if argv.len() != 3 {
        return Err(CommandError::WrongArity("SWAPDB"));
    }
    let db1 = parse_i64_arg(&argv[1])?;
    let db2 = parse_i64_arg(&argv[2])?;
    if db1 == 0 && db2 == 0 {
        Ok(RespFrame::SimpleString("OK".to_string()))
    } else {
        Ok(RespFrame::Error("ERR invalid DB index".to_string()))
    }
}

fn blpop(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    // BLPOP key [key ...] timeout
    if argv.len() < 3 {
        return Err(CommandError::WrongArity("BLPOP"));
    }
    // Last arg is timeout (ignored — we try once immediately)
    let _timeout = parse_f64_arg(&argv[argv.len() - 1])?;
    for key in &argv[1..argv.len() - 1] {
        match store.lpop(key, now_ms) {
            Ok(Some(val)) => {
                return Ok(RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(key.clone())),
                    RespFrame::BulkString(Some(val)),
                ])));
            }
            Ok(None) => continue,
            Err(e) => return Err(CommandError::Store(e)),
        }
    }
    // No data available — return nil (timeout expired)
    Ok(RespFrame::Array(None))
}

fn brpop(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    // BRPOP key [key ...] timeout
    if argv.len() < 3 {
        return Err(CommandError::WrongArity("BRPOP"));
    }
    let _timeout = parse_f64_arg(&argv[argv.len() - 1])?;
    for key in &argv[1..argv.len() - 1] {
        match store.rpop(key, now_ms) {
            Ok(Some(val)) => {
                return Ok(RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(key.clone())),
                    RespFrame::BulkString(Some(val)),
                ])));
            }
            Ok(None) => continue,
            Err(e) => return Err(CommandError::Store(e)),
        }
    }
    Ok(RespFrame::Array(None))
}

fn blmove(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    // BLMOVE source destination LEFT|RIGHT LEFT|RIGHT timeout
    if argv.len() != 6 {
        return Err(CommandError::WrongArity("BLMOVE"));
    }
    let _timeout = parse_f64_arg(&argv[5])?;
    if !argv[3].eq_ignore_ascii_case(b"LEFT") && !argv[3].eq_ignore_ascii_case(b"RIGHT") {
        return Ok(RespFrame::Error("ERR syntax error".to_string()));
    }
    if !argv[4].eq_ignore_ascii_case(b"LEFT") && !argv[4].eq_ignore_ascii_case(b"RIGHT") {
        return Ok(RespFrame::Error("ERR syntax error".to_string()));
    }
    match store.lmove(&argv[1], &argv[2], &argv[3], &argv[4], now_ms) {
        Ok(Some(val)) => Ok(RespFrame::BulkString(Some(val))),
        Ok(None) => Ok(RespFrame::BulkString(None)),
        Err(e) => Err(CommandError::Store(e)),
    }
}

fn blmpop(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    // BLMPOP timeout numkeys key [key ...] LEFT|RIGHT [COUNT count]
    if argv.len() < 4 {
        return Err(CommandError::WrongArity("BLMPOP"));
    }
    let _timeout = parse_f64_arg(&argv[1])?;
    let numkeys = parse_i64_arg(&argv[2])? as usize;
    if numkeys == 0 || argv.len() < 3 + numkeys + 1 {
        return Ok(RespFrame::Error("ERR syntax error".to_string()));
    }
    let direction_idx = 3 + numkeys;
    let direction =
        std::str::from_utf8(&argv[direction_idx]).map_err(|_| CommandError::InvalidUtf8Argument)?;
    let is_left = if direction.eq_ignore_ascii_case("LEFT") {
        true
    } else if direction.eq_ignore_ascii_case("RIGHT") {
        false
    } else {
        return Ok(RespFrame::Error("ERR syntax error".to_string()));
    };
    let mut count: usize = 1;
    let mut idx = direction_idx + 1;
    while idx < argv.len() {
        let opt = std::str::from_utf8(&argv[idx]).map_err(|_| CommandError::InvalidUtf8Argument)?;
        if opt.eq_ignore_ascii_case("COUNT") {
            idx += 1;
            if idx >= argv.len() {
                return Ok(RespFrame::Error("ERR syntax error".to_string()));
            }
            count = parse_i64_arg(&argv[idx])? as usize;
            if count == 0 {
                return Ok(RespFrame::Error(
                    "ERR COUNT value of 0 is not allowed".to_string(),
                ));
            }
        } else {
            return Ok(RespFrame::Error("ERR syntax error".to_string()));
        }
        idx += 1;
    }
    for ki in 0..numkeys {
        let key = &argv[3 + ki];
        let llen = match store.llen(key, now_ms) {
            Ok(n) => n,
            Err(_) => continue,
        };
        if llen == 0 {
            continue;
        }
        let pop_count = count.min(llen);
        let mut elements = Vec::with_capacity(pop_count);
        for _ in 0..pop_count {
            let val = if is_left {
                store.lpop(key, now_ms)
            } else {
                store.rpop(key, now_ms)
            };
            match val {
                Ok(Some(v)) => elements.push(RespFrame::BulkString(Some(v))),
                _ => break,
            }
        }
        if !elements.is_empty() {
            return Ok(RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(key.clone())),
                RespFrame::Array(Some(elements)),
            ])));
        }
    }
    Ok(RespFrame::Array(None))
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

fn dump_cmd(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() != 2 {
        return Err(CommandError::WrongArity("DUMP"));
    }
    match store.dump_key(&argv[1], now_ms) {
        Some(payload) => Ok(RespFrame::BulkString(Some(payload))),
        None => Ok(RespFrame::BulkString(None)),
    }
}

fn restore_cmd(
    argv: &[Vec<u8>],
    store: &mut Store,
    now_ms: u64,
) -> Result<RespFrame, CommandError> {
    if argv.len() < 4 {
        return Err(CommandError::WrongArity("RESTORE"));
    }
    let key = &argv[1];
    let ttl_ms: u64 = parse_i64_arg(&argv[2]).map(|v| v.max(0) as u64)?;
    let payload = &argv[3];
    // Check for REPLACE flag
    let replace = argv[4..]
        .iter()
        .any(|a| std::str::from_utf8(a).is_ok_and(|s| s.eq_ignore_ascii_case("REPLACE")));
    match store.restore_key(key, ttl_ms, payload, replace, now_ms) {
        Ok(()) => Ok(RespFrame::SimpleString("OK".to_string())),
        Err(StoreError::BusyKey) => Ok(RespFrame::Error(
            "BUSYKEY Target key name already exists.".to_string(),
        )),
        Err(StoreError::InvalidDumpPayload) => Ok(RespFrame::Error(
            "ERR DUMP payload version or checksum are wrong".to_string(),
        )),
        Err(e) => Err(CommandError::Store(e)),
    }
}

fn sort_cmd(argv: &[Vec<u8>], store: &mut Store, now_ms: u64) -> Result<RespFrame, CommandError> {
    if argv.len() < 2 {
        return Err(CommandError::WrongArity("SORT"));
    }
    let key = &argv[1];

    // ── Parse options ────────────────────────────────────────────────
    let mut by_pattern: Option<Vec<u8>> = None;
    let mut get_patterns: Vec<Vec<u8>> = Vec::new();
    let mut desc = false;
    let mut alpha = false;
    let mut limit_offset: i64 = 0;
    let mut limit_count: i64 = -1;
    let mut store_dest: Option<Vec<u8>> = None;

    let mut i = 2;
    while i < argv.len() {
        let arg = std::str::from_utf8(&argv[i]).map_err(|_| CommandError::InvalidUtf8Argument)?;
        if arg.eq_ignore_ascii_case("BY") {
            if i + 1 >= argv.len() {
                return Ok(RespFrame::Error("ERR syntax error".to_string()));
            }
            i += 1;
            by_pattern = Some(argv[i].clone());
        } else if arg.eq_ignore_ascii_case("GET") {
            if i + 1 >= argv.len() {
                return Ok(RespFrame::Error("ERR syntax error".to_string()));
            }
            i += 1;
            get_patterns.push(argv[i].clone());
        } else if arg.eq_ignore_ascii_case("LIMIT") {
            if i + 2 >= argv.len() {
                return Ok(RespFrame::Error("ERR syntax error".to_string()));
            }
            i += 1;
            limit_offset = parse_i64_arg(&argv[i])?;
            i += 1;
            limit_count = parse_i64_arg(&argv[i])?;
        } else if arg.eq_ignore_ascii_case("ASC") {
            desc = false;
        } else if arg.eq_ignore_ascii_case("DESC") {
            desc = true;
        } else if arg.eq_ignore_ascii_case("ALPHA") {
            alpha = true;
        } else if arg.eq_ignore_ascii_case("STORE") {
            if i + 1 >= argv.len() {
                return Ok(RespFrame::Error("ERR syntax error".to_string()));
            }
            i += 1;
            store_dest = Some(argv[i].clone());
        } else {
            return Ok(RespFrame::Error("ERR syntax error".to_string()));
        }
        i += 1;
    }

    // ── Get elements from the source key ─────────────────────────────
    let mut elements = store
        .sort_elements(key, now_ms)
        .map_err(CommandError::Store)?;

    if elements.is_empty() {
        if let Some(dest) = store_dest {
            store.del(&[dest], now_ms);
            return Ok(RespFrame::Integer(0));
        }
        return Ok(RespFrame::Array(Some(Vec::new())));
    }

    // ── Determine if sorting should be skipped (BY with constant pattern) ──
    let dontsort = by_pattern.as_ref().is_some_and(|p| !p.contains(&b'*'));

    // ── Sort the elements ────────────────────────────────────────────
    if dontsort {
        // BY nosort: preserve natural order (or force alpha-sort for sets with STORE)
        let is_set = store
            .value_type(key, now_ms)
            .is_some_and(|vt| vt == ValueType::Set);
        if is_set && store_dest.is_some() {
            // Force deterministic ordering for sets with STORE
            elements.sort();
        }
        if desc {
            elements.reverse();
        }
    } else {
        // Build sort keys for each element
        let sort_keys: Vec<Option<Vec<u8>>> = elements
            .iter()
            .map(|el| sort_lookup_by_pattern(store, &by_pattern, el, now_ms))
            .collect();

        if !alpha {
            // Numeric sort: parse sort keys as f64
            let mut scored: Vec<(f64, usize)> = Vec::with_capacity(elements.len());
            for (idx, sk) in sort_keys.iter().enumerate() {
                let val = sk.as_deref().unwrap_or(b"0");
                let s = std::str::from_utf8(val).unwrap_or("0");
                let Ok(score) = s.parse::<f64>() else {
                    return Ok(RespFrame::Error(
                        "ERR One or more scores can't be converted into double".to_string(),
                    ));
                };
                scored.push((score, idx));
            }
            scored.sort_by(|a, b| {
                let cmp = a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal);
                if cmp == std::cmp::Ordering::Equal {
                    // Lexicographic tiebreaker for stability
                    elements[a.1].cmp(&elements[b.1])
                } else if desc {
                    cmp.reverse()
                } else {
                    cmp
                }
            });
            let reordered: Vec<Vec<u8>> = scored
                .iter()
                .map(|(_, idx)| elements[*idx].clone())
                .collect();
            elements = reordered;
        } else {
            // Alpha (lexicographic) sort
            let mut indexed: Vec<(usize, &[u8])> = sort_keys
                .iter()
                .enumerate()
                .map(|(idx, sk)| {
                    let val: &[u8] = sk.as_deref().unwrap_or(b"");
                    (idx, val)
                })
                .collect();
            indexed.sort_by(|a, b| {
                // NULL (empty) sorts before non-empty
                let cmp = a.1.cmp(b.1);
                if desc { cmp.reverse() } else { cmp }
            });
            let reordered: Vec<Vec<u8>> = indexed
                .iter()
                .map(|(idx, _)| elements[*idx].clone())
                .collect();
            elements = reordered;
        }
    }

    // ── Apply LIMIT ──────────────────────────────────────────────────
    let total = elements.len();
    let start = (limit_offset.max(0) as usize).min(total);
    let count = if limit_count < 0 {
        total.saturating_sub(start)
    } else {
        (limit_count as usize).min(total.saturating_sub(start))
    };
    let sliced: Vec<Vec<u8>> = elements.into_iter().skip(start).take(count).collect();

    // ── Build output with GET patterns ───────────────────────────────
    let use_store = store_dest.is_some();
    let output: Vec<RespFrame> = if get_patterns.is_empty() {
        sliced
            .iter()
            .map(|el| RespFrame::BulkString(Some(el.clone())))
            .collect()
    } else {
        let mut out = Vec::with_capacity(sliced.len() * get_patterns.len());
        for el in &sliced {
            for pat in &get_patterns {
                let val = sort_lookup_get_pattern(store, pat, el, now_ms);
                match val {
                    Some(v) => out.push(RespFrame::BulkString(Some(v))),
                    None => {
                        if use_store {
                            out.push(RespFrame::BulkString(Some(Vec::new())));
                        } else {
                            out.push(RespFrame::BulkString(None));
                        }
                    }
                }
            }
        }
        out
    };

    // ── STORE or return ──────────────────────────────────────────────
    if let Some(dest) = store_dest {
        let result_count = output.len() as i64;
        if output.is_empty() {
            store.del(&[dest], now_ms);
        } else {
            let list_elements: Vec<Vec<u8>> = output
                .into_iter()
                .map(|f| match f {
                    RespFrame::BulkString(Some(v)) => v,
                    _ => Vec::new(),
                })
                .collect();
            store.store_as_list(dest, list_elements);
        }
        Ok(RespFrame::Integer(result_count))
    } else {
        Ok(RespFrame::Array(Some(output)))
    }
}

/// Look up a sort key using a BY pattern for the SORT command.
/// Substitutes `*` with the element value. Supports `->` for hash field dereference.
/// Returns None if the key doesn't exist or is the wrong type.
fn sort_lookup_by_pattern(
    store: &mut Store,
    by_pattern: &Option<Vec<u8>>,
    element: &[u8],
    now_ms: u64,
) -> Option<Vec<u8>> {
    let pattern = match by_pattern {
        Some(p) => p,
        None => return Some(element.to_vec()),
    };

    // Substitute * with element value
    let star_pos = pattern.iter().position(|&b| b == b'*');
    let lookup_key = match star_pos {
        Some(pos) => {
            let mut k = Vec::with_capacity(pattern.len() + element.len());
            k.extend_from_slice(&pattern[..pos]);
            k.extend_from_slice(element);
            k.extend_from_slice(&pattern[pos + 1..]);
            k
        }
        None => return Some(element.to_vec()),
    };

    // Check for hash field dereference (->)
    sort_resolve_key_or_hash(store, &lookup_key, now_ms)
}

/// Look up a value using a GET pattern for the SORT command.
/// `GET #` returns the element itself.
fn sort_lookup_get_pattern(
    store: &mut Store,
    pattern: &[u8],
    element: &[u8],
    now_ms: u64,
) -> Option<Vec<u8>> {
    // GET # returns the element itself
    if pattern == b"#" {
        return Some(element.to_vec());
    }

    // Substitute * with element value
    let star_pos = pattern.iter().position(|&b| b == b'*');
    let lookup_key = match star_pos {
        Some(pos) => {
            let mut k = Vec::with_capacity(pattern.len() + element.len());
            k.extend_from_slice(&pattern[..pos]);
            k.extend_from_slice(element);
            k.extend_from_slice(&pattern[pos + 1..]);
            k
        }
        None => return None, // GET without * always returns nil
    };

    sort_resolve_key_or_hash(store, &lookup_key, now_ms)
}

/// Resolve a key that may contain `->` for hash field dereference.
/// Without `->`, looks up as a string key.
/// With `->`, looks up hash key and field.
fn sort_resolve_key_or_hash(store: &mut Store, key: &[u8], now_ms: u64) -> Option<Vec<u8>> {
    // Check for hash field dereference: key->field
    if let Some(arrow_pos) = key.windows(2).position(|w| w == b"->") {
        let hash_key = &key[..arrow_pos];
        let field = &key[arrow_pos + 2..];
        // Attempt hash lookup; silently return None on wrong type or missing
        store.hget(hash_key, field, now_ms).ok().flatten()
    } else {
        // String key lookup; silently return None on wrong type or missing
        store.get(key, now_ms).ok().flatten()
    }
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
        } else if arg.eq_ignore_ascii_case("DB") {
            // COPY source destination [DB destination-db] — ignore DB (single-db mode)
            i += 1;
        } else {
            return Err(CommandError::SyntaxError);
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
    use fr_store::{Store, StoreError};

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
        if eq_ascii_command(cmd, b"XREADGROUP") {
            return Some(CommandId::Xreadgroup);
        }
        if eq_ascii_command(cmd, b"XCLAIM") {
            return Some(CommandId::Xclaim);
        }
        if eq_ascii_command(cmd, b"XAUTOCLAIM") {
            return Some(CommandId::Xautoclaim);
        }
        if eq_ascii_command(cmd, b"XPENDING") {
            return Some(CommandId::Xpending);
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
        if eq_ascii_command(cmd, b"XACK") {
            return Some(CommandId::Xack);
        }
        if eq_ascii_command(cmd, b"XSETID") {
            return Some(CommandId::Xsetid);
        }
        if eq_ascii_command(cmd, b"LOLWUT") {
            return Some(CommandId::Lolwut);
        }
        if eq_ascii_command(cmd, b"WAITAOF") {
            return Some(CommandId::Waitaof);
        }
        if eq_ascii_command(cmd, b"CLUSTER") {
            return Some(CommandId::Cluster);
        }
        if eq_ascii_command(cmd, b"REPLICAOF") {
            return Some(CommandId::Replicaof);
        }
        if eq_ascii_command(cmd, b"SLAVEOF") {
            return Some(CommandId::Replicaof);
        }
        if eq_ascii_command(cmd, b"FUNCTION") {
            return Some(CommandId::Function);
        }
        if eq_ascii_command(cmd, b"SSUBSCRIBE") {
            return Some(CommandId::Ssubscribe);
        }
        if eq_ascii_command(cmd, b"SUNSUBSCRIBE") {
            return Some(CommandId::Sunsubscribe);
        }
        if eq_ascii_command(cmd, b"SPUBLISH") {
            return Some(CommandId::Spublish);
        }
        if eq_ascii_command(cmd, b"SORT_RO") {
            return Some(CommandId::SortRo);
        }
        if eq_ascii_command(cmd, b"READONLY") {
            return Some(CommandId::Readonly);
        }
        if eq_ascii_command(cmd, b"READWRITE") {
            return Some(CommandId::Readwrite);
        }
        if eq_ascii_command(cmd, b"ZRANGESTORE") {
            return Some(CommandId::Zrangestore);
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
            b"XREADGROUP",
            b"xreadgroup",
            b"XCLAIM",
            b"xclaim",
            b"XAUTOCLAIM",
            b"xautoclaim",
            b"XPENDING",
            b"xpending",
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
        store.set(b"k".to_vec(), b"v".to_vec(), Some(5000), 1000);
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

        // expires_at_ms = 1000 + 2500 = 3500; EXPIRETIME truncates to seconds: 3500/1000 = 3
        let expiretime = dispatch_argv(&[b"EXPIRETIME".to_vec(), b"k".to_vec()], &mut store, 1_000)
            .expect("expiretime");
        assert_eq!(expiretime, RespFrame::Integer(3));

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
    fn xreadgroup_reads_new_entries_and_advances_group_cursor() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"XADD".to_vec(),
                b"s".to_vec(),
                b"2000-0".to_vec(),
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
                b"2000-1".to_vec(),
                b"field2".to_vec(),
                b"value2".to_vec(),
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

        let first = dispatch_argv(
            &[
                b"XREADGROUP".to_vec(),
                b"GROUP".to_vec(),
                b"g1".to_vec(),
                b"c1".to_vec(),
                b"STREAMS".to_vec(),
                b"s".to_vec(),
                b">".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xreadgroup first");
        assert_eq!(
            first,
            RespFrame::Array(Some(vec![RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"s".to_vec())),
                RespFrame::Array(Some(vec![
                    RespFrame::Array(Some(vec![
                        RespFrame::BulkString(Some(b"2000-0".to_vec())),
                        RespFrame::Array(Some(vec![
                            RespFrame::BulkString(Some(b"field1".to_vec())),
                            RespFrame::BulkString(Some(b"value1".to_vec())),
                        ])),
                    ])),
                    RespFrame::Array(Some(vec![
                        RespFrame::BulkString(Some(b"2000-1".to_vec())),
                        RespFrame::Array(Some(vec![
                            RespFrame::BulkString(Some(b"field2".to_vec())),
                            RespFrame::BulkString(Some(b"value2".to_vec())),
                        ])),
                    ])),
                ])),
            ]))]))
        );

        let second = dispatch_argv(
            &[
                b"XREADGROUP".to_vec(),
                b"GROUP".to_vec(),
                b"g1".to_vec(),
                b"c1".to_vec(),
                b"STREAMS".to_vec(),
                b"s".to_vec(),
                b">".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xreadgroup no new entries");
        assert_eq!(second, RespFrame::Array(None));

        dispatch_argv(
            &[
                b"XADD".to_vec(),
                b"s".to_vec(),
                b"2000-2".to_vec(),
                b"field3".to_vec(),
                b"value3".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xadd 3");

        let third = dispatch_argv(
            &[
                b"XREADGROUP".to_vec(),
                b"GROUP".to_vec(),
                b"g1".to_vec(),
                b"c1".to_vec(),
                b"COUNT".to_vec(),
                b"1".to_vec(),
                b"STREAMS".to_vec(),
                b"s".to_vec(),
                b">".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xreadgroup count");
        assert_eq!(
            third,
            RespFrame::Array(Some(vec![RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"s".to_vec())),
                RespFrame::Array(Some(vec![RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"2000-2".to_vec())),
                    RespFrame::Array(Some(vec![
                        RespFrame::BulkString(Some(b"field3".to_vec())),
                        RespFrame::BulkString(Some(b"value3".to_vec())),
                    ])),
                ]))])),
            ]))]))
        );

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
                RespFrame::Integer(1),
                RespFrame::BulkString(Some(b"pending".to_vec())),
                RespFrame::Integer(3),
                RespFrame::BulkString(Some(b"last-delivered-id".to_vec())),
                RespFrame::BulkString(Some(b"2000-2".to_vec())),
                RespFrame::BulkString(Some(b"entries-read".to_vec())),
                RespFrame::BulkString(None),
                RespFrame::BulkString(Some(b"lag".to_vec())),
                RespFrame::BulkString(None),
            ]))]))
        );
    }

    #[test]
    fn xreadgroup_validation_nogroup_and_wrongtype() {
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

        let nogroup = dispatch_argv(
            &[
                b"XREADGROUP".to_vec(),
                b"GROUP".to_vec(),
                b"g-missing".to_vec(),
                b"c1".to_vec(),
                b"STREAMS".to_vec(),
                b"s".to_vec(),
                b">".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xreadgroup missing group");
        assert_eq!(
            nogroup,
            RespFrame::Error(
                "NOGROUP No such key 's' or consumer group 'g-missing' in XREADGROUP with GROUP option"
                    .to_string()
            )
        );

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

        let invalid_id = dispatch_argv(
            &[
                b"XREADGROUP".to_vec(),
                b"GROUP".to_vec(),
                b"g1".to_vec(),
                b"c1".to_vec(),
                b"STREAMS".to_vec(),
                b"s".to_vec(),
                b"bad-id".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xreadgroup invalid id");
        assert_eq!(
            invalid_id,
            RespFrame::Error(
                "ERR Invalid stream ID specified as stream command argument".to_string()
            )
        );

        let syntax_block = dispatch_argv(
            &[
                b"XREADGROUP".to_vec(),
                b"GROUP".to_vec(),
                b"g1".to_vec(),
                b"c1".to_vec(),
                b"BLOCK".to_vec(),
                b"0".to_vec(),
                b"STREAMS".to_vec(),
                b"s".to_vec(),
                b">".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect_err("xreadgroup block unsupported");
        assert!(matches!(syntax_block, CommandError::SyntaxError));

        let syntax_streams = dispatch_argv(
            &[
                b"XREADGROUP".to_vec(),
                b"GROUP".to_vec(),
                b"g1".to_vec(),
                b"c1".to_vec(),
                b"NOTSTREAMS".to_vec(),
                b"s".to_vec(),
                b">".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect_err("xreadgroup missing STREAMS");
        assert!(matches!(syntax_streams, CommandError::SyntaxError));

        let arity = dispatch_argv(
            &[
                b"XREADGROUP".to_vec(),
                b"GROUP".to_vec(),
                b"g1".to_vec(),
                b"c1".to_vec(),
                b"STREAMS".to_vec(),
                b"s".to_vec(),
                b"s2".to_vec(),
                b">".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect_err("xreadgroup keys/ids mismatch");
        assert!(matches!(arity, CommandError::WrongArity("XREADGROUP")));

        store.set(b"k".to_vec(), b"v".to_vec(), None, 0);
        let wrongtype = dispatch_argv(
            &[
                b"XREADGROUP".to_vec(),
                b"GROUP".to_vec(),
                b"g1".to_vec(),
                b"c1".to_vec(),
                b"STREAMS".to_vec(),
                b"k".to_vec(),
                b">".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect_err("xreadgroup wrongtype");
        assert!(matches!(
            wrongtype,
            CommandError::Store(fr_store::StoreError::WrongType)
        ));
    }

    #[test]
    fn xreadgroup_pending_replay_and_noack_behavior() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"XADD".to_vec(),
                b"s".to_vec(),
                b"1000-0".to_vec(),
                b"field".to_vec(),
                b"v0".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xadd 0");
        dispatch_argv(
            &[
                b"XADD".to_vec(),
                b"s".to_vec(),
                b"1000-1".to_vec(),
                b"field".to_vec(),
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
                b"1000-2".to_vec(),
                b"field".to_vec(),
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

        let first_pending = dispatch_argv(
            &[
                b"XREADGROUP".to_vec(),
                b"GROUP".to_vec(),
                b"g1".to_vec(),
                b"c1".to_vec(),
                b"COUNT".to_vec(),
                b"1".to_vec(),
                b"STREAMS".to_vec(),
                b"s".to_vec(),
                b">".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("first pending read");
        assert_eq!(
            first_pending,
            RespFrame::Array(Some(vec![RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"s".to_vec())),
                RespFrame::Array(Some(vec![RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"1000-0".to_vec())),
                    RespFrame::Array(Some(vec![
                        RespFrame::BulkString(Some(b"field".to_vec())),
                        RespFrame::BulkString(Some(b"v0".to_vec())),
                    ])),
                ]))])),
            ]))]))
        );

        let replay_for_owner = dispatch_argv(
            &[
                b"XREADGROUP".to_vec(),
                b"GROUP".to_vec(),
                b"g1".to_vec(),
                b"c1".to_vec(),
                b"STREAMS".to_vec(),
                b"s".to_vec(),
                b"0".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("owner replay");
        assert_eq!(
            replay_for_owner,
            RespFrame::Array(Some(vec![RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"s".to_vec())),
                RespFrame::Array(Some(vec![RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"1000-0".to_vec())),
                    RespFrame::Array(Some(vec![
                        RespFrame::BulkString(Some(b"field".to_vec())),
                        RespFrame::BulkString(Some(b"v0".to_vec())),
                    ])),
                ]))])),
            ]))]))
        );

        let replay_for_other_consumer = dispatch_argv(
            &[
                b"XREADGROUP".to_vec(),
                b"GROUP".to_vec(),
                b"g1".to_vec(),
                b"c2".to_vec(),
                b"STREAMS".to_vec(),
                b"s".to_vec(),
                b"0".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("other consumer replay");
        assert_eq!(replay_for_other_consumer, RespFrame::Array(None));

        let noack_read = dispatch_argv(
            &[
                b"XREADGROUP".to_vec(),
                b"GROUP".to_vec(),
                b"g1".to_vec(),
                b"c1".to_vec(),
                b"NOACK".to_vec(),
                b"STREAMS".to_vec(),
                b"s".to_vec(),
                b">".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("noack read");
        assert_eq!(
            noack_read,
            RespFrame::Array(Some(vec![RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"s".to_vec())),
                RespFrame::Array(Some(vec![
                    RespFrame::Array(Some(vec![
                        RespFrame::BulkString(Some(b"1000-1".to_vec())),
                        RespFrame::Array(Some(vec![
                            RespFrame::BulkString(Some(b"field".to_vec())),
                            RespFrame::BulkString(Some(b"v1".to_vec())),
                        ])),
                    ])),
                    RespFrame::Array(Some(vec![
                        RespFrame::BulkString(Some(b"1000-2".to_vec())),
                        RespFrame::Array(Some(vec![
                            RespFrame::BulkString(Some(b"field".to_vec())),
                            RespFrame::BulkString(Some(b"v2".to_vec())),
                        ])),
                    ])),
                ])),
            ]))]))
        );

        let replay_after_noack = dispatch_argv(
            &[
                b"XREADGROUP".to_vec(),
                b"GROUP".to_vec(),
                b"g1".to_vec(),
                b"c1".to_vec(),
                b"STREAMS".to_vec(),
                b"s".to_vec(),
                b"0".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("replay after noack");
        assert_eq!(
            replay_after_noack,
            RespFrame::Array(Some(vec![RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"s".to_vec())),
                RespFrame::Array(Some(vec![RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"1000-0".to_vec())),
                    RespFrame::Array(Some(vec![
                        RespFrame::BulkString(Some(b"field".to_vec())),
                        RespFrame::BulkString(Some(b"v0".to_vec())),
                    ])),
                ]))])),
            ]))]))
        );

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
                RespFrame::Integer(2),
                RespFrame::BulkString(Some(b"pending".to_vec())),
                RespFrame::Integer(1),
                RespFrame::BulkString(Some(b"last-delivered-id".to_vec())),
                RespFrame::BulkString(Some(b"1000-2".to_vec())),
                RespFrame::BulkString(Some(b"entries-read".to_vec())),
                RespFrame::BulkString(None),
                RespFrame::BulkString(Some(b"lag".to_vec())),
                RespFrame::BulkString(None),
            ]))]))
        );
    }

    #[test]
    fn xpending_reports_summary_and_detailed_pending_entries() {
        let mut store = Store::new();
        for (id, value) in [("1000-0", "v0"), ("1000-1", "v1"), ("1000-2", "v2")] {
            dispatch_argv(
                &[
                    b"XADD".to_vec(),
                    b"s".to_vec(),
                    id.as_bytes().to_vec(),
                    b"field".to_vec(),
                    value.as_bytes().to_vec(),
                ],
                &mut store,
                0,
            )
            .expect("xadd");
        }
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

        dispatch_argv(
            &[
                b"XREADGROUP".to_vec(),
                b"GROUP".to_vec(),
                b"g1".to_vec(),
                b"c1".to_vec(),
                b"COUNT".to_vec(),
                b"2".to_vec(),
                b"STREAMS".to_vec(),
                b"s".to_vec(),
                b">".to_vec(),
            ],
            &mut store,
            10,
        )
        .expect("xreadgroup first");
        dispatch_argv(
            &[
                b"XREADGROUP".to_vec(),
                b"GROUP".to_vec(),
                b"g1".to_vec(),
                b"c1".to_vec(),
                b"STREAMS".to_vec(),
                b"s".to_vec(),
                b"0".to_vec(),
            ],
            &mut store,
            20,
        )
        .expect("xreadgroup replay");
        dispatch_argv(
            &[
                b"XREADGROUP".to_vec(),
                b"GROUP".to_vec(),
                b"g1".to_vec(),
                b"c2".to_vec(),
                b"COUNT".to_vec(),
                b"1".to_vec(),
                b"STREAMS".to_vec(),
                b"s".to_vec(),
                b">".to_vec(),
            ],
            &mut store,
            30,
        )
        .expect("xreadgroup second consumer");

        let summary = dispatch_argv(
            &[b"XPENDING".to_vec(), b"s".to_vec(), b"g1".to_vec()],
            &mut store,
            40,
        )
        .expect("xpending summary");
        assert_eq!(
            summary,
            RespFrame::Array(Some(vec![
                RespFrame::Integer(3),
                RespFrame::BulkString(Some(b"1000-0".to_vec())),
                RespFrame::BulkString(Some(b"1000-2".to_vec())),
                RespFrame::Array(Some(vec![
                    RespFrame::Array(Some(vec![
                        RespFrame::BulkString(Some(b"c1".to_vec())),
                        RespFrame::Integer(2),
                    ])),
                    RespFrame::Array(Some(vec![
                        RespFrame::BulkString(Some(b"c2".to_vec())),
                        RespFrame::Integer(1),
                    ])),
                ])),
            ]))
        );

        let detail = dispatch_argv(
            &[
                b"XPENDING".to_vec(),
                b"s".to_vec(),
                b"g1".to_vec(),
                b"-".to_vec(),
                b"+".to_vec(),
                b"10".to_vec(),
            ],
            &mut store,
            40,
        )
        .expect("xpending detail");
        assert_eq!(
            detail,
            RespFrame::Array(Some(vec![
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"1000-0".to_vec())),
                    RespFrame::BulkString(Some(b"c1".to_vec())),
                    RespFrame::Integer(20),
                    RespFrame::Integer(2),
                ])),
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"1000-1".to_vec())),
                    RespFrame::BulkString(Some(b"c1".to_vec())),
                    RespFrame::Integer(20),
                    RespFrame::Integer(2),
                ])),
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"1000-2".to_vec())),
                    RespFrame::BulkString(Some(b"c2".to_vec())),
                    RespFrame::Integer(10),
                    RespFrame::Integer(1),
                ])),
            ]))
        );

        let filtered = dispatch_argv(
            &[
                b"XPENDING".to_vec(),
                b"s".to_vec(),
                b"g1".to_vec(),
                b"-".to_vec(),
                b"+".to_vec(),
                b"10".to_vec(),
                b"c1".to_vec(),
            ],
            &mut store,
            40,
        )
        .expect("xpending filtered");
        assert_eq!(
            filtered,
            RespFrame::Array(Some(vec![
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"1000-0".to_vec())),
                    RespFrame::BulkString(Some(b"c1".to_vec())),
                    RespFrame::Integer(20),
                    RespFrame::Integer(2),
                ])),
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"1000-1".to_vec())),
                    RespFrame::BulkString(Some(b"c1".to_vec())),
                    RespFrame::Integer(20),
                    RespFrame::Integer(2),
                ])),
            ]))
        );
    }

    #[test]
    fn xpending_validation_and_nogroup_behavior() {
        let mut store = Store::new();
        let arity = dispatch_argv(&[b"XPENDING".to_vec(), b"s".to_vec()], &mut store, 0)
            .expect_err("xpending arity");
        assert!(matches!(arity, CommandError::WrongArity("XPENDING")));

        let invalid_count = dispatch_argv(
            &[
                b"XPENDING".to_vec(),
                b"s".to_vec(),
                b"g1".to_vec(),
                b"-".to_vec(),
                b"+".to_vec(),
                b"-1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect_err("xpending invalid count");
        assert_eq!(invalid_count, CommandError::InvalidInteger);

        let missing = dispatch_argv(
            &[b"XPENDING".to_vec(), b"s".to_vec(), b"g1".to_vec()],
            &mut store,
            0,
        )
        .expect("xpending missing group");
        assert_eq!(
            missing,
            RespFrame::Error(
                "NOGROUP No such key 's' or consumer group 'g1' in XPENDING command".to_string()
            )
        );
    }

    #[test]
    fn xclaim_transfers_pending_entries_and_supports_justid() {
        let mut store = Store::new();
        for (id, value) in [("1000-0", "v0"), ("1000-1", "v1")] {
            dispatch_argv(
                &[
                    b"XADD".to_vec(),
                    b"s".to_vec(),
                    id.as_bytes().to_vec(),
                    b"field".to_vec(),
                    value.as_bytes().to_vec(),
                ],
                &mut store,
                0,
            )
            .expect("xadd");
        }
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
        dispatch_argv(
            &[
                b"XREADGROUP".to_vec(),
                b"GROUP".to_vec(),
                b"g1".to_vec(),
                b"c1".to_vec(),
                b"STREAMS".to_vec(),
                b"s".to_vec(),
                b">".to_vec(),
            ],
            &mut store,
            10,
        )
        .expect("seed pending");

        let claimed = dispatch_argv(
            &[
                b"XCLAIM".to_vec(),
                b"s".to_vec(),
                b"g1".to_vec(),
                b"c2".to_vec(),
                b"5".to_vec(),
                b"1000-0".to_vec(),
            ],
            &mut store,
            30,
        )
        .expect("xclaim");
        assert_eq!(
            claimed,
            RespFrame::Array(Some(vec![RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"1000-0".to_vec())),
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"field".to_vec())),
                    RespFrame::BulkString(Some(b"v0".to_vec())),
                ])),
            ]))]))
        );

        let summary = dispatch_argv(
            &[b"XPENDING".to_vec(), b"s".to_vec(), b"g1".to_vec()],
            &mut store,
            30,
        )
        .expect("xpending summary");
        assert_eq!(
            summary,
            RespFrame::Array(Some(vec![
                RespFrame::Integer(2),
                RespFrame::BulkString(Some(b"1000-0".to_vec())),
                RespFrame::BulkString(Some(b"1000-1".to_vec())),
                RespFrame::Array(Some(vec![
                    RespFrame::Array(Some(vec![
                        RespFrame::BulkString(Some(b"c1".to_vec())),
                        RespFrame::Integer(1),
                    ])),
                    RespFrame::Array(Some(vec![
                        RespFrame::BulkString(Some(b"c2".to_vec())),
                        RespFrame::Integer(1),
                    ])),
                ])),
            ]))
        );

        let justid = dispatch_argv(
            &[
                b"XCLAIM".to_vec(),
                b"s".to_vec(),
                b"g1".to_vec(),
                b"c2".to_vec(),
                b"5".to_vec(),
                b"1000-1".to_vec(),
                b"JUSTID".to_vec(),
            ],
            &mut store,
            40,
        )
        .expect("xclaim justid");
        assert_eq!(
            justid,
            RespFrame::Array(Some(vec![RespFrame::BulkString(Some(b"1000-1".to_vec()))]))
        );
    }

    #[test]
    fn xautoclaim_claims_by_cursor_and_returns_next_start() {
        let mut store = Store::new();
        for (id, value) in [("1000-0", "v0"), ("1000-1", "v1"), ("1000-2", "v2")] {
            dispatch_argv(
                &[
                    b"XADD".to_vec(),
                    b"s".to_vec(),
                    id.as_bytes().to_vec(),
                    b"field".to_vec(),
                    value.as_bytes().to_vec(),
                ],
                &mut store,
                0,
            )
            .expect("xadd");
        }
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
        dispatch_argv(
            &[
                b"XREADGROUP".to_vec(),
                b"GROUP".to_vec(),
                b"g1".to_vec(),
                b"c1".to_vec(),
                b"STREAMS".to_vec(),
                b"s".to_vec(),
                b">".to_vec(),
            ],
            &mut store,
            10,
        )
        .expect("seed pending");

        let first = dispatch_argv(
            &[
                b"XAUTOCLAIM".to_vec(),
                b"s".to_vec(),
                b"g1".to_vec(),
                b"c2".to_vec(),
                b"5".to_vec(),
                b"0-0".to_vec(),
                b"COUNT".to_vec(),
                b"2".to_vec(),
            ],
            &mut store,
            30,
        )
        .expect("xautoclaim first");
        assert_eq!(
            first,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"1000-2".to_vec())),
                RespFrame::Array(Some(vec![
                    RespFrame::Array(Some(vec![
                        RespFrame::BulkString(Some(b"1000-0".to_vec())),
                        RespFrame::Array(Some(vec![
                            RespFrame::BulkString(Some(b"field".to_vec())),
                            RespFrame::BulkString(Some(b"v0".to_vec())),
                        ])),
                    ])),
                    RespFrame::Array(Some(vec![
                        RespFrame::BulkString(Some(b"1000-1".to_vec())),
                        RespFrame::Array(Some(vec![
                            RespFrame::BulkString(Some(b"field".to_vec())),
                            RespFrame::BulkString(Some(b"v1".to_vec())),
                        ])),
                    ])),
                ])),
                RespFrame::Array(Some(vec![])),
            ]))
        );

        let second = dispatch_argv(
            &[
                b"XAUTOCLAIM".to_vec(),
                b"s".to_vec(),
                b"g1".to_vec(),
                b"c3".to_vec(),
                b"0".to_vec(),
                b"1000-2".to_vec(),
                b"COUNT".to_vec(),
                b"5".to_vec(),
                b"JUSTID".to_vec(),
            ],
            &mut store,
            31,
        )
        .expect("xautoclaim second");
        assert_eq!(
            second,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"0-0".to_vec())),
                RespFrame::Array(Some(vec![RespFrame::BulkString(Some(b"1000-2".to_vec()))])),
                RespFrame::Array(Some(vec![])),
            ]))
        );
    }

    #[test]
    fn xclaim_and_xautoclaim_validation_and_nogroup_errors() {
        let mut store = Store::new();
        let xclaim_arity = dispatch_argv(
            &[
                b"XCLAIM".to_vec(),
                b"s".to_vec(),
                b"g1".to_vec(),
                b"c1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect_err("xclaim arity");
        assert!(matches!(xclaim_arity, CommandError::WrongArity("XCLAIM")));

        let xautoclaim_arity =
            dispatch_argv(&[b"XAUTOCLAIM".to_vec(), b"s".to_vec()], &mut store, 0)
                .expect_err("xautoclaim arity");
        assert!(matches!(
            xautoclaim_arity,
            CommandError::WrongArity("XAUTOCLAIM")
        ));

        let invalid_count = dispatch_argv(
            &[
                b"XAUTOCLAIM".to_vec(),
                b"s".to_vec(),
                b"g1".to_vec(),
                b"c1".to_vec(),
                b"0".to_vec(),
                b"0-0".to_vec(),
                b"COUNT".to_vec(),
                b"0".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect_err("xautoclaim invalid count");
        assert_eq!(invalid_count, CommandError::InvalidInteger);

        let xclaim_missing = dispatch_argv(
            &[
                b"XCLAIM".to_vec(),
                b"s".to_vec(),
                b"g1".to_vec(),
                b"c1".to_vec(),
                b"0".to_vec(),
                b"1-0".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xclaim nogroup");
        assert_eq!(
            xclaim_missing,
            RespFrame::Error(
                "NOGROUP No such key 's' or consumer group 'g1' in XCLAIM command".to_string()
            )
        );

        let xautoclaim_missing = dispatch_argv(
            &[
                b"XAUTOCLAIM".to_vec(),
                b"s".to_vec(),
                b"g1".to_vec(),
                b"c1".to_vec(),
                b"0".to_vec(),
                b"0-0".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xautoclaim nogroup");
        assert_eq!(
            xautoclaim_missing,
            RespFrame::Error(
                "NOGROUP No such key 's' or consumer group 'g1' in XAUTOCLAIM command".to_string()
            )
        );
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
    fn xinfo_consumers_reports_group_membership() {
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

        let empty = dispatch_argv(
            &[
                b"XINFO".to_vec(),
                b"CONSUMERS".to_vec(),
                b"s".to_vec(),
                b"g1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xinfo consumers empty");
        assert_eq!(empty, RespFrame::Array(Some(Vec::new())));

        dispatch_argv(
            &[
                b"XGROUP".to_vec(),
                b"CREATECONSUMER".to_vec(),
                b"s".to_vec(),
                b"g1".to_vec(),
                b"c2".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("createconsumer c2");
        dispatch_argv(
            &[
                b"XGROUP".to_vec(),
                b"CREATECONSUMER".to_vec(),
                b"s".to_vec(),
                b"g1".to_vec(),
                b"c1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("createconsumer c1");

        let consumers = dispatch_argv(
            &[
                b"XINFO".to_vec(),
                b"CONSUMERS".to_vec(),
                b"s".to_vec(),
                b"g1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xinfo consumers");
        assert_eq!(
            consumers,
            RespFrame::Array(Some(vec![
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"name".to_vec())),
                    RespFrame::BulkString(Some(b"c1".to_vec())),
                    RespFrame::BulkString(Some(b"pending".to_vec())),
                    RespFrame::Integer(0),
                    RespFrame::BulkString(Some(b"idle".to_vec())),
                    RespFrame::Integer(0),
                    RespFrame::BulkString(Some(b"inactive".to_vec())),
                    RespFrame::Integer(0),
                ])),
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"name".to_vec())),
                    RespFrame::BulkString(Some(b"c2".to_vec())),
                    RespFrame::BulkString(Some(b"pending".to_vec())),
                    RespFrame::Integer(0),
                    RespFrame::BulkString(Some(b"idle".to_vec())),
                    RespFrame::Integer(0),
                    RespFrame::BulkString(Some(b"inactive".to_vec())),
                    RespFrame::Integer(0),
                ])),
            ]))
        );
    }

    #[test]
    fn xinfo_consumers_validation_missing_group_key_wrongtype_and_arity() {
        let mut store = Store::new();

        let missing_key = dispatch_argv(
            &[
                b"XINFO".to_vec(),
                b"CONSUMERS".to_vec(),
                b"missing".to_vec(),
                b"g1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect_err("xinfo consumers missing key");
        assert!(matches!(missing_key, CommandError::NoSuchKey));

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
        let missing_group = dispatch_argv(
            &[
                b"XINFO".to_vec(),
                b"CONSUMERS".to_vec(),
                b"s".to_vec(),
                b"g1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xinfo consumers missing group");
        assert_eq!(
            missing_group,
            RespFrame::Error("NOGROUP No such consumer group 'g1' for key name 's'".to_string())
        );

        store.set(b"str".to_vec(), b"value".to_vec(), None, 0);
        let wrongtype = dispatch_argv(
            &[
                b"XINFO".to_vec(),
                b"CONSUMERS".to_vec(),
                b"str".to_vec(),
                b"g1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect_err("xinfo consumers wrongtype");
        assert!(matches!(
            wrongtype,
            CommandError::Store(fr_store::StoreError::WrongType)
        ));

        let arity = dispatch_argv(
            &[b"XINFO".to_vec(), b"CONSUMERS".to_vec(), b"s".to_vec()],
            &mut store,
            0,
        )
        .expect_err("xinfo consumers arity");
        assert!(matches!(arity, CommandError::WrongArity("XINFO")));
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

    #[test]
    fn xgroup_createconsumer_and_delconsumer_update_group_consumer_counts() {
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

        let create_1 = dispatch_argv(
            &[
                b"XGROUP".to_vec(),
                b"CREATECONSUMER".to_vec(),
                b"s".to_vec(),
                b"g1".to_vec(),
                b"c1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xgroup createconsumer c1");
        assert_eq!(create_1, RespFrame::Integer(1));

        let create_dup = dispatch_argv(
            &[
                b"XGROUP".to_vec(),
                b"CREATECONSUMER".to_vec(),
                b"s".to_vec(),
                b"g1".to_vec(),
                b"c1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xgroup createconsumer duplicate");
        assert_eq!(create_dup, RespFrame::Integer(0));

        let create_2 = dispatch_argv(
            &[
                b"XGROUP".to_vec(),
                b"CREATECONSUMER".to_vec(),
                b"s".to_vec(),
                b"g1".to_vec(),
                b"c2".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xgroup createconsumer c2");
        assert_eq!(create_2, RespFrame::Integer(1));

        let groups_with_two = dispatch_argv(
            &[b"XINFO".to_vec(), b"GROUPS".to_vec(), b"s".to_vec()],
            &mut store,
            0,
        )
        .expect("xinfo groups after createconsumer");
        assert_eq!(
            groups_with_two,
            RespFrame::Array(Some(vec![RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"name".to_vec())),
                RespFrame::BulkString(Some(b"g1".to_vec())),
                RespFrame::BulkString(Some(b"consumers".to_vec())),
                RespFrame::Integer(2),
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

        let del_1 = dispatch_argv(
            &[
                b"XGROUP".to_vec(),
                b"DELCONSUMER".to_vec(),
                b"s".to_vec(),
                b"g1".to_vec(),
                b"c1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xgroup delconsumer c1");
        assert_eq!(del_1, RespFrame::Integer(0));

        let groups_with_one = dispatch_argv(
            &[b"XINFO".to_vec(), b"GROUPS".to_vec(), b"s".to_vec()],
            &mut store,
            0,
        )
        .expect("xinfo groups after delconsumer");
        assert_eq!(
            groups_with_one,
            RespFrame::Array(Some(vec![RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"name".to_vec())),
                RespFrame::BulkString(Some(b"g1".to_vec())),
                RespFrame::BulkString(Some(b"consumers".to_vec())),
                RespFrame::Integer(1),
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

        let del_missing_consumer = dispatch_argv(
            &[
                b"XGROUP".to_vec(),
                b"DELCONSUMER".to_vec(),
                b"s".to_vec(),
                b"g1".to_vec(),
                b"missing".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xgroup delconsumer missing consumer");
        assert_eq!(del_missing_consumer, RespFrame::Integer(0));
    }

    #[test]
    fn xgroup_consumer_commands_validate_nogroup_wrongtype_and_arity() {
        let mut store = Store::new();

        let missing_key = dispatch_argv(
            &[
                b"XGROUP".to_vec(),
                b"CREATECONSUMER".to_vec(),
                b"missing".to_vec(),
                b"g1".to_vec(),
                b"c1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xgroup createconsumer missing key");
        assert_eq!(
            missing_key,
            RespFrame::Error(
                "NOGROUP No such key 'missing' or consumer group 'g1' in XGROUP command"
                    .to_string()
            )
        );

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
        let missing_group = dispatch_argv(
            &[
                b"XGROUP".to_vec(),
                b"DELCONSUMER".to_vec(),
                b"s".to_vec(),
                b"g1".to_vec(),
                b"c1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("xgroup delconsumer missing group");
        assert_eq!(
            missing_group,
            RespFrame::Error(
                "NOGROUP No such key 's' or consumer group 'g1' in XGROUP command".to_string()
            )
        );

        store.set(b"str".to_vec(), b"value".to_vec(), None, 0);
        let wrongtype = dispatch_argv(
            &[
                b"XGROUP".to_vec(),
                b"CREATECONSUMER".to_vec(),
                b"str".to_vec(),
                b"g1".to_vec(),
                b"c1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect_err("xgroup createconsumer wrongtype");
        assert!(matches!(
            wrongtype,
            CommandError::Store(fr_store::StoreError::WrongType)
        ));

        let arity_create = dispatch_argv(
            &[
                b"XGROUP".to_vec(),
                b"CREATECONSUMER".to_vec(),
                b"s".to_vec(),
                b"g1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect_err("xgroup createconsumer arity");
        assert!(matches!(arity_create, CommandError::WrongArity("XGROUP")));

        let arity_del = dispatch_argv(
            &[
                b"XGROUP".to_vec(),
                b"DELCONSUMER".to_vec(),
                b"s".to_vec(),
                b"g1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect_err("xgroup delconsumer arity");
        assert!(matches!(arity_del, CommandError::WrongArity("XGROUP")));
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

    #[test]
    fn sintercard_basic() {
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
        // Without LIMIT
        let out = dispatch_argv(
            &[
                b"SINTERCARD".to_vec(),
                b"2".to_vec(),
                b"s1".to_vec(),
                b"s2".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("sintercard");
        assert_eq!(out, RespFrame::Integer(2));
    }

    #[test]
    fn sintercard_with_limit() {
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
        // With LIMIT 1
        let out = dispatch_argv(
            &[
                b"SINTERCARD".to_vec(),
                b"2".to_vec(),
                b"s1".to_vec(),
                b"s2".to_vec(),
                b"LIMIT".to_vec(),
                b"1".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("sintercard limit");
        assert_eq!(out, RespFrame::Integer(1));
    }

    #[test]
    fn sintercard_empty_intersection() {
        let mut store = Store::new();
        dispatch_argv(
            &[b"SADD".to_vec(), b"s1".to_vec(), b"a".to_vec()],
            &mut store,
            0,
        )
        .expect("sadd");
        dispatch_argv(
            &[b"SADD".to_vec(), b"s2".to_vec(), b"b".to_vec()],
            &mut store,
            0,
        )
        .expect("sadd");
        let out = dispatch_argv(
            &[
                b"SINTERCARD".to_vec(),
                b"2".to_vec(),
                b"s1".to_vec(),
                b"s2".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("sintercard");
        assert_eq!(out, RespFrame::Integer(0));
    }

    #[test]
    fn sintercard_missing_key() {
        let mut store = Store::new();
        dispatch_argv(
            &[b"SADD".to_vec(), b"s1".to_vec(), b"a".to_vec()],
            &mut store,
            0,
        )
        .expect("sadd");
        let out = dispatch_argv(
            &[
                b"SINTERCARD".to_vec(),
                b"2".to_vec(),
                b"s1".to_vec(),
                b"missing".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("sintercard");
        assert_eq!(out, RespFrame::Integer(0));
    }

    #[test]
    fn lcs_basic() {
        let mut store = Store::new();
        dispatch_argv(
            &[b"SET".to_vec(), b"k1".to_vec(), b"ohmytext".to_vec()],
            &mut store,
            0,
        )
        .expect("set k1");
        dispatch_argv(
            &[b"SET".to_vec(), b"k2".to_vec(), b"mynewtext".to_vec()],
            &mut store,
            0,
        )
        .expect("set k2");
        let out = dispatch_argv(
            &[b"LCS".to_vec(), b"k1".to_vec(), b"k2".to_vec()],
            &mut store,
            0,
        )
        .expect("lcs");
        assert_eq!(out, RespFrame::BulkString(Some(b"mytext".to_vec())));
    }

    #[test]
    fn lcs_len() {
        let mut store = Store::new();
        dispatch_argv(
            &[b"SET".to_vec(), b"k1".to_vec(), b"ohmytext".to_vec()],
            &mut store,
            0,
        )
        .expect("set");
        dispatch_argv(
            &[b"SET".to_vec(), b"k2".to_vec(), b"mynewtext".to_vec()],
            &mut store,
            0,
        )
        .expect("set");
        let out = dispatch_argv(
            &[
                b"LCS".to_vec(),
                b"k1".to_vec(),
                b"k2".to_vec(),
                b"LEN".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("lcs len");
        assert_eq!(out, RespFrame::Integer(6));
    }

    #[test]
    fn lcs_missing_keys() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[b"LCS".to_vec(), b"k1".to_vec(), b"k2".to_vec()],
            &mut store,
            0,
        )
        .expect("lcs");
        assert_eq!(out, RespFrame::BulkString(Some(Vec::new())));
    }

    #[test]
    fn lmpop_basic() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"RPUSH".to_vec(),
                b"mylist".to_vec(),
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
                b"LMPOP".to_vec(),
                b"1".to_vec(),
                b"mylist".to_vec(),
                b"LEFT".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("lmpop");
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"mylist".to_vec())),
                RespFrame::Array(Some(vec![RespFrame::BulkString(Some(b"a".to_vec()))])),
            ]))
        );
    }

    #[test]
    fn lmpop_count() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"RPUSH".to_vec(),
                b"mylist".to_vec(),
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
                b"LMPOP".to_vec(),
                b"1".to_vec(),
                b"mylist".to_vec(),
                b"RIGHT".to_vec(),
                b"COUNT".to_vec(),
                b"2".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("lmpop");
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"mylist".to_vec())),
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"c".to_vec())),
                    RespFrame::BulkString(Some(b"b".to_vec())),
                ])),
            ]))
        );
    }

    #[test]
    fn lmpop_no_keys() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[
                b"LMPOP".to_vec(),
                b"1".to_vec(),
                b"missing".to_vec(),
                b"LEFT".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("lmpop");
        assert_eq!(out, RespFrame::Array(None));
    }

    #[test]
    fn zmpop_basic() {
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
                b"ZMPOP".to_vec(),
                b"1".to_vec(),
                b"zs".to_vec(),
                b"MIN".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("zmpop");
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"zs".to_vec())),
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"a".to_vec())),
                    RespFrame::BulkString(Some(b"1".to_vec())),
                ])),
            ]))
        );
    }

    #[test]
    fn zmpop_no_keys() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[
                b"ZMPOP".to_vec(),
                b"1".to_vec(),
                b"missing".to_vec(),
                b"MAX".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("zmpop");
        assert_eq!(out, RespFrame::Array(None));
    }

    #[test]
    fn memory_usage_string() {
        let mut store = Store::new();
        store.set(b"hello".to_vec(), b"world".to_vec(), None, 0);
        let out = dispatch_argv(
            &[b"MEMORY".to_vec(), b"USAGE".to_vec(), b"hello".to_vec()],
            &mut store,
            0,
        )
        .expect("memory usage");
        if let RespFrame::Integer(n) = out {
            assert!(n > 0, "memory usage should be positive");
        } else {
            panic!("expected Integer, got {out:?}");
        }
    }

    #[test]
    fn memory_usage_missing_key() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[b"MEMORY".to_vec(), b"USAGE".to_vec(), b"nokey".to_vec()],
            &mut store,
            0,
        )
        .expect("memory usage");
        assert_eq!(out, RespFrame::BulkString(None));
    }

    #[test]
    fn memory_usage_with_samples() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), b"v".to_vec(), None, 0);
        let out = dispatch_argv(
            &[
                b"MEMORY".to_vec(),
                b"USAGE".to_vec(),
                b"k".to_vec(),
                b"SAMPLES".to_vec(),
                b"5".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("memory usage samples");
        if let RespFrame::Integer(n) = out {
            assert!(n > 0);
        } else {
            panic!("expected Integer, got {out:?}");
        }
    }

    #[test]
    fn memory_doctor() {
        let mut store = Store::new();
        let out = dispatch_argv(&[b"MEMORY".to_vec(), b"DOCTOR".to_vec()], &mut store, 0)
            .expect("memory doctor");
        assert!(matches!(out, RespFrame::BulkString(Some(_))));
    }

    #[test]
    fn memory_help() {
        let mut store = Store::new();
        let out = dispatch_argv(&[b"MEMORY".to_vec(), b"HELP".to_vec()], &mut store, 0)
            .expect("memory help");
        assert!(matches!(out, RespFrame::Array(Some(_))));
    }

    #[test]
    fn save_returns_ok() {
        let mut store = Store::new();
        let out = dispatch_argv(&[b"SAVE".to_vec()], &mut store, 0).expect("save");
        assert_eq!(out, RespFrame::SimpleString("OK".to_string()));
    }

    #[test]
    fn bgsave_returns_ok() {
        let mut store = Store::new();
        let out = dispatch_argv(&[b"BGSAVE".to_vec()], &mut store, 0).expect("bgsave");
        assert_eq!(
            out,
            RespFrame::SimpleString("Background saving started".to_string())
        );
    }

    #[test]
    fn bgsave_schedule() {
        let mut store = Store::new();
        let out = dispatch_argv(&[b"BGSAVE".to_vec(), b"SCHEDULE".to_vec()], &mut store, 0)
            .expect("bgsave schedule");
        assert_eq!(
            out,
            RespFrame::SimpleString("Background saving started".to_string())
        );
    }

    #[test]
    fn bgrewriteaof_returns_ok() {
        let mut store = Store::new();
        let out = dispatch_argv(&[b"BGREWRITEAOF".to_vec()], &mut store, 0).expect("bgrewriteaof");
        assert_eq!(
            out,
            RespFrame::SimpleString("Background append only file rewriting started".to_string())
        );
    }

    #[test]
    fn lastsave_returns_timestamp() {
        let mut store = Store::new();
        let out =
            dispatch_argv(&[b"LASTSAVE".to_vec()], &mut store, 1700000000000).expect("lastsave");
        assert_eq!(out, RespFrame::Integer(1700000000));
    }

    #[test]
    fn swapdb_same_db_zero() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[b"SWAPDB".to_vec(), b"0".to_vec(), b"0".to_vec()],
            &mut store,
            0,
        )
        .expect("swapdb");
        assert_eq!(out, RespFrame::SimpleString("OK".to_string()));
    }

    #[test]
    fn swapdb_invalid_db() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[b"SWAPDB".to_vec(), b"0".to_vec(), b"1".to_vec()],
            &mut store,
            0,
        )
        .expect("swapdb");
        assert_eq!(out, RespFrame::Error("ERR invalid DB index".to_string()));
    }

    #[test]
    fn blpop_with_data() {
        let mut store = Store::new();
        dispatch_argv(
            &[b"RPUSH".to_vec(), b"mylist".to_vec(), b"a".to_vec()],
            &mut store,
            0,
        )
        .unwrap();
        let out = dispatch_argv(
            &[b"BLPOP".to_vec(), b"mylist".to_vec(), b"0".to_vec()],
            &mut store,
            0,
        )
        .expect("blpop");
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"mylist".to_vec())),
                RespFrame::BulkString(Some(b"a".to_vec())),
            ]))
        );
    }

    #[test]
    fn blpop_empty() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[b"BLPOP".to_vec(), b"nokey".to_vec(), b"0".to_vec()],
            &mut store,
            0,
        )
        .expect("blpop empty");
        assert_eq!(out, RespFrame::Array(None));
    }

    #[test]
    fn brpop_with_data() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"RPUSH".to_vec(),
                b"mylist".to_vec(),
                b"a".to_vec(),
                b"b".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        let out = dispatch_argv(
            &[b"BRPOP".to_vec(), b"mylist".to_vec(), b"0".to_vec()],
            &mut store,
            0,
        )
        .expect("brpop");
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"mylist".to_vec())),
                RespFrame::BulkString(Some(b"b".to_vec())),
            ]))
        );
    }

    #[test]
    fn blmove_with_data() {
        let mut store = Store::new();
        dispatch_argv(
            &[b"RPUSH".to_vec(), b"src".to_vec(), b"val".to_vec()],
            &mut store,
            0,
        )
        .unwrap();
        let out = dispatch_argv(
            &[
                b"BLMOVE".to_vec(),
                b"src".to_vec(),
                b"dst".to_vec(),
                b"LEFT".to_vec(),
                b"RIGHT".to_vec(),
                b"0".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("blmove");
        assert_eq!(out, RespFrame::BulkString(Some(b"val".to_vec())));
    }

    #[test]
    fn blmpop_with_data() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"RPUSH".to_vec(),
                b"mylist".to_vec(),
                b"x".to_vec(),
                b"y".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        let out = dispatch_argv(
            &[
                b"BLMPOP".to_vec(),
                b"0".to_vec(),
                b"1".to_vec(),
                b"mylist".to_vec(),
                b"LEFT".to_vec(),
                b"COUNT".to_vec(),
                b"2".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("blmpop");
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"mylist".to_vec())),
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"x".to_vec())),
                    RespFrame::BulkString(Some(b"y".to_vec())),
                ])),
            ]))
        );
    }

    #[test]
    fn blmpop_empty() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[
                b"BLMPOP".to_vec(),
                b"0".to_vec(),
                b"1".to_vec(),
                b"nokey".to_vec(),
                b"LEFT".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("blmpop empty");
        assert_eq!(out, RespFrame::Array(None));
    }

    #[test]
    fn subscribe_single_channel() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[b"SUBSCRIBE".to_vec(), b"mychannel".to_vec()],
            &mut store,
            0,
        )
        .expect("subscribe");
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"subscribe".to_vec())),
                RespFrame::BulkString(Some(b"mychannel".to_vec())),
                RespFrame::Integer(1),
            ]))
        );
    }

    #[test]
    fn unsubscribe_all() {
        let mut store = Store::new();
        let out = dispatch_argv(&[b"UNSUBSCRIBE".to_vec()], &mut store, 0).expect("unsubscribe");
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"unsubscribe".to_vec())),
                RespFrame::BulkString(None),
                RespFrame::Integer(0),
            ]))
        );
    }

    #[test]
    fn publish_returns_zero() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[b"PUBLISH".to_vec(), b"ch".to_vec(), b"msg".to_vec()],
            &mut store,
            0,
        )
        .expect("publish");
        assert_eq!(out, RespFrame::Integer(0));
    }

    #[test]
    fn pubsub_channels_empty() {
        let mut store = Store::new();
        let out = dispatch_argv(&[b"PUBSUB".to_vec(), b"CHANNELS".to_vec()], &mut store, 0)
            .expect("pubsub channels");
        assert_eq!(out, RespFrame::Array(Some(Vec::new())));
    }

    #[test]
    fn pubsub_numpat() {
        let mut store = Store::new();
        let out = dispatch_argv(&[b"PUBSUB".to_vec(), b"NUMPAT".to_vec()], &mut store, 0)
            .expect("pubsub numpat");
        assert_eq!(out, RespFrame::Integer(0));
    }

    #[test]
    fn psubscribe_pattern() {
        let mut store = Store::new();
        let out = dispatch_argv(&[b"PSUBSCRIBE".to_vec(), b"news.*".to_vec()], &mut store, 0)
            .expect("psubscribe");
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"psubscribe".to_vec())),
                RespFrame::BulkString(Some(b"news.*".to_vec())),
                RespFrame::Integer(1),
            ]))
        );
    }

    #[test]
    fn punsubscribe_all() {
        let mut store = Store::new();
        let out = dispatch_argv(&[b"PUNSUBSCRIBE".to_vec()], &mut store, 0).expect("punsubscribe");
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"punsubscribe".to_vec())),
                RespFrame::BulkString(None),
                RespFrame::Integer(0),
            ]))
        );
    }

    #[test]
    fn msetnx_all_new() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[
                b"MSETNX".to_vec(),
                b"a".to_vec(),
                b"1".to_vec(),
                b"b".to_vec(),
                b"2".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("msetnx");
        assert_eq!(out, RespFrame::Integer(1));
    }

    #[test]
    fn msetnx_key_exists() {
        let mut store = Store::new();
        store.set(b"a".to_vec(), b"old".to_vec(), None, 0);
        let out = dispatch_argv(
            &[
                b"MSETNX".to_vec(),
                b"a".to_vec(),
                b"1".to_vec(),
                b"b".to_vec(),
                b"2".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("msetnx");
        assert_eq!(out, RespFrame::Integer(0));
    }

    #[test]
    fn brpoplpush_with_data() {
        let mut store = Store::new();
        dispatch_argv(
            &[b"RPUSH".to_vec(), b"src".to_vec(), b"val".to_vec()],
            &mut store,
            0,
        )
        .unwrap();
        let out = dispatch_argv(
            &[
                b"BRPOPLPUSH".to_vec(),
                b"src".to_vec(),
                b"dst".to_vec(),
                b"0".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("brpoplpush");
        assert_eq!(out, RespFrame::BulkString(Some(b"val".to_vec())));
    }

    #[test]
    fn zdiff_basic() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"ZADD".to_vec(),
                b"z1".to_vec(),
                b"1".to_vec(),
                b"a".to_vec(),
                b"2".to_vec(),
                b"b".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        dispatch_argv(
            &[
                b"ZADD".to_vec(),
                b"z2".to_vec(),
                b"1".to_vec(),
                b"a".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        let out = dispatch_argv(
            &[
                b"ZDIFF".to_vec(),
                b"2".to_vec(),
                b"z1".to_vec(),
                b"z2".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("zdiff");
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![RespFrame::BulkString(Some(b"b".to_vec()))]))
        );
    }

    #[test]
    fn zintercard_basic() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"ZADD".to_vec(),
                b"z1".to_vec(),
                b"1".to_vec(),
                b"a".to_vec(),
                b"2".to_vec(),
                b"b".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        dispatch_argv(
            &[
                b"ZADD".to_vec(),
                b"z2".to_vec(),
                b"3".to_vec(),
                b"a".to_vec(),
                b"4".to_vec(),
                b"c".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        let out = dispatch_argv(
            &[
                b"ZINTERCARD".to_vec(),
                b"2".to_vec(),
                b"z1".to_vec(),
                b"z2".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("zintercard");
        assert_eq!(out, RespFrame::Integer(1));
    }

    #[test]
    fn zunion_basic() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"ZADD".to_vec(),
                b"z1".to_vec(),
                b"1".to_vec(),
                b"a".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        dispatch_argv(
            &[
                b"ZADD".to_vec(),
                b"z2".to_vec(),
                b"2".to_vec(),
                b"b".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        let out = dispatch_argv(
            &[
                b"ZUNION".to_vec(),
                b"2".to_vec(),
                b"z1".to_vec(),
                b"z2".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("zunion");
        if let RespFrame::Array(Some(frames)) = out {
            assert_eq!(frames.len(), 2); // two members
        } else {
            panic!("expected array");
        }
    }

    #[test]
    fn zinter_basic() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"ZADD".to_vec(),
                b"z1".to_vec(),
                b"1".to_vec(),
                b"a".to_vec(),
                b"2".to_vec(),
                b"b".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        dispatch_argv(
            &[
                b"ZADD".to_vec(),
                b"z2".to_vec(),
                b"3".to_vec(),
                b"a".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        let out = dispatch_argv(
            &[
                b"ZINTER".to_vec(),
                b"2".to_vec(),
                b"z1".to_vec(),
                b"z2".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("zinter");
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![RespFrame::BulkString(Some(b"a".to_vec()))]))
        );
    }

    #[test]
    fn eval_executes_lua() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[b"EVAL".to_vec(), b"return 1".to_vec(), b"0".to_vec()],
            &mut store,
            0,
        )
        .expect("eval");
        assert_eq!(out, RespFrame::Integer(1));
    }

    #[test]
    fn evalsha_returns_noscript() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[b"EVALSHA".to_vec(), b"abc123".to_vec(), b"0".to_vec()],
            &mut store,
            0,
        )
        .expect("evalsha");
        assert!(matches!(out, RespFrame::Error(_)));
    }

    #[test]
    fn script_exists_returns_zeros() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[
                b"SCRIPT".to_vec(),
                b"EXISTS".to_vec(),
                b"sha1".to_vec(),
                b"sha2".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("script exists");
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![RespFrame::Integer(0), RespFrame::Integer(0),]))
        );
    }

    #[test]
    fn script_flush() {
        let mut store = Store::new();
        let out = dispatch_argv(&[b"SCRIPT".to_vec(), b"FLUSH".to_vec()], &mut store, 0)
            .expect("script flush");
        assert_eq!(out, RespFrame::SimpleString("OK".to_string()));
    }

    #[test]
    fn role_returns_master() {
        let mut store = Store::new();
        let out = dispatch_argv(&[b"ROLE".to_vec()], &mut store, 0).expect("role");
        if let RespFrame::Array(Some(items)) = out {
            assert_eq!(items[0], RespFrame::BulkString(Some(b"master".to_vec())));
        } else {
            panic!("expected array");
        }
    }

    #[test]
    fn debug_sleep() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[b"DEBUG".to_vec(), b"SLEEP".to_vec(), b"0".to_vec()],
            &mut store,
            0,
        )
        .expect("debug sleep");
        assert_eq!(out, RespFrame::SimpleString("OK".to_string()));
    }

    #[test]
    fn shutdown_returns_ok() {
        let mut store = Store::new();
        let out = dispatch_argv(&[b"SHUTDOWN".to_vec()], &mut store, 0).expect("shutdown");
        assert_eq!(out, RespFrame::SimpleString("OK".to_string()));
    }

    #[test]
    fn move_same_db() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[b"MOVE".to_vec(), b"key".to_vec(), b"0".to_vec()],
            &mut store,
            0,
        )
        .expect("move");
        assert_eq!(out, RespFrame::Integer(0));
    }

    #[test]
    fn latency_latest() {
        let mut store = Store::new();
        let out = dispatch_argv(&[b"LATENCY".to_vec(), b"LATEST".to_vec()], &mut store, 0)
            .expect("latency latest");
        assert_eq!(out, RespFrame::Array(Some(Vec::new())));
    }

    #[test]
    fn bitfield_get() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[
                b"BITFIELD".to_vec(),
                b"key".to_vec(),
                b"GET".to_vec(),
                b"u8".to_vec(),
                b"0".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("bitfield get");
        assert_eq!(out, RespFrame::Array(Some(vec![RespFrame::Integer(0)])));
    }

    fn add_geo_points(store: &mut Store) {
        // Add some cities to a geo key
        // Palermo: 13.361389, 38.115556
        // Catania: 15.087269, 37.502669
        dispatch_argv(
            &[
                b"GEOADD".to_vec(),
                b"mygeo".to_vec(),
                b"13.361389".to_vec(),
                b"38.115556".to_vec(),
                b"Palermo".to_vec(),
                b"15.087269".to_vec(),
                b"37.502669".to_vec(),
                b"Catania".to_vec(),
            ],
            store,
            0,
        )
        .expect("geoadd");
    }

    #[test]
    fn georadius_basic() {
        let mut store = Store::new();
        add_geo_points(&mut store);
        let out = dispatch_argv(
            &[
                b"GEORADIUS".to_vec(),
                b"mygeo".to_vec(),
                b"15".to_vec(),
                b"37".to_vec(),
                b"200".to_vec(),
                b"km".to_vec(),
                b"ASC".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("georadius");
        if let RespFrame::Array(Some(frames)) = &out {
            assert_eq!(frames.len(), 2);
        } else {
            panic!("expected array, got {out:?}");
        }
    }

    #[test]
    fn georadiusbymember_basic() {
        let mut store = Store::new();
        add_geo_points(&mut store);
        let out = dispatch_argv(
            &[
                b"GEORADIUSBYMEMBER".to_vec(),
                b"mygeo".to_vec(),
                b"Palermo".to_vec(),
                b"200".to_vec(),
                b"km".to_vec(),
                b"ASC".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("georadiusbymember");
        if let RespFrame::Array(Some(frames)) = &out {
            assert!(!frames.is_empty(), "should find at least Palermo itself");
        } else {
            panic!("expected array, got {out:?}");
        }
    }

    #[test]
    fn geosearch_fromlonlat_byradius() {
        let mut store = Store::new();
        add_geo_points(&mut store);
        let out = dispatch_argv(
            &[
                b"GEOSEARCH".to_vec(),
                b"mygeo".to_vec(),
                b"FROMLONLAT".to_vec(),
                b"15".to_vec(),
                b"37".to_vec(),
                b"BYRADIUS".to_vec(),
                b"200".to_vec(),
                b"km".to_vec(),
                b"ASC".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("geosearch");
        if let RespFrame::Array(Some(frames)) = &out {
            assert_eq!(frames.len(), 2);
        } else {
            panic!("expected array, got {out:?}");
        }
    }

    #[test]
    fn geosearch_frommember_byradius() {
        let mut store = Store::new();
        add_geo_points(&mut store);
        let out = dispatch_argv(
            &[
                b"GEOSEARCH".to_vec(),
                b"mygeo".to_vec(),
                b"FROMMEMBER".to_vec(),
                b"Palermo".to_vec(),
                b"BYRADIUS".to_vec(),
                b"200".to_vec(),
                b"km".to_vec(),
                b"ASC".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("geosearch frommember");
        if let RespFrame::Array(Some(frames)) = &out {
            assert!(!frames.is_empty());
        } else {
            panic!("expected array, got {out:?}");
        }
    }

    #[test]
    fn geosearch_withdist() {
        let mut store = Store::new();
        add_geo_points(&mut store);
        let out = dispatch_argv(
            &[
                b"GEOSEARCH".to_vec(),
                b"mygeo".to_vec(),
                b"FROMLONLAT".to_vec(),
                b"15".to_vec(),
                b"37".to_vec(),
                b"BYRADIUS".to_vec(),
                b"200".to_vec(),
                b"km".to_vec(),
                b"ASC".to_vec(),
                b"WITHDIST".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("geosearch withdist");
        if let RespFrame::Array(Some(frames)) = &out {
            assert_eq!(frames.len(), 2);
            // Each result should be an array [name, dist]
            if let RespFrame::Array(Some(inner)) = &frames[0] {
                assert_eq!(inner.len(), 2);
            } else {
                panic!("expected inner array");
            }
        } else {
            panic!("expected array, got {out:?}");
        }
    }

    #[test]
    fn geosearchstore_basic() {
        let mut store = Store::new();
        add_geo_points(&mut store);
        let out = dispatch_argv(
            &[
                b"GEOSEARCHSTORE".to_vec(),
                b"dest".to_vec(),
                b"mygeo".to_vec(),
                b"FROMLONLAT".to_vec(),
                b"15".to_vec(),
                b"37".to_vec(),
                b"BYRADIUS".to_vec(),
                b"200".to_vec(),
                b"km".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("geosearchstore");
        assert_eq!(out, RespFrame::Integer(2));
        // Verify destination key has 2 members
        let members = store.zrange_withscores(b"dest", 0, -1, 0).expect("zrange");
        assert_eq!(members.len(), 2);
    }

    #[test]
    fn georadius_count_limit() {
        let mut store = Store::new();
        add_geo_points(&mut store);
        let out = dispatch_argv(
            &[
                b"GEORADIUS".to_vec(),
                b"mygeo".to_vec(),
                b"15".to_vec(),
                b"37".to_vec(),
                b"200".to_vec(),
                b"km".to_vec(),
                b"COUNT".to_vec(),
                b"1".to_vec(),
                b"ASC".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("georadius count");
        if let RespFrame::Array(Some(frames)) = &out {
            assert_eq!(frames.len(), 1);
        } else {
            panic!("expected array, got {out:?}");
        }
    }

    #[test]
    fn georadius_empty_key() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[
                b"GEORADIUS".to_vec(),
                b"nokey".to_vec(),
                b"15".to_vec(),
                b"37".to_vec(),
                b"200".to_vec(),
                b"km".to_vec(),
            ],
            &mut store,
            0,
        )
        .expect("georadius empty");
        assert_eq!(out, RespFrame::Array(Some(Vec::new())));
    }

    // ── SORT command tests ──────────────────────────────────────────

    #[test]
    fn sort_nonexistent_key_returns_empty() {
        let mut store = Store::new();
        let out = dispatch_argv(&[b"SORT".to_vec(), b"nokey".to_vec()], &mut store, 0)
            .expect("sort nonexistent");
        assert_eq!(out, RespFrame::Array(Some(Vec::new())));
    }

    #[test]
    fn sort_numeric_list_asc() {
        let mut store = Store::new();
        for val in [b"3", b"1", b"2"] {
            dispatch_argv(
                &[b"RPUSH".to_vec(), b"mylist".to_vec(), val.to_vec()],
                &mut store,
                0,
            )
            .unwrap();
        }
        let out = dispatch_argv(&[b"SORT".to_vec(), b"mylist".to_vec()], &mut store, 0).unwrap();
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"1".to_vec())),
                RespFrame::BulkString(Some(b"2".to_vec())),
                RespFrame::BulkString(Some(b"3".to_vec())),
            ]))
        );
    }

    #[test]
    fn sort_numeric_list_desc() {
        let mut store = Store::new();
        for val in [b"3", b"1", b"2"] {
            dispatch_argv(
                &[b"RPUSH".to_vec(), b"mylist".to_vec(), val.to_vec()],
                &mut store,
                0,
            )
            .unwrap();
        }
        let out = dispatch_argv(
            &[b"SORT".to_vec(), b"mylist".to_vec(), b"DESC".to_vec()],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"3".to_vec())),
                RespFrame::BulkString(Some(b"2".to_vec())),
                RespFrame::BulkString(Some(b"1".to_vec())),
            ]))
        );
    }

    #[test]
    fn sort_alpha_list() {
        let mut store = Store::new();
        for val in [b"charlie" as &[u8], b"alpha", b"bravo"] {
            dispatch_argv(
                &[b"RPUSH".to_vec(), b"mylist".to_vec(), val.to_vec()],
                &mut store,
                0,
            )
            .unwrap();
        }
        let out = dispatch_argv(
            &[b"SORT".to_vec(), b"mylist".to_vec(), b"ALPHA".to_vec()],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"alpha".to_vec())),
                RespFrame::BulkString(Some(b"bravo".to_vec())),
                RespFrame::BulkString(Some(b"charlie".to_vec())),
            ]))
        );
    }

    #[test]
    fn sort_with_limit() {
        let mut store = Store::new();
        for val in [b"5", b"3", b"1", b"4", b"2"] {
            dispatch_argv(
                &[b"RPUSH".to_vec(), b"mylist".to_vec(), val.to_vec()],
                &mut store,
                0,
            )
            .unwrap();
        }
        let out = dispatch_argv(
            &[
                b"SORT".to_vec(),
                b"mylist".to_vec(),
                b"LIMIT".to_vec(),
                b"1".to_vec(),
                b"2".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"2".to_vec())),
                RespFrame::BulkString(Some(b"3".to_vec())),
            ]))
        );
    }

    #[test]
    fn sort_store() {
        let mut store = Store::new();
        for val in [b"3", b"1", b"2"] {
            dispatch_argv(
                &[b"RPUSH".to_vec(), b"mylist".to_vec(), val.to_vec()],
                &mut store,
                0,
            )
            .unwrap();
        }
        let out = dispatch_argv(
            &[
                b"SORT".to_vec(),
                b"mylist".to_vec(),
                b"STORE".to_vec(),
                b"sorted".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(out, RespFrame::Integer(3));

        // Verify the stored list
        let range = dispatch_argv(
            &[
                b"LRANGE".to_vec(),
                b"sorted".to_vec(),
                b"0".to_vec(),
                b"-1".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(
            range,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"1".to_vec())),
                RespFrame::BulkString(Some(b"2".to_vec())),
                RespFrame::BulkString(Some(b"3".to_vec())),
            ]))
        );
    }

    #[test]
    fn sort_by_external_key() {
        let mut store = Store::new();
        for val in [b"a", b"b", b"c"] {
            dispatch_argv(
                &[b"RPUSH".to_vec(), b"mylist".to_vec(), val.to_vec()],
                &mut store,
                0,
            )
            .unwrap();
        }
        // Set external sort keys
        store.set(b"weight_a".to_vec(), b"3".to_vec(), None, 0);
        store.set(b"weight_b".to_vec(), b"1".to_vec(), None, 0);
        store.set(b"weight_c".to_vec(), b"2".to_vec(), None, 0);

        let out = dispatch_argv(
            &[
                b"SORT".to_vec(),
                b"mylist".to_vec(),
                b"BY".to_vec(),
                b"weight_*".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"b".to_vec())),
                RespFrame::BulkString(Some(b"c".to_vec())),
                RespFrame::BulkString(Some(b"a".to_vec())),
            ]))
        );
    }

    #[test]
    fn sort_get_external_value() {
        let mut store = Store::new();
        for val in [b"2", b"1", b"3"] {
            dispatch_argv(
                &[b"RPUSH".to_vec(), b"mylist".to_vec(), val.to_vec()],
                &mut store,
                0,
            )
            .unwrap();
        }
        store.set(b"name_1".to_vec(), b"Alice".to_vec(), None, 0);
        store.set(b"name_2".to_vec(), b"Bob".to_vec(), None, 0);
        store.set(b"name_3".to_vec(), b"Charlie".to_vec(), None, 0);

        let out = dispatch_argv(
            &[
                b"SORT".to_vec(),
                b"mylist".to_vec(),
                b"GET".to_vec(),
                b"name_*".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"Alice".to_vec())),
                RespFrame::BulkString(Some(b"Bob".to_vec())),
                RespFrame::BulkString(Some(b"Charlie".to_vec())),
            ]))
        );
    }

    #[test]
    fn sort_get_hash_pattern() {
        let mut store = Store::new();
        for val in [b"2", b"1"] {
            dispatch_argv(
                &[b"RPUSH".to_vec(), b"mylist".to_vec(), val.to_vec()],
                &mut store,
                0,
            )
            .unwrap();
        }
        dispatch_argv(
            &[
                b"HSET".to_vec(),
                b"user_1".to_vec(),
                b"name".to_vec(),
                b"Alice".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        dispatch_argv(
            &[
                b"HSET".to_vec(),
                b"user_2".to_vec(),
                b"name".to_vec(),
                b"Bob".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();

        let out = dispatch_argv(
            &[
                b"SORT".to_vec(),
                b"mylist".to_vec(),
                b"GET".to_vec(),
                b"user_*->name".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"Alice".to_vec())),
                RespFrame::BulkString(Some(b"Bob".to_vec())),
            ]))
        );
    }

    #[test]
    fn sort_get_hash_self() {
        let mut store = Store::new();
        for val in [b"3", b"1", b"2"] {
            dispatch_argv(
                &[b"RPUSH".to_vec(), b"mylist".to_vec(), val.to_vec()],
                &mut store,
                0,
            )
            .unwrap();
        }
        let out = dispatch_argv(
            &[
                b"SORT".to_vec(),
                b"mylist".to_vec(),
                b"GET".to_vec(),
                b"#".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"1".to_vec())),
                RespFrame::BulkString(Some(b"2".to_vec())),
                RespFrame::BulkString(Some(b"3".to_vec())),
            ]))
        );
    }

    #[test]
    fn sort_set() {
        let mut store = Store::new();
        for val in [b"3", b"1", b"2"] {
            dispatch_argv(
                &[b"SADD".to_vec(), b"myset".to_vec(), val.to_vec()],
                &mut store,
                0,
            )
            .unwrap();
        }
        let out = dispatch_argv(&[b"SORT".to_vec(), b"myset".to_vec()], &mut store, 0).unwrap();
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"1".to_vec())),
                RespFrame::BulkString(Some(b"2".to_vec())),
                RespFrame::BulkString(Some(b"3".to_vec())),
            ]))
        );
    }

    #[test]
    fn sort_by_nosort_preserves_order() {
        let mut store = Store::new();
        for val in [b"c", b"a", b"b"] {
            dispatch_argv(
                &[b"RPUSH".to_vec(), b"mylist".to_vec(), val.to_vec()],
                &mut store,
                0,
            )
            .unwrap();
        }
        let out = dispatch_argv(
            &[
                b"SORT".to_vec(),
                b"mylist".to_vec(),
                b"BY".to_vec(),
                b"nosort".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        // BY nosort preserves insertion order for lists
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"c".to_vec())),
                RespFrame::BulkString(Some(b"a".to_vec())),
                RespFrame::BulkString(Some(b"b".to_vec())),
            ]))
        );
    }

    #[test]
    fn sort_numeric_error_non_numeric() {
        let mut store = Store::new();
        for val in [b"foo" as &[u8], b"bar"] {
            dispatch_argv(
                &[b"RPUSH".to_vec(), b"mylist".to_vec(), val.to_vec()],
                &mut store,
                0,
            )
            .unwrap();
        }
        let out = dispatch_argv(&[b"SORT".to_vec(), b"mylist".to_vec()], &mut store, 0).unwrap();
        match out {
            RespFrame::Error(msg) => {
                assert!(msg.contains("scores can't be converted into double"));
            }
            _ => panic!("expected error for non-numeric sort"),
        }
    }

    #[test]
    fn sort_store_empty_deletes_dest() {
        let mut store = Store::new();
        // Set up a destination key first
        store.set(b"dest".to_vec(), b"existing".to_vec(), None, 0);
        let out = dispatch_argv(
            &[
                b"SORT".to_vec(),
                b"nokey".to_vec(),
                b"STORE".to_vec(),
                b"dest".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(out, RespFrame::Integer(0));
        // Destination should be deleted
        let get = dispatch_argv(&[b"GET".to_vec(), b"dest".to_vec()], &mut store, 0).unwrap();
        assert_eq!(get, RespFrame::BulkString(None));
    }

    #[test]
    fn sort_wrongtype_error() {
        let mut store = Store::new();
        store.set(b"mystr".to_vec(), b"hello".to_vec(), None, 0);
        let out = dispatch_argv(&[b"SORT".to_vec(), b"mystr".to_vec()], &mut store, 0);
        match out {
            Err(CommandError::Store(StoreError::WrongType)) => {}
            _ => panic!("expected WrongType error, got {out:?}"),
        }
    }

    // ── BITFIELD command tests ──────────────────────────────────────

    #[test]
    fn bitfield_get_empty_key() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[
                b"BITFIELD".to_vec(),
                b"bf".to_vec(),
                b"GET".to_vec(),
                b"u8".to_vec(),
                b"0".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(out, RespFrame::Array(Some(vec![RespFrame::Integer(0)])));
    }

    #[test]
    fn bitfield_set_and_get_u8() {
        let mut store = Store::new();
        // SET u8 at offset 0 to 200, should return old value (0)
        let out = dispatch_argv(
            &[
                b"BITFIELD".to_vec(),
                b"bf".to_vec(),
                b"SET".to_vec(),
                b"u8".to_vec(),
                b"0".to_vec(),
                b"200".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(out, RespFrame::Array(Some(vec![RespFrame::Integer(0)])));

        // GET u8 at offset 0, should return 200
        let out = dispatch_argv(
            &[
                b"BITFIELD".to_vec(),
                b"bf".to_vec(),
                b"GET".to_vec(),
                b"u8".to_vec(),
                b"0".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(out, RespFrame::Array(Some(vec![RespFrame::Integer(200)])));
    }

    #[test]
    fn bitfield_set_and_get_i8() {
        let mut store = Store::new();
        // SET i8 at offset 0 to -100, returns old value (0)
        let out = dispatch_argv(
            &[
                b"BITFIELD".to_vec(),
                b"bf".to_vec(),
                b"SET".to_vec(),
                b"i8".to_vec(),
                b"0".to_vec(),
                b"-100".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(out, RespFrame::Array(Some(vec![RespFrame::Integer(0)])));

        // GET i8, should return -100
        let out = dispatch_argv(
            &[
                b"BITFIELD".to_vec(),
                b"bf".to_vec(),
                b"GET".to_vec(),
                b"i8".to_vec(),
                b"0".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(out, RespFrame::Array(Some(vec![RespFrame::Integer(-100)])));
    }

    #[test]
    fn bitfield_incrby() {
        let mut store = Store::new();
        // INCRBY u8 offset 0 by 10
        let out = dispatch_argv(
            &[
                b"BITFIELD".to_vec(),
                b"bf".to_vec(),
                b"INCRBY".to_vec(),
                b"u8".to_vec(),
                b"0".to_vec(),
                b"10".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(out, RespFrame::Array(Some(vec![RespFrame::Integer(10)])));

        // INCRBY again by 20
        let out = dispatch_argv(
            &[
                b"BITFIELD".to_vec(),
                b"bf".to_vec(),
                b"INCRBY".to_vec(),
                b"u8".to_vec(),
                b"0".to_vec(),
                b"20".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(out, RespFrame::Array(Some(vec![RespFrame::Integer(30)])));
    }

    #[test]
    fn bitfield_overflow_wrap() {
        let mut store = Store::new();
        // Set u8 to 250, then INCRBY 10 with WRAP (default) = wraps to 4
        dispatch_argv(
            &[
                b"BITFIELD".to_vec(),
                b"bf".to_vec(),
                b"SET".to_vec(),
                b"u8".to_vec(),
                b"0".to_vec(),
                b"250".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        let out = dispatch_argv(
            &[
                b"BITFIELD".to_vec(),
                b"bf".to_vec(),
                b"INCRBY".to_vec(),
                b"u8".to_vec(),
                b"0".to_vec(),
                b"10".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(out, RespFrame::Array(Some(vec![RespFrame::Integer(4)])));
    }

    #[test]
    fn bitfield_overflow_sat() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"BITFIELD".to_vec(),
                b"bf".to_vec(),
                b"SET".to_vec(),
                b"u8".to_vec(),
                b"0".to_vec(),
                b"250".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        let out = dispatch_argv(
            &[
                b"BITFIELD".to_vec(),
                b"bf".to_vec(),
                b"OVERFLOW".to_vec(),
                b"SAT".to_vec(),
                b"INCRBY".to_vec(),
                b"u8".to_vec(),
                b"0".to_vec(),
                b"10".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(out, RespFrame::Array(Some(vec![RespFrame::Integer(255)])));
    }

    #[test]
    fn bitfield_overflow_fail() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"BITFIELD".to_vec(),
                b"bf".to_vec(),
                b"SET".to_vec(),
                b"u8".to_vec(),
                b"0".to_vec(),
                b"250".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        let out = dispatch_argv(
            &[
                b"BITFIELD".to_vec(),
                b"bf".to_vec(),
                b"OVERFLOW".to_vec(),
                b"FAIL".to_vec(),
                b"INCRBY".to_vec(),
                b"u8".to_vec(),
                b"0".to_vec(),
                b"10".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        // FAIL returns nil and doesn't modify
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![RespFrame::BulkString(None)]))
        );

        // Verify value unchanged
        let out = dispatch_argv(
            &[
                b"BITFIELD".to_vec(),
                b"bf".to_vec(),
                b"GET".to_vec(),
                b"u8".to_vec(),
                b"0".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(out, RespFrame::Array(Some(vec![RespFrame::Integer(250)])));
    }

    #[test]
    fn bitfield_hash_offset() {
        let mut store = Store::new();
        // SET u8 at #1 (= bit offset 8) to 42
        dispatch_argv(
            &[
                b"BITFIELD".to_vec(),
                b"bf".to_vec(),
                b"SET".to_vec(),
                b"u8".to_vec(),
                b"#1".to_vec(),
                b"42".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        let out = dispatch_argv(
            &[
                b"BITFIELD".to_vec(),
                b"bf".to_vec(),
                b"GET".to_vec(),
                b"u8".to_vec(),
                b"#1".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(out, RespFrame::Array(Some(vec![RespFrame::Integer(42)])));
        // #0 should still be 0
        let out = dispatch_argv(
            &[
                b"BITFIELD".to_vec(),
                b"bf".to_vec(),
                b"GET".to_vec(),
                b"u8".to_vec(),
                b"#0".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(out, RespFrame::Array(Some(vec![RespFrame::Integer(0)])));
    }

    #[test]
    fn bitfield_multiple_ops() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[
                b"BITFIELD".to_vec(),
                b"bf".to_vec(),
                b"SET".to_vec(),
                b"u8".to_vec(),
                b"0".to_vec(),
                b"100".to_vec(),
                b"SET".to_vec(),
                b"u8".to_vec(),
                b"8".to_vec(),
                b"200".to_vec(),
                b"GET".to_vec(),
                b"u8".to_vec(),
                b"0".to_vec(),
                b"GET".to_vec(),
                b"u8".to_vec(),
                b"8".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![
                RespFrame::Integer(0),   // old value of first SET
                RespFrame::Integer(0),   // old value of second SET
                RespFrame::Integer(100), // GET first byte
                RespFrame::Integer(200), // GET second byte
            ]))
        );
    }

    #[test]
    fn bitfield_u16() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"BITFIELD".to_vec(),
                b"bf".to_vec(),
                b"SET".to_vec(),
                b"u16".to_vec(),
                b"0".to_vec(),
                b"1000".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        let out = dispatch_argv(
            &[
                b"BITFIELD".to_vec(),
                b"bf".to_vec(),
                b"GET".to_vec(),
                b"u16".to_vec(),
                b"0".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(out, RespFrame::Array(Some(vec![RespFrame::Integer(1000)])));
    }

    // ── XACK tests ──────────────────────────────────────────────────

    #[test]
    fn xack_basic() {
        let mut store = Store::new();
        // Create stream and consumer group
        dispatch_argv(
            &[
                b"XADD".to_vec(),
                b"mystream".to_vec(),
                b"1-0".to_vec(),
                b"f".to_vec(),
                b"v".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        dispatch_argv(
            &[
                b"XGROUP".to_vec(),
                b"CREATE".to_vec(),
                b"mystream".to_vec(),
                b"grp".to_vec(),
                b"0-0".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        // Read to create pending entry
        dispatch_argv(
            &[
                b"XREADGROUP".to_vec(),
                b"GROUP".to_vec(),
                b"grp".to_vec(),
                b"consumer1".to_vec(),
                b"COUNT".to_vec(),
                b"10".to_vec(),
                b"STREAMS".to_vec(),
                b"mystream".to_vec(),
                b">".to_vec(),
            ],
            &mut store,
            1,
        )
        .unwrap();
        // ACK the message
        let out = dispatch_argv(
            &[
                b"XACK".to_vec(),
                b"mystream".to_vec(),
                b"grp".to_vec(),
                b"1-0".to_vec(),
            ],
            &mut store,
            2,
        )
        .unwrap();
        assert_eq!(out, RespFrame::Integer(1));
    }

    #[test]
    fn xack_nonexistent_id() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"XADD".to_vec(),
                b"s".to_vec(),
                b"1-0".to_vec(),
                b"f".to_vec(),
                b"v".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        dispatch_argv(
            &[
                b"XGROUP".to_vec(),
                b"CREATE".to_vec(),
                b"s".to_vec(),
                b"g".to_vec(),
                b"0-0".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        let out = dispatch_argv(
            &[
                b"XACK".to_vec(),
                b"s".to_vec(),
                b"g".to_vec(),
                b"99-99".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(out, RespFrame::Integer(0));
    }

    #[test]
    fn xack_no_stream() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[
                b"XACK".to_vec(),
                b"missing".to_vec(),
                b"g".to_vec(),
                b"1-0".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(out, RespFrame::Integer(0));
    }

    // ── XSETID tests ─────────────────────────────────────────────────

    #[test]
    fn xsetid_on_stream() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"XADD".to_vec(),
                b"s".to_vec(),
                b"1-0".to_vec(),
                b"f".to_vec(),
                b"v".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        let out = dispatch_argv(
            &[b"XSETID".to_vec(), b"s".to_vec(), b"5-0".to_vec()],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(out, RespFrame::SimpleString("OK".to_string()));
    }

    #[test]
    fn xsetid_missing_key() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[b"XSETID".to_vec(), b"missing".to_vec(), b"1-0".to_vec()],
            &mut store,
            0,
        )
        .unwrap();
        // Returns error when key doesn't exist
        match out {
            RespFrame::Error(_) => {}
            other => panic!("expected error, got {other:?}"),
        }
    }

    // ── LOLWUT test ─────────────────────────────────────────────────

    #[test]
    fn lolwut_returns_version() {
        let mut store = Store::new();
        let out = dispatch_argv(&[b"LOLWUT".to_vec()], &mut store, 0).unwrap();
        match out {
            RespFrame::BulkString(Some(data)) => {
                let text = String::from_utf8_lossy(&data);
                assert!(text.contains("FrankenRedis"));
            }
            other => panic!("expected bulk string, got {other:?}"),
        }
    }

    // ── WAITAOF test ────────────────────────────────────────────────

    #[test]
    fn waitaof_standalone() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[
                b"WAITAOF".to_vec(),
                b"0".to_vec(),
                b"0".to_vec(),
                b"0".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![RespFrame::Integer(0), RespFrame::Integer(0),]))
        );
    }

    // ── CLUSTER tests ───────────────────────────────────────────────

    #[test]
    fn cluster_info_standalone() {
        let mut store = Store::new();
        let out = dispatch_argv(&[b"CLUSTER".to_vec(), b"INFO".to_vec()], &mut store, 0).unwrap();
        match out {
            RespFrame::BulkString(Some(data)) => {
                let text = String::from_utf8_lossy(&data);
                assert!(text.contains("cluster_enabled:0"));
            }
            other => panic!("expected bulk string, got {other:?}"),
        }
    }

    #[test]
    fn cluster_myid() {
        let mut store = Store::new();
        let out = dispatch_argv(&[b"CLUSTER".to_vec(), b"MYID".to_vec()], &mut store, 0).unwrap();
        match out {
            RespFrame::BulkString(Some(_)) => {}
            other => panic!("expected bulk string, got {other:?}"),
        }
    }

    #[test]
    fn cluster_slots_empty() {
        let mut store = Store::new();
        let out = dispatch_argv(&[b"CLUSTER".to_vec(), b"SLOTS".to_vec()], &mut store, 0).unwrap();
        assert_eq!(out, RespFrame::Array(Some(Vec::new())));
    }

    #[test]
    fn cluster_keyslot() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[b"CLUSTER".to_vec(), b"KEYSLOT".to_vec(), b"foo".to_vec()],
            &mut store,
            0,
        )
        .unwrap();
        // "foo" -> CRC16 slot 12182
        assert_eq!(out, RespFrame::Integer(12182));
    }

    #[test]
    fn cluster_keyslot_hashtag() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[
                b"CLUSTER".to_vec(),
                b"KEYSLOT".to_vec(),
                b"{user}.info".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        // {user} -> hash "user" -> slot 5474
        assert_eq!(out, RespFrame::Integer(5474));
    }

    // ── CLIENT KILL/PAUSE/UNPAUSE tests ─────────────────────────────

    #[test]
    fn client_kill_returns_zero() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[
                b"CLIENT".to_vec(),
                b"KILL".to_vec(),
                b"127.0.0.1:1234".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(out, RespFrame::Integer(0));
    }

    #[test]
    fn client_pause_unpause() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[b"CLIENT".to_vec(), b"PAUSE".to_vec(), b"1000".to_vec()],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(out, RespFrame::SimpleString("OK".to_string()));
        let out = dispatch_argv(&[b"CLIENT".to_vec(), b"UNPAUSE".to_vec()], &mut store, 0).unwrap();
        assert_eq!(out, RespFrame::SimpleString("OK".to_string()));
    }

    // ── REPLICAOF / SLAVEOF tests ───────────────────────────────────

    #[test]
    fn replicaof_no_one() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[b"REPLICAOF".to_vec(), b"NO".to_vec(), b"ONE".to_vec()],
            &mut store,
            0,
        )
        .unwrap();
        match out {
            RespFrame::SimpleString(s) => assert!(s.contains("OK")),
            other => panic!("expected simple string, got {other:?}"),
        }
    }

    #[test]
    fn slaveof_no_one() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[b"SLAVEOF".to_vec(), b"NO".to_vec(), b"ONE".to_vec()],
            &mut store,
            0,
        )
        .unwrap();
        match out {
            RespFrame::SimpleString(s) => assert!(s.contains("OK")),
            other => panic!("expected simple string, got {other:?}"),
        }
    }

    // ── FUNCTION tests ──────────────────────────────────────────────

    #[test]
    fn function_list_empty() {
        let mut store = Store::new();
        let out = dispatch_argv(&[b"FUNCTION".to_vec(), b"LIST".to_vec()], &mut store, 0).unwrap();
        assert_eq!(out, RespFrame::Array(Some(Vec::new())));
    }

    #[test]
    fn function_flush() {
        let mut store = Store::new();
        let out = dispatch_argv(&[b"FUNCTION".to_vec(), b"FLUSH".to_vec()], &mut store, 0).unwrap();
        assert_eq!(out, RespFrame::SimpleString("OK".to_string()));
    }

    // ── SHARD PUB/SUB tests ─────────────────────────────────────────

    #[test]
    fn spublish_no_subscribers() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[b"SPUBLISH".to_vec(), b"ch".to_vec(), b"msg".to_vec()],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(out, RespFrame::Integer(0));
    }

    #[test]
    fn ssubscribe_returns_confirmation() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[b"SSUBSCRIBE".to_vec(), b"mychannel".to_vec()],
            &mut store,
            0,
        )
        .unwrap();
        match out {
            RespFrame::Array(Some(arr)) => {
                assert_eq!(arr.len(), 3);
            }
            other => panic!("expected array, got {other:?}"),
        }
    }

    #[test]
    fn set_pxat_absolute_expiry() {
        let mut store = Store::new();
        let now = 1000;
        // SET key value PXAT 5000 — absolute expiry at 5000ms
        let out = dispatch_argv(
            &[
                b"SET".to_vec(),
                b"k".to_vec(),
                b"v".to_vec(),
                b"PXAT".to_vec(),
                b"5000".to_vec(),
            ],
            &mut store,
            now,
        )
        .unwrap();
        assert_eq!(out, RespFrame::SimpleString("OK".to_string()));
        // Key should exist at now=4999
        assert_eq!(store.get(b"k", 4999).unwrap(), Some(b"v".to_vec()));
        // Key should be expired at now=5000
        assert_eq!(store.get(b"k", 5000).unwrap(), None);
    }

    #[test]
    fn set_exat_absolute_expiry() {
        let mut store = Store::new();
        let now = 1000;
        // SET key value EXAT 10 — absolute expiry at 10s = 10000ms
        let out = dispatch_argv(
            &[
                b"SET".to_vec(),
                b"k".to_vec(),
                b"v".to_vec(),
                b"EXAT".to_vec(),
                b"10".to_vec(),
            ],
            &mut store,
            now,
        )
        .unwrap();
        assert_eq!(out, RespFrame::SimpleString("OK".to_string()));
        // Key should exist at now=9999
        assert_eq!(store.get(b"k", 9999).unwrap(), Some(b"v".to_vec()));
        // Key should be expired at now=10000
        assert_eq!(store.get(b"k", 10000).unwrap(), None);
    }

    #[test]
    fn set_keepttl_preserves_existing_expiry() {
        let mut store = Store::new();
        let now = 1000;
        // SET key value PX 4000 — expires at now+4000 = 5000
        dispatch_argv(
            &[
                b"SET".to_vec(),
                b"k".to_vec(),
                b"v1".to_vec(),
                b"PX".to_vec(),
                b"4000".to_vec(),
            ],
            &mut store,
            now,
        )
        .unwrap();
        // SET key newvalue KEEPTTL — should keep expiry at 5000
        let out = dispatch_argv(
            &[
                b"SET".to_vec(),
                b"k".to_vec(),
                b"v2".to_vec(),
                b"KEEPTTL".to_vec(),
            ],
            &mut store,
            now,
        )
        .unwrap();
        assert_eq!(out, RespFrame::SimpleString("OK".to_string()));
        // Value should be updated
        assert_eq!(store.get(b"k", now).unwrap(), Some(b"v2".to_vec()));
        // Original TTL should be preserved — still alive at 4999
        assert_eq!(store.get(b"k", 4999).unwrap(), Some(b"v2".to_vec()));
        // Expired at 5000
        assert_eq!(store.get(b"k", 5000).unwrap(), None);
    }

    #[test]
    fn set_keepttl_no_prior_ttl() {
        let mut store = Store::new();
        // SET key value (no TTL)
        dispatch_argv(
            &[b"SET".to_vec(), b"k".to_vec(), b"v1".to_vec()],
            &mut store,
            0,
        )
        .unwrap();
        // SET key newvalue KEEPTTL — no prior TTL, so no expiry
        dispatch_argv(
            &[
                b"SET".to_vec(),
                b"k".to_vec(),
                b"v2".to_vec(),
                b"KEEPTTL".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        // Should still exist far in the future
        assert_eq!(store.get(b"k", 999_999_999).unwrap(), Some(b"v2".to_vec()));
    }

    #[test]
    fn set_conflicting_expiry_modes_rejected() {
        let mut store = Store::new();
        // EX + PX → syntax error
        let result = dispatch_argv(
            &[
                b"SET".to_vec(),
                b"k".to_vec(),
                b"v".to_vec(),
                b"EX".to_vec(),
                b"10".to_vec(),
                b"PX".to_vec(),
                b"10000".to_vec(),
            ],
            &mut store,
            0,
        );
        assert!(result.is_err());
        // KEEPTTL + EXAT → syntax error
        let result = dispatch_argv(
            &[
                b"SET".to_vec(),
                b"k".to_vec(),
                b"v".to_vec(),
                b"KEEPTTL".to_vec(),
                b"EXAT".to_vec(),
                b"10".to_vec(),
            ],
            &mut store,
            0,
        );
        assert!(result.is_err());
    }

    #[test]
    fn set_pxat_with_get() {
        let mut store = Store::new();
        // Set initial value
        dispatch_argv(
            &[b"SET".to_vec(), b"k".to_vec(), b"old".to_vec()],
            &mut store,
            0,
        )
        .unwrap();
        // SET k new PXAT 5000 GET — should return old value
        let out = dispatch_argv(
            &[
                b"SET".to_vec(),
                b"k".to_vec(),
                b"new".to_vec(),
                b"PXAT".to_vec(),
                b"5000".to_vec(),
                b"GET".to_vec(),
            ],
            &mut store,
            1000,
        )
        .unwrap();
        assert_eq!(out, RespFrame::BulkString(Some(b"old".to_vec())));
        // New value should be accessible
        assert_eq!(store.get(b"k", 1000).unwrap(), Some(b"new".to_vec()));
    }

    #[test]
    fn sort_ro_returns_sorted_list() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"RPUSH".to_vec(),
                b"mylist".to_vec(),
                b"3".to_vec(),
                b"1".to_vec(),
                b"2".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        let out = dispatch_argv(&[b"SORT_RO".to_vec(), b"mylist".to_vec()], &mut store, 0).unwrap();
        match out {
            RespFrame::Array(Some(arr)) => {
                assert_eq!(arr.len(), 3);
                assert_eq!(arr[0], RespFrame::BulkString(Some(b"1".to_vec())));
                assert_eq!(arr[1], RespFrame::BulkString(Some(b"2".to_vec())));
                assert_eq!(arr[2], RespFrame::BulkString(Some(b"3".to_vec())));
            }
            other => panic!("expected array, got {other:?}"),
        }
    }

    #[test]
    fn readonly_returns_ok() {
        let mut store = Store::new();
        let out = dispatch_argv(&[b"READONLY".to_vec()], &mut store, 0).unwrap();
        assert_eq!(out, RespFrame::SimpleString("OK".to_string()));
    }

    #[test]
    fn readwrite_returns_ok() {
        let mut store = Store::new();
        let out = dispatch_argv(&[b"READWRITE".to_vec()], &mut store, 0).unwrap();
        assert_eq!(out, RespFrame::SimpleString("OK".to_string()));
    }

    #[test]
    fn client_tracking_returns_ok() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[b"CLIENT".to_vec(), b"TRACKING".to_vec(), b"ON".to_vec()],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(out, RespFrame::SimpleString("OK".to_string()));
    }

    #[test]
    fn client_caching_returns_ok() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[b"CLIENT".to_vec(), b"CACHING".to_vec(), b"YES".to_vec()],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(out, RespFrame::SimpleString("OK".to_string()));
    }

    #[test]
    fn srandmember_with_positive_count() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"SADD".to_vec(),
                b"myset".to_vec(),
                b"a".to_vec(),
                b"b".to_vec(),
                b"c".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        let out = dispatch_argv(
            &[b"SRANDMEMBER".to_vec(), b"myset".to_vec(), b"2".to_vec()],
            &mut store,
            0,
        )
        .unwrap();
        match out {
            RespFrame::Array(Some(arr)) => {
                assert_eq!(arr.len(), 2);
            }
            other => panic!("expected array, got {other:?}"),
        }
    }

    #[test]
    fn srandmember_with_negative_count() {
        let mut store = Store::new();
        dispatch_argv(
            &[b"SADD".to_vec(), b"myset".to_vec(), b"x".to_vec()],
            &mut store,
            0,
        )
        .unwrap();
        // Negative count: return 5 elements (repeats allowed)
        let out = dispatch_argv(
            &[b"SRANDMEMBER".to_vec(), b"myset".to_vec(), b"-5".to_vec()],
            &mut store,
            0,
        )
        .unwrap();
        match out {
            RespFrame::Array(Some(arr)) => {
                assert_eq!(arr.len(), 5);
                for elem in &arr {
                    assert_eq!(*elem, RespFrame::BulkString(Some(b"x".to_vec())));
                }
            }
            other => panic!("expected array, got {other:?}"),
        }
    }

    #[test]
    fn srandmember_count_empty_set() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[b"SRANDMEMBER".to_vec(), b"nonexist".to_vec(), b"3".to_vec()],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(out, RespFrame::Array(Some(vec![])));
    }

    #[test]
    fn zrangestore_by_rank() {
        let mut store = Store::new();
        // ZADD src: a=1, b=2, c=3
        dispatch_argv(
            &[
                b"ZADD".to_vec(),
                b"src".to_vec(),
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
        .unwrap();
        // ZRANGESTORE dst src 0 1 — copy first 2 elements
        let out = dispatch_argv(
            &[
                b"ZRANGESTORE".to_vec(),
                b"dst".to_vec(),
                b"src".to_vec(),
                b"0".to_vec(),
                b"1".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(out, RespFrame::Integer(2));
        // Verify dst has 2 members
        let card = dispatch_argv(&[b"ZCARD".to_vec(), b"dst".to_vec()], &mut store, 0).unwrap();
        assert_eq!(card, RespFrame::Integer(2));
    }

    #[test]
    fn zrangestore_byscore() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"ZADD".to_vec(),
                b"src".to_vec(),
                b"1".to_vec(),
                b"a".to_vec(),
                b"2".to_vec(),
                b"b".to_vec(),
                b"3".to_vec(),
                b"c".to_vec(),
                b"4".to_vec(),
                b"d".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        // ZRANGESTORE dst src 2 3 BYSCORE
        let out = dispatch_argv(
            &[
                b"ZRANGESTORE".to_vec(),
                b"dst".to_vec(),
                b"src".to_vec(),
                b"2".to_vec(),
                b"3".to_vec(),
                b"BYSCORE".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(out, RespFrame::Integer(2));
        // Verify: should contain b and c
        let members = dispatch_argv(
            &[
                b"ZRANGE".to_vec(),
                b"dst".to_vec(),
                b"0".to_vec(),
                b"-1".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        match members {
            RespFrame::Array(Some(arr)) => {
                assert_eq!(arr.len(), 2);
            }
            other => panic!("expected array, got {other:?}"),
        }
    }

    #[test]
    fn zrange_byscore() {
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
                b"4".to_vec(),
                b"d".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        let out = dispatch_argv(
            &[
                b"ZRANGE".to_vec(),
                b"zs".to_vec(),
                b"2".to_vec(),
                b"3".to_vec(),
                b"BYSCORE".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        match out {
            RespFrame::Array(Some(arr)) => {
                assert_eq!(arr.len(), 2);
                assert_eq!(arr[0], RespFrame::BulkString(Some(b"b".to_vec())));
                assert_eq!(arr[1], RespFrame::BulkString(Some(b"c".to_vec())));
            }
            other => panic!("expected array, got {other:?}"),
        }
    }

    #[test]
    fn zrange_byscore_rev() {
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
        .unwrap();
        // ZRANGE zs 3 1 BYSCORE REV — reversed score range
        let out = dispatch_argv(
            &[
                b"ZRANGE".to_vec(),
                b"zs".to_vec(),
                b"3".to_vec(),
                b"1".to_vec(),
                b"BYSCORE".to_vec(),
                b"REV".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        match out {
            RespFrame::Array(Some(arr)) => {
                assert_eq!(arr.len(), 3);
                assert_eq!(arr[0], RespFrame::BulkString(Some(b"c".to_vec())));
                assert_eq!(arr[1], RespFrame::BulkString(Some(b"b".to_vec())));
                assert_eq!(arr[2], RespFrame::BulkString(Some(b"a".to_vec())));
            }
            other => panic!("expected array, got {other:?}"),
        }
    }

    #[test]
    fn zrange_byscore_limit() {
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
                b"4".to_vec(),
                b"d".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        // ZRANGE zs 1 4 BYSCORE LIMIT 1 2 — skip 1, take 2
        let out = dispatch_argv(
            &[
                b"ZRANGE".to_vec(),
                b"zs".to_vec(),
                b"1".to_vec(),
                b"4".to_vec(),
                b"BYSCORE".to_vec(),
                b"LIMIT".to_vec(),
                b"1".to_vec(),
                b"2".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        match out {
            RespFrame::Array(Some(arr)) => {
                assert_eq!(arr.len(), 2);
                assert_eq!(arr[0], RespFrame::BulkString(Some(b"b".to_vec())));
                assert_eq!(arr[1], RespFrame::BulkString(Some(b"c".to_vec())));
            }
            other => panic!("expected array, got {other:?}"),
        }
    }

    #[test]
    fn zrange_byscore_withscores() {
        let mut store = Store::new();
        dispatch_argv(
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
        .unwrap();
        let out = dispatch_argv(
            &[
                b"ZRANGE".to_vec(),
                b"zs".to_vec(),
                b"1".to_vec(),
                b"3".to_vec(),
                b"BYSCORE".to_vec(),
                b"WITHSCORES".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        match out {
            RespFrame::Array(Some(arr)) => {
                assert_eq!(arr.len(), 4); // 2 members * 2 (member + score)
                assert_eq!(arr[0], RespFrame::BulkString(Some(b"a".to_vec())));
                assert_eq!(arr[1], RespFrame::BulkString(Some(b"1.5".to_vec())));
                assert_eq!(arr[2], RespFrame::BulkString(Some(b"b".to_vec())));
                assert_eq!(arr[3], RespFrame::BulkString(Some(b"2.5".to_vec())));
            }
            other => panic!("expected array, got {other:?}"),
        }
    }

    #[test]
    fn zrange_rev_by_rank() {
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
        .unwrap();
        // ZRANGE zs 0 -1 REV — all elements reversed
        let out = dispatch_argv(
            &[
                b"ZRANGE".to_vec(),
                b"zs".to_vec(),
                b"0".to_vec(),
                b"-1".to_vec(),
                b"REV".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        match out {
            RespFrame::Array(Some(arr)) => {
                assert_eq!(arr.len(), 3);
                assert_eq!(arr[0], RespFrame::BulkString(Some(b"c".to_vec())));
                assert_eq!(arr[1], RespFrame::BulkString(Some(b"b".to_vec())));
                assert_eq!(arr[2], RespFrame::BulkString(Some(b"a".to_vec())));
            }
            other => panic!("expected array, got {other:?}"),
        }
    }

    #[test]
    fn zrangebyscore_withscores_and_limit() {
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
                b"4".to_vec(),
                b"d".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        // ZRANGEBYSCORE zs 1 4 WITHSCORES LIMIT 1 2
        let out = dispatch_argv(
            &[
                b"ZRANGEBYSCORE".to_vec(),
                b"zs".to_vec(),
                b"1".to_vec(),
                b"4".to_vec(),
                b"WITHSCORES".to_vec(),
                b"LIMIT".to_vec(),
                b"1".to_vec(),
                b"2".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        match out {
            RespFrame::Array(Some(arr)) => {
                // 2 members * 2 (member+score) = 4
                assert_eq!(arr.len(), 4);
                assert_eq!(arr[0], RespFrame::BulkString(Some(b"b".to_vec())));
                assert_eq!(arr[1], RespFrame::BulkString(Some(b"2".to_vec())));
                assert_eq!(arr[2], RespFrame::BulkString(Some(b"c".to_vec())));
                assert_eq!(arr[3], RespFrame::BulkString(Some(b"3".to_vec())));
            }
            other => panic!("expected array, got {other:?}"),
        }
    }

    #[test]
    fn zrevrangebyscore_withscores() {
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
        .unwrap();
        // ZREVRANGEBYSCORE zs 3 1 WITHSCORES
        let out = dispatch_argv(
            &[
                b"ZREVRANGEBYSCORE".to_vec(),
                b"zs".to_vec(),
                b"3".to_vec(),
                b"1".to_vec(),
                b"WITHSCORES".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        match out {
            RespFrame::Array(Some(arr)) => {
                assert_eq!(arr.len(), 6); // 3 members * 2
                assert_eq!(arr[0], RespFrame::BulkString(Some(b"c".to_vec())));
                assert_eq!(arr[1], RespFrame::BulkString(Some(b"3".to_vec())));
                assert_eq!(arr[4], RespFrame::BulkString(Some(b"a".to_vec())));
                assert_eq!(arr[5], RespFrame::BulkString(Some(b"1".to_vec())));
            }
            other => panic!("expected array, got {other:?}"),
        }
    }

    #[test]
    fn zrangebylex_with_limit() {
        let mut store = Store::new();
        // All same score for lex ordering
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
                b"0".to_vec(),
                b"e".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        // ZRANGEBYLEX zs [a [e LIMIT 1 2
        let out = dispatch_argv(
            &[
                b"ZRANGEBYLEX".to_vec(),
                b"zs".to_vec(),
                b"[a".to_vec(),
                b"[e".to_vec(),
                b"LIMIT".to_vec(),
                b"1".to_vec(),
                b"2".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        match out {
            RespFrame::Array(Some(arr)) => {
                assert_eq!(arr.len(), 2);
                assert_eq!(arr[0], RespFrame::BulkString(Some(b"b".to_vec())));
                assert_eq!(arr[1], RespFrame::BulkString(Some(b"c".to_vec())));
            }
            other => panic!("expected array, got {other:?}"),
        }
    }

    #[test]
    fn lpop_with_count() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"RPUSH".to_vec(),
                b"mylist".to_vec(),
                b"a".to_vec(),
                b"b".to_vec(),
                b"c".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        let out = dispatch_argv(
            &[b"LPOP".to_vec(), b"mylist".to_vec(), b"2".to_vec()],
            &mut store,
            0,
        )
        .unwrap();
        match out {
            RespFrame::Array(Some(arr)) => {
                assert_eq!(arr.len(), 2);
                assert_eq!(arr[0], RespFrame::BulkString(Some(b"a".to_vec())));
                assert_eq!(arr[1], RespFrame::BulkString(Some(b"b".to_vec())));
            }
            other => panic!("expected array, got {other:?}"),
        }
    }

    #[test]
    fn rpop_with_count() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"RPUSH".to_vec(),
                b"mylist".to_vec(),
                b"a".to_vec(),
                b"b".to_vec(),
                b"c".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        let out = dispatch_argv(
            &[b"RPOP".to_vec(), b"mylist".to_vec(), b"2".to_vec()],
            &mut store,
            0,
        )
        .unwrap();
        match out {
            RespFrame::Array(Some(arr)) => {
                assert_eq!(arr.len(), 2);
                assert_eq!(arr[0], RespFrame::BulkString(Some(b"c".to_vec())));
                assert_eq!(arr[1], RespFrame::BulkString(Some(b"b".to_vec())));
            }
            other => panic!("expected array, got {other:?}"),
        }
    }

    #[test]
    fn spop_with_count() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"SADD".to_vec(),
                b"s".to_vec(),
                b"a".to_vec(),
                b"b".to_vec(),
                b"c".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        let out = dispatch_argv(
            &[b"SPOP".to_vec(), b"s".to_vec(), b"2".to_vec()],
            &mut store,
            0,
        )
        .unwrap();
        match out {
            RespFrame::Array(Some(arr)) => {
                assert_eq!(arr.len(), 2);
            }
            other => panic!("expected array, got {other:?}"),
        }
        // Only 1 member left
        let card = dispatch_argv(&[b"SCARD".to_vec(), b"s".to_vec()], &mut store, 0).unwrap();
        assert_eq!(card, RespFrame::Integer(1));
    }

    #[test]
    fn zpopmin_with_count() {
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
        .unwrap();
        let out = dispatch_argv(
            &[b"ZPOPMIN".to_vec(), b"zs".to_vec(), b"2".to_vec()],
            &mut store,
            0,
        )
        .unwrap();
        match out {
            RespFrame::Array(Some(arr)) => {
                assert_eq!(arr.len(), 4); // 2 pairs
                assert_eq!(arr[0], RespFrame::BulkString(Some(b"a".to_vec())));
                assert_eq!(arr[1], RespFrame::BulkString(Some(b"1".to_vec())));
                assert_eq!(arr[2], RespFrame::BulkString(Some(b"b".to_vec())));
                assert_eq!(arr[3], RespFrame::BulkString(Some(b"2".to_vec())));
            }
            other => panic!("expected array, got {other:?}"),
        }
    }

    #[test]
    fn zpopmax_with_count() {
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
        .unwrap();
        let out = dispatch_argv(
            &[b"ZPOPMAX".to_vec(), b"zs".to_vec(), b"2".to_vec()],
            &mut store,
            0,
        )
        .unwrap();
        match out {
            RespFrame::Array(Some(arr)) => {
                assert_eq!(arr.len(), 4); // 2 pairs
                assert_eq!(arr[0], RespFrame::BulkString(Some(b"c".to_vec())));
                assert_eq!(arr[1], RespFrame::BulkString(Some(b"3".to_vec())));
                assert_eq!(arr[2], RespFrame::BulkString(Some(b"b".to_vec())));
                assert_eq!(arr[3], RespFrame::BulkString(Some(b"2".to_vec())));
            }
            other => panic!("expected array, got {other:?}"),
        }
    }

    #[test]
    fn zrangebyscore_exclusive_bounds() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"ZADD".to_vec(),
                b"z".to_vec(),
                b"1".to_vec(),
                b"a".to_vec(),
                b"2".to_vec(),
                b"b".to_vec(),
                b"3".to_vec(),
                b"c".to_vec(),
                b"4".to_vec(),
                b"d".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        // ZRANGEBYSCORE z (1 (4 — exclusive on both ends: only b(2) and c(3)
        let out = dispatch_argv(
            &[
                b"ZRANGEBYSCORE".to_vec(),
                b"z".to_vec(),
                b"(1".to_vec(),
                b"(4".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        match out {
            RespFrame::Array(Some(arr)) => {
                assert_eq!(arr.len(), 2);
                assert_eq!(arr[0], RespFrame::BulkString(Some(b"b".to_vec())));
                assert_eq!(arr[1], RespFrame::BulkString(Some(b"c".to_vec())));
            }
            other => panic!("expected array, got {other:?}"),
        }
    }

    #[test]
    fn zcount_exclusive_bounds() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"ZADD".to_vec(),
                b"z".to_vec(),
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
        .unwrap();
        // ZCOUNT z (1 3 — exclusive min: b(2) and c(3) = 2
        let out = dispatch_argv(
            &[
                b"ZCOUNT".to_vec(),
                b"z".to_vec(),
                b"(1".to_vec(),
                b"3".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(out, RespFrame::Integer(2));
        // ZCOUNT z 1 (3 — exclusive max: a(1) and b(2) = 2
        let out2 = dispatch_argv(
            &[
                b"ZCOUNT".to_vec(),
                b"z".to_vec(),
                b"1".to_vec(),
                b"(3".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(out2, RespFrame::Integer(2));
    }

    #[test]
    fn zrange_byscore_exclusive() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"ZADD".to_vec(),
                b"z".to_vec(),
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
        .unwrap();
        // ZRANGE z (1 (3 BYSCORE — only b(2)
        let out = dispatch_argv(
            &[
                b"ZRANGE".to_vec(),
                b"z".to_vec(),
                b"(1".to_vec(),
                b"(3".to_vec(),
                b"BYSCORE".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        match out {
            RespFrame::Array(Some(arr)) => {
                assert_eq!(arr.len(), 1);
                assert_eq!(arr[0], RespFrame::BulkString(Some(b"b".to_vec())));
            }
            other => panic!("expected array, got {other:?}"),
        }
    }

    #[test]
    fn zremrangebyscore_exclusive() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"ZADD".to_vec(),
                b"z".to_vec(),
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
        .unwrap();
        // ZREMRANGEBYSCORE z (1 (3 — removes only b(2)
        let out = dispatch_argv(
            &[
                b"ZREMRANGEBYSCORE".to_vec(),
                b"z".to_vec(),
                b"(1".to_vec(),
                b"(3".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(out, RespFrame::Integer(1));
        // Remaining: a(1) and c(3)
        let card = dispatch_argv(&[b"ZCARD".to_vec(), b"z".to_vec()], &mut store, 0).unwrap();
        assert_eq!(card, RespFrame::Integer(2));
    }

    #[test]
    fn lpop_count_nonexistent_returns_nil() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[b"LPOP".to_vec(), b"nokey".to_vec(), b"5".to_vec()],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(out, RespFrame::BulkString(None));
    }

    #[test]
    fn rpop_count_nonexistent_returns_nil() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[b"RPOP".to_vec(), b"nokey".to_vec(), b"5".to_vec()],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(out, RespFrame::BulkString(None));
    }

    #[test]
    fn scan_type_filter() {
        let mut store = Store::new();
        // Create keys of different types
        dispatch_argv(
            &[b"SET".to_vec(), b"str1".to_vec(), b"v".to_vec()],
            &mut store,
            0,
        )
        .unwrap();
        dispatch_argv(
            &[b"SADD".to_vec(), b"set1".to_vec(), b"m".to_vec()],
            &mut store,
            0,
        )
        .unwrap();
        dispatch_argv(
            &[b"RPUSH".to_vec(), b"list1".to_vec(), b"x".to_vec()],
            &mut store,
            0,
        )
        .unwrap();
        // SCAN 0 TYPE string — should only return str1
        let out = dispatch_argv(
            &[
                b"SCAN".to_vec(),
                b"0".to_vec(),
                b"TYPE".to_vec(),
                b"string".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        match out {
            RespFrame::Array(Some(arr)) => {
                assert_eq!(arr.len(), 2);
                if let RespFrame::Array(Some(keys)) = &arr[1] {
                    assert_eq!(keys.len(), 1);
                    assert_eq!(keys[0], RespFrame::BulkString(Some(b"str1".to_vec())));
                } else {
                    panic!("expected keys array");
                }
            }
            other => panic!("expected array, got {other:?}"),
        }
    }

    #[test]
    fn zrandmember_with_positive_count() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"ZADD".to_vec(),
                b"z".to_vec(),
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
        .unwrap();
        // ZRANDMEMBER z 2 — return 2 distinct members
        let out = dispatch_argv(
            &[b"ZRANDMEMBER".to_vec(), b"z".to_vec(), b"2".to_vec()],
            &mut store,
            0,
        )
        .unwrap();
        match out {
            RespFrame::Array(Some(arr)) => assert_eq!(arr.len(), 2),
            other => panic!("expected array, got {other:?}"),
        }
    }

    #[test]
    fn zrandmember_with_negative_count() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"ZADD".to_vec(),
                b"z".to_vec(),
                b"1".to_vec(),
                b"a".to_vec(),
                b"2".to_vec(),
                b"b".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        // ZRANDMEMBER z -5 — return 5 members with repeats
        let out = dispatch_argv(
            &[b"ZRANDMEMBER".to_vec(), b"z".to_vec(), b"-5".to_vec()],
            &mut store,
            0,
        )
        .unwrap();
        match out {
            RespFrame::Array(Some(arr)) => assert_eq!(arr.len(), 5),
            other => panic!("expected array, got {other:?}"),
        }
    }

    #[test]
    fn zrandmember_with_count_withscores() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"ZADD".to_vec(),
                b"z".to_vec(),
                b"1".to_vec(),
                b"a".to_vec(),
                b"2".to_vec(),
                b"b".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        // ZRANDMEMBER z 2 WITHSCORES — return pairs: member, score, member, score
        let out = dispatch_argv(
            &[
                b"ZRANDMEMBER".to_vec(),
                b"z".to_vec(),
                b"2".to_vec(),
                b"WITHSCORES".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        match out {
            RespFrame::Array(Some(arr)) => {
                // 2 members * 2 (member + score) = 4 elements
                assert_eq!(arr.len(), 4);
            }
            other => panic!("expected array, got {other:?}"),
        }
    }

    #[test]
    fn zrandmember_count_empty_set() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[b"ZRANDMEMBER".to_vec(), b"nokey".to_vec(), b"3".to_vec()],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(out, RespFrame::Array(Some(vec![])));
    }

    #[test]
    fn zrandmember_count_exceeds_cardinality() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"ZADD".to_vec(),
                b"z".to_vec(),
                b"1".to_vec(),
                b"a".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        // ZRANDMEMBER z 10 — only 1 member, return just that one
        let out = dispatch_argv(
            &[b"ZRANDMEMBER".to_vec(), b"z".to_vec(), b"10".to_vec()],
            &mut store,
            0,
        )
        .unwrap();
        match out {
            RespFrame::Array(Some(arr)) => assert_eq!(arr.len(), 1),
            other => panic!("expected array, got {other:?}"),
        }
    }

    #[test]
    fn hrandfield_with_positive_count() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"HSET".to_vec(),
                b"h".to_vec(),
                b"f1".to_vec(),
                b"v1".to_vec(),
                b"f2".to_vec(),
                b"v2".to_vec(),
                b"f3".to_vec(),
                b"v3".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        // HRANDFIELD h 2 — return 2 distinct fields
        let out = dispatch_argv(
            &[b"HRANDFIELD".to_vec(), b"h".to_vec(), b"2".to_vec()],
            &mut store,
            0,
        )
        .unwrap();
        match out {
            RespFrame::Array(Some(arr)) => assert_eq!(arr.len(), 2),
            other => panic!("expected array, got {other:?}"),
        }
    }

    #[test]
    fn hrandfield_with_negative_count() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"HSET".to_vec(),
                b"h".to_vec(),
                b"f1".to_vec(),
                b"v1".to_vec(),
                b"f2".to_vec(),
                b"v2".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        // HRANDFIELD h -5 — return 5 fields with repeats
        let out = dispatch_argv(
            &[b"HRANDFIELD".to_vec(), b"h".to_vec(), b"-5".to_vec()],
            &mut store,
            0,
        )
        .unwrap();
        match out {
            RespFrame::Array(Some(arr)) => assert_eq!(arr.len(), 5),
            other => panic!("expected array, got {other:?}"),
        }
    }

    #[test]
    fn hrandfield_with_count_withvalues() {
        let mut store = Store::new();
        dispatch_argv(
            &[
                b"HSET".to_vec(),
                b"h".to_vec(),
                b"f1".to_vec(),
                b"v1".to_vec(),
                b"f2".to_vec(),
                b"v2".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        // HRANDFIELD h 2 WITHVALUES — return pairs: field, value, field, value
        let out = dispatch_argv(
            &[
                b"HRANDFIELD".to_vec(),
                b"h".to_vec(),
                b"2".to_vec(),
                b"WITHVALUES".to_vec(),
            ],
            &mut store,
            0,
        )
        .unwrap();
        match out {
            RespFrame::Array(Some(arr)) => {
                // 2 fields * 2 (field + value) = 4 elements
                assert_eq!(arr.len(), 4);
            }
            other => panic!("expected array, got {other:?}"),
        }
    }

    #[test]
    fn hrandfield_count_empty_hash() {
        let mut store = Store::new();
        let out = dispatch_argv(
            &[b"HRANDFIELD".to_vec(), b"nokey".to_vec(), b"3".to_vec()],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(out, RespFrame::Array(Some(vec![])));
    }
}
