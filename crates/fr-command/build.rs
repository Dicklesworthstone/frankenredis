use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// Append `value` to `categories` if not already present. Order is
/// post-processed via `canonical_acl_category_order` before emission
/// to match upstream Redis 7.2.4's CMD_CATEGORY_* bitmap iteration
/// order in server.h. (frankenredis-4utdc)
fn push_unique(categories: &mut Vec<String>, value: String) {
    if !categories.iter().any(|existing| existing == &value) {
        categories.push(value);
    }
}

/// Upstream Redis 7.2 COMMAND INFO emits categories by walking the
/// CMD_CATEGORY_* bitmap defined in server.h. The bitmap has a fixed
/// enum order which is the de facto wire-format order for any client
/// snapshotting category lists. fr's build.rs must produce the same
/// order regardless of JSON declaration order or flag-derived push
/// order. List taken from server.h::CMD_CATEGORY_* on Redis 7.2.4.
const CANONICAL_ACL_CATEGORY_ORDER: &[&str] = &[
    "keyspace",
    "read",
    "write",
    "set",
    "sortedset",
    "list",
    "hash",
    "string",
    "bitmap",
    "hyperloglog",
    "geo",
    "stream",
    "pubsub",
    "admin",
    "fast",
    "slow",
    "blocking",
    "dangerous",
    "connection",
    "transaction",
    "scripting",
];

fn sort_canonical(categories: &mut [String]) {
    categories.sort_by_key(|cat| {
        CANONICAL_ACL_CATEGORY_ORDER
            .iter()
            .position(|known| *known == cat.as_str())
            .unwrap_or(usize::MAX)
    });
}

fn main() -> Result<(), Box<dyn Error>> {
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "CARGO_MANIFEST_DIR is not set for build.rs",
        )
    })?);
    let commands_dir = manifest_dir
        .join("../..")
        .join("legacy_redis_code/redis/src/commands");

    println!("cargo:rerun-if-changed={}", commands_dir.display());

    // BTreeMap (ordered by command name) of insertion-ordered Vec<String>
    // — preserves the JSON acl_categories declaration order then the
    // flag-derived suffix order so COMMAND INFO matches upstream.
    let mut entries = BTreeMap::<String, Vec<String>>::new();
    // Parallel BTreeMap of (summary, complexity, since) tuples for COMMAND
    // DOCS. Optional strings (None for missing fields) so command_docs_entry
    // can omit fields the upstream JSON didn't declare. (frankenredis-f39s3)
    let mut docs_meta = BTreeMap::<String, (Option<String>, Option<String>, Option<String>)>::new();
    // Parallel BTreeMap of history entries: each is a Vec<(version,
    // message)>. Empty vec means upstream didn't declare any history
    // entries for the command, in which case command_docs_entry omits
    // the field entirely (matching upstream t_string.c::
    // commandDocsCommand). (frankenredis-az2a4)
    let mut docs_history = BTreeMap::<String, Vec<(String, String)>>::new();
    // (frankenredis-qsl84) Deprecation/syscmd metadata harvested from
    // doc_flags + deprecated_since + replaced_by in the upstream JSON.
    // Tuple is (doc_flags lowercase, deprecated_since, replaced_by);
    // an entry only lands here when at least one of those fields is
    // present, so the consumer can binary_search and skip on miss.
    let mut docs_deprecation =
        BTreeMap::<String, (Vec<String>, Option<String>, Option<String>)>::new();
    // (frankenredis-f7670) Set of command names whose upstream JSON
    // declares an `arguments` field. Container commands (CLUSTER,
    // CONFIG, CLIENT, DEBUG, MEMORY, OBJECT, SCRIPT, FUNCTION, ACL,
    // LATENCY, SLOWLOG, MODULE, COMMAND, PUBSUB, XGROUP, XINFO,
    // SENTINEL) and no-arg commands (BGSAVE, BGREWRITEAOF, RANDOMKEY,
    // ROLE, ...) lack args; the consumer uses absence-from-this-set as
    // the signal to skip the fallback arg-fabrication.
    let mut docs_has_args = std::collections::BTreeSet::<String>::new();
    // (frankenredis-50j77) Collect per-command arg trees verbatim from
    // the JSON. Embedded as a single JSON blob; lib.rs parses once
    // (OnceLock) and converts to RespFrame on demand.
    let mut docs_arg_trees = BTreeMap::<String, serde_json::Value>::new();
    // (CrimsonHawk) The Redis command JSON (legacy_redis_code/redis/src/commands) is
    // gitignored (clean-room/licensing boundary) and not synced to remote rch workers, so
    // fr-command — hence fr-runtime/fr-server — cannot build remotely, blocking per-crate
    // benching of those crates. With FR_ALLOW_STUB_COMMANDS set, fall back to EMPTY ACL-CAT
    // / COMMAND-DOCS tables so the crate COMPILES for REMOTE-BUILD/BENCH ONLY (ACL CAT and
    // COMMAND DOCS are degraded to empty — NOT for production binaries). The DEFAULT (env
    // unset) preserves the hard-fail, so a production build with the JSON dir missing still
    // fails loudly rather than silently shipping wrong ACL categories.
    let command_paths = match command_json_paths(&commands_dir) {
        Ok(paths) => paths,
        Err(err) => {
            if std::env::var_os("FR_ALLOW_STUB_COMMANDS").is_some() {
                println!(
                    "cargo:warning=fr-command: {err}; FR_ALLOW_STUB_COMMANDS set -> EMPTY ACL/COMMAND-DOCS tables (degraded; remote-build/bench only, NOT production)"
                );
                Vec::new()
            } else {
                return Err(err.into());
            }
        }
    };
    for path in command_paths {
        println!("cargo:rerun-if-changed={}", path.display());
        let raw = fs::read_to_string(&path).map_err(|err| {
            io::Error::new(
                err.kind(),
                format!(
                    "failed to read Redis command metadata {}: {err}",
                    path.display()
                ),
            )
        })?;
        let value: Value = serde_json::from_str(&raw).map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "failed to parse Redis command metadata {}: {err}",
                    path.display()
                ),
            )
        })?;
        let object = value.as_object().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "top-level command metadata is not an object: {}",
                    path.display()
                ),
            )
        })?;

        for (name, metadata) in object {
            let flags = string_array(metadata.get("command_flags"));
            if flags.contains(&"ONLY_SENTINEL") {
                continue;
            }

            let command_name = metadata
                .get("container")
                .and_then(Value::as_str)
                .map(|container| format!("{container}|{name}"))
                .unwrap_or_else(|| name.clone())
                .to_ascii_lowercase();

            let categories = entries.entry(command_name.clone()).or_default();
            for category in string_array(metadata.get("acl_categories")) {
                push_unique(categories, category.to_ascii_lowercase());
            }

            if flags.contains(&"WRITE") {
                push_unique(categories, "write".to_string());
            }
            if flags.contains(&"READONLY") && !categories.iter().any(|c| c == "scripting") {
                push_unique(categories, "read".to_string());
            }
            if flags.contains(&"ADMIN") {
                push_unique(categories, "admin".to_string());
                push_unique(categories, "dangerous".to_string());
            }
            if flags.contains(&"PUBSUB") {
                push_unique(categories, "pubsub".to_string());
            }
            if flags.contains(&"FAST") {
                push_unique(categories, "fast".to_string());
            }
            if flags.contains(&"BLOCKING") {
                push_unique(categories, "blocking".to_string());
            }
            if !categories.iter().any(|c| c == "fast") {
                push_unique(categories, "slow".to_string());
            }

            // Harvest COMMAND DOCS metadata from the same JSON. Each
            // field is optional — upstream omits them for some
            // commands, and we mirror that. (frankenredis-f39s3)
            let summary = metadata
                .get("summary")
                .and_then(Value::as_str)
                .map(str::to_string);
            let complexity = metadata
                .get("complexity")
                .and_then(Value::as_str)
                .map(str::to_string);
            let since = metadata
                .get("since")
                .and_then(Value::as_str)
                .map(str::to_string);
            docs_meta.insert(command_name.clone(), (summary, complexity, since));

            // History is a JSON array of [version, message] 2-tuples
            // — pairs of strings. Skip the entry entirely when absent
            // or empty so the generated table doesn't carry useless
            // rows. (frankenredis-az2a4)
            let history_entries: Vec<(String, String)> = metadata
                .get("history")
                .and_then(Value::as_array)
                .map(|array| {
                    array
                        .iter()
                        .filter_map(|item| {
                            let pair = item.as_array()?;
                            if pair.len() != 2 {
                                return None;
                            }
                            let version = pair[0].as_str()?.to_string();
                            let message = pair[1].as_str()?.to_string();
                            Some((version, message))
                        })
                        .collect()
                })
                .unwrap_or_default();
            if !history_entries.is_empty() {
                docs_history.insert(command_name.clone(), history_entries);
            }

            // (frankenredis-qsl84) doc_flags is an array of upper-case
            // tokens like "DEPRECATED" / "SYSCMD" — vendored emits them
            // lower-case in COMMAND DOCS replies. deprecated_since and
            // replaced_by are simple strings.
            let doc_flags: Vec<String> = string_array(metadata.get("doc_flags"))
                .into_iter()
                .map(|f| f.to_ascii_lowercase())
                .collect();
            let deprecated_since = metadata
                .get("deprecated_since")
                .and_then(Value::as_str)
                .map(str::to_string);
            let replaced_by = metadata
                .get("replaced_by")
                .and_then(Value::as_str)
                .map(str::to_string);
            if !doc_flags.is_empty() || deprecated_since.is_some() || replaced_by.is_some() {
                docs_deprecation.insert(
                    command_name.clone(),
                    (doc_flags, deprecated_since, replaced_by),
                );
            }

            // (frankenredis-f7670) Mark commands with declared args so
            // the consumer can skip fallback fabrication for containers
            // and no-arg commands.
            if metadata
                .get("arguments")
                .and_then(Value::as_array)
                .is_some_and(|arr| !arr.is_empty())
            {
                docs_has_args.insert(command_name.clone());
            }

            // (frankenredis-50j77) Capture the raw arguments JSON for
            // every command that has it. lib.rs parses this blob lazily
            // and converts to RespFrame, so the consumer doesn't need
            // hand-written arg trees for each subcommand.
            if let Some(args) = metadata.get("arguments").and_then(Value::as_array)
                && !args.is_empty()
            {
                docs_arg_trees.insert(command_name, Value::Array(args.clone()));
            }
        }
    }

    let mut out = String::from("const UPSTREAM_ACL_CATEGORY_ENTRIES: &[(&str, &[&str])] = &[\n");
    for (command, mut categories) in entries {
        // Sort to upstream's CMD_CATEGORY_* bitmap order before emitting
        // so COMMAND INFO matches byte-for-byte. (frankenredis-4utdc)
        sort_canonical(&mut categories);
        out.push_str("    (\"");
        out.push_str(&escape_rust_string(&command));
        out.push_str("\", &[");
        for category in categories {
            out.push('"');
            out.push_str(&escape_rust_string(&category));
            out.push_str("\", ");
        }
        out.push_str("]),\n");
    }
    out.push_str("];\n");

    // Emit the parallel COMMAND DOCS metadata table. Each tuple is
    // (name, summary, complexity, since); empty strings stand in for
    // None and are filtered out at lookup time so command_docs_entry
    // only emits fields the upstream JSON declared. (frankenredis-f39s3)
    out.push_str("const UPSTREAM_COMMAND_DOCS_META: &[(&str, &str, &str, &str)] = &[\n");
    for (command, (summary, complexity, since)) in &docs_meta {
        out.push_str("    (\"");
        out.push_str(&escape_rust_string(command));
        out.push_str("\", \"");
        if let Some(s) = summary.as_deref() {
            out.push_str(&escape_rust_string(s));
        }
        out.push_str("\", \"");
        if let Some(s) = complexity.as_deref() {
            out.push_str(&escape_rust_string(s));
        }
        out.push_str("\", \"");
        if let Some(s) = since.as_deref() {
            out.push_str(&escape_rust_string(s));
        }
        out.push_str("\"),\n");
    }
    out.push_str("];\n");

    // Emit the COMMAND DOCS history table. Each row is (name, &[(
    // version, message)]). Only commands with at least one history
    // entry appear; the consumer falls through to omitting the field
    // when binary_search misses. (frankenredis-az2a4)
    out.push_str("const UPSTREAM_COMMAND_DOCS_HISTORY: &[(&str, &[(&str, &str)])] = &[\n");
    for (command, history) in &docs_history {
        out.push_str("    (\"");
        out.push_str(&escape_rust_string(command));
        out.push_str("\", &[");
        for (version, message) in history {
            out.push_str("(\"");
            out.push_str(&escape_rust_string(version));
            out.push_str("\", \"");
            out.push_str(&escape_rust_string(message));
            out.push_str("\"), ");
        }
        out.push_str("]),\n");
    }
    out.push_str("];\n");

    // (frankenredis-qsl84) Emit the COMMAND DOCS deprecation table.
    // Each row is (name, &[doc_flag,...], deprecated_since, replaced_by).
    // Empty strings stand in for None; the consumer skips empty values.
    // (frankenredis-f7670) Sorted list of command names whose upstream
    // JSON declared an arguments array. Consumed by command_docs_arguments
    // to suppress the arity-derived fallback for containers and no-arg
    // commands.
    out.push_str("const UPSTREAM_COMMAND_DOCS_HAS_ARGS: &[&str] = &[\n");
    for command in &docs_has_args {
        out.push_str("    \"");
        out.push_str(&escape_rust_string(command));
        out.push_str("\",\n");
    }
    out.push_str("];\n");

    out.push_str("const UPSTREAM_COMMAND_DOCS_DEPRECATION: &[(&str, &[&str], &str, &str)] = &[\n");
    for (command, (doc_flags, deprecated_since, replaced_by)) in &docs_deprecation {
        out.push_str("    (\"");
        out.push_str(&escape_rust_string(command));
        out.push_str("\", &[");
        for flag in doc_flags {
            out.push('"');
            out.push_str(&escape_rust_string(flag));
            out.push_str("\", ");
        }
        out.push_str("], \"");
        if let Some(s) = deprecated_since.as_deref() {
            out.push_str(&escape_rust_string(s));
        }
        out.push_str("\", \"");
        if let Some(s) = replaced_by.as_deref() {
            out.push_str(&escape_rust_string(s));
        }
        out.push_str("\"),\n");
    }
    out.push_str("];\n");

    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "OUT_DIR is not set for build.rs")
    })?);
    fs::write(out_dir.join("acl_categories.rs"), out)?;

    // (frankenredis-50j77) Emit the harvested per-command arg trees as
    // a single JSON blob. lib.rs picks the blob up via include_str! and
    // lazily parses it on first access. Layout is { name: [argnode...] }.
    let trees_json = serde_json::to_string(&docs_arg_trees).map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("failed to serialize docs_arg_trees: {err}"),
        )
    })?;
    fs::write(out_dir.join("docs_arg_trees.json"), trees_json)?;

    Ok(())
}

fn command_json_paths(commands_dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut paths: Vec<PathBuf> = fs::read_dir(commands_dir)
        .map_err(|err| {
            io::Error::new(
                err.kind(),
                format!(
                    "failed to read Redis commands dir {}: {err}",
                    commands_dir.display()
                ),
            )
        })?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<io::Result<Vec<_>>>()?
        .into_iter()
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();
    Ok(paths)
}

fn string_array(value: Option<&Value>) -> Vec<&str> {
    value
        .and_then(Value::as_array)
        .map(|array| array.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

fn escape_rust_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}
