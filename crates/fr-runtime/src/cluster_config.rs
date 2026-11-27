//! Durable local Redis Cluster identity and slot ownership.
//!
//! This implements the single-node subset of Redis 7.2's nodes.conf format.
//! Peer, replica and in-flight migration records are rejected rather than lost
//! when a configuration is loaded and subsequently saved by this server.

use std::{
    collections::BTreeSet,
    fmt::Write as _,
    fs::{File, OpenOptions, TryLockError},
    io::{self, Read, Write as _},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use fr_store::Store;

const MAX_CONFIG_BYTES: u64 = 2 * 1024 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ClusterConfig {
    node_id: String,
    shard_id: String,
    current_epoch: u64,
    config_epoch: u64,
    pub(super) last_vote_epoch: u64,
    slots: BTreeSet<u16>,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn node_id(value: &str) -> io::Result<String> {
    if value.len() != 40 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid(
            "cluster node and shard IDs must contain 40 hexadecimal digits",
        ));
    }
    Ok(value.to_ascii_lowercase())
}

fn unsigned(value: &str) -> io::Result<u64> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid("invalid unsigned integer in cluster configuration"));
    }
    value
        .parse()
        .map_err(|_| invalid("cluster configuration integer overflow"))
}

fn slot(value: &str) -> io::Result<u16> {
    let parsed = unsigned(value)?;
    if parsed >= 16_384 {
        return Err(invalid("cluster hash slot is outside 0..16383"));
    }
    Ok(parsed as u16)
}

impl ClusterConfig {
    pub(super) fn from_store(store: &Store, last_vote_epoch: u64) -> Self {
        Self {
            node_id: store.cluster_node_id.clone(),
            shard_id: store.cluster_shard_id.clone(),
            current_epoch: store.cluster_current_epoch,
            config_epoch: store.cluster_my_config_epoch,
            last_vote_epoch,
            slots: store.cluster_assigned_slots.clone(),
        }
    }

    pub(super) fn apply(&self, store: &mut Store) {
        store.cluster_node_id.clone_from(&self.node_id);
        store.cluster_shard_id.clone_from(&self.shard_id);
        store.cluster_current_epoch = self.current_epoch;
        store.cluster_my_config_epoch = self.config_epoch;
        store.cluster_assigned_slots.clone_from(&self.slots);
    }

    pub(super) fn parse(input: &str) -> io::Result<Self> {
        let mut local = None;
        let mut current_epoch = None;
        let mut last_vote_epoch = None;
        for line in input.lines().filter(|line| !line.trim().is_empty()) {
            let fields: Vec<_> = line.split_ascii_whitespace().collect();
            if fields
                .first()
                .is_some_and(|field| field.eq_ignore_ascii_case("vars"))
            {
                if fields.len().is_multiple_of(2) {
                    return Err(invalid("cluster vars must be name/value pairs"));
                }
                for pair in fields[1..].chunks_exact(2) {
                    let value = unsigned(pair[1])?;
                    let destination = if pair[0].eq_ignore_ascii_case("currentEpoch") {
                        &mut current_epoch
                    } else if pair[0].eq_ignore_ascii_case("lastVoteEpoch") {
                        &mut last_vote_epoch
                    } else {
                        return Err(invalid("unsupported cluster configuration variable"));
                    };
                    if destination.replace(value).is_some() {
                        return Err(invalid("duplicate cluster configuration variable"));
                    }
                }
                continue;
            }
            if fields.len() < 8 {
                return Err(invalid(
                    "cluster node record requires at least eight fields",
                ));
            }
            if local.is_some() {
                return Err(invalid(
                    "multi-node cluster configurations are not supported yet",
                ));
            }
            let id = node_id(fields[0])?;
            let flags: Vec<_> = fields[2].split(',').collect();
            if !flags.contains(&"myself")
                || !flags.contains(&"master")
                || flags
                    .iter()
                    .any(|flag| !matches!(*flag, "myself" | "master"))
                || fields[3] != "-"
            {
                return Err(invalid(
                    "only a healthy local primary cluster record is supported",
                ));
            }
            let address: Vec<_> = fields[1].split(',').collect();
            let (host, ports) = address[0]
                .rsplit_once(':')
                .ok_or_else(|| invalid("invalid cluster node address"))?;
            if !host.is_empty() && host.parse::<std::net::IpAddr>().is_err() {
                return Err(invalid("invalid cluster node IP address"));
            }
            let (port, bus_port) = ports.split_once('@').unwrap_or((ports, "0"));
            if unsigned(port)? > u64::from(u16::MAX) || unsigned(bus_port)? > u64::from(u16::MAX) {
                return Err(invalid("invalid cluster node port"));
            }
            let mut shard_id = None;
            for auxiliary in address.iter().skip(2) {
                let (name, value) = auxiliary
                    .split_once('=')
                    .ok_or_else(|| invalid("invalid cluster auxiliary field"))?;
                match name {
                    "shard-id" => {
                        if shard_id.replace(node_id(value)?).is_some() {
                            return Err(invalid("duplicate cluster shard ID"));
                        }
                    }
                    "tcp-port" | "tls-port" => {
                        if unsigned(value)? > u64::from(u16::MAX) {
                            return Err(invalid("invalid cluster auxiliary port"));
                        }
                    }
                    "nodename" => {}
                    _ => return Err(invalid("unsupported cluster auxiliary field")),
                }
            }
            unsigned(fields[4])?;
            unsigned(fields[5])?;
            let config_epoch = unsigned(fields[6])?;
            if !matches!(fields[7], "connected" | "disconnected") {
                return Err(invalid("invalid cluster link state"));
            }
            let mut slots = BTreeSet::new();
            for range in &fields[8..] {
                if range.starts_with('[') {
                    return Err(invalid(
                        "cluster slot migration configurations are not supported yet",
                    ));
                }
                let (first, last) = range.split_once('-').unwrap_or((range, range));
                let first = slot(first)?;
                let last = slot(last)?;
                if first > last {
                    return Err(invalid("cluster slot range is reversed"));
                }
                for value in first..=last {
                    if !slots.insert(value) {
                        return Err(invalid("cluster hash slot is assigned more than once"));
                    }
                }
            }
            local = Some(Self {
                node_id: id,
                // Older nodes.conf versions predate shard-id. Assign once and
                // persist it during initialization, as Redis 7.2 does.
                shard_id: shard_id.unwrap_or_else(fr_store::generate_run_id_hex),
                current_epoch: 0,
                config_epoch,
                last_vote_epoch: 0,
                slots,
            });
        }
        let mut local = local.ok_or_else(|| invalid("cluster configuration has no local node"))?;
        // Redis protects against a manually edited currentEpoch below configEpoch.
        local.current_epoch = current_epoch.unwrap_or(0).max(local.config_epoch);
        local.last_vote_epoch = last_vote_epoch.unwrap_or(0);
        if local.last_vote_epoch > local.current_epoch {
            return Err(invalid("cluster lastVoteEpoch exceeds currentEpoch"));
        }
        Ok(local)
    }

    pub(super) fn encode(&self, port: u16, bus_port: u16) -> String {
        let mut out = format!(
            "{} :{port}@{bus_port},,shard-id={} myself,master - 0 0 {} connected",
            self.node_id, self.shard_id, self.config_epoch
        );
        let mut slots = self.slots.iter().copied();
        if let Some(mut start) = slots.next() {
            let mut end = start;
            for value in slots.chain(std::iter::once(u16::MAX)) {
                if value == end + 1 {
                    end = value;
                    continue;
                }
                if start == end {
                    let _ = write!(out, " {start}");
                } else {
                    let _ = write!(out, " {start}-{end}");
                }
                start = value;
                end = value;
            }
        }
        let _ = write!(
            out,
            "\nvars currentEpoch {} lastVoteEpoch {}\n",
            self.current_epoch, self.last_vote_epoch
        );
        out
    }
}

/// The stable companion lock survives atomic replacement of nodes.conf. The
/// published file is also locked, interoperating with Redis's config-file lock.
#[derive(Debug)]
pub(super) struct ClusterConfigFile {
    path: PathBuf,
    _lease: File,
    published: File,
}

fn lock(file: &File) -> io::Result<()> {
    file.try_lock().map_err(|error| match error {
        TryLockError::WouldBlock => io::Error::new(
            io::ErrorKind::WouldBlock,
            "cluster configuration is already in use by another server",
        ),
        TryLockError::Error(error) => error,
    })
}

impl ClusterConfigFile {
    pub(super) fn open(path: &Path) -> io::Result<(Self, Option<ClusterConfig>)> {
        let filename = path
            .file_name()
            .ok_or_else(|| invalid("invalid cluster config filename"))?;
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let canonical = parent.canonicalize()?.join(filename);
        // Resolve an existing symlink before locking and replacing the target.
        let path = match canonical.canonicalize() {
            Ok(path) => path,
            Err(error) if error.kind() == io::ErrorKind::NotFound => canonical,
            Err(error) => return Err(error),
        };
        if let Ok(metadata) = path.metadata()
            && !metadata.is_file()
        {
            return Err(invalid("cluster configuration must be a regular file"));
        }
        let mut lease_path = path.as_os_str().to_os_string();
        lease_path.push(".lock");
        let lease = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(PathBuf::from(lease_path))?;
        lock(&lease)?;
        let published = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        lock(&published)?;
        if !published.metadata()?.is_file() {
            return Err(invalid("cluster configuration must be a regular file"));
        }
        let mut bytes = Vec::new();
        (&published)
            .take(MAX_CONFIG_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_CONFIG_BYTES {
            return Err(invalid("cluster configuration exceeds the size limit"));
        }
        let state = if bytes.is_empty() {
            None
        } else {
            let text = std::str::from_utf8(&bytes)
                .map_err(|_| invalid("cluster configuration is not UTF-8"))?;
            Some(ClusterConfig::parse(text)?)
        };
        Ok((
            Self {
                path,
                _lease: lease,
                published,
            },
            state,
        ))
    }

    pub(super) fn publish(&mut self, bytes: &[u8]) -> io::Result<()> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| invalid("cluster config has no parent directory"))?;
        let permissions = self.published.metadata()?.permissions();
        let mut staged = None;
        for _ in 0..128 {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let mut name = self.path.as_os_str().to_os_string();
            name.push(format!(".tmp-{}-{sequence}", std::process::id()));
            let path = PathBuf::from(name);
            match OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => {
                    staged = Some((path, file));
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        let (path, mut file) = staged.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "cannot reserve cluster configuration staging file",
            )
        })?;
        let result = (|| {
            lock(&file)?;
            file.write_all(bytes)?;
            file.set_permissions(permissions)?;
            file.sync_all()?;
            std::fs::rename(&path, &self.path)?;
            // Hold the new inode's lock even if directory fsync reports an error.
            self.published = file;
            #[cfg(unix)]
            File::open(parent)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&path);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::{ClusterConfig, ClusterConfigFile};
    use crate::{Runtime, RuntimePolicy};
    use fr_protocol::RespFrame;
    use std::path::{Path, PathBuf};

    const NODE: &str = "0123456789abcdef0123456789abcdef01234567";
    const SHARD: &str = "abcdef0123456789abcdef0123456789abcdef01";

    fn fixture(slots: &str) -> String {
        format!(
            "{NODE} 127.0.0.1:6379@16379,,shard-id={SHARD} myself,master - 0 0 7 connected {slots}\nvars currentEpoch 9 lastVoteEpoch 8\n"
        )
    }

    fn directory(label: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "fr_cluster_config_{label}_{}_{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn command(runtime: &mut Runtime, parts: &[&[u8]]) -> RespFrame {
        runtime.execute_frame(
            RespFrame::Array(Some(
                parts
                    .iter()
                    .map(|part| RespFrame::BulkString(Some(part.to_vec())))
                    .collect(),
            )),
            0,
        )
    }

    fn runtime(directory: &Path) -> Runtime {
        let mut runtime = Runtime::new(RuntimePolicy::default());
        runtime.set_server_port(6379);
        runtime.set_cluster_enabled(true);
        runtime.initialize_cluster_config(directory).unwrap();
        runtime
    }

    #[test]
    fn cluster_config_roundtrips_identity_epochs_and_noncontiguous_slots() {
        let state = ClusterConfig::parse(&fixture("0-2 42 16000-16383")).unwrap();
        let encoded = state.encode(6380, 16380);
        assert!(encoded.contains(":6380@16380,,shard-id="));
        assert!(encoded.contains("connected 0-2 42 16000-16383\n"));
        assert_eq!(ClusterConfig::parse(&encoded).unwrap(), state);
        assert_eq!(state.last_vote_epoch, 8);
    }

    #[test]
    fn cluster_config_rejects_corrupt_or_unsupported_topology() {
        for text in [
            fixture("0-2 2"),
            fixture("16384"),
            fixture("4-2"),
            fixture(&format!("[0->-{SHARD}]")),
            fixture("0").replace("myself,master", "myself,slave"),
            fixture("0").replace(NODE, "short"),
            fixture("0").replace("currentEpoch 9", "currentEpoch -1"),
            fixture("0").replace("currentEpoch 9", "currentEpoch 18446744073709551616"),
            fixture("0").replace("lastVoteEpoch 8", "lastVoteEpoch 10"),
            format!("{}{}", fixture("0"), fixture("1")),
            format!("{}vars currentEpoch 10\n", fixture("0")),
            "vars currentEpoch 1 lastVoteEpoch 0\n".to_string(),
        ] {
            assert!(ClusterConfig::parse(&text).is_err(), "accepted {text:?}");
        }
    }

    #[test]
    fn cluster_config_accepts_old_format_and_raises_current_epoch() {
        let state = ClusterConfig::parse(&format!("{NODE} :6379 myself,master - 0 0 7 connected 0-16383\nvars currentEpoch 3 lastVoteEpoch 2\n")).unwrap();
        assert_eq!(state.current_epoch, 7);
        assert_eq!(state.shard_id.len(), 40);
        assert_eq!(state.slots.len(), 16_384);
        assert_eq!(
            ClusterConfig::parse(&state.encode(6379, 16379)).unwrap(),
            state
        );
    }

    #[test]
    fn cluster_config_lock_survives_atomic_publication_and_releases_on_drop() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("fr_cluster_config_{}_{unique}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("nodes.conf");
        let (mut file, initial) = ClusterConfigFile::open(&path).unwrap();
        assert!(initial.is_none());
        let expected = ClusterConfig::parse(&fixture("1-3 9")).unwrap();
        file.publish(expected.encode(6379, 16379).as_bytes())
            .unwrap();
        assert_eq!(
            ClusterConfigFile::open(&path).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        file.publish(expected.encode(6380, 16380).as_bytes())
            .unwrap();
        assert_eq!(
            ClusterConfigFile::open(&path).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        drop(file);
        let (_reopened, restored) = ClusterConfigFile::open(&path).unwrap();
        assert_eq!(restored.unwrap(), expected);
    }

    #[test]
    fn cluster_mutations_and_explicit_save_survive_runtime_restart() {
        let directory = directory("restart");
        let mut first = runtime(&directory);
        let identity = command(&mut first, &[b"CLUSTER", b"MYID"]);
        let shard = command(&mut first, &[b"CLUSTER", b"MYSHARDID"]);
        assert_eq!(
            command(&mut first, &[b"CLUSTER", b"ADDSLOTSRANGE", b"0", b"16383"]),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            command(&mut first, &[b"CLUSTER", b"SET-CONFIG-EPOCH", b"17"]),
            RespFrame::SimpleString("OK".to_string())
        );
        // Automatic persistence must be complete without an explicit SAVECONFIG.
        drop(first);
        let mut second = runtime(&directory);
        assert_eq!(command(&mut second, &[b"CLUSTER", b"MYID"]), identity);
        assert_eq!(command(&mut second, &[b"CLUSTER", b"MYSHARDID"]), shard);
        assert_eq!(second.server.store.cluster_assigned_slots.len(), 16_384);
        assert_eq!(second.server.store.cluster_current_epoch, 17);
        assert_eq!(second.server.store.cluster_my_config_epoch, 17);
        assert_eq!(
            command(&mut second, &[b"CLUSTER", b"SAVECONFIG"]),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            command(&mut second, &[b"CLUSTER", b"RESET", b"HARD"]),
            RespFrame::SimpleString("OK".to_string())
        );
        let reset_identity = command(&mut second, &[b"CLUSTER", b"MYID"]);
        assert_ne!(reset_identity, identity);
        drop(second);
        let mut third = runtime(&directory);
        assert_eq!(command(&mut third, &[b"CLUSTER", b"MYID"]), reset_identity);
        assert!(third.server.store.cluster_assigned_slots.is_empty());
        assert_eq!(third.server.store.cluster_current_epoch, 0);
        assert_eq!(third.server.store.cluster_my_config_epoch, 0);
    }

    #[test]
    fn custom_config_preserves_imported_vote_epoch_and_hard_reset_clears_it() {
        let directory = directory("custom");
        let path = directory.join("local.conf");
        std::fs::write(&path, fixture("1-3 9")).unwrap();
        let mut runtime = Runtime::new(RuntimePolicy::default());
        runtime.set_server_port(6379);
        runtime.set_cluster_enabled(true);
        assert_eq!(
            runtime.execute_startup_config_directive(b"cluster-config-file", b"local.conf", 0),
            RespFrame::SimpleString("OK".to_string())
        );
        runtime.initialize_cluster_config(&directory).unwrap();
        assert_eq!(runtime.server.cluster_last_vote_epoch, 8);
        assert_eq!(
            command(&mut runtime, &[b"CLUSTER", b"DELSLOTS", b"9"]),
            RespFrame::SimpleString("OK".to_string())
        );
        let persisted = ClusterConfig::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(persisted.node_id, NODE);
        assert_eq!(persisted.shard_id, SHARD);
        assert_eq!(persisted.last_vote_epoch, 8);
        assert!(!persisted.slots.contains(&9));
        assert!(!directory.join("nodes.conf").exists());
        assert_eq!(
            command(&mut runtime, &[b"CLUSTER", b"RESET", b"HARD"]),
            RespFrame::SimpleString("OK".to_string())
        );
        let reset = ClusterConfig::parse(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(reset.last_vote_epoch, 0);
        assert_eq!(reset.current_epoch, 0);
    }

    #[test]
    fn invalid_configuration_does_not_replace_live_identity_or_disk_contents() {
        let directory = directory("invalid");
        let path = directory.join("nodes.conf");
        let invalid = fixture("1 1");
        std::fs::write(&path, &invalid).unwrap();
        let mut runtime = Runtime::new(RuntimePolicy::default());
        runtime.set_server_port(6379);
        runtime.set_cluster_enabled(true);
        let identity = runtime.server.store.cluster_node_id.clone();
        assert!(runtime.initialize_cluster_config(&directory).is_err());
        assert_eq!(runtime.server.store.cluster_node_id, identity);
        assert_eq!(std::fs::read_to_string(path).unwrap(), invalid);
    }

    #[test]
    fn saveconfig_reports_unbound_and_publication_failures() {
        let mut unbound = Runtime::new(RuntimePolicy::default());
        unbound.set_cluster_enabled(true);
        assert!(matches!(
            command(&mut unbound, &[b"CLUSTER", b"SAVECONFIG"]),
            RespFrame::Error(_)
        ));
        let directory = directory("save_failure");
        let mut runtime = runtime(&directory);
        let path = directory.join("nodes.conf");
        let saved = directory.join("saved.conf");
        let before = std::fs::read(&path).unwrap();
        std::fs::rename(&path, &saved).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(matches!(
            command(&mut runtime, &[b"CLUSTER", b"SAVECONFIG"]),
            RespFrame::Error(_)
        ));
        assert!(matches!(
            command(&mut runtime, &[b"CLUSTER", b"ADDSLOTS", b"42"]),
            RespFrame::Error(_)
        ));
        assert!(runtime.server.store.cluster_assigned_slots.contains(&42));
        assert_eq!(std::fs::read(saved).unwrap(), before);
    }
}
