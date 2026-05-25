#!/usr/bin/env python3
"""
Redis 7.2.4 vs FrankenRedis command parity audit.
Canonical source: Redis 7.2.4 command set (241 base commands + subcommands).
"""

# Redis 7.2.4 SUBCOMMANDS (parent -> list of subcommands)
REDIS_724_SUBCOMMANDS = {
    "ACL": ["CAT", "DELUSER", "DRYRUN", "GENPASS", "GETUSER", "HELP", "LIST", "LOAD", "LOG", "SAVE", "SETUSER", "USERS", "WHOAMI"],
    "CLIENT": ["CACHING", "GETNAME", "GETREDIR", "HELP", "ID", "INFO", "KILL", "LIST", "NO-EVICT", "NO-TOUCH", "PAUSE", "REPLY", "SETINFO", "SETNAME", "TRACKING", "TRACKINGINFO", "UNBLOCK", "UNPAUSE"],
    "CLUSTER": ["ADDSLOTS", "ADDSLOTSRANGE", "BUMPEPOCH", "COUNT-FAILURE-REPORTS", "COUNTKEYSINSLOT", "DELSLOTS", "DELSLOTSRANGE", "FAILOVER", "FLUSHSLOTS", "FORGET", "GETKEYSINSLOT", "HELP", "INFO", "KEYSLOT", "LINKS", "MEET", "MYID", "MYSHARDID", "NODES", "REPLICAS", "REPLICATE", "RESET", "SAVECONFIG", "SET-CONFIG-EPOCH", "SETSLOT", "SHARDS", "SLAVES", "SLOTS"],
    "COMMAND": ["COUNT", "DOCS", "GETKEYS", "GETKEYSANDFLAGS", "HELP", "INFO", "LIST"],
    "CONFIG": ["GET", "HELP", "RESETSTAT", "REWRITE", "SET"],
    "DEBUG": [],  # DEBUG has many subcommands but they're internal/undocumented
    "FUNCTION": ["DELETE", "DUMP", "FLUSH", "HELP", "KILL", "LIST", "LOAD", "RESTORE", "STATS"],
    "LATENCY": ["DOCTOR", "GRAPH", "HELP", "HISTOGRAM", "HISTORY", "LATEST", "RESET"],
    "MEMORY": ["DOCTOR", "HELP", "MALLOC-SIZE", "MALLOC-STATS", "PURGE", "STATS", "USAGE"],
    "MODULE": ["HELP", "LIST", "LOAD", "LOADEX", "UNLOAD"],
    "OBJECT": ["ENCODING", "FREQ", "HELP", "IDLETIME", "REFCOUNT"],
    "PUBSUB": ["CHANNELS", "HELP", "NUMPAT", "NUMSUB", "SHARDCHANNELS", "SHARDNUMSUB"],
    "SCRIPT": ["DEBUG", "EXISTS", "FLUSH", "HELP", "KILL", "LOAD"],
    "SENTINEL": [],  # Many sentinel subcommands, separate concern
    "SLOWLOG": ["GET", "HELP", "LEN", "RESET"],
    "XGROUP": ["CREATE", "CREATECONSUMER", "DELCONSUMER", "DESTROY", "HELP", "SETID"],
    "XINFO": ["CONSUMERS", "GROUPS", "HELP", "STREAM"],
}

# Commands that are aliases or handled in fr-runtime
FR_RUNTIME_HANDLED = {
    "AUTH", "HELLO", "ACL", "MULTI", "EXEC", "DISCARD", "WATCH", "UNWATCH",
    "ASKING", "SYNC",  # Server/replication
}

FR_COMMAND_ALIASES = {
    "SLAVEOF": "REPLICAOF",
    "GEORADIUS_RO": "GEORADIUS",
    "GEORADIUSBYMEMBER_RO": "GEORADIUSBYMEMBER",
    "RESTORE-ASKING": "RESTORE",
}

def check_subcommand_in_file(parent: str, sub: str, filepath: str) -> bool:
    """Check if a subcommand is implemented by searching for its handler."""
    import subprocess
    # Search for the subcommand in the implementation
    result = subprocess.run(
        ["grep", "-qi", f'"{sub}"', filepath],
        capture_output=True
    )
    return result.returncode == 0

def main():
    import subprocess
    import os

    fr_command_path = "crates/fr-command/src/lib.rs"
    fr_runtime_path = "crates/fr-runtime/src/lib.rs"

    print("=== Redis 7.2.4 Subcommand Parity Audit ===\n")

    missing_subcommands = []

    for parent, subs in REDIS_724_SUBCOMMANDS.items():
        if not subs:
            continue

        for sub in subs:
            # Search in fr-command
            found = False
            result = subprocess.run(
                ["grep", "-qi", f'"{sub}"', fr_command_path],
                capture_output=True
            )
            if result.returncode == 0:
                found = True

            # Search in fr-runtime for ACL/CLIENT/etc
            if not found:
                result = subprocess.run(
                    ["grep", "-qi", f'"{sub}"', fr_runtime_path],
                    capture_output=True
                )
                if result.returncode == 0:
                    found = True

            if not found:
                missing_subcommands.append(f"{parent} {sub}")

    if missing_subcommands:
        print(f"POTENTIALLY MISSING SUBCOMMANDS ({len(missing_subcommands)}):")
        for cmd in sorted(missing_subcommands):
            print(f"  - {cmd}")
    else:
        print("All subcommands appear to be implemented!")

    # Also count total coverage
    total_subs = sum(len(subs) for subs in REDIS_724_SUBCOMMANDS.values())
    covered = total_subs - len(missing_subcommands)
    if total_subs > 0:
        pct = 100 * covered / total_subs
        print(f"\nSubcommand coverage: {covered}/{total_subs} = {pct:.1f}%")

if __name__ == "__main__":
    main()
