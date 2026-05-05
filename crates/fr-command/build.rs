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
    for path in command_json_paths(&commands_dir)? {
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

            let categories = entries.entry(command_name).or_default();
            for category in string_array(metadata.get("acl_categories")) {
                push_unique(categories, category.to_ascii_lowercase());
            }

            if flags.contains(&"WRITE") {
                push_unique(categories, "write".to_string());
            }
            if flags.contains(&"READONLY")
                && !categories.iter().any(|c| c == "scripting")
            {
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

    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "OUT_DIR is not set for build.rs")
    })?);
    fs::write(out_dir.join("acl_categories.rs"), out)?;
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
