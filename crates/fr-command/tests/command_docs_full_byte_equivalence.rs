use fr_command::dispatch_argv;
use fr_protocol::RespFrame;
use fr_store::Store;

// (frankenredis-50j77) Pin that COMMAND DOCS arg trees are now harvested
// from the upstream JSON at build time and emitted as-is, mirroring
// upstream's generate-command-code.py behavior:
//   - name lowercased (Arg.__init__: self.name = desc["name"].lower())
//   - token uppercased (get_optional_desc_string force_uppercase=True)
//   - empty-string token (JSON `""`) becomes a 0-byte bulk reply
//     (Python `"%s" % "\"\""` → C string literal `""""` which the
//     compiler concatenates to the empty C string)
//   - deprecated_since per-arg field emitted between since and flags
//   - multiple_token flag added to the flags array when JSON declares it
//
// These tests pin the specific arg shapes most prone to regression
// rather than the full bulk byte-equivalent diff (which is exercised
// against vendored at probe time but isn't suitable for an offline
// unit test).

fn run(name: &str) -> RespFrame {
    let mut store = Store::new();
    dispatch_argv(
        &[
            b"COMMAND".to_vec(),
            b"DOCS".to_vec(),
            name.as_bytes().to_vec(),
        ],
        &mut store,
        0,
    )
    .expect("COMMAND DOCS should succeed")
}

fn outer_pairs(out: &RespFrame) -> &[RespFrame] {
    let RespFrame::Array(Some(pairs)) = out else {
        panic!("expected outer Array");
    };
    pairs
}

fn entry_kv(out: &RespFrame) -> &[RespFrame] {
    let pairs = outer_pairs(out);
    let RespFrame::Array(Some(kv)) = &pairs[1] else {
        panic!("expected inner Array");
    };
    kv
}

fn field<'a>(kv: &'a [RespFrame], key: &str) -> Option<&'a RespFrame> {
    let mut i = 0;
    while i + 1 < kv.len() {
        if let RespFrame::BulkString(Some(k)) = &kv[i]
            && k.as_slice() == key.as_bytes()
        {
            return Some(&kv[i + 1]);
        }
        i += 2;
    }
    None
}

fn arguments(out: &RespFrame) -> &Vec<RespFrame> {
    let RespFrame::Array(Some(arr)) = field(entry_kv(out), "arguments").unwrap() else {
        panic!("arguments must be array");
    };
    arr
}

fn bulk(frame: &RespFrame) -> &[u8] {
    let RespFrame::BulkString(Some(b)) = frame else {
        panic!("expected BulkString");
    };
    b
}

// Walk a Vec<RespFrame> tree and collect every BulkString as a String.
fn collect_bulk_strings(frame: &RespFrame, out: &mut Vec<String>) {
    match frame {
        RespFrame::BulkString(Some(b)) => {
            if let Ok(s) = std::str::from_utf8(b) {
                out.push(s.to_string());
            }
        }
        RespFrame::Array(Some(items)) => {
            for item in items {
                collect_bulk_strings(item, out);
            }
        }
        _ => {}
    }
}

#[test]
fn command_docs_client_kill_normal_token_uppercased() {
    // CLIENT KILL TYPE pure-tokens have JSON "token": "normal" (lower)
    // and "name": "normal" — upstream emits display_text=normal AND
    // token=NORMAL (force_uppercase). Pre-50j77, fr's converter passed
    // the token through unchanged so it landed as lowercase.
    let out = run("CLIENT");
    let mut strings = Vec::new();
    collect_bulk_strings(&out, &mut strings);
    assert!(
        strings.contains(&"NORMAL".to_string()),
        "CLIENT KILL TYPE must emit NORMAL token (uppercase)"
    );
    assert!(
        strings.contains(&"MASTER".to_string()),
        "CLIENT KILL TYPE must emit MASTER token (uppercase)"
    );
    assert!(
        strings.contains(&"REPLICA".to_string()),
        "CLIENT KILL TYPE must emit REPLICA token (uppercase)"
    );
}

#[test]
fn command_docs_client_tracking_bcast_name_lowercased() {
    // CLIENT TRACKING's BCAST/OPTIN/OPTOUT/NOLOOP pure-tokens have JSON
    // "name": "BCAST" (uppercase) but upstream lowercases the name in
    // its Arg parser. Emission must include display_text=bcast AND
    // token=BCAST (uppercase via force_uppercase).
    let out = run("CLIENT");
    let mut strings = Vec::new();
    collect_bulk_strings(&out, &mut strings);
    assert!(
        strings.contains(&"bcast".to_string()),
        "must contain lowercase bcast"
    );
    assert!(
        strings.contains(&"BCAST".to_string()),
        "must contain uppercase BCAST token"
    );
}

#[test]
fn command_docs_migrate_empty_string_token_is_zero_byte() {
    // MIGRATE's "empty-string" pure-token has JSON `"token": "\"\""`
    // (two-char string). Upstream's Python formatter wraps it in quotes
    // then C concatenates adjacent literals, yielding the empty C
    // string. fr's converter must therefore emit a 0-byte BulkString
    // for that token.
    let out = run("MIGRATE");
    let args = arguments(&out);
    fn find_empty_string(args: &[RespFrame]) -> Option<&Vec<RespFrame>> {
        for arg in args {
            let RespFrame::Array(Some(arg_items)) = arg else {
                continue;
            };
            let mut i = 0;
            while i + 1 < arg_items.len() {
                if let RespFrame::BulkString(Some(k)) = &arg_items[i]
                    && k.as_slice() == b"name"
                    && let RespFrame::BulkString(Some(v)) = &arg_items[i + 1]
                    && v.as_slice() == b"empty-string"
                {
                    return Some(arg_items);
                }
                i += 2;
            }
            if let Some(sub_args) = field(arg_items, "arguments") {
                let RespFrame::Array(Some(sub_list)) = sub_args else {
                    continue;
                };
                if let Some(inner) = find_empty_string(sub_list) {
                    return Some(inner);
                }
            }
        }
        None
    }
    let empty_arg = find_empty_string(args).expect("empty-string arg must exist");
    let RespFrame::BulkString(Some(tok)) = field(empty_arg, "token").unwrap() else {
        panic!("token must be BulkString");
    };
    assert!(
        tok.is_empty(),
        "empty-string token must be 0-byte bulk, got {tok:?}"
    );
}

#[test]
fn command_docs_module_loadex_carries_multiple_token_flag() {
    // MODULE LOADEX's `args` arg has JSON `multiple_token: true`.
    // Upstream emits "multiple_token" alongside "multiple" in the flags
    // array.
    let out = run("MODULE");
    let s = format!("{out:?}");
    assert!(
        s.contains("multiple_token"),
        "module|loadex args must emit multiple_token flag"
    );
}

#[test]
fn command_docs_cluster_addslots_has_real_arg_tree() {
    // Pre-50j77, fr's fallback emitted `arg1 string optional 1` for
    // every container subcommand. cluster|addslots's JSON arg tree is
    // a single key with multiple flag.
    let out = run("CLUSTER");
    let kv = entry_kv(&out);
    let RespFrame::Array(Some(subs)) = field(kv, "subcommands").unwrap() else {
        panic!("subcommands must be array");
    };
    let mut addslots_kv: Option<&Vec<RespFrame>> = None;
    let mut i = 0;
    while i + 1 < subs.len() {
        if bulk(&subs[i]) == b"cluster|addslots" {
            let RespFrame::Array(Some(kv)) = &subs[i + 1] else {
                panic!("subcommand body must be Array");
            };
            addslots_kv = Some(kv);
            break;
        }
        i += 2;
    }
    let kv = addslots_kv.expect("cluster|addslots must be present");
    let RespFrame::Array(Some(args)) = field(kv, "arguments").unwrap() else {
        panic!("arguments must be array");
    };
    assert_eq!(args.len(), 1);
    let RespFrame::Array(Some(arg0)) = &args[0] else {
        panic!("arg must be array");
    };
    assert_eq!(
        field(arg0, "name").unwrap(),
        &RespFrame::BulkString(Some(b"slot".to_vec()))
    );
    assert_eq!(
        field(arg0, "type").unwrap(),
        &RespFrame::BulkString(Some(b"integer".to_vec()))
    );
    let RespFrame::Array(Some(flags)) = field(arg0, "flags").unwrap() else {
        panic!("flags must be array");
    };
    assert_eq!(flags.len(), 1);
    assert_eq!(flags[0], RespFrame::SimpleString("multiple".to_string()));
}
