#![forbid(unsafe_code)]

use crate::SentinelState;
use fr_protocol::RespFrame;

pub fn dispatch_sentinel_command(
    state: &mut SentinelState,
    args: &[&[u8]],
) -> RespFrame {
    if args.is_empty() {
        return RespFrame::Error("ERR wrong number of arguments for 'sentinel' command".into());
    }

    let subcommand = String::from_utf8_lossy(args[0]).to_ascii_uppercase();
    match subcommand.as_str() {
        "MYID" => cmd_myid(state),
        "MASTERS" => cmd_masters(state),
        "MASTER" => cmd_master(state, &args[1..]),
        "REPLICAS" | "SLAVES" => cmd_replicas(state, &args[1..]),
        "SENTINELS" => cmd_sentinels(state, &args[1..]),
        "MONITOR" => cmd_monitor(state, &args[1..]),
        "REMOVE" => cmd_remove(state, &args[1..]),
        "SET" => cmd_set(state, &args[1..]),
        "RESET" => cmd_reset(state, &args[1..]),
        "GET-MASTER-ADDR-BY-NAME" => cmd_get_master_addr(state, &args[1..]),
        "CKQUORUM" => cmd_ckquorum(state, &args[1..]),
        "FLUSHCONFIG" => cmd_flushconfig(state),
        "FAILOVER" => cmd_failover(state, &args[1..]),
        "PENDING-SCRIPTS" => cmd_pending_scripts(state),
        "INFO-CACHE" => cmd_info_cache(state, &args[1..]),
        "DEBUG" => cmd_debug(state, &args[1..]),
        "HELP" => cmd_help(),
        _ => RespFrame::Error(format!(
            "ERR Unknown sentinel subcommand '{}'",
            subcommand
        )),
    }
}

fn cmd_myid(state: &SentinelState) -> RespFrame {
    RespFrame::BulkString(Some(state.myid_hex().into_bytes()))
}

fn cmd_masters(state: &SentinelState) -> RespFrame {
    let masters: Vec<RespFrame> = state
        .masters
        .values()
        .map(instance_to_info_array)
        .collect();
    RespFrame::Array(Some(masters))
}

fn cmd_master(state: &SentinelState, args: &[&[u8]]) -> RespFrame {
    if args.is_empty() {
        return RespFrame::Error("ERR wrong number of arguments for 'sentinel master' command".into());
    }
    let name = String::from_utf8_lossy(args[0]);
    match state.get_master(&name) {
        Some(master) => instance_to_info_array(master),
        None => RespFrame::Error(format!("ERR No such master with that name: {}", name)),
    }
}

fn cmd_replicas(state: &SentinelState, args: &[&[u8]]) -> RespFrame {
    if args.is_empty() {
        return RespFrame::Error("ERR wrong number of arguments for 'sentinel replicas' command".into());
    }
    let name = String::from_utf8_lossy(args[0]);
    match state.get_master(&name) {
        Some(master) => {
            let replicas: Vec<RespFrame> = master
                .slaves
                .values()
                .map(instance_to_info_array)
                .collect();
            RespFrame::Array(Some(replicas))
        }
        None => RespFrame::Error(format!("ERR No such master with that name: {}", name)),
    }
}

fn cmd_sentinels(state: &SentinelState, args: &[&[u8]]) -> RespFrame {
    if args.is_empty() {
        return RespFrame::Error("ERR wrong number of arguments for 'sentinel sentinels' command".into());
    }
    let name = String::from_utf8_lossy(args[0]);
    match state.get_master(&name) {
        Some(master) => {
            let sentinels: Vec<RespFrame> = master
                .sentinels
                .values()
                .map(instance_to_info_array)
                .collect();
            RespFrame::Array(Some(sentinels))
        }
        None => RespFrame::Error(format!("ERR No such master with that name: {}", name)),
    }
}

fn cmd_monitor(state: &mut SentinelState, args: &[&[u8]]) -> RespFrame {
    if args.len() < 4 {
        return RespFrame::Error(
            "ERR wrong number of arguments for 'sentinel monitor' command".into(),
        );
    }
    let name = String::from_utf8_lossy(args[0]);
    let ip = String::from_utf8_lossy(args[1]);
    let port: u16 = match String::from_utf8_lossy(args[2]).parse() {
        Ok(p) => p,
        Err(_) => return RespFrame::Error("ERR Invalid port number".into()),
    };
    let quorum: u32 = match String::from_utf8_lossy(args[3]).parse() {
        Ok(q) => q,
        Err(_) => return RespFrame::Error("ERR Invalid quorum number".into()),
    };

    match state.monitor(name.as_ref(), ip.as_ref(), port, quorum) {
        Ok(()) => RespFrame::SimpleString("OK".into()),
        Err(e) => RespFrame::Error(e.into()),
    }
}

fn cmd_remove(state: &mut SentinelState, args: &[&[u8]]) -> RespFrame {
    if args.is_empty() {
        return RespFrame::Error("ERR wrong number of arguments for 'sentinel remove' command".into());
    }
    let name = String::from_utf8_lossy(args[0]);
    match state.remove(&name) {
        Ok(()) => RespFrame::SimpleString("OK".into()),
        Err(e) => RespFrame::Error(e.into()),
    }
}

fn cmd_set(state: &mut SentinelState, args: &[&[u8]]) -> RespFrame {
    if args.len() < 3 || args.len() % 2 == 0 {
        return RespFrame::Error(
            "ERR wrong number of arguments for 'sentinel set' command".into(),
        );
    }
    let name = String::from_utf8_lossy(args[0]);
    let master = match state.get_master_mut(&name) {
        Some(m) => m,
        None => return RespFrame::Error(format!("ERR No such master with that name: {}", name)),
    };

    let mut i = 1;
    while i + 1 < args.len() {
        let option = String::from_utf8_lossy(args[i]).to_ascii_lowercase();
        let value = String::from_utf8_lossy(args[i + 1]);

        match option.as_str() {
            "down-after-milliseconds" => {
                master.down_after_period = value.parse().unwrap_or(master.down_after_period);
            }
            "failover-timeout" => {
                master.failover_timeout = value.parse().unwrap_or(master.failover_timeout);
            }
            "parallel-syncs" => {
                master.parallel_syncs = value.parse().unwrap_or(master.parallel_syncs);
            }
            "quorum" => {
                master.quorum = value.parse().unwrap_or(master.quorum);
            }
            "auth-pass" => {
                master.auth_pass = Some(value.into_owned());
            }
            "auth-user" => {
                master.auth_user = Some(value.into_owned());
            }
            _ => {
                return RespFrame::Error(format!("ERR Unknown option '{}'", option));
            }
        }
        i += 2;
    }
    RespFrame::SimpleString("OK".into())
}

fn cmd_reset(state: &mut SentinelState, args: &[&[u8]]) -> RespFrame {
    if args.is_empty() {
        return RespFrame::Error("ERR wrong number of arguments for 'sentinel reset' command".into());
    }
    let pattern = String::from_utf8_lossy(args[0]);
    let mut count = 0i64;

    let names_to_reset: Vec<String> = state
        .masters
        .keys()
        .filter(|name| glob_match(&pattern, name))
        .cloned()
        .collect();

    for name in names_to_reset {
        if let Some(master) = state.masters.get_mut(&name) {
            master.sentinels.clear();
            master.slaves.clear();
            count += 1;
        }
    }
    RespFrame::Integer(count)
}

fn cmd_get_master_addr(state: &SentinelState, args: &[&[u8]]) -> RespFrame {
    if args.is_empty() {
        return RespFrame::Error(
            "ERR wrong number of arguments for 'sentinel get-master-addr-by-name' command".into(),
        );
    }
    let name = String::from_utf8_lossy(args[0]);
    match state.get_master(&name) {
        Some(master) => RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(master.addr.hostname.clone().into_bytes())),
            RespFrame::BulkString(Some(master.addr.port.to_string().into_bytes())),
        ])),
        None => RespFrame::BulkString(None),
    }
}

fn cmd_ckquorum(state: &SentinelState, args: &[&[u8]]) -> RespFrame {
    if args.is_empty() {
        return RespFrame::Error(
            "ERR wrong number of arguments for 'sentinel ckquorum' command".into(),
        );
    }
    let name = String::from_utf8_lossy(args[0]);
    match state.get_master(&name) {
        Some(master) => {
            let sentinel_count = master.sentinels.len() as u32 + 1;
            if sentinel_count >= master.quorum {
                RespFrame::SimpleString(format!(
                    "OK {} usable Sentinels. Quorum and failover authorization is possible",
                    sentinel_count
                ))
            } else {
                RespFrame::Error(format!(
                    "NOQUORUM {} Sentinels known, {} needed for quorum",
                    sentinel_count, master.quorum
                ))
            }
        }
        None => RespFrame::Error(format!("ERR No such master with that name: {}", name)),
    }
}

fn cmd_flushconfig(_state: &SentinelState) -> RespFrame {
    RespFrame::SimpleString("OK".into())
}

fn cmd_failover(state: &mut SentinelState, args: &[&[u8]]) -> RespFrame {
    if args.is_empty() {
        return RespFrame::Error(
            "ERR wrong number of arguments for 'sentinel failover' command".into(),
        );
    }
    let name = String::from_utf8_lossy(args[0]);
    match state.get_master_mut(&name) {
        Some(master) => {
            use crate::{FailoverState, InstanceFlags};
            master.flags.insert(InstanceFlags::FORCE_FAILOVER);
            master.failover_state = FailoverState::WaitStart;
            RespFrame::SimpleString("OK".into())
        }
        None => RespFrame::Error(format!("ERR No such master with that name: {}", name)),
    }
}

fn cmd_pending_scripts(state: &SentinelState) -> RespFrame {
    let scripts: Vec<RespFrame> = state
        .scripts_queue
        .iter()
        .map(|script| {
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"path".to_vec())),
                RespFrame::BulkString(Some(script.path.clone().into_bytes())),
            ]))
        })
        .collect();
    RespFrame::Array(Some(scripts))
}

fn cmd_info_cache(_state: &SentinelState, args: &[&[u8]]) -> RespFrame {
    if args.is_empty() {
        return RespFrame::Error(
            "ERR wrong number of arguments for 'sentinel info-cache' command".into(),
        );
    }
    RespFrame::Array(Some(vec![]))
}

fn cmd_debug(_state: &SentinelState, _args: &[&[u8]]) -> RespFrame {
    RespFrame::Array(Some(vec![
        RespFrame::BulkString(Some(b"ping-period".to_vec())),
        RespFrame::Integer(crate::PING_PERIOD_MS as i64),
        RespFrame::BulkString(Some(b"info-period".to_vec())),
        RespFrame::Integer(crate::INFO_PERIOD_MS as i64),
        RespFrame::BulkString(Some(b"tilt-trigger".to_vec())),
        RespFrame::Integer(crate::TILT_TRIGGER_MS as i64),
        RespFrame::BulkString(Some(b"tilt-period".to_vec())),
        RespFrame::Integer(crate::TILT_PERIOD_MS as i64),
    ]))
}

fn cmd_help() -> RespFrame {
    let help = vec![
        "SENTINEL MYID",
        "SENTINEL MASTERS",
        "SENTINEL MASTER <name>",
        "SENTINEL REPLICAS <name>",
        "SENTINEL SENTINELS <name>",
        "SENTINEL MONITOR <name> <ip> <port> <quorum>",
        "SENTINEL REMOVE <name>",
        "SENTINEL SET <name> <option> <value> ...",
        "SENTINEL RESET <pattern>",
        "SENTINEL GET-MASTER-ADDR-BY-NAME <name>",
        "SENTINEL CKQUORUM <name>",
        "SENTINEL FLUSHCONFIG",
        "SENTINEL FAILOVER <name>",
        "SENTINEL PENDING-SCRIPTS",
        "SENTINEL INFO-CACHE <name>",
        "SENTINEL DEBUG [<param> <value> ...]",
    ];
    RespFrame::Array(Some(
        help.into_iter()
            .map(|s| RespFrame::BulkString(Some(s.as_bytes().to_vec())))
            .collect(),
    ))
}

fn instance_to_info_array(instance: &crate::SentinelRedisInstance) -> RespFrame {
    let mut pairs = vec![
        RespFrame::BulkString(Some(b"name".to_vec())),
        RespFrame::BulkString(Some(instance.name.clone().into_bytes())),
        RespFrame::BulkString(Some(b"ip".to_vec())),
        RespFrame::BulkString(Some(instance.addr.hostname.clone().into_bytes())),
        RespFrame::BulkString(Some(b"port".to_vec())),
        RespFrame::BulkString(Some(instance.addr.port.to_string().into_bytes())),
        RespFrame::BulkString(Some(b"flags".to_vec())),
        RespFrame::BulkString(Some(flags_to_string(&instance.flags).into_bytes())),
        RespFrame::BulkString(Some(b"quorum".to_vec())),
        RespFrame::BulkString(Some(instance.quorum.to_string().into_bytes())),
        RespFrame::BulkString(Some(b"down-after-milliseconds".to_vec())),
        RespFrame::BulkString(Some(instance.down_after_period.to_string().into_bytes())),
        RespFrame::BulkString(Some(b"failover-timeout".to_vec())),
        RespFrame::BulkString(Some(instance.failover_timeout.to_string().into_bytes())),
        RespFrame::BulkString(Some(b"parallel-syncs".to_vec())),
        RespFrame::BulkString(Some(instance.parallel_syncs.to_string().into_bytes())),
        RespFrame::BulkString(Some(b"num-slaves".to_vec())),
        RespFrame::BulkString(Some(instance.slaves.len().to_string().into_bytes())),
        RespFrame::BulkString(Some(b"num-other-sentinels".to_vec())),
        RespFrame::BulkString(Some(instance.sentinels.len().to_string().into_bytes())),
    ];

    if let Some(ref runid) = instance.runid {
        pairs.push(RespFrame::BulkString(Some(b"runid".to_vec())));
        pairs.push(RespFrame::BulkString(Some(runid.clone().into_bytes())));
    }

    RespFrame::Array(Some(pairs))
}

fn flags_to_string(flags: &crate::InstanceFlags) -> String {
    let mut parts = Vec::new();
    if flags.contains(crate::InstanceFlags::MASTER) {
        parts.push("master");
    }
    if flags.contains(crate::InstanceFlags::SLAVE) {
        parts.push("slave");
    }
    if flags.contains(crate::InstanceFlags::SENTINEL) {
        parts.push("sentinel");
    }
    if flags.contains(crate::InstanceFlags::S_DOWN) {
        parts.push("s_down");
    }
    if flags.contains(crate::InstanceFlags::O_DOWN) {
        parts.push("o_down");
    }
    if flags.contains(crate::InstanceFlags::MASTER_DOWN) {
        parts.push("master_down");
    }
    if flags.contains(crate::InstanceFlags::FAILOVER_IN_PROGRESS) {
        parts.push("failover_in_progress");
    }
    if parts.is_empty() {
        "none".to_string()
    } else {
        parts.join(",")
    }
}

fn glob_match(pattern: &str, text: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if pattern.starts_with('*') && pattern.ends_with('*') {
        let mid = &pattern[1..pattern.len() - 1];
        return text.contains(mid);
    }
    if pattern.starts_with('*') {
        return text.ends_with(&pattern[1..]);
    }
    if pattern.ends_with('*') {
        return text.starts_with(&pattern[..pattern.len() - 1]);
    }
    pattern == text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_myid() {
        let mut state = SentinelState::new();
        let result = dispatch_sentinel_command(&mut state, &[b"MYID"]);
        assert!(matches!(result, RespFrame::BulkString(Some(_))));
    }

    #[test]
    fn test_monitor_and_masters() {
        let mut state = SentinelState::new();
        let result = dispatch_sentinel_command(
            &mut state,
            &[b"MONITOR", b"mymaster", b"127.0.0.1", b"6379", b"2"],
        );
        assert!(matches!(result, RespFrame::SimpleString(_)));

        let result = dispatch_sentinel_command(&mut state, &[b"MASTERS"]);
        if let RespFrame::Array(Some(arr)) = result {
            assert_eq!(arr.len(), 1);
        } else {
            panic!("Expected array");
        }
    }

    #[test]
    fn test_get_master_addr() {
        let mut state = SentinelState::new();
        let _ = dispatch_sentinel_command(
            &mut state,
            &[b"MONITOR", b"mymaster", b"192.168.1.100", b"6379", b"2"],
        );

        let result = dispatch_sentinel_command(
            &mut state,
            &[b"GET-MASTER-ADDR-BY-NAME", b"mymaster"],
        );
        if let RespFrame::Array(Some(arr)) = result {
            assert_eq!(arr.len(), 2);
        } else {
            panic!("Expected array");
        }
    }

    #[test]
    fn test_set_options() {
        let mut state = SentinelState::new();
        let _ = dispatch_sentinel_command(
            &mut state,
            &[b"MONITOR", b"mymaster", b"127.0.0.1", b"6379", b"2"],
        );

        let result = dispatch_sentinel_command(
            &mut state,
            &[b"SET", b"mymaster", b"down-after-milliseconds", b"5000"],
        );
        assert!(matches!(result, RespFrame::SimpleString(_)));

        let master = state.get_master("mymaster").unwrap();
        assert_eq!(master.down_after_period, 5000);
    }

    #[test]
    fn test_failover() {
        let mut state = SentinelState::new();
        let _ = dispatch_sentinel_command(
            &mut state,
            &[b"MONITOR", b"mymaster", b"127.0.0.1", b"6379", b"2"],
        );

        let result = dispatch_sentinel_command(&mut state, &[b"FAILOVER", b"mymaster"]);
        assert!(matches!(result, RespFrame::SimpleString(_)));

        let master = state.get_master("mymaster").unwrap();
        assert!(master.flags.contains(crate::InstanceFlags::FORCE_FAILOVER));
        assert_eq!(master.failover_state, crate::FailoverState::WaitStart);
    }

    #[test]
    fn test_glob_match() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("my*", "mymaster"));
        assert!(!glob_match("my*", "yourmaster"));
        assert!(glob_match("*master", "mymaster"));
        assert!(glob_match("*mast*", "mymaster"));
        assert!(glob_match("mymaster", "mymaster"));
        assert!(!glob_match("mymaster", "mymaster2"));
    }

    #[test]
    fn test_help() {
        let mut state = SentinelState::new();
        let result = dispatch_sentinel_command(&mut state, &[b"HELP"]);
        if let RespFrame::Array(Some(arr)) = result {
            assert!(!arr.is_empty());
        } else {
            panic!("Expected array");
        }
    }
}
