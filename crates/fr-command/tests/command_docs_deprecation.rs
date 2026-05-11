use fr_command::dispatch_argv;
use fr_protocol::RespFrame;
use fr_store::Store;

// (frankenredis-qsl84) Pin COMMAND DOCS deprecation metadata. Upstream
// commandDocsCommand emits doc_flags + deprecated_since + replaced_by
// between complexity and history when the JSON declares them. fr now
// harvests these from legacy_redis_code/redis/src/commands/*.json at
// build time and emits them in the same order.

fn run(name: &str) -> RespFrame {
    let mut store = Store::new();
    dispatch_argv(
        &[b"COMMAND".to_vec(), b"DOCS".to_vec(), name.as_bytes().to_vec()],
        &mut store,
        0,
    )
    .expect("COMMAND DOCS should succeed")
}

fn kv(out: &RespFrame) -> &Vec<RespFrame> {
    let RespFrame::Array(Some(entries)) = out else {
        panic!("expected outer Array");
    };
    let RespFrame::Array(Some(kv)) = &entries[1] else {
        panic!("expected inner kv array");
    };
    kv
}

fn kv_field<'a>(kv: &'a [RespFrame], key: &str) -> Option<&'a RespFrame> {
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

fn key_index(kv: &[RespFrame], key: &str) -> Option<usize> {
    let mut i = 0;
    while i < kv.len() {
        if let RespFrame::BulkString(Some(k)) = &kv[i]
            && k.as_slice() == key.as_bytes()
        {
            return Some(i);
        }
        i += 2;
    }
    None
}

fn assert_bulk(frame: &RespFrame, expected: &str) {
    match frame {
        RespFrame::BulkString(Some(b)) => {
            assert_eq!(b.as_slice(), expected.as_bytes(), "expected {expected}")
        }
        other => panic!("expected BulkString({expected}), got {other:?}"),
    }
}

fn assert_deprecated(cmd: &str, since: &str, replaced_by: &str) {
    let out = run(cmd);
    let kv = kv(&out);

    let doc_flags = kv_field(kv, "doc_flags").expect("doc_flags must be emitted");
    let RespFrame::Array(Some(items)) = doc_flags else {
        panic!("doc_flags must be array");
    };
    assert_eq!(items.len(), 1);
    assert_eq!(items[0], RespFrame::SimpleString("deprecated".to_string()));

    assert_bulk(kv_field(kv, "deprecated_since").unwrap(), since);
    assert_bulk(kv_field(kv, "replaced_by").unwrap(), replaced_by);

    // (qsl84) Field order must be: complexity, doc_flags,
    // deprecated_since, replaced_by — matches upstream
    // commandDocsCommand emission order.
    let complexity_idx = key_index(kv, "complexity").expect("complexity present");
    let docflags_idx = key_index(kv, "doc_flags").unwrap();
    let depsince_idx = key_index(kv, "deprecated_since").unwrap();
    let replaced_idx = key_index(kv, "replaced_by").unwrap();
    assert!(complexity_idx < docflags_idx);
    assert!(docflags_idx < depsince_idx);
    assert!(depsince_idx < replaced_idx);
}

#[test]
fn command_docs_setnx_deprecation() {
    assert_deprecated("SETNX", "2.6.12", "`SET` with the `NX` argument");
}

#[test]
fn command_docs_substr_deprecation() {
    assert_deprecated("SUBSTR", "2.0.0", "`GETRANGE`");
}

#[test]
fn command_docs_getset_deprecation() {
    assert_deprecated("GETSET", "6.2.0", "`SET` with the `!GET` argument");
}

#[test]
fn command_docs_setex_deprecation() {
    assert_deprecated("SETEX", "2.6.12", "`SET` with the `EX` argument");
}

#[test]
fn command_docs_psetex_deprecation() {
    assert_deprecated("PSETEX", "2.6.12", "`SET` with the `PX` argument");
}

#[test]
fn command_docs_quit_deprecation() {
    assert_deprecated("QUIT", "7.2.0", "just closing the connection");
}

#[test]
fn command_docs_debug_syscmd_doc_flag() {
    // DEBUG carries the SYSCMD doc_flag (lowercased to "syscmd"), no
    // deprecated_since/replaced_by since it isn't deprecated. Pin the
    // doc_flags array shape and the absence of deprecation strings.
    let out = run("DEBUG");
    let kv = kv(&out);
    let doc_flags = kv_field(kv, "doc_flags").expect("doc_flags must be emitted");
    let RespFrame::Array(Some(items)) = doc_flags else {
        panic!("doc_flags must be array");
    };
    assert_eq!(items.len(), 1);
    assert_eq!(items[0], RespFrame::SimpleString("syscmd".to_string()));
    assert!(kv_field(kv, "deprecated_since").is_none());
    assert!(kv_field(kv, "replaced_by").is_none());
}

#[test]
fn command_docs_quit_omits_arguments_field() {
    // QUIT has no upstream args (cmd->args == NULL); fr should mirror
    // upstream and skip emitting the `arguments` key altogether.
    let out = run("QUIT");
    let kv = kv(&out);
    assert!(
        kv_field(kv, "arguments").is_none(),
        "QUIT must not emit arguments field"
    );
}

#[test]
fn command_docs_get_omits_deprecation_metadata() {
    // GET is not deprecated and not syscmd, so doc_flags / depsince /
    // replaced_by must all be absent. Guards against accidental
    // table-population mistakes.
    let out = run("GET");
    let kv = kv(&out);
    assert!(kv_field(kv, "doc_flags").is_none());
    assert!(kv_field(kv, "deprecated_since").is_none());
    assert!(kv_field(kv, "replaced_by").is_none());
}
