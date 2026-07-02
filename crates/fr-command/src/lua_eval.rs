// Minimal Lua 5.1 evaluator for Redis scripting.
//
// Supports: variables (local/global), arithmetic, string concat, comparisons,
// logical ops, if/elseif/else, numeric for, generic for (pairs/ipairs),
// while, repeat/until, tables, function calls/definitions, redis.call/pcall,
// KEYS/ARGV, and standard library functions.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::{Rc, Weak};

/// (CrimsonHawk) Fast, DoS-resistant hasher for the Lua interpreter's HOT maps —
/// table string-fields and the globals env, hashed on every field/global access.
/// The std default `RandomState` (SipHash) was ~9.5% of a redis.call-heavy EVAL
/// profile; this matches fr-store's keyspace hasher (`foldhash::quality`).
type LuaMap<K, V> = std::collections::HashMap<K, V, foldhash::quality::RandomState>;

/// (CrimsonHawk) Fast decimal i64 → owned ASCII bytes, byte-identical to
/// `format!("{}", v).into_bytes()` but without the core::fmt machinery. Used on
/// the redis.call integer-argument hot path. `unsigned_abs()` makes i64::MIN safe.
fn i64_to_ascii_bytes(v: i64) -> Vec<u8> {
    let mut buf = [0u8; 20]; // i64::MIN = "-9223372036854775808" = 20 chars
    let mut i = buf.len();
    let neg = v < 0;
    let mut u = v.unsigned_abs();
    loop {
        i -= 1;
        buf[i] = b'0' + (u % 10) as u8;
        u /= 10;
        if u == 0 {
            break;
        }
    }
    if neg {
        i -= 1;
        buf[i] = b'-';
    }
    buf[i..].to_vec()
}
use std::time::Instant;

use fr_protocol::RespFrame;
use fr_store::{SCRIPT_PROPAGATE_ALL, SCRIPT_PROPAGATE_AOF, SCRIPT_PROPAGATE_REPLICA, Store};

use crate::{CommandError, SCRIPT_NOSCRIPT_ERROR, dispatch_argv, parse_i64_arg};

// ── Lua cycle-breaking GC (frankenredis-qqq17) ──────────────────────────────
//
// Our Lua values are reference-counted (`Rc<RefCell<..>>`) with no tracing
// collector, so any value CYCLE leaks: the strong counts never reach zero even
// after the owning `LuaState` drops. Two shapes occur in practice and were both
// surfaced by `fuzz_lua_eval` under LeakSanitizer:
//   (a) a recursive `local function f() ... f ... end` — the closure's
//       `captured_env` upvalue cell holds an `Rc` to a `LuaValue::Function`
//       whose `captured_env` holds that same cell;
//   (b) a self-referential table `local t = {}; t.x = t` — the table inner
//       holds an `Rc` (via a contained `LuaValue::Table`) back to itself.
// Real Redis's Lua has a mark-sweep collector; we emulate the teardown half.
// Every allocation that can participate in a cycle — table inners and local
// upvalue cells — registers a `Weak` handle in a thread-local registry. At the
// end of each `eval_script` (success, error, OR panic-unwind, via the
// `LuaGcScope` Drop guard) we sweep the slice of the registry allocated by that
// eval and CLEAR each still-live object's contents: emptying a table inner /
// setting a cell to `Nil` severs the internal back-edge, the strong counts
// collapse to zero, and the memory is reclaimed. The sweep runs only after the
// script's return value has been serialized to an owned `RespFrame` and the
// `LuaState` has dropped, so live results and non-cyclic data are unaffected
// (their `Weak`s simply fail to upgrade). The registry holds only `Weak`s, so
// it never itself keeps a Lua object alive.

enum LuaGcHandle {
    Table(Weak<RefCell<LuaTableInner>>),
    Cell(Weak<RefCell<LuaValue>>),
}

thread_local! {
    static LUA_GC_REGISTRY: RefCell<Vec<LuaGcHandle>> = const { RefCell::new(Vec::new()) };
}

// (frankenredis-qqq17) Test-only live-`LuaTableInner` counter, used by the
// cycle-leak regression test to prove that a self-referential table is
// ACTUALLY reclaimed (count returns to baseline) rather than merely that the
// registry was truncated. Zero cost in non-test builds (cfg-gated out).
#[cfg(test)]
thread_local! {
    static LUA_TEST_LIVE_TABLES: std::cell::Cell<i64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn lua_test_live_tables() -> i64 {
    LUA_TEST_LIVE_TABLES.with(std::cell::Cell::get)
}

#[cfg(test)]
impl Drop for LuaTableInner {
    fn drop(&mut self) {
        if self.shared_template {
            return;
        }
        LUA_TEST_LIVE_TABLES.with(|c| c.set(c.get() - 1));
    }
}

/// Register a freshly-created table inner so the next eval-end sweep can break
/// any cycle it participates in.
fn lua_gc_register_table(inner: &Rc<RefCell<LuaTableInner>>) {
    LUA_GC_REGISTRY.with(|reg| {
        reg.borrow_mut()
            .push(LuaGcHandle::Table(Rc::downgrade(inner)))
    });
}

/// Register a freshly-created local/upvalue cell (the carrier of recursive
/// closure self-references).
fn lua_gc_register_cell(cell: &Rc<RefCell<LuaValue>>) {
    LUA_GC_REGISTRY.with(|reg| {
        reg.borrow_mut()
            .push(LuaGcHandle::Cell(Rc::downgrade(cell)))
    });
}

/// RAII guard scoping one `eval_script` invocation. Records the registry
/// high-water mark on entry; on drop (incl. panic unwind) it breaks every cycle
/// allocated since then and truncates the registry back to the mark, so the
/// registry length returns to its pre-eval baseline. Declared BEFORE the
/// `LuaState` in `eval_script` so the state (and its `Env`) drop first, leaving
/// only genuinely-leaked cycle islands for the sweep to clear. Nested/reentrant
/// evals each sweep only their own `[mark..]` range.
struct LuaGcScope {
    mark: usize,
}

impl LuaGcScope {
    fn enter() -> Self {
        let mark = LUA_GC_REGISTRY.with(|reg| reg.borrow().len());
        Self { mark }
    }
}

impl Drop for LuaGcScope {
    fn drop(&mut self) {
        LUA_GC_REGISTRY.with(|reg| {
            let mut reg = reg.borrow_mut();
            for handle in reg.iter().skip(self.mark) {
                match handle {
                    LuaGcHandle::Table(weak) => {
                        if let Some(inner) = weak.upgrade() {
                            // try_borrow_mut is defensive: post-teardown nothing
                            // should hold a live borrow, and a contended borrow
                            // must never panic during cleanup.
                            if let Ok(mut inner) = inner.try_borrow_mut() {
                                inner.array.clear();
                                inner.string_hash.clear();
                                inner.other_hash.clear();
                                inner.other_keys.clear();
                                inner.metatable = None;
                            }
                        }
                    }
                    LuaGcHandle::Cell(weak) => {
                        if let Some(cell) = weak.upgrade()
                            && let Ok(mut slot) = cell.try_borrow_mut()
                        {
                            *slot = LuaValue::Nil;
                        }
                    }
                }
            }
            reg.truncate(self.mark);
        });
    }
}

// ── Value type ──────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub enum LuaValue {
    Nil,
    Bool(bool),
    Number(f64),
    Str(Vec<u8>),
    Table(LuaTable),
    // (CrimsonHawk) Boxed: LuaFunc is 144 bytes; inline it made every LuaValue
    // (even a Number) 144 bytes, so every clone/move copied 144 bytes. Boxing
    // shrinks LuaValue to 32 bytes, speeding all value ops interpreter-wide.
    // Box (not Rc) preserves the prior deep-clone semantics.
    Function(Box<LuaFunc>),
    RustFunction(String), // name of built-in
    Userdata(LuaUserdata),
    Coroutine(LuaCoroutine),
    WrappedCoroutine(LuaCoroutine),
}

#[derive(Clone, Debug)]
pub enum LuaUserdata {
    CjsonNull,
    Proxy(LuaProxy),
}

#[derive(Clone, Debug)]
pub struct LuaProxy {
    identity: u64,
    metatable: Option<LuaTable>,
}

impl LuaProxy {
    fn new(metatable: Option<LuaTable>) -> Self {
        Self {
            identity: next_userdata_identity(),
            metatable,
        }
    }
}

fn next_userdata_identity() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Encode a dotted `major.minor.patch` version as upstream's
/// `REDIS_VERSION_NUM`: `(major << 16) | (minor << 8) | patch`. Missing or
/// non-numeric components are treated as 0, mirroring the C macro on a
/// well-formed version string. (frankenredis-luaver)
fn redis_version_num(version: &str) -> u32 {
    let mut parts = version.split('.');
    let major: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let minor: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let patch: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    (major << 16) | (minor << 8) | patch
}

impl std::hash::Hash for LuaUserdata {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        match self {
            LuaUserdata::CjsonNull => "cjson-null".hash(state),
            LuaUserdata::Proxy(proxy) => proxy.identity.hash(state),
        }
    }
}

impl PartialEq for LuaUserdata {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (LuaUserdata::CjsonNull, LuaUserdata::CjsonNull) => true,
            (LuaUserdata::Proxy(a), LuaUserdata::Proxy(b)) => a.identity == b.identity,
            _ => false,
        }
    }
}

impl Eq for LuaUserdata {}

#[derive(Clone, Debug)]
pub struct LuaTable {
    pub inner: Rc<RefCell<LuaTableInner>>,
}

#[derive(Clone, Debug)]
pub struct LuaTableInner {
    pub array: Vec<LuaValue>,
    pub string_hash: LuaMap<Vec<u8>, LuaValue>,
    pub other_hash: Vec<(LuaValue, LuaValue)>,
    /// Set of keys in `other_hash` for fast O(1) existence checks.
    pub other_keys: HashSet<LuaHashKey>,
    /// Optional metatable for Lua 5.1 metamethods.
    pub metatable: Option<LuaTable>,
    /// Shared immutable standard-library template tables are reused across
    /// EVAL calls and must not be cleared by per-state cycle teardown.
    /// They are script-readonly before user code can observe them.
    pub shared_template: bool,
    /// When true, this table is protected: any script-level write (field
    /// assignment, `rawset`, `setmetatable`, `table.insert/remove/sort`) raises
    /// "Attempt to modify a readonly table". Redis applies this recursively to
    /// the whole script global env (math/string/table/struct/bit/cjson/cmsgpack/
    /// redis) via `luaSetTableProtectionRecursively`; fr previously left these
    /// library tables mutable. Internal `set()` during env construction is NOT
    /// gated (the flag is only consulted at the script-facing write sites), so
    /// the interpreter can still build the tables before locking them.
    /// (frankenredis-8mwy9)
    pub readonly: bool,
}

#[derive(Clone, Debug)]
pub struct LuaHashKey(pub LuaValue);

impl std::hash::Hash for LuaHashKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        match &self.0 {
            LuaValue::Nil => 0.hash(state),
            LuaValue::Bool(b) => b.hash(state),
            LuaValue::Number(n) => n.to_bits().hash(state),
            LuaValue::Str(s) => s.hash(state),
            LuaValue::Table(_) => "table".hash(state),
            // (frankenredis-sxqtm) Hash by function identity so distinct
            // functions land in different buckets — required for
            // function-as-table-key support per Lua 5.1 object identity.
            LuaValue::Function(f) => f.identity.hash(state),
            LuaValue::RustFunction(n) => n.hash(state),
            LuaValue::Userdata(kind) => kind.hash(state),
            LuaValue::Coroutine(co) | LuaValue::WrappedCoroutine(co) => {
                (Rc::as_ptr(&co.inner) as usize).hash(state);
            }
        }
    }
}

impl PartialEq for LuaHashKey {
    fn eq(&self, other: &Self) -> bool {
        lua_raw_equal(&self.0, &other.0)
    }
}

impl Eq for LuaHashKey {}

/// A block of statements, each tagged with the 1-based source line where it
/// begins. The evaluator updates `current_line` from these tags so error
/// messages report the real line (not a hardcoded 1). (frankenredis-m7oy8)
pub type Block = Vec<(u32, Stmt)>;

/// Stamp the real source line into a leading `user_script:1:` prefix. The
/// interpreter's ~90 error sites hardcode line 1; the evaluator tracks the
/// executing statement's line in `current_line` and applies it here, at the
/// single uncaught-error boundary. Only the chunk-start `user_script:1:`
/// prefix is rewritten — `line == 1` is a no-op (the common single-line case),
/// and other chunk labels (e.g. loadstring's `[string "..."]:N:`) and
/// already-stamped prefixes are left untouched. (frankenredis-m7oy8)
fn stamp_user_script_line(msg: String, line: u32) -> String {
    if line == 1 {
        return msg;
    }
    match msg.strip_prefix("user_script:1:") {
        Some(rest) => format!("user_script:{line}:{rest}"),
        None => msg,
    }
}

type LuaCell = Rc<RefCell<LuaValue>>;
type LuaCapturedScope = Vec<(String, LuaCell)>;
type LuaCapturedEnv = Vec<LuaCapturedScope>;

#[derive(Clone, Debug)]
pub struct LuaFunc {
    pub params: Vec<String>,
    pub body: Block,
    pub is_variadic: bool,
    /// Captured lexical environment (upvalues) from function definition site.
    pub captured_env: Option<LuaCapturedEnv>,
    /// Lua 5.1 function environment used for unresolved global reads and
    /// writes. `None` means the interpreter's default protected globals.
    pub env_table: Rc<RefCell<Option<LuaTable>>>,
    /// For `local function f(x) ... end`, stores the name so the function
    /// can be injected into its own call scope for self-recursion.
    pub self_name: Option<String>,
    /// Source-location label this function uses for runtime error
    /// prefixes. Set by `loadstring`/`load` so chunk errors carry the
    /// chunk's name (e.g. `[string "src"]:1:`) instead of the outer
    /// script's `user_script:1:`. None means the function inherits the
    /// outer script's prefix. (frankenredis-ycaog)
    pub source_label: Option<String>,
    /// (frankenredis-sxqtm) Process-unique identity, assigned at
    /// function-definition time and preserved across clones. Lua 5.1
    /// uses object identity for function equality (==) and for table-key
    /// hashing. Without an explicit id, two LuaValue::Function clones of
    /// the same source-function would never compare equal under
    /// lua_raw_equal — silently dropping the entry from table lookups.
    pub identity: u64,
}

/// (frankenredis-sxqtm) Source for `LuaFunc::identity` values. Each call
/// to `next_function_identity()` returns a fresh u64 used as the function
/// object's identity for `==` and table-key hashing.
fn next_function_identity() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

#[derive(Clone, Debug)]
pub struct LuaCoroutine {
    inner: Rc<RefCell<LuaCoroutineInner>>,
}

#[derive(Clone, Debug)]
struct LuaCoroutineInner {
    func: LuaFunc,
    status: LuaCoroutineStatus,
    env: Option<Env>,
    varargs: Vec<LuaValue>,
    pc: usize,
    continuation: Option<LuaCoroutineContinuation>,
}

#[derive(Clone, Debug)]
enum LuaCoroutineContinuation {
    Assign {
        lhs: Vec<Expr>,
        prefix: Vec<LuaValue>,
        remaining: Vec<Expr>,
        yield_was_last: bool,
    },
    LocalAssign {
        names: Vec<String>,
        prefix: Vec<LuaValue>,
        remaining: Vec<Expr>,
        yield_was_last: bool,
    },
    Return {
        prefix: Vec<LuaValue>,
        remaining: Vec<Expr>,
        yield_was_last: bool,
    },
    NumericFor {
        name: String,
        stop: f64,
        step: f64,
        body: Block,
        current: f64,
        body_pc: usize,
    },
    /// (CrimsonHawk 7lmle) A top-level `if <coroutine.yield(...)> then ...` whose
    /// branch condition was a bare yield. On resume the yielded value becomes that
    /// condition's result: truthy → run `then_body`; falsy → fall through to the
    /// `remaining` branches / `else_body` (evaluated normally — a second bare-yield
    /// condition there errors exactly as before, so this is purely additive).
    If {
        then_body: Block,
        remaining: Vec<(Expr, Block)>,
        else_body: Option<Block>,
    },
    /// (CrimsonHawk 7lmle) A top-level `while <coroutine.yield(...)> do ... end`
    /// whose loop condition is a bare yield. The condition is evaluated (and thus
    /// suspends) once per iteration: on a truthy resume the body runs and the loop
    /// re-yields at the condition; a falsy resume exits the loop. A body-level
    /// yield still errors exactly as before, so this is purely additive.
    While {
        cond: Expr,
        body: Block,
    },
    /// (CrimsonHawk 7lmle) A top-level `repeat ... until <coroutine.yield(...)>`
    /// whose loop condition is a bare yield. The body runs first, then the
    /// condition suspends (still inside the body's scope, so the yield args see
    /// body locals): a truthy resume exits the loop, a falsy resume re-runs the
    /// body and re-yields. Additive — a body-level yield still errors as before.
    Repeat {
        body: Block,
        cond: Expr,
    },
    /// (CrimsonHawk 7lmle) A top-level `for <names> in <iter exprs> do ... end`
    /// whose iterator expression list contains a bare `coroutine.yield(...)`.
    /// On resume the yielded value(s) complete the iterator triple (fn, state,
    /// control) — exactly as for a local/assign RHS — and the loop then runs to
    /// completion. A body-level yield still errors as before (additive).
    GenericFor {
        names: Vec<String>,
        prefix: Vec<LuaValue>,
        remaining: Vec<Expr>,
        yield_was_last: bool,
        body: Block,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LuaCoroutineStatus {
    Suspended,
    Running,
    Dead,
}

impl LuaCoroutine {
    fn new(func: LuaFunc) -> Self {
        Self {
            inner: Rc::new(RefCell::new(LuaCoroutineInner {
                func,
                status: LuaCoroutineStatus::Suspended,
                env: None,
                varargs: Vec::new(),
                pc: 0,
                continuation: None,
            })),
        }
    }

    fn status_name(&self) -> &'static [u8] {
        match self.inner.borrow().status {
            LuaCoroutineStatus::Suspended => b"suspended",
            LuaCoroutineStatus::Running => b"running",
            LuaCoroutineStatus::Dead => b"dead",
        }
    }
}

impl LuaTable {
    fn new() -> Self {
        let inner = Rc::new(RefCell::new(LuaTableInner {
            array: Vec::new(),
            string_hash: LuaMap::default(),
            other_hash: Vec::new(),
            other_keys: HashSet::new(),
            metatable: None,
            shared_template: false,
            readonly: false,
        }));
        // (frankenredis-qqq17) Track for cycle-breaking at eval end.
        lua_gc_register_table(&inner);
        #[cfg(test)]
        LUA_TEST_LIVE_TABLES.with(|c| c.set(c.get() + 1));
        Self { inner }
    }

    fn new_shared_template() -> Self {
        Self {
            inner: Rc::new(RefCell::new(LuaTableInner {
                array: Vec::new(),
                string_hash: LuaMap::default(),
                other_hash: Vec::new(),
                other_keys: HashSet::new(),
                metatable: None,
                shared_template: true,
                readonly: false,
            })),
        }
    }

    fn get(&self, key: &LuaValue) -> LuaValue {
        self.inner.borrow().get(key)
    }
    /// Get with __index metamethod fallback. Returns the value if found in
    /// the table, otherwise consults the metatable's __index entry.
    fn get_with_index(&self, key: &LuaValue) -> LuaValue {
        self.get_with_index_depth(key, 16)
    }

    fn get_with_index_depth(&self, key: &LuaValue, depth: u8) -> LuaValue {
        let val = self.get(key);
        if !matches!(val, LuaValue::Nil) || depth == 0 {
            return val;
        }
        // Extract the __index handler while the borrow is scoped, so we can
        // safely recurse without holding the RefCell borrow.
        let index_handler = {
            let inner = self.inner.borrow();
            let Some(mt) = &inner.metatable else {
                return LuaValue::Nil;
            };
            mt.get(&LuaValue::Str(b"__index".to_vec()))
        };
        match &index_handler {
            LuaValue::Table(fallback) => fallback.get_with_index_depth(key, depth - 1),
            _ => LuaValue::Nil,
        }
    }
    fn set(&self, key: LuaValue, value: LuaValue) {
        self.inner.borrow_mut().set(key, value)
    }
    /// Whether this table is protected against script-level writes. (8mwy9)
    fn is_readonly(&self) -> bool {
        self.inner.borrow().readonly
    }
    /// Mark this table (and any nested table values, guarding against cycles)
    /// read-only, mirroring redis's `luaSetTableProtectionRecursively`. Called
    /// once on the library tables after env construction, before user code runs.
    /// (frankenredis-8mwy9)
    fn mark_readonly_recursive(&self) {
        // Already protected (or being protected up the stack) → stop; this also
        // breaks self-referential cycles like `_G._G`.
        if self.inner.borrow().readonly {
            return;
        }
        let nested: Vec<LuaTable> = {
            let mut inner = self.inner.borrow_mut();
            inner.readonly = true;
            inner
                .array
                .iter()
                .chain(inner.string_hash.values())
                .chain(inner.other_hash.iter().map(|(_, v)| v))
                .filter_map(|v| match v {
                    LuaValue::Table(t) => Some(t.clone()),
                    _ => None,
                })
                .collect()
        };
        for t in nested {
            t.mark_readonly_recursive();
        }
    }
    fn len(&self) -> usize {
        self.inner.borrow().len()
    }
    fn hash_pairs(&self) -> Vec<(LuaValue, LuaValue)> {
        self.inner.borrow().hash_pairs()
    }
    #[allow(dead_code)]
    fn hash_is_empty(&self) -> bool {
        self.inner.borrow().hash_is_empty()
    }
}

impl LuaTableInner {
    fn get(&self, key: &LuaValue) -> LuaValue {
        match key {
            LuaValue::Number(n) => {
                let idx = *n as usize;
                if idx >= 1 && idx <= self.array.len() && *n == idx as f64 {
                    return self.array[idx - 1].clone();
                }
                self.hash_get(key)
            }
            LuaValue::Str(s) => {
                if let Some(v) = self.string_hash.get(s) {
                    return v.clone();
                }
                self.hash_get(key)
            }
            _ => self.hash_get(key),
        }
    }

    fn hash_get(&self, key: &LuaValue) -> LuaValue {
        if let LuaValue::Str(s) = key
            && let Some(v) = self.string_hash.get(s)
        {
            return v.clone();
        }
        // O(1) fast-fail if key is not in other_hash.
        if !self.other_keys.contains(&LuaHashKey(key.clone())) {
            return LuaValue::Nil;
        }
        for (k, v) in &self.other_hash {
            if lua_raw_equal(k, key) {
                return v.clone();
            }
        }
        LuaValue::Nil
    }

    fn set(&mut self, key: LuaValue, value: LuaValue) {
        match &key {
            LuaValue::Number(n) => {
                let idx = *n as usize;
                if idx >= 1 && *n == idx as f64 {
                    if idx <= self.array.len() {
                        self.array[idx - 1] = value;
                        while let Some(LuaValue::Nil) = self.array.last() {
                            self.array.pop();
                        }
                        return;
                    } else if idx == self.array.len() + 1 {
                        if !matches!(value, LuaValue::Nil) {
                            self.array.push(value);
                        }
                        return;
                    }
                }
                self.hash_set(key, value);
            }
            LuaValue::Str(_) => {
                self.hash_set(key, value);
            }
            _ => {
                self.hash_set(key, value);
            }
        }
    }

    fn hash_set(&mut self, key: LuaValue, value: LuaValue) {
        if matches!(value, LuaValue::Nil) {
            if let LuaValue::Str(s) = key {
                self.string_hash.remove(&s);
            } else {
                self.other_keys.remove(&LuaHashKey(key.clone()));
                self.other_hash
                    .retain(|entry| !lua_raw_equal(&entry.0, &key));
            }
            return;
        }
        if let LuaValue::Str(s) = key {
            self.string_hash.insert(s, value);
            return;
        }
        self.other_keys.insert(LuaHashKey(key.clone()));
        for entry in &mut self.other_hash {
            if lua_raw_equal(&entry.0, &key) {
                entry.1 = value;
                return;
            }
        }
        self.other_hash.push((key, value));
    }

    fn len(&self) -> usize {
        self.array.len()
    }

    /// Returns all hash pairs (string_hash + other_hash) as `(LuaValue, LuaValue)`.
    fn hash_pairs(&self) -> Vec<(LuaValue, LuaValue)> {
        let mut string_keys: Vec<&Vec<u8>> = self.string_hash.keys().collect();
        string_keys.sort();
        let mut pairs: Vec<(LuaValue, LuaValue)> = string_keys
            .into_iter()
            .filter_map(|k| {
                self.string_hash
                    .get(k)
                    .map(|v| (LuaValue::Str(k.clone()), v.clone()))
            })
            .collect();
        pairs.extend(self.other_hash.iter().cloned());
        pairs
    }

    /// Returns true if the hash part (both string and other) is empty.
    fn hash_is_empty(&self) -> bool {
        self.string_hash.is_empty() && self.other_hash.is_empty()
    }

    /// (frankenredis-y0ri2) Lua 5.1 luaH_getn border search. When the array
    /// part ends in nil, binary-search for a border `i` such that
    /// `t[i] != nil && t[i+1] == nil`. When the array's last slot is
    /// non-nil and there are integer keys in the hash part, unbound_search
    /// extends the search past sizearray. Used by the `#` operator and by
    /// table.concat's default `last` argument.
    fn border_len(&self) -> usize {
        let j = self.array.len();
        if j > 0 && matches!(self.array[j - 1], LuaValue::Nil) {
            let mut i: usize = 0;
            let mut j: usize = j;
            while j - i > 1 {
                let m = (i + j) / 2;
                if matches!(self.array[m - 1], LuaValue::Nil) {
                    j = m;
                } else {
                    i = m;
                }
            }
            return i;
        }
        if self.hash_is_empty() {
            return j;
        }
        // Unbound search in the hash part: double-and-binary-search to find
        // any integer-key border t[i] != nil && t[i+1] == nil.
        let mut i = j;
        let mut hi = j + 1;
        while !matches!(self.hash_get(&LuaValue::Number(hi as f64)), LuaValue::Nil) {
            i = hi;
            if hi > usize::MAX / 2 {
                let mut k = 1usize;
                while !matches!(self.hash_get(&LuaValue::Number(k as f64)), LuaValue::Nil) {
                    k += 1;
                }
                return k - 1;
            }
            hi *= 2;
        }
        while hi - i > 1 {
            let m = (i + hi) / 2;
            if matches!(self.hash_get(&LuaValue::Number(m as f64)), LuaValue::Nil) {
                hi = m;
            } else {
                i = m;
            }
        }
        i
    }
}

/// (frankenredis-y0ri2) Lua 5.1 SETLIST semantics: positional fields in a
/// table constructor occupy fixed slots even when the value is nil. The
/// general LuaTable::set path drops nil at `array.len() + 1` (matching
/// `t[i] = nil` delete semantics), which is wrong for `{1, nil, 3}` since
/// it loses the slot. This helper grows the array with nil padding so
/// the positional slot is preserved.
fn set_positional_array_slot(table: &LuaTable, slot: usize, value: LuaValue) {
    let mut inner = table.inner.borrow_mut();
    if slot >= 1 && slot <= inner.array.len() {
        inner.array[slot - 1] = value;
        return;
    }
    while inner.array.len() + 1 < slot {
        inner.array.push(LuaValue::Nil);
    }
    inner.array.push(value);
}

/// Format the chunk-name prefix Lua 5.1 wraps around loadstring/load
/// parse errors. Mirrors luaO_chunkid in lobject.c:
///   - chunkname starts with '=' → strip the '=' and use as a literal label
///   - chunkname starts with '@' → strip the '@' and use as a literal label
///     (upstream uses this for filenames)
///   - any other chunkname → wrap in `[string "NAME"]`
///   - no chunkname → use the first line of the source, truncated with
///     `"..."` if the source spans multiple lines, wrapped in `[string "..."]`
///
/// (frankenredis-cfflo)
fn format_lua_chunk_label(chunkname: Option<&[u8]>, source: &[u8]) -> String {
    if let Some(name) = chunkname {
        if let Some(rest) = name.strip_prefix(b"=").or_else(|| name.strip_prefix(b"@")) {
            return String::from_utf8_lossy(rest).into_owned();
        }
        return format!("[string \"{}\"]", String::from_utf8_lossy(name));
    }
    let first_newline = source.iter().position(|&b| b == b'\n');
    let snippet: &[u8] = match first_newline {
        Some(pos) => &source[..pos],
        None => source,
    };
    let suffix = if first_newline.is_some() { "..." } else { "" };
    format!(
        "[string \"{}{}\"]",
        String::from_utf8_lossy(snippet),
        suffix
    )
}

fn lua_raw_equal(a: &LuaValue, b: &LuaValue) -> bool {
    match (a, b) {
        (LuaValue::Nil, LuaValue::Nil) => true,
        (LuaValue::Bool(x), LuaValue::Bool(y)) => x == y,
        (LuaValue::Number(x), LuaValue::Number(y)) => x == y,
        (LuaValue::Str(x), LuaValue::Str(y)) => x == y,
        // (frankenredis-tbu4k) RustFunctions are identified by their
        // dispatch name; two LuaValues referring to e.g. "string.upper"
        // share the same identity in vendored Lua 5.1 because the
        // string library table holds one shared function pointer per
        // name. Matching by name preserves `string.upper == s.upper`
        // and `string.upper == ('x').upper` equalities.
        (LuaValue::RustFunction(x), LuaValue::RustFunction(y)) => x == y,
        // (frankenredis-cxmsu) LuaTable values share an Rc handle; Lua
        // 5.1 raw-compares tables by reference identity. Required so
        // `error(t); ... err == t` (round-tripping a table through
        // pcall) holds and so `t == t` for a single-table variable is
        // true regardless of contents.
        (LuaValue::Table(x), LuaValue::Table(y)) => Rc::ptr_eq(&x.inner, &y.inner),
        (LuaValue::Coroutine(x), LuaValue::Coroutine(y))
        | (LuaValue::WrappedCoroutine(x), LuaValue::WrappedCoroutine(y)) => {
            Rc::ptr_eq(&x.inner, &y.inner)
        }
        // (frankenredis-sxqtm) Lua 5.1 functions compare by object
        // identity (==). Use the per-function identity counter assigned
        // at definition time; clones inherit the same id.
        (LuaValue::Function(x), LuaValue::Function(y)) => x.identity == y.identity,
        (LuaValue::Userdata(x), LuaValue::Userdata(y)) => x == y,
        _ => false,
    }
}

/// (frankenredis-kqd16 / frankenredis-i18ug) Upstream luaL_argerror reads
/// the function name from lua_getinfo("n.name") on the active C closure;
/// when the closure was invoked indirectly via pcall/xpcall the name
/// resolves to NULL and the C closure's source-location is empty. So the
/// final wording is:
///   - Direct AST call (e.g. `ipairs(nil)`): inv_name=Some("ipairs"),
///     output `user_script:1: bad argument #1 to 'ipairs' (...)`.
///   - Indirect via pcall: inv_name=None, output `bad argument #1 to '?' (...)`.
///
/// The got-label helper distinguishes "no value" (missing arg) from
/// explicit "nil".
fn lua_bad_table_arg(inv_name: Option<&str>, index: usize, value: Option<&LuaValue>) -> String {
    let got = lua_arg_got_label(value);
    match inv_name {
        Some(name) => {
            format!("user_script:1: bad argument #{index} to '{name}' (table expected, got {got})")
        }
        None => format!("bad argument #{index} to '?' (table expected, got {got})"),
    }
}

fn lua_bad_number_arg(inv_name: Option<&str>, index: usize, value: Option<&LuaValue>) -> String {
    let got = lua_arg_got_label(value);
    match inv_name {
        Some(name) => {
            format!("user_script:1: bad argument #{index} to '{name}' (number expected, got {got})")
        }
        None => format!("bad argument #{index} to '?' (number expected, got {got})"),
    }
}

fn lua_table_arg<'a>(
    inv_name: Option<&str>,
    index: usize,
    value: Option<&'a LuaValue>,
) -> Result<&'a LuaTable, String> {
    match value {
        Some(LuaValue::Table(table)) => Ok(table),
        _ => Err(lua_bad_table_arg(inv_name, index, value)),
    }
}

fn lua_required_integer_arg(
    inv_name: Option<&str>,
    index: usize,
    value: &LuaValue,
) -> Result<i64, String> {
    match value.to_number() {
        Some(number) if number.is_finite() => Ok(number as i64),
        _ => Err(lua_bad_number_arg(inv_name, index, Some(value))),
    }
}

fn lua_optional_integer_arg(
    inv_name: Option<&str>,
    index: usize,
    value: Option<&LuaValue>,
    default: i64,
) -> Result<i64, String> {
    match value {
        None | Some(LuaValue::Nil) => Ok(default),
        Some(value) => lua_required_integer_arg(inv_name, index, value),
    }
}

/// (frankenredis-tb9vb) Lua 5.1's table-set primitive (ltable.c::luaH_set
/// invoked from lvm.c::luaV_settable) rejects nil and NaN keys with a
/// runtime error before the slot is allocated. Both `t[k]=v` syntax and
/// `{[k]=v}` constructors funnel through luaV_settable, so the check
/// must fire at both sites in fr. Positive/negative infinity are
/// allowed — only NaN is rejected. Returns the user_script:1-prefixed
/// error string matching the VM-runtime wording (rawset emits the same
/// message without the prefix; see the rawset arm).
fn lua_check_table_key(key: &LuaValue) -> Result<(), String> {
    match key {
        LuaValue::Nil => Err("user_script:1: table index is nil".to_string()),
        LuaValue::Number(n) if n.is_nan() => Err("user_script:1: table index is NaN".to_string()),
        _ => Ok(()),
    }
}

/// Render the upstream "got" suffix for a bad-argument error.
/// `None` means the arg position was never set, producing "no value";
/// `Some(LuaValue::Nil)` produces "nil"; any other value produces the
/// type name. (frankenredis-nf29w)
fn lua_arg_got_label(value: Option<&LuaValue>) -> &'static str {
    match value {
        None => "no value",
        Some(v) => v.type_name(),
    }
}

// (frankenredis-3osi6) Mirror upstream lauxlib.c:luaL_checknumber. Both the
// missing-arg and the wrong-type cases go through the same "bad argument #N
// to '<fname>' (number expected, got <got>)" template, with the user_script:N
// source-location prefix that Lua's luaL_argerror auto-prepends.
/// (frankenredis-i18ug) Format a bad-arg error using the AST-callsite
/// invocation name when available (Some → prefix + name) or the indirect
/// '?' form (None → no prefix, '?' for name). Mirrors the rule used by
/// LuaState::format_builtin_argerror, expressed as a free helper so the
/// check_* fns can stay context-light. The `_fallback_name` parameter is
/// retained for callers that want a meaningful internal name in error
/// messages they assemble themselves, but it is NOT used in the
/// rendered output — vendored's luaL_argerror always uses lua_getinfo
/// "n.name" which is None for pcall-invoked C closures.
fn lua_format_argerror(
    inv_name: Option<&str>,
    _fallback_name: &str,
    idx: usize,
    reason: &str,
) -> String {
    match inv_name {
        Some(name) => format!("user_script:1: bad argument #{idx} to '{name}' ({reason})"),
        None => format!("bad argument #{idx} to '?' ({reason})"),
    }
}

fn lua_check_number(
    inv_name: Option<&str>,
    args: &[LuaValue],
    idx: usize,
    fname: &str,
) -> Result<f64, String> {
    let arg = args.get(idx);
    if let Some(v) = arg
        && let Some(n) = v.to_number()
    {
        return Ok(n);
    }
    Err(lua_format_argerror(
        inv_name,
        fname,
        idx + 1,
        &format!("number expected, got {}", lua_arg_got_label(arg)),
    ))
}

// Mirror upstream luaL_checklstring. Numbers are coerced to their
// luaO_str2d-formatted string; everything else (nil, bool, table, function,
// thread) raises with the standard 'string expected, got <type>' wording.
fn lua_check_string(
    inv_name: Option<&str>,
    args: &[LuaValue],
    idx: usize,
    fname: &str,
) -> Result<Vec<u8>, String> {
    match args.get(idx) {
        Some(LuaValue::Str(b)) => Ok(b.clone()),
        Some(LuaValue::Number(n)) => {
            if *n == (*n as i64) as f64 && n.is_finite() {
                Ok(format!("{}", *n as i64).into_bytes())
            } else {
                Ok(lua_number_to_string(*n).into_bytes())
            }
        }
        other => Err(lua_format_argerror(
            inv_name,
            fname,
            idx + 1,
            &format!("string expected, got {}", lua_arg_got_label(other)),
        )),
    }
}

// Mirror upstream luaL_checktype(L, idx, LUA_TTABLE). Returns a cloned
// LuaTable handle (refcounted) so the caller can borrow without lifetime
// shenanigans.
fn lua_check_table(
    inv_name: Option<&str>,
    args: &[LuaValue],
    idx: usize,
    fname: &str,
) -> Result<LuaTable, String> {
    match args.get(idx) {
        Some(LuaValue::Table(t)) => Ok(t.clone()),
        other => Err(lua_format_argerror(
            inv_name,
            fname,
            idx + 1,
            &format!("table expected, got {}", lua_arg_got_label(other)),
        )),
    }
}

impl LuaValue {
    // (CrimsonHawk) Construct a Function value, boxing the 144-byte LuaFunc off
    // the hot LuaValue (see the enum comment).
    #[inline]
    fn function(f: LuaFunc) -> Self {
        LuaValue::Function(Box::new(f))
    }

    fn is_truthy(&self) -> bool {
        !matches!(self, LuaValue::Nil | LuaValue::Bool(false))
    }

    fn type_name(&self) -> &'static str {
        match self {
            LuaValue::Nil => "nil",
            LuaValue::Bool(_) => "boolean",
            LuaValue::Number(_) => "number",
            LuaValue::Str(_) => "string",
            LuaValue::Table(_) => "table",
            LuaValue::Function(_) | LuaValue::RustFunction(_) => "function",
            LuaValue::Userdata(_) => "userdata",
            LuaValue::Coroutine(_) => "thread",
            LuaValue::WrappedCoroutine(_) => "function",
        }
    }

    fn to_number(&self) -> Option<f64> {
        match self {
            LuaValue::Number(n) => Some(*n),
            LuaValue::Str(s) => {
                let s = std::str::from_utf8(s).ok()?;
                let trimmed = s.trim();
                // (frankenredis-83zqp) Lua 5.1's lua_tonumber funnels
                // through strtod, which accepts C99 hex floats — both
                // the `0x10` integer form and the `0x1.8p2` binary-
                // exponent form. Rust's f64::FromStr only handles
                // decimal, so try the hex helper first and fall back
                // to decimal if the input is not a hex literal.
                if let Some(val) = crate::try_parse_hex_float(trimmed) {
                    if val.is_nan() {
                        return None;
                    }
                    return Some(val);
                }
                trimmed.parse::<f64>().ok()
            }
            _ => None,
        }
    }

    fn to_redis_arg(&self) -> Result<Vec<u8>, String> {
        // Upstream src/script_lua.c::luaArgsToRedisArgv only accepts
        // numbers (formatted via %.17g) and strings; lua_tolstring on
        // any other type (nil, boolean, table, function, thread,
        // userdata) returns NULL, and the loop bails with a single
        // unified error: 'Lua redis lib command arguments must be
        // strings or integers'. fr previously had per-type error
        // wordings AND silently coerced LuaValue::Bool to "1"/"0",
        // so `redis.call("SET", k, true)` returned OK on fr but errored
        // on vendored. (frankenredis-redisargtype)
        match self {
            LuaValue::Number(n) => {
                if *n == (*n as i64) as f64 && n.is_finite() {
                    // (CrimsonHawk) Manual itoa on the redis.call integer-arg hot
                    // path — avoids the `format!`/core::fmt machinery (~2.6% of a
                    // redis.call-heavy EVAL profile). Byte-identical to
                    // `format!("{}", v)`.
                    Ok(i64_to_ascii_bytes(*n as i64))
                } else {
                    Ok(format!("{n}").into_bytes())
                }
            }
            LuaValue::Str(s) => Ok(s.clone()),
            _ => Err("Lua redis lib command arguments must be strings or integers".to_string()),
        }
    }

    fn to_display_string(&self) -> Vec<u8> {
        match self {
            LuaValue::Nil => b"nil".to_vec(),
            LuaValue::Bool(b) => {
                if *b {
                    b"true".to_vec()
                } else {
                    b"false".to_vec()
                }
            }
            LuaValue::Number(n) => {
                // (frankenredis-n4eln) Skip the integer fast path for
                // -0.0 so the helper preserves the sign bit -- Rust
                // i64 cast collapses -0.0 to 0 which then formats as
                // "0", losing the sign.
                let is_neg_zero = *n == 0.0 && n.is_sign_negative();
                // (frankenredis-ie595) Also skip the fast path once
                // the value would push C's %.14g into scientific
                // notation (i.e. |n| >= 1e14). tostring(2^53) must
                // emit '9.007199254741e+15', not the full 16-digit
                // integer literal.
                let abs = n.abs();
                let needs_scientific = abs >= 1e14;
                if !is_neg_zero && !needs_scientific && *n == (*n as i64) as f64 && n.is_finite() {
                    format!("{}", *n as i64).into_bytes()
                } else {
                    lua_number_to_string(*n).into_bytes()
                }
            }
            LuaValue::Str(s) => s.clone(),
            // (frankenredis-7qoww) Lua 5.1 tostring on reference types
            // emits '<type>: 0x<hex>'. fr previously emitted just the
            // type name. Use the underlying Rc/Vec heap address so the
            // value is stable across clones of the same logical value
            // (Tables/Coroutines share an Rc; Functions clone their
            // Vec params so the address changes per clone, which is OK
            // because the FORMAT is what matters for scripts that
            // string-match on tostring output).
            LuaValue::Table(t) => {
                let addr = Rc::as_ptr(&t.inner) as usize;
                format!("table: 0x{addr:014x}").into_bytes()
            }
            LuaValue::Function(f) => {
                let addr = f.params.as_ptr() as usize;
                format!("function: 0x{addr:014x}").into_bytes()
            }
            LuaValue::RustFunction(n) => {
                let addr = n.as_ptr() as usize;
                format!("function: 0x{addr:014x}").into_bytes()
            }
            LuaValue::Userdata(LuaUserdata::CjsonNull) => b"userdata: (nil)".to_vec(),
            LuaValue::Userdata(LuaUserdata::Proxy(proxy)) => {
                format!("userdata: 0x{:014x}", proxy.identity).into_bytes()
            }
            LuaValue::Coroutine(co) => {
                let addr = Rc::as_ptr(&co.inner) as usize;
                format!("thread: 0x{addr:014x}").into_bytes()
            }
            LuaValue::WrappedCoroutine(co) => {
                let addr = Rc::as_ptr(&co.inner) as usize;
                format!("function: 0x{addr:014x}").into_bytes()
            }
        }
    }
}

// ── Tokens ──────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Number(f64),
    Str(Vec<u8>),
    Name(String),
    // Keywords
    And,
    Break,
    Do,
    Else,
    ElseIf,
    End,
    False,
    For,
    Function,
    If,
    In,
    Local,
    Nil,
    Not,
    Or,
    Repeat,
    Return,
    Then,
    True,
    Until,
    While,
    // Operators
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Caret,
    Hash,
    EqEq,
    TildeEq,
    Lt,
    Gt,
    LtEq,
    GtEq,
    Eq,
    DotDot,
    Dots,
    // Punctuation
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Comma,
    Semi,
    Colon,
    Dot,
    Eof,
}

/// Parse a string like "0xFF" or "-0X1A" as a Lua number, mirroring
/// upstream Lua 5.1's lobject.c::luaO_str2d hex fallback. Returns
/// LuaValue::Nil on any malformed input (no digits, junk, etc.) so
/// callers can route directly into tonumber's nil-on-failure path.
/// (frankenredis-luatonumhex)
/// (frankenredis-8reid) Reproduce upstream luaB_tonumber's
/// strtoul-based wrap. With an explicit non-10 base, Lua 5.1 calls
/// `strtoul(s, _, base)` which parses the digits as unsigned and then
/// applies unsigned negation when a leading '-' was consumed. So
/// `tonumber("-5", 16)` returns ((unsigned long)-5) cast to a Lua
/// number — ~1.8446744073709552e19. For base=10 explicit, vendored
/// takes a different code path (lua_isnumber/lua_tonumber → strtod),
/// which is signed; preserve that here.
fn lua_tonumber_strtoul_result(sign: i64, digits: u64, base: u32) -> f64 {
    if sign >= 0 {
        return digits as f64;
    }
    if base == 10 {
        // strtod-style signed negation.
        return -(digits as f64);
    }
    // strtoul wrap: parse magnitude as unsigned, apply unsigned
    // negation, then cast to f64. The lua_to_resp boundary then
    // mimics the C `(long long)` cast which yields INT64_MIN.
    digits.wrapping_neg() as f64
}

fn hex_str_to_lua_number(trimmed: &str) -> LuaValue {
    let (neg, rest) = match trimmed.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, trimmed.strip_prefix('+').unwrap_or(trimmed)),
    };
    let Some(hex) = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")) else {
        return LuaValue::Nil;
    };
    if hex.is_empty() {
        return LuaValue::Nil;
    }
    match u64::from_str_radix(hex, 16) {
        Ok(n) => {
            let v = n as f64;
            LuaValue::Number(if neg { -v } else { v })
        }
        Err(_) => LuaValue::Nil,
    }
}

/// Format a token the way upstream Lua surfaces it in syntax errors —
/// e.g. 'lua', '<eof>', 'b', '+'. Used by the parser to reproduce
/// upstream's "'=' expected near '<token>'" wording for invalid
/// expression statements. (frankenredis-luabarestmt)
/// Best-effort rendering of the leading token of an Expr, used to
/// produce a vendored-style "unexpected symbol near '<X>'"-flavoured
/// message when a non-lvalue is supplied on the LHS of an assignment.
/// (frankenredis-s9mxn)
fn lua_lvalue_first_token(expr: &Expr) -> String {
    match expr {
        Expr::Nil => "nil".to_string(),
        Expr::Bool(true) => "true".to_string(),
        Expr::Bool(false) => "false".to_string(),
        Expr::Number(n) => {
            if n.fract() == 0.0 && n.is_finite() && n.abs() < 1e15 {
                format!("{}", *n as i64)
            } else {
                format!("{n}")
            }
        }
        Expr::Str(s) => format!("'{}'", String::from_utf8_lossy(s)),
        Expr::Name(n) | Expr::LocalName(n, _) => n.clone(),
        Expr::VarArgs => "...".to_string(),
        Expr::Call(_, _) | Expr::MethodCall(_, _, _) => "()".to_string(),
        Expr::TableConstructor(_) => "{".to_string(),
        Expr::FunctionDef(_, _, _) => "function".to_string(),
        Expr::BinOp(left, _, _) | Expr::UnaryOp(_, left) => lua_lvalue_first_token(left),
        Expr::Index(left, _) | Expr::Field(left, _) => lua_lvalue_first_token(left),
    }
}

/// (frankenredis-i0h24) Build the "expected"-slot label that Lua 5.1's
/// parser uses in 'X expected near Y' diagnostics. Keywords and
/// punctuation are quoted with their literal spelling; identifiers
/// (when the parser needs any name) render as '<name>'.
fn expected_token_label(t: &Token) -> String {
    match t {
        Token::Name(_) => "<name>".to_string(),
        other => token_display(other),
    }
}

/// Build an upstream-shaped 'EXPECTED expected near GOT' message
/// using single-quoted token-display forms on both sides.
fn parser_expected_near(expected: &Token, got: &Token) -> String {
    format!(
        "'{}' expected near '{}'",
        expected_token_label(expected),
        token_display(got),
    )
}

/// Same shape with a literal "<name>" expected slot, used when the
/// parser requires an identifier in a specific position (function
/// name, parameter, field, local, etc).
fn parser_name_expected_near(got: &Token) -> String {
    format!("'<name>' expected near '{}'", token_display(got))
}

fn token_display(tok: &Token) -> String {
    match tok {
        Token::Name(n) => n.clone(),
        Token::Number(n) => n.to_string(),
        Token::Str(_) => "<string>".to_string(),
        Token::Eof => "<eof>".to_string(),
        Token::And => "and".to_string(),
        Token::Break => "break".to_string(),
        Token::Do => "do".to_string(),
        Token::Else => "else".to_string(),
        Token::ElseIf => "elseif".to_string(),
        Token::End => "end".to_string(),
        Token::False => "false".to_string(),
        Token::For => "for".to_string(),
        Token::Function => "function".to_string(),
        Token::If => "if".to_string(),
        Token::In => "in".to_string(),
        Token::Local => "local".to_string(),
        Token::Nil => "nil".to_string(),
        Token::Not => "not".to_string(),
        Token::Or => "or".to_string(),
        Token::Repeat => "repeat".to_string(),
        Token::Return => "return".to_string(),
        Token::Then => "then".to_string(),
        Token::True => "true".to_string(),
        Token::Until => "until".to_string(),
        Token::While => "while".to_string(),
        Token::Plus => "+".to_string(),
        Token::Minus => "-".to_string(),
        Token::Star => "*".to_string(),
        Token::Slash => "/".to_string(),
        Token::Percent => "%".to_string(),
        Token::Caret => "^".to_string(),
        Token::Hash => "#".to_string(),
        Token::EqEq => "==".to_string(),
        Token::TildeEq => "~=".to_string(),
        Token::Lt => "<".to_string(),
        Token::Gt => ">".to_string(),
        Token::LtEq => "<=".to_string(),
        Token::GtEq => ">=".to_string(),
        Token::Eq => "=".to_string(),
        Token::DotDot => "..".to_string(),
        Token::Dots => "...".to_string(),
        Token::LParen => "(".to_string(),
        Token::RParen => ")".to_string(),
        Token::LBracket => "[".to_string(),
        Token::RBracket => "]".to_string(),
        Token::LBrace => "{".to_string(),
        Token::RBrace => "}".to_string(),
        Token::Comma => ",".to_string(),
        Token::Semi => ";".to_string(),
        Token::Colon => ":".to_string(),
        Token::Dot => ".".to_string(),
    }
}

// ── Lexer ───────────────────────────────────────────────────────────────

struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a [u8]) -> Self {
        Self { src, pos: 0 }
    }

    fn peek_byte(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn advance(&mut self) -> Option<u8> {
        let b = self.src.get(self.pos).copied()?;
        self.pos += 1;
        Some(b)
    }

    fn skip_whitespace_and_comments(&mut self) -> Result<(), String> {
        loop {
            // Skip whitespace
            while let Some(b) = self.peek_byte() {
                if b == b' ' || b == b'\t' || b == b'\r' || b == b'\n' {
                    self.pos += 1;
                } else {
                    break;
                }
            }
            // Skip comments
            if self.pos + 1 < self.src.len()
                && self.src[self.pos] == b'-'
                && self.src[self.pos + 1] == b'-'
            {
                self.pos += 2;
                // (frankenredis-h1vbd) Long comment with optional
                // level markers: `--[=*[ ... ]=*]`. fr previously only
                // recognised level-0 (`--[[ ... ]]`) and emitted
                // `'=' expected near '<eof>'` for level >= 1 because
                // the trailing `[=[` made the lexer attempt to parse
                // `[=` as something else.
                if let Some(level) = self.try_long_bracket_open() {
                    self.pos += level + 2; // consume `[<level eq>[`
                    let mut closed = false;
                    while self.pos + level + 1 < self.src.len() {
                        if self.src[self.pos] == b']'
                            && self.src[self.pos + level + 1] == b']'
                            && (1..=level).all(|i| self.src[self.pos + i] == b'=')
                        {
                            self.pos += level + 2;
                            closed = true;
                            break;
                        }
                        self.pos += 1;
                    }
                    if !closed {
                        // Match upstream wording on unterminated body.
                        // Drain to EOF before erroring so the caller
                        // sees the end-of-input position.
                        self.pos = self.src.len();
                        return Err("unfinished long comment near '<eof>'".to_string());
                    }
                } else {
                    // Line comment
                    while let Some(b) = self.peek_byte() {
                        if b == b'\n' {
                            break;
                        }
                        self.pos += 1;
                    }
                }
                continue;
            }
            break;
        }
        Ok(())
    }

    fn read_string(&mut self, delim: u8) -> Result<Vec<u8>, String> {
        let mut buf = Vec::new();
        loop {
            let Some(b) = self.advance() else {
                // (frankenredis-yunl8) Upstream llex.c emits the
                // "near '<eof>'" suffix when the lexer hits end-of-
                // input mid-token. fr previously emitted just the
                // bare "unterminated string".
                return Err("unfinished string near '<eof>'".to_string());
            };
            if b == delim {
                return Ok(buf);
            }
            if b == b'\\' {
                let Some(esc) = self.advance() else {
                    return Err("unterminated escape".to_string());
                };
                // (frankenredis-whyor) Mirror Lua 5.1.5's llex.c::
                // read_string exactly: recognized named escapes resolve
                // to their control bytes; numeric \ddd reads up to 3
                // base-10 digits and rejects >255; ANY other escape
                // (e.g. \x, \z, \q) drops the backslash and keeps the
                // following byte verbatim — Lua 5.1 has no \xNN hex
                // escape, so pre-fix fr's "preserve both bytes"
                // behavior diverged from vendored on every script that
                // used \xff-style literals.
                match esc {
                    b'a' => buf.push(0x07),
                    b'b' => buf.push(0x08),
                    b'f' => buf.push(0x0c),
                    b'n' => buf.push(b'\n'),
                    b'r' => buf.push(b'\r'),
                    b't' => buf.push(b'\t'),
                    b'v' => buf.push(0x0b),
                    b'\n' | b'\r' => buf.push(b'\n'),
                    b'0'..=b'9' => {
                        let mut num = (esc - b'0') as u16;
                        for _ in 0..2 {
                            if let Some(d) = self.peek_byte() {
                                if d.is_ascii_digit() {
                                    num = num * 10 + (d - b'0') as u16;
                                    self.pos += 1;
                                } else {
                                    break;
                                }
                            }
                        }
                        if num > 255 {
                            // (frankenredis-8xuri) Mirror Lua 5.1.5
                            // llex.c::luaX_lexerror near-suffix: when
                            // the error fires mid-string, the current
                            // token rendering is the open/close quote
                            // character. Format: "...near '<delim>'".
                            return Err(format!(
                                "escape sequence too large near '{}'",
                                delim as char
                            ));
                        }
                        buf.push(num as u8);
                    }
                    _ => buf.push(esc),
                }
            } else {
                buf.push(b);
            }
        }
    }

    /// (frankenredis-h1vbd) Try to read a long-bracket opener
    /// (`[=*[`) at the current position. Returns the level count
    /// (number of `=` between brackets, including zero) and advances
    /// past the entire opener on success; leaves position unchanged
    /// on failure. Mirrors upstream llex.c::skip_sep + check_next.
    fn try_long_bracket_open(&self) -> Option<usize> {
        if self.peek_byte() != Some(b'[') {
            return None;
        }
        let mut probe = self.pos + 1;
        let mut level = 0;
        while probe < self.src.len() && self.src[probe] == b'=' {
            level += 1;
            probe += 1;
        }
        if probe < self.src.len() && self.src[probe] == b'[' {
            Some(level)
        } else {
            None
        }
    }

    fn read_long_string(&mut self, level: usize) -> Result<Vec<u8>, String> {
        // Already consumed the opener `[<level eq>[`
        let mut buf = Vec::new();
        // Skip first newline if present
        if self.peek_byte() == Some(b'\n') {
            self.pos += 1;
        }
        loop {
            // (frankenredis-h1vbd) Closing pattern matches the level:
            // `]` + `level` × `=` + `]`. The 0-level case folds back to
            // the original `]]` matcher; higher levels need to count
            // the equal signs between the closing brackets.
            if self.pos + level + 1 < self.src.len()
                && self.src[self.pos] == b']'
                && self.src[self.pos + level + 1] == b']'
                && (1..=level).all(|i| self.src[self.pos + i] == b'=')
            {
                self.pos += level + 2;
                return Ok(buf);
            }
            let Some(b) = self.advance() else {
                // (frankenredis-yunl8) Mirror upstream's "near '<eof>'"
                // suffix on unterminated long-string tokens.
                return Err("unfinished long string near '<eof>'".to_string());
            };
            buf.push(b);
        }
    }

    fn next_token(&mut self) -> Result<Token, String> {
        self.skip_whitespace_and_comments()?;
        let Some(b) = self.peek_byte() else {
            return Ok(Token::Eof);
        };
        match b {
            b'0'..=b'9' => {
                let start = self.pos;
                // Detect hex prefix `0x` / `0X` BEFORE consuming further
                // digits — Lua 5.1 (Redis's embedded Lua) accepts
                // 0xFF, 0X1A, etc. The previous gate `self.pos - start
                // >= 2` was unreachable: at that point only the leading
                // `0` had been consumed (1 byte), so the hex block was
                // skipped, the trailing `xFF` got re-lexed as a Name,
                // and `bit.band(0xFF, 0)` errored with
                // "expected RParen, got Name(\"xFF\")".
                // (frankenredis-luahexlit)
                if self.src[self.pos] == b'0'
                    && self.pos + 1 < self.src.len()
                    && (self.src[self.pos + 1] == b'x' || self.src[self.pos + 1] == b'X')
                {
                    self.pos += 2; // consume "0x"
                    while let Some(d) = self.peek_byte() {
                        if d.is_ascii_hexdigit() {
                            self.pos += 1;
                        } else {
                            break;
                        }
                    }
                    // (frankenredis-5ife7) Greedy-consume alphanumeric
                    // continuation so trailing junk lands inside the
                    // malformed lexeme — vendored emits "malformed
                    // number near '0xG'" not "...'0x'".
                    let hex_end = self.pos;
                    while let Some(d) = self.peek_byte() {
                        if d.is_ascii_alphanumeric() || d == b'.' {
                            self.pos += 1;
                        } else {
                            break;
                        }
                    }
                    let s = std::str::from_utf8(&self.src[start..self.pos])
                        .map_err(|_| "invalid number")?;
                    if self.pos != hex_end || s.len() <= 2 {
                        return Err(format!("malformed number near '{s}'"));
                    }
                    let n = u64::from_str_radix(&s[2..], 16)
                        .map(|i| i as f64)
                        .map_err(|e| e.to_string())?;
                    return Ok(Token::Number(n));
                }
                while let Some(d) = self.peek_byte() {
                    if d.is_ascii_digit() || d == b'.' {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                // Handle scientific notation
                if let Some(e) = self.peek_byte()
                    && (e == b'e' || e == b'E')
                {
                    self.pos += 1;
                    if let Some(s) = self.peek_byte()
                        && (s == b'+' || s == b'-')
                    {
                        self.pos += 1;
                    }
                    while let Some(d) = self.peek_byte() {
                        if d.is_ascii_digit() {
                            self.pos += 1;
                        } else {
                            break;
                        }
                    }
                }
                // (frankenredis-5ife7) Greedy-consume alphanumeric
                // continuation so trailing junk (e.g. `1ea`, `1.2.3`)
                // ends up inside the lexeme and the upstream
                // "malformed number near '<lexeme>'" wording matches
                // verbatim. Without this, `1ea` would stop at `1e`
                // (the scientific-notation block already advanced
                // past 'e' once, but only digits follow it normally,
                // so `a` would be lexed as a Name and the parser
                // would emit a different error).
                while let Some(d) = self.peek_byte() {
                    if d.is_ascii_alphanumeric() || d == b'.' {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                let s = std::str::from_utf8(&self.src[start..self.pos])
                    .map_err(|_| "invalid number")?;
                let n = s.parse::<f64>().map_err(|_| {
                    // (frankenredis-5ife7) Upstream Lua's llex.c::lex_number
                    // raises "malformed number near '<lexeme>'" for any
                    // sequence that scans like a number but doesn't parse
                    // (multiple dots, trailing letters, incomplete
                    // scientific notation, etc.). fr previously bubbled
                    // Rust's stdlib "invalid float literal" verbatim.
                    format!("malformed number near '{s}'")
                })?;
                Ok(Token::Number(n))
            }
            b'"' | b'\'' => {
                self.pos += 1;
                let s = self.read_string(b)?;
                Ok(Token::Str(s))
            }
            // (frankenredis-h1vbd) Long-string with optional level
            // markers: `[=*[` ... `]=*]`. Falls through to LBracket
            // when the `[` isn't followed by `=*[`.
            b'[' if self.try_long_bracket_open().is_some() => {
                let level = self.try_long_bracket_open().unwrap();
                self.pos += level + 2; // consume `[<level eq>[`
                let s = self.read_long_string(level)?;
                Ok(Token::Str(s))
            }
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => {
                let start = self.pos;
                while let Some(c) = self.peek_byte() {
                    if c.is_ascii_alphanumeric() || c == b'_' {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                let name = std::str::from_utf8(&self.src[start..self.pos])
                    .map_err(|_| "invalid identifier")?;
                let tok = match name {
                    "and" => Token::And,
                    "break" => Token::Break,
                    "do" => Token::Do,
                    "else" => Token::Else,
                    "elseif" => Token::ElseIf,
                    "end" => Token::End,
                    "false" => Token::False,
                    "for" => Token::For,
                    "function" => Token::Function,
                    "if" => Token::If,
                    "in" => Token::In,
                    "local" => Token::Local,
                    "nil" => Token::Nil,
                    "not" => Token::Not,
                    "or" => Token::Or,
                    "repeat" => Token::Repeat,
                    "return" => Token::Return,
                    "then" => Token::Then,
                    "true" => Token::True,
                    "until" => Token::Until,
                    "while" => Token::While,
                    _ => Token::Name(name.to_string()),
                };
                Ok(tok)
            }
            b'+' => {
                self.pos += 1;
                Ok(Token::Plus)
            }
            b'-' => {
                self.pos += 1;
                Ok(Token::Minus)
            }
            b'*' => {
                self.pos += 1;
                Ok(Token::Star)
            }
            b'/' => {
                self.pos += 1;
                Ok(Token::Slash)
            }
            b'%' => {
                self.pos += 1;
                Ok(Token::Percent)
            }
            b'^' => {
                self.pos += 1;
                Ok(Token::Caret)
            }
            b'#' => {
                self.pos += 1;
                Ok(Token::Hash)
            }
            b'(' => {
                self.pos += 1;
                Ok(Token::LParen)
            }
            b')' => {
                self.pos += 1;
                Ok(Token::RParen)
            }
            b'[' => {
                self.pos += 1;
                Ok(Token::LBracket)
            }
            b']' => {
                self.pos += 1;
                Ok(Token::RBracket)
            }
            b'{' => {
                self.pos += 1;
                Ok(Token::LBrace)
            }
            b'}' => {
                self.pos += 1;
                Ok(Token::RBrace)
            }
            b',' => {
                self.pos += 1;
                Ok(Token::Comma)
            }
            b';' => {
                self.pos += 1;
                Ok(Token::Semi)
            }
            b':' => {
                self.pos += 1;
                Ok(Token::Colon)
            }
            b'=' => {
                self.pos += 1;
                if self.peek_byte() == Some(b'=') {
                    self.pos += 1;
                    Ok(Token::EqEq)
                } else {
                    Ok(Token::Eq)
                }
            }
            b'~' => {
                self.pos += 1;
                if self.peek_byte() == Some(b'=') {
                    self.pos += 1;
                    Ok(Token::TildeEq)
                } else {
                    // (br-frankenredis-fo1s) — match upstream Lua's
                    // "unexpected symbol near '~'" wording.
                    Err("unexpected symbol near '~'".to_string())
                }
            }
            b'<' => {
                self.pos += 1;
                if self.peek_byte() == Some(b'=') {
                    self.pos += 1;
                    Ok(Token::LtEq)
                } else {
                    Ok(Token::Lt)
                }
            }
            b'>' => {
                self.pos += 1;
                if self.peek_byte() == Some(b'=') {
                    self.pos += 1;
                    Ok(Token::GtEq)
                } else {
                    Ok(Token::Gt)
                }
            }
            b'.' => {
                self.pos += 1;
                if self.peek_byte() == Some(b'.') {
                    self.pos += 1;
                    if self.peek_byte() == Some(b'.') {
                        self.pos += 1;
                        Ok(Token::Dots)
                    } else {
                        Ok(Token::DotDot)
                    }
                } else if self.peek_byte().is_some_and(|d| d.is_ascii_digit()) {
                    // Decimal number starting with .
                    let start = self.pos - 1;
                    while let Some(d) = self.peek_byte() {
                        if d.is_ascii_digit() {
                            self.pos += 1;
                        } else {
                            break;
                        }
                    }
                    let s = std::str::from_utf8(&self.src[start..self.pos])
                        .map_err(|_| "invalid number")?;
                    let n: f64 = s
                        .parse()
                        .map_err(|e: std::num::ParseFloatError| e.to_string())?;
                    Ok(Token::Number(n))
                } else {
                    Ok(Token::Dot)
                }
            }
            _ => {
                self.pos += 1;
                // (br-frankenredis-fo1s) — match upstream Lua's
                // "unexpected symbol near '<char>'" wording.
                Err(format!("unexpected symbol near '{}'", b as char))
            }
        }
    }

    #[allow(dead_code)]
    fn tokenize_all(&mut self) -> Result<Vec<Token>, String> {
        Ok(self.tokenize_all_with_lines()?.0)
    }

    /// Tokenize, also returning the 1-based source line where each token begins
    /// (parallel to the token vec, including the trailing `Eof`). Used to thread
    /// real line numbers into Lua error messages. (frankenredis-m7oy8)
    #[allow(dead_code)]
    fn tokenize_all_with_lines(&mut self) -> Result<(Vec<Token>, Vec<u32>), String> {
        // Bare-message form — preserves the contract tokenize_all / loadstring
        // assert on. (frankenredis-5qhz7)
        self.tokenize_all_located().map_err(|(_, msg)| msg)
    }

    /// Like `tokenize_all_with_lines`, but a lexer error carries the 1-based
    /// line where it occurred so callers can render `user_script:N`.
    /// (frankenredis-5qhz7)
    fn tokenize_all_located(&mut self) -> Result<(Vec<Token>, Vec<u32>), (u32, String)> {
        let mut tokens = Vec::new();
        let mut lines = Vec::new();
        let mut cur_line: u32 = 1;
        let mut counted: usize = 0;
        loop {
            // Position at the next token's first byte, then count newlines in
            // the gap since the previous token (O(n) total, not O(n^2)).
            self.skip_whitespace_and_comments().map_err(|e| {
                let upto = self.pos.min(self.src.len());
                let ln = cur_line
                    + self.src[counted..upto]
                        .iter()
                        .filter(|&&b| b == b'\n')
                        .count() as u32;
                (ln, e)
            })?;
            let start = self.pos.min(self.src.len());
            cur_line += self.src[counted..start]
                .iter()
                .filter(|&&b| b == b'\n')
                .count() as u32;
            counted = start;
            let tok = self.next_token().map_err(|e| (cur_line, e))?;
            let is_eof = tok == Token::Eof;
            tokens.push(tok);
            lines.push(cur_line);
            if is_eof {
                break;
            }
        }
        Ok((tokens, lines))
    }
}

// ── AST ─────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub enum Expr {
    Nil,
    Bool(bool),
    Number(f64),
    Str(Vec<u8>),
    Name(String),
    LocalName(String, LocalSlotRef),
    VarArgs,
    BinOp(Box<Expr>, BinOp, Box<Expr>),
    UnaryOp(UnaryOp, Box<Expr>),
    Index(Box<Expr>, Box<Expr>),
    Field(Box<Expr>, String),
    Call(Box<Expr>, Vec<Expr>),
    MethodCall(Box<Expr>, String, Vec<Expr>),
    TableConstructor(Vec<TableField>),
    FunctionDef(Vec<String>, bool, Block),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalSlotRef {
    depth: usize,
    slot: usize,
}

#[derive(Clone, Debug)]
pub enum TableField {
    Index(Expr, Expr),
    Named(String, Expr),
    Positional(Expr),
}

#[derive(Clone, Debug)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    Concat,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    And,
    Or,
}

#[derive(Clone, Debug)]
pub enum UnaryOp {
    Neg,
    Not,
    Len,
}

#[derive(Clone, Debug)]
pub enum Stmt {
    Assign(Vec<Expr>, Vec<Expr>),
    LocalAssign(Vec<String>, Vec<Expr>),
    Expression(Expr),
    If(Vec<(Expr, Block)>, Option<Block>),
    NumericFor(String, Expr, Expr, Option<Expr>, Block),
    GenericFor(Vec<String>, Vec<Expr>, Block),
    While(Expr, Block),
    Repeat(Block, Expr),
    DoBlock(Block),
    Return(Vec<Expr>),
    Break,
    FunctionDecl(Vec<String>, Vec<String>, bool, Block),
    LocalFunctionDecl(String, Vec<String>, bool, Block),
}

// ── Parser ──────────────────────────────────────────────────────────────

struct Parser {
    tokens: Vec<Token>,
    /// 1-based source line for each token (parallel to `tokens`). (m7oy8)
    lines: Vec<u32>,
    pos: usize,
    /// (frankenredis-i0h24) Lexical loop nesting depth — used so the
    /// parser can reject `break` outside any loop with vendored's
    /// "no loop to break near '<eof>'" message. Incremented on
    /// while/repeat/for entry, decremented on exit.
    loop_depth: usize,
}

impl Parser {
    #[allow(dead_code)]
    fn new(tokens: Vec<Token>) -> Self {
        // Default line map (all 1) for callers that don't supply one — keeps
        // the line numbers harmless when source positions aren't tracked.
        let lines = vec![1u32; tokens.len()];
        Self::with_lines(tokens, lines)
    }

    fn with_lines(tokens: Vec<Token>, lines: Vec<u32>) -> Self {
        Self {
            tokens,
            lines,
            pos: 0,
            loop_depth: 0,
        }
    }

    /// 1-based source line of the token at the parser cursor. (m7oy8)
    fn cur_line(&self) -> u32 {
        self.lines.get(self.pos).copied().unwrap_or(1)
    }

    /// 1-based line of the parse-error position, clamped to the last real token
    /// so an error at `<eof>` reports the final line rather than 1. Used to
    /// surface the true compile-error line in `user_script:N`. (frankenredis-5qhz7)
    fn error_line(&self) -> u32 {
        let idx = self.pos.min(self.lines.len().saturating_sub(1));
        self.lines.get(idx).copied().unwrap_or(1)
    }

    fn peek(&self) -> &Token {
        self.tokens.get(self.pos).unwrap_or(&Token::Eof)
    }

    fn advance(&mut self) -> Token {
        let tok = self.tokens.get(self.pos).cloned().unwrap_or(Token::Eof);
        self.pos += 1;
        tok
    }

    fn expect(&mut self, expected: &Token) -> Result<(), String> {
        let tok = self.advance();
        if std::mem::discriminant(&tok) == std::mem::discriminant(expected) {
            Ok(())
        } else {
            // (frankenredis-i0h24) Use upstream's "X expected near Y" wording.
            Err(parser_expected_near(expected, &tok))
        }
    }

    /// Lua 5.1 lparser.c::check_match — consume a block closer (`end`/`until`), or, when it is
    /// missing, raise the diagnostic Lua does: if the opener sits on a DIFFERENT source line
    /// than the (unexpected) current token, append "(to close '<opener>' at line <N>)";
    /// otherwise fall back to the plain "'<close>' expected near '<got>'". The same-line case
    /// is why a one-line `if true then return 1` reports just "'end' expected near '<eof>'".
    /// (frankenredis-5qhz7)
    fn check_match(&mut self, close: &Token, opener: &str, opener_line: u32) -> Result<(), String> {
        let got_line = self.error_line();
        let tok = self.advance();
        if std::mem::discriminant(&tok) == std::mem::discriminant(close) {
            return Ok(());
        }
        if got_line == opener_line {
            Err(parser_expected_near(close, &tok))
        } else {
            Err(format!(
                "'{}' expected (to close '{}' at line {}) near '{}'",
                expected_token_label(close),
                opener,
                opener_line,
                token_display(&tok),
            ))
        }
    }

    fn check(&self, expected: &Token) -> bool {
        std::mem::discriminant(self.peek()) == std::mem::discriminant(expected)
    }

    fn parse_block(&mut self) -> Result<Block, String> {
        let mut stmts: Block = Vec::new();
        loop {
            // Skip semicolons
            while self.check(&Token::Semi) {
                self.advance();
            }
            match self.peek() {
                Token::End | Token::Else | Token::ElseIf | Token::Until | Token::Eof => break,
                _ => {
                    // (frankenredis-yunl8) Upstream Lua 5.1 lparser.c
                    // treats `return` as the terminal statement of a
                    // block — once parsed, the block loop exits. Any
                    // extra tokens then surface at the chunk-level
                    // check_match(<eos>) and report
                    // `'<eof>' expected near '<token>'`. fr previously
                    // kept looping past the return and tried to parse
                    // the trailing token as a new statement, surfacing
                    // a generic "unexpected symbol near 'X'" instead.
                    let is_return = matches!(self.peek(), Token::Return);
                    let line = self.cur_line();
                    let stmt = self.parse_statement()?;
                    stmts.push((line, stmt));
                    if is_return {
                        break;
                    }
                }
            }
        }
        Ok(stmts)
    }

    fn parse_statement(&mut self) -> Result<Stmt, String> {
        match self.peek().clone() {
            Token::If => self.parse_if(),
            Token::While => self.parse_while(),
            Token::Repeat => self.parse_repeat(),
            Token::For => self.parse_for(),
            Token::Do => {
                let opener_line = self.cur_line();
                self.advance();
                let body = self.parse_block()?;
                self.check_match(&Token::End, "do", opener_line)?;
                Ok(Stmt::DoBlock(body))
            }
            Token::Local => self.parse_local(),
            Token::Return => self.parse_return(),
            Token::Break => {
                // (frankenredis-i0h24) Upstream lparser.c::breakstat
                // raises "no loop to break" when the current function's
                // breaklist is empty.
                if self.loop_depth == 0 {
                    self.advance();
                    return Err(format!(
                        "no loop to break near '{}'",
                        token_display(self.peek()),
                    ));
                }
                self.advance();
                Ok(Stmt::Break)
            }
            Token::Function => self.parse_function_decl(),
            _ => self.parse_expr_or_assign(),
        }
    }

    fn parse_if(&mut self) -> Result<Stmt, String> {
        let opener_line = self.cur_line();
        self.advance(); // 'if'
        let mut branches = Vec::new();
        let cond = self.parse_expr()?;
        self.expect(&Token::Then)?;
        let body = self.parse_block()?;
        branches.push((cond, body));

        let mut else_body = None;
        loop {
            if self.check(&Token::ElseIf) {
                self.advance();
                let cond = self.parse_expr()?;
                self.expect(&Token::Then)?;
                let body = self.parse_block()?;
                branches.push((cond, body));
            } else if self.check(&Token::Else) {
                self.advance();
                else_body = Some(self.parse_block()?);
                break;
            } else {
                break;
            }
        }
        self.check_match(&Token::End, "if", opener_line)?;
        Ok(Stmt::If(branches, else_body))
    }

    fn parse_while(&mut self) -> Result<Stmt, String> {
        let opener_line = self.cur_line();
        self.advance(); // 'while'
        let cond = self.parse_expr()?;
        self.expect(&Token::Do)?;
        self.loop_depth += 1;
        let body_result = self.parse_block();
        self.loop_depth -= 1;
        let body = body_result?;
        self.check_match(&Token::End, "while", opener_line)?;
        Ok(Stmt::While(cond, body))
    }

    fn parse_repeat(&mut self) -> Result<Stmt, String> {
        let opener_line = self.cur_line();
        self.advance(); // 'repeat'
        self.loop_depth += 1;
        let body_result = self.parse_block();
        self.loop_depth -= 1;
        let body = body_result?;
        self.check_match(&Token::Until, "repeat", opener_line)?;
        let cond = self.parse_expr()?;
        Ok(Stmt::Repeat(body, cond))
    }

    fn parse_for(&mut self) -> Result<Stmt, String> {
        let opener_line = self.cur_line();
        self.advance(); // 'for'
        let name = match self.advance() {
            Token::Name(n) => n,
            t => return Err(parser_name_expected_near(&t)),
        };

        if self.check(&Token::Eq) {
            // Numeric for: for name = start, stop [, step] do ... end
            self.advance(); // '='
            let start = self.parse_expr()?;
            self.expect(&Token::Comma)?;
            let stop = self.parse_expr()?;
            let step = if self.check(&Token::Comma) {
                self.advance();
                Some(self.parse_expr()?)
            } else {
                None
            };
            self.expect(&Token::Do)?;
            self.loop_depth += 1;
            let body_result = self.parse_block();
            self.loop_depth -= 1;
            let body = body_result?;
            self.check_match(&Token::End, "for", opener_line)?;
            Ok(Stmt::NumericFor(name, start, stop, step, body))
        } else {
            // Generic for: for name [, name ...] in explist do ... end
            let mut names = vec![name];
            while self.check(&Token::Comma) {
                self.advance();
                match self.advance() {
                    Token::Name(n) => names.push(n),
                    t => return Err(parser_name_expected_near(&t)),
                }
            }
            self.expect(&Token::In)?;
            let exprs = self.parse_expr_list()?;
            self.expect(&Token::Do)?;
            self.loop_depth += 1;
            let body_result = self.parse_block();
            self.loop_depth -= 1;
            let body = body_result?;
            self.check_match(&Token::End, "for", opener_line)?;
            Ok(Stmt::GenericFor(names, exprs, body))
        }
    }

    fn parse_local(&mut self) -> Result<Stmt, String> {
        self.advance(); // 'local'
        if self.check(&Token::Function) {
            let fn_line = self.cur_line();
            self.advance(); // 'function'
            let name = match self.advance() {
                Token::Name(n) => n,
                t => return Err(parser_name_expected_near(&t)),
            };
            let (params, is_variadic, body) = self.parse_func_body(fn_line)?;
            return Ok(Stmt::LocalFunctionDecl(name, params, is_variadic, body));
        }

        let mut names = Vec::new();
        match self.advance() {
            Token::Name(n) => names.push(n),
            t => return Err(parser_name_expected_near(&t)),
        }
        while self.check(&Token::Comma) {
            self.advance();
            match self.advance() {
                Token::Name(n) => names.push(n),
                t => return Err(parser_name_expected_near(&t)),
            }
        }
        let exprs = if self.check(&Token::Eq) {
            self.advance();
            self.parse_expr_list()?
        } else {
            Vec::new()
        };
        Ok(Stmt::LocalAssign(names, exprs))
    }

    fn parse_return(&mut self) -> Result<Stmt, String> {
        self.advance(); // 'return'
        let exprs = match self.peek() {
            Token::End | Token::Else | Token::ElseIf | Token::Until | Token::Eof | Token::Semi => {
                Vec::new()
            }
            _ => self.parse_expr_list()?,
        };
        // Optional semicolon after return
        if self.check(&Token::Semi) {
            self.advance();
        }
        Ok(Stmt::Return(exprs))
    }

    fn parse_function_decl(&mut self) -> Result<Stmt, String> {
        let fn_line = self.cur_line();
        self.advance(); // 'function'
        let mut names = Vec::new();
        match self.advance() {
            Token::Name(n) => names.push(n),
            t => return Err(parser_name_expected_near(&t)),
        }
        while self.check(&Token::Dot) {
            self.advance();
            match self.advance() {
                Token::Name(n) => names.push(n),
                t => return Err(parser_name_expected_near(&t)),
            }
        }
        let (params, is_variadic, body) = self.parse_func_body(fn_line)?;
        Ok(Stmt::FunctionDecl(names, params, is_variadic, body))
    }

    fn parse_func_body(&mut self, opener_line: u32) -> Result<(Vec<String>, bool, Block), String> {
        self.expect(&Token::LParen)?;
        let mut params = Vec::new();
        let mut is_variadic = false;
        if !self.check(&Token::RParen) {
            loop {
                if self.check(&Token::Dots) {
                    self.advance();
                    is_variadic = true;
                    break;
                }
                match self.advance() {
                    Token::Name(n) => params.push(n),
                    t => return Err(parser_name_expected_near(&t)),
                }
                if !self.check(&Token::Comma) {
                    break;
                }
                self.advance();
            }
        }
        self.expect(&Token::RParen)?;
        let body = self.parse_block()?;
        self.check_match(&Token::End, "function", opener_line)?;
        Ok((params, is_variadic, body))
    }

    fn parse_expr_or_assign(&mut self) -> Result<Stmt, String> {
        // (frankenredis-cdfpx) Upstream lparser.c restricts statement
        // starts to keywords, Name, or '(' for a parenthesized
        // prefixexp. Anything else (Number, Str, True, False, Nil,
        // operators that aren't statement starts) is rejected with
        // 'unexpected symbol near <X>'. fr's parse_suffixed_expr will
        // happily start an expression on Number/Str/Bool/Nil and only
        // fail later with the assignment wording, so guard here so
        // SCRIPT LOAD / loadstring on '123' / '1.5' / '"x"' / 'true'
        // surface the right wording.
        if !matches!(self.peek(), Token::Name(_) | Token::LParen) {
            return Err(format!(
                "unexpected symbol near '{}'",
                token_display(self.peek())
            ));
        }
        let expr = self.parse_suffixed_expr()?;

        // Check for assignment
        if self.check(&Token::Comma) || self.check(&Token::Eq) {
            let mut lhs = vec![expr];
            while self.check(&Token::Comma) {
                self.advance();
                lhs.push(self.parse_suffixed_expr()?);
            }
            self.expect(&Token::Eq)?;
            // (frankenredis-s9mxn) Upstream Lua's grammar restricts
            // varlist to Name | prefixexp '[' exp ']' | prefixexp '.'
            // Name. Literals, operators, and function-call results are
            // NOT valid assignment targets, and vendored's parser
            // rejects them at compile time (`SCRIPT LOAD "1 = 2"` ->
            // "unexpected symbol near '1'"). fr's parser accepted any
            // suffixed expression on the LHS, deferring rejection to
            // runtime where `invalid assignment target` fires only on
            // EVAL/EVALSHA — meaning SCRIPT LOAD would happily cache a
            // sha for malformed source. Validate each LHS entry here so
            // the parse fails before the sha is returned.
            for lvalue in &lhs {
                if !matches!(
                    lvalue,
                    Expr::Name(_) | Expr::LocalName(_, _) | Expr::Index(_, _) | Expr::Field(_, _)
                ) {
                    return Err(format!(
                        "syntax error near '=': '{}' is not a valid assignment target",
                        lua_lvalue_first_token(lvalue)
                    ));
                }
            }
            let rhs = self.parse_expr_list()?;
            Ok(Stmt::Assign(lhs, rhs))
        } else {
            // Lua's grammar restricts expression statements to function
            // calls only:
            //   stat ::= varlist '=' explist | functioncall | ...
            // A bare `var` (Name / Field / Index) is *not* a valid
            // statement. Upstream Lua's parser surfaces this as
            // "'=' expected near '<token>'" because after parsing the
            // var it expects either ',' or '=' to start an assignment.
            // fr was wrapping any suffixed expression in
            // Stmt::Expression and silently executing it as nil-discard,
            // so SCRIPT LOAD / EVAL of `'invalid lua'`, `'foo bar'`,
            // `'foo'` were succeeding instead of erroring.
            // (frankenredis-luabarestmt)
            match &expr {
                Expr::Call(_, _) | Expr::MethodCall(_, _, _) => Ok(Stmt::Expression(expr)),
                _ => Err(format!(
                    "'=' expected near '{}'",
                    token_display(self.peek())
                )),
            }
        }
    }

    fn parse_expr_list(&mut self) -> Result<Vec<Expr>, String> {
        let mut exprs = vec![self.parse_expr()?];
        while self.check(&Token::Comma) {
            self.advance();
            exprs.push(self.parse_expr()?);
        }
        Ok(exprs)
    }

    // Expression parsing with precedence climbing
    fn parse_expr(&mut self) -> Result<Expr, String> {
        self.parse_or_expr()
    }

    fn parse_or_expr(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_and_expr()?;
        while self.check(&Token::Or) {
            self.advance();
            let right = self.parse_and_expr()?;
            left = Expr::BinOp(Box::new(left), BinOp::Or, Box::new(right));
        }
        Ok(left)
    }

    fn parse_and_expr(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_comparison()?;
        while self.check(&Token::And) {
            self.advance();
            let right = self.parse_comparison()?;
            left = Expr::BinOp(Box::new(left), BinOp::And, Box::new(right));
        }
        Ok(left)
    }

    fn parse_comparison(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_concat()?;
        loop {
            let op = match self.peek() {
                Token::EqEq => BinOp::Eq,
                Token::TildeEq => BinOp::Ne,
                Token::Lt => BinOp::Lt,
                Token::Gt => BinOp::Gt,
                Token::LtEq => BinOp::Le,
                Token::GtEq => BinOp::Ge,
                _ => break,
            };
            self.advance();
            let right = self.parse_concat()?;
            left = Expr::BinOp(Box::new(left), op, Box::new(right));
        }
        Ok(left)
    }

    fn parse_concat(&mut self) -> Result<Expr, String> {
        let left = self.parse_add_sub()?;
        // .. is right-associative
        if self.check(&Token::DotDot) {
            self.advance();
            let right = self.parse_concat()?;
            Ok(Expr::BinOp(Box::new(left), BinOp::Concat, Box::new(right)))
        } else {
            Ok(left)
        }
    }

    fn parse_add_sub(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_mul_div()?;
        loop {
            let op = match self.peek() {
                Token::Plus => BinOp::Add,
                Token::Minus => BinOp::Sub,
                _ => break,
            };
            self.advance();
            let right = self.parse_mul_div()?;
            left = Expr::BinOp(Box::new(left), op, Box::new(right));
        }
        Ok(left)
    }

    fn parse_mul_div(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Token::Star => BinOp::Mul,
                Token::Slash => BinOp::Div,
                Token::Percent => BinOp::Mod,
                _ => break,
            };
            self.advance();
            let right = self.parse_unary()?;
            left = Expr::BinOp(Box::new(left), op, Box::new(right));
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Expr, String> {
        match self.peek().clone() {
            Token::Not => {
                self.advance();
                let expr = self.parse_unary()?;
                Ok(Expr::UnaryOp(UnaryOp::Not, Box::new(expr)))
            }
            Token::Minus => {
                self.advance();
                let expr = self.parse_unary()?;
                Ok(Expr::UnaryOp(UnaryOp::Neg, Box::new(expr)))
            }
            Token::Hash => {
                self.advance();
                let expr = self.parse_power()?;
                Ok(Expr::UnaryOp(UnaryOp::Len, Box::new(expr)))
            }
            _ => self.parse_power(),
        }
    }

    fn parse_power(&mut self) -> Result<Expr, String> {
        let base = self.parse_suffixed_expr()?;
        // ^ is right-associative
        if self.check(&Token::Caret) {
            self.advance();
            let exp = self.parse_unary()?;
            Ok(Expr::BinOp(Box::new(base), BinOp::Pow, Box::new(exp)))
        } else {
            Ok(base)
        }
    }

    fn parse_suffixed_expr(&mut self) -> Result<Expr, String> {
        // (frankenredis-oee7k) Upstream Lua 5.1's grammar distinguishes
        // primaryexp (Name | `(`exp`)`) — which is suffixable via
        // var/functioncall productions — from simpleexp (numeric/string/
        // nil/true/false/table/function literals + varargs) which is
        // never suffixed. fr previously routed every literal through
        // suffix parsing, so `1[2]`, `nil.foo`, `'abc':upper()` all
        // parsed (the last one even RAN). Track whether the primary
        // came from the suffixable subset and exit the loop early
        // otherwise.
        let (mut expr, suffixable) = self.parse_primary_with_kind()?;
        if !suffixable {
            return Ok(expr);
        }
        loop {
            match self.peek().clone() {
                Token::Dot => {
                    self.advance();
                    match self.advance() {
                        Token::Name(n) => expr = Expr::Field(Box::new(expr), n),
                        t => return Err(parser_name_expected_near(&t)),
                    }
                }
                Token::LBracket => {
                    self.advance();
                    let idx = self.parse_expr()?;
                    self.expect(&Token::RBracket)?;
                    expr = Expr::Index(Box::new(expr), Box::new(idx));
                }
                Token::Colon => {
                    self.advance();
                    let method = match self.advance() {
                        Token::Name(n) => n,
                        t => return Err(parser_name_expected_near(&t)),
                    };
                    let args = self.parse_call_args()?;
                    expr = Expr::MethodCall(Box::new(expr), method, args);
                }
                Token::LParen | Token::LBrace | Token::Str(_) => {
                    let args = self.parse_call_args()?;
                    expr = Expr::Call(Box::new(expr), args);
                }
                _ => break,
            }
        }
        Ok(expr)
    }

    /// (frankenredis-oee7k) Parses a primary expression and returns
    /// it along with a flag indicating whether the value can be
    /// suffixed by `.`/`[`/`:`/call args. Only Name and parenthesized
    /// expressions match upstream's `primaryexp` production; literals
    /// (number/string/bool/nil/table/function/varargs) match
    /// `simpleexp` and bypass the suffix loop.
    fn parse_primary_with_kind(&mut self) -> Result<(Expr, bool), String> {
        match self.peek().clone() {
            Token::Name(n) => {
                self.advance();
                Ok((Expr::Name(n), true))
            }
            Token::LParen => {
                self.advance();
                let expr = self.parse_expr()?;
                self.expect(&Token::RParen)?;
                Ok((expr, true))
            }
            _ => Ok((self.parse_primary()?, false)),
        }
    }

    fn parse_call_args(&mut self) -> Result<Vec<Expr>, String> {
        match self.peek().clone() {
            Token::LParen => {
                self.advance();
                let args = if self.check(&Token::RParen) {
                    Vec::new()
                } else {
                    self.parse_expr_list()?
                };
                self.expect(&Token::RParen)?;
                Ok(args)
            }
            Token::LBrace => {
                let table = self.parse_table_constructor()?;
                Ok(vec![table])
            }
            Token::Str(s) => {
                self.advance();
                Ok(vec![Expr::Str(s)])
            }
            // (frankenredis-yunl8) Mirror upstream llex.c wording:
            // "function arguments expected near '<token>'". The
            // current peek token names the place where the missing
            // args were expected.
            _ => Err(format!(
                "function arguments expected near '{}'",
                token_display(self.peek())
            )),
        }
    }

    fn parse_primary(&mut self) -> Result<Expr, String> {
        match self.peek().clone() {
            Token::Name(n) => {
                self.advance();
                Ok(Expr::Name(n))
            }
            Token::LParen => {
                self.advance();
                let expr = self.parse_expr()?;
                self.expect(&Token::RParen)?;
                Ok(expr)
            }
            Token::Number(n) => {
                self.advance();
                Ok(Expr::Number(n))
            }
            Token::Str(s) => {
                self.advance();
                Ok(Expr::Str(s))
            }
            Token::True => {
                self.advance();
                Ok(Expr::Bool(true))
            }
            Token::False => {
                self.advance();
                Ok(Expr::Bool(false))
            }
            Token::Nil => {
                self.advance();
                Ok(Expr::Nil)
            }
            Token::LBrace => self.parse_table_constructor(),
            Token::Function => {
                let fn_line = self.cur_line();
                self.advance();
                let (params, is_variadic, body) = self.parse_func_body(fn_line)?;
                Ok(Expr::FunctionDef(params, is_variadic, body))
            }
            Token::Dots => {
                self.advance();
                Ok(Expr::VarArgs)
            }
            t => Err(format!("unexpected symbol near '{}'", token_display(&t))),
        }
    }

    fn parse_table_constructor(&mut self) -> Result<Expr, String> {
        self.expect(&Token::LBrace)?;
        let mut fields = Vec::new();
        while !self.check(&Token::RBrace) {
            if self.check(&Token::LBracket) {
                // [expr] = expr
                self.advance();
                let key = self.parse_expr()?;
                self.expect(&Token::RBracket)?;
                self.expect(&Token::Eq)?;
                let val = self.parse_expr()?;
                fields.push(TableField::Index(key, val));
            } else if let Token::Name(n) = self.peek().clone() {
                // Could be name = expr or just expr
                let saved_pos = self.pos;
                self.advance();
                if self.check(&Token::Eq) {
                    self.advance();
                    let val = self.parse_expr()?;
                    fields.push(TableField::Named(n, val));
                } else {
                    // Rewind and parse as expression
                    self.pos = saved_pos;
                    let val = self.parse_expr()?;
                    fields.push(TableField::Positional(val));
                }
            } else {
                let val = self.parse_expr()?;
                fields.push(TableField::Positional(val));
            }

            // Field separator: , or ;
            if self.check(&Token::Comma) || self.check(&Token::Semi) {
                self.advance();
            } else {
                break;
            }
        }
        self.expect(&Token::RBrace)?;
        Ok(Expr::TableConstructor(fields))
    }
}

// ── Local slot resolver ──────────────────────────────────────────────────

#[derive(Default)]
struct ResolveScope {
    locals: Vec<String>,
}

struct LocalResolver {
    scopes: Vec<ResolveScope>,
}

impl LocalResolver {
    fn new() -> Self {
        Self {
            scopes: vec![ResolveScope::default()],
        }
    }

    fn enter_scope(&mut self) {
        self.scopes.push(ResolveScope::default());
    }

    fn exit_scope(&mut self) {
        let _ = self.scopes.pop();
    }

    fn declare_local(&mut self, name: &str) {
        let Some(scope) = self.scopes.last_mut() else {
            return;
        };
        if scope
            .locals
            .iter()
            .rposition(|local| local == name)
            .is_none()
        {
            scope.locals.push(name.to_string());
        }
    }

    fn resolve_local(&self, name: &str) -> Option<LocalSlotRef> {
        for (scope_idx, scope) in self.scopes.iter().enumerate().rev() {
            if let Some(slot) = scope.locals.iter().rposition(|local| local == name) {
                return Some(LocalSlotRef {
                    depth: self.scopes.len() - 1 - scope_idx,
                    slot,
                });
            }
        }
        None
    }

    fn resolve_child_block(&mut self, body: &mut Block) {
        self.enter_scope();
        self.resolve_stmts(body);
        self.exit_scope();
    }

    fn resolve_function_body(&mut self, params: &[String], body: &mut Block) {
        self.enter_scope();
        for param in params {
            self.declare_local(param);
        }
        self.resolve_stmts(body);
        self.exit_scope();
    }

    fn resolve_stmts(&mut self, stmts: &mut Block) {
        for (_, stmt) in stmts {
            self.resolve_stmt(stmt);
        }
    }

    fn resolve_stmt(&mut self, stmt: &mut Stmt) {
        match stmt {
            Stmt::Assign(lhs, rhs) => {
                for expr in lhs {
                    self.resolve_expr(expr);
                }
                for expr in rhs {
                    self.resolve_expr(expr);
                }
            }
            Stmt::LocalAssign(names, exprs) => {
                for expr in exprs {
                    self.resolve_expr(expr);
                }
                for name in names {
                    self.declare_local(name);
                }
            }
            Stmt::Expression(expr) => self.resolve_expr(expr),
            Stmt::If(branches, else_body) => {
                for (cond, body) in branches {
                    self.resolve_expr(cond);
                    self.resolve_child_block(body);
                }
                if let Some(body) = else_body {
                    self.resolve_child_block(body);
                }
            }
            Stmt::NumericFor(name, start, stop, step, body) => {
                self.resolve_expr(start);
                self.resolve_expr(stop);
                if let Some(step) = step {
                    self.resolve_expr(step);
                }
                self.enter_scope();
                self.declare_local(name);
                self.resolve_stmts(body);
                self.exit_scope();
            }
            Stmt::GenericFor(names, iter_exprs, body) => {
                for expr in iter_exprs {
                    self.resolve_expr(expr);
                }
                self.enter_scope();
                for name in names {
                    self.declare_local(name);
                }
                self.resolve_stmts(body);
                self.exit_scope();
            }
            Stmt::While(cond, body) => {
                self.resolve_expr(cond);
                self.resolve_child_block(body);
            }
            Stmt::Repeat(body, cond) => {
                self.enter_scope();
                self.resolve_stmts(body);
                self.resolve_expr(cond);
                self.exit_scope();
            }
            Stmt::DoBlock(body) => self.resolve_child_block(body),
            Stmt::Return(exprs) => {
                for expr in exprs {
                    self.resolve_expr(expr);
                }
            }
            Stmt::FunctionDecl(_, params, _, body) => self.resolve_function_body(params, body),
            Stmt::LocalFunctionDecl(name, params, _, body) => {
                self.declare_local(name);
                self.resolve_function_body(params, body);
            }
            Stmt::Break => {}
        }
    }

    fn resolve_expr(&mut self, expr: &mut Expr) {
        match expr {
            Expr::Name(name) => {
                if let Some(local) = self.resolve_local(name) {
                    *expr = Expr::LocalName(name.clone(), local);
                }
            }
            Expr::LocalName(_, _) | Expr::Nil | Expr::Bool(_) | Expr::Number(_) | Expr::Str(_) => {}
            Expr::VarArgs => {}
            Expr::BinOp(left, _, right) => {
                self.resolve_expr(left);
                self.resolve_expr(right);
            }
            Expr::UnaryOp(_, inner) => self.resolve_expr(inner),
            Expr::Index(table, key) => {
                self.resolve_expr(table);
                self.resolve_expr(key);
            }
            Expr::Field(table, _) => self.resolve_expr(table),
            Expr::Call(func, args) => {
                self.resolve_expr(func);
                for arg in args {
                    self.resolve_expr(arg);
                }
            }
            Expr::MethodCall(obj, _, args) => {
                self.resolve_expr(obj);
                for arg in args {
                    self.resolve_expr(arg);
                }
            }
            Expr::TableConstructor(fields) => {
                for field in fields {
                    match field {
                        TableField::Index(key, val) => {
                            self.resolve_expr(key);
                            self.resolve_expr(val);
                        }
                        TableField::Named(_, val) | TableField::Positional(val) => {
                            self.resolve_expr(val);
                        }
                    }
                }
            }
            Expr::FunctionDef(params, _, body) => self.resolve_function_body(params, body),
        }
    }
}

fn resolve_lua_local_slots(stmts: &mut Block) {
    LocalResolver::new().resolve_stmts(stmts);
}

// ── Evaluator ───────────────────────────────────────────────────────────

const MAX_CALL_DEPTH: usize = 128;
const MAX_ITERATIONS: u64 = 1_000_000;
const LUA_YIELD_SENTINEL: &str = "__frankenredis_lua_coroutine_yield__";
/// Sentinel error string emitted by `error()` when the argument is a
/// non-string, non-number value (bool/nil/table/function/thread). The
/// real LuaValue is stashed on `LuaState::pending_error_value` and
/// pcall/xpcall splice it back so callers see the original type.
/// (frankenredis-cxmsu)
const LUA_TYPED_ERROR_SENTINEL: &str = "__frankenredis_lua_typed_error__";

/// Prefix tag that marks an `error(...)` string as already carrying the
/// complete RESP error body (so the dispatch wrapper must NOT auto-add
/// the "ERR " code). Used when error() unwraps a `{err = STRING}` table:
/// upstream's protocol path emits the verbatim err field, never adding
/// a code prefix. The tag is stripped by `lua_strip_raw_error_marker`
/// inside `eval_script_error_reply` (fr-command/src/lib.rs).
/// (frankenredis-vkqn0)
pub const LUA_RAW_ERROR_BODY_MARKER: &str = "\u{0001}fr-raw-err\u{0001}";

/// Strip the LUA_RAW_ERROR_BODY_MARKER prefix from an error string if
/// present, returning (stripped_body, was_marker_present). The dispatch
/// wrapper uses `was_marker_present == true` to skip the "ERR "
/// auto-prefix step.
pub fn lua_strip_raw_error_marker(error: &str) -> (String, bool) {
    if let Some(rest) = error.strip_prefix(LUA_RAW_ERROR_BODY_MARKER) {
        (rest.to_string(), true)
    } else {
        (error.to_string(), false)
    }
}

enum ControlFlow {
    None,
    Return(Vec<LuaValue>),
    Break,
}

enum CoroutineRun {
    Complete(Vec<LuaValue>),
    Yield {
        values: Vec<LuaValue>,
        next_pc: usize,
    },
}

pub struct LuaState<'a> {
    pub store: &'a mut Store,
    pub now_ms: u64,
    globals: LuaMap<String, LuaValue>,
    /// (frankenredis-j02x9) Set to true at the start of execute(); once
    /// locked, any write to a top-level global raises "Attempt to modify
    /// a readonly table" and any read of an undefined global raises
    /// "Script attempted to access nonexistent global variable 'NAME'".
    /// Mirrors upstream script_lua.c::luaSetTableProtectionRecursively
    /// applied to the globals table after init, plus the
    /// luaProtectedTableError __index handler.
    globals_locked: bool,
    /// (frankenredis-vr8rg) RESP version the script's `redis.call` /
    /// `redis.pcall` use to materialize replies, toggled by `redis.setresp`.
    /// Defaults to 2 (every script starts in RESP2 regardless of the client's
    /// HELLO version) and is reset at the top of `execute()`. Under 3, the
    /// dispatched command produces RESP3 frames (Double/Map/Set/Null/BigNumber)
    /// which `resp_to_lua` then converts with upstream's RESP3 Lua mapping
    /// (`{double=…}` / `{map=…}` / `{set=…}` / nil / `{big_number=…}`).
    resp_version: i64,
    call_depth: usize,
    /// (frankenredis-0k259) Per-frame kind stack used to satisfy Lua 5.1's
    /// `luaL_where(L, level)` semantics from `error()` / `assert()`. Each
    /// entry is `true` for a Lua function frame (script chunk or
    /// user-defined `function`) and `false` for a C/Rust-builtin frame
    /// (pcall, xpcall, error, assert, etc.). The bottom of the stack is
    /// pushed by `execute()` for the script top-level; `call_function`
    /// maintains the rest. `error(msg, level)` reads the slot at
    /// `len - 1 - level` (skipping the error builtin's own top entry) to
    /// decide whether to prepend the `user_script:1: ` source-location.
    lua_frame_kinds: Vec<bool>,
    iterations: u64,
    rng_seed: u64,
    /// (frankenredis-lwj8o) Lua math.random must produce values
    /// bit-compatible with vendored Redis 7.2.4. Vendored Redis overrides
    /// Lua's C rand()/srand() with its own redisLrand48/redisSrand48 (see
    /// legacy_redis_code/redis/src/rand.c) — a 48-bit drand48-style LCG.
    /// We mirror that algorithm exactly so seeded math.random sequences
    /// match vendored output byte-for-byte.
    lua_random: RedisLrand48,
    script_started_at: Instant,
    current_coroutine: Option<LuaCoroutine>,
    pending_yield: Option<Vec<LuaValue>>,
    /// Original LuaValue passed to `error()` when its type is not
    /// representable as a plain string (bool/nil/table/function/thread).
    /// Set in tandem with returning `Err(LUA_TYPED_ERROR_SENTINEL)`;
    /// consumed atomically via `Option::take` in pcall/xpcall so the
    /// original type round-trips back to the calling script. Cleared on
    /// every `eval_script` call since each script creates a fresh
    /// LuaState. (frankenredis-cxmsu)
    pending_error_value: Option<LuaValue>,
    /// True iff the pending_error_value came from an `error({err=STR})`
    /// unwrap (frankenredis-vkqn0). When the typed-error sentinel
    /// escapes uncaught at the top level, the rendering site prepends
    /// LUA_RAW_ERROR_BODY_MARKER so eval_script_error_reply skips its
    /// "ERR " auto-prefix — matching vendored, which emits the verbatim
    /// err string with no code prefix. Cleared by every consumer that
    /// also clears pending_error_value.
    pending_error_is_raw_body: bool,
    /// Source-location prefix to use for new Lua functions defined while
    /// this state is active. Defaults to `None` (which renders as
    /// "user_script"). loadstring/load functions push their chunk label
    /// onto this slot for the duration of their execution so functions
    /// defined inside the chunk inherit that label. (frankenredis-ycaog)
    current_source_label: Option<String>,
    /// Syntactic call-site name for the currently-dispatching builtin
    /// (e.g. "select" for a direct `select(...)` call or "f" for
    /// `local f = select; f(...)`). `None` means there is no AST
    /// context — Lua 5.1 reports such errors using `'?'` as the name
    /// and omits the `user_script:1:` source-location prefix.
    /// Pushed/restored by `call_function_with_callee` and explicitly
    /// cleared by `pcall`/`xpcall` around their protected callback.
    /// (frankenredis-557p3)
    current_invocation_name: Option<String>,
    /// True when the current invocation entered via method-style call
    /// `t:f(args)` — Lua desugars this to `t.f(t, args...)` and
    /// reports arg #1 type errors with the `'calling 'f' on bad self
    /// (... expected, got ...)'` wording instead of the standard
    /// `'bad argument #1 to 'f' (...)'`. Stashed/restored by
    /// `call_function_with_callee` alongside `current_invocation_name`.
    /// (frankenredis-rbec9)
    current_invocation_is_method: bool,
    /// Depth of nested exec_stmts calls. The coroutine resume path
    /// (exec_coroutine_stmts → exec_stmt) keeps this at 0 for top-
    /// level body statements; any nested control-flow block (for,
    /// while, repeat, if-then, function-call body) bumps it via
    /// exec_stmts. coroutine.yield inspects it: yielding from a
    /// nested scope cannot be resumed by bw15's outer-stmt PC
    /// tracking, so the yield must error rather than silently drop
    /// iterations on resume. (frankenredis-ztawj)
    nested_exec_stmts_depth: usize,
    /// 1-based source line of the statement currently being executed, updated
    /// by `exec_stmts` from the parser's per-statement line tags. Used to stamp
    /// the real line into the `user_script:N:` error prefix at the uncaught-
    /// error boundary (default 1 == upstream chunk start). (frankenredis-m7oy8)
    current_line: u32,
    /// True iff exec_stmt is currently dispatching a bare
    /// Stmt::Expression (a stmt that's a single discarded-result
    /// expression). Combined with nested_exec_stmts_depth == 0,
    /// this is the only context where coroutine.yield can be
    /// resumed correctly: bw15's offset-based PC tracking can
    /// re-enter at next_pc only when yield was the whole stmt
    /// (no surrounding LocalAssign / Assign / Return / function-
    /// arg evaluation that would have its bind step skipped).
    /// (frankenredis-gdbca)
    inside_bare_expression_stmt: bool,
}

/// (frankenredis-lwj8o) Redis 7.2.4 overrides Lua's math.random/randomseed
/// with `redisLrand48` / `redisSrand48` (rand.c) — a portable 48-bit
/// linear congruential generator that produces the SAME sequence on every
/// platform regardless of libc. The algorithm is the BSD/SystemV drand48
/// variant: state is three 16-bit limbs x[0..3], constants a[0..3] and c
/// drive `x = (a * x + c) mod 2^48`, and the public output is a 31-bit
/// integer assembled from the top two limbs.
#[derive(Clone, Debug)]
struct RedisLrand48 {
    x: [u32; 3],
}

impl RedisLrand48 {
    const N: u32 = 16;
    const MASK: u32 = 0xFFFF;
    /// Mirrors `REDIS_LRAND48_MAX` in script_lua.c (2^31 - 1).
    const RAND_MAX: i32 = 0x7fff_ffff;
    /// Seed defaults from upstream rand.c (X0/X1/X2, A0/A1/A2, C).
    const X0: u32 = 0x330E;
    const X1: u32 = 0xABCD;
    const X2: u32 = 0x1234;
    const A0: u64 = 0xE66D;
    const A1: u64 = 0xDEEC;
    const A2: u64 = 0x0005;
    const C: u32 = 0xB;

    fn new() -> Self {
        // upstream's static initializer for x = {X0, X1, X2}.
        Self {
            x: [Self::X0, Self::X1, Self::X2],
        }
    }

    /// Mirror `redisSrand48`: SEED macro keeps x[0] at X0, splits seedval
    /// into low/high 16-bit halves into x[1]/x[2]. a and c are reset to
    /// their constants — but since they're const here they're unchanged.
    fn srand(&mut self, seedval: i32) {
        let s = seedval as u32;
        self.x[0] = Self::X0;
        self.x[1] = s & Self::MASK;
        self.x[2] = (s >> Self::N) & Self::MASK;
    }

    /// Mirror `next()` in rand.c: 48-bit LCG step. Each limb is 16 bits;
    /// the carry chain reproduces the C-level uint16_t arithmetic.
    fn next(&mut self) {
        let mask = Self::MASK;
        let x0 = self.x[0] as u64;
        let x1 = self.x[1] as u64;
        let x2 = self.x[2] as u64;

        // MUL(a[0], x[0], p)
        let l = Self::A0 * x0;
        let mut p0 = (l & mask as u64) as u32;
        let mut p1 = ((l >> Self::N) & mask as u64) as u32;

        // ADDEQU(p[0], c, carry0): p[0] += c, carry0 = overflow.
        let sum = p0 + Self::C;
        let carry0 = if sum > mask { 1u32 } else { 0 };
        p0 = sum & mask;

        // ADDEQU(p[1], carry0, carry1)
        let sum = p1 + carry0;
        let carry1 = if sum > mask { 1u32 } else { 0 };
        p1 = sum & mask;

        // MUL(a[0], x[1], q)
        let l = Self::A0 * x1;
        let q0 = (l & mask as u64) as u32;
        let q1 = ((l >> Self::N) & mask as u64) as u32;

        // ADDEQU(p[1], q[0], carry0)
        let sum = p1 + q0;
        let carry0 = if sum > mask { 1u32 } else { 0 };
        p1 = sum & mask;

        // MUL(a[1], x[0], r)
        let l = Self::A1 * x0;
        let r0 = (l & mask as u64) as u32;
        let r1 = ((l >> Self::N) & mask as u64) as u32;

        // CARRY(p[1], r[0])
        let carry_p1_r0 = if p1 + r0 > mask { 1u32 } else { 0 };

        // x[2] = LOW(carry0 + carry1 + CARRY(p[1], r[0]) + q[1] + r[1] +
        //            a[0]*x[2] + a[1]*x[1] + a[2]*x[0]);
        let raw_x2 = carry0 as u64
            + carry1 as u64
            + carry_p1_r0 as u64
            + q1 as u64
            + r1 as u64
            + Self::A0 * x2
            + Self::A1 * x1
            + Self::A2 * x0;
        self.x[2] = (raw_x2 & mask as u64) as u32;
        // x[1] = LOW(p[1] + r[0]);
        self.x[1] = (p1 + r0) & mask;
        // x[0] = LOW(p[0]);
        self.x[0] = p0 & mask;
    }

    /// Mirror `redisLrand48`: advance state then return the public 31-bit
    /// integer assembled from the top two limbs.
    fn rand(&mut self) -> i32 {
        self.next();
        ((self.x[2] as i32) << (Self::N - 1)) + ((self.x[1] >> 1) as i32)
    }
}

#[derive(Clone, Debug)]
struct LocalBinding {
    name: String,
    cell: LuaCell,
}

#[derive(Clone, Debug)]
struct Scope {
    locals: Vec<LocalBinding>,
}

impl Scope {
    fn new() -> Self {
        Self { locals: Vec::new() }
    }

    fn set_local_cell(&mut self, name: &str, cell: LuaCell) {
        if let Some(existing) = self
            .locals
            .iter_mut()
            .rev()
            .find(|local| local.name == name)
        {
            existing.cell = cell;
        } else {
            self.locals.push(LocalBinding {
                name: name.to_string(),
                cell,
            });
        }
    }

    fn get_local_cell(&self, name: &str) -> Option<&LuaCell> {
        self.locals
            .iter()
            .rev()
            .find(|local| local.name == name)
            .map(|local| &local.cell)
    }

    fn get_local_cell_at(&self, slot: usize) -> Option<&LuaCell> {
        self.locals.get(slot).map(|local| &local.cell)
    }

    fn contains_local(&self, name: &str) -> bool {
        self.locals.iter().rev().any(|local| local.name == name)
    }

    fn captured_locals(&self) -> Vec<(String, LuaCell)> {
        self.locals
            .iter()
            .map(|local| (local.name.clone(), local.cell.clone()))
            .collect()
    }
}

#[derive(Clone, Debug)]
struct Env {
    scopes: Vec<Scope>,
    /// Active Lua 5.1 environment table for unresolved global accesses.
    /// None keeps using the interpreter's default protected globals map.
    global_env: Option<LuaTable>,
    /// Index of the first scope that holds a "local" (vs upvalue) for the
    /// purposes of error messages. Scopes at indices < local_floor are
    /// captured upvalues (Lua 5.1 reports them as "upvalue 'NAME'"); scopes
    /// at indices >= local_floor are locals declared in or below the
    /// current function body. (frankenredis-md71j)
    local_floor: usize,
}

fn is_lua_yield_signal(err: &str) -> bool {
    err == LUA_YIELD_SENTINEL
}

impl Env {
    fn new() -> Self {
        Self {
            scopes: vec![Scope::new()],
            global_env: None,
            local_floor: 0,
        }
    }

    fn current_global_env(&self) -> Option<LuaTable> {
        self.global_env.clone()
    }

    fn set_global_env(&mut self, table: LuaTable) {
        self.global_env = Some(table);
    }

    fn push_scope(&mut self) {
        self.scopes.push(Scope::new());
    }

    fn pop_scope(&mut self) {
        if self.scopes.len() > 1 {
            self.scopes.pop();
        }
    }

    fn set_local(&mut self, name: &str, value: LuaValue) {
        if let Some(scope) = self.scopes.last_mut() {
            let cell = Rc::new(RefCell::new(value));
            // (frankenredis-qqq17) Track upvalue cells: a recursive closure.s
            // captured_env can hold an Rc back to this cell, forming a leak.
            lua_gc_register_cell(&cell);
            scope.set_local_cell(name, cell);
        }
    }

    // (CrimsonHawk) Insert an ALREADY-allocated (and already GC-registered) cell
    // into the top scope. Lets the numeric-for loop reuse one loop-variable cell
    // across iterations instead of allocating + GC-registering a fresh one each
    // time (safe only while nothing captured the previous cell).
    fn set_local_cell_top(&mut self, name: &str, cell: LuaCell) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.set_local_cell(name, cell);
        }
    }

    // (CrimsonHawk) Clone the cell for `name` out of the top scope, if present.
    fn top_local_cell(&self, name: &str) -> Option<LuaCell> {
        self.scopes.last()?.get_local_cell(name).cloned()
    }

    fn get_local(&self, name: &str) -> Option<LuaValue> {
        for scope in self.scopes.iter().rev() {
            if let Some(value) = scope.get_local_cell(name) {
                return Some(value.borrow().clone());
            }
        }
        None
    }

    fn set_existing_local(&mut self, name: &str, value: LuaValue) -> bool {
        for scope in self.scopes.iter().rev() {
            if let Some(existing) = scope.get_local_cell(name) {
                *existing.borrow_mut() = value;
                return true;
            }
        }
        false
    }

    fn scope_index_for_slot(&self, local: LocalSlotRef) -> Option<usize> {
        let from_top = local.depth.checked_add(1)?;
        self.scopes.len().checked_sub(from_top)
    }

    fn get_local_slot(&self, local: LocalSlotRef) -> Option<LuaValue> {
        let scope_idx = self.scope_index_for_slot(local)?;
        let cell = self.scopes.get(scope_idx)?.get_local_cell_at(local.slot)?;
        Some(cell.borrow().clone())
    }

    fn set_existing_local_slot(&mut self, local: LocalSlotRef, value: LuaValue) -> bool {
        let Some(scope_idx) = self.scope_index_for_slot(local) else {
            return false;
        };
        let Some(cell) = self
            .scopes
            .get(scope_idx)
            .and_then(|scope| scope.get_local_cell_at(local.slot))
        else {
            return false;
        };
        *cell.borrow_mut() = value;
        true
    }

    fn classify_slot(&self, local: LocalSlotRef) -> Option<bool> {
        let scope_idx = self.scope_index_for_slot(local)?;
        Some(scope_idx >= self.local_floor)
    }

    /// Snapshot all current scope locals for upvalue capture.
    fn snapshot(&self) -> LuaCapturedEnv {
        self.scopes.iter().map(Scope::captured_locals).collect()
    }

    /// Create an Env pre-loaded with captured upvalue scopes.
    fn from_captured(captured: &[LuaCapturedScope]) -> Self {
        // (frankenredis-md71j) The captured scopes are upvalues from the
        // outer function; any scopes pushed AFTER this point belong to the
        // freshly-entered function body and are reported as "local".
        let local_floor = captured.len();
        Self {
            scopes: captured
                .iter()
                .map(|locals| Scope {
                    locals: locals
                        .iter()
                        .map(|(name, cell)| LocalBinding {
                            name: name.clone(),
                            cell: cell.clone(),
                        })
                        .collect(),
                })
                .collect(),
            global_env: None,
            local_floor,
        }
    }

    /// Classify where `name` lives for error-message purposes:
    /// `Some(true)` if it's a true local of the current function, `Some(false)`
    /// if it's a captured upvalue, `None` if not in any scope.
    /// (frankenredis-md71j)
    fn classify_name(&self, name: &str) -> Option<bool> {
        for (idx, scope) in self.scopes.iter().enumerate().rev() {
            if scope.contains_local(name) {
                return Some(idx >= self.local_floor);
            }
        }
        None
    }
}

thread_local! {
    static LUA_BASE_GLOBALS_TEMPLATE: RefCell<Option<LuaMap<String, LuaValue>>> =
        const { RefCell::new(None) };
}

fn lua_base_globals_template() -> LuaMap<String, LuaValue> {
    LUA_BASE_GLOBALS_TEMPLATE.with(|template| {
        let mut template = template.borrow_mut();
        template
            .get_or_insert_with(build_lua_base_globals_template)
            .clone()
    })
}

fn build_lua_base_globals_template() -> LuaMap<String, LuaValue> {
    let mut globals = LuaMap::default();
    // Register built-in functions.
    for name in &[
        "tonumber",
        "tostring",
        "type",
        "error",
        "pcall",
        "pairs",
        "ipairs",
        "next",
        "unpack",
        "select",
        "rawget",
        "rawset",
        // (frankenredis-uyj7c) rawlen was added in Lua 5.2; vendored Redis
        // ships Lua 5.1, so the global must not be exposed. The dispatch
        // handler is kept for any internal callers but is not bound.
        "setmetatable",
        "getmetatable",
        "newproxy",
        "getfenv",
        "setfenv",
        "assert",
        "xpcall",
        // (frankenredis-cfflo) loadstring/load parse a chunk of source code
        // and return a callable function. Redis only blocks loadfile/dofile/
        // io/os/require/print.
        "loadstring",
        "load",
    ] {
        globals.insert(name.to_string(), LuaValue::RustFunction(name.to_string()));
    }

    let math_table = LuaTable::new_shared_template();
    for name in &[
        "floor",
        "ceil",
        "abs",
        "max",
        "min",
        "sqrt",
        "huge",
        "random",
        "randomseed",
        "fmod",
        "log",
        "log10",
        "exp",
        "pow",
        "sin",
        "cos",
        "tan",
        "asin",
        "acos",
        "atan",
        "atan2",
        "modf",
        "frexp",
        "ldexp",
        "deg",
        "rad",
        "sinh",
        "cosh",
        "tanh",
    ] {
        math_table.set(
            LuaValue::Str(name.as_bytes().to_vec()),
            if *name == "huge" {
                LuaValue::Number(f64::INFINITY)
            } else {
                LuaValue::RustFunction(format!("math.{name}"))
            },
        );
    }
    math_table.set(
        LuaValue::Str(b"pi".to_vec()),
        LuaValue::Number(std::f64::consts::PI),
    );
    globals.insert("math".to_string(), LuaValue::Table(math_table));

    let string_table = LuaTable::new_shared_template();
    for name in &[
        "sub", "len", "rep", "lower", "upper", "byte", "char", "reverse", "format", "find",
        "match", "gsub", "gmatch", "dump",
    ] {
        string_table.set(
            LuaValue::Str(name.as_bytes().to_vec()),
            LuaValue::RustFunction(format!("string.{name}")),
        );
    }
    globals.insert("string".to_string(), LuaValue::Table(string_table));

    let table_lib = LuaTable::new_shared_template();
    for name in &[
        "insert", "remove", "concat", "sort", "getn", "maxn", "foreach", "foreachi",
    ] {
        table_lib.set(
            LuaValue::Str(name.as_bytes().to_vec()),
            LuaValue::RustFunction(format!("table.{name}")),
        );
    }
    globals.insert("table".to_string(), LuaValue::Table(table_lib));

    let cjson_table = LuaTable::new_shared_template();
    for name in &["encode", "decode"] {
        cjson_table.set(
            LuaValue::Str(name.as_bytes().to_vec()),
            LuaValue::RustFunction(format!("cjson.{name}")),
        );
    }
    cjson_table.set(
        LuaValue::Str(b"null".to_vec()),
        LuaValue::Userdata(LuaUserdata::CjsonNull),
    );
    globals.insert("cjson".to_string(), LuaValue::Table(cjson_table));

    let cmsgpack_table = LuaTable::new_shared_template();
    for name in &["pack", "unpack", "unpack_one", "unpack_limit"] {
        cmsgpack_table.set(
            LuaValue::Str(name.as_bytes().to_vec()),
            LuaValue::RustFunction(format!("cmsgpack.{name}")),
        );
    }
    for (name, value) in &[
        ("_NAME", "cmsgpack"),
        ("_VERSION", "lua-cmsgpack 0.4.0"),
        ("_COPYRIGHT", "Copyright (C) 2012, Salvatore Sanfilippo"),
        ("_DESCRIPTION", "MessagePack C implementation for Lua"),
    ] {
        cmsgpack_table.set(
            LuaValue::Str(name.as_bytes().to_vec()),
            LuaValue::Str(value.as_bytes().to_vec()),
        );
    }
    globals.insert("cmsgpack".to_string(), LuaValue::Table(cmsgpack_table));

    let struct_table = LuaTable::new_shared_template();
    for name in &["pack", "unpack", "size"] {
        struct_table.set(
            LuaValue::Str(name.as_bytes().to_vec()),
            LuaValue::RustFunction(format!("struct.{name}")),
        );
    }
    globals.insert("struct".to_string(), LuaValue::Table(struct_table));

    globals.insert("_VERSION".to_string(), LuaValue::Str(b"Lua 5.1".to_vec()));
    globals.insert(
        "rawequal".to_string(),
        LuaValue::RustFunction("rawequal".to_string()),
    );
    globals.insert(
        "gcinfo".to_string(),
        LuaValue::RustFunction("gcinfo".to_string()),
    );
    globals.insert(
        "collectgarbage".to_string(),
        LuaValue::RustFunction("collectgarbage".to_string()),
    );

    let bit_table = LuaTable::new_shared_template();
    for name in &[
        "band", "bor", "bxor", "bnot", "lshift", "rshift", "arshift", "rol", "ror", "bswap",
        "tobit", "tohex",
    ] {
        bit_table.set(
            LuaValue::Str(name.as_bytes().to_vec()),
            LuaValue::RustFunction(format!("bit.{name}")),
        );
    }
    globals.insert("bit".to_string(), LuaValue::Table(bit_table));
    globals.insert(
        "redis".to_string(),
        LuaValue::Table(lua_redis_table_template()),
    );

    for name in [
        "math", "string", "table", "struct", "bit", "cjson", "cmsgpack", "redis",
    ] {
        if let Some(LuaValue::Table(table)) = globals.get(name) {
            table.mark_readonly_recursive();
        }
    }
    globals
}

fn lua_redis_table_template() -> LuaTable {
    let redis_table = LuaTable::new_shared_template();
    redis_table.set(
        LuaValue::Str(b"call".to_vec()),
        LuaValue::RustFunction("redis.call".to_string()),
    );
    redis_table.set(
        LuaValue::Str(b"pcall".to_vec()),
        LuaValue::RustFunction("redis.pcall".to_string()),
    );
    redis_table.set(
        LuaValue::Str(b"error_reply".to_vec()),
        LuaValue::RustFunction("redis.error_reply".to_string()),
    );
    redis_table.set(
        LuaValue::Str(b"status_reply".to_vec()),
        LuaValue::RustFunction("redis.status_reply".to_string()),
    );
    redis_table.set(
        LuaValue::Str(b"log".to_vec()),
        LuaValue::RustFunction("redis.log".to_string()),
    );
    redis_table.set(
        LuaValue::Str(b"sha1hex".to_vec()),
        LuaValue::RustFunction("redis.sha1hex".to_string()),
    );
    redis_table.set(
        LuaValue::Str(b"replicate_commands".to_vec()),
        LuaValue::RustFunction("redis.replicate_commands".to_string()),
    );
    redis_table.set(
        LuaValue::Str(b"set_repl".to_vec()),
        LuaValue::RustFunction("redis.set_repl".to_string()),
    );
    redis_table.set(
        LuaValue::Str(b"breakpoint".to_vec()),
        LuaValue::RustFunction("redis.breakpoint".to_string()),
    );
    redis_table.set(
        LuaValue::Str(b"debug".to_vec()),
        LuaValue::RustFunction("redis.debug".to_string()),
    );
    redis_table.set(
        LuaValue::Str(b"setresp".to_vec()),
        LuaValue::RustFunction("redis.setresp".to_string()),
    );
    redis_table.set(
        LuaValue::Str(b"acl_check_cmd".to_vec()),
        LuaValue::RustFunction("redis.acl_check_cmd".to_string()),
    );
    redis_table.set(LuaValue::Str(b"LOG_DEBUG".to_vec()), LuaValue::Number(0.0));
    redis_table.set(
        LuaValue::Str(b"LOG_VERBOSE".to_vec()),
        LuaValue::Number(1.0),
    );
    redis_table.set(LuaValue::Str(b"LOG_NOTICE".to_vec()), LuaValue::Number(2.0));
    redis_table.set(
        LuaValue::Str(b"LOG_WARNING".to_vec()),
        LuaValue::Number(3.0),
    );
    redis_table.set(LuaValue::Str(b"REPL_NONE".to_vec()), LuaValue::Number(0.0));
    redis_table.set(LuaValue::Str(b"REPL_AOF".to_vec()), LuaValue::Number(1.0));
    redis_table.set(LuaValue::Str(b"REPL_SLAVE".to_vec()), LuaValue::Number(2.0));
    redis_table.set(
        LuaValue::Str(b"REPL_REPLICA".to_vec()),
        LuaValue::Number(2.0),
    );
    redis_table.set(LuaValue::Str(b"REPL_ALL".to_vec()), LuaValue::Number(3.0));
    redis_table.set(
        LuaValue::Str(b"REDIS_VERSION".to_vec()),
        LuaValue::Str(fr_store::REDIS_COMPAT_VERSION.as_bytes().to_vec()),
    );
    redis_table.set(
        LuaValue::Str(b"REDIS_VERSION_NUM".to_vec()),
        LuaValue::Number(f64::from(redis_version_num(fr_store::REDIS_COMPAT_VERSION))),
    );
    redis_table
}

impl<'a> LuaState<'a> {
    pub fn new(store: &'a mut Store, now_ms: u64) -> Self {
        let globals = lua_base_globals_template();
        let rng_seed = store.rng_seed;
        // (frankenredis-lwj8o) Initialize math.random's PRNG with the
        // low 32 bits of the store's rng_seed (or 1 if zero). User
        // scripts can re-seed via math.randomseed(n). We mirror vendored
        // Redis's redisLrand48 (NOT glibc rand) — see rand.c.
        let mut lua_random = RedisLrand48::new();
        let initial_seed = rng_seed as u32;
        if initial_seed != 0 {
            lua_random.srand(initial_seed as i32);
        }
        Self {
            store,
            now_ms,
            globals,
            globals_locked: false,
            resp_version: 2,
            call_depth: 0,
            lua_frame_kinds: Vec::new(),
            iterations: 0,
            current_line: 1,
            rng_seed,
            lua_random,
            script_started_at: Instant::now(),
            current_coroutine: None,
            pending_yield: None,
            pending_error_value: None,
            pending_error_is_raw_body: false,
            current_source_label: None,
            current_invocation_name: None,
            current_invocation_is_method: false,
            nested_exec_stmts_depth: 0,
            inside_bare_expression_stmt: false,
        }
    }

    #[allow(dead_code)]
    fn next_rand(&mut self) -> u64 {
        self.rng_seed = self
            .rng_seed
            .wrapping_mul(0x5851_f42d_4c95_7f2d)
            .wrapping_add(1);
        self.rng_seed
    }

    pub fn set_keys_argv(&mut self, keys: Vec<LuaValue>, argv: Vec<LuaValue>) {
        let keys_table = LuaTable::new();
        keys_table.inner.borrow_mut().array = keys;
        let argv_table = LuaTable::new();
        argv_table.inner.borrow_mut().array = argv;
        self.globals
            .insert("KEYS".to_string(), LuaValue::Table(keys_table));
        self.globals
            .insert("ARGV".to_string(), LuaValue::Table(argv_table));

        // (frankenredis-1khox) Redis 7.2.4's Lua sandbox does NOT
        // expose the os table -- scripts that reference 'os' get the
        // standard 'Script attempted to access nonexistent global
        // variable' error. The os.clock dispatch handler is left in
        // place but the table is not bound.

        // Coroutine table registration. Redis 7.2 keeps the Lua
        // coroutine library available inside the script sandbox.
        let coroutine_table = LuaTable::new();
        for name in &["create", "resume", "yield", "status", "wrap", "running"] {
            coroutine_table.set(
                LuaValue::Str(name.as_bytes().to_vec()),
                LuaValue::RustFunction(format!("coroutine.{name}")),
            );
        }
        self.globals
            .insert("coroutine".to_string(), LuaValue::Table(coroutine_table));
    }

    pub fn execute(&mut self, source: &[u8]) -> Result<LuaValue, String> {
        let stmts = compile_lua_chunk_cached(source)?;
        self.execute_compiled(stmts.as_ref())
    }

    fn execute_compiled(&mut self, stmts: &Block) -> Result<LuaValue, String> {
        // (frankenredis-j02x9) Lock the globals table — from this point
        // forward any user-script write to globals raises a readonly-
        // table error and any read of an undefined global raises the
        // upstream sandbox error. Mirrors script_lua.c::
        // luaSetTableProtectionRecursively run after script env init.
        // (frankenredis-u24vv) The script's environment is exposed as `_G`.
        // Build that snapshot lazily when script code observes `_G` or asks
        // for the default environment; trivial scripts should not pay for a
        // full globals table clone. The snapshot stays behavior-equivalent
        // because globals are locked before user code executes.
        self.globals_locked = true;
        // (frankenredis-8mwy9) Protect the standard library tables the same way
        // redis's `luaSetTableProtectionRecursively` does: after env init the
        // math/string/table/struct/bit/cjson/cmsgpack/redis tables are read-only
        // for the duration of the script. KEYS/ARGV stay mutable (redis leaves
        // them writable), so they are deliberately excluded.
        for name in [
            "math", "string", "table", "struct", "bit", "cjson", "cmsgpack", "redis",
        ] {
            if let Some(LuaValue::Table(t)) = self.globals.get(name) {
                t.mark_readonly_recursive();
            }
        }
        // (frankenredis-vr8rg) Every script starts in RESP2 for redis.call,
        // independent of the client's HELLO version.
        self.resp_version = 2;
        let mut env = Env::new();
        let mut varargs = Vec::new();
        // (frankenredis-0k259) The script top-level chunk is a Lua function
        // frame for the purposes of luaL_where; push before exec_block so
        // error(msg, N) at the bottom of the call stack can find it.
        self.lua_frame_kinds.push(true);
        let outcome = self.exec_block(stmts, &mut env, &mut varargs);
        self.lua_frame_kinds.pop();
        match outcome {
            Ok(ControlFlow::Return(vals)) => Ok(vals.into_iter().next().unwrap_or(LuaValue::Nil)),
            Ok(_) => Ok(LuaValue::Nil),
            // (frankenredis-cxmsu) An uncaught `error({...})` /
            // `error(true)` / `error(nil)` escapes through the
            // sentinel string. Convert to a sensible reply string at
            // the boundary — Redis cannot return a non-string error
            // to the wire (vendored Redis 7.2 actually crashes on the
            // table case; fr emits a tostring()-style representation).
            Err(msg) if msg == LUA_TYPED_ERROR_SENTINEL => {
                let val = self.pending_error_value.take().unwrap_or(LuaValue::Nil);
                let was_raw_body = self.pending_error_is_raw_body;
                self.pending_error_is_raw_body = false;
                let rendered = String::from_utf8_lossy(&val.to_display_string()).to_string();
                // (frankenredis-vkqn0) error({err=STRING}) tagged this
                // path. Prepend LUA_RAW_ERROR_BODY_MARKER so the
                // eval_script_error_reply wrapper recognises the
                // pre-formed RESP body and skips its "ERR " auto-prefix.
                Err(if was_raw_body {
                    format!("{LUA_RAW_ERROR_BODY_MARKER}{rendered}")
                } else {
                    rendered
                })
            }
            // (frankenredis-m7oy8) Stamp the real source line into the
            // hardcoded `user_script:1:` prefix the interpreter's error sites
            // emit. For single-line scripts current_line==1 (no-op); multi-line
            // scripts whose error lands on line >1 now report the right line,
            // matching vendored Redis 7.2.4's luaL_where output.
            Err(msg) => Err(stamp_user_script_line(msg, self.current_line)),
        }
    }

    fn exec_block(
        &mut self,
        stmts: &[(u32, Stmt)],
        env: &mut Env,
        varargs: &mut Vec<LuaValue>,
    ) -> Result<ControlFlow, String> {
        env.push_scope();
        let result = self.exec_stmts(stmts, env, varargs);
        env.pop_scope();
        result
    }

    fn exec_stmts(
        &mut self,
        stmts: &[(u32, Stmt)],
        env: &mut Env,
        varargs: &mut Vec<LuaValue>,
    ) -> Result<ControlFlow, String> {
        // Bump nested-exec depth so coroutine.yield can detect it's
        // about to fire from inside a control-flow block / function-
        // call body. bw15's resume_coroutine only tracks outer-stmt
        // PC, so yielding from a nested scope would silently drop
        // iterations on resume. The decrement runs even on error
        // via a manual fallthrough so the counter stays balanced.
        // (frankenredis-ztawj)
        self.nested_exec_stmts_depth = self.nested_exec_stmts_depth.saturating_add(1);
        let mut outcome: Result<ControlFlow, String> = Ok(ControlFlow::None);
        for (line, stmt) in stmts {
            self.current_line = *line;
            self.iterations += 1;
            if self.iterations > MAX_ITERATIONS {
                outcome = Err("script exceeded maximum iteration count".to_string());
                break;
            }
            match self.exec_stmt(stmt, env, varargs) {
                Ok(ControlFlow::None) => {}
                Ok(other) => {
                    outcome = Ok(other);
                    break;
                }
                Err(err) => {
                    outcome = Err(err);
                    break;
                }
            }
        }
        self.nested_exec_stmts_depth = self.nested_exec_stmts_depth.saturating_sub(1);
        outcome
    }

    fn direct_coroutine_yield_args(expr: &Expr) -> Option<&[Expr]> {
        let Expr::Call(func_expr, args) = expr else {
            return None;
        };
        let Expr::Field(table_expr, method) = func_expr.as_ref() else {
            return None;
        };
        if method == "yield"
            && matches!(table_expr.as_ref(), Expr::Name(name) if name == "coroutine")
        {
            Some(args)
        } else {
            None
        }
    }

    #[allow(clippy::type_complexity)]
    fn split_direct_yield_exprs(
        &mut self,
        exprs: &[Expr],
        env: &mut Env,
        varargs: &mut Vec<LuaValue>,
    ) -> Result<Option<(Vec<LuaValue>, Vec<Expr>, Vec<Expr>, bool)>, String> {
        for (idx, expr) in exprs.iter().enumerate() {
            let Some(yield_args) = Self::direct_coroutine_yield_args(expr) else {
                continue;
            };
            let mut prefix = Vec::with_capacity(idx);
            for prefix_expr in &exprs[..idx] {
                prefix.push(self.eval_expr(prefix_expr, env, varargs)?);
            }
            return Ok(Some((
                prefix,
                yield_args.to_vec(),
                exprs[idx + 1..].to_vec(),
                idx + 1 == exprs.len(),
            )));
        }
        Ok(None)
    }

    fn complete_exprs_after_yield(
        &mut self,
        mut prefix: Vec<LuaValue>,
        remaining: &[Expr],
        yield_was_last: bool,
        resume_args: &[LuaValue],
        env: &mut Env,
        varargs: &mut Vec<LuaValue>,
    ) -> Result<Vec<LuaValue>, String> {
        if yield_was_last {
            prefix.extend_from_slice(resume_args);
        } else {
            prefix.push(resume_args.first().cloned().unwrap_or(LuaValue::Nil));
            prefix.extend(self.eval_call_args(remaining, env, varargs)?);
        }
        Ok(prefix)
    }

    /// Run a generic-for loop given its already-resolved iterator values
    /// (fn, state, control). Shared by the direct `Stmt::GenericFor` path and
    /// the coroutine resume path where the iterator expression list yielded.
    fn run_generic_for_from_iter_vals(
        &mut self,
        names: &[String],
        iter_vals: Vec<LuaValue>,
        body: &[(u32, Stmt)],
        env: &mut Env,
        varargs: &mut Vec<LuaValue>,
    ) -> Result<ControlFlow, String> {
        let iter_fn = iter_vals.first().cloned().unwrap_or(LuaValue::Nil);
        let mut state = iter_vals.get(1).cloned().unwrap_or(LuaValue::Nil);
        let mut control = iter_vals.get(2).cloned().unwrap_or(LuaValue::Nil);
        // (CrimsonHawk) Per-var loop-cell reuse (same Rc::strong_count trick as
        // numeric-for): reuse each name.s cell unless a closure captured it.
        let mut loop_cells: Vec<Option<LuaCell>> = vec![None; names.len()];

        loop {
            self.iterations += 1;
            if self.iterations > MAX_ITERATIONS {
                return Err("script exceeded maximum iteration count".to_string());
            }
            let mut iter_args = vec![state.clone(), control.clone()];
            let results = self.call_function(&iter_fn, &mut iter_args, env, varargs)?;
            // Update state from mutated args (needed for stateful iterators like gmatch)
            state = iter_args[0].clone();
            let first = results.first().cloned().unwrap_or(LuaValue::Nil);
            if matches!(first, LuaValue::Nil) {
                break;
            }
            control = first.clone();
            env.push_scope();
            for (i, name) in names.iter().enumerate() {
                let val = results.get(i).cloned().unwrap_or(LuaValue::Nil);
                match loop_cells[i].take() {
                    Some(cell) if Rc::strong_count(&cell) == 1 => {
                        *cell.borrow_mut() = val;
                        env.set_local_cell_top(name, cell.clone());
                        loop_cells[i] = Some(cell);
                    }
                    _ => {
                        env.set_local(name, val);
                        loop_cells[i] = env.top_local_cell(name);
                    }
                }
            }
            let cf = self.exec_stmts(body, env, varargs)?;
            env.pop_scope();
            match cf {
                ControlFlow::Break => break,
                ControlFlow::Return(v) => return Ok(ControlFlow::Return(v)),
                ControlFlow::None => {}
            }
        }
        Ok(ControlFlow::None)
    }

    fn start_coroutine_yield(
        &mut self,
        yield_args: &[Expr],
        continuation: LuaCoroutineContinuation,
        allow_nested: bool,
        env: &mut Env,
        varargs: &mut Vec<LuaValue>,
    ) -> Result<ControlFlow, String> {
        if self.current_coroutine.is_none() || (self.nested_exec_stmts_depth > 0 && !allow_nested) {
            return Err("attempt to yield across metamethod/C-call boundary".to_string());
        }
        let values = self.eval_call_args(yield_args, env, varargs)?;
        if let Some(coroutine) = &self.current_coroutine {
            coroutine.inner.borrow_mut().continuation = Some(continuation);
        }
        self.pending_yield = Some(values);
        Err(LUA_YIELD_SENTINEL.to_string())
    }

    #[allow(clippy::too_many_arguments)]
    fn exec_numeric_for_body_from(
        &mut self,
        name: &str,
        stop: f64,
        step: f64,
        body: &[(u32, Stmt)],
        current: f64,
        start_pc: usize,
        env: &mut Env,
        varargs: &mut Vec<LuaValue>,
    ) -> Result<ControlFlow, String> {
        self.nested_exec_stmts_depth = self.nested_exec_stmts_depth.saturating_add(1);
        let mut outcome = Ok(ControlFlow::None);
        for (offset, (line, stmt)) in body.iter().enumerate().skip(start_pc) {
            self.current_line = *line;
            self.iterations += 1;
            if self.iterations > MAX_ITERATIONS {
                outcome = Err("script exceeded maximum iteration count".to_string());
                break;
            }
            if let Stmt::Expression(expr) = stmt
                && let Some(yield_args) = Self::direct_coroutine_yield_args(expr)
            {
                outcome = self.start_coroutine_yield(
                    yield_args,
                    LuaCoroutineContinuation::NumericFor {
                        name: name.to_string(),
                        stop,
                        step,
                        body: body.to_vec(),
                        current,
                        body_pc: offset + 1,
                    },
                    true,
                    env,
                    varargs,
                );
                break;
            }
            match self.exec_stmt(stmt, env, varargs) {
                Ok(ControlFlow::None) => {}
                Ok(other) => {
                    outcome = Ok(other);
                    break;
                }
                Err(err) => {
                    outcome = Err(err);
                    break;
                }
            }
        }
        self.nested_exec_stmts_depth = self.nested_exec_stmts_depth.saturating_sub(1);
        outcome
    }

    fn exec_stmt(
        &mut self,
        stmt: &Stmt,
        env: &mut Env,
        varargs: &mut Vec<LuaValue>,
    ) -> Result<ControlFlow, String> {
        match stmt {
            Stmt::Return(exprs) => {
                if let Some((prefix, yield_args, remaining, yield_was_last)) =
                    self.split_direct_yield_exprs(exprs, env, varargs)?
                {
                    return self.start_coroutine_yield(
                        &yield_args,
                        LuaCoroutineContinuation::Return {
                            prefix,
                            remaining,
                            yield_was_last,
                        },
                        false,
                        env,
                        varargs,
                    );
                }
                let vals = self.eval_expr_list(exprs, env, varargs)?;
                Ok(ControlFlow::Return(vals))
            }
            Stmt::Break => Ok(ControlFlow::Break),
            Stmt::Expression(expr) => {
                // Mark the bare-expression-stmt scope so yield
                // (called from inside this expr) can verify it's
                // resumable. Save/restore in case Stmt::Expression
                // is reached inside a nested function call where
                // the flag was already true. (frankenredis-gdbca)
                let prev = std::mem::replace(&mut self.inside_bare_expression_stmt, true);
                let result = self
                    .eval_expr(expr, env, varargs)
                    .map(|_| ControlFlow::None);
                self.inside_bare_expression_stmt = prev;
                result
            }
            Stmt::LocalAssign(names, exprs) => {
                // (CrimsonHawk) Fast path for the ubiquitous `local x = expr`
                // (one name, one value, non-yield RHS): skip the whole-list
                // yield scan and the eval_expr_list Vec allocation + per-slot
                // clone. eval_expr adjusts a multi-return call to its first value
                // (see Expr::Call arm), matching the general path's vals.get(0),
                // so this is byte-identical.
                if names.len() == 1
                    && exprs.len() == 1
                    && Self::direct_coroutine_yield_args(&exprs[0]).is_none()
                {
                    let val = self.eval_expr(&exprs[0], env, varargs)?;
                    env.set_local(&names[0], val);
                    return Ok(ControlFlow::None);
                }
                if let Some((prefix, yield_args, remaining, yield_was_last)) =
                    self.split_direct_yield_exprs(exprs, env, varargs)?
                {
                    return self.start_coroutine_yield(
                        &yield_args,
                        LuaCoroutineContinuation::LocalAssign {
                            names: names.clone(),
                            prefix,
                            remaining,
                            yield_was_last,
                        },
                        false,
                        env,
                        varargs,
                    );
                }
                let vals = self.eval_expr_list(exprs, env, varargs)?;
                for (i, name) in names.iter().enumerate() {
                    let val = vals.get(i).cloned().unwrap_or(LuaValue::Nil);
                    env.set_local(name, val);
                }
                Ok(ControlFlow::None)
            }
            Stmt::Assign(lhs_list, rhs_list) => {
                // (CrimsonHawk) Fast path for the ubiquitous `x = expr` (one
                // target, one value, non-yield RHS): skip the whole-list yield
                // scan and the eval_expr_list Vec allocation + per-slot clone.
                // eval_expr adjusts a multi-return call to its first value (see
                // Expr::Call arm), matching the general path's vals.get(0), so
                // this is byte-identical.
                if lhs_list.len() == 1
                    && rhs_list.len() == 1
                    && Self::direct_coroutine_yield_args(&rhs_list[0]).is_none()
                {
                    let val = self.eval_expr(&rhs_list[0], env, varargs)?;
                    self.assign_to(&lhs_list[0], val, env, varargs)?;
                    return Ok(ControlFlow::None);
                }
                if let Some((prefix, yield_args, remaining, yield_was_last)) =
                    self.split_direct_yield_exprs(rhs_list, env, varargs)?
                {
                    return self.start_coroutine_yield(
                        &yield_args,
                        LuaCoroutineContinuation::Assign {
                            lhs: lhs_list.clone(),
                            prefix,
                            remaining,
                            yield_was_last,
                        },
                        false,
                        env,
                        varargs,
                    );
                }
                let vals = self.eval_expr_list(rhs_list, env, varargs)?;
                for (i, lhs) in lhs_list.iter().enumerate() {
                    let val = vals.get(i).cloned().unwrap_or(LuaValue::Nil);
                    self.assign_to(lhs, val, env, varargs)?;
                }
                Ok(ControlFlow::None)
            }
            Stmt::If(branches, else_body) => {
                for (idx, (cond, body)) in branches.iter().enumerate() {
                    // (CrimsonHawk 7lmle) A branch condition that is a bare
                    // `coroutine.yield(...)` suspends here; on resume the yielded
                    // value is the condition result. Only the FIRST such condition
                    // is handled per suspension (resume must not re-yield); a later
                    // branch's bare-yield condition still errors as before.
                    if let Some(yield_args) = Self::direct_coroutine_yield_args(cond) {
                        return self.start_coroutine_yield(
                            yield_args,
                            LuaCoroutineContinuation::If {
                                then_body: body.clone(),
                                remaining: branches[idx + 1..].to_vec(),
                                else_body: else_body.clone(),
                            },
                            false,
                            env,
                            varargs,
                        );
                    }
                    let cv = self.eval_expr(cond, env, varargs)?;
                    if cv.is_truthy() {
                        return self.exec_block(body, env, varargs);
                    }
                }
                if let Some(body) = else_body {
                    return self.exec_block(body, env, varargs);
                }
                Ok(ControlFlow::None)
            }
            Stmt::While(cond, body) => {
                loop {
                    self.iterations += 1;
                    if self.iterations > MAX_ITERATIONS {
                        return Err("script exceeded maximum iteration count".to_string());
                    }
                    // (CrimsonHawk 7lmle) A bare `coroutine.yield(...)` loop
                    // condition suspends here; on resume the yielded value is the
                    // condition result (truthy → run body then re-check; falsy →
                    // exit). Only valid at the coroutine's top statement level;
                    // deeper it errors as before.
                    if let Some(yield_args) = Self::direct_coroutine_yield_args(cond) {
                        return self.start_coroutine_yield(
                            yield_args,
                            LuaCoroutineContinuation::While {
                                cond: cond.clone(),
                                body: body.clone(),
                            },
                            false,
                            env,
                            varargs,
                        );
                    }
                    let cv = self.eval_expr(cond, env, varargs)?;
                    if !cv.is_truthy() {
                        break;
                    }
                    match self.exec_block(body, env, varargs)? {
                        ControlFlow::Break => break,
                        ControlFlow::Return(v) => return Ok(ControlFlow::Return(v)),
                        ControlFlow::None => {}
                    }
                }
                Ok(ControlFlow::None)
            }
            Stmt::Repeat(body, cond) => {
                loop {
                    self.iterations += 1;
                    if self.iterations > MAX_ITERATIONS {
                        return Err("script exceeded maximum iteration count".to_string());
                    }
                    env.push_scope();
                    let cf = self.exec_stmts(body, env, varargs)?;
                    // A `break`/`return` in the body exits before the until
                    // condition is ever evaluated (matches Lua 5.1).
                    match cf {
                        ControlFlow::Break => {
                            env.pop_scope();
                            break;
                        }
                        ControlFlow::Return(v) => {
                            env.pop_scope();
                            return Ok(ControlFlow::Return(v));
                        }
                        ControlFlow::None => {}
                    }
                    // (CrimsonHawk 7lmle) A bare `coroutine.yield(...)` until
                    // condition suspends here. The body scope stays pushed so the
                    // yield args see body locals; it is saved with the env on
                    // yield and popped in the resume handler. Top-level only;
                    // deeper it errors as before.
                    if let Some(yield_args) = Self::direct_coroutine_yield_args(cond) {
                        return self.start_coroutine_yield(
                            yield_args,
                            LuaCoroutineContinuation::Repeat {
                                body: body.clone(),
                                cond: cond.clone(),
                            },
                            false,
                            env,
                            varargs,
                        );
                    }
                    let cv = self.eval_expr(cond, env, varargs)?;
                    env.pop_scope();
                    if cv.is_truthy() {
                        break;
                    }
                }
                Ok(ControlFlow::None)
            }
            Stmt::NumericFor(name, start, stop, step, body) => {
                // (frankenredis-7vqyo) Upstream luaV_execute raises these
                // via luaG_runerror which prepends the script source
                // location. The initial-value variant reads "initial
                // value" (not "start") in Lua 5.1's lvm.c.
                let s = self
                    .eval_expr(start, env, varargs)?
                    .to_number()
                    .ok_or("user_script:1: 'for' initial value must be a number")?;
                let e = self
                    .eval_expr(stop, env, varargs)?
                    .to_number()
                    .ok_or("user_script:1: 'for' limit must be a number")?;
                let st = match step {
                    Some(expr) => self
                        .eval_expr(expr, env, varargs)?
                        .to_number()
                        .ok_or("user_script:1: 'for' step must be a number")?,
                    None => 1.0,
                };
                // (frankenredis-4hhz5) Lua 5.1 allows step=0; the body
                // either breaks/returns or the loop is infinite (caller's
                // responsibility, same as `while true do end`). Vendored
                // does not reject step=0 at the runtime layer.
                let mut i = s;
                // (CrimsonHawk) Reuse one loop-var cell across iterations unless a
                // closure captured it (Rc strong_count > 1; GC registry holds only a
                // Weak). Byte-identical to fresh-every-iteration.
                let mut loop_cell: Option<LuaCell> = None;
                loop {
                    self.iterations += 1;
                    if self.iterations > MAX_ITERATIONS {
                        return Err("script exceeded maximum iteration count".to_string());
                    }
                    if (st > 0.0 && i > e) || (st < 0.0 && i < e) {
                        break;
                    }
                    env.push_scope();
                    match loop_cell.take() {
                        Some(cell) if Rc::strong_count(&cell) == 1 => {
                            *cell.borrow_mut() = LuaValue::Number(i);
                            env.set_local_cell_top(name, cell.clone());
                            loop_cell = Some(cell);
                        }
                        _ => {
                            env.set_local(name, LuaValue::Number(i));
                            loop_cell = env.top_local_cell(name);
                        }
                    }
                    let cf =
                        self.exec_numeric_for_body_from(name, e, st, body, i, 0, env, varargs)?;
                    env.pop_scope();
                    match cf {
                        ControlFlow::Break => break,
                        ControlFlow::Return(v) => return Ok(ControlFlow::Return(v)),
                        ControlFlow::None => {}
                    }
                    i += st;
                }
                Ok(ControlFlow::None)
            }
            Stmt::GenericFor(names, iter_exprs, body) => {
                // (CrimsonHawk 7lmle) A bare `coroutine.yield(...)` anywhere in
                // the iterator expression list suspends here; on resume the
                // yielded value(s) complete the iterator triple and the loop
                // runs. Top-level only; deeper it errors as before.
                if let Some((prefix, yield_args, remaining, yield_was_last)) =
                    self.split_direct_yield_exprs(iter_exprs, env, varargs)?
                {
                    return self.start_coroutine_yield(
                        &yield_args,
                        LuaCoroutineContinuation::GenericFor {
                            names: names.clone(),
                            prefix,
                            remaining,
                            yield_was_last,
                            body: body.clone(),
                        },
                        false,
                        env,
                        varargs,
                    );
                }
                let iter_vals = self.eval_expr_list(iter_exprs, env, varargs)?;
                self.run_generic_for_from_iter_vals(names, iter_vals, body, env, varargs)
            }
            Stmt::DoBlock(body) => self.exec_block(body, env, varargs),
            Stmt::FunctionDecl(names, params, is_variadic, body) => {
                let func = LuaValue::function(LuaFunc {
                    params: params.clone(),
                    body: body.clone(),
                    is_variadic: *is_variadic,
                    captured_env: Some(env.snapshot()),
                    env_table: Rc::new(RefCell::new(env.current_global_env())),
                    self_name: None,
                    source_label: self.current_source_label.clone(),
                    identity: next_function_identity(),
                });
                if names.len() == 1 {
                    // (frankenredis-j02x9) `function f() end` is
                    // equivalent to `f = function() end`; both write
                    // to the globals table. Block once locked.
                    self.assign_to(&Expr::Name(names[0].clone()), func, env, varargs)?;
                } else {
                    // Nested field assignment: a.b.c = func
                    self.set_nested_field(names, func)?;
                }
                Ok(ControlFlow::None)
            }
            Stmt::LocalFunctionDecl(name, params, is_variadic, body) => {
                env.set_local(name, LuaValue::Nil);
                let func = LuaValue::function(LuaFunc {
                    params: params.clone(),
                    body: body.clone(),
                    is_variadic: *is_variadic,
                    captured_env: Some(env.snapshot()),
                    env_table: Rc::new(RefCell::new(env.current_global_env())),
                    self_name: None,
                    source_label: self.current_source_label.clone(),
                    identity: next_function_identity(),
                });
                if !env.set_existing_local(name, func.clone()) {
                    env.set_local(name, func);
                }
                Ok(ControlFlow::None)
            }
        }
    }

    fn set_nested_field(&mut self, names: &[String], value: LuaValue) -> Result<(), String> {
        if names.len() < 2 {
            return Ok(());
        }
        let Some((root_name, tail)) = names.split_first() else {
            return Ok(());
        };
        let Some((last_field, parent_fields)) = tail.split_last() else {
            return Ok(());
        };
        // (frankenredis-dfly7) Multi-segment function decls
        // (`function a.b.c() … end`) start by reading the root global
        // through the sandbox layer. Upstream's _G __index handler
        // emits "Script attempted to access nonexistent global
        // variable 'a'" for missing keys when the sandbox is locked.
        // fr previously fell through to "attempt to index a nil
        // value" because the read bypassed Expr::Name's sandbox path.
        let mut current = match self.globals.get(root_name) {
            Some(val) => val.clone(),
            None => {
                if self.globals_locked {
                    return Err(format!(
                        "user_script:1: Script attempted to access nonexistent global variable '{root_name}'"
                    ));
                }
                LuaValue::Nil
            }
        };
        if !matches!(current, LuaValue::Table(_)) {
            return Err(format!(
                "user_script:1: attempt to index a {} value",
                current.type_name()
            ));
        }
        // Navigate to the parent table
        let mut path: Vec<LuaValue> = vec![current.clone()];
        for name in parent_fields {
            let next = match &current {
                LuaValue::Table(t) => t.get(&LuaValue::Str(name.as_bytes().to_vec())),
                other => {
                    return Err(format!(
                        "user_script:1: attempt to index a {} value",
                        other.type_name()
                    ));
                }
            };
            if !matches!(next, LuaValue::Table(_)) {
                return Err(format!(
                    "user_script:1: attempt to index a {} value",
                    next.type_name()
                ));
            }
            current = next;
            path.push(current.clone());
        }
        // Set the value in the innermost table
        let Some(last_entry) = path.last_mut() else {
            return Err("user_script:1: attempt to index a nil value".to_string());
        };
        if let LuaValue::Table(t) = last_entry {
            t.set(LuaValue::Str(last_field.as_bytes().to_vec()), value);
            // Rebuild the chain
            let mut val = path
                .pop()
                .ok_or_else(|| "user_script:1: attempt to index a nil value".to_string())?;
            for i in (0..parent_fields.len()).rev() {
                if let Some(LuaValue::Table(parent)) = path.get_mut(i) {
                    parent.set(LuaValue::Str(parent_fields[i].as_bytes().to_vec()), val);
                    val = path[i].clone();
                }
            }
            self.globals.insert(root_name.clone(), val);
        }
        Ok(())
    }

    fn assign_to(
        &mut self,
        lhs: &Expr,
        value: LuaValue,
        env: &mut Env,
        varargs: &mut Vec<LuaValue>,
    ) -> Result<(), String> {
        match lhs {
            Expr::LocalName(name, local) => {
                if !env.set_existing_local_slot(*local, value.clone())
                    && !env.set_existing_local(name, value.clone())
                {
                    if let Some(global_env) = env.current_global_env() {
                        self.table_assign_with_newindex(
                            global_env,
                            LuaValue::Str(name.as_bytes().to_vec()),
                            value,
                            env,
                            varargs,
                        )?;
                        return Ok(());
                    }
                    if self.globals_locked {
                        return Err("user_script:1: Attempt to modify a readonly table".to_string());
                    }
                    self.globals.insert(name.clone(), value);
                }
            }
            Expr::Name(name) => {
                if !env.set_existing_local(name, value.clone()) {
                    if let Some(global_env) = env.current_global_env() {
                        self.table_assign_with_newindex(
                            global_env,
                            LuaValue::Str(name.as_bytes().to_vec()),
                            value,
                            env,
                            varargs,
                        )?;
                        return Ok(());
                    }
                    // (frankenredis-j02x9) Once locked, the globals
                    // table is read-only — both new and overwriting
                    // assignments raise. Locals (Stmt::LocalAssign)
                    // bypass this path entirely.
                    if self.globals_locked {
                        return Err("user_script:1: Attempt to modify a readonly table".to_string());
                    }
                    self.globals.insert(name.clone(), value);
                }
            }
            Expr::Index(table_expr, key_expr) => {
                let table = self.eval_expr(table_expr, env, varargs)?;
                let key = self.eval_expr(key_expr, env, varargs)?;
                // (frankenredis-tb9vb) Reject nil/NaN keys before the
                // assignment dispatches into __newindex or hits the
                // table's internal storage — upstream luaV_settable
                // raises here, never silently dropping the value.
                lua_check_table_key(&key)?;
                self.table_set_by_expr(table_expr, table, key, value, env, varargs)?;
            }
            Expr::Field(table_expr, field) => {
                let table = self.eval_expr(table_expr, env, varargs)?;
                let key = LuaValue::Str(field.as_bytes().to_vec());
                self.table_set_by_expr(table_expr, table, key, value, env, varargs)?;
            }
            _ => return Err("invalid assignment target".to_string()),
        }
        Ok(())
    }

    fn table_set_by_expr(
        &mut self,
        table_expr: &Expr,
        mut table: LuaValue,
        key: LuaValue,
        value: LuaValue,
        env: &mut Env,
        varargs: &mut Vec<LuaValue>,
    ) -> Result<(), String> {
        if let LuaValue::Table(t) = &mut table {
            // (frankenredis-9f16h) Route table assignments through the
            // full __newindex metamethod chain: callable handlers fire,
            // table handlers cascade, existing keys bypass.
            self.table_assign_with_newindex(t.clone(), key, value, env, varargs)?;
            self.write_back_table_expr(table_expr, table, env, varargs)?;
            Ok(())
        } else {
            // (frankenredis-ct3ir) Route through type_error_with_label
            // so newindex on a non-table picks up the accessor context
            // (local 'x' / field 'f' / global 'g') just like reads do.
            Err(self.type_error_with_label("index", table_expr, &table, env))
        }
    }

    /// Write `value` to `table[key]` honoring Lua 5.1's full __newindex
    /// metamethod chain:
    ///   - If `key` already exists in the table → direct write (no
    ///     metamethod invocation).
    ///   - Else if metatable has __newindex == table → cascade the
    ///     write to that table (recursively).
    ///   - Else if __newindex is callable → invoke
    ///     `__newindex(table, key, value)`.
    ///   - Else if __newindex is non-nil non-callable → emit "attempt
    ///     to index a TYPE value" (vendored behavior; e.g.
    ///     __newindex='nope' tries to index the string).
    ///   - Else → direct write.
    ///
    /// Cap the cascade at 16 hops, matching the __index depth limit.
    /// (frankenredis-9f16h)
    fn table_assign_with_newindex(
        &mut self,
        table: LuaTable,
        key: LuaValue,
        value: LuaValue,
        env: &mut Env,
        varargs: &mut Vec<LuaValue>,
    ) -> Result<(), String> {
        let mut current = table;
        for _ in 0..16 {
            // (frankenredis-8mwy9) A protected library table rejects every
            // field write (new or overwriting) with redis's readonly error.
            if current.is_readonly() {
                return Err("user_script:1: Attempt to modify a readonly table".to_string());
            }
            let existing = current.inner.borrow().get(&key);
            if !matches!(existing, LuaValue::Nil) {
                current.set(key, value);
                return Ok(());
            }
            let handler = {
                let inner = current.inner.borrow();
                match &inner.metatable {
                    Some(mt) => mt.get(&LuaValue::Str(b"__newindex".to_vec())),
                    None => LuaValue::Nil,
                }
            };
            match handler {
                LuaValue::Nil => {
                    current.set(key, value);
                    return Ok(());
                }
                LuaValue::Table(next) => {
                    current = next;
                    continue;
                }
                callable @ (LuaValue::RustFunction(_)
                | LuaValue::Function(_)
                | LuaValue::WrappedCoroutine(_)) => {
                    let mut args = vec![LuaValue::Table(current.clone()), key, value];
                    self.call_function(&callable, &mut args, env, varargs)?;
                    return Ok(());
                }
                other => {
                    return Err(format!(
                        "user_script:1: attempt to index a {} value",
                        other.type_name()
                    ));
                }
            }
        }
        // (frankenredis-fhf2s) Upstream lvm.c::luaV_settable raises
        // 'loop in settable' when the MAXTAGLOOP cap is exhausted —
        // the write-side counterpart to 'loop in gettable'
        // (frankenredis-91w0c). fr previously emitted a bespoke
        // '__newindex cascade exceeded depth limit' string that
        // didn't match vendored.
        Err("user_script:1: loop in settable".to_string())
    }

    fn write_back_table_expr(
        &mut self,
        table_expr: &Expr,
        table: LuaValue,
        env: &mut Env,
        varargs: &mut Vec<LuaValue>,
    ) -> Result<(), String> {
        match table_expr {
            Expr::LocalName(name, local) => {
                if !env.set_existing_local_slot(*local, table.clone())
                    && !env.set_existing_local(name, table.clone())
                {
                    if let Some(global_env) = env.current_global_env() {
                        self.table_assign_with_newindex(
                            global_env,
                            LuaValue::Str(name.as_bytes().to_vec()),
                            table,
                            env,
                            varargs,
                        )?;
                        return Ok(());
                    }
                    self.globals.insert(name.clone(), table);
                }
                Ok(())
            }
            Expr::Name(name) => {
                if !env.set_existing_local(name, table.clone()) {
                    if let Some(global_env) = env.current_global_env() {
                        self.table_assign_with_newindex(
                            global_env,
                            LuaValue::Str(name.as_bytes().to_vec()),
                            table,
                            env,
                            varargs,
                        )?;
                        return Ok(());
                    }
                    self.globals.insert(name.clone(), table);
                }
                Ok(())
            }
            Expr::Index(parent_expr, key_expr) => {
                let parent = self.eval_expr(parent_expr, env, varargs)?;
                let key = self.eval_expr(key_expr, env, varargs)?;
                self.table_set_by_expr(parent_expr, parent, key, table, env, varargs)
            }
            Expr::Field(parent_expr, field) => {
                let parent = self.eval_expr(parent_expr, env, varargs)?;
                let key = LuaValue::Str(field.as_bytes().to_vec());
                self.table_set_by_expr(parent_expr, parent, key, table, env, varargs)
            }
            _ => Err("invalid assignment target".to_string()),
        }
    }

    fn eval_expr(
        &mut self,
        expr: &Expr,
        env: &mut Env,
        varargs: &mut Vec<LuaValue>,
    ) -> Result<LuaValue, String> {
        match expr {
            Expr::Nil => Ok(LuaValue::Nil),
            Expr::Bool(b) => Ok(LuaValue::Bool(*b)),
            Expr::Number(n) => Ok(LuaValue::Number(*n)),
            Expr::Str(s) => Ok(LuaValue::Str(s.clone())),
            Expr::VarArgs => {
                // Return first vararg; multi-value context handled in eval_expr_list
                Ok(varargs.first().cloned().unwrap_or(LuaValue::Nil))
            }
            Expr::LocalName(name, local) => {
                if let Some(val) = env.get_local_slot(*local) {
                    Ok(val)
                } else if let Some(val) = env.get_local(name) {
                    Ok(val)
                } else if let Some(global_env) = env.current_global_env() {
                    self.table_lookup_with_index_meta(
                        &global_env,
                        &LuaValue::Str(name.as_bytes().to_vec()),
                        env,
                        varargs,
                    )
                } else if name == "_G" {
                    Ok(LuaValue::Table(self.ensure_g_table()))
                } else if let Some(val) = self.globals.get(name) {
                    Ok(val.clone())
                } else if self.globals_locked {
                    Err(format!(
                        "user_script:1: Script attempted to access nonexistent global variable '{name}'"
                    ))
                } else {
                    Ok(LuaValue::Nil)
                }
            }
            Expr::Name(name) => {
                if let Some(val) = env.get_local(name) {
                    Ok(val.clone())
                } else if let Some(global_env) = env.current_global_env() {
                    self.table_lookup_with_index_meta(
                        &global_env,
                        &LuaValue::Str(name.as_bytes().to_vec()),
                        env,
                        varargs,
                    )
                } else if name == "_G" {
                    Ok(LuaValue::Table(self.ensure_g_table()))
                } else if let Some(val) = self.globals.get(name) {
                    Ok(val.clone())
                } else if self.globals_locked {
                    // (frankenredis-j02x9) Mirror upstream's
                    // luaProtectedTableError __index handler — reading
                    // an undefined global from a sandboxed user script
                    // is a hard error, not silent nil.
                    // (frankenredis-1khox) Prepend the standard
                    // 'user_script:N: ' source-location prefix that
                    // upstream's luaL_error / luaProtectedTableError
                    // wraps around every C-side script error.
                    Err(format!(
                        "user_script:1: Script attempted to access nonexistent global variable '{name}'"
                    ))
                } else {
                    Ok(LuaValue::Nil)
                }
            }
            Expr::BinOp(left, op, right) => {
                // Short-circuit for and/or
                match op {
                    BinOp::And => {
                        let lv = self.eval_expr(left, env, varargs)?;
                        if !lv.is_truthy() {
                            return Ok(lv);
                        }
                        self.eval_expr(right, env, varargs)
                    }
                    BinOp::Or => {
                        let lv = self.eval_expr(left, env, varargs)?;
                        if lv.is_truthy() {
                            return Ok(lv);
                        }
                        self.eval_expr(right, env, varargs)
                    }
                    _ => {
                        let lv = self.eval_expr(left, env, varargs)?;
                        let rv = self.eval_expr(right, env, varargs)?;
                        // (frankenredis-hqevr) When `..` is applied to a
                        // table (or any non-string-non-number) and either
                        // operand has a `__concat` metamethod, dispatch
                        // to it as Lua 5.1 does. Non-callable handlers
                        // produce the standard "attempt to call a TYPE
                        // value" error via call_function — that's the
                        // wording vendored emits for e.g. __concat=42.
                        if matches!(op, BinOp::Concat) {
                            let lhs_simple = matches!(lv, LuaValue::Str(_) | LuaValue::Number(_));
                            let rhs_simple = matches!(rv, LuaValue::Str(_) | LuaValue::Number(_));
                            if !(lhs_simple && rhs_simple) {
                                if let Some(handler) =
                                    self.lookup_binop_metamethod(&lv, &rv, "__concat")
                                {
                                    let mut args = vec![lv, rv];
                                    let results =
                                        self.call_function(&handler, &mut args, env, varargs)?;
                                    return Ok(results.into_iter().next().unwrap_or(LuaValue::Nil));
                                }
                                // (frankenredis-22c3u) No __concat handler:
                                // emit Lua 5.1's accessor-aware wording.
                                // The first non-string/non-number operand
                                // (LHS preferred) supplies the
                                // local/upvalue/field/method label.
                                let (bad_val, bad_expr) = if !lhs_simple {
                                    (&lv, &**left)
                                } else {
                                    (&rv, &**right)
                                };
                                let label = self.callee_label(bad_expr, env);
                                return Err(match label {
                                    Some(l) => format!(
                                        "user_script:1: attempt to concatenate {l} (a {} value)",
                                        bad_val.type_name()
                                    ),
                                    None => format!(
                                        "user_script:1: attempt to concatenate a {} value",
                                        bad_val.type_name()
                                    ),
                                });
                            }
                        }
                        // (frankenredis-mdxsk) Arithmetic operators dispatch
                        // to __add / __sub / __mul / __div / __mod / __pow
                        // when at least one operand fails to coerce to a
                        // number (matches Lua 5.1's `arith` C function:
                        // tonumber both; if either fails, try metamethod).
                        let is_arith = matches!(
                            op,
                            BinOp::Add
                                | BinOp::Sub
                                | BinOp::Mul
                                | BinOp::Div
                                | BinOp::Mod
                                | BinOp::Pow
                        );
                        if is_arith {
                            let name = match op {
                                BinOp::Add => "__add",
                                BinOp::Sub => "__sub",
                                BinOp::Mul => "__mul",
                                BinOp::Div => "__div",
                                BinOp::Mod => "__mod",
                                BinOp::Pow => "__pow",
                                _ => unreachable!(),
                            };
                            if lv.to_number().is_none() || rv.to_number().is_none() {
                                if let Some(handler) = self.lookup_binop_metamethod(&lv, &rv, name)
                                {
                                    let mut args = vec![lv, rv];
                                    let results =
                                        self.call_function(&handler, &mut args, env, varargs)?;
                                    return Ok(results.into_iter().next().unwrap_or(LuaValue::Nil));
                                }
                                // (frankenredis-9ckvq) No metamethod; emit
                                // the accessor-aware "attempt to perform
                                // arithmetic on local/field/upvalue/global"
                                // wording, labeling the first non-numeric
                                // operand (LHS preferred to match Lua's
                                // left-to-right tonumber probe order).
                                let (bad_val, bad_expr) = if lv.to_number().is_none() {
                                    (&lv, &**left)
                                } else {
                                    (&rv, &**right)
                                };
                                return Err(self.type_error_with_label(
                                    "perform arithmetic on",
                                    bad_expr,
                                    bad_val,
                                    env,
                                ));
                            }
                        }
                        // (frankenredis-ijlzv) Comparison metamethods.
                        // For ==/~= on tables: if raw-equal is false and
                        // both tables share a __eq metamethod by identity,
                        // dispatch. For </>/<=/>= on tables: try __lt or
                        // __le; > and >= swap args; <= falls back to
                        // `not __lt(rhs, lhs)` when __le is missing.
                        if matches!(op, BinOp::Eq | BinOp::Ne)
                            && let (LuaValue::Table(la), LuaValue::Table(rb)) = (&lv, &rv)
                            && !Rc::ptr_eq(&la.inner, &rb.inner)
                        {
                            // Lua 5.1 calls __eq only when both
                            // tables share a __eq metamethod. fr
                            // models that as: both tables point at
                            // the SAME metatable LuaTable (Rc), or
                            // both __eq slots are raw-equal (covers
                            // RustFunction-by-name plus the
                            // shared-metatable cases via
                            // lua_raw_equal). A LuaValue::Function
                            // lacks identity in fr, so two
                            // syntactically-identical-but-separate
                            // function literals are still treated
                            // as distinct (matching upstream).
                            let mt_a = la.inner.borrow().metatable.clone();
                            let mt_b = rb.inner.borrow().metatable.clone();
                            let shared_mt = match (&mt_a, &mt_b) {
                                (Some(ma), Some(mb)) => Rc::ptr_eq(&ma.inner, &mb.inner),
                                _ => false,
                            };
                            let eq_a = mt_a
                                .as_ref()
                                .map(|mt| mt.get(&LuaValue::Str(b"__eq".to_vec())))
                                .unwrap_or(LuaValue::Nil);
                            let eq_b = mt_b
                                .as_ref()
                                .map(|mt| mt.get(&LuaValue::Str(b"__eq".to_vec())))
                                .unwrap_or(LuaValue::Nil);
                            let same_eq = shared_mt
                                || (!matches!(eq_a, LuaValue::Nil) && lua_raw_equal(&eq_a, &eq_b));
                            if !matches!(eq_a, LuaValue::Nil) && same_eq {
                                let mut args = vec![lv.clone(), rv.clone()];
                                let results = self.call_function(&eq_a, &mut args, env, varargs)?;
                                let raw = results.into_iter().next().unwrap_or(LuaValue::Nil);
                                let is_eq = raw.is_truthy();
                                return Ok(LuaValue::Bool(match op {
                                    BinOp::Eq => is_eq,
                                    BinOp::Ne => !is_eq,
                                    _ => unreachable!(),
                                }));
                            }
                        }
                        if matches!(op, BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge) {
                            let both_numbers =
                                matches!((&lv, &rv), (LuaValue::Number(_), LuaValue::Number(_)));
                            let both_strings =
                                matches!((&lv, &rv), (LuaValue::Str(_), LuaValue::Str(_)));
                            if !both_numbers
                                && !both_strings
                                && let (LuaValue::Table(_), LuaValue::Table(_)) = (&lv, &rv)
                            {
                                // __lt for </>; for <=/>= prefer __le but
                                // fall back to `not __lt(swapped)`.
                                let (primary, swap, invert) = match op {
                                    BinOp::Lt => ("__lt", false, false),
                                    BinOp::Gt => ("__lt", true, false),
                                    BinOp::Le => ("__le", false, false),
                                    BinOp::Ge => ("__le", true, false),
                                    _ => unreachable!(),
                                };
                                if let Some(handler) =
                                    self.lookup_binop_metamethod(&lv, &rv, primary)
                                {
                                    let mut args = if swap {
                                        vec![rv.clone(), lv.clone()]
                                    } else {
                                        vec![lv.clone(), rv.clone()]
                                    };
                                    let results =
                                        self.call_function(&handler, &mut args, env, varargs)?;
                                    let raw = results.into_iter().next().unwrap_or(LuaValue::Nil);
                                    let truthy = raw.is_truthy();
                                    return Ok(LuaValue::Bool(if invert {
                                        !truthy
                                    } else {
                                        truthy
                                    }));
                                }
                                // __le fallback: `a <= b` => `not (b < a)`.
                                if matches!(op, BinOp::Le | BinOp::Ge)
                                    && let Some(handler) =
                                        self.lookup_binop_metamethod(&lv, &rv, "__lt")
                                {
                                    let mut args = match op {
                                        BinOp::Le => vec![rv.clone(), lv.clone()],
                                        BinOp::Ge => vec![lv.clone(), rv.clone()],
                                        _ => unreachable!(),
                                    };
                                    let results =
                                        self.call_function(&handler, &mut args, env, varargs)?;
                                    let raw = results.into_iter().next().unwrap_or(LuaValue::Nil);
                                    return Ok(LuaValue::Bool(!raw.is_truthy()));
                                }
                            }
                        }
                        self.eval_binop(&lv, op, &rv)
                    }
                }
            }
            Expr::UnaryOp(op, inner) => {
                let val = self.eval_expr(inner, env, varargs)?;
                match op {
                    UnaryOp::Neg => {
                        // (frankenredis-mdxsk) Try __unm before bailing
                        // when the operand isn't a coercible number.
                        // Lua 5.1 invokes __unm(value) and uses the first
                        // return value; non-callable handlers naturally
                        // produce "attempt to call a TYPE value" via
                        // call_function.
                        if val.to_number().is_none()
                            && let LuaValue::Table(t) = &val
                        {
                            let handler = {
                                let inner = t.inner.borrow();
                                inner
                                    .metatable
                                    .as_ref()
                                    .map(|mt| mt.get(&LuaValue::Str(b"__unm".to_vec())))
                                    .unwrap_or(LuaValue::Nil)
                            };
                            if !matches!(handler, LuaValue::Nil) {
                                let mut args = vec![val.clone()];
                                let results =
                                    self.call_function(&handler, &mut args, env, varargs)?;
                                return Ok(results.into_iter().next().unwrap_or(LuaValue::Nil));
                            }
                        }
                        // (frankenredis-7w22v) Use the operand's actual
                        // type name to match Lua 5.1's "a string value" /
                        // "a boolean value" wording.
                        // (frankenredis-9ckvq) Label the operand by its
                        // syntactic accessor when available.
                        let n = val.to_number().ok_or_else(|| {
                            self.type_error_with_label("perform arithmetic on", inner, &val, env)
                        })?;
                        Ok(LuaValue::Number(-n))
                    }
                    UnaryOp::Not => Ok(LuaValue::Bool(!val.is_truthy())),
                    UnaryOp::Len => match &val {
                        LuaValue::Str(s) => Ok(LuaValue::Number(s.len() as f64)),
                        // (frankenredis-y0ri2) `#t` mirrors Lua 5.1
                        // luaH_getn which does a binary border search
                        // when the array part ends in nil (or extends
                        // the search into the hash part). t.len() only
                        // returns the raw array.len(), which diverges
                        // from vendored on sparse / nil-hole tables.
                        LuaValue::Table(t) => {
                            Ok(LuaValue::Number(t.inner.borrow().border_len() as f64))
                        }
                        // (frankenredis-7w22v / frankenredis-9ckvq) Prepend
                        // user_script:1: prefix and label the bad operand.
                        _ => Err(self.type_error_with_label("get length of", inner, &val, env)),
                    },
                }
            }
            Expr::Index(table_expr, key_expr) => {
                let table = self.eval_expr(table_expr, env, varargs)?;
                let key = self.eval_expr(key_expr, env, varargs)?;
                match &table {
                    // (frankenredis-vhbp3) Route through the full __index
                    // metamethod chain so function-valued __index is
                    // invoked rather than silently returning nil.
                    LuaValue::Table(t) => self.table_lookup_with_index_meta(t, &key, env, varargs),
                    // (frankenredis-tbu4k) Lua 5.1 sets the string library
                    // as the metatable __index for strings, so indexing a
                    // string with a string key looks up that field in the
                    // 'string' table (unknown keys yield nil). Non-string
                    // keys (numeric, boolean, etc.) just return nil.
                    LuaValue::Str(_) => Ok(self.lookup_string_field(&key)),
                    // (frankenredis-9ckvq) Label the bad operand by the
                    // syntactic accessor that produced it.
                    _ => Err(self.type_error_with_label("index", table_expr, &table, env)),
                }
            }
            Expr::Field(table_expr, field) => {
                let table = self.eval_expr(table_expr, env, varargs)?;
                match &table {
                    LuaValue::Table(t) => {
                        let key = LuaValue::Str(field.as_bytes().to_vec());
                        self.table_lookup_with_index_meta(t, &key, env, varargs)
                    }
                    // (frankenredis-tbu4k) Same string-as-metatable behavior
                    // as Expr::Index — `s.upper` returns string.upper,
                    // `s.fld` for unknown field returns nil.
                    LuaValue::Str(_) => {
                        Ok(self.lookup_string_field(&LuaValue::Str(field.as_bytes().to_vec())))
                    }
                    _ => Err(self.type_error_with_label("index", table_expr, &table, env)),
                }
            }
            Expr::Call(func_expr, args) => {
                let func = self.eval_expr(func_expr, env, varargs)?;
                let mut arg_vals = self.eval_call_args(args, env, varargs)?;
                // (frankenredis-md71j) Plumb the callee AST node so a
                // non-callable target reports "local 'x'" / "field 'f'" /
                // "upvalue 'y'" / "global 'g'" context.
                let results = self.call_function_with_callee(
                    func_expr,
                    &func,
                    &mut arg_vals,
                    env,
                    varargs,
                    None,
                )?;
                // Write back table mutations (table.sort/insert/remove mutate args[0] in-place).
                // The inner `if` has a side-effect (set_existing_local) so must not be collapsed.
                #[allow(clippy::collapsible_if, clippy::collapsible_match)]
                if let LuaValue::RustFunction(ref name) = func
                    && matches!(
                        name.as_str(),
                        "table.sort" | "table.insert" | "table.remove" | "rawset"
                    )
                {
                    match args.first() {
                        Some(Expr::LocalName(var_name, local)) => {
                            if !env.set_existing_local_slot(*local, arg_vals[0].clone())
                                && !env.set_existing_local(var_name, arg_vals[0].clone())
                            {
                                self.globals.insert(var_name.clone(), arg_vals[0].clone());
                            }
                        }
                        Some(Expr::Name(var_name)) => {
                            match env.set_existing_local(var_name, arg_vals[0].clone()) {
                                true => {}
                                false => {
                                    self.globals.insert(var_name.clone(), arg_vals[0].clone());
                                }
                            }
                        }
                        _ => {}
                    }
                }
                Ok(results.into_iter().next().unwrap_or(LuaValue::Nil))
            }
            Expr::MethodCall(obj_expr, method, args) => {
                let obj = self.eval_expr(obj_expr, env, varargs)?;
                let func = match &obj {
                    LuaValue::Table(t) => {
                        // (frankenredis-vhbp3) Method dispatch uses the
                        // same __index metamethod chain as field reads;
                        // function-valued __index can return the method
                        // dynamically.
                        let key = LuaValue::Str(method.as_bytes().to_vec());
                        self.table_lookup_with_index_meta(t, &key, env, varargs)?
                    }
                    LuaValue::Str(_) => {
                        // (frankenredis-tbu4k) Look up the method in the
                        // 'string' library so `s:upper()` / `s:len()` etc.
                        // resolve to the corresponding RustFunction;
                        // unknown methods still yield nil, which then
                        // reports "attempt to call method 'NAME'".
                        self.lookup_string_field(&LuaValue::Str(method.as_bytes().to_vec()))
                    }
                    _ => {
                        // (frankenredis-md71j) Mirror Lua 5.1's "attempt to
                        // index a TYPE value" wording for the receiver-side
                        // index failure of `obj:m()` against nil/bool/etc.
                        // (frankenredis-aaudb) Route through
                        // type_error_with_label so the receiver accessor
                        // (local 'x' / field 'f' / etc.) is preserved —
                        // upstream emits the indexing error here BEFORE
                        // attempting the call, with full accessor context.
                        return Err(self.type_error_with_label("index", obj_expr, &obj, env));
                    }
                };
                let mut arg_vals = vec![obj.clone()];
                arg_vals.extend(self.eval_call_args(args, env, varargs)?);
                // (frankenredis-md71j) method-call errors carry "method
                // 'NAME'" context regardless of the receiver expression.
                let results = self.call_function_with_callee(
                    obj_expr,
                    &func,
                    &mut arg_vals,
                    env,
                    varargs,
                    Some(method.as_str()),
                )?;
                Ok(results.into_iter().next().unwrap_or(LuaValue::Nil))
            }
            Expr::TableConstructor(fields) => {
                let table = LuaTable::new();
                let mut auto_idx = 1usize;
                let last_idx = fields.len().checked_sub(1);
                for (i, field) in fields.iter().enumerate() {
                    match field {
                        TableField::Positional(expr) => {
                            // (frankenredis-d4vlx) Lua 5.1 expands the
                            // LAST field of a table constructor to its
                            // full multi-value if it's `...` or a function
                            // call. Other positions take only the first
                            // value. Mirrors the call-args expansion rule
                            // in eval_call_args.
                            let is_last_field = Some(i) == last_idx;
                            if is_last_field {
                                let values: Vec<LuaValue> = match expr {
                                    Expr::VarArgs => varargs.clone(),
                                    Expr::Call(func_expr, call_args) => {
                                        let func = self.eval_expr(func_expr, env, varargs)?;
                                        let mut arg_vals =
                                            self.eval_call_args(call_args, env, varargs)?;
                                        self.call_function_with_callee(
                                            func_expr,
                                            &func,
                                            &mut arg_vals,
                                            env,
                                            varargs,
                                            None,
                                        )?
                                    }
                                    Expr::MethodCall(obj_expr, method, call_args) => {
                                        let obj = self.eval_expr(obj_expr, env, varargs)?;
                                        let func = match &obj {
                                            // (frankenredis-vhbp3) Same __index
                                            // metamethod chain handling as the
                                            // single MethodCall arm.
                                            LuaValue::Table(t) => {
                                                let key = LuaValue::Str(method.as_bytes().to_vec());
                                                self.table_lookup_with_index_meta(
                                                    t, &key, env, varargs,
                                                )?
                                            }
                                            LuaValue::Str(_) => self.lookup_string_field(
                                                &LuaValue::Str(method.as_bytes().to_vec()),
                                            ),
                                            _ => LuaValue::Nil,
                                        };
                                        let mut arg_vals = vec![obj];
                                        arg_vals
                                            .extend(self.eval_call_args(call_args, env, varargs)?);
                                        self.call_function_with_callee(
                                            obj_expr,
                                            &func,
                                            &mut arg_vals,
                                            env,
                                            varargs,
                                            Some(method.as_str()),
                                        )?
                                    }
                                    _ => vec![self.eval_expr(expr, env, varargs)?],
                                };
                                for val in values {
                                    set_positional_array_slot(&table, auto_idx, val);
                                    auto_idx += 1;
                                }
                            } else {
                                let val = self.eval_expr(expr, env, varargs)?;
                                set_positional_array_slot(&table, auto_idx, val);
                                auto_idx += 1;
                            }
                        }
                        TableField::Named(name, expr) => {
                            let val = self.eval_expr(expr, env, varargs)?;
                            table.set(LuaValue::Str(name.as_bytes().to_vec()), val);
                        }
                        TableField::Index(key_expr, val_expr) => {
                            let key = self.eval_expr(key_expr, env, varargs)?;
                            let val = self.eval_expr(val_expr, env, varargs)?;
                            // (frankenredis-tb9vb) {[k]=v} constructors
                            // funnel through the same luaV_settable as
                            // assignment, so nil/NaN keys raise here
                            // too — not at first lookup.
                            lua_check_table_key(&key)?;
                            table.set(key, val);
                        }
                    }
                }
                Ok(LuaValue::Table(table))
            }
            Expr::FunctionDef(params, is_variadic, body) => Ok(LuaValue::function(LuaFunc {
                params: params.clone(),
                body: body.clone(),
                is_variadic: *is_variadic,
                captured_env: Some(env.snapshot()),
                env_table: Rc::new(RefCell::new(env.current_global_env())),
                self_name: None,
                source_label: self.current_source_label.clone(),
                identity: next_function_identity(),
            })),
        }
    }

    fn eval_call_args(
        &mut self,
        args: &[Expr],
        env: &mut Env,
        varargs: &mut Vec<LuaValue>,
    ) -> Result<Vec<LuaValue>, String> {
        // For the last argument, expand multi-values (varargs, calls)
        if args.is_empty() {
            return Ok(Vec::new());
        }
        let mut vals = Vec::new();
        for (i, arg) in args.iter().enumerate() {
            if i == args.len() - 1 {
                // Last arg: expand multi-value
                match arg {
                    Expr::VarArgs => {
                        vals.extend(varargs.clone());
                    }
                    Expr::Call(func_expr, call_args) => {
                        let func = self.eval_expr(func_expr, env, varargs)?;
                        let mut arg_vals = self.eval_call_args(call_args, env, varargs)?;
                        // (frankenredis-md71j) Same accessor-context plumbing
                        // as Expr::Call when expanded as a trailing arg.
                        let results = self.call_function_with_callee(
                            func_expr,
                            &func,
                            &mut arg_vals,
                            env,
                            varargs,
                            None,
                        )?;
                        vals.extend(results);
                    }
                    Expr::MethodCall(obj_expr, method, call_args) => {
                        let obj = self.eval_expr(obj_expr, env, varargs)?;
                        let func = match &obj {
                            // (frankenredis-vhbp3) Route table receivers
                            // through the full __index metamethod chain.
                            LuaValue::Table(t) => {
                                let key = LuaValue::Str(method.as_bytes().to_vec());
                                self.table_lookup_with_index_meta(t, &key, env, varargs)?
                            }
                            // (frankenredis-tbu4k) String receivers route
                            // through the string library, same as the
                            // single-call MethodCall arm above.
                            LuaValue::Str(_) => {
                                self.lookup_string_field(&LuaValue::Str(method.as_bytes().to_vec()))
                            }
                            // (frankenredis-aaudb) Non-table receivers
                            // fail at the index step BEFORE the call —
                            // mirror upstream's accessor-aware
                            // "attempt to index local 'x' (a TYPE value)".
                            _ => {
                                return Err(
                                    self.type_error_with_label("index", obj_expr, &obj, env)
                                );
                            }
                        };
                        let mut arg_vals = vec![obj];
                        arg_vals.extend(self.eval_call_args(call_args, env, varargs)?);
                        let results = self.call_function_with_callee(
                            obj_expr,
                            &func,
                            &mut arg_vals,
                            env,
                            varargs,
                            Some(method.as_str()),
                        )?;
                        vals.extend(results);
                    }
                    _ => {
                        vals.push(self.eval_expr(arg, env, varargs)?);
                    }
                }
            } else {
                vals.push(self.eval_expr(arg, env, varargs)?);
            }
        }
        Ok(vals)
    }

    fn eval_expr_list(
        &mut self,
        exprs: &[Expr],
        env: &mut Env,
        varargs: &mut Vec<LuaValue>,
    ) -> Result<Vec<LuaValue>, String> {
        self.eval_call_args(exprs, env, varargs)
    }

    fn prepare_lua_function_env(
        func_value: LuaValue,
        lua_func: &LuaFunc,
        args: &[LuaValue],
    ) -> (Env, Vec<LuaValue>) {
        let mut new_env = match &lua_func.captured_env {
            Some(captured) => Env::from_captured(captured),
            None => Env::new(),
        };
        new_env.global_env = lua_func.env_table.borrow().clone();
        new_env.push_scope();
        if let Some(name) = &lua_func.self_name {
            new_env.set_local(name, func_value);
        }
        for (i, param) in lua_func.params.iter().enumerate() {
            let val = args.get(i).cloned().unwrap_or(LuaValue::Nil);
            new_env.set_local(param, val);
        }
        let func_varargs = if lua_func.is_variadic {
            args.get(lua_func.params.len()..).unwrap_or(&[]).to_vec()
        } else {
            Vec::new()
        };
        (new_env, func_varargs)
    }

    fn exec_coroutine_stmts(
        &mut self,
        stmts: &[(u32, Stmt)],
        start_pc: usize,
        env: &mut Env,
        varargs: &mut Vec<LuaValue>,
    ) -> Result<CoroutineRun, String> {
        for (offset, (line, stmt)) in stmts.iter().enumerate().skip(start_pc) {
            self.current_line = *line;
            self.iterations += 1;
            if self.iterations > MAX_ITERATIONS {
                return Err("script exceeded maximum iteration count".to_string());
            }
            match self.exec_stmt(stmt, env, varargs) {
                Ok(ControlFlow::None) => {}
                Ok(ControlFlow::Return(vals)) => return Ok(CoroutineRun::Complete(vals)),
                Ok(ControlFlow::Break) => return Ok(CoroutineRun::Complete(vec![LuaValue::Nil])),
                Err(err) if is_lua_yield_signal(&err) && self.pending_yield.is_some() => {
                    // (frankenredis-sjuu1) Real coroutine.yield always
                    // sets pending_yield BEFORE returning the sentinel.
                    // Gating on pending_yield.is_some() prevents a user
                    // script from forging the yield via
                    // error('__frankenredis_lua_coroutine_yield__'),
                    // which would otherwise reach this arm with
                    // pending_yield == None and produce a spoofed
                    // empty yield.
                    let values = self.pending_yield.take().unwrap_or_default();
                    return Ok(CoroutineRun::Yield {
                        values,
                        next_pc: offset + 1,
                    });
                }
                Err(err) => return Err(err),
            }
        }
        Ok(CoroutineRun::Complete(Vec::new()))
    }

    #[allow(clippy::too_many_arguments)]
    fn resume_numeric_for_continuation(
        &mut self,
        name: String,
        stop: f64,
        step: f64,
        body: Block,
        mut current: f64,
        mut body_pc: usize,
        outer_stmts: &[(u32, Stmt)],
        outer_pc: usize,
        env: &mut Env,
        varargs: &mut Vec<LuaValue>,
    ) -> Result<CoroutineRun, String> {
        loop {
            match self.exec_numeric_for_body_from(
                &name, stop, step, &body, current, body_pc, env, varargs,
            ) {
                Ok(ControlFlow::None) => {}
                Ok(ControlFlow::Break) => {
                    env.pop_scope();
                    break;
                }
                Ok(ControlFlow::Return(vals)) => {
                    env.pop_scope();
                    return Ok(CoroutineRun::Complete(vals));
                }
                Err(err) if is_lua_yield_signal(&err) && self.pending_yield.is_some() => {
                    let values = self.pending_yield.take().unwrap_or_default();
                    return Ok(CoroutineRun::Yield {
                        values,
                        next_pc: outer_pc,
                    });
                }
                Err(err) => {
                    env.pop_scope();
                    return Err(err);
                }
            }
            env.pop_scope();
            current += step;
            if (step > 0.0 && current > stop) || (step < 0.0 && current < stop) {
                break;
            }
            env.push_scope();
            env.set_local(&name, LuaValue::Number(current));
            body_pc = 0;
        }
        self.exec_coroutine_stmts(outer_stmts, outer_pc, env, varargs)
    }

    fn resume_coroutine_continuation(
        &mut self,
        continuation: LuaCoroutineContinuation,
        resume_args: &[LuaValue],
        outer_stmts: &[(u32, Stmt)],
        outer_pc: usize,
        env: &mut Env,
        varargs: &mut Vec<LuaValue>,
    ) -> Result<CoroutineRun, String> {
        match continuation {
            LuaCoroutineContinuation::Assign {
                lhs,
                prefix,
                remaining,
                yield_was_last,
            } => {
                let vals = self.complete_exprs_after_yield(
                    prefix,
                    &remaining,
                    yield_was_last,
                    resume_args,
                    env,
                    varargs,
                )?;
                for (i, lhs_expr) in lhs.iter().enumerate() {
                    let val = vals.get(i).cloned().unwrap_or(LuaValue::Nil);
                    self.assign_to(lhs_expr, val, env, varargs)?;
                }
                self.exec_coroutine_stmts(outer_stmts, outer_pc, env, varargs)
            }
            LuaCoroutineContinuation::LocalAssign {
                names,
                prefix,
                remaining,
                yield_was_last,
            } => {
                let vals = self.complete_exprs_after_yield(
                    prefix,
                    &remaining,
                    yield_was_last,
                    resume_args,
                    env,
                    varargs,
                )?;
                for (i, name) in names.iter().enumerate() {
                    let val = vals.get(i).cloned().unwrap_or(LuaValue::Nil);
                    env.set_local(name, val);
                }
                self.exec_coroutine_stmts(outer_stmts, outer_pc, env, varargs)
            }
            LuaCoroutineContinuation::Return {
                prefix,
                remaining,
                yield_was_last,
            } => {
                let vals = self.complete_exprs_after_yield(
                    prefix,
                    &remaining,
                    yield_was_last,
                    resume_args,
                    env,
                    varargs,
                )?;
                Ok(CoroutineRun::Complete(vals))
            }
            LuaCoroutineContinuation::NumericFor {
                name,
                stop,
                step,
                body,
                current,
                body_pc,
            } => self.resume_numeric_for_continuation(
                name,
                stop,
                step,
                body,
                current,
                body_pc,
                outer_stmts,
                outer_pc,
                env,
                varargs,
            ),
            LuaCoroutineContinuation::If {
                then_body,
                remaining,
                else_body,
            } => {
                // The yielded value is the branch condition's result.
                let cond_val = resume_args.first().cloned().unwrap_or(LuaValue::Nil);
                let cf = if cond_val.is_truthy() {
                    self.exec_block(&then_body, env, varargs)?
                } else if !remaining.is_empty() || else_body.is_some() {
                    // Fall through to the remaining branches / else, evaluated
                    // normally via the Stmt::If arm — which itself detects a
                    // bare-yield `elseif` condition and suspends again. Real
                    // redis 7.2.4 supports such chained yields (each `elseif
                    // coroutine.yield(...)` is a fresh suspension point), so
                    // propagate the re-yield as another CoroutineRun::Yield
                    // rather than erroring: start_coroutine_yield has already
                    // stored the nested If continuation, and next_pc stays at
                    // outer_pc so that once the chain resolves, execution
                    // resumes right after the original `if`.
                    match self.exec_stmt(&Stmt::If(remaining, else_body), env, varargs) {
                        Ok(cf) => cf,
                        Err(e) if is_lua_yield_signal(&e) && self.pending_yield.is_some() => {
                            let values = self.pending_yield.take().unwrap_or_default();
                            return Ok(CoroutineRun::Yield {
                                values,
                                next_pc: outer_pc,
                            });
                        }
                        Err(e) => return Err(e),
                    }
                } else {
                    ControlFlow::None
                };
                match cf {
                    ControlFlow::Return(vals) => Ok(CoroutineRun::Complete(vals)),
                    ControlFlow::Break => Ok(CoroutineRun::Complete(vec![LuaValue::Nil])),
                    ControlFlow::None => {
                        self.exec_coroutine_stmts(outer_stmts, outer_pc, env, varargs)
                    }
                }
            }
            LuaCoroutineContinuation::While { cond, body } => {
                // The yielded value is this iteration's loop-condition result.
                let cond_val = resume_args.first().cloned().unwrap_or(LuaValue::Nil);
                if cond_val.is_truthy() {
                    match self.exec_block(&body, env, varargs)? {
                        ControlFlow::Return(vals) => return Ok(CoroutineRun::Complete(vals)),
                        // `break` inside the body exits the loop; fall through
                        // to the outer statements after the while.
                        ControlFlow::Break => {}
                        ControlFlow::None => {
                            // Loop back: re-execute the while, which suspends
                            // again at its bare-yield condition. Propagate that
                            // re-yield as a fresh CoroutineRun::Yield (the nested
                            // While continuation is already stored); next_pc
                            // stays at outer_pc so the post-loop statements run
                            // once the condition finally reads falsy.
                            match self.exec_stmt(
                                &Stmt::While(cond, body),
                                env,
                                varargs,
                            ) {
                                Ok(ControlFlow::Return(vals)) => {
                                    return Ok(CoroutineRun::Complete(vals));
                                }
                                Ok(ControlFlow::Break | ControlFlow::None) => {}
                                Err(e)
                                    if is_lua_yield_signal(&e)
                                        && self.pending_yield.is_some() =>
                                {
                                    let values = self.pending_yield.take().unwrap_or_default();
                                    return Ok(CoroutineRun::Yield {
                                        values,
                                        next_pc: outer_pc,
                                    });
                                }
                                Err(e) => return Err(e),
                            }
                        }
                    }
                }
                // Condition read falsy (or the loop broke): continue with the
                // statements following the while.
                self.exec_coroutine_stmts(outer_stmts, outer_pc, env, varargs)
            }
            LuaCoroutineContinuation::Repeat { body, cond } => {
                // We suspended at the until-condition with the just-run
                // iteration's body scope still on the env; pop it now.
                let cond_val = resume_args.first().cloned().unwrap_or(LuaValue::Nil);
                env.pop_scope();
                if cond_val.is_truthy() {
                    // until-condition true → exit the loop.
                    return self.exec_coroutine_stmts(outer_stmts, outer_pc, env, varargs);
                }
                // Falsy → run another iteration by re-executing the repeat,
                // which suspends again at its bare-yield until-condition. Chain
                // the re-yield as a fresh CoroutineRun::Yield; next_pc stays at
                // outer_pc so post-loop statements run once it finally reads true.
                match self.exec_stmt(&Stmt::Repeat(body, cond), env, varargs) {
                    Ok(ControlFlow::Return(vals)) => Ok(CoroutineRun::Complete(vals)),
                    Ok(ControlFlow::Break | ControlFlow::None) => {
                        self.exec_coroutine_stmts(outer_stmts, outer_pc, env, varargs)
                    }
                    Err(e) if is_lua_yield_signal(&e) && self.pending_yield.is_some() => {
                        let values = self.pending_yield.take().unwrap_or_default();
                        Ok(CoroutineRun::Yield {
                            values,
                            next_pc: outer_pc,
                        })
                    }
                    Err(e) => Err(e),
                }
            }
            LuaCoroutineContinuation::GenericFor {
                names,
                prefix,
                remaining,
                yield_was_last,
                body,
            } => {
                // The yielded value(s) complete the iterator triple; then run
                // the loop to completion and continue past it.
                let iter_vals = self.complete_exprs_after_yield(
                    prefix,
                    &remaining,
                    yield_was_last,
                    resume_args,
                    env,
                    varargs,
                )?;
                match self
                    .run_generic_for_from_iter_vals(&names, iter_vals, &body, env, varargs)?
                {
                    ControlFlow::Return(vals) => Ok(CoroutineRun::Complete(vals)),
                    ControlFlow::Break | ControlFlow::None => {
                        self.exec_coroutine_stmts(outer_stmts, outer_pc, env, varargs)
                    }
                }
            }
        }
    }

    fn resume_coroutine(
        &mut self,
        coroutine: &LuaCoroutine,
        args: &[LuaValue],
    ) -> Result<Vec<LuaValue>, String> {
        let (func, mut env, mut func_varargs, pc, continuation) = {
            let mut inner = coroutine.inner.borrow_mut();
            match inner.status {
                LuaCoroutineStatus::Running => {
                    return Ok(vec![
                        LuaValue::Bool(false),
                        LuaValue::Str(b"cannot resume non-suspended coroutine".to_vec()),
                    ]);
                }
                LuaCoroutineStatus::Dead => {
                    return Ok(vec![
                        LuaValue::Bool(false),
                        LuaValue::Str(b"cannot resume dead coroutine".to_vec()),
                    ]);
                }
                LuaCoroutineStatus::Suspended => {}
            }
            inner.status = LuaCoroutineStatus::Running;
            let func = inner.func.clone();
            let pc = inner.pc;
            let continuation = inner.continuation.take();
            if let Some(env) = inner.env.take() {
                (
                    func,
                    env,
                    std::mem::take(&mut inner.varargs),
                    pc,
                    continuation,
                )
            } else {
                let func_value = LuaValue::function(func.clone());
                let (env, func_varargs) = Self::prepare_lua_function_env(func_value, &func, args);
                (func, env, func_varargs, pc, continuation)
            }
        };

        let previous_coroutine = self.current_coroutine.replace(coroutine.clone());
        let previous_yield = self.pending_yield.take();
        // Reset nested-exec depth at the coroutine boundary so a yield
        // from the body's outer-stmt level reads as depth 0 (resumable
        // by exec_coroutine_stmts' PC tracking) even though eval_script
        // and any enclosing call_function bumped the outer counter.
        // Yields from inside the body's nested control-flow blocks
        // bump back to >= 1 and are correctly rejected by yield's
        // depth check. (frankenredis-ztawj)
        let previous_nested_depth = std::mem::replace(&mut self.nested_exec_stmts_depth, 0);
        // Reset the bare-stmt flag too — the coroutine body's outer
        // stmts are dispatched by exec_coroutine_stmts (not by an
        // outer Stmt::Expression), so the flag must start false and
        // be set per-stmt by exec_stmt's Stmt::Expression arm.
        // (frankenredis-gdbca)
        let previous_bare_stmt = std::mem::replace(&mut self.inside_bare_expression_stmt, false);
        // (frankenredis-0k259) The coroutine body is a Lua function but it
        // doesn't enter through call_function — push the kind frame here
        // so error()/assert() inside the body see a Lua frame at level 1.
        self.lua_frame_kinds.push(true);
        let run_result = if let Some(continuation) = continuation {
            self.resume_coroutine_continuation(
                continuation,
                args,
                &func.body,
                pc,
                &mut env,
                &mut func_varargs,
            )
        } else {
            self.exec_coroutine_stmts(&func.body, pc, &mut env, &mut func_varargs)
        };
        self.lua_frame_kinds.pop();
        self.inside_bare_expression_stmt = previous_bare_stmt;
        self.nested_exec_stmts_depth = previous_nested_depth;
        self.current_coroutine = previous_coroutine;
        self.pending_yield = previous_yield;

        match run_result {
            Ok(CoroutineRun::Complete(vals)) => {
                coroutine.inner.borrow_mut().status = LuaCoroutineStatus::Dead;
                let mut out = Vec::with_capacity(vals.len() + 1);
                out.push(LuaValue::Bool(true));
                out.extend(vals);
                Ok(out)
            }
            Ok(CoroutineRun::Yield { values, next_pc }) => {
                let mut inner = coroutine.inner.borrow_mut();
                inner.status = LuaCoroutineStatus::Suspended;
                inner.env = Some(env);
                inner.varargs = func_varargs;
                inner.pc = next_pc;
                drop(inner);

                let mut out = Vec::with_capacity(values.len() + 1);
                out.push(LuaValue::Bool(true));
                out.extend(values);
                Ok(out)
            }
            Err(err) => {
                coroutine.inner.borrow_mut().status = LuaCoroutineStatus::Dead;
                Ok(vec![LuaValue::Bool(false), LuaValue::Str(err.into_bytes())])
            }
        }
    }

    fn eval_binop(&self, lv: &LuaValue, op: &BinOp, rv: &LuaValue) -> Result<LuaValue, String> {
        match op {
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod | BinOp::Pow => {
                // (frankenredis-4hhz5) Mirror Lua 5.1's
                // "attempt to perform arithmetic on a <type> value"
                // wording rather than the generic "non-number". Use the
                // first non-number operand's actual type. Numeric
                // strings still coerce via to_number per Lua semantics.
                let a = lv.to_number().ok_or_else(|| {
                    format!(
                        "user_script:1: attempt to perform arithmetic on a {} value",
                        lv.type_name()
                    )
                })?;
                let b = rv.to_number().ok_or_else(|| {
                    format!(
                        "user_script:1: attempt to perform arithmetic on a {} value",
                        rv.type_name()
                    )
                })?;
                let result = match op {
                    BinOp::Add => a + b,
                    BinOp::Sub => a - b,
                    BinOp::Mul => a * b,
                    BinOp::Div => a / b,
                    BinOp::Mod => a - (a / b).floor() * b,
                    BinOp::Pow => a.powf(b),
                    _ => return Err("unsupported arithmetic operator".to_string()),
                };
                Ok(LuaValue::Number(result))
            }
            BinOp::Concat => {
                // (frankenredis-7w22v) Lua 5.1 only concatenates strings and
                // numbers; nil/bool/table/function/thread operands raise
                // "attempt to concatenate a <type> value". Check the right
                // operand first to match upstream's evaluation order.
                fn concat_bytes(v: &LuaValue) -> Result<Vec<u8>, String> {
                    match v {
                        LuaValue::Str(s) => Ok(s.clone()),
                        LuaValue::Number(_) => Ok(v.to_display_string()),
                        _ => Err(format!(
                            "user_script:1: attempt to concatenate a {} value",
                            v.type_name()
                        )),
                    }
                }
                let mut a = concat_bytes(lv)?;
                a.extend_from_slice(&concat_bytes(rv)?);
                Ok(LuaValue::Str(a))
            }
            BinOp::Eq => Ok(LuaValue::Bool(lua_raw_equal(lv, rv))),
            BinOp::Ne => Ok(LuaValue::Bool(!lua_raw_equal(lv, rv))),
            BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge => {
                let result = match (lv, rv) {
                    (LuaValue::Number(a), LuaValue::Number(b)) => match op {
                        BinOp::Lt => a < b,
                        BinOp::Gt => a > b,
                        BinOp::Le => a <= b,
                        BinOp::Ge => a >= b,
                        _ => return Err("unsupported comparison operator".to_string()),
                    },
                    (LuaValue::Str(a), LuaValue::Str(b)) => match op {
                        BinOp::Lt => a < b,
                        BinOp::Gt => a > b,
                        BinOp::Le => a <= b,
                        BinOp::Ge => a >= b,
                        _ => return Err("unsupported comparison operator".to_string()),
                    },
                    _ => {
                        // (frankenredis-7w22v) Lua 5.1 uses two different
                        // phrasings: "attempt to compare A with B" when the
                        // operand types differ, and "attempt to compare two
                        // T values" when both operands share a non-orderable
                        // type (e.g. two booleans or two tables).
                        let an = lv.type_name();
                        let bn = rv.type_name();
                        return Err(if an == bn {
                            format!("user_script:1: attempt to compare two {} values", an)
                        } else {
                            format!("user_script:1: attempt to compare {} with {}", an, bn)
                        });
                    }
                };
                Ok(LuaValue::Bool(result))
            }
            BinOp::And | BinOp::Or => {
                Err("unexpected logical operator in binary evaluation".to_string())
            }
        }
    }

    /// Look up `key` in the global `string` table. Used to implement
    /// Lua 5.1's string-as-metatable __index behavior: `s.foo` and
    /// `s:foo()` resolve `foo` via the string library. Unknown keys
    /// (including non-string keys) return nil. (frankenredis-tbu4k)
    /// Build the `_G` table that mirrors the post-init globals and install it
    /// under the "_G" key. Idempotent so lazy callers can force materialization
    /// from global lookup, getfenv, and setfenv surfaces. (frankenredis-u24vv)
    fn ensure_g_table(&mut self) -> LuaTable {
        if let Some(LuaValue::Table(table)) = self.globals.get("_G") {
            return table.clone();
        }
        let g_table = LuaTable::new();
        for (k, v) in &self.globals {
            g_table.set(LuaValue::Str(k.as_bytes().to_vec()), v.clone());
        }
        g_table.set(
            LuaValue::Str(b"_G".to_vec()),
            LuaValue::Table(g_table.clone()),
        );
        let mt = LuaTable::new();
        mt.set(
            LuaValue::Str(b"__index".to_vec()),
            LuaValue::RustFunction("__fr_g_protected_index".to_string()),
        );
        mt.set(
            LuaValue::Str(b"__newindex".to_vec()),
            LuaValue::RustFunction("__fr_g_readonly_newindex".to_string()),
        );
        g_table.inner.borrow_mut().metatable = Some(mt);
        self.globals
            .insert("_G".to_string(), LuaValue::Table(g_table.clone()));
        g_table
    }

    fn default_global_env_table(&mut self) -> LuaTable {
        self.ensure_g_table()
    }

    fn effective_global_env_table(&mut self, env: &Env) -> LuaTable {
        env.current_global_env()
            .unwrap_or_else(|| self.default_global_env_table())
    }

    fn lookup_string_field(&self, key: &LuaValue) -> LuaValue {
        if let Some(LuaValue::Table(t)) = self.globals.get("string") {
            t.get_with_index(key)
        } else {
            LuaValue::Nil
        }
    }

    /// Read `key` from `table` honoring the full Lua 5.1 __index
    /// metamethod chain: if a raw lookup yields nil, walk the metatable
    /// __index value. When __index is a TABLE, recurse into it (already
    /// handled inside LuaTable::get_with_index_depth). When __index is
    /// a FUNCTION (RustFunction / Lua Function / WrappedCoroutine),
    /// invoke it as `__index(table, key)` and return the first result.
    /// Cap recursion at 16 hops, matching get_with_index_depth.
    /// (frankenredis-vhbp3)
    fn table_lookup_with_index_meta(
        &mut self,
        table: &LuaTable,
        key: &LuaValue,
        env: &mut Env,
        varargs: &mut Vec<LuaValue>,
    ) -> Result<LuaValue, String> {
        let mut current = table.clone();
        for _ in 0..16 {
            let raw = current.inner.borrow().get(key);
            if !matches!(raw, LuaValue::Nil) {
                return Ok(raw);
            }
            let handler = {
                let inner = current.inner.borrow();
                match &inner.metatable {
                    Some(mt) => mt.get(&LuaValue::Str(b"__index".to_vec())),
                    None => return Ok(LuaValue::Nil),
                }
            };
            match handler {
                LuaValue::Nil => return Ok(LuaValue::Nil),
                LuaValue::Table(next) => {
                    current = next;
                    continue;
                }
                callable @ (LuaValue::RustFunction(_)
                | LuaValue::Function(_)
                | LuaValue::WrappedCoroutine(_)) => {
                    let mut args = vec![LuaValue::Table(current.clone()), key.clone()];
                    let results = self.call_function(&callable, &mut args, env, varargs)?;
                    return Ok(results.into_iter().next().unwrap_or(LuaValue::Nil));
                }
                _ => return Ok(LuaValue::Nil),
            }
        }
        // (frankenredis-91w0c) Upstream lvm.c::luaV_gettable raises
        // 'loop in gettable' when the MAXTAGLOOP cap is exhausted —
        // the canonical signal for a cyclic __index chain like
        // setmetatable(t, {__index=t}). fr previously returned nil
        // silently, swallowing the diagnosis.
        Err("user_script:1: loop in gettable".to_string())
    }

    /// Compute the Lua 5.1-style accessor label for a call site, e.g.
    /// `"local 'x'"`, `"upvalue 'y'"`, `"global 'g'"`, `"field 'f'"`,
    /// `"method 'm'"`, or `"field '?'"` for non-literal index keys.
    /// Returns `None` for call sites without a syntactic name (parenthesized
    /// expressions, call results, etc.), which match vendored Lua's
    /// (frankenredis-9ckvq) Build Lua 5.1's accessor-aware runtime error
    /// for index/arith/length/concat/call when the offending operand
    /// resolved from a known syntactic source (a local, upvalue, field,
    /// or global). Mirrors lvm.c::luaG_typeerror which walks debug info
    /// to recover the variable name; fr's tree-walker has direct AST
    /// access so it can label without bytecode metadata. Returns the
    /// unlabeled form when the operand has no resolvable syntactic
    /// name (e.g. arithmetic on a function-call result).
    fn type_error_with_label(
        &self,
        op_phrase: &str,
        expr: &Expr,
        val: &LuaValue,
        env: &Env,
    ) -> String {
        let label = self.callee_label(expr, env);
        match label {
            Some(l) => format!(
                "user_script:1: attempt to {op_phrase} {l} (a {} value)",
                val.type_name()
            ),
            None => format!(
                "user_script:1: attempt to {op_phrase} a {} value",
                val.type_name()
            ),
        }
    }

    /// "attempt to call a TYPE value" wording sans accessor.
    /// (frankenredis-md71j)
    fn callee_label(&self, expr: &Expr, env: &Env) -> Option<String> {
        match expr {
            Expr::LocalName(name, local) => match env
                .classify_slot(*local)
                .or_else(|| env.classify_name(name))
            {
                Some(true) => Some(format!("local '{name}'")),
                Some(false) => Some(format!("upvalue '{name}'")),
                None => None,
            },
            Expr::Name(name) => match env.classify_name(name) {
                Some(true) => Some(format!("local '{name}'")),
                Some(false) => Some(format!("upvalue '{name}'")),
                None => {
                    if self.globals.contains_key(name) {
                        Some(format!("global '{name}'"))
                    } else {
                        None
                    }
                }
            },
            Expr::Field(_, name) => Some(format!("field '{name}'")),
            Expr::Index(_, key) => match key.as_ref() {
                Expr::Str(s) if !s.is_empty() => match std::str::from_utf8(s) {
                    Ok(name) => Some(format!("field '{name}'")),
                    Err(_) => Some("field '?'".to_string()),
                },
                _ => Some("field '?'".to_string()),
            },
            _ => None,
        }
    }

    /// Variant of `call_function` for call sites that have a syntactic
    /// callee expression. When `func` is non-callable, rewrites the error
    /// to include the accessor context Lua 5.1 emits (e.g. "attempt to
    /// call local 'x' (a nil value)" or "attempt to call field 'm'
    /// (a nil value)"). (frankenredis-md71j)
    /// Format Lua 5.1's `luaL_argerror` wording for a C-builtin
    /// bad-argument case. When invoked through an AST call site, the
    /// name comes from the call-site variable and the error carries
    /// the `user_script:1:` source prefix. When invoked indirectly
    /// (e.g. `pcall(select, ...)`), the C closure has no debug name —
    /// vendored emits `'?'` and no source prefix. (frankenredis-557p3)
    fn format_builtin_argerror(
        &self,
        _fallback_name: &str,
        arg_idx: usize,
        reason: &str,
    ) -> String {
        // (frankenredis-rbec9) Method-style invocation (`t:f(args)`) is
        // desugared to `t.f(t, args...)`. Lua 5.1 reports arg #1 type
        // failures with the `calling 'f' on bad self (...)` wording
        // instead of the standard bad-argument template. Match upstream
        // when both: invocation came via colon-call AND the failing arg
        // is the synthetic self (arg #1).
        if arg_idx == 1
            && self.current_invocation_is_method
            && let Some(name) = &self.current_invocation_name
        {
            return format!("user_script:1: calling '{name}' on bad self ({reason})");
        }
        match &self.current_invocation_name {
            Some(name) => format!("user_script:1: bad argument #{arg_idx} to '{name}' ({reason})"),
            None => format!("bad argument #{arg_idx} to '?' ({reason})"),
        }
    }

    /// Bare syntactic name for an AST call site, used by C-builtin
    /// bad-argument errors. Mirrors Lua 5.1's `lua_getinfo` "n.name":
    /// the variable used to invoke the function, not the function's
    /// internal name. (frankenredis-557p3)
    fn ast_call_name(&self, callee_expr: &Expr, method_override: Option<&str>) -> Option<String> {
        if let Some(m) = method_override {
            return Some(m.to_string());
        }
        match callee_expr {
            Expr::Name(n) | Expr::LocalName(n, _) => Some(n.clone()),
            Expr::Field(_, f) => Some(f.clone()),
            Expr::Index(_, key) => match key.as_ref() {
                Expr::Str(s) if !s.is_empty() => std::str::from_utf8(s).ok().map(str::to_string),
                _ => None,
            },
            _ => None,
        }
    }

    fn call_function_with_callee(
        &mut self,
        callee_expr: &Expr,
        func: &LuaValue,
        args: &mut [LuaValue],
        env: &mut Env,
        varargs: &mut Vec<LuaValue>,
        method_override: Option<&str>,
    ) -> Result<Vec<LuaValue>, String> {
        if matches!(
            func,
            LuaValue::RustFunction(_) | LuaValue::Function(_) | LuaValue::WrappedCoroutine(_)
        ) {
            // (frankenredis-557p3) Stash the AST-derived call-site name
            // so C-builtin errors can use it (matching Lua 5.1's
            // lua_getinfo("n.name") behavior). Restored on return.
            // (frankenredis-rbec9) Also stash whether this was a
            // method-style invocation so format_builtin_argerror can
            // emit the `'calling 'f' on bad self'` wording for arg #1.
            let inv_name = self.ast_call_name(callee_expr, method_override);
            let prev = std::mem::replace(&mut self.current_invocation_name, inv_name);
            let prev_method = std::mem::replace(
                &mut self.current_invocation_is_method,
                method_override.is_some(),
            );
            let result = self.call_function(func, args, env, varargs);
            self.current_invocation_name = prev;
            self.current_invocation_is_method = prev_method;
            return result;
        }
        // (frankenredis-2c7hj) Tables with a callable __call metamethod
        // delegate to call_function (which prepends the table to args).
        // Non-callable __call values fall through to the error path so
        // the accessor-aware "attempt to call <label> (a table value)"
        // wording is preserved.
        if matches!(func, LuaValue::Table(_)) && self.metatable_call_handler(func).is_some() {
            let inv_name = self.ast_call_name(callee_expr, method_override);
            let prev = std::mem::replace(&mut self.current_invocation_name, inv_name);
            let prev_method = std::mem::replace(
                &mut self.current_invocation_is_method,
                method_override.is_some(),
            );
            let result = self.call_function(func, args, env, varargs);
            self.current_invocation_name = prev;
            self.current_invocation_is_method = prev_method;
            return result;
        }
        let label = match method_override {
            Some(m) => Some(format!("method '{m}'")),
            None => self.callee_label(callee_expr, env),
        };
        let ty = match func {
            LuaValue::Coroutine(_) => "thread",
            other => other.type_name(),
        };
        Err(match label {
            Some(l) => format!("user_script:1: attempt to call {l} (a {ty} value)"),
            None => format!("user_script:1: attempt to call a {ty} value"),
        })
    }

    /// Look up the first non-nil binop metamethod (`__concat`, `__add`,
    /// `__sub`, `__eq`, etc.) by checking the LHS metatable first then
    /// the RHS metatable. Unlike `__index`/`__call`/`__newindex` we do
    /// NOT filter for callability — Lua 5.1 invokes the handler
    /// unconditionally and a non-callable handler produces the standard
    /// "attempt to call a TYPE value" error through call_function.
    /// (frankenredis-hqevr)
    fn lookup_binop_metamethod(
        &self,
        lv: &LuaValue,
        rv: &LuaValue,
        name: &str,
    ) -> Option<LuaValue> {
        let key = LuaValue::Str(name.as_bytes().to_vec());
        for operand in [lv, rv] {
            if let LuaValue::Table(t) = operand {
                let inner = t.inner.borrow();
                if let Some(mt) = &inner.metatable {
                    let handler = mt.get(&key);
                    if !matches!(handler, LuaValue::Nil) {
                        return Some(handler);
                    }
                }
            }
        }
        None
    }

    /// Resolve a value's `__call` metamethod. Returns the bound
    /// callable LuaValue only when the handler is a function/closure;
    /// non-callable handlers (table/string/etc.) yield `None` and the
    /// caller emits the normal "attempt to call a TYPE value" error —
    /// vendored Lua 5.1 does not chain `__call` recursively through
    /// non-function handlers. (frankenredis-2c7hj)
    fn metatable_call_handler(&self, value: &LuaValue) -> Option<LuaValue> {
        let table = match value {
            LuaValue::Table(t) => t,
            _ => return None,
        };
        let handler = {
            let inner = table.inner.borrow();
            let mt = inner.metatable.as_ref()?;
            mt.get(&LuaValue::Str(b"__call".to_vec()))
        };
        match handler {
            LuaValue::RustFunction(_) | LuaValue::Function(_) | LuaValue::WrappedCoroutine(_) => {
                Some(handler)
            }
            _ => None,
        }
    }

    fn call_function(
        &mut self,
        func: &LuaValue,
        args: &mut [LuaValue],
        env: &mut Env,
        _varargs: &mut Vec<LuaValue>,
    ) -> Result<Vec<LuaValue>, String> {
        self.call_depth += 1;
        if self.call_depth > MAX_CALL_DEPTH {
            self.call_depth -= 1;
            return Err("script exceeded maximum call depth".to_string());
        }
        // (frankenredis-4ovjf) Snapshot the caller's frame kind BEFORE
        // pushing so the bad-callable arms below can decide whether to
        // prepend the user_script:1: source-location prefix. Upstream's
        // luaG_addinfo derives source:line from the immediate Lua
        // caller's frame; the chunk itself counts as a Lua frame, so
        // an empty kind stack also yields "caller is Lua". A C caller
        // (RustFunction frame, e.g. pcall invoking a non-callable)
        // hides the source.
        let caller_is_lua_frame = match self.lua_frame_kinds.last() {
            None => true,
            Some(true) => true,
            Some(false) => false,
        };
        // (frankenredis-0k259) Push a frame for this call so error()/assert()
        // can walk `level` entries back through the kind stack to decide
        // whether to prepend the user_script:1 source-location prefix.
        // LuaValue::Function is the only kind that counts as a Lua frame.
        self.lua_frame_kinds
            .push(matches!(func, LuaValue::Function(_)));
        // (frankenredis-2c7hj) Tables with a callable `__call` metamethod
        // act like the underlying function with the table prepended as
        // the first arg. Handled here so internal callers (iterators,
        // pcall, coroutine resumption, sort comparators) honor the same
        // semantics as AST call sites.
        if let Some(callable) = self.metatable_call_handler(func) {
            let mut new_args = Vec::with_capacity(args.len() + 1);
            new_args.push(func.clone());
            new_args.extend_from_slice(args);
            self.call_depth -= 1;
            self.lua_frame_kinds.pop();
            return self.call_function(&callable, &mut new_args, env, _varargs);
        }
        let result = match func {
            LuaValue::RustFunction(name) => self.call_builtin(name, args, env),
            LuaValue::WrappedCoroutine(coroutine) => {
                let resumed = self.resume_coroutine(coroutine, args)?;
                match resumed.as_slice() {
                    [LuaValue::Bool(true), values @ ..] => Ok(values.to_vec()),
                    [LuaValue::Bool(false), LuaValue::Str(err), ..] => {
                        Err(String::from_utf8_lossy(err).to_string())
                    }
                    [LuaValue::Bool(false), ..] => Err("cannot resume coroutine".to_string()),
                    _ => Ok(Vec::new()),
                }
            }
            LuaValue::Function(lua_func) => {
                let (mut new_env, mut func_varargs) =
                    Self::prepare_lua_function_env(func.clone(), lua_func, args);
                // (frankenredis-ycaog) Functions carrying a source_label
                // (loaded via loadstring/load) push their chunk label so
                // any inner function definitions inherit it and runtime
                // errors get rewritten to that prefix on the way out.
                let prev_label = if lua_func.source_label.is_some() {
                    std::mem::replace(
                        &mut self.current_source_label,
                        lua_func.source_label.clone(),
                    )
                } else {
                    self.current_source_label.clone()
                };
                let result = self.exec_stmts(&lua_func.body, &mut new_env, &mut func_varargs);
                if lua_func.source_label.is_some() {
                    self.current_source_label = prev_label;
                }
                match result {
                    Ok(ControlFlow::Return(vals)) => Ok(vals),
                    Ok(_) => Ok(vec![LuaValue::Nil]),
                    Err(e) => {
                        if let Some(label) = lua_func.source_label.as_deref() {
                            // Rewrite the standard "user_script:" prefix
                            // (the only error prefix fr emits) to the
                            // chunk label. Errors that already carry a
                            // chunk label or that don't have any prefix
                            // pass through unchanged.
                            if let Some(rest) = e.strip_prefix("user_script:") {
                                return Err(format!("{label}:{rest}"));
                            }
                        }
                        Err(e)
                    }
                }
            }
            // (frankenredis-8w2ag) Prepend the standard
            // 'user_script:N: ' source-location prefix that upstream
            // Lua 5.1 wraps around every runtime error.
            //
            // (frankenredis-4ovjf) The prefix tracks the caller's
            // frame kind, NOT current_invocation_name. Lua-frame
            // callers (a Lua function body or the chunk itself)
            // supply source:line; C-frame callers (RustFunction, e.g.
            // pcall directly invoking a non-callable) leave it empty.
            // Snapshotted into caller_is_lua_frame above before the
            // kind-stack push.
            LuaValue::Nil => {
                let body = "attempt to call a nil value";
                if caller_is_lua_frame {
                    Err(format!("user_script:1: {body}"))
                } else {
                    Err(body.to_string())
                }
            }
            LuaValue::Coroutine(_) => {
                let body = "attempt to call a thread value";
                if caller_is_lua_frame {
                    Err(format!("user_script:1: {body}"))
                } else {
                    Err(body.to_string())
                }
            }
            other => {
                let body = format!("attempt to call a {} value", other.type_name());
                if caller_is_lua_frame {
                    Err(format!("user_script:1: {body}"))
                } else {
                    Err(body)
                }
            }
        };
        self.call_depth -= 1;
        self.lua_frame_kinds.pop();
        result
    }

    fn call_builtin(
        &mut self,
        name: &str,
        args: &mut [LuaValue],
        env: &mut Env,
    ) -> Result<Vec<LuaValue>, String> {
        match name {
            "redis.call" => self.redis_call(args, false),
            "redis.pcall" => self.redis_call(args, true),
            "redis.error_reply" => {
                // Upstream script_lua.c::luaRedisErrorReplyCommand
                // requires exactly one string argument; non-string
                // (number/table/nil/bool) or no args replies via
                // luaPushError + `return 1`, NOT `luaError`. So the
                // wrong-type response is a return value, not a raise,
                // and carries no 'script: <sha>' suffix.
                // (br-frankenredis-replyargtype, frankenredis-20ggg)
                // 20ggg: upstream luaRedisReturnSingleFieldTable
                // checks lua_gettop(lua) != 1, so 2+ args must also
                // produce the wrong-args reply — fr previously took
                // `args.first()` and silently ignored the extras.
                let make_err_table = |bytes: Vec<u8>| {
                    let t = LuaTable::new();
                    t.set(LuaValue::Str(b"err".to_vec()), LuaValue::Str(bytes));
                    LuaValue::Table(t)
                };
                if args.len() != 1 {
                    return Ok(vec![make_err_table(
                        b"ERR wrong number or type of arguments".to_vec(),
                    )]);
                }
                let Some(LuaValue::Str(bytes)) = args.first() else {
                    return Ok(vec![make_err_table(
                        b"ERR wrong number or type of arguments".to_vec(),
                    )]);
                };
                // Upstream pre-pends '-' to the user-supplied error
                // body before handing it to luaPushErrorBuff, which
                // gives the body a chance to declare its own code.
                let with_dash: Vec<u8> = if bytes.first() == Some(&b'-') {
                    bytes.clone()
                } else {
                    let mut v = Vec::with_capacity(bytes.len() + 1);
                    v.push(b'-');
                    v.extend_from_slice(bytes);
                    v
                };
                let final_msg = lua_format_error_reply_payload(&with_dash);
                Ok(vec![make_err_table(final_msg)])
            }
            "redis.status_reply" => {
                // (br-frankenredis-replyargtype, frankenredis-20ggg)
                // Upstream luaRedisReturnSingleFieldTable requires
                // exactly one string arg. fr previously accepted any
                // length and silently used args[0].
                if args.len() != 1 {
                    let t = LuaTable::new();
                    t.set(
                        LuaValue::Str(b"err".to_vec()),
                        LuaValue::Str(b"ERR wrong number or type of arguments".to_vec()),
                    );
                    return Ok(vec![LuaValue::Table(t)]);
                }
                let Some(LuaValue::Str(bytes)) = args.first() else {
                    let t = LuaTable::new();
                    t.set(
                        LuaValue::Str(b"err".to_vec()),
                        LuaValue::Str(b"ERR wrong number or type of arguments".to_vec()),
                    );
                    return Ok(vec![LuaValue::Table(t)]);
                };
                Ok(vec![LuaValue::Table({
                    let t = LuaTable::new();
                    t.set(LuaValue::Str(b"ok".to_vec()), LuaValue::Str(bytes.clone()));
                    t
                })])
            }
            "redis.log" => {
                // Upstream script_lua.c::luaLogCommand requires
                // (level, msg [, msg2 …]) — at least two args, with
                // the first arg numeric AND in the LL_DEBUG..LL_WARNING
                // range (0..=3). (br-frankenredis-redislog,
                // frankenredis-20ggg)
                //
                // (frankenredis-r9f5y) Each error string carries the
                // "ERR " prefix because upstream's luaPushError stores
                // the prefix as part of the error table's err field —
                // pcall sees "ERR ..." verbatim, while the direct-call
                // wrapper recognises the prefix via
                // error_has_resp_code_prefix and does not double-add.
                if args.len() < 2 {
                    return Err("ERR redis.log() requires two arguments or more.".to_string());
                }
                let LuaValue::Number(level_f) = args[0] else {
                    return Err("ERR First argument must be a number (log level).".to_string());
                };
                // Match upstream: level cast to int via lua_tonumber,
                // then bounds-check against LL_DEBUG (0) and LL_WARNING
                // (3). NaN, negative, or > LL_WARNING all raise the
                // same "Invalid debug level." error.
                let level_i = level_f as i64;
                if !level_f.is_finite() || !(0..=3).contains(&level_i) {
                    return Err("ERR Invalid debug level.".to_string());
                }
                Ok(vec![LuaValue::Nil])
            }
            "redis.replicate_commands" => {
                // No-op: effects replication was removed in Redis 7.0+
                // Always returns true for compatibility
                Ok(vec![LuaValue::Bool(true)])
            }
            "redis.set_repl" => {
                // (frankenredis-op1r0) Upstream script_lua.c::
                // luaRedisSetReplCommand calls lua_tonumber + truncates
                // to int. lua_tonumber returns 0 for nil/bool/table/
                // non-numeric-string and truncates fractional floats.
                // The only gate is `flags & ~(PROPAGATE_AOF|REPL)` —
                // so set_repl('abc'), set_repl(nil), set_repl({}) all
                // resolve to flags=0 and are silently accepted.
                //
                // (frankenredis-lrrr4) Upstream's luaPushError stamps
                // "ERR " on both error bodies; pcall sees the prefixed
                // form and the direct-call wrapper recognises it via
                // error_has_resp_code_prefix.
                if args.len() != 1 {
                    return Err("ERR redis.set_repl() requires one argument.".to_string());
                }
                // to_number().unwrap_or(0.0) mirrors lua_tonumber's
                // "0 for non-coercible" semantics. The `as i64` cast
                // truncates fractional doubles toward zero, matching
                // C's `int flags = lua_tonumber(...)` implicit
                // conversion.
                let flags_f = args[0].to_number().unwrap_or(0.0);
                let flags_i = if flags_f.is_finite() {
                    flags_f as i64
                } else {
                    0
                };
                if flags_i & !(SCRIPT_PROPAGATE_AOF as i64 | SCRIPT_PROPAGATE_REPLICA as i64) != 0 {
                    return Err("ERR Invalid replication flags. Use REPL_AOF, REPL_REPLICA, REPL_ALL or REPL_NONE.".to_string());
                }
                self.store.script_propagation_mode = flags_i as u8;
                Ok(vec![LuaValue::Nil])
            }
            "redis.breakpoint" => {
                // Redis returns false when the Lua debugger is inactive.
                Ok(vec![LuaValue::Bool(false)])
            }
            "redis.setresp" => {
                // Upstream script_lua.c::luaSetRespCommand calls
                // lua_tonumber (0 for non-numeric, truncates floats),
                // then checks `resp != 2 && resp != 3`. So:
                //   setresp(2.5) → 2 → accepted
                //   setresp('abc'), setresp(nil), setresp({}) → 0 → rejected
                //   setresp(5) → rejected
                // (br-frankenredis-redislua, frankenredis-op1r0)
                //
                // (frankenredis-lrrr4) Upstream's luaPushError stamps
                // "ERR " on both error bodies.
                if args.len() != 1 {
                    return Err("ERR redis.setresp() requires one argument.".to_string());
                }
                let v_f = args[0].to_number().unwrap_or(0.0);
                let v = if v_f.is_finite() { v_f as i64 } else { 0 };
                if v != 2 && v != 3 {
                    return Err("ERR RESP version must be 2 or 3.".to_string());
                }
                // (frankenredis-vr8rg) Record the version so subsequent
                // redis.call/redis.pcall dispatch with it and materialize
                // replies via the matching RESP2/RESP3 Lua conversion.
                self.resp_version = v;
                Ok(vec![LuaValue::Nil])
            }
            "redis.acl_check_cmd" => {
                // Upstream script_lua.c::luaRedisAclCheckCmdCommand
                // requires at least one argument (the command name);
                // rejects unknown commands. (br-frankenredis-redislua)
                //
                // (frankenredis-vqjp9) Upstream's per-arg loop calls
                // lua_tolstring, which coerces both strings and numbers
                // to byte strings — so `redis.acl_check_cmd(123)` looks
                // up "123" as a command name (yielding the unknown-cmd
                // error), and `redis.acl_check_cmd(3.14)` looks up
                // "3.14". Only bool/nil/table arguments hit the
                // type-rejection branch. All three error strings carry
                // the "ERR " prefix because upstream's luaPushError
                // stores it as part of the error table's `err` field —
                // pcall sees "ERR ..." and the direct-call wrapper
                // recognises the prefix via error_has_resp_code_prefix
                // and does not double-add.
                if args.is_empty() {
                    return Err(
                        "ERR Please specify at least one argument for this redis lib call"
                            .to_string(),
                    );
                }
                let cmd_bytes: Vec<u8> = match &args[0] {
                    LuaValue::Str(b) => b.clone(),
                    LuaValue::Number(n) => {
                        if *n == (*n as i64) as f64 && n.is_finite() {
                            format!("{}", *n as i64).into_bytes()
                        } else {
                            lua_number_to_string(*n).into_bytes()
                        }
                    }
                    _ => {
                        return Err(
                            "ERR Lua redis lib command arguments must be strings or integers"
                                .to_string(),
                        );
                    }
                };
                if !crate::is_known_command(&cmd_bytes) {
                    return Err("ERR Invalid command passed to redis.acl_check_cmd()".to_string());
                }
                // Standalone mode without per-call ACL gating: assume
                // the command is allowed.
                Ok(vec![LuaValue::Bool(true)])
            }
            "redis.debug" => {
                // Redis emits debugger console output only when the Lua debugger is active.
                // Outside that mode the call is a no-op.
                Ok(vec![LuaValue::Nil])
            }
            "redis.sha1hex" => {
                // Upstream script_lua.c::luaRedisSha1hexCommand
                // requires exactly one argument and calls lua_tolstring,
                // which only converts strings and numbers — booleans,
                // nil, tables, functions yield NULL (treated as empty
                // bytes by sha1hex). fr previously used to_display_string
                // which renders bool/nil/table as "true"/"false"/"nil"/
                // table:0x...". (br-frankenredis-replyargtype,
                // frankenredis-20ggg)
                //
                // (frankenredis-f5rgn) Upstream also rejects argc != 1
                // with "wrong number of arguments" — both the
                // zero-args case (which fr already handled) and the
                // multi-args case must error. fr previously silently
                // ignored extras. The raw error string carries the
                // "ERR " prefix because upstream's luaPushError stores
                // it as part of the error table's `err` field — pcall
                // sees "ERR wrong number of arguments" and the direct-
                // call wrapper recognises the prefix via
                // error_has_resp_code_prefix and does not double-add.
                if args.len() != 1 {
                    return Err("ERR wrong number of arguments".to_string());
                }
                let Some(arg) = args.first() else {
                    return Err("ERR wrong number of arguments".to_string());
                };
                let data: Vec<u8> = match arg {
                    LuaValue::Str(s) => s.clone(),
                    LuaValue::Number(n) => {
                        if *n == (*n as i64) as f64 && n.is_finite() {
                            format!("{}", *n as i64).into_bytes()
                        } else {
                            lua_number_to_string(*n).into_bytes()
                        }
                    }
                    // lua_tolstring returns NULL for nil/bool/table/...
                    // → upstream hashes the empty byte string.
                    _ => Vec::new(),
                };
                let hex = fr_store::sha1_hex_public(&data);
                Ok(vec![LuaValue::Str(hex.into_bytes())])
            }
            "tonumber" => {
                // (frankenredis-3osi6) Upstream lbaselib.c:luaB_tonumber treats
                // the value as a luaL_checkany — explicit absence raises.
                // (frankenredis-i18ug) Funnel through format_builtin_argerror
                // so the prefix/name match indirect-pcall semantics.
                if args.is_empty() {
                    return Err(self.format_builtin_argerror("tonumber", 1, "value expected"));
                }
                let val = args.first().cloned().unwrap_or(LuaValue::Nil);
                // (frankenredis-5qv1n) Upstream: explicit nil base behaves
                // like no base (default 10/string parse); float base is
                // truncated via luaL_checkint; missing-arg-or-absent base
                // also defaults. Only non-nil/non-coercible values raise
                // "base out of range" — and that error must carry the
                // user_script:1: prefix from luaL_argerror.
                let base = match args.get(1) {
                    Some(LuaValue::Nil) | None => None,
                    Some(base_value) => {
                        let Some(base_f) = base_value.to_number() else {
                            return Err(self.format_builtin_argerror(
                                "tonumber",
                                2,
                                "base out of range",
                            ));
                        };
                        if !base_f.is_finite() {
                            return Err(self.format_builtin_argerror(
                                "tonumber",
                                2,
                                "base out of range",
                            ));
                        }
                        // luaL_checkint truncates floats — 10.5 -> 10,
                        // -1.9 -> -1. Match that here before bounds-checking.
                        let base = base_f as i64;
                        if !(2..=36).contains(&base) {
                            return Err(self.format_builtin_argerror(
                                "tonumber",
                                2,
                                "base out of range",
                            ));
                        }
                        Some(base as u32)
                    }
                };
                // (frankenredis-s99a4) Upstream luaB_tonumber: when base
                // is explicit, arg 1 goes through luaL_checkstring which
                // coerces numbers to their printed form, then strtoul
                // parses in the given base. So tonumber(5, 3) sees the
                // string "5" and fails to parse a base-3 digit ('5' >=
                // '3'), returning nil — not the unchanged number 5.
                if let Some(base) = base
                    && let LuaValue::Number(n) = &val
                {
                    let s = if *n == (*n as i64) as f64 && n.is_finite() {
                        format!("{}", *n as i64)
                    } else {
                        lua_number_to_string(*n)
                    };
                    let trimmed = s.trim();
                    let (sign, body) = match trimmed.as_bytes().first() {
                        Some(b'-') => (-1i64, &trimmed[1..]),
                        Some(b'+') => (1i64, &trimmed[1..]),
                        _ => (1i64, trimmed),
                    };
                    let stripped: &str = if base == 16 {
                        body.strip_prefix("0x")
                            .or_else(|| body.strip_prefix("0X"))
                            .unwrap_or(body)
                    } else {
                        body
                    };
                    return match u64::from_str_radix(stripped, base) {
                        Ok(n) => Ok(vec![LuaValue::Number(lua_tonumber_strtoul_result(
                            sign, n, base,
                        ))]),
                        Err(_) => Ok(vec![LuaValue::Nil]),
                    };
                }
                match &val {
                    LuaValue::Number(n) => Ok(vec![LuaValue::Number(*n)]),
                    LuaValue::Str(s) => {
                        let s_str = std::str::from_utf8(s).unwrap_or("");
                        let trimmed = s_str.trim();
                        if let Some(base) = base {
                            // (frankenredis-5qv1n) Upstream strtoul(s, _,
                            // base) accepts a leading "0x"/"0X" prefix
                            // when base is 16 (or 0). Rust's from_str_radix
                            // does not, so strip the prefix here for the
                            // base-16 path, including the optional sign.
                            let (sign, body) = match trimmed.as_bytes().first() {
                                Some(b'-') => (-1i64, &trimmed[1..]),
                                Some(b'+') => (1i64, &trimmed[1..]),
                                _ => (1i64, trimmed),
                            };
                            let stripped: &str = if base == 16 {
                                body.strip_prefix("0x")
                                    .or_else(|| body.strip_prefix("0X"))
                                    .unwrap_or(body)
                            } else {
                                body
                            };
                            match u64::from_str_radix(stripped, base) {
                                Ok(n) => Ok(vec![LuaValue::Number(lua_tonumber_strtoul_result(
                                    sign, n, base,
                                ))]),
                                Err(_) => Ok(vec![LuaValue::Nil]),
                            }
                        } else {
                            // (frankenredis-83zqp) Lua 5.1's lua_tonumber
                            // calls strtod, which accepts C99 hex floats
                            // including the `0x1.8p2` binary-exponent
                            // form. Try the hex helper FIRST so
                            // `tonumber("0x1.8p2")` returns 6.0; fall
                            // back to decimal parse, and finally to the
                            // strtoul(base=16) hex-int path for
                            // `tonumber("0xFF")` → 255 etc.
                            if let Some(val) = crate::try_parse_hex_float(trimmed)
                                && !val.is_nan()
                            {
                                return Ok(vec![LuaValue::Number(val)]);
                            }
                            match trimmed.parse::<f64>() {
                                Ok(n) => Ok(vec![LuaValue::Number(n)]),
                                Err(_) => {
                                    // (frankenredis-luatonumhex) Lua 5.1
                                    // lobject.c::luaO_str2d falls back to
                                    // strtoul(s, _, 16) when strtod fails
                                    // or stops at 'x'/'X'.
                                    Ok(vec![hex_str_to_lua_number(trimmed)])
                                }
                            }
                        }
                    }
                    _ => Ok(vec![LuaValue::Nil]),
                }
            }
            "tostring" => {
                // (frankenredis-2ddgn) Lua 5.1 lbaselib.c::luaB_tostring
                // checks for a __tostring metamethod first (via
                // luaL_callmeta) and returns its result if found —
                // without coercing to string. Vendored does not enforce
                // that the metamethod returns a string; tostring({})
                // with __tostring=function() return 99 end yields the
                // number 99. Non-callable handlers raise the standard
                // "attempt to call a TYPE value" error via call_function.
                // (frankenredis-6iqkt) luaB_tostring calls luaL_checkany
                // before any other logic, so a missing arg raises
                // "bad argument #1 to '?' (value expected)" (or the
                // call-site name when available).
                let val = match args.first().cloned() {
                    Some(v) => v,
                    None => {
                        return Err(self.format_builtin_argerror("tostring", 1, "value expected"));
                    }
                };
                let handler = match &val {
                    LuaValue::Table(t) => {
                        let inner = t.inner.borrow();
                        inner
                            .metatable
                            .as_ref()
                            .map(|mt| mt.get(&LuaValue::Str(b"__tostring".to_vec())))
                            .unwrap_or(LuaValue::Nil)
                    }
                    LuaValue::Userdata(LuaUserdata::Proxy(proxy)) => proxy
                        .metatable
                        .as_ref()
                        .map(|mt| mt.get(&LuaValue::Str(b"__tostring".to_vec())))
                        .unwrap_or(LuaValue::Nil),
                    _ => LuaValue::Nil,
                };
                if !matches!(handler, LuaValue::Nil) {
                    let mut meta_args = vec![val.clone()];
                    let results =
                        self.call_function(&handler, &mut meta_args, env, &mut Vec::new())?;
                    return Ok(vec![results.into_iter().next().unwrap_or(LuaValue::Nil)]);
                }
                Ok(vec![LuaValue::Str(val.to_display_string())])
            }
            "type" => {
                // (frankenredis-6iqkt) luaB_type calls luaL_checkany so
                // a missing arg errors with the standard "value
                // expected" wording. fr previously returned 'nil'.
                let val = match args.first().cloned() {
                    Some(v) => v,
                    None => {
                        return Err(self.format_builtin_argerror("type", 1, "value expected"));
                    }
                };
                Ok(vec![LuaValue::Str(val.type_name().as_bytes().to_vec())])
            }
            "error" => {
                // (frankenredis-cxmsu / frankenredis-0k259) Lua 5.1's
                // lbaselib.c::luaB_error tests `lua_isstring(L, 1)` (true
                // for both strings and numbers) before prepending the
                // luaL_where source-location for the requested level.
                // The location is added iff `lua_getstack(L, level, &ar)`
                // succeeds AND the frame at that level is a Lua function
                // with source info — C-builtin frames like pcall/xpcall
                // produce no location. fr tracks each in-flight call's
                // kind in `lua_frame_kinds` so we can answer that lookup
                // here. For non-string/non-number args the value is
                // re-raised via the typed-error sentinel so pcall/xpcall
                // can return the original type to the script.
                let mut raw = args.first().cloned().unwrap_or(LuaValue::Nil);
                let level = lua_optional_integer_arg(
                    self.current_invocation_name.as_deref(),
                    2,
                    args.get(1),
                    1,
                )?;
                // (frankenredis-vkqn0) Upstream Redis hooks lua_error so
                // that an error value of the form `{err = STRING, …}`
                // gets unwrapped to the bare `err` string at the
                // lua_error boundary — both pcall and the top-level
                // reply path see the string, not the table. Tables
                // without a string `err` field pass through unchanged.
                // The unwrap also skips the level-based where-prefix
                // because the table form already encodes the final
                // error message verbatim (live probe confirmed:
                // `error({err='x'})` produces just `x`, never
                // `user_script:1: x`). Implement this by replacing
                // `raw` with the inner string and forcing the
                // no-prefix branch via `level = 0`.
                let mut tagged_err_unwrap = false;
                let level = if let LuaValue::Table(ref t) = raw {
                    let err_field = t.get(&LuaValue::Str(b"err".to_vec()));
                    if let LuaValue::Str(s) = err_field {
                        raw = LuaValue::Str(s);
                        tagged_err_unwrap = true;
                        0_i64
                    } else {
                        level
                    }
                } else {
                    level
                };
                // luaB_error:
                //   if (lua_isstring(L, 1) && level > 0) {
                //       luaL_where(L, level); lua_pushvalue(L, 1);
                //       lua_concat(L, 2);
                //   }
                //   return lua_error(L);
                // luaL_where prints '<source>:<line>: ' iff lua_getstack
                // succeeds AND the frame is a Lua function with source
                // info; for C frames (pcall/xpcall/etc.) it returns the
                // empty string. So for string/number args with level>0
                // the result is always coerced to string; the where
                // prefix is only added when there is a Lua frame at the
                // requested level. For level<=0 the raw value passes
                // through with its original type.
                if level > 0 && matches!(&raw, LuaValue::Str(_) | LuaValue::Number(_)) {
                    // The top of lua_frame_kinds is error's own
                    // RustFunction frame; level N refers to N entries
                    // back from there.
                    let level_is_lua = self
                        .lua_frame_kinds
                        .len()
                        .checked_sub(1 + level as usize)
                        .and_then(|i| self.lua_frame_kinds.get(i).copied())
                        .unwrap_or(false);
                    let msg = String::from_utf8_lossy(&raw.to_display_string()).to_string();
                    if level_is_lua {
                        return Err(format!("user_script:1: {msg}"));
                    }
                    return Err(msg);
                }
                self.pending_error_value = Some(raw);
                self.pending_error_is_raw_body = tagged_err_unwrap;
                Err(LUA_TYPED_ERROR_SENTINEL.to_string())
            }
            "assert" => {
                // (frankenredis-nf29w) Lua 5.1 lbaselib.c::luaB_assert
                // calls luaL_checkany first, so a zero-arg invocation
                // raises "bad argument #1 to ? (value expected)" rather
                // than "assertion failed!".
                if args.is_empty() {
                    return Err(self.format_builtin_argerror("assert", 1, "value expected"));
                }
                let val = &args[0];
                if val.is_truthy() {
                    Ok(args.to_vec())
                } else {
                    // (frankenredis-l4k9y) Upstream luaL_error prepends
                    // the where(1) source-location, which is empty when
                    // assert was invoked indirectly via pcall (C frame)
                    // and "user_script:1: " when invoked from a Lua
                    // function or directly from the chunk top.
                    // (frankenredis-i18ug) current_invocation_name carries
                    // that distinction.
                    //
                    // (frankenredis-wmu03) Upstream uses
                    // `luaL_optstring(L, 2, "assertion failed!")` which
                    // treats nil OR absent as default, accepts
                    // string/number, and rejects other types with
                    // "bad argument #2 (string expected, got TYPE)".
                    // fr previously used to_display_string on whatever
                    // arg 2 happened to be, so explicit nil rendered
                    // "nil" and booleans/tables silently coerced.
                    let inv = self.current_invocation_name.as_deref();
                    let msg = match args.get(1) {
                        None | Some(LuaValue::Nil) => "assertion failed!".to_string(),
                        Some(LuaValue::Str(b)) => String::from_utf8_lossy(b).to_string(),
                        Some(LuaValue::Number(n)) => {
                            if *n == (*n as i64) as f64 && n.is_finite() {
                                format!("{}", *n as i64)
                            } else {
                                lua_number_to_string(*n)
                            }
                        }
                        Some(other) => {
                            return Err(lua_format_argerror(
                                inv,
                                "assert",
                                2,
                                &format!("string expected, got {}", other.type_name()),
                            ));
                        }
                    };
                    Err(match inv {
                        Some(_) => format!("user_script:1: {msg}"),
                        None => msg,
                    })
                }
            }
            "loadstring" => {
                // (frankenredis-cfflo) Lua 5.1 loadstring(s [, chunkname])
                // parses `s` as a chunk and returns the resulting function,
                // or (nil, errmsg) on parse failure. The chunk runs in the
                // same sandboxed environment as the calling script — it
                // inherits globals_locked, so the loaded function still
                // hits the same nonexistent-global / readonly-table
                // errors a top-level script would.
                let arg = args.first();
                let chunkname = args.get(1).and_then(|v| match v {
                    LuaValue::Str(s) => Some(s.clone()),
                    _ => None,
                });
                // (frankenredis-uyj7c) Upstream luaB_loadstring uses
                // luaL_checklstring(L,1,&l) which raises with the user_script:1
                // prefix and the standard "string expected, got <type|no value>"
                // wording for missing/wrong-type arg #1.
                let src_bytes: Vec<u8> = match arg {
                    Some(LuaValue::Str(s)) => s.clone(),
                    Some(LuaValue::Number(n)) => n.to_string().into_bytes(),
                    Some(other) => {
                        return Err(format!(
                            "user_script:1: bad argument #1 to 'loadstring' (string expected, got {})",
                            other.type_name()
                        ));
                    }
                    None => {
                        return Err(
                            "user_script:1: bad argument #1 to 'loadstring' (string expected, got no value)"
                                .to_string(),
                        );
                    }
                };
                let chunk_label = format_lua_chunk_label(chunkname.as_deref(), &src_bytes);
                // (frankenredis-5qhz7) Use the located compile path so a
                // multi-line chunk reports the true error line, not :1:.
                match parse_lua_chunk_located(&src_bytes) {
                    Ok(body) => Ok(vec![LuaValue::function(LuaFunc {
                        params: Vec::new(),
                        body,
                        is_variadic: true,
                        captured_env: Some(env.snapshot()),
                        env_table: Rc::new(RefCell::new(env.current_global_env())),
                        self_name: None,
                        // (frankenredis-ycaog) Tag the chunk so runtime
                        // errors raised from inside it are reported with
                        // the loadstring chunk-label prefix instead of
                        // the outer script's `user_script:1:` prefix.
                        source_label: Some(chunk_label),
                        identity: next_function_identity(),
                    })]),
                    Err((line, msg)) => Ok(vec![
                        LuaValue::Nil,
                        LuaValue::Str(format!("{chunk_label}:{line}: {msg}").into_bytes()),
                    ]),
                }
            }
            "load" => {
                // (frankenredis-cfflo) Lua 5.1 load(func [, chunkname])
                // accepts a function that returns chunks. fr's tree-
                // walking interpreter cannot stream source from a Lua
                // callback in a useful way (the script-tree-of-Stmt form
                // is already fully materialised), so we only implement
                // the type-checked rejection upstream emits for non-
                // function arguments — which matches what real Redis 7.2
                // scripts do when they call `load('some src')` by
                // accident.
                //
                // (frankenredis-4zmde) Upstream lbaselib.c registers
                // `load` via luaL_register on the base_funcs table, so
                // lua_getinfo reports its debug name as `load` when
                // called directly. When called via pcall/xpcall the
                // call site is lost and the name reports as `'?'`. The
                // direct-call form also carries the `user_script:1: `
                // source-location prefix; the pcall form does not.
                // Live probe vs vendored 7.2.4 confirmed both shapes —
                // fr's prior wording was the pcall form even on direct
                // calls. lua_format_argerror handles the two cases via
                // current_invocation_name.
                let raw = args.first().cloned().unwrap_or(LuaValue::Nil);
                match &raw {
                    LuaValue::Function(_)
                    | LuaValue::RustFunction(_)
                    | LuaValue::WrappedCoroutine(_) => {
                        // (frankenredis-36wn7) Lua 5.1 load(func [, chunkname])
                        // calls the reader function repeatedly; each call
                        // returns a string piece and a nil/empty-string return
                        // terminates. fr's tree-walking interpreter cannot
                        // STREAM source, but it can EAGERLY collect every piece,
                        // concatenate, and compile via the same path as
                        // loadstring — the identical result Lua's load(func)
                        // yields. Matches redis 7.2.4 (which supports load(func));
                        // the prior fr build rejected it outright.
                        let func = raw.clone();
                        let chunkname = args.get(1).and_then(|v| match v {
                            LuaValue::Str(s) => Some(s.clone()),
                            _ => None,
                        });
                        // Defensive cap so a generator that never terminates
                        // can't build an unbounded chunk (the per-script timeout
                        // is the ultimate backstop).
                        const MAX_LOAD_SOURCE_BYTES: usize = 64 * 1024 * 1024;
                        let mut src_bytes: Vec<u8> = Vec::new();
                        loop {
                            let mut call_args: Vec<LuaValue> = Vec::new();
                            let piece =
                                self.call_function(&func, &mut call_args, env, &mut Vec::new())?;
                            match piece.into_iter().next() {
                                None | Some(LuaValue::Nil) => break,
                                Some(LuaValue::Str(s)) => {
                                    if s.is_empty() {
                                        break;
                                    }
                                    src_bytes.extend_from_slice(&s);
                                }
                                // Lua's generic_reader accepts a number (lua_isstring
                                // is true for numbers); coerce like loadstring does.
                                Some(LuaValue::Number(n)) => {
                                    src_bytes.extend_from_slice(n.to_string().as_bytes());
                                }
                                Some(_other) => {
                                    return Ok(vec![
                                        LuaValue::Nil,
                                        LuaValue::Str(
                                            b"reader function must return a string".to_vec(),
                                        ),
                                    ]);
                                }
                            }
                            if src_bytes.len() > MAX_LOAD_SOURCE_BYTES {
                                return Ok(vec![
                                    LuaValue::Nil,
                                    LuaValue::Str(b"too long".to_vec()),
                                ]);
                            }
                        }
                        // (frankenredis-5qhz7) load(func) with no chunkname uses
                        // the literal `(load)` label (vendored's `=(load)`), not a
                        // source-derived `[string "..."]` label; and the located
                        // compile path reports the true error line, not :1:.
                        let chunk_label = match chunkname.as_deref() {
                            Some(n) => format_lua_chunk_label(Some(n), &src_bytes),
                            None => "(load)".to_string(),
                        };
                        match parse_lua_chunk_located(&src_bytes) {
                            Ok(body) => Ok(vec![LuaValue::function(LuaFunc {
                                params: Vec::new(),
                                body,
                                is_variadic: true,
                                captured_env: Some(env.snapshot()),
                                env_table: Rc::new(RefCell::new(env.current_global_env())),
                                self_name: None,
                                source_label: Some(chunk_label),
                                identity: next_function_identity(),
                            })]),
                            Err((line, msg)) => Ok(vec![
                                LuaValue::Nil,
                                LuaValue::Str(format!("{chunk_label}:{line}: {msg}").into_bytes()),
                            ]),
                        }
                    }
                    _ => Err(lua_format_argerror(
                        self.current_invocation_name.as_deref(),
                        "load",
                        1,
                        &format!("function expected, got {}", raw.type_name()),
                    )),
                }
            }
            "pcall" => {
                let func = args.first().cloned().unwrap_or(LuaValue::Nil);
                let mut call_args_vec = args.get(1..).unwrap_or(&[]).to_vec();
                // (frankenredis-557p3) The protected callback runs WITHOUT
                // an AST call site — Lua 5.1's lua_getinfo returns no
                // "n.name" for C closures invoked via lua_pcall, so any
                // C-builtin errors raised inside use '?' as the name.
                let prev_inv = self.current_invocation_name.take();
                let prev_method = std::mem::replace(&mut self.current_invocation_is_method, false);
                let result = self.call_function(&func, &mut call_args_vec, env, &mut Vec::new());
                self.current_invocation_name = prev_inv;
                self.current_invocation_is_method = prev_method;
                match result {
                    Ok(vals) => {
                        // Drop any stale typed-error stash; the protected
                        // call completed normally.
                        self.pending_error_value = None;
                        self.pending_error_is_raw_body = false;
                        let mut new_vals = Vec::with_capacity(vals.len() + 1);
                        new_vals.push(LuaValue::Bool(true));
                        new_vals.extend(vals);
                        Ok(new_vals)
                    }
                    // (frankenredis-sjuu1) Re-raise only a REAL yield —
                    // pending_yield must be set by an actual call to
                    // coroutine.yield. A user script that forges the
                    // sentinel via error('__...') has pending_yield ==
                    // None and falls through to the regular Err arm.
                    Err(msg) if is_lua_yield_signal(&msg) && self.pending_yield.is_some() => {
                        Err(msg)
                    }
                    Err(msg) => {
                        // (frankenredis-cxmsu) If the protected call
                        // raised a typed error via `error({...})` /
                        // `error(true)` / `error(nil)`, the sentinel
                        // string carries no value — splice the original
                        // LuaValue back from `pending_error_value` so
                        // `pcall` returns (false, table|bool|nil|...).
                        let err_val = if msg == LUA_TYPED_ERROR_SENTINEL
                            && self.pending_error_value.is_some()
                        {
                            self.pending_error_value.take().unwrap()
                        } else {
                            self.pending_error_value = None;
                            LuaValue::Str(msg.into_bytes())
                        };
                        // The raw-body marker only matters for uncaught
                        // top-level errors — pcall has already converted
                        // the sentinel back to a typed value.
                        self.pending_error_is_raw_body = false;
                        Ok(vec![LuaValue::Bool(false), err_val])
                    }
                }
            }
            "xpcall" => {
                // xpcall(f, msgh, ...) — like pcall but with error handler
                // (frankenredis-nf29w) Lua 5.1 lbaselib.c::luaB_xpcall
                // calls luaL_checkany on the msgh slot before doing
                // anything else, so a missing msgh raises
                // "bad argument #2 to ? (value expected)" — fr
                // previously silently substituted nil.
                if args.len() < 2 {
                    return Err(self.format_builtin_argerror("xpcall", 2, "value expected"));
                }
                let func = args.first().cloned().unwrap_or(LuaValue::Nil);
                let err_handler = args.get(1).cloned().unwrap_or(LuaValue::Nil);
                let mut call_args_vec = args.get(2..).unwrap_or(&[]).to_vec();
                // (frankenredis-557p3) Same AST-context clear as pcall.
                let prev_inv = self.current_invocation_name.take();
                let prev_method = std::mem::replace(&mut self.current_invocation_is_method, false);
                let result = self.call_function(&func, &mut call_args_vec, env, &mut Vec::new());
                self.current_invocation_name = prev_inv;
                self.current_invocation_is_method = prev_method;
                match result {
                    Ok(vals) => {
                        self.pending_error_value = None;
                        self.pending_error_is_raw_body = false;
                        let mut new_vals = Vec::with_capacity(vals.len() + 1);
                        new_vals.push(LuaValue::Bool(true));
                        new_vals.extend(vals);
                        Ok(new_vals)
                    }
                    // (frankenredis-sjuu1) Same gating as pcall — only
                    // re-raise a real yield signaled by coroutine.yield.
                    Err(msg) if is_lua_yield_signal(&msg) && self.pending_yield.is_some() => {
                        Err(msg)
                    }
                    Err(msg) => {
                        // (frankenredis-cxmsu) Reconstruct the typed
                        // error value the same way pcall does, then hand
                        // the original LuaValue to the user-supplied
                        // message handler.
                        let err_val = if msg == LUA_TYPED_ERROR_SENTINEL
                            && self.pending_error_value.is_some()
                        {
                            self.pending_error_value.take().unwrap()
                        } else {
                            self.pending_error_value = None;
                            LuaValue::Str(msg.into_bytes())
                        };
                        self.pending_error_is_raw_body = false;
                        let mut handler_args = vec![err_val.clone()];
                        match self.call_function(
                            &err_handler,
                            &mut handler_args,
                            env,
                            &mut Vec::new(),
                        ) {
                            Ok(handler_results) => {
                                let transformed =
                                    handler_results.into_iter().next().unwrap_or(err_val);
                                Ok(vec![LuaValue::Bool(false), transformed])
                            }
                            Err(_) => {
                                // (frankenredis-l4k9y) Upstream's
                                // lua_pcall returns LUA_ERRERR when the
                                // message handler itself errors, and
                                // pushes the canonical "error in error
                                // handling" string onto the stack. fr
                                // previously returned the original error
                                // value, swallowing the handler failure.
                                self.pending_error_value = None;
                                Ok(vec![
                                    LuaValue::Bool(false),
                                    LuaValue::Str(b"error in error handling".to_vec()),
                                ])
                            }
                        }
                    }
                }
            }
            "pairs" => {
                let table = args.first().cloned().unwrap_or(LuaValue::Nil);
                if !matches!(table, LuaValue::Table(_)) {
                    return Err(lua_bad_table_arg(
                        self.current_invocation_name.as_deref(),
                        1,
                        args.first(),
                    ));
                }
                // Return next, table, nil
                Ok(vec![
                    LuaValue::RustFunction("next".to_string()),
                    table,
                    LuaValue::Nil,
                ])
            }
            "ipairs" => {
                let table = args.first().cloned().unwrap_or(LuaValue::Nil);
                if !matches!(table, LuaValue::Table(_)) {
                    return Err(lua_bad_table_arg(
                        self.current_invocation_name.as_deref(),
                        1,
                        args.first(),
                    ));
                }
                Ok(vec![
                    LuaValue::RustFunction("__ipairs_iter".to_string()),
                    table,
                    LuaValue::Number(0.0),
                ])
            }
            "__ipairs_iter" => {
                let table = args.first().cloned().unwrap_or(LuaValue::Nil);
                let idx = args.get(1).and_then(|v| v.to_number()).unwrap_or(0.0) as usize + 1;
                if let LuaValue::Table(t) = &table {
                    if idx <= t.inner.borrow().array.len() {
                        let v = t.inner.borrow().array[idx - 1].clone();
                        // (frankenredis-y0ri2) ipairs stops at the first nil
                        // value per Lua 5.1 ref §5.4. Now that table
                        // constructors preserve nil slots in the array
                        // (`{1, nil, 3}` -> array=[1, Nil, 3]), the iterator
                        // must not return the nil — that would erroneously
                        // step past the hole and emit (3, 3).
                        if matches!(v, LuaValue::Nil) {
                            Ok(vec![LuaValue::Nil])
                        } else {
                            Ok(vec![LuaValue::Number(idx as f64), v])
                        }
                    } else {
                        Ok(vec![LuaValue::Nil])
                    }
                } else {
                    Ok(vec![LuaValue::Nil])
                }
            }
            "__gmatch_iter" => {
                // Iterator for string.gmatch: state table has __gmatch_data and __gmatch_idx
                let state = args.first().cloned().unwrap_or(LuaValue::Nil);
                if let LuaValue::Table(ref t) = state {
                    let idx_key = LuaValue::Str(b"__gmatch_idx".to_vec());
                    let data_key = LuaValue::Str(b"__gmatch_data".to_vec());
                    let idx = match t.get(&idx_key) {
                        LuaValue::Number(n) => n as usize + 1,
                        _ => 1,
                    };
                    if let LuaValue::Table(data) = t.get(&data_key) {
                        let row_key = LuaValue::Number(idx as f64);
                        if let LuaValue::Table(row) = data.get(&row_key) {
                            // Update index in state - we need to mutate args[0]
                            if let LuaValue::Table(ref mut st) = args[0] {
                                st.set(idx_key, LuaValue::Number(idx as f64));
                            }
                            // Return the captures from this row
                            let mut results = Vec::new();
                            let mut i = 1;
                            loop {
                                let v = row.get(&LuaValue::Number(i as f64));
                                if matches!(v, LuaValue::Nil) {
                                    break;
                                }
                                results.push(v);
                                i += 1;
                            }
                            if results.is_empty() {
                                return Ok(vec![LuaValue::Nil]);
                            }
                            return Ok(results);
                        }
                    }
                }
                Ok(vec![LuaValue::Nil])
            }
            "next" => {
                let table = args.first().cloned().unwrap_or(LuaValue::Nil);
                let key = args.get(1).cloned().unwrap_or(LuaValue::Nil);
                let LuaValue::Table(t) = &table else {
                    return Err(lua_bad_table_arg(
                        self.current_invocation_name.as_deref(),
                        1,
                        args.first(),
                    ));
                };

                // (frankenredis-y0ri2) Find the next non-nil array slot
                // at or after `from` (0-based). Lua 5.1 next() skips nil
                // values — the table constructor now preserves nil
                // positional slots in the array part, so a naive
                // contiguous step would yield (2, nil) and `pairs` would
                // emit a spurious nil-valued pair.
                let next_array_slot = |from: usize| -> Option<usize> {
                    let inner = t.inner.borrow();
                    (from..inner.array.len()).find(|&i| !matches!(inner.array[i], LuaValue::Nil))
                };

                // Find next key after the given key.
                if matches!(key, LuaValue::Nil) {
                    if let Some(i) = next_array_slot(0) {
                        return Ok(vec![
                            LuaValue::Number((i + 1) as f64),
                            t.inner.borrow().array[i].clone(),
                        ]);
                    }
                    let hash_pairs = t.hash_pairs();
                    if let Some((k, v)) = hash_pairs.first() {
                        return Ok(vec![k.clone(), v.clone()]);
                    }
                    return Ok(vec![LuaValue::Nil]);
                }

                if let LuaValue::Number(n) = &key {
                    let idx = *n as usize;
                    if idx >= 1 && idx <= t.inner.borrow().array.len() && *n == idx as f64 {
                        if let Some(i) = next_array_slot(idx) {
                            return Ok(vec![
                                LuaValue::Number((i + 1) as f64),
                                t.inner.borrow().array[i].clone(),
                            ]);
                        }
                        let hash_pairs = t.hash_pairs();
                        if let Some((k, v)) = hash_pairs.first() {
                            return Ok(vec![k.clone(), v.clone()]);
                        }
                        return Ok(vec![LuaValue::Nil]);
                    }
                }

                let hash_pairs = t.hash_pairs();
                let mut found = false;
                for (i, (k, _v)) in hash_pairs.iter().enumerate() {
                    if found {
                        return Ok(vec![hash_pairs[i].0.clone(), hash_pairs[i].1.clone()]);
                    }
                    if lua_raw_equal(k, &key) {
                        found = true;
                    }
                }

                if found {
                    Ok(vec![LuaValue::Nil])
                } else {
                    Err("invalid key to 'next'".to_string())
                }
            }
            "unpack" => {
                let inv = self.current_invocation_name.as_deref();
                let t = lua_table_arg(inv, 1, args.first())?;
                let start = lua_optional_integer_arg(inv, 2, args.get(1), 1)?;
                let end = lua_optional_integer_arg(
                    inv,
                    3,
                    args.get(2),
                    t.inner.borrow().array.len() as i64,
                )?;
                if start <= end && end.saturating_sub(start) >= 8000 {
                    return Err("too many results to unpack".to_string());
                }
                let mut results = Vec::new();
                for i in start..=end {
                    results.push(t.get(&LuaValue::Number(i as f64)));
                }
                Ok(results)
            }
            "select" => {
                // (frankenredis-w3wkp) Tighten select's bad-argument
                // handling to match vendored Lua 5.1. Non-numeric args
                // now report "number expected, got TYPE" with the #1
                // index prefix; NaN/±inf coerce via C-style cast so NaN
                // and -inf trip the "index out of range" check while
                // +inf saturates to the past-the-end empty case; the
                // old "bad argument to 'select'" wording (no #N, no
                // type detail) is gone. Note: Lua 5.1 emits the C
                // closure's debug name ('select' here), or '?' when
                // invoked via pcall — fr always emits 'select' since
                // call-site name plumbing is filed separately (see
                // frankenredis-md71j-style follow-up).
                let idx_opt = args.first();
                let rest = args.get(1..).unwrap_or(&[]);
                let idx = idx_opt.cloned().unwrap_or(LuaValue::Nil);
                match &idx {
                    LuaValue::Str(s) if s == b"#" => Ok(vec![LuaValue::Number(rest.len() as f64)]),
                    _ => {
                        let raw_index = idx.to_number().ok_or_else(|| {
                            // (frankenredis-i18ug follow-up) Use the
                            // got-label helper so missing args report
                            // "got no value" rather than "got nil".
                            self.format_builtin_argerror(
                                "select",
                                1,
                                &format!("number expected, got {}", lua_arg_got_label(idx_opt),),
                            )
                        })?;
                        let arg_count = rest.len() as i64;
                        // NaN → 0 via C-style cast; trips "index out of
                        // range" alongside zero and negative-out-of-range.
                        let index = if raw_index.is_nan() {
                            0i64
                        } else if raw_index.is_infinite() {
                            // ±inf: positive saturates to arg_count+1 so
                            // start > arg_count yields empty; negative
                            // trips "index out of range".
                            if raw_index > 0.0 {
                                arg_count + 1
                            } else {
                                i64::MIN
                            }
                        } else {
                            raw_index as i64
                        };
                        if index == 0 || index < -arg_count {
                            return Err(self.format_builtin_argerror(
                                "select",
                                1,
                                "index out of range",
                            ));
                        }

                        let start = if index > 0 {
                            index
                        } else {
                            arg_count + index + 1
                        };
                        if start > arg_count {
                            Ok(Vec::new())
                        } else {
                            Ok(rest[(start - 1) as usize..].to_vec())
                        }
                    }
                }
            }
            "rawget" => {
                // (frankenredis-nf29w) Lua 5.1 luaB_rawget calls
                // luaL_checktype(L, 1, LUA_TTABLE) (which uses
                // "got no value" for missing args) and luaL_checkany on
                // the key slot.
                let table_opt = args.first();
                let key_opt = args.get(1);
                let table = match table_opt {
                    Some(LuaValue::Table(t)) => t.clone(),
                    other => {
                        return Err(self.format_builtin_argerror(
                            "rawget",
                            1,
                            &format!("table expected, got {}", lua_arg_got_label(other)),
                        ));
                    }
                };
                if key_opt.is_none() {
                    return Err(self.format_builtin_argerror("rawget", 2, "value expected"));
                }
                let key = key_opt.cloned().unwrap();
                Ok(vec![table.get(&key)])
            }
            "rawset" => {
                // (frankenredis-uyj7c) Upstream luaB_rawset uses
                // luaL_checktype(L,1,LUA_TTABLE) then luaL_checkany(L,2)
                // then luaL_checkany(L,3) — all three are required, with
                // 'value expected' wording for the trailing args.
                // (frankenredis-i18ug) Funnel through the inv_name-aware
                // helpers so `pcall(rawset, nil)` reports '?' / no prefix.
                if !matches!(args.first(), Some(LuaValue::Table(_))) {
                    return Err(lua_bad_table_arg(
                        self.current_invocation_name.as_deref(),
                        1,
                        args.first(),
                    ));
                }
                if args.get(1).is_none() {
                    return Err(self.format_builtin_argerror("rawset", 2, "value expected"));
                }
                if args.get(2).is_none() {
                    return Err(self.format_builtin_argerror("rawset", 3, "value expected"));
                }
                // (frankenredis-8mwy9) rawset bypasses __newindex but NOT the
                // table protection — redis raises the readonly error (no
                // user_script prefix) before the nil/NaN key checks below.
                if matches!(&args[0], LuaValue::Table(t) if t.is_readonly()) {
                    return Err("Attempt to modify a readonly table".to_string());
                }
                // (frankenredis-uyj7c) Upstream lua_rawset routes a nil key
                // through luaH_set which raises 'table index is nil' (no
                // user_script:1 prefix — this comes from the VM core rather
                // than the library wrapper).
                if matches!(&args[1], LuaValue::Nil) {
                    return Err("table index is nil".to_string());
                }
                if matches!(&args[1], LuaValue::Number(n) if n.is_nan()) {
                    return Err("table index is NaN".to_string());
                }
                let key = args[1].clone();
                let val = args[2].clone();
                if let LuaValue::Table(ref mut t) = args[0] {
                    t.set(key, val);
                }
                Ok(vec![args[0].clone()])
            }
            "setmetatable" => {
                // (frankenredis-nf29w) Lua 5.1 luaB_setmetatable uses
                // luaL_checktype which reports "got no value" for a
                // missing arg.
                let table = match args.first() {
                    Some(LuaValue::Table(t)) => LuaValue::Table(t.clone()),
                    other => {
                        return Err(self.format_builtin_argerror(
                            "setmetatable",
                            1,
                            &format!("table expected, got {}", lua_arg_got_label(other)),
                        ));
                    }
                };
                let LuaValue::Table(t) = &table else {
                    unreachable!()
                };
                // (frankenredis-fnh42) Upstream luaL_argcheck rejects a
                // missing #2 arg (LUA_TNONE) as "nil or table expected"
                // — only an explicit nil or a table passes. fr was
                // converting missing → nil and silently clearing the
                // metatable.
                let Some(mt_arg) = args.get(1).cloned() else {
                    return Err(self.format_builtin_argerror(
                        "setmetatable",
                        2,
                        "nil or table expected",
                    ));
                };
                if !matches!(mt_arg, LuaValue::Nil | LuaValue::Table(_)) {
                    return Err(self.format_builtin_argerror(
                        "setmetatable",
                        2,
                        "nil or table expected",
                    ));
                }
                // (frankenredis-8mwy9) A protected library table rejects
                // setmetatable with the bare readonly error (no source prefix),
                // matching redis.
                if t.is_readonly() {
                    return Err("Attempt to modify a readonly table".to_string());
                }
                // (frankenredis-fnh42) Upstream luaL_getmetafield checks
                // the existing metatable for __metatable; if present,
                // raise — the protection blocks reassignment regardless
                // of what __metatable resolves to. luaL_error pre-pends
                // the where(1) source-location, which is empty for C
                // callers (pcall(setmetatable, t, mt)) and
                // 'user_script:1: ' when called from a Lua function;
                // current_invocation_name encodes that distinction the
                // same way format_builtin_argerror does.
                {
                    let inner = t.inner.borrow();
                    if let Some(existing_mt) = &inner.metatable {
                        let protected = existing_mt
                            .inner
                            .borrow()
                            .get(&LuaValue::Str(b"__metatable".to_vec()));
                        if !matches!(protected, LuaValue::Nil) {
                            let msg = "cannot change a protected metatable";
                            return Err(match &self.current_invocation_name {
                                Some(_) => format!("user_script:1: {msg}"),
                                None => msg.to_string(),
                            });
                        }
                    }
                }
                match mt_arg {
                    LuaValue::Table(mt) => {
                        t.inner.borrow_mut().metatable = Some(mt);
                    }
                    LuaValue::Nil => {
                        t.inner.borrow_mut().metatable = None;
                    }
                    _ => unreachable!(),
                }
                Ok(vec![table])
            }
            "newproxy" => {
                let metatable = match args.first() {
                    None | Some(LuaValue::Nil) | Some(LuaValue::Bool(false)) => None,
                    Some(LuaValue::Bool(true)) => Some(LuaTable::new()),
                    Some(LuaValue::Userdata(LuaUserdata::Proxy(proxy))) => {
                        let Some(mt) = &proxy.metatable else {
                            return Err(self.format_builtin_argerror(
                                "newproxy",
                                1,
                                "boolean or proxy expected",
                            ));
                        };
                        Some(mt.clone())
                    }
                    _ => {
                        return Err(self.format_builtin_argerror(
                            "newproxy",
                            1,
                            "boolean or proxy expected",
                        ));
                    }
                };
                Ok(vec![LuaValue::Userdata(LuaUserdata::Proxy(LuaProxy::new(
                    metatable,
                )))])
            }
            "getmetatable" => {
                // (frankenredis-nf29w) Lua 5.1 luaB_getmetatable calls
                // luaL_checkany so a zero-arg call raises
                // "bad argument #1 to ? (value expected)".
                if args.is_empty() {
                    return Err(self.format_builtin_argerror("getmetatable", 1, "value expected"));
                }
                match &args[0] {
                    LuaValue::Table(t) => {
                        let inner = t.inner.borrow();
                        match &inner.metatable {
                            // (frankenredis-fnh42) luaL_getmetafield
                            // returns the __metatable field if present
                            // — shielding the real metatable from the
                            // caller. Only fall back to the metatable
                            // itself when __metatable is absent.
                            Some(mt) => {
                                let masked = mt
                                    .inner
                                    .borrow()
                                    .get(&LuaValue::Str(b"__metatable".to_vec()));
                                if matches!(masked, LuaValue::Nil) {
                                    Ok(vec![LuaValue::Table(mt.clone())])
                                } else {
                                    Ok(vec![masked])
                                }
                            }
                            None => Ok(vec![LuaValue::Nil]),
                        }
                    }
                    LuaValue::Userdata(LuaUserdata::Proxy(proxy)) => match &proxy.metatable {
                        Some(mt) => {
                            let masked = mt
                                .inner
                                .borrow()
                                .get(&LuaValue::Str(b"__metatable".to_vec()));
                            if matches!(masked, LuaValue::Nil) {
                                Ok(vec![LuaValue::Table(mt.clone())])
                            } else {
                                Ok(vec![masked])
                            }
                        }
                        None => Ok(vec![LuaValue::Nil]),
                    },
                    _ => Ok(vec![LuaValue::Nil]),
                }
            }
            "getfenv" => {
                // (frankenredis-cp1gs) Vendored Redis exposes Lua 5.1's
                // base getfenv. Numeric levels beyond the current script
                // frame are rejected; C/Rust builtins report the default
                // protected globals table.
                let table = match args.first() {
                    None | Some(LuaValue::Nil) => self.effective_global_env_table(env),
                    Some(LuaValue::Function(func)) => func
                        .env_table
                        .borrow()
                        .clone()
                        .unwrap_or_else(|| self.default_global_env_table()),
                    Some(LuaValue::RustFunction(_)) => self.default_global_env_table(),
                    Some(value) => {
                        let Some(number) = value.to_number() else {
                            return Err(lua_bad_number_arg(
                                self.current_invocation_name.as_deref(),
                                1,
                                Some(value),
                            ));
                        };
                        let level = number as i64;
                        if level < 0 {
                            return Err(lua_format_argerror(
                                self.current_invocation_name.as_deref(),
                                "getfenv",
                                1,
                                "level must be non-negative",
                            ));
                        }
                        if level > 1 {
                            return Err(lua_format_argerror(
                                self.current_invocation_name.as_deref(),
                                "getfenv",
                                1,
                                "invalid level",
                            ));
                        }
                        self.effective_global_env_table(env)
                    }
                };
                Ok(vec![LuaValue::Table(table)])
            }
            "setfenv" => {
                // Keep the same check order as lbaselib.c: argument #2
                // must be a table before the target function/level is
                // resolved.
                let inv = self.current_invocation_name.as_deref();
                let new_env = lua_table_arg(inv, 2, args.get(1))?.clone();
                let cannot_change = || match inv {
                    Some(_) => "user_script:1: 'setfenv' cannot change environment of given object"
                        .to_string(),
                    None => "'setfenv' cannot change environment of given object".to_string(),
                };
                match args.first() {
                    Some(LuaValue::Function(func)) => {
                        *func.env_table.borrow_mut() = Some(new_env);
                        Ok(vec![args[0].clone()])
                    }
                    Some(LuaValue::RustFunction(_)) => Err(cannot_change()),
                    Some(value) => {
                        let Some(number) = value.to_number() else {
                            return Err(lua_bad_number_arg(inv, 1, Some(value)));
                        };
                        let level = number as i64;
                        if level < 0 {
                            return Err(lua_format_argerror(
                                inv,
                                "setfenv",
                                1,
                                "level must be non-negative",
                            ));
                        }
                        if level > 1 {
                            return Err(lua_format_argerror(inv, "setfenv", 1, "invalid level"));
                        }
                        env.set_global_env(new_env);
                        if level == 0 {
                            Ok(Vec::new())
                        } else {
                            Ok(vec![LuaValue::RustFunction(
                                "__fr_current_chunk_env".to_string(),
                            )])
                        }
                    }
                    None => Err(lua_bad_number_arg(inv, 1, None)),
                }
            }
            "rawlen" => {
                let val = args.first().cloned().unwrap_or(LuaValue::Nil);
                match &val {
                    LuaValue::Table(t) => Ok(vec![LuaValue::Number(t.len() as f64)]),
                    LuaValue::Str(s) => Ok(vec![LuaValue::Number(s.len() as f64)]),
                    _ => Ok(vec![LuaValue::Number(0.0)]),
                }
            }
            "print" => {
                // Silently consume (Redis disables print)
                Ok(vec![LuaValue::Nil])
            }
            // ── Math library ────────────────────────────────────────────
            "math.floor" => {
                let n =
                    lua_check_number(self.current_invocation_name.as_deref(), args, 0, "floor")?;
                Ok(vec![LuaValue::Number(n.floor())])
            }
            "math.ceil" => {
                let n = lua_check_number(self.current_invocation_name.as_deref(), args, 0, "ceil")?;
                Ok(vec![LuaValue::Number(n.ceil())])
            }
            "math.abs" => {
                let n = lua_check_number(self.current_invocation_name.as_deref(), args, 0, "abs")?;
                Ok(vec![LuaValue::Number(n.abs())])
            }
            "math.max" => {
                // (frankenredis-n4eln) Lua 5.1's lmathlib.c::math_max
                // uses luaL_checknumber on every arg starting at arg 1,
                // so calling with zero args raises 'bad argument #1 to
                // max (number expected, got no value)'.
                // (frankenredis-a6r5p) Wrong-type args at index > 0
                // route through the same luaL_argerror.
                // (frankenredis-ymb2q) Route the empty-args case through
                // lua_format_argerror so `pcall(math.max)` emits the
                // anonymous-C-function shape (`'?'` name, no prefix).
                if args.is_empty() {
                    return Err(lua_format_argerror(
                        self.current_invocation_name.as_deref(),
                        "max",
                        1,
                        "number expected, got no value",
                    ));
                }
                let mut max = f64::NEG_INFINITY;
                for (i, _) in args.iter().enumerate() {
                    let n =
                        lua_check_number(self.current_invocation_name.as_deref(), args, i, "max")?;
                    if n > max {
                        max = n;
                    }
                }
                Ok(vec![LuaValue::Number(max)])
            }
            "math.min" => {
                // (frankenredis-a6r5p) Same template as math.max — see
                // comment above for the upstream wording rationale.
                // (frankenredis-ymb2q) Empty-args branch via
                // lua_format_argerror for the dual direct/pcall shape.
                if args.is_empty() {
                    return Err(lua_format_argerror(
                        self.current_invocation_name.as_deref(),
                        "min",
                        1,
                        "number expected, got no value",
                    ));
                }
                let mut min = f64::INFINITY;
                for (i, _) in args.iter().enumerate() {
                    let n =
                        lua_check_number(self.current_invocation_name.as_deref(), args, i, "min")?;
                    if n < min {
                        min = n;
                    }
                }
                Ok(vec![LuaValue::Number(min)])
            }
            "math.sqrt" => {
                let n = lua_check_number(self.current_invocation_name.as_deref(), args, 0, "sqrt")?;
                Ok(vec![LuaValue::Number(n.sqrt())])
            }
            "math.random" => {
                // (frankenredis-nwmly) Upstream lmathlib.c::math_random
                // dispatches on lua_gettop(L). 0 args -> float [0,1);
                // 1 arg -> int [1,u] with luaL_argcheck(_, 1<=u, 1, ...);
                // 2 args -> int [l,u] with luaL_argcheck(_, l<=u, 2, ...)
                // — note arg #2 is reported as the bad arg when l>u;
                // 3+ args -> luaL_error(_, "wrong number of arguments").
                //
                // (frankenredis-o3epl) All 5 errors must honor the
                // dual direct/pcall shape. The 4 luaL_argerror calls
                // (interval-empty + number-expected) route through
                // lua_format_argerror; the luaL_error wrong-arity uses
                // a conditional-prefix helper because it has no name
                // in the message.
                let inv_owned: Option<String> = self.current_invocation_name.clone();
                let inv = inv_owned.as_deref();
                let number_expected = |arg_idx: usize, got: &LuaValue| -> String {
                    lua_format_argerror(
                        inv,
                        "random",
                        arg_idx,
                        &format!("number expected, got {}", got.type_name()),
                    )
                };
                let interval_empty = |arg_idx: usize| -> String {
                    lua_format_argerror(inv, "random", arg_idx, "interval is empty")
                };
                let wrong_arity = || -> String {
                    if inv.is_some() {
                        "user_script:1: wrong number of arguments".to_string()
                    } else {
                        "wrong number of arguments".to_string()
                    }
                };
                // (frankenredis-lwj8o) Vendored lmathlib.c::math_random:
                //   lua_Number r = (lua_Number)(rand()%RAND_MAX) /
                //                  (lua_Number)RAND_MAX;
                // Then 0-arg returns r; 1-arg(u) returns floor(r*u)+1;
                // 2-arg(l,u) returns floor(r*(u-l+1))+l. Vendored Redis
                // overrides Lua's rand()/srand() with its own
                // redisLrand48/redisSrand48 (rand.c) — a 48-bit LCG with
                // RAND_MAX 0x7FFFFFFF. We mirror that here.
                let rand_raw = self.lua_random.rand();
                let rand_max = RedisLrand48::RAND_MAX;
                let r = ((rand_raw % rand_max) as f64) / (rand_max as f64);
                match args.len() {
                    0 => Ok(vec![LuaValue::Number(r)]),
                    1 => {
                        let u = args[0]
                            .to_number()
                            .ok_or_else(|| number_expected(1, &args[0]))?
                            as i64;
                        if u < 1 {
                            return Err(interval_empty(1));
                        }
                        let val = (r * u as f64).floor() as i64 + 1;
                        Ok(vec![LuaValue::Number(val as f64)])
                    }
                    2 => {
                        let l = args[0]
                            .to_number()
                            .ok_or_else(|| number_expected(1, &args[0]))?
                            as i64;
                        let u = args[1]
                            .to_number()
                            .ok_or_else(|| number_expected(2, &args[1]))?
                            as i64;
                        if l > u {
                            return Err(interval_empty(2));
                        }
                        let range = (u as i128 - l as i128 + 1) as f64;
                        let val = l as i128 + (r * range).floor() as i128;
                        Ok(vec![LuaValue::Number(val as f64)])
                    }
                    _ => Err(wrong_arity()),
                }
            }
            "math.fmod" => {
                // (frankenredis-3osi6) Vendored math_fmod is implemented as
                //   lua_pushnumber(L, fmod(luaL_checknumber(L,1),
                //                          luaL_checknumber(L,2)));
                // and the compiler (gcc default ABI on x86_64) evaluates
                // the inner arguments right-to-left, so arg #2 is checked
                // first. The same quirk applies to math.pow / math.atan2 /
                // math.ldexp below.
                let b = lua_check_number(self.current_invocation_name.as_deref(), args, 1, "fmod")?;
                let a = lua_check_number(self.current_invocation_name.as_deref(), args, 0, "fmod")?;
                Ok(vec![LuaValue::Number(a % b)])
            }
            "math.log" => {
                let n = lua_check_number(self.current_invocation_name.as_deref(), args, 0, "log")?;
                Ok(vec![LuaValue::Number(n.ln())])
            }
            "math.exp" => {
                let n = lua_check_number(self.current_invocation_name.as_deref(), args, 0, "exp")?;
                Ok(vec![LuaValue::Number(n.exp())])
            }
            "math.pow" => {
                let b = lua_check_number(self.current_invocation_name.as_deref(), args, 1, "pow")?;
                let a = lua_check_number(self.current_invocation_name.as_deref(), args, 0, "pow")?;
                Ok(vec![LuaValue::Number(a.powf(b))])
            }
            "math.sin" => {
                let n = lua_check_number(self.current_invocation_name.as_deref(), args, 0, "sin")?;
                Ok(vec![LuaValue::Number(n.sin())])
            }
            "math.cos" => {
                let n = lua_check_number(self.current_invocation_name.as_deref(), args, 0, "cos")?;
                Ok(vec![LuaValue::Number(n.cos())])
            }
            "math.tan" => {
                let n = lua_check_number(self.current_invocation_name.as_deref(), args, 0, "tan")?;
                Ok(vec![LuaValue::Number(n.tan())])
            }
            "math.asin" => {
                let n = lua_check_number(self.current_invocation_name.as_deref(), args, 0, "asin")?;
                Ok(vec![LuaValue::Number(n.asin())])
            }
            "math.acos" => {
                let n = lua_check_number(self.current_invocation_name.as_deref(), args, 0, "acos")?;
                Ok(vec![LuaValue::Number(n.acos())])
            }
            "math.atan" => {
                let n = lua_check_number(self.current_invocation_name.as_deref(), args, 0, "atan")?;
                Ok(vec![LuaValue::Number(n.atan())])
            }
            "math.atan2" => {
                let x =
                    lua_check_number(self.current_invocation_name.as_deref(), args, 1, "atan2")?;
                let y =
                    lua_check_number(self.current_invocation_name.as_deref(), args, 0, "atan2")?;
                Ok(vec![LuaValue::Number(y.atan2(x))])
            }
            // (frankenredis-9dmqr) Five additional math helpers Lua 5.1
            // exposes that fr's interpreter was missing dispatch arms
            // for. Rust's f64 stdlib supplies each operation directly.
            "math.deg" => {
                let n = lua_check_number(self.current_invocation_name.as_deref(), args, 0, "deg")?;
                Ok(vec![LuaValue::Number(n.to_degrees())])
            }
            "math.rad" => {
                let n = lua_check_number(self.current_invocation_name.as_deref(), args, 0, "rad")?;
                Ok(vec![LuaValue::Number(n.to_radians())])
            }
            "math.sinh" => {
                let n = lua_check_number(self.current_invocation_name.as_deref(), args, 0, "sinh")?;
                Ok(vec![LuaValue::Number(n.sinh())])
            }
            "math.cosh" => {
                let n = lua_check_number(self.current_invocation_name.as_deref(), args, 0, "cosh")?;
                Ok(vec![LuaValue::Number(n.cosh())])
            }
            "math.tanh" => {
                let n = lua_check_number(self.current_invocation_name.as_deref(), args, 0, "tanh")?;
                Ok(vec![LuaValue::Number(n.tanh())])
            }
            "math.log10" => {
                let n =
                    lua_check_number(self.current_invocation_name.as_deref(), args, 0, "log10")?;
                Ok(vec![LuaValue::Number(n.log10())])
            }
            "math.modf" => {
                let n = lua_check_number(self.current_invocation_name.as_deref(), args, 0, "modf")?;
                let trunc = n.trunc();
                let frac = n - trunc;
                Ok(vec![LuaValue::Number(trunc), LuaValue::Number(frac)])
            }
            "math.frexp" => {
                let n =
                    lua_check_number(self.current_invocation_name.as_deref(), args, 0, "frexp")?;
                if n == 0.0 {
                    Ok(vec![LuaValue::Number(0.0), LuaValue::Number(0.0)])
                } else {
                    // frexp: n = m * 2^e where 0.5 <= |m| < 1
                    let bits = n.to_bits();
                    let exp_raw = ((bits >> 52) & 0x7FF) as i64;
                    if exp_raw == 0 {
                        // subnormal
                        let norm = n * (1u64 << 52) as f64;
                        let bits2 = norm.to_bits();
                        let exp2 = ((bits2 >> 52) & 0x7FF) as i64;
                        let e = exp2 - 1023 - 52;
                        let mantissa_bits = (bits2 & 0x000F_FFFF_FFFF_FFFF) | 0x3FE0_0000_0000_0000;
                        let m = f64::from_bits(mantissa_bits).copysign(n);
                        Ok(vec![LuaValue::Number(m), LuaValue::Number((e + 1) as f64)])
                    } else {
                        let e = exp_raw - 1023;
                        let mantissa_bits = (bits & 0x000F_FFFF_FFFF_FFFF) | 0x3FE0_0000_0000_0000;
                        let m = f64::from_bits(mantissa_bits).copysign(n);
                        Ok(vec![LuaValue::Number(m), LuaValue::Number((e + 1) as f64)])
                    }
                }
            }
            "math.ldexp" => {
                let e = lua_check_number(self.current_invocation_name.as_deref(), args, 1, "ldexp")?
                    as i32;
                let m =
                    lua_check_number(self.current_invocation_name.as_deref(), args, 0, "ldexp")?;
                Ok(vec![LuaValue::Number(m * 2f64.powi(e))])
            }
            "math.randomseed" => {
                // (frankenredis-lwj8o) Upstream lmathlib.c::math_randomseed:
                //   srand(luaL_checkint(L, 1));
                // Vendored Redis routes that srand to redisSrand48, which
                // initializes the 48-bit LCG state x[0]=0x330E, x[1]=low16,
                // x[2]=high16 of the seed.
                // (frankenredis-4xjb0) luaL_checkint raises 'bad argument
                // #1 (number expected, got TYPE)' for missing/nil/non-
                // numeric args. fr previously silently no-op'd on any
                // bogus input, masking the error.
                let arg_opt = args.first();
                let n = match arg_opt {
                    Some(v) => {
                        lua_required_integer_arg(self.current_invocation_name.as_deref(), 1, v)?
                    }
                    None => {
                        return Err(lua_bad_number_arg(
                            self.current_invocation_name.as_deref(),
                            1,
                            None,
                        ));
                    }
                };
                let n_f = n as f64;
                let seed_i32 = n as i32; // matches C int cast
                self.lua_random.srand(seed_i32);
                self.rng_seed = n_f.to_bits();
                Ok(vec![LuaValue::Nil])
            }
            // ── OS library ───────────────────────────────────────────────
            "os.clock" => {
                // Redis exposes Lua's os.clock. Approximate CPU time with
                // monotonic elapsed wall time for this script invocation.
                Ok(vec![LuaValue::Number(
                    self.script_started_at.elapsed().as_secs_f64(),
                )])
            }
            // ── Coroutine library ──────────────────────────────────────
            // Redis exposes Lua 5.1 coroutines inside scripts. This
            // interpreter supports the common script patterns directly:
            // create/resume/status/wrap/running plus top-level yields in
            // Lua function bodies. More exotic "yield as expression"
            // continuations intentionally still fall back to ordinary Lua
            // errors instead of panicking.
            // (frankenredis-96j2u) Route all 4 coroutine.* argerror
            // sites through lua_format_argerror so pcall(coroutine.X,…)
            // emits the anonymous shape (`'?'`, no prefix) while
            // direct calls keep the named/prefixed shape.
            "coroutine.create" => {
                if let Some(LuaValue::Function(func)) = args.first() {
                    Ok(vec![LuaValue::Coroutine(LuaCoroutine::new((**func).clone()))])
                } else {
                    Err(lua_format_argerror(
                        self.current_invocation_name.as_deref(),
                        "create",
                        1,
                        "Lua function expected",
                    ))
                }
            }
            "coroutine.wrap" => {
                if let Some(LuaValue::Function(func)) = args.first() {
                    Ok(vec![LuaValue::WrappedCoroutine(LuaCoroutine::new(
                        (**func).clone(),
                    ))])
                } else {
                    Err(lua_format_argerror(
                        self.current_invocation_name.as_deref(),
                        "wrap",
                        1,
                        "Lua function expected",
                    ))
                }
            }
            "coroutine.resume" => match args.first() {
                Some(LuaValue::Coroutine(coroutine)) => {
                    self.resume_coroutine(coroutine, args.get(1..).unwrap_or(&[]))
                }
                _ => Err(lua_format_argerror(
                    self.current_invocation_name.as_deref(),
                    "resume",
                    1,
                    "coroutine expected",
                )),
            },
            "coroutine.status" => match args.first() {
                Some(LuaValue::Coroutine(coroutine)) => {
                    Ok(vec![LuaValue::Str(coroutine.status_name().to_vec())])
                }
                _ => Err(lua_format_argerror(
                    self.current_invocation_name.as_deref(),
                    "status",
                    1,
                    "coroutine expected",
                )),
            },
            "coroutine.yield" => {
                // (frankenredis-ztawj) Yield is only resumable when
                // it fires from the coroutine's OUTER statement
                // level. resume_coroutine + exec_coroutine_stmts
                // track the next-pc at outer-stmt granularity, so a
                // yield from inside any nested exec_stmts call
                // (for / while / repeat / if-then bodies, function-
                // call bodies, pcall / xpcall lambdas) cannot be
                // resumed correctly — the outer for-loop / call /
                // pcall would be skipped on the next resume,
                // silently dropping iterations. Reject those cases
                // with the upstream Lua 5.1 wording instead of
                // letting the impl produce wrong results.
                if self.current_coroutine.is_none()
                    || self.nested_exec_stmts_depth > 0
                    || !self.inside_bare_expression_stmt
                {
                    // (frankenredis-gdbca) Reject yield as part of
                    // LocalAssign / Assign / Return / function-arg
                    // evaluation: bw15's PC tracking advances next_pc
                    // past the whole containing stmt, so the bind /
                    // assign / return step would be silently skipped
                    // on resume — producing nil values where the
                    // user expected resume args. Until the resume
                    // mechanism captures yield-call return values,
                    // the only safe yield site is a bare
                    // Stmt::Expression at the body's outer level.
                    return Err("attempt to yield across metamethod/C-call boundary".to_string());
                }
                self.pending_yield = Some(args.to_vec());
                Err(LUA_YIELD_SENTINEL.to_string())
            }
            "coroutine.running" => {
                if let Some(coroutine) = &self.current_coroutine {
                    Ok(vec![LuaValue::Coroutine(coroutine.clone())])
                } else {
                    Ok(vec![LuaValue::Nil])
                }
            }
            // ── String library ──────────────────────────────────────────
            "string.len" => {
                let s = lua_check_string(self.current_invocation_name.as_deref(), args, 0, "len")?;
                Ok(vec![LuaValue::Number(s.len() as f64)])
            }
            "string.sub" => {
                let s = lua_check_string(self.current_invocation_name.as_deref(), args, 0, "sub")?;
                let len = s.len() as i64;
                let mut i =
                    lua_check_number(self.current_invocation_name.as_deref(), args, 1, "sub")?
                        as i64;
                // (frankenredis-v2ipw) Upstream luaB_sub uses
                // luaL_optinteger for the j arg, raising 'bad
                // argument #3 (number expected, got TYPE)' for
                // non-number-convertible values. fr previously
                // coerced via to_number() and silently defaulted to
                // -1 (full-length), masking the error.
                let mut j = lua_optional_integer_arg(
                    self.current_invocation_name.as_deref(),
                    3,
                    args.get(2),
                    -1,
                )?;
                // Lua string indices: negative means from end
                if i < 0 {
                    i = (len + i + 1).max(1);
                }
                if j < 0 {
                    j = len + j + 1;
                }
                if i < 1 {
                    i = 1;
                }
                if j > len {
                    j = len;
                }
                if i > j {
                    Ok(vec![LuaValue::Str(Vec::new())])
                } else {
                    let start = (i - 1) as usize;
                    let end = j as usize;
                    Ok(vec![LuaValue::Str(s[start..end].to_vec())])
                }
            }
            "string.rep" => {
                let s = lua_check_string(self.current_invocation_name.as_deref(), args, 0, "rep")?;
                let n_val =
                    lua_check_number(self.current_invocation_name.as_deref(), args, 1, "rep")?;
                if n_val < 0.0 {
                    return Ok(vec![LuaValue::Str(Vec::new())]);
                }
                // (frankenredis-jwkhc) The previous unconditional cap on
                // n_val tripped 'string length overflow' for the no-op case
                // string.rep('', N), which vendored just returns as ''. Guard
                // on the *product* instead; an empty source short-circuits.
                if s.is_empty() {
                    return Ok(vec![LuaValue::Str(Vec::new())]);
                }
                let n = if n_val > usize::MAX as f64 {
                    return Err("string length overflow".to_string());
                } else {
                    n_val as usize
                };
                let target_len = s.len().checked_mul(n).ok_or("string length overflow")?;
                if target_len > 512 * 1024 * 1024 {
                    return Err("string length overflow".to_string());
                }

                let mut result = Vec::with_capacity(target_len);
                for _ in 0..n {
                    result.extend_from_slice(&s);
                }
                Ok(vec![LuaValue::Str(result)])
            }
            "string.lower" => {
                let s =
                    lua_check_string(self.current_invocation_name.as_deref(), args, 0, "lower")?;
                Ok(vec![LuaValue::Str(s.to_ascii_lowercase())])
            }
            "string.upper" => {
                let s =
                    lua_check_string(self.current_invocation_name.as_deref(), args, 0, "upper")?;
                Ok(vec![LuaValue::Str(s.to_ascii_uppercase())])
            }
            "string.reverse" => {
                let mut s =
                    lua_check_string(self.current_invocation_name.as_deref(), args, 0, "reverse")?;
                s.reverse();
                Ok(vec![LuaValue::Str(s)])
            }
            // (frankenredis-dqbdr) Lua 5.1 string.dump serialises a
            // function to its bytecode form. fr's tree-walking
            // interpreter has no bytecode representation, so the
            // function is registered (so `type(string.dump)` returns
            // 'function') but errors when invoked.
            "string.dump" => Err("user_script:1: unable to dump given function".to_string()),
            "string.byte" => {
                // (frankenredis-ii6en) Upstream string.byte applies
                // luaL_optint to the optional i and j args, which
                // raises 'bad argument #N to byte (number expected,
                // got TYPE)' when the present arg isn't number-
                // convertible. fr previously routed through
                // to_number().unwrap_or(default), silently masking the
                // error.
                let inv = self.current_invocation_name.as_deref();
                let s = lua_check_string(inv, args, 0, "byte")?;
                let len = s.len() as i64;
                let mut i = lua_optional_integer_arg(inv, 2, args.get(1), 1)?;
                let mut j = lua_optional_integer_arg(inv, 3, args.get(2), i)?;
                if i < 0 {
                    i = len + i + 1;
                }
                if j < 0 {
                    j = len + j + 1;
                }
                let mut results = Vec::new();
                let start = i.max(1);
                let end = j.min(len);
                for idx in start..=end {
                    results.push(LuaValue::Number(s[(idx - 1) as usize] as f64));
                }
                Ok(results)
            }
            "string.char" => {
                // (frankenredis-uni2j) Upstream's luaL_argerror prepends
                // the source-location prefix "user_script:1: " to the
                // argument-error template.
                // (frankenredis-ymb2q) Route through lua_format_argerror
                // so `pcall(string.char, …)` emits the anonymous-C-
                // function shape (`'?'` name, no `user_script:1:`
                // prefix) while direct/Lua-wrapped calls keep the named
                // shape.
                let inv = self.current_invocation_name.as_deref();
                let mut result = Vec::new();
                for (i, a) in args.iter().enumerate() {
                    let n = a.to_number().ok_or_else(|| {
                        lua_format_argerror(
                            inv,
                            "char",
                            i + 1,
                            &format!("number expected, got {}", a.type_name()),
                        )
                    })? as i64;
                    if !(0..=255).contains(&n) {
                        return Err(lua_format_argerror(inv, "char", i + 1, "invalid value"));
                    }
                    result.push(n as u8);
                }
                Ok(vec![LuaValue::Str(result)])
            }
            "string.format" => {
                // (frankenredis-be7o1) DOS fix: previously, calling
                // string.format() with no args panicked on the args[1..]
                // slice and killed the server. Validate arg #1 explicitly,
                // matching upstream luaL_checkstring wording.
                //
                // (frankenredis-fllxr) Route through current_invocation_name
                // so `pcall(string.format, …)` emits the anonymous-C-function
                // shape (`bad argument #N to '?' (…)`, no `user_script:1:`
                // prefix) while direct/lua-wrapped calls keep the named
                // `user_script:1: bad argument #N to 'format' (…)` shape.
                let inv = self.current_invocation_name.as_deref();
                let fmt_bytes = match args.first() {
                    Some(LuaValue::Str(b)) => b.clone(),
                    Some(LuaValue::Number(n)) => {
                        if *n == (*n as i64) as f64 && n.is_finite() {
                            format!("{}", *n as i64).into_bytes()
                        } else {
                            lua_number_to_string(*n).into_bytes()
                        }
                    }
                    Some(other) => {
                        return Err(lua_format_argerror(
                            inv,
                            "format",
                            1,
                            &format!("string expected, got {}", other.type_name()),
                        ));
                    }
                    None => {
                        return Err(lua_format_argerror(
                            inv,
                            "format",
                            1,
                            "string expected, got no value",
                        ));
                    }
                };
                let fmt_str = String::from_utf8_lossy(&fmt_bytes).to_string();
                let rest: &[LuaValue] = if args.is_empty() { &[] } else { &args[1..] };
                let result = lua_string_format(inv, &fmt_str, rest)?;
                Ok(vec![LuaValue::Str(result)])
            }
            "string.find" => {
                let s = lua_check_string(self.current_invocation_name.as_deref(), args, 0, "find")?;
                let pattern =
                    lua_check_string(self.current_invocation_name.as_deref(), args, 1, "find")?;
                // (frankenredis-izta5) Upstream luaB_str_find_aux uses
                // luaL_optinteger for the init arg, which raises 'bad
                // argument #3 (number expected, got TYPE)' when the
                // value is present but neither a number nor a numeric
                // string. fr previously coerced via to_number() and
                // silently defaulted to 1 for bogus inputs, masking
                // the error.
                let init_raw = lua_optional_integer_arg(
                    self.current_invocation_name.as_deref(),
                    3,
                    args.get(2),
                    1,
                )?;
                let init = if init_raw < 0 {
                    (s.len() as i64 + init_raw).max(0) as usize
                } else {
                    (init_raw as usize).saturating_sub(1)
                };
                let init = init.min(s.len());
                let plain = args.get(3).map(|v| v.is_truthy()).unwrap_or(false);
                // (frankenredis-vfv8s) Validate the pattern eagerly so
                // malformed inputs raise upstream-shaped errors instead
                // of silently returning nil. The plain-search path is
                // exempt because Lua's plain mode treats the pattern
                // bytes literally.
                // (frankenredis-uqnq6) Route through inv_name so
                // pcall(string.find,…,bad) drops the user_script:1:
                // prefix to match the anonymous C-builtin shape.
                if !plain {
                    lua_pattern_validate_named(self.current_invocation_name.as_deref(), &pattern)?;
                }
                if plain {
                    // Plain substring search
                    if let Some(pos) = s[init..]
                        .windows(pattern.len().max(1))
                        .position(|w| w == pattern.as_slice())
                    {
                        let start = init + pos + 1; // 1-indexed
                        let end = start + pattern.len() - 1;
                        Ok(vec![
                            LuaValue::Number(start as f64),
                            LuaValue::Number(end as f64),
                        ])
                    } else if pattern.is_empty() {
                        Ok(vec![
                            LuaValue::Number((init + 1) as f64),
                            LuaValue::Number(init as f64),
                        ])
                    } else {
                        Ok(vec![LuaValue::Nil])
                    }
                } else {
                    // Lua pattern matching
                    if let Some(m) = lua_pattern_find(&s, &pattern, init) {
                        let mut result = vec![
                            LuaValue::Number((m.start + 1) as f64), // 1-indexed
                            LuaValue::Number(m.end as f64),         // inclusive end
                        ];
                        // Append captures if any
                        for cap in &m.captures {
                            match cap {
                                LuaCapture::Substring(cs, Some(ce)) => {
                                    result.push(LuaValue::Str(s[*cs..*ce].to_vec()));
                                }
                                LuaCapture::Substring(_, None) => {}
                                LuaCapture::Position(pos) => {
                                    result.push(LuaValue::Number(*pos as f64 + 1.0));
                                }
                            }
                        }
                        Ok(result)
                    } else {
                        Ok(vec![LuaValue::Nil])
                    }
                }
            }
            "string.match" => {
                let s =
                    lua_check_string(self.current_invocation_name.as_deref(), args, 0, "match")?;
                let pattern =
                    lua_check_string(self.current_invocation_name.as_deref(), args, 1, "match")?;
                // (frankenredis-izta5) Same init-arg validation as
                // string.find — upstream luaB_str_find_aux raises
                // 'bad argument #3' for non-number-convertible init.
                let init_raw = lua_optional_integer_arg(
                    self.current_invocation_name.as_deref(),
                    3,
                    args.get(2),
                    1,
                )?;
                let init = if init_raw < 0 {
                    (s.len() as i64 + init_raw).max(0) as usize
                } else {
                    (init_raw as usize).saturating_sub(1)
                };
                // (frankenredis-vfv8s) Validate pattern eagerly.
                // (frankenredis-uqnq6) inv_name routes the prefix.
                lua_pattern_validate_named(self.current_invocation_name.as_deref(), &pattern)?;
                if let Some(m) = lua_pattern_find(&s, &pattern, init) {
                    Ok(lua_match_captures(&s, &m))
                } else {
                    Ok(vec![LuaValue::Nil])
                }
            }
            "string.gmatch" => {
                // Returns an iterator function. Each call returns next match.
                // We collect all matches and return a closure-like iterator via a table.
                let s =
                    lua_check_string(self.current_invocation_name.as_deref(), args, 0, "gmatch")?;
                let pattern =
                    lua_check_string(self.current_invocation_name.as_deref(), args, 1, "gmatch")?;
                // (frankenredis-vfv8s) Validate pattern eagerly so the
                // iterator constructor surfaces malformed patterns the
                // same way upstream's gmatch does.
                // (frankenredis-uqnq6) inv_name routes the prefix.
                lua_pattern_validate_named(self.current_invocation_name.as_deref(), &pattern)?;
                // Collect all matches
                let mut matches: Vec<Vec<LuaValue>> = Vec::new();
                let mut pos = 0;
                while pos <= s.len() {
                    if let Some(m) = lua_pattern_find(&s, &pattern, pos) {
                        matches.push(lua_match_captures(&s, &m));
                        pos = if m.end == m.start { m.end + 1 } else { m.end };
                    } else {
                        break;
                    }
                }
                // Build result table with all matches for iteration
                let result_table = LuaTable::new();
                for (i, cap_vals) in matches.iter().enumerate() {
                    let row = LuaTable::new();
                    for (j, val) in cap_vals.iter().enumerate() {
                        row.set(LuaValue::Number((j + 1) as f64), val.clone());
                    }
                    result_table.set(LuaValue::Number((i + 1) as f64), LuaValue::Table(row));
                }
                // (frankenredis-8vp9w) gmatch must return a stateful
                // iterator that works both via for-in dispatch and via
                // direct calls (e.g. `gmatch(s,p)()` or stored in a
                // local and called repeatedly). Vendored gmatch returns
                // a closure with the source/pattern/position captured
                // as upvalues. We approximate that with a Table holding
                // the precomputed matches plus a __call metamethod
                // pointing at __gmatch_iter. The for-in path still
                // works: GenericFor calls iter_fn(state, control), the
                // table dispatches through metatable_call_handler which
                // prepends the table itself as the first arg —
                // matching the state slot __gmatch_iter already reads.
                let iter_state = LuaTable::new();
                iter_state.set(
                    LuaValue::Str(b"__gmatch_data".to_vec()),
                    LuaValue::Table(result_table),
                );
                iter_state.set(
                    LuaValue::Str(b"__gmatch_idx".to_vec()),
                    LuaValue::Number(0.0),
                );
                let mt = LuaTable::new();
                mt.set(
                    LuaValue::Str(b"__call".to_vec()),
                    LuaValue::RustFunction("__gmatch_iter".to_string()),
                );
                iter_state.inner.borrow_mut().metatable = Some(mt);
                Ok(vec![
                    LuaValue::Table(iter_state),
                    LuaValue::Nil,
                    LuaValue::Nil,
                ])
            }
            "string.gsub" => {
                let inv_owned: Option<String> = self.current_invocation_name.clone();
                let inv = inv_owned.as_deref();
                let s = lua_check_string(inv, args, 0, "gsub")?;
                let pattern = lua_check_string(inv, args, 1, "gsub")?;
                let repl = args.get(2).cloned().unwrap_or(LuaValue::Nil);
                // (frankenredis-tfob7) Upstream lstrlib.c:str_gsub
                // validates the repl type at function entry via
                // luaL_argcheck(... "string/function/table expected").
                // fr previously only checked the type inside the per-
                // match dispatch, so calling `string.gsub('a','b')`
                // (nil repl, pattern doesn't match) or with a boolean
                // repl returned the input unchanged instead of erroring.
                match &repl {
                    LuaValue::Str(_)
                    | LuaValue::Number(_)
                    | LuaValue::Table(_)
                    | LuaValue::Function(_)
                    | LuaValue::RustFunction(_)
                    | LuaValue::WrappedCoroutine(_) => {}
                    _ => {
                        return Err(lua_format_argerror(
                            inv,
                            "gsub",
                            3,
                            "string/function/table expected",
                        ));
                    }
                }
                // (frankenredis-mzjqw) Upstream lstrlib.c::str_gsub uses
                // luaL_optinteger for the optional 4th arg ('max
                // substitutions'), raising 'bad argument #4 (number
                // expected, got TYPE)' for non-number-convertible
                // values. fr previously coerced via to_number() and
                // silently defaulted to None (unlimited), masking
                // the error. For negative n, preserve the original
                // saturating `f64 as usize` behavior (which yielded
                // 0 substitutions on -N) — vendored matches because
                // it treats negative-as-0 too.
                let max_n: Option<usize> = match args.get(3) {
                    None | Some(LuaValue::Nil) => None,
                    Some(v) => {
                        let n = lua_required_integer_arg(inv, 4, v)?;
                        Some(if n < 0 { 0 } else { n as usize })
                    }
                };
                // (frankenredis-vfv8s) Validate pattern eagerly.
                // (frankenredis-uqnq6) inv_name routes the prefix.
                lua_pattern_validate_named(inv, &pattern)?;
                let mut result = Vec::new();
                let mut pos = 0;
                let mut count = 0usize;
                while pos <= s.len() {
                    if let Some(limit) = max_n
                        && count >= limit
                    {
                        break;
                    }
                    if let Some(m) = lua_pattern_find(&s, &pattern, pos) {
                        result.extend_from_slice(&s[pos..m.start]);
                        // (frankenredis-76y97) Dispatch on the repl
                        // value type per Lua 5.1 spec: string uses %0/%N
                        // capture refs (legacy path), table is indexed
                        // by the 1st capture or whole match, function
                        // is called with captures.
                        let replacement: Vec<u8> = match &repl {
                            LuaValue::Str(repl_bytes) => lua_gsub_replace(&s, &m, repl_bytes)?,
                            LuaValue::Table(t) => {
                                let key_bytes = lua_gsub_capture_key(&s, &m);
                                let key = LuaValue::Str(key_bytes);
                                let val = t.get(&key);
                                lua_gsub_normalise_repl(&s, &m, &val)?
                            }
                            LuaValue::Function(_)
                            | LuaValue::RustFunction(_)
                            | LuaValue::WrappedCoroutine(_) => {
                                let mut call_args = lua_gsub_capture_args(&s, &m);
                                let mut varargs = Vec::new();
                                let ret =
                                    self.call_function(&repl, &mut call_args, env, &mut varargs)?;
                                let first = ret.into_iter().next().unwrap_or(LuaValue::Nil);
                                lua_gsub_normalise_repl(&s, &m, &first)?
                            }
                            LuaValue::Number(_) => {
                                // Legacy: numeric repl coerces to string.
                                lua_gsub_replace(&s, &m, &repl.to_display_string())?
                            }
                            _ => {
                                // Unreachable: the upfront repl-type
                                // check (frankenredis-tfob7) rejects
                                // anything outside string/number/table/
                                // function before the loop runs.
                                return Err(lua_format_argerror(
                                    inv_owned.as_deref(),
                                    "gsub",
                                    3,
                                    "string/function/table expected",
                                ));
                            }
                        };
                        result.extend_from_slice(&replacement);
                        count += 1;
                        if m.end == m.start {
                            if m.end < s.len() {
                                result.push(s[m.end]);
                            }
                            pos = m.end + 1;
                        } else {
                            pos = m.end;
                        }
                    } else {
                        break;
                    }
                }
                if pos <= s.len() {
                    result.extend_from_slice(&s[pos..]);
                }
                Ok(vec![LuaValue::Str(result), LuaValue::Number(count as f64)])
            }
            // ── Table library ───────────────────────────────────────────
            "table.insert" => {
                let inv = self.current_invocation_name.as_deref();
                let table = args.first().cloned().unwrap_or(LuaValue::Nil);
                let LuaValue::Table(_) = &table else {
                    return Err(lua_bad_table_arg(inv, 1, args.first()));
                };
                // (frankenredis-jwkhc) Upstream Redis-vendored ltablib.c
                // raises 'wrong number of arguments to insert' for any
                // shape other than (t, v) or (t, pos, v).
                // (frankenredis-toecv) luaL_error wording: drop the
                // user_script:1: prefix when invoked via pcall(C-builtin).
                if args.len() != 2 && args.len() != 3 {
                    let body = "wrong number of arguments to 'insert'";
                    return Err(if inv.is_some() {
                        format!("user_script:1: {body}")
                    } else {
                        body.to_string()
                    });
                }
                // (frankenredis-8mwy9) Writing into a protected library table
                // raises the readonly error (after the arity check, before any
                // element move), matching redis.
                if matches!(&args[0], LuaValue::Table(t) if t.is_readonly()) {
                    return Err("Attempt to modify a readonly table".to_string());
                }
                if args.len() == 2 {
                    let val = args[1].clone();
                    if let LuaValue::Table(ref mut t) = args[0] {
                        t.inner.borrow_mut().array.push(val);
                    }
                } else {
                    // (frankenredis-jwkhc) Redis's vendored Lua has the bounds
                    // check stripped from ltablib.c::tinsert:
                    //   if (pos > e) e = pos;  /* `grow' array if necessary */
                    // So pos may be 0, negative, or > #t+1 — vendored grows the
                    // array (or stores in the hash part for non-positive keys)
                    // rather than erroring.
                    let pos_i = lua_required_integer_arg(inv, 2, &args[1])?;
                    let val = args[2].clone();
                    if let LuaValue::Table(ref mut t) = args[0] {
                        if pos_i >= 1 {
                            let pos = pos_i as usize;
                            let len = t.inner.borrow().array.len();
                            if pos <= len + 1 {
                                t.inner.borrow_mut().array.insert(pos - 1, val);
                            } else {
                                // Grow array with nils up to pos, then place val.
                                {
                                    let mut inner = t.inner.borrow_mut();
                                    inner.array.resize(pos - 1, LuaValue::Nil);
                                    inner.array.push(val);
                                }
                            }
                        } else {
                            // pos <= 0 — store with integer key in the hash
                            // part. Upstream's lua_rawseti writes to the slot
                            // directly; fr's table mirrors that via .set on a
                            // numeric key.
                            t.set(LuaValue::Number(pos_i as f64), val);
                        }
                    }
                }
                Ok(vec![LuaValue::Nil])
            }
            "table.remove" => {
                let inv = self.current_invocation_name.as_deref();
                let table = args.first().cloned().unwrap_or(LuaValue::Nil);
                if !matches!(table, LuaValue::Table(_)) {
                    return Err(lua_bad_table_arg(inv, 1, args.first()));
                }
                let pos_arg = args.get(1).cloned();
                if let LuaValue::Table(ref mut t) = args[0] {
                    let len = t.inner.borrow().array.len() as i64;
                    let pos = lua_optional_integer_arg(inv, 2, pos_arg.as_ref(), len)?;
                    // (frankenredis-2hgg1) Redis's vendored Lua 5.1's
                    // ltablib.c::tremove returns 0 Lua values (not a
                    // pushed nil) when the position is out of bounds:
                    //   if (!(1 <= pos && pos <= e)) return 0;
                    // fr previously returned a single nil for this
                    // case, breaking the return arity contract scripts
                    // depend on (select('#', ...) == 0 vs 1).
                    if pos < 1 || pos > len {
                        return Ok(vec![]);
                    }
                    let removed = t.inner.borrow_mut().array.remove((pos - 1) as usize);
                    return Ok(vec![removed]);
                }
                Ok(vec![])
            }
            "table.concat" => {
                let inv = self.current_invocation_name.as_deref();
                let t = lua_table_arg(inv, 1, args.first())?;
                // (frankenredis-jwkhc) Upstream luaL_optstring(L, 2, '') maps
                // nil/missing sep to the empty string. fr previously routed
                // through to_display_string() which renders nil as the literal
                // 'nil', producing 'a nil b' instead of 'ab'.
                // (frankenredis-a3ksp) luaL_optlstring further requires that
                // non-nil sep be a string or number — anything else (table,
                // boolean, function, ...) raises bad-argument. fr previously
                // accepted any value and silently produced garbage like
                // "1table:0x...2table:0x...3" when the sep was a table.
                let sep: Vec<u8> = match args.get(1) {
                    None | Some(LuaValue::Nil) => Vec::new(),
                    Some(LuaValue::Str(s)) => s.clone(),
                    Some(LuaValue::Number(n)) => {
                        if *n == (*n as i64) as f64 && n.is_finite() {
                            format!("{}", *n as i64).into_bytes()
                        } else {
                            lua_number_to_string(*n).into_bytes()
                        }
                    }
                    Some(other) => {
                        return Err(format!(
                            "user_script:1: bad argument #2 to 'concat' (string expected, got {})",
                            other.type_name()
                        ));
                    }
                };
                // (frankenredis-y0ri2) Upstream ltablib.c::tconcat defaults
                // `last` to `luaL_getn(L,1)` which is the same border-search
                // length as the `#` operator — not array.sizearray. Using
                // raw array.len() here would mean `table.concat({1,nil,3})`
                // silently truncates at the first nil instead of raising,
                // because fr's array previously dropped nil-hole slots and
                // now retains them.
                let array_len = t.inner.borrow().border_len() as i64;
                let start = lua_optional_integer_arg(inv, 3, args.get(2), 1)?;
                let end = lua_optional_integer_arg(inv, 4, args.get(3), array_len)?;
                // (frankenredis-jwkhc) Upstream ltablib.c::tconcat validates
                // each element in [start, end] and raises 'invalid value (nil
                // | <type>) at index N in table for concat' for any non-
                // string/non-number entry. fr previously silently dropped
                // out-of-range indices and nil holes.
                // (frankenredis-toecv) luaL_error wording: prefix only
                // when invoked via a named call site.
                let invalid_value = |idx: i64, ty: &str| -> String {
                    let body = format!("invalid value ({ty}) at index {idx} in table for 'concat'");
                    if inv.is_some() {
                        format!("user_script:1: {body}")
                    } else {
                        body
                    }
                };
                let mut result: Vec<u8> = Vec::new();
                if start <= end {
                    let mut first = true;
                    for i in start..=end {
                        let val = t.get(&LuaValue::Number(i as f64));
                        let bytes = match val {
                            LuaValue::Str(b) => b,
                            LuaValue::Number(n) => {
                                if n == (n as i64) as f64 && n.is_finite() {
                                    format!("{}", n as i64).into_bytes()
                                } else {
                                    lua_number_to_string(n).into_bytes()
                                }
                            }
                            LuaValue::Nil => {
                                return Err(invalid_value(i, "nil"));
                            }
                            other => {
                                return Err(invalid_value(i, other.type_name()));
                            }
                        };
                        if !first {
                            result.extend_from_slice(&sep);
                        }
                        result.extend_from_slice(&bytes);
                        first = false;
                    }
                }
                Ok(vec![LuaValue::Str(result)])
            }
            "table.sort" => {
                // (frankenredis-3osi6) Upstream ltablib.c:sort uses
                // luaL_checktype(L, 1, LUA_TTABLE) which raises on missing
                // or non-table arg #1.
                let _ = lua_check_table(self.current_invocation_name.as_deref(), args, 0, "sort")?;
                // (frankenredis-b2cmq) Validate comparator type: it
                // must be nil/missing OR a callable. Anything else
                // (string, number, table-without-__call, ...) raises
                // the luaL_checktype 'function expected, got TYPE'
                // wording before any comparison is attempted.
                let comp_fn = match args.get(1) {
                    None | Some(LuaValue::Nil) => None,
                    Some(v) => {
                        let callable =
                            matches!(v, LuaValue::Function(_) | LuaValue::RustFunction(_))
                                || (matches!(v, LuaValue::Table(_))
                                    && self.metatable_call_handler(v).is_some());
                        if !callable {
                            return Err(self.format_builtin_argerror(
                                "sort",
                                2,
                                &format!("function expected, got {}", v.type_name()),
                            ));
                        }
                        Some(v.clone())
                    }
                };
                {
                    // Extract array so we can call comparator without borrow conflicts
                    let mut arr = if let LuaValue::Table(ref mut t) = args[0] {
                        std::mem::take(&mut t.inner.borrow_mut().array)
                    } else {
                        return Ok(vec![LuaValue::Nil]);
                    };
                    if let Some(comp) = comp_fn {
                        // Custom comparator: use insertion sort since we need
                        // to call self.call_function for each comparison
                        for i in 1..arr.len() {
                            let key = arr[i].clone();
                            let mut j = i;
                            while j > 0 {
                                let mut cmp_args = vec![key.clone(), arr[j - 1].clone()];
                                let result =
                                    self.call_function(&comp, &mut cmp_args, env, &mut Vec::new())?;
                                let key_before =
                                    result.first().map(|v| v.is_truthy()).unwrap_or(false);
                                if !key_before {
                                    break;
                                }
                                arr[j] = arr[j - 1].clone();
                                j -= 1;
                            }
                            arr[j] = key;
                        }
                    } else {
                        // (frankenredis-b2cmq) Default sort: comparable
                        // pairs are (Number, Number) or (Str, Str).
                        // Anything else (nil, table, bool, mixed-type)
                        // raises the same "attempt to compare X with Y"
                        // wording the < operator emits at runtime — that
                        // is exactly what upstream propagates from the
                        // C-level luaO_str2d-driven default sort.
                        for i in 1..arr.len() {
                            let key = arr[i].clone();
                            let mut j = i;
                            while j > 0 {
                                let order = match (&key, &arr[j - 1]) {
                                    (LuaValue::Number(a), LuaValue::Number(b)) => {
                                        a.partial_cmp(b).ok_or_else(|| {
                                            // (frankenredis-b2cmq) Errors from the
                                            // C-level default sort do NOT carry the
                                            // user_script:1 prefix; vendored emits
                                            // the bare wording.
                                            "attempt to compare two number values".to_string()
                                        })?
                                    }
                                    (LuaValue::Str(a), LuaValue::Str(b)) => a.cmp(b),
                                    (a, b) => {
                                        return Err(format!(
                                            "attempt to compare {} with {}",
                                            a.type_name(),
                                            b.type_name()
                                        ));
                                    }
                                };
                                if order != std::cmp::Ordering::Less {
                                    break;
                                }
                                arr[j] = arr[j - 1].clone();
                                j -= 1;
                            }
                            arr[j] = key;
                        }
                    }
                    // Put array back
                    if let LuaValue::Table(ref mut t) = args[0] {
                        t.inner.borrow_mut().array = arr;
                    }
                }
                Ok(vec![LuaValue::Nil])
            }
            "table.getn" => {
                // (frankenredis-ian3l) Upstream ltablib.c::getn uses
                // luaL_checktype(L, 1, LUA_TTABLE) which raises 'bad
                // argument #1 (table expected, got TYPE)' for any
                // non-table arg (including missing/nil). fr previously
                // silently returned 0 for bogus inputs, matching the
                // table.maxn fix already in place.
                let t = lua_check_table(self.current_invocation_name.as_deref(), args, 0, "getn")?;
                Ok(vec![LuaValue::Number(t.len() as f64)])
            }
            "table.maxn" => {
                // (frankenredis-3osi6) Upstream ltablib.c:maxn uses
                // luaL_checktype(L, 1, LUA_TTABLE).
                let _ = lua_check_table(self.current_invocation_name.as_deref(), args, 0, "maxn")?;
                let table = args.first().cloned().unwrap_or(LuaValue::Nil);
                if let LuaValue::Table(t) = &table {
                    let mut max_n: f64 = 0.0;
                    // Check array part
                    if !t.inner.borrow().array.is_empty() {
                        max_n = t.inner.borrow().array.len() as f64;
                    }
                    // Check hash part for numeric keys
                    for (k, _) in t.inner.borrow().other_hash.clone() {
                        if let LuaValue::Number(n) = k
                            && n > max_n
                        {
                            max_n = n;
                        }
                    }
                    Ok(vec![LuaValue::Number(max_n)])
                } else {
                    Ok(vec![LuaValue::Number(0.0)])
                }
            }
            // (frankenredis-1ohjy) Lua 5.1 ltablib.c:tforeach and
            // tforeachi — deprecated/removed in 5.2+ but Redis pins
            // Lua 5.1 so vendored exposes both. foreach iterates the
            // entire table via lua_next (array part skipping nil
            // holes, then hash part); foreachi iterates 1..n inclusive
            // calling the function with (i, t[i]) for every index.
            // Both short-circuit and return the first non-nil value
            // the callback yields; otherwise no value is returned.
            "table.foreach" => {
                let inv = self.current_invocation_name.as_deref();
                let table = lua_check_table(inv, args, 0, "foreach")?;
                let func = match args.get(1) {
                    Some(v)
                        if matches!(
                            v,
                            LuaValue::Function(_)
                                | LuaValue::RustFunction(_)
                                | LuaValue::WrappedCoroutine(_)
                        ) || (matches!(v, LuaValue::Table(_))
                            && self.metatable_call_handler(v).is_some()) =>
                    {
                        v.clone()
                    }
                    other => {
                        return Err(self.format_builtin_argerror(
                            "foreach",
                            2,
                            &format!("function expected, got {}", lua_arg_got_label(other),),
                        ));
                    }
                };
                let array_snapshot = table.inner.borrow().array.clone();
                for (i, v) in array_snapshot.iter().enumerate() {
                    if matches!(v, LuaValue::Nil) {
                        continue;
                    }
                    let mut call_args = vec![LuaValue::Number((i + 1) as f64), v.clone()];
                    let result = self.call_function(&func, &mut call_args, env, &mut Vec::new())?;
                    if let Some(first) = result.first()
                        && !matches!(first, LuaValue::Nil)
                    {
                        return Ok(vec![first.clone()]);
                    }
                }
                let hash_pairs = table.hash_pairs();
                for (k, v) in hash_pairs {
                    let mut call_args = vec![k, v];
                    let result = self.call_function(&func, &mut call_args, env, &mut Vec::new())?;
                    if let Some(first) = result.first()
                        && !matches!(first, LuaValue::Nil)
                    {
                        return Ok(vec![first.clone()]);
                    }
                }
                Ok(vec![])
            }
            "table.foreachi" => {
                let inv = self.current_invocation_name.as_deref();
                let table = lua_check_table(inv, args, 0, "foreachi")?;
                let func = match args.get(1) {
                    Some(v)
                        if matches!(
                            v,
                            LuaValue::Function(_)
                                | LuaValue::RustFunction(_)
                                | LuaValue::WrappedCoroutine(_)
                        ) || (matches!(v, LuaValue::Table(_))
                            && self.metatable_call_handler(v).is_some()) =>
                    {
                        v.clone()
                    }
                    other => {
                        return Err(self.format_builtin_argerror(
                            "foreachi",
                            2,
                            &format!("function expected, got {}", lua_arg_got_label(other),),
                        ));
                    }
                };
                let n = table.inner.borrow().array.len();
                for i in 1..=n {
                    let v = table.inner.borrow().array[i - 1].clone();
                    let mut call_args = vec![LuaValue::Number(i as f64), v];
                    let result = self.call_function(&func, &mut call_args, env, &mut Vec::new())?;
                    if let Some(first) = result.first()
                        && !matches!(first, LuaValue::Nil)
                    {
                        return Ok(vec![first.clone()]);
                    }
                }
                Ok(vec![])
            }
            // ── plain top-level globals (frankenredis-vgnsc) ──────────
            "rawequal" => {
                // (frankenredis-uyj7c) Upstream luaB_rawequal uses
                // luaL_checkany on both args — missing/explicit-nil call
                // raises 'bad argument #N to rawequal (value expected)'.
                // (frankenredis-i18ug) Funnel through format_builtin_argerror
                // so the prefix/name reflect AST vs indirect-pcall callers.
                if args.is_empty() {
                    return Err(self.format_builtin_argerror("rawequal", 1, "value expected"));
                }
                if args.get(1).is_none() {
                    return Err(self.format_builtin_argerror("rawequal", 2, "value expected"));
                }
                let a = &args[0];
                let b = &args[1];
                // rawequal: identity / value equality with no metamethod
                // dispatch. fr's LuaValue Eq derives the right semantics
                // for primitive kinds; tables compare by Rc pointer
                // identity through their underlying Rc<RefCell>.
                let eq = match (a, b) {
                    (LuaValue::Nil, LuaValue::Nil) => true,
                    (LuaValue::Bool(x), LuaValue::Bool(y)) => x == y,
                    (LuaValue::Number(x), LuaValue::Number(y)) => x == y,
                    (LuaValue::Str(x), LuaValue::Str(y)) => x == y,
                    (LuaValue::Table(x), LuaValue::Table(y)) => Rc::ptr_eq(&x.inner, &y.inner),
                    _ => false,
                };
                Ok(vec![LuaValue::Bool(eq)])
            }
            "gcinfo" => {
                // Upstream LuaJIT/Lua 5.1 returns the Lua heap usage in
                // kilobytes as an integer. fr has no Lua-side GC so a
                // stable placeholder keeps scripts that only branch on
                // "is the value finite?" working.
                Ok(vec![LuaValue::Number(32.0)])
            }
            "collectgarbage" => {
                // (frankenredis-uyj7c) Upstream luaB_collectgarbage accepts
                // a documented option set; anything else triggers
                // luaL_argerror with 'invalid option <name>'. fr previously
                // returned 0 for any unknown option.
                //
                // (frankenredis-kx6jm) Route both error sites through
                // lua_format_argerror so pcall(collectgarbage,…) emits
                // the anonymous-C-function shape (`'?'` name, no
                // user_script:1: prefix) while direct calls keep the
                // named shape.
                let inv = self.current_invocation_name.as_deref();
                let opt = match args.first() {
                    Some(LuaValue::Str(s)) => String::from_utf8_lossy(s).to_string(),
                    Some(LuaValue::Number(n)) => format!("{n}"),
                    Some(LuaValue::Nil) | None => "collect".to_string(),
                    Some(other) => {
                        return Err(lua_format_argerror(
                            inv,
                            "collectgarbage",
                            1,
                            &format!("string expected, got {}", other.type_name()),
                        ));
                    }
                };
                let known = matches!(
                    opt.as_str(),
                    "collect" | "stop" | "restart" | "step" | "setpause" | "setstepmul" | "count"
                );
                if !known {
                    return Err(lua_format_argerror(
                        inv,
                        "collectgarbage",
                        1,
                        &format!("invalid option '{opt}'"),
                    ));
                }
                let n = if opt == "count" { 32.0 } else { 0.0 };
                Ok(vec![LuaValue::Number(n)])
            }
            // ── bit library (LuaJIT 32-bit semantics) ─────────────────
            // (frankenredis-v95aj) Mirrors Lua 5.1 LuaJIT bit library
            // semantics: every numeric input is normalised to u32 via
            // tobit_u32(), every numeric result is returned as the i32
            // reinterpretation of that u32 so values >= 2^31 print as
            // negative numbers (matching upstream's "all returns are
            // signed 32-bit Lua numbers" rule).
            "bit.band" => {
                // (frankenredis-d3ovh) Upstream LuaJIT bit.band requires
                // at least one numeric arg; calling with zero args raises
                // "bad argument #1 to 'band' (number expected, got no
                // value)". fr previously silently returned 0xFFFFFFFF.
                let inv = self.current_invocation_name.as_deref();
                if args.is_empty() {
                    return Err(lua_value_to_u32_for_bitop(inv, 1, "band", None).unwrap_err());
                }
                let mut acc = 0xFFFF_FFFFu32;
                for (i, a) in args.iter().enumerate() {
                    acc &= lua_value_to_u32_for_bitop(inv, i + 1, "band", Some(a))?;
                }
                Ok(vec![LuaValue::Number(acc as i32 as f64)])
            }
            "bit.bor" => {
                let inv = self.current_invocation_name.as_deref();
                if args.is_empty() {
                    return Err(lua_value_to_u32_for_bitop(inv, 1, "bor", None).unwrap_err());
                }
                let mut acc = 0u32;
                for (i, a) in args.iter().enumerate() {
                    acc |= lua_value_to_u32_for_bitop(inv, i + 1, "bor", Some(a))?;
                }
                Ok(vec![LuaValue::Number(acc as i32 as f64)])
            }
            "bit.bxor" => {
                let inv = self.current_invocation_name.as_deref();
                if args.is_empty() {
                    return Err(lua_value_to_u32_for_bitop(inv, 1, "bxor", None).unwrap_err());
                }
                let mut acc = 0u32;
                for (i, a) in args.iter().enumerate() {
                    acc ^= lua_value_to_u32_for_bitop(inv, i + 1, "bxor", Some(a))?;
                }
                Ok(vec![LuaValue::Number(acc as i32 as f64)])
            }
            "bit.bnot" => {
                let inv = self.current_invocation_name.as_deref();
                let x = lua_value_to_u32_for_bitop(inv, 1, "bnot", args.first())?;
                Ok(vec![LuaValue::Number(!x as i32 as f64)])
            }
            "bit.lshift" => {
                let inv = self.current_invocation_name.as_deref();
                let x = lua_value_to_u32_for_bitop(inv, 1, "lshift", args.first())?;
                let n = lua_value_to_u32_for_bitop(inv, 2, "lshift", args.get(1))? & 31;
                Ok(vec![LuaValue::Number((x << n) as i32 as f64)])
            }
            "bit.rshift" => {
                let inv = self.current_invocation_name.as_deref();
                let x = lua_value_to_u32_for_bitop(inv, 1, "rshift", args.first())?;
                let n = lua_value_to_u32_for_bitop(inv, 2, "rshift", args.get(1))? & 31;
                Ok(vec![LuaValue::Number((x >> n) as i32 as f64)])
            }
            "bit.arshift" => {
                let inv = self.current_invocation_name.as_deref();
                let x = lua_value_to_u32_for_bitop(inv, 1, "arshift", args.first())? as i32;
                let n = lua_value_to_u32_for_bitop(inv, 2, "arshift", args.get(1))? & 31;
                Ok(vec![LuaValue::Number((x >> n) as f64)])
            }
            "bit.rol" => {
                let inv = self.current_invocation_name.as_deref();
                let x = lua_value_to_u32_for_bitop(inv, 1, "rol", args.first())?;
                let n = lua_value_to_u32_for_bitop(inv, 2, "rol", args.get(1))? & 31;
                Ok(vec![LuaValue::Number(x.rotate_left(n) as i32 as f64)])
            }
            "bit.ror" => {
                let inv = self.current_invocation_name.as_deref();
                let x = lua_value_to_u32_for_bitop(inv, 1, "ror", args.first())?;
                let n = lua_value_to_u32_for_bitop(inv, 2, "ror", args.get(1))? & 31;
                Ok(vec![LuaValue::Number(x.rotate_right(n) as i32 as f64)])
            }
            "bit.bswap" => {
                let inv = self.current_invocation_name.as_deref();
                let x = lua_value_to_u32_for_bitop(inv, 1, "bswap", args.first())?;
                Ok(vec![LuaValue::Number(x.swap_bytes() as i32 as f64)])
            }
            "bit.tobit" => {
                let inv = self.current_invocation_name.as_deref();
                let x = lua_value_to_u32_for_bitop(inv, 1, "tobit", args.first())?;
                Ok(vec![LuaValue::Number(x as i32 as f64)])
            }
            "bit.tohex" => {
                let inv = self.current_invocation_name.as_deref();
                let x = lua_value_to_u32_for_bitop(inv, 1, "tohex", args.first())?;
                // Second arg (optional): digit count; negative = upper case.
                let n = match args.get(1) {
                    Some(v) => lua_value_to_u32_for_bitop(inv, 2, "tohex", Some(v))? as i32,
                    None => 8,
                };
                let abs_n = n.unsigned_abs().min(8) as usize;
                let s = if n < 0 {
                    format!("{x:0width$X}", width = abs_n)
                } else {
                    format!("{x:0width$x}", width = abs_n)
                };
                let trimmed: String = if s.len() > abs_n {
                    s.chars()
                        .rev()
                        .take(abs_n)
                        .collect::<String>()
                        .chars()
                        .rev()
                        .collect()
                } else {
                    s
                };
                Ok(vec![LuaValue::Str(trimmed.into_bytes())])
            }
            // ── cjson library ───────────────────────────────────────────
            "cjson.encode" => {
                // (frankenredis-u4mn6) Upstream lua_cjson.c::json_encode
                // requires exactly 1 arg; fr was treating missing arg
                // as nil and returning "null".
                // (frankenredis-yovmj) The same luaL_argcheck(L,
                // lua_gettop(L) == 1, ...) also rejects extra args.
                // fr previously silently ignored the surplus.
                let inv = self.current_invocation_name.as_deref();
                if args.len() != 1 {
                    return Err(lua_format_argerror(inv, "encode", 1, "expected 1 argument"));
                }
                let val = args.first().cloned().unwrap_or(LuaValue::Nil);
                // (frankenredis-bum6y) Upstream cjson raises via luaL_error,
                // which auto-prepends 'user_script:N: '. fr's wrap does not
                // add it for non-runtime errors, so do it explicitly here
                // to match vendored verbatim.
                // (frankenredis-u4mn6) luaL_error wording: drop the
                // user_script:1: prefix when invoked via pcall(C-builtin).
                let json = lua_value_to_json(&val).map_err(|e| {
                    if inv.is_some() {
                        format!("user_script:1: {e}")
                    } else {
                        e
                    }
                })?;
                Ok(vec![LuaValue::Str(json.into_bytes())])
            }
            "cjson.decode" => {
                // (frankenredis-pt4d4) Upstream lua_cjson.c::json_decode
                // calls luaL_argcheck(L, lua_gettop(L) == 1, 1, ...) and
                // requires arg #1 to be a string. fr was permissively
                // coercing nil to "" and returning nil silently.
                // (frankenredis-u4mn6) Route bad-argument errors through
                // lua_format_argerror for dual direct/pcall shape.
                let inv = self.current_invocation_name.as_deref();
                if args.is_empty() {
                    return Err(lua_format_argerror(inv, "decode", 1, "expected 1 argument"));
                }
                let data = match args.first() {
                    Some(LuaValue::Str(s)) => s.clone(),
                    Some(LuaValue::Number(n)) => {
                        if *n == (*n as i64) as f64 && n.is_finite() {
                            format!("{}", *n as i64).into_bytes()
                        } else {
                            lua_number_to_string(*n).into_bytes()
                        }
                    }
                    other => {
                        return Err(lua_format_argerror(
                            inv,
                            "decode",
                            1,
                            &format!(
                                "string expected, got {}",
                                other.map(|v| v.type_name()).unwrap_or("no value")
                            ),
                        ));
                    }
                };
                let s = String::from_utf8_lossy(&data).to_string();
                // (frankenredis-pt4d4) Reject an empty buffer the same
                // way upstream's json_next_token does.
                if s.trim().is_empty() {
                    return Err(
                        "user_script:1: Expected value but found T_END at character 1".to_string(),
                    );
                }
                // (frankenredis-9pvke) Map fr's generic `invalid JSON:`
                // fallback onto upstream lua_cjson's parse-failure shape
                // `Expected value but found invalid token at character N`.
                // The character offset is best-effort — fr's recursive
                // parser doesn't track byte position through nested
                // splits, so we use the offset of the first non-
                // whitespace byte in the original input.
                let val = match json_to_lua_value(&s) {
                    Ok(v) => v,
                    Err(e) => {
                        let mapped = if let Some(rest) = e.strip_prefix("invalid JSON: ") {
                            let offset = s
                                .as_bytes()
                                .windows(rest.len().min(s.len()))
                                .position(|w| w == rest.as_bytes())
                                .map(|p| p + 1)
                                .unwrap_or(1);
                            format!(
                                "user_script:1: Expected value but found invalid token at character {offset}"
                            )
                        } else if !e.starts_with("user_script:") {
                            format!("user_script:1: {e}")
                        } else {
                            e
                        };
                        return Err(mapped);
                    }
                };
                Ok(vec![val])
            }
            "cmsgpack.pack" => {
                let inv = self.current_invocation_name.as_deref();
                if args.is_empty() {
                    return Err(lua_format_argerror(
                        inv,
                        "pack",
                        0,
                        "MessagePack pack needs input.",
                    ));
                }
                let mut out = Vec::new();
                for value in args.iter() {
                    cmsgpack_pack_value(value, &mut out, 0).map_err(|e| {
                        if inv.is_some() {
                            format!("user_script:1: {e}")
                        } else {
                            e
                        }
                    })?;
                }
                Ok(vec![LuaValue::Str(out)])
            }
            "cmsgpack.unpack" => {
                let inv = self.current_invocation_name.as_deref();
                let data = lua_check_string(inv, args, 0, "unpack")?;
                cmsgpack_unpack_values(&data, 0, usize::MAX, false)
            }
            "cmsgpack.unpack_one" => {
                let inv = self.current_invocation_name.as_deref();
                let data = lua_check_string(inv, args, 0, "unpack_one")?;
                let offset = lua_optional_integer_arg(inv, 2, args.get(1), 0)?;
                cmsgpack_unpack_with_offset(&data, offset, 1)
            }
            "cmsgpack.unpack_limit" => {
                let inv = self.current_invocation_name.as_deref();
                let data = lua_check_string(inv, args, 0, "unpack_limit")?;
                let limit = match args.get(1) {
                    Some(value) => lua_required_integer_arg(inv, 2, value)?,
                    None => {
                        return Err(lua_bad_number_arg(inv, 2, None));
                    }
                };
                let offset = lua_optional_integer_arg(inv, 3, args.get(2), 0)?;
                cmsgpack_unpack_with_offset(&data, offset, limit)
            }
            "struct.pack" => {
                let inv = self.current_invocation_name.as_deref();
                let fmt = lua_check_string(inv, args, 0, "pack")?;
                lua_struct_pack(inv, &fmt, args)
            }
            "struct.unpack" => {
                let inv = self.current_invocation_name.as_deref();
                let fmt = lua_check_string(inv, args, 0, "unpack")?;
                let data = lua_check_string(inv, args, 1, "unpack")?;
                let pos = lua_optional_integer_arg(inv, 3, args.get(2), 1)?;
                lua_struct_unpack(inv, &fmt, &data, pos)
            }
            "struct.size" => {
                let inv = self.current_invocation_name.as_deref();
                let fmt = lua_check_string(inv, args, 0, "size")?;
                Ok(vec![LuaValue::Number(lua_struct_size(inv, &fmt)? as f64)])
            }
            // (frankenredis-u24vv) `_G` metatable handlers. These are
            // never user-callable directly; they fire when scripts
            // read/write missing keys on `_G`, mirroring the protected
            // env behavior vendored installs on the script's `_ENV`.
            "__fr_g_protected_index" => {
                let key = args.get(1).cloned().unwrap_or(LuaValue::Nil);
                let name_bytes = match &key {
                    LuaValue::Str(s) => s.clone(),
                    _ => return Ok(vec![LuaValue::Nil]),
                };
                let name = match std::str::from_utf8(&name_bytes) {
                    Ok(s) => s,
                    Err(_) => return Ok(vec![LuaValue::Nil]),
                };
                if let Some(val) = self.globals.get(name) {
                    Ok(vec![val.clone()])
                } else if self.globals_locked {
                    Err(format!(
                        "user_script:1: Script attempted to access nonexistent global variable '{name}'"
                    ))
                } else {
                    Ok(vec![LuaValue::Nil])
                }
            }
            "__fr_g_readonly_newindex" => {
                Err("user_script:1: Attempt to modify a readonly table".to_string())
            }
            _ => Err(format!("attempt to call unknown built-in '{name}'")),
        }
    }

    fn redis_call(&mut self, args: &[LuaValue], is_pcall: bool) -> Result<Vec<LuaValue>, String> {
        // (frankenredis-sj52g) Upstream's luaPushError prepends "ERR "
        // to the body of the error table that bubbles out of
        // luaRedisGenericCommand — both for the empty-args branch and
        // for the per-arg lua_tolstring rejection. For redis.pcall both
        // failure modes get packaged into `{err = "ERR …"}` rather than
        // bubbling up as a Lua error; redis.call lets them propagate.
        // fr previously returned bare error strings AND only packaged
        // dispatch errors (the `?` short-circuit on to_redis_arg
        // bypassed the is_pcall table form entirely).
        let arg_error = |msg: &str, is_pcall: bool| -> Result<Vec<LuaValue>, String> {
            let body = format!("ERR {msg}");
            if is_pcall {
                let t = LuaTable::new();
                t.set(
                    LuaValue::Str(b"err".to_vec()),
                    LuaValue::Str(body.into_bytes()),
                );
                Ok(vec![LuaValue::Table(t)])
            } else {
                Err(body)
            }
        };

        if args.is_empty() {
            // Upstream script_lua.c::luaRedisGenericCommand emits
            // 'Please specify at least one argument for this redis
            // lib call' when no command name is provided.
            // (br-frankenredis-replyargtype, frankenredis-sj52g)
            return arg_error(
                "Please specify at least one argument for this redis lib call",
                is_pcall,
            );
        }

        // Build argv for dispatch
        let mut argv: Vec<Vec<u8>> = Vec::new();
        for arg in args {
            match arg.to_redis_arg() {
                Ok(b) => argv.push(b),
                Err(msg) => return arg_error(&msg, is_pcall),
            }
        }

        let dirty_before = self.store.dirty;
        // (frankenredis-vr8rg) Dispatch the command with the script's RESP
        // version so handlers materialize RESP3 frames (Double/Map/Set/Null/
        // BigNumber) under `redis.setresp(3)`; restore the client's version
        // afterward so the script's own reply to the client is unaffected.
        let saved_resp_version = self.store.dispatch_client_ctx.resp_protocol_version;
        self.store.dispatch_client_ctx.resp_protocol_version = self.resp_version;
        let command_result = if let Some(intercepted) = script_command_intercept(&argv) {
            intercepted
        } else {
            match dispatch_argv(&argv, self.store, self.now_ms) {
                Ok(frame) => Ok(frame),
                Err(e) => {
                    let err_msg = match e.to_resp() {
                        RespFrame::Error(msg) => msg,
                        _ => format!("{e:?}"),
                    };
                    // Upstream script_lua.c::luaRedisGenericCommand →
                    // scriptVerifyCommandArity rewrites unknown-command
                    // and wrong-arity surfaces into script-context
                    // wording before bubbling up. The rewritten string
                    // keeps the 'ERR ' prefix so redis.pcall's .err
                    // table preserves it; the redis.call wrapper at
                    // lib.rs::script_error_with_context detects an
                    // existing prefix and avoids double-stamping.
                    // (br-frankenredis-fo1s, br-frankenredis-fdys,
                    // br-frankenredis-pcallerr)
                    Err(err_msg)
                }
            }
        };
        self.store.dispatch_client_ctx.resp_protocol_version = saved_resp_version;

        match command_result {
            Ok(frame) => {
                // (frankenredis-ax9ox) Mirror this inner command to MONITOR with
                // the `lua` address, like upstream call(). Recorded here (the
                // post-exec point, matching redis) and drained by the runtime
                // after the EVAL command's own monitor line; no-op when no
                // MONITOR clients are attached.
                self.store.record_script_monitor(&argv);
                let dirty_after = self.store.dirty;
                if dirty_after > dirty_before || command_may_propagate_from_script(&argv) {
                    // (frankenredis-x1225) Record the DETERMINISTIC effect form so
                    // a script's XADD `*` / SPOP / INCRBYFLOAT (etc.) propagates a
                    // concrete command, not the non-deterministic one — otherwise
                    // replicas/AOF replay regenerate ids/randomness and diverge.
                    let effect = crate::rewrite_effect_command_for_propagation(
                        &argv,
                        &frame,
                        self.store,
                        self.now_ms,
                    )
                    .unwrap_or_else(|| argv.clone());
                    self.store.record_script_propagation(&effect);
                }
                // (frankenredis-0czgc) redis.call must RAISE a command error reply
                // (aborting the script), not return it as a value; redis.pcall packages
                // it as {err=...}. fr-command returns ~160 command errors as
                // Ok(RespFrame::Error(..)) rather than Err, which previously let the
                // script CONTINUE past a failed redis.call AND dropped redis's
                // "script: <sha>" context suffix. Route Error reply-frames through the
                // same path as a dispatch Err so both behaviors match upstream.
                if let RespFrame::Error(msg) = &frame {
                    // (frankenredis-0czgc) redis resolves a container command's
                    // subcommand at the script command-lookup stage, so an
                    // unresolvable subcommand surfaces as "Unknown Redis command
                    // called from script" — not the command's own
                    // "unknown subcommand '<x>'. Try <CMD> HELP." reply (which is the
                    // byte-exact DIRECT wording). Rewrite uniformly for all containers.
                    let err_msg = script_context_rewrite_error(msg.clone());
                    return if is_pcall {
                        let t = LuaTable::new();
                        t.set(
                            LuaValue::Str(b"err".to_vec()),
                            LuaValue::Str(err_msg.into_bytes()),
                        );
                        Ok(vec![LuaValue::Table(t)])
                    } else {
                        Err(err_msg)
                    };
                }
                Ok(vec![resp_to_lua_command_result(
                    &argv,
                    &frame,
                    self.resp_version == 3,
                )])
            }
            Err(err_msg) => {
                // (frankenredis-0czgc) apply the script command-lookup rewrites
                // uniformly — intercepted paths (e.g. acl_script_result) produce the
                // raw command error, bypassing the dispatch-Err branch's rewrite.
                let err_msg = script_context_rewrite_error(err_msg);
                if is_pcall {
                    let t = LuaTable::new();
                    t.set(
                        LuaValue::Str(b"err".to_vec()),
                        LuaValue::Str(err_msg.into_bytes()),
                    );
                    Ok(vec![LuaValue::Table(t)])
                } else {
                    Err(err_msg)
                }
            }
        }
    }
}

/// (frankenredis-0czgc) Mirror redis's script command-lookup error rewrites that
/// happen before/around luaRedisGenericCommand: an unresolvable command OR container
/// subcommand becomes "Unknown Redis command called from script", and an arity failure
/// becomes "Wrong number of args calling Redis command from script". Applied uniformly
/// to every command error surfaced from redis.call/redis.pcall — whether it came from
/// dispatch (Err) or an intercepted path (acl_script_result, etc.) or a command reply
/// frame (Ok(Error)). The verbatim "unknown subcommand '<x>'. Try <CMD> HELP." /
/// "wrong number of arguments for '<cmd>'" wording is the DIRECT (non-script) reply.
/// Idempotent: the rewritten strings don't match the input prefixes.
fn script_context_rewrite_error(err_msg: String) -> String {
    if err_msg.starts_with("ERR unknown command ") || err_msg.starts_with("ERR unknown subcommand ")
    {
        "ERR Unknown Redis command called from script".to_string()
    } else if err_msg.starts_with("ERR wrong number of arguments")
        || err_msg.starts_with("ERR Unknown subcommand or wrong number of arguments")
    {
        "ERR Wrong number of args calling Redis command from script".to_string()
    } else {
        err_msg
    }
}

fn command_error_string(err: CommandError) -> String {
    match err.to_resp() {
        RespFrame::Error(msg) => msg,
        other => format!("{other:?}"),
    }
}

/// Mirror upstream script_lua.c::luaPushErrorBuff: derive a
/// `<CODE> <msg>` body for the {err=...} table that
/// luaReplyToRedisReply emits. Upstream prepends '-' to the raw
/// argument when missing, then splits "<CODE> <rest>" by the first
/// space — so any error_reply whose first word is followed by a
/// space gets that word as the code (regardless of whether the
/// caller supplied the leading '-'). If there is no space at all,
/// the code falls back to "ERR" with the entire body as the
/// message. Trailing CR/LF are trimmed. (br-frankenredis-xvlj)
fn lua_format_error_reply_payload(input: &[u8]) -> Vec<u8> {
    // Mirror script_lua.c::luaPushErrorBuff. The two-stage logic
    // matters because callers (like luaRedisErrorReplyCommand)
    // pre-pend '-' to user input before invoking it; the
    // 'else' arm here is reserved for callers that genuinely have no
    // RESP code (e.g. the C-side error-string wrappers).
    let body: &[u8] = if input.first() == Some(&b'-') {
        &input[1..]
    } else {
        input
    };
    let (code, msg): (&[u8], &[u8]) = match body.iter().position(|&b| b == b' ') {
        Some(idx) => (&body[..idx], &body[idx + 1..]),
        None => (b"ERR", body),
    };
    let mut out = Vec::with_capacity(code.len() + 1 + msg.len());
    out.extend_from_slice(code);
    out.push(b' ');
    out.extend_from_slice(msg);
    while matches!(out.last(), Some(b'\r' | b'\n')) {
        out.pop();
    }
    out
}

fn script_command_intercept(argv: &[Vec<u8>]) -> Option<Result<RespFrame, String>> {
    transaction_control_script_result(argv)
        .or_else(|| acl_script_result(argv))
        .or_else(|| auth_script_result(argv))
        .or_else(|| hello_script_result(argv))
        .or_else(|| sync_script_result(argv))
}

fn transaction_control_script_result(argv: &[Vec<u8>]) -> Option<Result<RespFrame, String>> {
    let command = argv.first()?;
    let wrong_arity = |name: &str| {
        Some(Err(format!(
            "ERR wrong number of arguments for '{}' command",
            name.to_ascii_lowercase()
        )))
    };

    if command.eq_ignore_ascii_case(b"MULTI") {
        if argv.len() != 1 {
            return wrong_arity("MULTI");
        }
        return Some(Err(SCRIPT_NOSCRIPT_ERROR.to_string()));
    }

    if command.eq_ignore_ascii_case(b"EXEC") {
        if argv.len() != 1 {
            return wrong_arity("EXEC");
        }
        return Some(Err(SCRIPT_NOSCRIPT_ERROR.to_string()));
    }

    if command.eq_ignore_ascii_case(b"DISCARD") {
        if argv.len() != 1 {
            return wrong_arity("DISCARD");
        }
        return Some(Err(SCRIPT_NOSCRIPT_ERROR.to_string()));
    }

    if command.eq_ignore_ascii_case(b"WATCH") {
        if argv.len() < 2 {
            return wrong_arity("WATCH");
        }
        return Some(Err(SCRIPT_NOSCRIPT_ERROR.to_string()));
    }

    if command.eq_ignore_ascii_case(b"UNWATCH") {
        if argv.len() != 1 {
            return wrong_arity("UNWATCH");
        }
        return Some(Err(SCRIPT_NOSCRIPT_ERROR.to_string()));
    }

    None
}

fn command_may_propagate_from_script(argv: &[Vec<u8>]) -> bool {
    let Some(command) = argv.first() else {
        return false;
    };
    command.eq_ignore_ascii_case(b"PUBLISH") || command.eq_ignore_ascii_case(b"SPUBLISH")
}

fn acl_script_result(argv: &[Vec<u8>]) -> Option<Result<RespFrame, String>> {
    let command = argv.first()?;
    if !command.eq_ignore_ascii_case(b"ACL") {
        return None;
    }

    if argv.len() < 2 {
        return Some(Err(command_error_string(CommandError::WrongArity("ACL"))));
    }

    let sub = match std::str::from_utf8(&argv[1]) {
        Ok(sub) => sub,
        Err(_) => return Some(Err(command_error_string(CommandError::InvalidUtf8Argument))),
    };

    let wrong_subcommand_arity = |subcommand: &str| {
        Err(command_error_string(CommandError::WrongSubcommandArity {
            command: "ACL",
            subcommand: subcommand.to_string(),
        }))
    };

    if sub.eq_ignore_ascii_case("WHOAMI")
        || sub.eq_ignore_ascii_case("LIST")
        || sub.eq_ignore_ascii_case("USERS")
        || sub.eq_ignore_ascii_case("SAVE")
        || sub.eq_ignore_ascii_case("LOAD")
    {
        if argv.len() != 2 {
            return Some(wrong_subcommand_arity(sub));
        }
        return Some(Err(SCRIPT_NOSCRIPT_ERROR.to_string()));
    }

    if sub.eq_ignore_ascii_case("SETUSER") || sub.eq_ignore_ascii_case("DELUSER") {
        if argv.len() < 3 {
            return Some(wrong_subcommand_arity(sub));
        }
        return Some(Err(SCRIPT_NOSCRIPT_ERROR.to_string()));
    }

    if sub.eq_ignore_ascii_case("GETUSER") {
        if argv.len() != 3 {
            return Some(wrong_subcommand_arity("GETUSER"));
        }
        return Some(Err(SCRIPT_NOSCRIPT_ERROR.to_string()));
    }

    if sub.eq_ignore_ascii_case("CAT") {
        if argv.len() != 2 && argv.len() != 3 {
            return Some(wrong_subcommand_arity("CAT"));
        }
        if argv.len() == 3 && std::str::from_utf8(&argv[2]).is_err() {
            return Some(Err(command_error_string(CommandError::InvalidUtf8Argument)));
        }
        return Some(Err(SCRIPT_NOSCRIPT_ERROR.to_string()));
    }

    if sub.eq_ignore_ascii_case("GENPASS") {
        if argv.len() == 3 {
            // Upstream acl.c::aclCommand parses GENPASS bits via
            // getLongFromObjectOrReply (NULL msg → "value is not an
            // integer or out of range") THEN range-checks with the
            // dedicated wording. fr-runtime's handle_acl_genpass already
            // splits the two; mirror that split here so the lua-call
            // path emits the same parse vs range distinction (noscript
            // still wins at the upstream layer, but fr's pre-noscript
            // validation should match its own non-script wording).
            // (frankenredis-genpassluasplit)
            match parse_i64_arg(&argv[2]) {
                Ok(bits) if bits > 0 && bits <= 4096 => {}
                Ok(_) => {
                    return Some(Err(
                        "ERR ACL GENPASS argument must be the number of bits for the output password, a positive number up to 4096"
                            .to_string(),
                    ));
                }
                Err(_) => {
                    return Some(Err(command_error_string(CommandError::InvalidInteger)));
                }
            }
        } else if argv.len() != 2 {
            return Some(wrong_subcommand_arity("GENPASS"));
        }
        return Some(Err(SCRIPT_NOSCRIPT_ERROR.to_string()));
    }

    if sub.eq_ignore_ascii_case("LOG") {
        if argv.len() == 3 {
            if !argv[2].eq_ignore_ascii_case(b"RESET") {
                match parse_i64_arg(&argv[2]) {
                    Ok(count) if count >= 0 => {}
                    _ => return Some(Err(command_error_string(CommandError::InvalidInteger))),
                }
            }
        } else if argv.len() != 2 {
            return Some(wrong_subcommand_arity("LOG"));
        }
        return Some(Err(SCRIPT_NOSCRIPT_ERROR.to_string()));
    }

    if sub.eq_ignore_ascii_case("DRYRUN") {
        if argv.len() < 4 {
            return Some(wrong_subcommand_arity("DRYRUN"));
        }
        return Some(Err(SCRIPT_NOSCRIPT_ERROR.to_string()));
    }

    if sub.eq_ignore_ascii_case("HELP") {
        if argv.len() != 2 {
            return Some(wrong_subcommand_arity("HELP"));
        }
        return Some(Ok(acl_help_frame()));
    }

    Some(Err(command_error_string(CommandError::UnknownSubcommand {
        command: "ACL",
        subcommand: sub.to_string(),
    })))
}

fn acl_help_frame() -> RespFrame {
    // (frankenredis-sebba) Mirror the wire-side ACL HELP in
    // fr-runtime/src/lib.rs:7327 — 29 entries (header + 26 from
    // upstream acl.c::ACL_CMD_HELP + 2-entry footer) using
    // SimpleString frames (addReplyStatus in upstream).
    // Previously this surfaced 28 entries with paraphrased wording
    // and BulkString frames, so `redis.call('ACL','HELP')` from a
    // script returned different content than wire ACL HELP — and a
    // different RESP shape (SimpleString -> Lua {ok=...} table vs
    // BulkString -> plain string). Source of truth is upstream
    // acl.c:3047-3074 + networking.c::addReplyHelp.
    let status = |s: &str| RespFrame::SimpleString(s.to_string());
    RespFrame::Array(Some(vec![
        status("ACL <subcommand> [<arg> [value] [opt] ...]. Subcommands are:"),
        status("CAT [<category>]"),
        status("    List all commands that belong to <category>, or all command categories"),
        status("    when no category is specified."),
        status("DELUSER <username> [<username> ...]"),
        status("    Delete a list of users."),
        status("DRYRUN <username> <command> [<arg> ...]"),
        status(
            "    Returns whether the user can execute the given command without executing the command.",
        ),
        status("GETUSER <username>"),
        status("    Get the user's details."),
        status("GENPASS [<bits>]"),
        status("    Generate a secure 256-bit user password. The optional `bits` argument can"),
        status("    be used to specify a different size."),
        status("LIST"),
        status("    Show users details in config file format."),
        status("LOAD"),
        status("    Reload users from the ACL file."),
        status("LOG [<count> | RESET]"),
        status("    Show the ACL log entries."),
        status("SAVE"),
        status("    Save the current config to the ACL file."),
        status("SETUSER <username> <attribute> [<attribute> ...]"),
        status("    Create or modify a user with the specified attributes."),
        status("USERS"),
        status("    List all the registered usernames."),
        status("WHOAMI"),
        status("    Return the current connection username."),
        status("HELP"),
        status("    Print this help."),
    ]))
}

fn auth_script_result(argv: &[Vec<u8>]) -> Option<Result<RespFrame, String>> {
    let command = argv.first()?;
    if !command.eq_ignore_ascii_case(b"AUTH") {
        return None;
    }

    if argv.len() != 2 && argv.len() != 3 {
        return Some(Err(command_error_string(CommandError::WrongArity("AUTH"))));
    }

    Some(Err(SCRIPT_NOSCRIPT_ERROR.to_string()))
}

fn hello_script_result(argv: &[Vec<u8>]) -> Option<Result<RespFrame, String>> {
    let command = argv.first()?;
    if !command.eq_ignore_ascii_case(b"HELLO") {
        return None;
    }

    if argv.len() == 1 {
        return Some(Err(SCRIPT_NOSCRIPT_ERROR.to_string()));
    }

    let protocol_version = match parse_i64_arg(&argv[1]) {
        Ok(version) => version,
        Err(err) => return Some(Err(command_error_string(err))),
    };

    if protocol_version != 2 && protocol_version != 3 {
        return Some(Err(format!(
            "NOPROTO unsupported protocol version '{}'",
            protocol_version
        )));
    }

    let mut options = argv[2..].iter();
    while let Some(option_arg) = options.next() {
        let option = match std::str::from_utf8(option_arg) {
            Ok(option) => option,
            Err(_) => return Some(Err(command_error_string(CommandError::InvalidUtf8Argument))),
        };
        if option.eq_ignore_ascii_case("AUTH") {
            if options.next().is_none() || options.next().is_none() {
                return Some(Err(command_error_string(CommandError::SyntaxError)));
            }
            continue;
        }
        if option.eq_ignore_ascii_case("SETNAME") {
            let Some(name) = options.next() else {
                return Some(Err(command_error_string(CommandError::SyntaxError)));
            };
            if !hello_client_name_is_valid(name) {
                return Some(Err(
                    "ERR Client names cannot contain spaces, newlines or special characters."
                        .to_string(),
                ));
            }
            continue;
        }
        return Some(Err(command_error_string(CommandError::SyntaxError)));
    }

    Some(Err(SCRIPT_NOSCRIPT_ERROR.to_string()))
}

fn hello_client_name_is_valid(name: &[u8]) -> bool {
    name.iter().all(|&b| b > b' ')
}

fn sync_script_result(argv: &[Vec<u8>]) -> Option<Result<RespFrame, String>> {
    let command = argv.first()?;
    if !command.eq_ignore_ascii_case(b"SYNC") {
        return None;
    }

    if argv.len() != 1 {
        return Some(Err(command_error_string(CommandError::WrongArity("SYNC"))));
    }

    Some(Err(SCRIPT_NOSCRIPT_ERROR.to_string()))
}

// ── Lua pattern matching engine ─────────────────────────────────────────
//
// Implements Lua 5.1 pattern matching: character classes (%a, %d, etc.),
// quantifiers (*, +, -, ?), anchors (^, $), captures, and character sets.

/// Result of a successful pattern match.
struct LuaPatMatch {
    start: usize, // 0-indexed byte offset of match start
    end: usize,   // 0-indexed exclusive end of match
    captures: Vec<LuaCapture>,
}

enum LuaCapture {
    Substring(usize, Option<usize>), // start, end (0-indexed, exclusive end; None = open/unclosed)
    Position(usize),                 // position capture from ()
}

/// Check if byte matches a Lua character class letter (the char after %).
fn lua_class_match(class: u8, ch: u8) -> bool {
    match class {
        b'a' => ch.is_ascii_alphabetic(),
        b'A' => !ch.is_ascii_alphabetic(),
        b'd' => ch.is_ascii_digit(),
        b'D' => !ch.is_ascii_digit(),
        b'l' => ch.is_ascii_lowercase(),
        b'L' => !ch.is_ascii_lowercase(),
        b'u' => ch.is_ascii_uppercase(),
        b'U' => !ch.is_ascii_uppercase(),
        b'w' => ch.is_ascii_alphanumeric(),
        b'W' => !ch.is_ascii_alphanumeric(),
        b's' => ch.is_ascii_whitespace(),
        b'S' => !ch.is_ascii_whitespace(),
        b'p' => ch.is_ascii_punctuation(),
        b'P' => !ch.is_ascii_punctuation(),
        b'c' => ch.is_ascii_control(),
        b'C' => !ch.is_ascii_control(),
        b'x' => ch.is_ascii_hexdigit(),
        b'X' => !ch.is_ascii_hexdigit(),
        _ => ch == class, // %% matches %, %( matches (, etc.
    }
}

/// Check if a byte matches a single pattern element at position `pi` in pattern.
/// Returns the number of pattern bytes consumed.
fn lua_single_match(pat: &[u8], pi: usize, ch: u8) -> bool {
    if pi >= pat.len() {
        return false;
    }
    match pat[pi] {
        b'.' => true,
        b'%' => {
            if pi + 1 < pat.len() {
                lua_class_match(pat[pi + 1], ch)
            } else {
                false
            }
        }
        b'[' => lua_set_match(pat, pi, ch),
        c => c == ch,
    }
}

/// How many pattern bytes does a single element consume?
/// Validate a Lua pattern eagerly, returning the upstream wording when
/// the pattern is malformed. Mirrors lstrlib.c's two-class of errors:
///   - "malformed pattern (ends with '%')" — trailing `%` without a
///     following class char.
///   - "malformed pattern (missing ']')" — `[...]` set whose closing
///     `]` is absent (respecting `]` immediately after `[` or `[^` as
///     literal, just like upstream singlematchclass).
///
/// (frankenredis-vfv8s)
/// Validates a Lua match pattern eagerly and routes the upstream
/// `<source>:<line>: ` prefix through `inv_name`:
///   - `Some("...")` → prepend "user_script:1: " (named/direct-call shape)
///   - `None`        → no prefix (anonymous pcall(C-builtin) shape)
///
/// (frankenredis-uqnq6) Matches luaL_error's behavior: luaL_where(L,1) is
/// "" when the caller of the C-builtin is itself a C frame (pcall).
fn lua_pattern_validate_named(inv_name: Option<&str>, pat: &[u8]) -> Result<(), String> {
    let prefix = if inv_name.is_some() {
        "user_script:1: "
    } else {
        ""
    };
    let err = |msg: &str| -> String { format!("{prefix}{msg}") };
    let mut i = 0;
    // (frankenredis-skwin) Capture-balance: each '(' opens a capture
    // slot; ')' closes the innermost; the engine errors at match time
    // on either an unclosed group ("unfinished capture") or an extra
    // ')' ("invalid pattern capture"). Track depth here so malformed
    // captures are rejected eagerly with the upstream wording.
    let mut open_captures: i32 = 0;
    while i < pat.len() {
        match pat[i] {
            b'%' => {
                if i + 1 >= pat.len() {
                    return Err(err("malformed pattern (ends with '%')"));
                }
                // (frankenredis-3zxc1) Upstream lstrlib.c::do_match raises
                // luaL_error("missing '[' after '%%f' in pattern") when %f
                // is not immediately followed by a character class. Advance
                // only to the '[' so the next loop iteration validates the
                // [set] body just like any other bracket expression.
                if pat[i + 1] == b'f' {
                    if i + 2 >= pat.len() || pat[i + 2] != b'[' {
                        return Err(err("missing '[' after '%f' in pattern"));
                    }
                    i += 2;
                    continue;
                }
                // (frankenredis-skwin) %bxy consumes 4 pattern bytes
                // (%, b, open, close). Without this case the next loop
                // iteration would see the open char as the start of a
                // new pattern element — e.g. %b() would treat '(' as a
                // capture open, '%b[' as a set, etc.
                if pat[i + 1] == b'b' {
                    // (frankenredis-skwin) Redis-vendored Lua emits
                    // "unbalanced pattern" when %b lacks the 2-char
                    // open/close argument suffix.
                    if i + 3 >= pat.len() {
                        return Err(err("unbalanced pattern"));
                    }
                    i += 4;
                    continue;
                }
                i += 2;
            }
            b'[' => {
                let mut j = i + 1;
                if j < pat.len() && pat[j] == b'^' {
                    j += 1;
                }
                // Upstream: a `]` immediately after `[` or `[^` is treated as
                // literal, so we must advance past it before searching for the
                // real closing bracket.
                if j < pat.len() && pat[j] == b']' {
                    j += 1;
                }
                let mut closed = false;
                while j < pat.len() {
                    if pat[j] == b'%' && j + 1 < pat.len() {
                        j += 2;
                        continue;
                    }
                    if pat[j] == b']' {
                        closed = true;
                        j += 1;
                        break;
                    }
                    j += 1;
                }
                if !closed {
                    return Err(err("malformed pattern (missing ']')"));
                }
                i = j;
            }
            b'(' => {
                open_captures += 1;
                i += 1;
            }
            b')' => {
                open_captures -= 1;
                if open_captures < 0 {
                    return Err(err("invalid pattern capture"));
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    if open_captures > 0 {
        return Err(err("unfinished capture"));
    }
    Ok(())
}

fn lua_pattern_element_len(pat: &[u8], pi: usize) -> usize {
    if pi >= pat.len() {
        return 0;
    }
    match pat[pi] {
        // (frankenredis-skwin) %bxy is a 4-byte pattern element.
        b'%' if pi + 1 < pat.len() && pat[pi + 1] == b'b' && pi + 3 < pat.len() => 4,
        b'%' if pi + 1 < pat.len() => 2,
        b'%' => 1,
        b'[' => {
            // Find closing ]
            let mut j = pi + 1;
            if j < pat.len() && pat[j] == b'^' {
                j += 1;
            }
            if j < pat.len() && pat[j] == b']' {
                j += 1; // ] right after [ or [^ is literal
            }
            while j < pat.len() && pat[j] != b']' {
                if pat[j] == b'%' {
                    j += 1; // skip escaped char
                }
                j += 1;
            }
            if j < pat.len() {
                j + 1 - pi // include the ]
            } else {
                pat.len() - pi
            }
        }
        _ => 1,
    }
}

/// Check if `ch` matches a [...] set starting at pat[pi].
fn lua_set_match(pat: &[u8], pi: usize, ch: u8) -> bool {
    let mut j = pi + 1; // skip [
    let negate = j < pat.len() && pat[j] == b'^';
    if negate {
        j += 1;
    }
    // ] right after [ or [^ is literal
    if j < pat.len() && pat[j] == b']' {
        if ch == b']' {
            return !negate;
        }
        j += 1;
    }
    let mut matched = false;
    while j < pat.len() && pat[j] != b']' {
        if pat[j] == b'%' && j + 1 < pat.len() {
            if lua_class_match(pat[j + 1], ch) {
                matched = true;
            }
            j += 2;
        } else if j + 2 < pat.len() && pat[j + 1] == b'-' && pat[j + 2] != b']' {
            // Range: a-z
            if ch >= pat[j] && ch <= pat[j + 2] {
                matched = true;
            }
            j += 3;
        } else {
            if pat[j] == ch {
                matched = true;
            }
            j += 1;
        }
    }
    if negate { !matched } else { matched }
}

/// Core recursive pattern matcher.
/// Returns the end position (exclusive) of the match on success.
fn lua_pat_match(
    s: &[u8],
    si: usize,
    pat: &[u8],
    pi: usize,
    captures: &mut Vec<LuaCapture>,
    depth: usize,
) -> Option<usize> {
    if depth > 200 {
        return None; // prevent stack overflow
    }
    if pi >= pat.len() {
        return Some(si);
    }

    // Handle captures: (
    if pat[pi] == b'(' {
        if pi + 1 < pat.len() && pat[pi + 1] == b')' {
            // Position capture
            let cap_idx = captures.len();
            captures.push(LuaCapture::Position(si));
            if let Some(end) = lua_pat_match(s, si, pat, pi + 2, captures, depth + 1) {
                return Some(end);
            }
            captures.truncate(cap_idx);
            return None;
        }
        // Start substring capture
        let cap_idx = captures.len();
        captures.push(LuaCapture::Substring(si, None)); // open capture
        if let Some(end) = lua_pat_match(s, si, pat, pi + 1, captures, depth + 1) {
            return Some(end);
        }
        captures.truncate(cap_idx);
        return None;
    }

    // Handle capture close: )
    if pat[pi] == b')' {
        // Find the last open capture and close it
        for i in (0..captures.len()).rev() {
            if let LuaCapture::Substring(start, None) = captures[i] {
                captures[i] = LuaCapture::Substring(start, Some(si));
                if let Some(end) = lua_pat_match(s, si, pat, pi + 1, captures, depth + 1) {
                    return Some(end);
                }
                captures[i] = LuaCapture::Substring(start, None); // restore
                return None;
            }
        }
        return None; // unmatched close paren
    }

    // Handle $ anchor at end of pattern
    if pat[pi] == b'$' && pi + 1 == pat.len() {
        return if si == s.len() { Some(si) } else { None };
    }

    // (frankenredis-53u08) Handle %N back-references (N = 1..9):
    // upstream lstrlib.c::match_capture compares the N-th captured
    // substring's bytes against s starting at si. The capture must
    // already be closed (Substring(start, Some(end)) or Position).
    // fr previously fell through to lua_single_match → lua_class_match
    // which treated '%1' as the literal char '1'.
    if pi + 1 < pat.len() && pat[pi] == b'%' && (b'1'..=b'9').contains(&pat[pi + 1]) {
        let cap_idx = (pat[pi + 1] - b'0') as usize - 1;
        // Out-of-range or unclosed → no match (upstream raises
        // "invalid capture index" at compile-ish time, but for the
        // common case we just fail the match here).
        let cap_bytes: Vec<u8> = match captures.get(cap_idx) {
            Some(LuaCapture::Substring(start, Some(end))) => s[*start..*end].to_vec(),
            Some(LuaCapture::Position(pos)) => format!("{}", pos + 1).into_bytes(),
            _ => return None,
        };
        let end = si + cap_bytes.len();
        if end > s.len() || &s[si..end] != cap_bytes.as_slice() {
            return None;
        }
        return lua_pat_match(s, end, pat, pi + 2, captures, depth + 1);
    }

    // (frankenredis-3zxc1) Handle %f[set] frontier matcher — a zero-width
    // assertion that matches the empty string at a position where the
    // previous byte does NOT match [set] but the current byte DOES.
    // Mirrors lstrlib.c::do_match's L_ESC + 'f' branch. The pre-loop
    // validator (lua_pattern_validate) guarantees pat[pi+2] == '['.
    if pi + 2 < pat.len() && pat[pi] == b'%' && pat[pi + 1] == b'f' && pat[pi + 2] == b'[' {
        let set_len = lua_pattern_element_len(pat, pi + 2);
        // Upstream treats s[-1] as '\0' at the start of the string, and
        // s[len] as '\0' off the end (matching the C convention).
        let prev = if si == 0 { 0u8 } else { s[si - 1] };
        let cur = if si < s.len() { s[si] } else { 0u8 };
        if !lua_set_match(pat, pi + 2, prev) && lua_set_match(pat, pi + 2, cur) {
            return lua_pat_match(s, si, pat, pi + 2 + set_len, captures, depth + 1);
        }
        return None;
    }

    // (frankenredis-skwin) Handle %bxy balanced match. Starting at the
    // current string position, advance through paired open/close chars
    // tracking nesting depth; on success continue matching with the
    // string position past the close char and the pattern past the
    // 4-byte %bxy element. Mirrors lstrlib.c::matchbalance.
    if pi + 3 < pat.len() && pat[pi] == b'%' && pat[pi + 1] == b'b' {
        let open_ch = pat[pi + 2];
        let close_ch = pat[pi + 3];
        if si >= s.len() || s[si] != open_ch {
            return None;
        }
        let mut depth_b: i32 = 1;
        let mut j = si + 1;
        while j < s.len() {
            // For the degenerate case where open == close (e.g. %bxx),
            // every match closes the outermost bracket.
            if s[j] == close_ch {
                depth_b -= 1;
                if depth_b == 0 {
                    return lua_pat_match(s, j + 1, pat, pi + 4, captures, depth + 1);
                }
            } else if s[j] == open_ch {
                depth_b += 1;
            }
            j += 1;
        }
        return None;
    }

    let elem_len = lua_pattern_element_len(pat, pi);
    let after_elem = pi + elem_len;

    // Check for quantifier after element
    if after_elem < pat.len() {
        match pat[after_elem] {
            b'*' => {
                // Greedy 0+
                return lua_pat_greedy(s, si, pat, pi, after_elem + 1, captures, depth);
            }
            b'+' => {
                // Greedy 1+
                if si < s.len() && lua_single_match(pat, pi, s[si]) {
                    return lua_pat_greedy(s, si + 1, pat, pi, after_elem + 1, captures, depth);
                }
                return None;
            }
            b'-' => {
                // Lazy 0+
                return lua_pat_lazy(s, si, pat, pi, after_elem + 1, captures, depth);
            }
            b'?' => {
                // Optional
                if si < s.len()
                    && lua_single_match(pat, pi, s[si])
                    && let Some(end) =
                        lua_pat_match(s, si + 1, pat, after_elem + 1, captures, depth + 1)
                {
                    return Some(end);
                }
                return lua_pat_match(s, si, pat, after_elem + 1, captures, depth + 1);
            }
            _ => {}
        }
    }

    // No quantifier: match single element
    if si < s.len() && lua_single_match(pat, pi, s[si]) {
        return lua_pat_match(s, si + 1, pat, after_elem, captures, depth + 1);
    }

    None
}

/// Greedy quantifier: match as many as possible, then backtrack.
fn lua_pat_greedy(
    s: &[u8],
    si: usize,
    pat: &[u8],
    elem_pi: usize,
    rest_pi: usize,
    captures: &mut Vec<LuaCapture>,
    depth: usize,
) -> Option<usize> {
    let mut count = 0;
    while si + count < s.len() && lua_single_match(pat, elem_pi, s[si + count]) {
        count += 1;
    }
    // Try from longest match down
    loop {
        if let Some(end) = lua_pat_match(s, si + count, pat, rest_pi, captures, depth + 1) {
            return Some(end);
        }
        if count == 0 {
            break;
        }
        count -= 1;
    }
    None
}

/// Lazy quantifier: match as few as possible, then try rest.
fn lua_pat_lazy(
    s: &[u8],
    si: usize,
    pat: &[u8],
    elem_pi: usize,
    rest_pi: usize,
    captures: &mut Vec<LuaCapture>,
    depth: usize,
) -> Option<usize> {
    let mut pos = si;
    loop {
        if let Some(end) = lua_pat_match(s, pos, pat, rest_pi, captures, depth + 1) {
            return Some(end);
        }
        if pos < s.len() && lua_single_match(pat, elem_pi, s[pos]) {
            pos += 1;
        } else {
            return None;
        }
    }
}

/// Top-level pattern match: try matching pattern at each position starting from `init`.
/// If pattern starts with ^, only try at `init`.
fn lua_pattern_find(s: &[u8], pat: &[u8], init: usize) -> Option<LuaPatMatch> {
    let (anchored, pat_start) = if !pat.is_empty() && pat[0] == b'^' {
        (true, 1)
    } else {
        (false, 0)
    };

    if anchored {
        let mut captures = Vec::new();
        if let Some(end) = lua_pat_match(s, init, pat, pat_start, &mut captures, 0) {
            return Some(LuaPatMatch {
                start: init,
                end,
                captures,
            });
        }
        return None;
    }

    for start in init..=s.len() {
        let mut captures = Vec::new();
        if let Some(end) = lua_pat_match(s, start, pat, pat_start, &mut captures, 0) {
            return Some(LuaPatMatch {
                start,
                end,
                captures,
            });
        }
    }
    None
}

/// Extract capture values from a match. If no explicit captures, return the whole match.
fn lua_match_captures(s: &[u8], m: &LuaPatMatch) -> Vec<LuaValue> {
    if m.captures.is_empty() {
        return vec![LuaValue::Str(s[m.start..m.end].to_vec())];
    }
    m.captures
        .iter()
        .map(|cap| match cap {
            LuaCapture::Substring(start, Some(end)) => LuaValue::Str(s[*start..*end].to_vec()),
            LuaCapture::Substring(_, None) => LuaValue::Nil,
            LuaCapture::Position(pos) => LuaValue::Number(*pos as f64 + 1.0), // 1-indexed
        })
        .collect()
}

/// Apply gsub replacement for one match. Handles string replacements with %0-%9.
/// (frankenredis-76y97) Build the lookup key for table-style
/// gsub replacement. Uses the 1st capture if present, otherwise the
/// whole match. Mirrors Lua 5.1's lstrlib.c::str_gsub when typ == LUA_TTABLE.
fn lua_gsub_capture_key(s: &[u8], m: &LuaPatMatch) -> Vec<u8> {
    if m.captures.is_empty() {
        s[m.start..m.end].to_vec()
    } else {
        match &m.captures[0] {
            LuaCapture::Substring(cs, Some(ce)) => s[*cs..*ce].to_vec(),
            LuaCapture::Substring(cs, None) => s[*cs..].to_vec(),
            LuaCapture::Position(pos) => format!("{}", pos + 1).into_bytes(),
        }
    }
}

/// (frankenredis-76y97) Build the positional args passed to a
/// function-style gsub replacement: every capture as a Lua value, or
/// the whole match when there are no captures. Position captures
/// become numbers; substring captures become strings.
fn lua_gsub_capture_args(s: &[u8], m: &LuaPatMatch) -> Vec<LuaValue> {
    if m.captures.is_empty() {
        vec![LuaValue::Str(s[m.start..m.end].to_vec())]
    } else {
        m.captures
            .iter()
            .map(|c| match c {
                LuaCapture::Substring(cs, Some(ce)) => LuaValue::Str(s[*cs..*ce].to_vec()),
                LuaCapture::Substring(cs, None) => LuaValue::Str(s[*cs..].to_vec()),
                LuaCapture::Position(pos) => LuaValue::Number((*pos + 1) as f64),
            })
            .collect()
    }
}

/// (frankenredis-76y97) Normalise a table-lookup or function-return
/// value to its replacement bytes per Lua 5.1 spec: nil/false leaves
/// the match unchanged, strings/numbers are coerced as expected,
/// other types raise the standard "invalid replacement value" error.
fn lua_gsub_normalise_repl(s: &[u8], m: &LuaPatMatch, val: &LuaValue) -> Result<Vec<u8>, String> {
    match val {
        LuaValue::Nil | LuaValue::Bool(false) => Ok(s[m.start..m.end].to_vec()),
        LuaValue::Bool(true) => {
            Err("user_script:1: invalid replacement value (a boolean)".to_string())
        }
        LuaValue::Str(b) => Ok(b.clone()),
        LuaValue::Number(n) => {
            if *n == (*n as i64) as f64 && n.is_finite() {
                Ok(format!("{}", *n as i64).into_bytes())
            } else {
                Ok(lua_number_to_string(*n).into_bytes())
            }
        }
        _ => Err(format!(
            "user_script:1: invalid replacement value (a {})",
            val.type_name()
        )),
    }
}

fn lua_gsub_replace(s: &[u8], m: &LuaPatMatch, repl: &[u8]) -> Result<Vec<u8>, String> {
    let mut result = Vec::new();
    let mut i = 0;
    while i < repl.len() {
        if repl[i] == b'%' && i + 1 < repl.len() {
            let next = repl[i + 1];
            if next.is_ascii_digit() {
                let idx = (next - b'0') as usize;
                if idx == 0 {
                    // %0 = whole match
                    result.extend_from_slice(&s[m.start..m.end]);
                } else if idx <= m.captures.len() {
                    match &m.captures[idx - 1] {
                        LuaCapture::Substring(cs, Some(ce)) => {
                            result.extend_from_slice(&s[*cs..*ce]);
                        }
                        LuaCapture::Substring(_, None) => {}
                        LuaCapture::Position(pos) => {
                            result.extend_from_slice(format!("{}", pos + 1).as_bytes());
                        }
                    }
                } else if idx == 1 && m.captures.is_empty() {
                    // (frankenredis-127za) Upstream lstrlib.c
                    // ::push_onecapture has a documented special case:
                    // when `i == 0` (i.e. %1) AND `ms->level == 0`
                    // (no captures in the pattern), it pushes the
                    // whole match instead of erroring. So `%1` is the
                    // implicit "whole match" reference for a
                    // capture-less pattern, just like `%0`.
                    result.extend_from_slice(&s[m.start..m.end]);
                } else {
                    // (frankenredis-127za) Upstream lstrlib.c::add_s
                    // raises luaL_error("invalid capture index") when
                    // %N references a capture index higher than the
                    // pattern's capture count. fr previously silently
                    // skipped the reference, producing surprising
                    // empty-match output (e.g. gsub('abc','(.)','%5')
                    // would erase every byte instead of erroring).
                    return Err("user_script:1: invalid capture index".to_string());
                }
                i += 2;
            } else if next == b'%' {
                result.push(b'%');
                i += 2;
            } else {
                // (frankenredis-6oe9g) Upstream lstrlib.c::add_s emits
                // the literal char after % for any non-digit non-'%'
                // suffix (e.g. '%w' -> 'w'). fr previously pushed
                // both '%' and the following char, leaking '%w'.
                result.push(next);
                i += 2;
            }
        } else {
            result.push(repl[i]);
            i += 1;
        }
    }
    Ok(result)
}

// ── Type conversions ────────────────────────────────────────────────────

fn resp_to_lua_command_result(argv: &[Vec<u8>], frame: &RespFrame, resp3: bool) -> LuaValue {
    if config_get_returns_map_in_lua(argv)
        && let Some(table) = config_get_resp_to_lua_map(frame, resp3)
    {
        return LuaValue::Table(table);
    }
    resp_to_lua(frame, resp3)
}

fn config_get_returns_map_in_lua(argv: &[Vec<u8>]) -> bool {
    argv.len() >= 2
        && argv[0].eq_ignore_ascii_case(b"CONFIG")
        && argv[1].eq_ignore_ascii_case(b"GET")
}

fn config_get_resp_to_lua_map(frame: &RespFrame, resp3: bool) -> Option<LuaTable> {
    let items = match frame {
        RespFrame::Array(Some(items)) | RespFrame::Sequence(items) => items,
        RespFrame::Array(None) => return Some(LuaTable::new()),
        _ => return None,
    };

    if items.len() % 2 != 0 {
        return None;
    }

    let table = LuaTable::new();
    for chunk in items.chunks_exact(2) {
        let key = match &chunk[0] {
            RespFrame::BulkString(Some(bytes)) => bytes.clone(),
            RespFrame::SimpleString(text) => text.as_bytes().to_vec(),
            _ => return None,
        };
        table.set(LuaValue::Str(key), resp_to_lua(&chunk[1], resp3));
    }

    Some(table)
}

/// Convert a `redis.call` reply frame to a Lua value, mirroring upstream
/// script_lua.c::redisProtocolToLuaType. `resp3` selects the conversion table:
/// it is `true` only after `redis.setresp(3)`, where the dispatched command also
/// materializes RESP3 frames. The RESP2 path (default) keeps the historical
/// behavior — a null is Lua `false`, and any RESP3 frame that slips through is
/// rendered in its flattened RESP2-callsite form.
fn resp_to_lua(frame: &RespFrame, resp3: bool) -> LuaValue {
    // Null sentinel: RESP2 → Lua false; RESP3 → Lua nil. (frankenredis-vr8rg)
    let null = || {
        if resp3 {
            LuaValue::Nil
        } else {
            LuaValue::Bool(false)
        }
    };
    match frame {
        // RESP3 Boolean → Lua boolean (upstream redisProtocolToLuaType_Bool).
        // (frankenredis-0gz4g)
        RespFrame::Bool(b) => LuaValue::Bool(*b),
        // RESP3 Attribute → its metadata pairs as a table, mirroring the Map
        // conversion. No 7.2 command returns an attribute reply through
        // redis.call, so this is a defensive arm. (frankenredis-01weh)
        RespFrame::Attribute(pairs) => resp_to_lua(&RespFrame::Map(Some(pairs.clone())), resp3),
        RespFrame::SimpleString(s) => {
            let t = LuaTable::new();
            t.set(
                LuaValue::Str(b"ok".to_vec()),
                LuaValue::Str(s.as_bytes().to_vec()),
            );
            LuaValue::Table(t)
        }
        RespFrame::Error(s) => {
            let t = LuaTable::new();
            t.set(
                LuaValue::Str(b"err".to_vec()),
                LuaValue::Str(s.as_bytes().to_vec()),
            );
            LuaValue::Table(t)
        }
        RespFrame::Integer(n) => LuaValue::Number(*n as f64),
        RespFrame::BulkString(None) => null(),
        RespFrame::BulkString(Some(data)) => LuaValue::Str(data.clone()),
        RespFrame::Array(None) => null(),
        RespFrame::Array(Some(items)) | RespFrame::Push(items) | RespFrame::Sequence(items) => {
            let t = LuaTable::new();
            for (i, item) in items.iter().enumerate() {
                t.set(LuaValue::Number((i + 1) as f64), resp_to_lua(item, resp3));
            }
            LuaValue::Table(t)
        }
        RespFrame::Map(None) => null(),
        RespFrame::Map(Some(pairs)) => {
            if resp3 {
                // Upstream redisProtocolToLuaType_Map wraps the map in a
                // `{map = {k = v, …}}` table under RESP3. (frankenredis-vr8rg)
                let inner = LuaTable::new();
                for (k, v) in pairs.iter() {
                    inner.set(resp_to_lua(k, resp3), resp_to_lua(v, resp3));
                }
                let outer = LuaTable::new();
                outer.set(LuaValue::Str(b"map".to_vec()), LuaValue::Table(inner));
                LuaValue::Table(outer)
            } else {
                // RESP2 Lua callsite: flatten to an alternating k/v array.
                // (br-frankenredis-r80v / r72v)
                let t = LuaTable::new();
                for (i, (k, v)) in pairs.iter().enumerate() {
                    t.set(LuaValue::Number((2 * i + 1) as f64), resp_to_lua(k, resp3));
                    t.set(LuaValue::Number((2 * i + 2) as f64), resp_to_lua(v, resp3));
                }
                LuaValue::Table(t)
            }
        }
        RespFrame::Double(s) => {
            let n = s.parse::<f64>().unwrap_or(f64::NAN);
            if resp3 {
                // Upstream redisProtocolToLuaType_Double → `{double = n}`.
                // (frankenredis-vr8rg)
                let t = LuaTable::new();
                t.set(LuaValue::Str(b"double".to_vec()), LuaValue::Number(n));
                LuaValue::Table(t)
            } else {
                LuaValue::Number(n)
            }
        }
        RespFrame::Set(None) => null(),
        RespFrame::Set(Some(items)) => {
            if resp3 {
                // Upstream redisProtocolToLuaType_Set → `{set = {member = true, …}}`.
                // (frankenredis-vr8rg)
                let inner = LuaTable::new();
                for item in items.iter() {
                    inner.set(resp_to_lua(item, resp3), LuaValue::Bool(true));
                }
                let outer = LuaTable::new();
                outer.set(LuaValue::Str(b"set".to_vec()), LuaValue::Table(inner));
                LuaValue::Table(outer)
            } else {
                let t = LuaTable::new();
                for (i, item) in items.iter().enumerate() {
                    t.set(LuaValue::Number((i + 1) as f64), resp_to_lua(item, resp3));
                }
                LuaValue::Table(t)
            }
        }
        // RESP3 Verbatim: Lua sees the body as a plain string (the "txt:"
        // format tag is not surfaced — minor residual vs upstream's
        // `{format=…, string=…}` table).
        RespFrame::Verbatim(s) => LuaValue::Str(s.as_bytes().to_vec()),
        // RESP3 Big Number → `{big_number = "<digits>"}`. (frankenredis-h2uga)
        RespFrame::BigNumber(s) => {
            let t = LuaTable::new();
            t.set(
                LuaValue::Str(b"big_number".to_vec()),
                LuaValue::Str(s.as_bytes().to_vec()),
            );
            LuaValue::Table(t)
        }
    }
}

pub fn lua_to_resp(val: &LuaValue, resp3: bool) -> RespFrame {
    match val {
        LuaValue::Nil => RespFrame::BulkString(None),
        // (frankenredis-0gz4g) Upstream luaReplyToRedisReply uses addReplyBool
        // for a Lua boolean once the script is on RESP3 (redis.setresp(3)) — a
        // RESP3 `#t`/`#f` that downgrades to `:1`/`:0` for a RESP2 client. In the
        // default RESP2 script the historical mapping holds: true -> :1, false ->
        // nil. The RespFrame::Bool RESP2 downgrade happens in
        // downconvert_lua_reply_to_resp2.
        LuaValue::Bool(b) if resp3 => RespFrame::Bool(*b),
        LuaValue::Bool(true) => RespFrame::Integer(1),
        LuaValue::Bool(false) => RespFrame::BulkString(None),
        LuaValue::Number(n) => {
            // Upstream src/script_lua.c::luaReplyToRedisReply line 622:
            //   addReplyLongLong(c, (long long)lua_tonumber(lua, -1));
            // The cast `(long long)x` for non-finite double is UB in
            // C, but on x86-64 GCC it consistently returns LLONG_MIN
            // (the "indefinite integer" sentinel). Rust's `as i64`
            // instead saturates: +inf → i64::MAX, -inf → i64::MIN,
            // NaN → 0. Pin upstream's de-facto behavior so EVAL of
            // math.huge / 1/0 / 0/0 surfaces the same Integer reply
            // (Integer(-9223372036854775808)) as vendored 7.2.4.
            // (frankenredis-luanonfinite)
            //
            // (frankenredis-8reid) cvttsd2si also returns LLONG_MIN
            // for any finite double outside [INT64_MIN, INT64_MAX +
            // 1). That bites the strtoul-wrap case from
            // luaB_tonumber('-5', 16) where the Lua value is
            // ~1.844674407371e+19. Rust's `as i64` saturates to
            // i64::MAX (9223372036854775807) instead. Match the C
            // result by treating any out-of-range finite double the
            // same as non-finite.
            let i = if n.is_finite()
                && *n >= -9223372036854775808.0_f64
                && *n < 9223372036854775808.0_f64
            {
                *n as i64
            } else {
                i64::MIN
            };
            RespFrame::Integer(i)
        }
        LuaValue::Str(s) => RespFrame::BulkString(Some(s.clone())),
        LuaValue::Userdata(_) => RespFrame::BulkString(None),
        LuaValue::Table(t) => {
            // (frankenredis-ly7jr) Upstream script_lua.c::luaReplyToRedisReply
            // checks the 'err' field BEFORE 'ok' so a table carrying
            // both collapses to an error reply with the err contents.
            // fr previously checked ok first and would emit a status
            // reply, swallowing the err side.
            if let LuaValue::Str(err) = t.get(&LuaValue::Str(b"err".to_vec())) {
                return RespFrame::Error(String::from_utf8_lossy(&err).to_string());
            }
            if let LuaValue::Str(ok) = t.get(&LuaValue::Str(b"ok".to_vec())) {
                return RespFrame::SimpleString(String::from_utf8_lossy(&ok).to_string());
            }

            // Upstream src/script_lua.c::luaReplyToRedisReply checks for
            // RESP3 type-hint tables AFTER ok/err. fr was missing the
            // entire group, so map / set / double / big_number /
            // verbatim_string hint tables were all serialized as
            // empty arrays (no integer keys at top level).
            // (frankenredis-vr8rghint)

            // {map = t}: emit a Map frame whose entries are the hash
            // pairs of the inner table. The fr-protocol layer flattens
            // Map → 2N alternating Array under RESP2 and emits the `%`
            // prefix under RESP3.
            let map_field = t.get(&LuaValue::Str(b"map".to_vec()));
            if let LuaValue::Table(inner) = map_field {
                let pairs = inner
                    .hash_pairs()
                    .into_iter()
                    .map(|(k, v)| (lua_to_resp(&k, resp3), lua_to_resp(&v, resp3)))
                    .collect();
                return RespFrame::Map(Some(pairs));
            }

            // {set = t}: emit the inner table's KEYS (not values) as
            // an Array. Upstream src/script_lua.c::luaReplyToRedisReply
            // iterates with lua_next, pops the value, and recurses on
            // the key — so `{set={a=true, b=true}}` yields {"a","b"}
            // and `{set={1,2,3}}` yields {1,2,3} (the integer indices
            // of the positional table, not its values).
            // (frankenredis-jp7gs continuation: set-keys, not values)
            let set_field = t.get(&LuaValue::Str(b"set".to_vec()));
            if let LuaValue::Table(inner) = set_field {
                let mut items: Vec<RespFrame> = Vec::new();
                let inner_borrow = inner.inner.borrow();
                // Array part: positional keys 1..N, skipping holes
                // (where the value is Nil — `lua_next` would skip
                // them too because the slot is unset).
                for (idx, value) in inner_borrow.array.iter().enumerate() {
                    if matches!(value, LuaValue::Nil) {
                        continue;
                    }
                    items.push(RespFrame::Integer(
                        i64::try_from(idx + 1).unwrap_or(i64::MAX),
                    ));
                }
                // String hash keys.
                for k in inner_borrow.string_hash.keys() {
                    items.push(RespFrame::BulkString(Some(k.clone())));
                }
                // Other-hash keys (numeric non-array, boolean, …).
                for (k, _) in &inner_borrow.other_hash {
                    items.push(lua_to_resp(k, resp3));
                }
                return RespFrame::Array(Some(items));
            }

            // {double = x}: upstream script_lua.c::luaReplyToRedisReply
            // emits the value via addReplyDouble — a RESP3 Double (`,`)
            // frame formatted by d2string, downconverted to a bulk string
            // under RESP2. fr previously emitted a bulk string in BOTH
            // modes (so RESP3 saw `$3.14` instead of `,3.14`) and used
            // Rust Display (so large magnitudes like 1e20 expanded instead
            // of `1e+20`). double_from_f64 routes through the shared
            // fr-protocol::format_redis_double; the RESP2 downconvert below
            // turns the Double frame back into a bulk string.
            // (frankenredis-aae3d, supersedes frankenredis-s964e)
            let double_field = t.get(&LuaValue::Str(b"double".to_vec()));
            if let LuaValue::Number(n) = double_field {
                return RespFrame::double_from_f64(n);
            }

            // {big_number = "..."}: upstream luaReplyToRedisReply emits a
            // RESP3 Big Number (`(<digits>\r\n`), downconverted to a bulk
            // string under RESP2 (handled in downconvert_lua_reply_to_resp2).
            // It also maps CR/LF to spaces before writing the line-based frame.
            // (frankenredis-h2uga, frankenredis-sg1nm)
            let bn_field = t.get(&LuaValue::Str(b"big_number".to_vec()));
            if let LuaValue::Str(s) = bn_field {
                return RespFrame::BigNumber(lua_big_number_payload(&s));
            }

            // {verbatim_string = {format = "<3char>", string = "..."}}:
            // emit BulkString of the inner `string` field. Upstream's
            // RESP3 `=<len>\r\n<fmt>:<payload>` framing isn't
            // representable in fr's frame enum; the BulkString of the
            // raw payload is the documented RESP2 fallback.
            // (frankenredis-eojcu) Vendored script_lua.c requires
            // BOTH subfields to be strings — if either is missing or
            // a non-string type, the hint is rejected and the table
            // falls through to the normal array-part serialisation
            // (which yields an empty array for a hint-only table).
            let vs_field = t.get(&LuaValue::Str(b"verbatim_string".to_vec()));
            if let LuaValue::Table(inner) = vs_field {
                let fmt_ok = matches!(
                    inner.get(&LuaValue::Str(b"format".to_vec())),
                    LuaValue::Str(_)
                );
                let str_field = inner.get(&LuaValue::Str(b"string".to_vec()));
                if fmt_ok && let LuaValue::Str(s) = str_field {
                    return RespFrame::BulkString(Some(s));
                }
            }

            // Convert array part to RESP array (stop at first nil, matching Redis)
            let mut items = Vec::new();
            for item in t.inner.borrow().array.clone() {
                if matches!(item, LuaValue::Nil) {
                    break;
                }
                items.push(lua_to_resp(&item, resp3));
            }
            RespFrame::Array(Some(items))
        }
        LuaValue::Function(_)
        | LuaValue::RustFunction(_)
        | LuaValue::Coroutine(_)
        | LuaValue::WrappedCoroutine(_) => RespFrame::BulkString(None),
    }
}

fn lua_big_number_payload(bytes: &[u8]) -> String {
    let mut text = String::from_utf8_lossy(bytes).into_owned();
    if text.as_bytes().iter().any(|&b| b == b'\r' || b == b'\n') {
        text = text
            .chars()
            .map(|c| if c == '\r' || c == '\n' { ' ' } else { c })
            .collect();
    }
    text
}

// ── string.format implementation ────────────────────────────────────────

fn lua_string_format(
    inv_name: Option<&str>,
    fmt: &str,
    args: &[LuaValue],
) -> Result<Vec<u8>, String> {
    // (frankenredis-fllxr) When inv_name is Some, render the named/
    // prefixed shape (direct or Lua-wrapped call). When None, render
    // the anonymous/unprefixed shape (pcall directly invoking the
    // C-builtin). luaL_argerror calls go through lua_format_argerror
    // which already handles the dual shape; luaL_error calls (the
    // "invalid option" branch) preserve the 'format' name in both
    // shapes but drop the source:line prefix when invoked via pcall.
    let invalid_option = |conv: char| -> String {
        match inv_name {
            Some(_) => format!("user_script:1: invalid option '%{conv}' to 'format'"),
            None => format!("invalid option '%{conv}' to 'format'"),
        }
    };
    let mut result = Vec::new();
    let mut arg_idx = 0;
    let mut chars = fmt.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '%' {
            // (frankenredis-xpopu) Upstream lstrlib.c::str_format scans
            // a conversion item after L_ESC; when the format string ends
            // with a bare `%` the scanner increments the arg counter and
            // then bails on the missing arg via luaL_check*, surfacing
            // "bad argument #N to 'format' (no value)". fr previously
            // emitted the literal `%` silently.
            if chars.peek().is_none() {
                return Err(lua_format_argerror(
                    inv_name,
                    "format",
                    arg_idx + 2,
                    "no value",
                ));
            }
            if let Some(&next) = chars.peek() {
                if next == '%' {
                    chars.next();
                    result.push(b'%');
                    continue;
                }
                // Parse flags
                let mut left_align = false;
                let mut zero_pad = false;
                let mut show_sign = false;
                let mut space_sign = false;
                let mut alt_form = false;
                // (frankenredis-b8y0g) Upstream lstrlib.c::scanformat
                // bounds the flag-consumption loop via
                // `(p - strfrmt) >= sizeof(FLAGS)` where FLAGS = "-+ #0"
                // (5 chars + null = 6 bytes). 6+ flag chars raises
                // 'invalid format (repeated flags)'. fr previously
                // accepted any number of repeated flags silently.
                let mut flag_chars: usize = 0;
                while let Some(&fc) = chars.peek() {
                    match fc {
                        '-' => left_align = true,
                        '0' => zero_pad = true,
                        '+' => show_sign = true,
                        ' ' => space_sign = true,
                        '#' => alt_form = true,
                        _ => break,
                    }
                    chars.next();
                    flag_chars += 1;
                    if flag_chars > 5 {
                        return Err(match inv_name {
                            Some(_) => "user_script:1: invalid format (repeated flags)".to_string(),
                            None => "invalid format (repeated flags)".to_string(),
                        });
                    }
                }
                // Width
                let mut width: Option<usize> = None;
                let mut w_str = String::new();
                while let Some(&fc) = chars.peek() {
                    if fc.is_ascii_digit() {
                        w_str.push(fc);
                        chars.next();
                    } else {
                        break;
                    }
                }
                // (frankenredis-94zyf) Upstream Lua 5.1.5 lstrlib.c
                // ::scanformat reads at most 2 digits for width;
                // a third digit raises 'invalid format (width or
                // precision too long)'. fr previously accepted
                // arbitrary-length runs, producing absurd output for
                // specs like '%100d' / '%999d' that vendored rejects.
                if w_str.len() > 2 {
                    return Err(match inv_name {
                        Some(_) => "user_script:1: invalid format (width or precision too long)"
                            .to_string(),
                        None => "invalid format (width or precision too long)".to_string(),
                    });
                }
                if !w_str.is_empty() {
                    width = w_str.parse().ok();
                }
                // Precision
                let mut precision: Option<usize> = None;
                if chars.peek() == Some(&'.') {
                    chars.next();
                    let mut p_str = String::new();
                    while let Some(&fc) = chars.peek() {
                        if fc.is_ascii_digit() {
                            p_str.push(fc);
                            chars.next();
                        } else {
                            break;
                        }
                    }
                    // (frankenredis-94zyf) Same 2-digit cap as width.
                    if p_str.len() > 2 {
                        return Err(match inv_name {
                            Some(_) => {
                                "user_script:1: invalid format (width or precision too long)"
                                    .to_string()
                            }
                            None => "invalid format (width or precision too long)".to_string(),
                        });
                    }
                    precision = Some(p_str.parse().unwrap_or(0));
                }
                // Conversion
                if let Some(conv) = chars.next() {
                    let arg = match args.get(arg_idx) {
                        Some(v) => v.clone(),
                        None => {
                            // (frankenredis-be7o1) Upstream luaL_checknumber/
                            // luaL_checkstring number the arguments from the
                            // caller's perspective, where the format string
                            // is arg #1. arg_idx is 0-based into args[1..],
                            // so the missing value is Lua arg #(arg_idx + 2).
                            return Err(lua_format_argerror(
                                inv_name,
                                "format",
                                arg_idx + 2,
                                "no value",
                            ));
                        }
                    };
                    arg_idx += 1;
                    // (frankenredis-be7o1) Vendored luaL_checknumber errors with
                    // "number expected, got <type>" when the arg is not a
                    // number or numeric string. fr was silently 0-defaulting.
                    let require_number = |v: &LuaValue| -> Result<f64, String> {
                        if let Some(n) = v.to_number() {
                            Ok(n)
                        } else {
                            Err(lua_format_argerror(
                                inv_name,
                                "format",
                                arg_idx + 1,
                                &format!("number expected, got {}", v.type_name()),
                            ))
                        }
                    };
                    let formatted: Vec<u8> = match conv {
                        'd' | 'i' => {
                            // (frankenredis-3pug0) C printf treats
                            // precision for integer conversions as the
                            // minimum digit count (zero-padded). When
                            // precision is set, zero-padding from width
                            // is suppressed and spaces fill the
                            // remaining width. When precision is 0 and
                            // value is 0, NO digits are produced.
                            // (frankenredis-45jr8) Upstream Lua 5.1.5
                            // calls (long)x on the result of
                            // luaL_checknumber. For finite f64 values
                            // exceeding LLONG_MAX the C cast is UB; on
                            // x86_64 it produces LLONG_MIN. Rust's
                            // `f64 as i64` saturates to i64::MAX, so
                            // mirror x86_64 UB explicitly for parity.
                            let raw = require_number(&arg)?;
                            let n = if raw.is_finite()
                                && !(-9223372036854775808.0..9223372036854775808.0).contains(&raw)
                            {
                                i64::MIN
                            } else {
                                raw as i64
                            };
                            let abs_str = if n == i64::MIN {
                                "9223372036854775808".to_string()
                            } else {
                                format!("{}", n.unsigned_abs())
                            };
                            let padded_digits = match precision {
                                Some(0) if n == 0 => String::new(),
                                Some(p) if p > abs_str.len() => {
                                    let pad = p - abs_str.len();
                                    format!("{}{}", "0".repeat(pad), abs_str)
                                }
                                _ => abs_str,
                            };
                            let s = if n < 0 {
                                format!("-{padded_digits}")
                            } else if show_sign {
                                format!("+{padded_digits}")
                            } else if space_sign {
                                format!(" {padded_digits}")
                            } else {
                                padded_digits
                            };
                            // Per C printf: an explicit precision
                            // suppresses the 0-flag for integer
                            // conversions.
                            let effective_pad = if precision.is_some() {
                                ' '
                            } else if zero_pad {
                                '0'
                            } else {
                                ' '
                            };
                            lua_fmt_pad(&s, width, left_align, effective_pad).into_bytes()
                        }
                        'u' => {
                            // (frankenredis-t1ah8) Upstream %u prints the unsigned
                            // bit pattern of the C `long`/`int` result of luaL_checkinteger.
                            // Going via `as i64 as u64` recovers the two's complement
                            // bit pattern for negatives (e.g. -1 -> 18446744073709551615).
                            // fr was rendering negatives as signed (-1) by sharing the
                            // %d arm.
                            let n = require_number(&arg)? as i64 as u64;
                            let digits = format!("{n}");
                            let padded_digits = match precision {
                                Some(0) if n == 0 => String::new(),
                                Some(p) if p > digits.len() => {
                                    format!("{}{}", "0".repeat(p - digits.len()), digits)
                                }
                                _ => digits,
                            };
                            let effective_pad = if precision.is_some() {
                                ' '
                            } else if zero_pad {
                                '0'
                            } else {
                                ' '
                            };
                            lua_fmt_pad(&padded_digits, width, left_align, effective_pad)
                                .into_bytes()
                        }
                        'f' => {
                            let n = require_number(&arg)?;
                            if let Some(s) = lua_fmt_nonfinite(n, conv == 'F') {
                                let s = if show_sign && !n.is_sign_negative() {
                                    format!("+{s}")
                                } else if space_sign && !n.is_sign_negative() {
                                    format!(" {s}")
                                } else {
                                    s
                                };
                                lua_fmt_pad(&s, width, left_align, ' ').into_bytes()
                            } else {
                                let prec = precision.unwrap_or(6);
                                let s = if show_sign && n >= 0.0 {
                                    format!("+{n:.prec$}")
                                } else if space_sign && n >= 0.0 {
                                    format!(" {n:.prec$}")
                                } else {
                                    format!("{n:.prec$}")
                                };
                                lua_fmt_pad(&s, width, left_align, if zero_pad { '0' } else { ' ' })
                                    .into_bytes()
                            }
                        }
                        'e' | 'E' => {
                            let n = require_number(&arg)?;
                            if let Some(s) = lua_fmt_nonfinite(n, conv == 'E') {
                                let s = if show_sign && !n.is_sign_negative() {
                                    format!("+{s}")
                                } else if space_sign && !n.is_sign_negative() {
                                    format!(" {s}")
                                } else {
                                    s
                                };
                                lua_fmt_pad(&s, width, left_align, ' ').into_bytes()
                            } else {
                                let prec = precision.unwrap_or(6);
                                let s = lua_fmt_scientific(n, prec, conv == 'E');
                                let s = if show_sign && n >= 0.0 {
                                    format!("+{s}")
                                } else {
                                    s
                                };
                                lua_fmt_pad(&s, width, left_align, ' ').into_bytes()
                            }
                        }
                        'g' | 'G' => {
                            let n = require_number(&arg)?;
                            if let Some(s) = lua_fmt_nonfinite(n, conv == 'G') {
                                let s = if show_sign && !n.is_sign_negative() {
                                    format!("+{s}")
                                } else if space_sign && !n.is_sign_negative() {
                                    format!(" {s}")
                                } else {
                                    s
                                };
                                lua_fmt_pad(&s, width, left_align, ' ').into_bytes()
                            } else {
                                let prec = precision.unwrap_or(6).max(1);
                                let s = lua_fmt_g(n, prec, conv == 'G');
                                let s = if show_sign && n >= 0.0 {
                                    format!("+{s}")
                                } else {
                                    s
                                };
                                lua_fmt_pad(&s, width, left_align, ' ').into_bytes()
                            }
                        }
                        's' => {
                            // (frankenredis-u5qgq follow-up) Lua 5.1's
                            // luaO_pushvfstring rejects non-string/non-
                            // number args to %s with "bad argument #N
                            // to 'format' (string expected, got <type>)".
                            // fr was permissively accepting nil/tables/bool.
                            let s = match &arg {
                                LuaValue::Str(b) => b.clone(),
                                LuaValue::Number(n) => {
                                    if *n == (*n as i64) as f64 && n.is_finite() {
                                        format!("{}", *n as i64).into_bytes()
                                    } else {
                                        lua_number_to_string(*n).into_bytes()
                                    }
                                }
                                _ => {
                                    // arg_idx is already +1 (incremented before the match);
                                    // add another +1 to account for the format-string itself
                                    // being arg #1 in the Lua-call position scheme.
                                    // (frankenredis-5rnrx) Prepend the standard Lua error
                                    // location prefix; matches luaL_argerror in upstream
                                    // which wraps with "<source>:<line>: " automatically.
                                    // (frankenredis-fllxr) Route through lua_format_argerror
                                    // for the dual direct/pcall shape.
                                    return Err(lua_format_argerror(
                                        inv_name,
                                        "format",
                                        arg_idx + 1,
                                        &format!("string expected, got {}", arg.type_name()),
                                    ));
                                }
                            };
                            let mut s = s;
                            if let Some(prec) = precision {
                                s.truncate(prec);
                            }
                            lua_fmt_pad_bytes(s, width, left_align, b' ')
                        }
                        'q' => {
                            // (frankenredis-u5qgq follow-up) Lua 5.1
                            // string.format("%q", s) escapes newline as
                            // a backslash followed by a *literal*
                            // newline (so the output is multi-line),
                            // not "\\n". This matches the original
                            // luaO_str2d / addquoted in lstrlib.c.
                            // (frankenredis-xpopu) Upstream addquoted
                            // calls luaL_checklstring which accepts
                            // strings and numbers (lua_tolstring coerces
                            // numbers) but errors on nil/bool/table/etc.
                            // fr previously took any type via
                            // to_display_string and produced surprising
                            // output like "\"nil\"" or "\"table: 0x...\"".
                            let s = match &arg {
                                LuaValue::Str(b) => b.clone(),
                                LuaValue::Number(n) => {
                                    if *n == (*n as i64) as f64 && n.is_finite() {
                                        format!("{}", *n as i64).into_bytes()
                                    } else {
                                        lua_number_to_string(*n).into_bytes()
                                    }
                                }
                                _ => {
                                    return Err(lua_format_argerror(
                                        inv_name,
                                        "format",
                                        arg_idx + 1,
                                        &format!("string expected, got {}", arg.type_name()),
                                    ));
                                }
                            };
                            let mut q = Vec::new();
                            q.push(b'"');
                            for &b in &s {
                                match b {
                                    b'\\' => q.extend_from_slice(b"\\\\"),
                                    b'"' => q.extend_from_slice(b"\\\""),
                                    b'\n' => q.extend_from_slice(b"\\\n"),
                                    b'\r' => q.extend_from_slice(b"\\r"),
                                    // (frankenredis-0en30) Upstream
                                    // Lua 5.1.5 lstrlib.c::addquoted
                                    // emits NUL as the three-digit
                                    // zero-padded "\\000" form so a
                                    // subsequent digit can't be misread
                                    // as part of the escape; fr was
                                    // emitting the ambiguous "\\0".
                                    b'\0' => q.extend_from_slice(b"\\000"),
                                    _ => q.push(b),
                                }
                            }
                            q.push(b'"');
                            q
                        }
                        'x' | 'X' => {
                            // (frankenredis-t1ah8) Upstream luaL_checkinteger
                            // returns lua_Integer (C `ptrdiff_t`/long); %x/%X
                            // prints the unsigned bit pattern. `as u64` directly
                            // from f64 saturates negatives to 0; going through
                            // i64 first recovers the two's complement bits, so
                            // -1 -> ffffffffffffffff matching vendored.
                            // (frankenredis-3pug0) Precision: minimum digit
                            // count, zero-padded; the # alt-form prefix sits
                            // outside the precision pad; precision suppresses
                            // the width's 0-flag.
                            let n = require_number(&arg)? as i64 as u64;
                            let digits = if conv == 'x' {
                                format!("{n:x}")
                            } else {
                                format!("{n:X}")
                            };
                            let padded_digits = match precision {
                                Some(0) if n == 0 => String::new(),
                                Some(p) if p > digits.len() => {
                                    format!("{}{}", "0".repeat(p - digits.len()), digits)
                                }
                                _ => digits,
                            };
                            let s = if alt_form && n != 0 {
                                if conv == 'x' {
                                    format!("0x{padded_digits}")
                                } else {
                                    format!("0X{padded_digits}")
                                }
                            } else {
                                padded_digits
                            };
                            let effective_pad = if precision.is_some() {
                                ' '
                            } else if zero_pad {
                                '0'
                            } else {
                                ' '
                            };
                            lua_fmt_pad(&s, width, left_align, effective_pad).into_bytes()
                        }
                        'o' => {
                            // (frankenredis-t1ah8) Same fix as %x/%X — recover
                            // the unsigned bit pattern for negative inputs.
                            // (frankenredis-3pug0) Precision applies to digit
                            // count; alt-form '#' ensures a leading 0 (just
                            // a precision bump by 1 when needed).
                            let n = require_number(&arg)? as i64 as u64;
                            let digits = format!("{n:o}");
                            let padded_digits = match precision {
                                Some(0) if n == 0 => String::new(),
                                Some(p) if p > digits.len() => {
                                    format!("{}{}", "0".repeat(p - digits.len()), digits)
                                }
                                _ => digits,
                            };
                            let s = if alt_form && !padded_digits.starts_with('0') {
                                format!("0{padded_digits}")
                            } else {
                                padded_digits
                            };
                            let effective_pad = if precision.is_some() {
                                ' '
                            } else if zero_pad {
                                '0'
                            } else {
                                ' '
                            };
                            lua_fmt_pad(&s, width, left_align, effective_pad).into_bytes()
                        }
                        'c' => {
                            // (frankenredis-be7o1) Upstream printf %c casts to
                            // unsigned char with modulo-256 wrap; e.g. -1 -> 0xFF,
                            // 256 -> 0. Rust's 'as u8' saturates floats, so go via
                            // i64 first to recover the C wrap-around semantics.
                            let n = require_number(&arg)? as i64 as u8;
                            // (frankenredis-se4hs) Upstream lstrlib.c::str_format
                            // dispatches %c through sprintf into a temp buffer,
                            // then uses luaL_addsize(b, strlen(buff)) to copy the
                            // result. With n=0 sprintf writes a single NUL byte
                            // and strlen returns 0, so the formatted output
                            // contains nothing. fr's String accumulator would
                            // preserve the NUL, so suppress it explicitly to
                            // match vendored.
                            if n == 0 { Vec::new() } else { vec![n] }
                        }
                        _ => {
                            // (frankenredis-be7o1) Upstream lstrlib.c:str_format
                            // rejects unknown conversion specifiers via luaL_error
                            // with the verbatim wording below.
                            // (frankenredis-fllxr) luaL_error preserves the
                            // 'format' name in both shapes but drops the
                            // user_script:1: prefix when invoked via pcall.
                            return Err(invalid_option(conv));
                        }
                    };
                    result.extend_from_slice(&formatted);
                }
            } else {
                let mut buf = [0u8; 4];
                result.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        } else {
            let mut buf = [0u8; 4];
            result.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
    }
    Ok(result)
}

fn lua_fmt_pad_bytes(s: Vec<u8>, width: Option<usize>, left_align: bool, pad: u8) -> Vec<u8> {
    let mut w = match width {
        Some(w) if w > s.len() => w,
        _ => return s,
    };
    if w > 512 * 1024 * 1024 {
        w = 512 * 1024 * 1024;
    }
    let padding = w - s.len();
    let mut out = Vec::with_capacity(w);
    if left_align {
        out.extend_from_slice(&s);
        out.resize(w, b' ');
    } else {
        out.resize(padding, pad);
        out.extend_from_slice(&s);
    }
    out
}

/// Pad a string to a given width, respecting left-align and pad character.
fn lua_fmt_pad(s: &str, width: Option<usize>, left_align: bool, pad: char) -> String {
    let mut w = match width {
        Some(w) if w > s.len() => w,
        _ => return s.to_string(),
    };
    if w > 512 * 1024 * 1024 {
        w = 512 * 1024 * 1024;
    }
    let padding = w - s.len();
    if left_align {
        format!("{s}{}", " ".repeat(padding))
    } else if pad == '0' && (s.starts_with('-') || s.starts_with('+') || s.starts_with(' ')) {
        // Zero-pad after sign
        let (sign, rest) = s.split_at(1);
        format!("{sign}{}{rest}", "0".repeat(padding))
    } else {
        format!(
            "{}{s}",
            std::iter::repeat_n(pad, padding).collect::<String>()
        )
    }
}

/// (frankenredis-t1ah8) Render non-finite floats the way C's printf does for
/// %e/%E/%f/%F/%g/%G: "inf"/"-inf" or "nan"/"-nan", upper-cased for the
/// upper-case conversion variants. The sign-bit of NaN drives the leading
/// '-' to match x86-64 glibc (0/0 produces a sign-negative NaN, which Lua
/// prints as "-nan"). Returns None for finite values so the caller can fall
/// through to the regular formatter.
fn lua_fmt_nonfinite(n: f64, upper: bool) -> Option<String> {
    if n.is_infinite() {
        Some(if n.is_sign_negative() {
            if upper {
                "-INF".to_string()
            } else {
                "-inf".to_string()
            }
        } else if upper {
            "INF".to_string()
        } else {
            "inf".to_string()
        })
    } else if n.is_nan() {
        Some(if n.is_sign_negative() {
            if upper {
                "-NAN".to_string()
            } else {
                "-nan".to_string()
            }
        } else if upper {
            "NAN".to_string()
        } else {
            "nan".to_string()
        })
    } else {
        None
    }
}

/// Format a number in scientific notation (%e/%E).
fn lua_fmt_scientific(n: f64, prec: usize, upper: bool) -> String {
    if n == 0.0 {
        let e = if upper { 'E' } else { 'e' };
        return format!("{:.prec$}{e}+00", 0.0);
    }
    let abs = n.abs();
    let exp = abs.log10().floor() as i32;
    let mantissa = n / 10f64.powi(exp);
    let e = if upper { 'E' } else { 'e' };
    let sign = if exp >= 0 { '+' } else { '-' };
    format!("{mantissa:.prec$}{e}{sign}{:02}", exp.unsigned_abs())
}

/// Format using %g/%G: use %e if exponent < -4 or >= precision, else %f without trailing zeros.
fn lua_fmt_g(n: f64, prec: usize, upper: bool) -> String {
    if n == 0.0 {
        return "0".to_string();
    }
    let abs = n.abs();
    let exp = abs.log10().floor() as i32;
    if exp < -4 || exp >= prec as i32 {
        // (frankenredis-u5qgq follow-up) C's %g also strips trailing
        // zeros from the scientific notation mantissa — '1.00000e-05'
        // collapses to '1e-05'. lua_fmt_scientific keeps the zeros for
        // the explicit %e/%E formatters; do the cleanup inline here.
        let s = lua_fmt_scientific(n, prec.saturating_sub(1), upper);
        let e_char = if upper { 'E' } else { 'e' };
        if let Some(epos) = s.find(e_char) {
            let mantissa = &s[..epos];
            let exp_part = &s[epos..];
            let trimmed_mantissa = if mantissa.contains('.') {
                let m = mantissa.trim_end_matches('0');
                m.trim_end_matches('.').to_string()
            } else {
                mantissa.to_string()
            };
            format!("{trimmed_mantissa}{exp_part}")
        } else {
            s
        }
    } else {
        let decimal_prec = (prec as i32 - 1 - exp).max(0) as usize;
        let s = format!("{n:.decimal_prec$}");
        // Remove trailing zeros after decimal point
        if s.contains('.') {
            let s = s.trim_end_matches('0');
            let s = s.trim_end_matches('.');
            s.to_string()
        } else {
            s
        }
    }
}

// ── cmsgpack / cjson helpers ────────────────────────────────────────────

fn cmsgpack_pack_value(value: &LuaValue, out: &mut Vec<u8>, depth: usize) -> Result<(), String> {
    if depth >= 16 {
        out.push(0xc0);
        return Ok(());
    }
    match value {
        LuaValue::Nil
        | LuaValue::Function(_)
        | LuaValue::RustFunction(_)
        | LuaValue::Userdata(_)
        | LuaValue::Coroutine(_)
        | LuaValue::WrappedCoroutine(_) => out.push(0xc0),
        LuaValue::Bool(false) => out.push(0xc2),
        LuaValue::Bool(true) => out.push(0xc3),
        LuaValue::Number(n) => cmsgpack_pack_number(*n, out),
        LuaValue::Str(bytes) => cmsgpack_pack_bytes(bytes, out),
        LuaValue::Table(table) => cmsgpack_pack_table(table, out, depth + 1)?,
    }
    Ok(())
}

fn cmsgpack_pack_number(n: f64, out: &mut Vec<u8>) {
    if n.is_finite() && n.fract() == 0.0 && n >= i64::MIN as f64 && n <= i64::MAX as f64 {
        cmsgpack_pack_int(n as i64, out);
    } else {
        let f = n as f32;
        if n == f as f64 {
            out.push(0xca);
            out.extend_from_slice(&f.to_bits().to_be_bytes());
        } else {
            out.push(0xcb);
            out.extend_from_slice(&n.to_bits().to_be_bytes());
        }
    }
}

fn cmsgpack_pack_int(n: i64, out: &mut Vec<u8>) {
    if n >= 0 {
        if n <= 0x7f {
            out.push(n as u8);
        } else if n <= u8::MAX as i64 {
            out.push(0xcc);
            out.push(n as u8);
        } else if n <= u16::MAX as i64 {
            out.push(0xcd);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        } else if n <= u32::MAX as i64 {
            out.push(0xce);
            out.extend_from_slice(&(n as u32).to_be_bytes());
        } else {
            out.push(0xcf);
            out.extend_from_slice(&(n as u64).to_be_bytes());
        }
    } else if (-32..=-1).contains(&n) {
        out.push(n as i8 as u8);
    } else if n >= i8::MIN as i64 {
        out.push(0xd0);
        out.push(n as i8 as u8);
    } else if n >= i16::MIN as i64 {
        out.push(0xd1);
        out.extend_from_slice(&(n as i16).to_be_bytes());
    } else if n >= i32::MIN as i64 {
        out.push(0xd2);
        out.extend_from_slice(&(n as i32).to_be_bytes());
    } else {
        out.push(0xd3);
        out.extend_from_slice(&n.to_be_bytes());
    }
}

fn cmsgpack_pack_len(prefix_fix: u8, op16: u8, op32: u8, len: usize, out: &mut Vec<u8>) {
    if len <= 15 && (prefix_fix == 0x90 || prefix_fix == 0x80) {
        out.push(prefix_fix | len as u8);
    } else if len <= u16::MAX as usize {
        out.push(op16);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(op32);
        out.extend_from_slice(&(len as u32).to_be_bytes());
    }
}

fn cmsgpack_pack_bytes(bytes: &[u8], out: &mut Vec<u8>) {
    let len = bytes.len();
    if len <= 31 {
        out.push(0xa0 | len as u8);
    } else if len <= u8::MAX as usize {
        out.push(0xd9);
        out.push(len as u8);
    } else if len <= u16::MAX as usize {
        out.push(0xda);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(0xdb);
        out.extend_from_slice(&(len as u32).to_be_bytes());
    }
    out.extend_from_slice(bytes);
}

fn cmsgpack_pack_table(table: &LuaTable, out: &mut Vec<u8>, depth: usize) -> Result<(), String> {
    let (array_values, map_pairs) = cmsgpack_table_shape(table);
    if let Some(values) = array_values {
        cmsgpack_pack_len(0x90, 0xdc, 0xdd, values.len(), out);
        for value in values {
            cmsgpack_pack_value(&value, out, depth)?;
        }
    } else {
        cmsgpack_pack_len(0x80, 0xde, 0xdf, map_pairs.len(), out);
        for (key, value) in map_pairs {
            cmsgpack_pack_value(&key, out, depth)?;
            cmsgpack_pack_value(&value, out, depth)?;
        }
    }
    Ok(())
}

fn cmsgpack_table_shape(table: &LuaTable) -> (Option<Vec<LuaValue>>, Vec<(LuaValue, LuaValue)>) {
    let inner = table.inner.borrow();
    let mut numeric_values: std::collections::HashMap<i64, LuaValue> =
        std::collections::HashMap::new();
    let mut count = 0_i64;
    let mut max = 0_i64;
    let mut all_positive_integer_keys = true;

    for (idx, value) in inner.array.iter().enumerate() {
        if matches!(value, LuaValue::Nil) {
            continue;
        }
        let key = (idx + 1) as i64;
        numeric_values.insert(key, value.clone());
        count += 1;
        max = max.max(key);
    }
    let hash_pairs = inner.hash_pairs();
    drop(inner);

    for (key, value) in &hash_pairs {
        match key {
            LuaValue::Number(n) if n.is_finite() && *n > 0.0 && *n == (*n as i64) as f64 => {
                let key = *n as i64;
                numeric_values.insert(key, value.clone());
                count += 1;
                max = max.max(key);
            }
            _ => {
                all_positive_integer_keys = false;
                break;
            }
        }
    }

    if all_positive_integer_keys && max == count {
        let mut values = Vec::with_capacity(max as usize);
        for idx in 1..=max {
            values.push(numeric_values.remove(&idx).unwrap_or(LuaValue::Nil));
        }
        return (Some(values), Vec::new());
    }

    let inner = table.inner.borrow();
    let mut pairs = Vec::new();
    for (idx, value) in inner.array.iter().enumerate() {
        if !matches!(value, LuaValue::Nil) {
            pairs.push((LuaValue::Number((idx + 1) as f64), value.clone()));
        }
    }
    drop(inner);
    pairs.extend(hash_pairs);
    (None, pairs)
}

fn cmsgpack_unpack_values(
    data: &[u8],
    offset: usize,
    limit: usize,
    include_offset: bool,
) -> Result<Vec<LuaValue>, String> {
    let mut cursor = MsgpackCursor::new(data, offset)?;
    let mut values = Vec::new();
    while !cursor.is_eof() && values.len() < limit {
        values.push(cursor.decode_value()?);
    }
    if include_offset {
        let next = if cursor.is_eof() {
            -1.0
        } else {
            cursor.pos as f64
        };
        values.insert(0, LuaValue::Number(next));
    }
    Ok(values)
}

fn cmsgpack_unpack_with_offset(
    data: &[u8],
    offset: i64,
    limit: i64,
) -> Result<Vec<LuaValue>, String> {
    if offset < 0 || limit < 0 {
        return Err(format!(
            "Invalid request to unpack with offset of {offset} and limit of {}.",
            data.len()
        ));
    }
    let offset = offset as usize;
    if offset > data.len() {
        return Err(format!(
            "Start offset {offset} greater than input length {}.",
            data.len()
        ));
    }
    cmsgpack_unpack_values(data, offset, limit as usize, true)
}

struct MsgpackCursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> MsgpackCursor<'a> {
    fn new(data: &'a [u8], offset: usize) -> Result<Self, String> {
        if offset > data.len() {
            return Err(format!(
                "Start offset {offset} greater than input length {}.",
                data.len()
            ));
        }
        Ok(Self { data, pos: offset })
    }

    fn is_eof(&self) -> bool {
        self.pos >= self.data.len()
    }

    fn read_u8(&mut self) -> Result<u8, String> {
        let Some(value) = self.data.get(self.pos).copied() else {
            return Err("Missing bytes in input.".to_string());
        };
        self.pos += 1;
        Ok(value)
    }

    fn read_exact(&mut self, len: usize) -> Result<&'a [u8], String> {
        let end = self.pos.saturating_add(len);
        let Some(slice) = self.data.get(self.pos..end) else {
            return Err("Missing bytes in input.".to_string());
        };
        self.pos = end;
        Ok(slice)
    }

    fn read_u16(&mut self) -> Result<u16, String> {
        let bytes = self.read_exact(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn read_u32(&mut self) -> Result<u32, String> {
        let bytes = self.read_exact(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn read_u64(&mut self) -> Result<u64, String> {
        let bytes = self.read_exact(8)?;
        Ok(u64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    fn decode_value(&mut self) -> Result<LuaValue, String> {
        let tag = self.read_u8()?;
        match tag {
            0x00..=0x7f => Ok(LuaValue::Number(tag as f64)),
            0xe0..=0xff => Ok(LuaValue::Number((tag as i8) as f64)),
            0xc0 => Ok(LuaValue::Nil),
            0xc2 => Ok(LuaValue::Bool(false)),
            0xc3 => Ok(LuaValue::Bool(true)),
            0xcc => Ok(LuaValue::Number(self.read_u8()? as f64)),
            0xcd => Ok(LuaValue::Number(self.read_u16()? as f64)),
            0xce => Ok(LuaValue::Number(self.read_u32()? as f64)),
            0xcf => Ok(LuaValue::Number(self.read_u64()? as f64)),
            0xd0 => Ok(LuaValue::Number((self.read_u8()? as i8) as f64)),
            0xd1 => Ok(LuaValue::Number((self.read_u16()? as i16) as f64)),
            0xd2 => Ok(LuaValue::Number((self.read_u32()? as i32) as f64)),
            0xd3 => Ok(LuaValue::Number((self.read_u64()? as i64) as f64)),
            0xca => Ok(LuaValue::Number(f32::from_bits(self.read_u32()?) as f64)),
            0xcb => Ok(LuaValue::Number(f64::from_bits(self.read_u64()?))),
            0xd9 => {
                let len = self.read_u8()? as usize;
                self.decode_bytes(len)
            }
            0xda => {
                let len = self.read_u16()? as usize;
                self.decode_bytes(len)
            }
            0xdb => {
                let len = self.read_u32()? as usize;
                self.decode_bytes(len)
            }
            0xdc => {
                let len = self.read_u16()? as usize;
                self.decode_array(len)
            }
            0xdd => {
                let len = self.read_u32()? as usize;
                self.decode_array(len)
            }
            0xde => {
                let len = self.read_u16()? as usize;
                self.decode_map(len)
            }
            0xdf => {
                let len = self.read_u32()? as usize;
                self.decode_map(len)
            }
            tag if (tag & 0xe0) == 0xa0 => self.decode_bytes((tag & 0x1f) as usize),
            tag if (tag & 0xf0) == 0x90 => self.decode_array((tag & 0x0f) as usize),
            tag if (tag & 0xf0) == 0x80 => self.decode_map((tag & 0x0f) as usize),
            _ => Err("Bad data format in input.".to_string()),
        }
    }

    fn decode_bytes(&mut self, len: usize) -> Result<LuaValue, String> {
        Ok(LuaValue::Str(self.read_exact(len)?.to_vec()))
    }

    fn decode_array(&mut self, len: usize) -> Result<LuaValue, String> {
        let table = LuaTable::new();
        for _ in 0..len {
            table.inner.borrow_mut().array.push(self.decode_value()?);
        }
        Ok(LuaValue::Table(table))
    }

    fn decode_map(&mut self, len: usize) -> Result<LuaValue, String> {
        let table = LuaTable::new();
        for _ in 0..len {
            let key = self.decode_value()?;
            let value = self.decode_value()?;
            table.set(key, value);
        }
        Ok(LuaValue::Table(table))
    }
}

fn json_escape_bytes(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let mut out = String::from('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            // (frankenredis-t6bqz) Upstream lua_cjson always escapes
            // forward slash to "\/" (the table in lua_cjson.c marks
            // 0x2F as needing escape). The JSON RFC permits but does
            // not require this, but bundled lua_cjson does it for
            // HTML/script-safe embedding. fr previously emitted '/'
            // unescaped.
            '/' => out.push_str("\\/"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c <= '\u{1F}' => out.push_str(&format!("\\u{:04x}", c as u32)),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// (frankenredis-u5qgq) Format a Lua Number as Lua 5.1 does via
/// LUAI_NUMFMT = "%.14g". The C-stdlib %g spec: 14 significant digits,
/// strip trailing zeros after the decimal, use scientific notation when
/// the exponent is < -4 or >= precision (14). Returns nan / inf / -inf
/// in the same canonical lowercase form Lua emits.
pub(crate) fn lua_number_to_string(n: f64) -> String {
    if n.is_nan() {
        // (frankenredis-9dmqr) C printf %g of a quiet NaN with the
        // sign bit set (which IEEE 754 0/0 produces on x86-64) prints
        // '-nan'. Lua 5.1's tostring goes through %.14g so its output
        // matches. Use the sign bit to choose, with positive NaN as
        // a fallback for hand-built NaN values.
        return if n.is_sign_negative() {
            "-nan".to_string()
        } else {
            "nan".to_string()
        };
    }
    if n.is_infinite() {
        return if n > 0.0 {
            "inf".to_string()
        } else {
            "-inf".to_string()
        };
    }
    if n == 0.0 {
        // (frankenredis-n4eln) Preserve the sign of -0.0 so
        // tostring(math.ceil(-0.5)) emits '-0' the way C printf %.14g
        // does. Rust's `0.0 == -0.0` is true so an unguarded equality
        // would conflate the two.
        return if n.is_sign_negative() {
            "-0".to_string()
        } else {
            "0".to_string()
        };
    }
    const PRECISION: i32 = 14;
    let abs = n.abs();
    let exponent = abs.log10().floor() as i32;
    // C's %g uses scientific when X < -4 or X >= precision, where
    // X is the rounded exponent. Approximating with log10().floor()
    // is safe for everything except boundary cases right at 10^k;
    // the formatted output is checked via the format! result below.
    if !(-4..PRECISION).contains(&exponent) {
        // Scientific: %.13e style (13 fractional digits = 14 sig figs).
        let formatted = format!("{:.*e}", (PRECISION - 1) as usize, n);
        // Rust emits '1.2e5'; C %g emits '1.2e+05' for positive exponents
        // and pads to two digits. Lua relies on C printf; mirror that.
        return rust_e_to_c_g(&formatted);
    }
    // Fixed notation with (PRECISION - 1 - exponent) fractional digits,
    // then strip trailing zeros after the decimal point.
    let frac_digits = (PRECISION - 1 - exponent).max(0) as usize;
    let s = format!("{:.*}", frac_digits, n);
    strip_trailing_zeros(&s)
}

/// Convert Rust's "1.2e5" / "1.2e-5" to C %g style "1.2e+05" / "1.2e-05".
/// Then strip trailing zeros from the mantissa (before the 'e').
fn rust_e_to_c_g(formatted: &str) -> String {
    let Some(e_pos) = formatted.find('e') else {
        return formatted.to_string();
    };
    let mantissa = &formatted[..e_pos];
    let exp = &formatted[e_pos + 1..];
    let mantissa_stripped = strip_trailing_zeros(mantissa);
    // Normalise exponent: must have sign and >= 2 digits.
    let (sign, digits) = if let Some(rest) = exp.strip_prefix('-') {
        ('-', rest)
    } else if let Some(rest) = exp.strip_prefix('+') {
        ('+', rest)
    } else {
        ('+', exp)
    };
    let padded = if digits.len() < 2 {
        format!("0{digits}")
    } else {
        digits.to_string()
    };
    format!("{mantissa_stripped}e{sign}{padded}")
}

/// Strip trailing zeros from the fractional portion of a decimal string.
/// "1.5000" -> "1.5"; "1.000" -> "1"; "1" -> "1". Negative-sign safe.
fn strip_trailing_zeros(s: &str) -> String {
    if !s.contains('.') {
        return s.to_string();
    }
    let trimmed = s.trim_end_matches('0');
    let trimmed = trimmed.trim_end_matches('.');
    trimmed.to_string()
}

// ── struct helpers ─────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum LuaStructEndian {
    Big,
    Little,
}

#[derive(Clone, Copy)]
struct LuaStructHeader {
    endian: LuaStructEndian,
    align: usize,
}

impl LuaStructHeader {
    fn new() -> Self {
        Self {
            endian: LuaStructEndian::Little,
            align: 1,
        }
    }
}

const LUA_STRUCT_MAX_INT_SIZE: usize = 32;
const LUA_STRUCT_INT_SIZE: usize = 4;
const LUA_STRUCT_LONG_SIZE: usize = 8;
const LUA_STRUCT_SIZE_T_SIZE: usize = std::mem::size_of::<usize>();
const LUA_STRUCT_MAX_ALIGN: usize = 8;

fn lua_struct_pack(
    inv_name: Option<&str>,
    fmt: &[u8],
    args: &[LuaValue],
) -> Result<Vec<LuaValue>, String> {
    let mut header = LuaStructHeader::new();
    let mut out = Vec::new();
    let mut totalsize = 0_usize;
    let mut arg_idx = 1_usize;
    let mut fmt_idx = 0_usize;

    while fmt_idx < fmt.len() && fmt[fmt_idx] != 0 {
        let opt = fmt[fmt_idx];
        fmt_idx += 1;
        let mut size = lua_struct_optsize(inv_name, opt, fmt, &mut fmt_idx)?;
        let to_align = lua_struct_to_align(totalsize, &header, opt, size);
        totalsize = totalsize.saturating_add(to_align);
        out.extend(std::iter::repeat_n(0, to_align));

        match opt {
            b'b' | b'B' | b'h' | b'H' | b'l' | b'L' | b'T' | b'i' | b'I' => {
                let n = lua_check_number(inv_name, args, arg_idx, "pack")?;
                lua_struct_put_integer(n, header.endian, size, &mut out);
                arg_idx += 1;
            }
            b'x' => out.push(0),
            b'f' => {
                let n = lua_check_number(inv_name, args, arg_idx, "pack")?;
                let bytes = match header.endian {
                    LuaStructEndian::Big => (n as f32).to_be_bytes(),
                    LuaStructEndian::Little => (n as f32).to_le_bytes(),
                };
                out.extend_from_slice(&bytes);
                arg_idx += 1;
            }
            b'd' => {
                let n = lua_check_number(inv_name, args, arg_idx, "pack")?;
                let bytes = match header.endian {
                    LuaStructEndian::Big => n.to_be_bytes(),
                    LuaStructEndian::Little => n.to_le_bytes(),
                };
                out.extend_from_slice(&bytes);
                arg_idx += 1;
            }
            b'c' | b's' => {
                let bytes = lua_check_string(inv_name, args, arg_idx, "pack")?;
                if size == 0 {
                    size = bytes.len();
                }
                if bytes.len() < size {
                    return Err(lua_format_argerror(
                        inv_name,
                        "pack",
                        arg_idx + 1,
                        "string too short",
                    ));
                }
                out.extend_from_slice(&bytes[..size]);
                if opt == b's' {
                    out.push(0);
                    size += 1;
                }
                arg_idx += 1;
            }
            _ => lua_struct_control_option(inv_name, "pack", opt, fmt, &mut fmt_idx, &mut header)?,
        }
        totalsize = totalsize.saturating_add(size);
    }

    Ok(vec![LuaValue::Str(out)])
}

fn lua_struct_unpack(
    inv_name: Option<&str>,
    fmt: &[u8],
    data: &[u8],
    pos: i64,
) -> Result<Vec<LuaValue>, String> {
    if pos <= 0 {
        return Err(lua_format_argerror(
            inv_name,
            "unpack",
            3,
            "offset must be 1 or greater",
        ));
    }

    let mut header = LuaStructHeader::new();
    let mut results = Vec::new();
    let mut data_pos = (pos - 1) as usize;
    let mut fmt_idx = 0_usize;

    while fmt_idx < fmt.len() && fmt[fmt_idx] != 0 {
        let opt = fmt[fmt_idx];
        fmt_idx += 1;
        let mut size = lua_struct_optsize(inv_name, opt, fmt, &mut fmt_idx)?;
        data_pos = data_pos.saturating_add(lua_struct_to_align(data_pos, &header, opt, size));
        lua_struct_check_data_len(inv_name, data, data_pos, size)?;

        match opt {
            b'b' | b'B' | b'h' | b'H' | b'l' | b'L' | b'T' | b'i' | b'I' => {
                let is_signed = opt.is_ascii_lowercase();
                let value = lua_struct_get_integer(
                    &data[data_pos..data_pos + size],
                    header.endian,
                    is_signed,
                );
                results.push(LuaValue::Number(value));
            }
            b'x' => {}
            b'f' => {
                let mut bytes = [0_u8; 4];
                bytes.copy_from_slice(&data[data_pos..data_pos + 4]);
                let value = match header.endian {
                    LuaStructEndian::Big => f32::from_be_bytes(bytes),
                    LuaStructEndian::Little => f32::from_le_bytes(bytes),
                };
                results.push(LuaValue::Number(value as f64));
            }
            b'd' => {
                let mut bytes = [0_u8; 8];
                bytes.copy_from_slice(&data[data_pos..data_pos + 8]);
                let value = match header.endian {
                    LuaStructEndian::Big => f64::from_be_bytes(bytes),
                    LuaStructEndian::Little => f64::from_le_bytes(bytes),
                };
                results.push(LuaValue::Number(value));
            }
            b'c' => {
                if size == 0 {
                    let Some(LuaValue::Number(n)) = results.pop() else {
                        return Err(lua_struct_runtime_error(
                            inv_name,
                            "format 'c0' needs a previous size",
                        ));
                    };
                    size = lua_struct_number_to_size(n);
                    lua_struct_check_data_len(inv_name, data, data_pos, size)?;
                }
                results.push(LuaValue::Str(data[data_pos..data_pos + size].to_vec()));
            }
            b's' => {
                let Some(end_rel) = data[data_pos..].iter().position(|b| *b == 0) else {
                    return Err(lua_struct_runtime_error(
                        inv_name,
                        "unfinished string in data",
                    ));
                };
                size = end_rel + 1;
                results.push(LuaValue::Str(data[data_pos..data_pos + end_rel].to_vec()));
            }
            _ => {
                lua_struct_control_option(inv_name, "unpack", opt, fmt, &mut fmt_idx, &mut header)?
            }
        }
        data_pos = data_pos.saturating_add(size);
    }

    results.push(LuaValue::Number((data_pos + 1) as f64));
    Ok(results)
}

fn lua_struct_size(inv_name: Option<&str>, fmt: &[u8]) -> Result<usize, String> {
    let mut header = LuaStructHeader::new();
    let mut pos = 0_usize;
    let mut fmt_idx = 0_usize;

    while fmt_idx < fmt.len() && fmt[fmt_idx] != 0 {
        let opt = fmt[fmt_idx];
        fmt_idx += 1;
        let size = lua_struct_optsize(inv_name, opt, fmt, &mut fmt_idx)?;
        pos = pos.saturating_add(lua_struct_to_align(pos, &header, opt, size));
        if opt == b's' {
            return Err(lua_format_argerror(
                inv_name,
                "size",
                1,
                "option 's' has no fixed size",
            ));
        }
        if opt == b'c' && size == 0 {
            return Err(lua_format_argerror(
                inv_name,
                "size",
                1,
                "option 'c0' has no fixed size",
            ));
        }
        if !opt.is_ascii_alphanumeric() {
            lua_struct_control_option(inv_name, "size", opt, fmt, &mut fmt_idx, &mut header)?;
        }
        pos = pos.saturating_add(size);
    }

    Ok(pos)
}

fn lua_struct_optsize(
    inv_name: Option<&str>,
    opt: u8,
    fmt: &[u8],
    fmt_idx: &mut usize,
) -> Result<usize, String> {
    match opt {
        b'B' | b'b' => Ok(1),
        b'H' | b'h' => Ok(2),
        b'L' | b'l' => Ok(LUA_STRUCT_LONG_SIZE),
        b'T' => Ok(LUA_STRUCT_SIZE_T_SIZE),
        b'f' => Ok(4),
        b'd' => Ok(8),
        b'x' => Ok(1),
        b'c' => lua_struct_getnum(inv_name, fmt, fmt_idx, 1),
        b'i' | b'I' => {
            let size = lua_struct_getnum(inv_name, fmt, fmt_idx, LUA_STRUCT_INT_SIZE)?;
            if size > LUA_STRUCT_MAX_INT_SIZE {
                return Err(lua_struct_runtime_error(
                    inv_name,
                    &format!(
                        "integral size {size} is larger than limit of {LUA_STRUCT_MAX_INT_SIZE}"
                    ),
                ));
            }
            Ok(size)
        }
        _ => Ok(0),
    }
}

fn lua_struct_getnum(
    inv_name: Option<&str>,
    fmt: &[u8],
    fmt_idx: &mut usize,
    default: usize,
) -> Result<usize, String> {
    if fmt.get(*fmt_idx).is_none_or(|b| !b.is_ascii_digit()) {
        return Ok(default);
    }

    let mut value = 0_usize;
    while let Some(byte) = fmt.get(*fmt_idx)
        && byte.is_ascii_digit()
    {
        let digit = (byte - b'0') as usize;
        if value > (i32::MAX as usize / 10) || value.saturating_mul(10) > i32::MAX as usize - digit
        {
            return Err(lua_struct_runtime_error(inv_name, "integral size overflow"));
        }
        value = value * 10 + digit;
        *fmt_idx += 1;
    }
    Ok(value)
}

fn lua_struct_control_option(
    inv_name: Option<&str>,
    fname: &str,
    opt: u8,
    fmt: &[u8],
    fmt_idx: &mut usize,
    header: &mut LuaStructHeader,
) -> Result<(), String> {
    match opt {
        b' ' => Ok(()),
        b'>' => {
            header.endian = LuaStructEndian::Big;
            Ok(())
        }
        b'<' => {
            header.endian = LuaStructEndian::Little;
            Ok(())
        }
        b'!' => {
            let align = lua_struct_getnum(inv_name, fmt, fmt_idx, LUA_STRUCT_MAX_ALIGN)?;
            if align == 0 || (align & (align - 1)) != 0 {
                return Err(lua_struct_runtime_error(
                    inv_name,
                    &format!("alignment {align} is not a power of 2"),
                ));
            }
            header.align = align;
            Ok(())
        }
        _ => Err(lua_format_argerror(
            inv_name,
            fname,
            1,
            &format!("invalid format option '{}'", char::from(opt)),
        )),
    }
}

fn lua_struct_to_align(len: usize, header: &LuaStructHeader, opt: u8, mut size: usize) -> usize {
    if size == 0 || opt == b'c' {
        return 0;
    }
    if size > header.align {
        size = header.align;
    }
    (size - (len & (size - 1))) & (size - 1)
}

fn lua_struct_put_integer(n: f64, endian: LuaStructEndian, size: usize, out: &mut Vec<u8>) {
    let mut value = if n < 0.0 { (n as i64) as u64 } else { n as u64 };
    match endian {
        LuaStructEndian::Little => {
            for _ in 0..size {
                out.push((value & 0xff) as u8);
                value >>= 8;
            }
        }
        LuaStructEndian::Big => {
            let mut buf = vec![0_u8; size];
            for byte in buf.iter_mut().rev() {
                *byte = (value & 0xff) as u8;
                value >>= 8;
            }
            out.extend_from_slice(&buf);
        }
    }
}

fn lua_struct_get_integer(bytes: &[u8], endian: LuaStructEndian, is_signed: bool) -> f64 {
    let mut value = 0_u64;
    match endian {
        LuaStructEndian::Big => {
            for byte in bytes {
                value = (value << 8) | u64::from(*byte);
            }
        }
        LuaStructEndian::Little => {
            for byte in bytes.iter().rev() {
                value = (value << 8) | u64::from(*byte);
            }
        }
    }

    if !is_signed {
        return value as f64;
    }
    let bit_count = (bytes.len() * 8).min(64);
    if bit_count == 0 {
        return 0.0;
    }
    if bit_count == 64 {
        return (value as i64) as f64;
    }
    let sign_bit = 1_u64 << (bit_count - 1);
    if value & sign_bit != 0 {
        let mask = (!0_u64) << bit_count;
        (value | mask) as i64 as f64
    } else {
        value as f64
    }
}

fn lua_struct_number_to_size(n: f64) -> usize {
    if !n.is_finite() || n == 0.0 {
        0
    } else if n < 0.0 || n >= usize::MAX as f64 {
        usize::MAX
    } else {
        n as usize
    }
}

fn lua_struct_check_data_len(
    inv_name: Option<&str>,
    data: &[u8],
    pos: usize,
    size: usize,
) -> Result<(), String> {
    if size > data.len() || pos > data.len() - size {
        Err(lua_format_argerror(
            inv_name,
            "unpack",
            2,
            "data string too short",
        ))
    } else {
        Ok(())
    }
}

fn lua_struct_runtime_error(inv_name: Option<&str>, message: &str) -> String {
    match inv_name {
        Some(_) => format!("user_script:1: {message}"),
        None => message.to_string(),
    }
}

/// (frankenredis-v95aj) Normalise a LuaValue to u32 for bit library
/// ops. Mirrors LuaJIT bit.tobit: numbers are first cast to int32 via
/// truncate-toward-zero (matching f64 -> i32 semantics for finite
/// values), then reinterpreted as u32. Strings are parsed if they look
/// numeric, otherwise an error is raised.
/// (frankenredis-d3ovh) Convert a Lua value to a u32 the way LuaJIT's
/// bit.* family does. Numbers go through banker's rounding (round-half-
/// to-even, matching lj_num2int's lrint-based path) then narrow to
/// i32→u32. Numeric strings coerce via the same path. Anything else
/// raises a standard "bad argument #N to 'FUNC' (number expected, got
/// TYPE)" error, with the prefix/name controlled by inv_name.
fn lua_value_to_u32_for_bitop(
    inv_name: Option<&str>,
    arg_idx: usize,
    fname: &str,
    val_opt: Option<&LuaValue>,
) -> Result<u32, String> {
    let val = match val_opt {
        Some(v) => v,
        None => {
            return Err(lua_format_argerror(
                inv_name,
                fname,
                arg_idx,
                "number expected, got no value",
            ));
        }
    };
    let bad = |got: &str| {
        lua_format_argerror(
            inv_name,
            fname,
            arg_idx,
            &format!("number expected, got {got}"),
        )
    };
    match val {
        LuaValue::Number(f) => {
            if !f.is_finite() {
                // LuaJIT permits NaN/inf — lj_num2int rounds NaN/±inf
                // to 0x80000000 (INT32_MIN). Match that here.
                return Ok(0x8000_0000u32);
            }
            // Banker's rounding (round-half-to-even). Rust's `f64::
            // round_ties_even` does exactly this.
            let rounded = f.round_ties_even();
            Ok((rounded as i64 as i32) as u32)
        }
        LuaValue::Str(s) => {
            let text = String::from_utf8_lossy(s);
            let trimmed = text.trim();
            if let Ok(n) = trimmed.parse::<i64>() {
                return Ok(n as i32 as u32);
            }
            if let Ok(f) = trimmed.parse::<f64>()
                && f.is_finite()
            {
                return Ok((f.round_ties_even() as i64 as i32) as u32);
            }
            Err(bad("string"))
        }
        LuaValue::Nil => Err(bad("nil")),
        other => Err(bad(other.type_name())),
    }
}

// (frankenredis-bum6y) Mirror Redis-bundled lua_cjson.c::json_create_encoder
// — return the exact error wording vendored emits for unserialisable values.
// The script error wrap adds 'user_script:N: ' prefix and the trailing
// 'script: <sha>, on @user_script:N.' tail.
fn lua_value_to_json(val: &LuaValue) -> Result<String, String> {
    match val {
        LuaValue::Nil => Ok("null".to_string()),
        LuaValue::Bool(b) => Ok(if *b { "true" } else { "false" }.to_string()),
        LuaValue::Number(n) => {
            if !n.is_finite() {
                return Err("Cannot serialise number: must not be NaN or Inf".to_string());
            }
            // (frankenredis-t6bqz) Upstream lua_cjson's
            // json_append_number routes ALL numbers through
            // fpconv_g_fmt — essentially printf("%.14g") — so
            // (a) integers up to 14 significant digits emit as
            // bare decimals, (b) larger integer-valued doubles get
            // rounded into scientific notation, and (c) -0.0
            // preserves its sign. fr's previous integer-cast path
            // bypassed (b) and (c). lua_number_to_string already
            // implements %.14g semantics, so reuse it.
            Ok(lua_number_to_string(*n))
        }
        LuaValue::Str(s) => Ok(json_escape_bytes(s)),
        LuaValue::Table(t) => {
            // (frankenredis-pt4d4) Mirror Lua-bundled cjson's
            // lua_array_length: a table encodes as an array iff every
            // key is a positive integer AND the result isn't "too
            // sparse" (cjson's defaults: max ≤ encode_sparse_safe=10
            // OR count ≥ max/encode_sparse_ratio=2). The array form
            // pads missing indices with null. Boolean/table/function
            // keys raise "Cannot serialise <type>: table key must be
            // a number or string".
            let inner = t.inner.borrow();
            let array_len = inner.array.len();
            let hash_pairs = inner.hash_pairs();
            drop(inner);

            // Validate all keys: numbers/strings allowed; anything
            // else raises before we even attempt to render.
            for (k, _) in &hash_pairs {
                match k {
                    LuaValue::Str(_) | LuaValue::Number(_) => {}
                    other => {
                        return Err(format!(
                            "Cannot serialise {}: table key must be a number or string",
                            other.type_name()
                        ));
                    }
                }
            }

            // Tally positive-integer keys vs total keys.
            let mut max_int_key: i64 = array_len as i64;
            let mut int_key_count: i64 = array_len as i64;
            let mut has_non_int_key = false;
            for (k, _) in &hash_pairs {
                match k {
                    LuaValue::Number(n)
                        if n.is_finite()
                            && *n > 0.0
                            && *n == (*n as i64) as f64
                            && (*n as i64) <= i64::MAX / 2 =>
                    {
                        let i = *n as i64;
                        if i > max_int_key {
                            max_int_key = i;
                        }
                        int_key_count += 1;
                    }
                    _ => {
                        has_non_int_key = true;
                        break;
                    }
                }
            }

            let candidate_array = !has_non_int_key && (array_len > 0 || !hash_pairs.is_empty());
            let array_ok =
                candidate_array && (max_int_key <= 10 || int_key_count * 2 >= max_int_key);

            // (frankenredis-pt4d4) Pure-integer-key tables that fail
            // the sparse threshold are rejected by Lua-bundled cjson
            // under its default encode_sparse_convert=false setting.
            // Mixed/string-key tables fall through to object form
            // unchanged.
            if candidate_array && !array_ok {
                return Err("Cannot serialise table: excessively sparse array".to_string());
            }

            if array_ok {
                // Render as array, gathering values by index.
                let mut by_idx: std::collections::HashMap<i64, LuaValue> =
                    std::collections::HashMap::new();
                let inner = t.inner.borrow();
                for (i, v) in inner.array.iter().enumerate() {
                    by_idx.insert((i + 1) as i64, v.clone());
                }
                drop(inner);
                for (k, v) in &hash_pairs {
                    if let LuaValue::Number(n) = k {
                        by_idx.insert(*n as i64, v.clone());
                    }
                }
                let mut items = Vec::with_capacity(max_int_key as usize);
                for i in 1..=max_int_key {
                    match by_idx.get(&i) {
                        Some(v) => items.push(lua_value_to_json(v)?),
                        None => items.push("null".to_string()),
                    }
                }
                return Ok(format!("[{}]", items.join(",")));
            }

            // Object form: every key (array indices + hash) becomes a
            // string key in the JSON object.
            if array_len == 0 && hash_pairs.is_empty() {
                return Ok("{}".to_string());
            }
            let mut pairs: Vec<String> = Vec::new();
            let inner = t.inner.borrow();
            for (i, v) in inner.array.iter().enumerate() {
                pairs.push(format!("\"{}\":{}", i + 1, lua_value_to_json(v)?));
            }
            drop(inner);
            for (k, v) in &hash_pairs {
                let key_json = json_escape_bytes(&k.to_display_string());
                pairs.push(format!("{key_json}:{}", lua_value_to_json(v)?));
            }
            Ok(format!("{{{}}}", pairs.join(",")))
        }
        LuaValue::Function(_) | LuaValue::RustFunction(_) => {
            Err("Cannot serialise function: type not supported".to_string())
        }
        LuaValue::Userdata(LuaUserdata::CjsonNull) => Ok("null".to_string()),
        LuaValue::Userdata(LuaUserdata::Proxy(_)) => {
            Err("Cannot serialise userdata: type not supported".to_string())
        }
        LuaValue::Coroutine(_) | LuaValue::WrappedCoroutine(_) => {
            Err("Cannot serialise thread: type not supported".to_string())
        }
    }
}

fn json_to_lua_value(s: &str) -> Result<LuaValue, String> {
    let mut parser = JsonParser::new(s);
    let value = parser.parse_value()?;
    parser.skip_ws();
    if !parser.is_eof() {
        return Err(format!(
            "Expected the end but found {} at character {}",
            parser.token_name_at(parser.pos),
            parser.char_pos()
        ));
    }
    Ok(value)
}

struct JsonParser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> JsonParser<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            bytes: input.as_bytes(),
            pos: 0,
        }
    }

    fn is_eof(&self) -> bool {
        self.pos >= self.bytes.len()
    }

    fn char_pos(&self) -> usize {
        self.pos + 1
    }

    fn skip_ws(&mut self) {
        while matches!(self.bytes.get(self.pos), Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.pos += 1;
        }
    }

    fn token_name_at(&self, pos: usize) -> &'static str {
        match self.bytes.get(pos).copied() {
            None => "T_END",
            Some(b']') => "T_ARR_END",
            Some(b'}') => "T_OBJ_END",
            Some(b',') => "T_COMMA",
            Some(b':') => "T_COLON",
            Some(b'"') => "T_STRING",
            Some(b'-' | b'0'..=b'9') => "T_NUMBER",
            _ => "invalid token",
        }
    }

    fn parse_value(&mut self) -> Result<LuaValue, String> {
        self.skip_ws();
        match self.bytes.get(self.pos).copied() {
            None => Err(format!(
                "Expected value but found T_END at character {}",
                self.char_pos()
            )),
            Some(b'n') if self.consume_literal(b"null") => {
                Ok(LuaValue::Userdata(LuaUserdata::CjsonNull))
            }
            Some(b't') if self.consume_literal(b"true") => Ok(LuaValue::Bool(true)),
            Some(b'f') if self.consume_literal(b"false") => Ok(LuaValue::Bool(false)),
            Some(b'"') => self.parse_string().map(LuaValue::Str),
            Some(b'[') => self.parse_array(),
            Some(b'{') => self.parse_object(),
            Some(b'-' | b'0'..=b'9') => self.parse_number().map(LuaValue::Number),
            Some(b']' | b'}') => Err(format!(
                "Expected value but found {} at character {}",
                self.token_name_at(self.pos),
                self.char_pos()
            )),
            Some(_) => Err(format!(
                "Expected value but found invalid token at character {}",
                self.char_pos()
            )),
        }
    }

    fn consume_literal(&mut self, literal: &[u8]) -> bool {
        if self.bytes[self.pos..].starts_with(literal) {
            self.pos += literal.len();
            true
        } else {
            false
        }
    }

    fn parse_string(&mut self) -> Result<Vec<u8>, String> {
        self.pos += 1;
        let mut result = Vec::new();
        while let Some(b) = self.bytes.get(self.pos).copied() {
            self.pos += 1;
            match b {
                b'"' => return Ok(result),
                b'\\' => {
                    let Some(esc) = self.bytes.get(self.pos).copied() else {
                        return Err(format!(
                            "Expected value but found unexpected end of string at character {}",
                            self.char_pos()
                        ));
                    };
                    self.pos += 1;
                    match esc {
                        b'"' => result.push(b'"'),
                        b'\\' => result.push(b'\\'),
                        // (frankenredis-4h221) lua_cjson unescapes `\/`
                        // to `/`, matching its encode-side slash escape.
                        b'/' => result.push(b'/'),
                        b'b' => result.push(0x08),
                        b'f' => result.push(0x0C),
                        b'n' => result.push(b'\n'),
                        b'r' => result.push(b'\r'),
                        b't' => result.push(b'\t'),
                        b'u' => self.push_unicode_escape(&mut result)?,
                        _ => {
                            return Err(format!(
                                "Expected value but found invalid escape character at character {}",
                                self.pos
                            ));
                        }
                    }
                }
                0x00..=0x1f => {
                    return Err(format!(
                        "Expected value but found invalid string character at character {}",
                        self.pos
                    ));
                }
                _ => result.push(b),
            }
        }
        Err(format!(
            "Expected value but found unexpected end of string at character {}",
            self.bytes.len() + 1
        ))
    }

    fn push_unicode_escape(&mut self, out: &mut Vec<u8>) -> Result<(), String> {
        let Some(mut codepoint) = self.parse_hex4() else {
            return Err(format!(
                "Expected value but found invalid unicode escape at character {}",
                self.char_pos()
            ));
        };
        if (0xD800..=0xDBFF).contains(&codepoint)
            && self.bytes.get(self.pos) == Some(&b'\\')
            && self.bytes.get(self.pos + 1) == Some(&b'u')
        {
            let saved = self.pos;
            self.pos += 2;
            if let Some(low) = self.parse_hex4() {
                if (0xDC00..=0xDFFF).contains(&low) {
                    codepoint = 0x10000 + ((codepoint - 0xD800) << 10) + (low - 0xDC00);
                } else {
                    self.pos = saved;
                }
            } else {
                self.pos = saved;
            }
        }
        if let Some(decoded) = char::from_u32(codepoint) {
            let mut utf8 = [0u8; 4];
            out.extend_from_slice(decoded.encode_utf8(&mut utf8).as_bytes());
        }
        Ok(())
    }

    fn parse_hex4(&mut self) -> Option<u32> {
        let end = self.pos.checked_add(4)?;
        let slice = self.bytes.get(self.pos..end)?;
        let text = std::str::from_utf8(slice).ok()?;
        let value = u32::from_str_radix(text, 16).ok()?;
        self.pos = end;
        Some(value)
    }

    fn parse_array(&mut self) -> Result<LuaValue, String> {
        self.pos += 1;
        let table = LuaTable::new();
        self.skip_ws();
        if self.bytes.get(self.pos) == Some(&b']') {
            self.pos += 1;
            return Ok(LuaValue::Table(table));
        }
        loop {
            let value = self.parse_value()?;
            table.inner.borrow_mut().array.push(value);
            self.skip_ws();
            match self.bytes.get(self.pos).copied() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    return Ok(LuaValue::Table(table));
                }
                _ => {
                    return Err(format!(
                        "Expected comma or array end but found {} at character {}",
                        self.token_name_at(self.pos),
                        self.char_pos()
                    ));
                }
            }
        }
    }

    fn parse_object(&mut self) -> Result<LuaValue, String> {
        self.pos += 1;
        let table = LuaTable::new();
        self.skip_ws();
        if self.bytes.get(self.pos) == Some(&b'}') {
            self.pos += 1;
            return Ok(LuaValue::Table(table));
        }
        loop {
            self.skip_ws();
            if self.bytes.get(self.pos) != Some(&b'"') {
                return Err(format!(
                    "Expected object key string but found {} at character {}",
                    self.token_name_at(self.pos),
                    self.char_pos()
                ));
            }
            let key = self.parse_string()?;
            self.skip_ws();
            if self.bytes.get(self.pos) != Some(&b':') {
                return Err(format!(
                    "Expected colon but found {} at character {}",
                    self.token_name_at(self.pos),
                    self.char_pos()
                ));
            }
            self.pos += 1;
            let value = self.parse_value()?;
            table.set(LuaValue::Str(key), value);
            self.skip_ws();
            match self.bytes.get(self.pos).copied() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(LuaValue::Table(table));
                }
                _ => {
                    return Err(format!(
                        "Expected comma or object end but found {} at character {}",
                        self.token_name_at(self.pos),
                        self.char_pos()
                    ));
                }
            }
        }
    }

    fn parse_number(&mut self) -> Result<f64, String> {
        let start = self.pos;
        if self.bytes.get(self.pos) == Some(&b'-') {
            self.pos += 1;
        }
        match self.bytes.get(self.pos).copied() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                self.pos += 1;
                while matches!(self.bytes.get(self.pos), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => {
                return Err(format!(
                    "Expected value but found invalid token at character {}",
                    start + 1
                ));
            }
        }
        if self.bytes.get(self.pos) == Some(&b'.') {
            self.pos += 1;
            let digits_start = self.pos;
            while matches!(self.bytes.get(self.pos), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
            if self.pos == digits_start {
                return Err(format!(
                    "Expected value but found invalid token at character {}",
                    start + 1
                ));
            }
        }
        if matches!(self.bytes.get(self.pos), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.bytes.get(self.pos), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            let digits_start = self.pos;
            while matches!(self.bytes.get(self.pos), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
            if self.pos == digits_start {
                return Err(format!(
                    "Expected value but found invalid token at character {}",
                    start + 1
                ));
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos]).map_err(|_| {
            format!(
                "Expected value but found invalid token at character {}",
                start + 1
            )
        })?;
        text.parse::<f64>().map_err(|_| {
            format!(
                "Expected value but found invalid token at character {}",
                start + 1
            )
        })
    }
}

impl Drop for LuaState<'_> {
    fn drop(&mut self) {
        // Break Rc<RefCell<LuaTableInner>> cycles to prevent memory leaks.
        // User scripts can create cyclic tables (e.g., `t.self = t`) which
        // form Rc cycles that won't be reclaimed without manual clearing.
        // We recursively clear all tables reachable from globals to break
        // these cycles on LuaState destruction.
        fn clear_table_recursive(table: &LuaTable, visited: &mut HashSet<usize>) {
            let ptr = Rc::as_ptr(&table.inner) as usize;
            if !visited.insert(ptr) {
                return;
            }
            if table.inner.borrow().shared_template {
                return;
            }
            let inner = &mut *table.inner.borrow_mut();
            for value in inner.array.drain(..) {
                if let LuaValue::Table(t) = value {
                    clear_table_recursive(&t, visited);
                }
            }
            for (_, value) in inner.string_hash.drain() {
                if let LuaValue::Table(t) = value {
                    clear_table_recursive(&t, visited);
                }
            }
            for (k, v) in inner.other_hash.drain(..) {
                if let LuaValue::Table(t) = k {
                    clear_table_recursive(&t, visited);
                }
                if let LuaValue::Table(t) = v {
                    clear_table_recursive(&t, visited);
                }
            }
            inner.other_keys.clear();
            if let Some(mt) = inner.metatable.take() {
                clear_table_recursive(&mt, visited);
            }
        }

        let mut visited = HashSet::new();
        for value in self.globals.values() {
            if let LuaValue::Table(t) = value {
                clear_table_recursive(t, &mut visited);
            }
        }
    }
}

// ── Compiled chunk cache ────────────────────────────────────────────────

const LUA_COMPILED_CHUNK_CACHE_MAX: usize = 256;

thread_local! {
    static LUA_COMPILED_CHUNK_CACHE: RefCell<HashMap<Vec<u8>, Rc<Block>>> =
        RefCell::new(HashMap::new());
}

fn lua_execution_source(script: &[u8]) -> Cow<'_, [u8]> {
    if script.starts_with(b"#!") {
        let line_end = script
            .iter()
            .position(|&b| b == b'\n')
            .unwrap_or(script.len());
        let mut stripped = Vec::with_capacity(script.len());
        stripped.extend(std::iter::repeat_n(b' ', line_end));
        stripped.extend_from_slice(&script[line_end..]);
        Cow::Owned(stripped)
    } else {
        Cow::Borrowed(script)
    }
}

/// Compile a chunk, returning the parse error as `(1-based line, bare message)`
/// on failure. The line comes from the parser's per-token line map (with_lines).
/// (frankenredis-5qhz7)
fn parse_lua_chunk_located(source: &[u8]) -> Result<Block, (u32, String)> {
    let mut lexer = Lexer::new(source);
    let (tokens, lines) = lexer.tokenize_all_located()?;
    let mut parser = Parser::with_lines(tokens, lines);
    let mut stmts = parser.parse_block().map_err(|m| (parser.error_line(), m))?;
    if !parser.check(&Token::Eof) {
        return Err((
            parser.error_line(),
            format!("'<eof>' expected near '{}'", token_display(parser.peek())),
        ));
    }
    resolve_lua_local_slots(&mut stmts);
    Ok(stmts)
}

fn parse_lua_chunk(source: &[u8]) -> Result<Block, String> {
    // Bare-message form (no line prefix) — preserves the contract that
    // compile_check / loadstring assert on. Use compile_error_line() when the
    // caller needs the `user_script:N` line. (frankenredis-5qhz7)
    parse_lua_chunk_located(source).map_err(|(_, msg)| msg)
}

/// The compile error for `source` formatted as `N: <message>` with the true
/// 1-based error line — for callers that render `user_script:N: <message>`
/// (EVAL/EVALSHA/SCRIPT LOAD). Returns the same bare message parse_lua_chunk
/// produces, prefixed with the actual line. (frankenredis-5qhz7)
pub(crate) fn compile_error_line(source: &[u8]) -> String {
    let resolved = lua_execution_source(source);
    match parse_lua_chunk_located(resolved.as_ref()) {
        Err((line, msg)) => format!("{line}: {msg}"),
        // Only called on the error path; if it unexpectedly compiles, fall back
        // to the line-1 form so the envelope stays well-formed.
        Ok(_) => "1: ".to_string(),
    }
}

pub(crate) fn compile_lua_chunk_cached(script: &[u8]) -> Result<Rc<Block>, String> {
    let source = lua_execution_source(script);
    if let Some(cached) =
        LUA_COMPILED_CHUNK_CACHE.with(|cache| cache.borrow().get(source.as_ref()).cloned())
    {
        return Ok(cached);
    }

    let compiled = Rc::new(parse_lua_chunk(source.as_ref())?);
    let cached = LUA_COMPILED_CHUNK_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(cached) = cache.get(source.as_ref()) {
            return cached.clone();
        }
        if cache.len() >= LUA_COMPILED_CHUNK_CACHE_MAX {
            cache.clear();
        }
        cache.insert(source.as_ref().to_vec(), compiled.clone());
        compiled
    });
    Ok(cached)
}

// ── Public entry point ──────────────────────────────────────────────────

pub fn eval_script(
    script: &[u8],
    keys: &[Vec<u8>],
    argv: &[Vec<u8>],
    store: &mut Store,
    now_ms: u64,
) -> Result<RespFrame, String> {
    let compiled = compile_lua_chunk_cached(script)?;
    eval_compiled_script(compiled, keys, argv, store, now_ms)
}

pub(crate) fn eval_compiled_script(
    compiled: Rc<Block>,
    keys: &[Vec<u8>],
    argv: &[Vec<u8>],
    store: &mut Store,
    now_ms: u64,
) -> Result<RespFrame, String> {
    store.clear_script_propagation_state();
    store.script_propagation_mode = SCRIPT_PROPAGATE_ALL;
    // (frankenredis-qqq17) Break any Rc cycles this script allocates when it
    // returns. Declared before `state` so the LuaState/Env drop first (reverse
    // declaration order), leaving only leaked cycle islands for the sweep.
    let _lua_gc = LuaGcScope::enter();
    let mut state = LuaState::new(store, now_ms);

    let keys_vals: Vec<LuaValue> = keys.iter().map(|k| LuaValue::Str(k.clone())).collect();
    let argv_vals: Vec<LuaValue> = argv.iter().map(|a| LuaValue::Str(a.clone())).collect();
    state.set_keys_argv(keys_vals, argv_vals);

    // (frankenredis-m7oy8) On error, record the failing statement's source line
    // so the command layer can stamp it into the `on @user_script:N.` envelope
    // suffix (covers prefix-less errors like "invalid key to 'next'" too).
    let result = match state.execute_compiled(compiled.as_ref()) {
        Ok(value) => value,
        Err(err) => {
            let line = state.current_line;
            drop(state);
            store.lua_error_line = line;
            return Err(err);
        }
    };
    let frame = lua_to_resp(&result, state.resp_version == 3);
    // Drop state explicitly to release the mutable borrow of store before
    // accessing store.dispatch_client_ctx below.
    drop(state);
    // (frankenredis-luaresp2map) Upstream src/script_lua.c::
    // luaReplyToRedisReply emits Map/Set/Push/Double/etc only when the
    // caller connection is RESP3; for RESP2 it auto-downconverts
    // (Map → flat 2N Array, Set → Array). fr-protocol's encoder
    // unconditionally writes `%N\r\n` for RespFrame::Map, so a Lua
    // `{map={…}}` reply leaked a RESP3 map frame to RESP2 clients.
    // Walk the result tree and flatten Map → flat Array when the
    // caller is on RESP2. Read the version off the store's dispatch
    // context since EVAL always flows through dispatch.
    let frame = if store.dispatch_client_ctx.resp_protocol_version == 3 {
        frame
    } else {
        downconvert_lua_reply_to_resp2(frame)
    };
    Ok(frame)
}

/// Walk a RESP frame tree and rewrite RESP3-only shapes into their
/// RESP2 equivalents (Map → flat 2N Array). Applied to Lua reply
/// frames before they leave eval_script when the calling client is on
/// RESP2. (frankenredis-luaresp2map)
///
/// General RESP3→RESP2 downconverter — also reused for SENTINEL replies,
/// which upstream builds with `addReplyMapLen` (a flat array in RESP2).
pub(crate) fn downconvert_lua_reply_to_resp2(frame: RespFrame) -> RespFrame {
    match frame {
        RespFrame::Map(Some(entries)) => {
            let mut flat = Vec::with_capacity(entries.len() * 2);
            for (k, v) in entries {
                flat.push(downconvert_lua_reply_to_resp2(k));
                flat.push(downconvert_lua_reply_to_resp2(v));
            }
            RespFrame::Array(Some(flat))
        }
        RespFrame::Map(None) => RespFrame::Array(None),
        RespFrame::Array(Some(items)) => RespFrame::Array(Some(
            items
                .into_iter()
                .map(downconvert_lua_reply_to_resp2)
                .collect(),
        )),
        RespFrame::Push(items) => RespFrame::Array(Some(
            items
                .into_iter()
                .map(downconvert_lua_reply_to_resp2)
                .collect(),
        )),
        // RESP2 has no Double type; upstream addReplyDouble emits the
        // d2string text as a bulk string. The Double frame already carries
        // that exact text. (frankenredis-aae3d)
        RespFrame::Double(s) => RespFrame::BulkString(Some(s.into_bytes())),
        // RESP2 has no Big Number type; upstream emits the digits as a bulk
        // string. (frankenredis-h2uga)
        RespFrame::BigNumber(s) => RespFrame::BulkString(Some(s.into_bytes())),
        // RESP2 has no Boolean type; upstream addReplyBool downgrades to the
        // integer `:1` / `:0`. (frankenredis-0gz4g)
        RespFrame::Bool(b) => RespFrame::Integer(i64::from(b)),
        other => other,
    }
}

/// Lex+parse a script body without executing it. Returns the parser's
/// error message verbatim on failure. Mirrors the shebang-stripping
/// performed by `eval_script` so SCRIPT LOAD validates the same source
/// EVAL would later run. (frankenredis-scrldch)
pub fn compile_check(script: &[u8]) -> Result<(), String> {
    compile_lua_chunk_cached(script).map(|_| ())
}

#[cfg(test)]
mod tests {
    use fr_protocol::RespFrame;
    use fr_store::Store;

    use super::{
        Env, LuaState, LuaTable, LuaValue, SCRIPT_NOSCRIPT_ERROR, compile_check, eval_script,
        json_to_lua_value, lua_raw_equal, lua_test_live_tables, lua_value_to_json,
    };

    /// (frankenredis-qqq17) Regression gate for the Lua Rc-cycle leak DoS.
    /// Cyclic scripts — a self-referential table and a recursive closure that
    /// captures its own upvalue cell — used to leak on every EVAL because the
    /// GC-less `Rc<RefCell<..>>` graph never reached refcount 0. The
    /// `LuaGcScope` sweep at `eval_script` end must break those cycles so the
    /// objects are actually reclaimed. We prove reclamation directly via the
    /// test-only live-`LuaTableInner` counter (returns to baseline), not just
    /// registry truncation — and confirm behavior parity is untouched (the
    /// sweep runs post-serialization, so the scripts still return correctly).
    #[test]
    fn lua_cyclic_scripts_do_not_leak_qqq17() {
        let mut store = Store::new();
        let bulk = |s: &str| RespFrame::BulkString(Some(s.as_bytes().to_vec()));

        let baseline = lua_test_live_tables();

        // (b) self-referential table: `t.x = t`.
        for _ in 0..200 {
            let r = eval_script(
                b"local t={}; t.x=t; return type(t.x)",
                &[],
                &[],
                &mut store,
                0,
            )
            .unwrap();
            assert_eq!(
                r,
                bulk("table"),
                "self-referential table output must be intact"
            );
        }
        // (a) recursive closure capturing its own binding.
        for _ in 0..200 {
            let r = eval_script(
                b"local function f(n) if n<=0 then return 0 else return f(n-1) end end return f(5)",
                &[],
                &[],
                &mut store,
                0,
            )
            .unwrap();
            assert_eq!(
                r,
                RespFrame::Integer(0),
                "recursive closure must still compute"
            );
        }
        // A deeper nested cycle: table holding a closure that captures the table.
        for _ in 0..200 {
            let r = eval_script(
                b"local t={}; t.f=function() return t end; return type(t.f().f)",
                &[],
                &[],
                &mut store,
                0,
            )
            .unwrap();
            assert_eq!(r, bulk("function"));
        }

        // The live-table count must return to its baseline: every cyclic table
        // allocated across the 600 evals has been reclaimed. Before the fix this
        // grew without bound (one leaked inner per cyclic eval).
        let after = lua_test_live_tables();
        assert_eq!(
            after, baseline,
            "Lua table inners leaked: {after} live vs baseline {baseline} (qqq17 cycle sweep regressed)"
        );
    }

    #[test]
    fn redis_setresp3_drives_resp3_call_reply_conversion_vr8rg() {
        // (frankenredis-vr8rg) redis.setresp(3) makes redis.call materialize
        // RESP3 frames and convert them via upstream's RESP3 Lua mapping:
        // null->nil, Double->{double=n}, Map->{map=…}, Set->{set={m=true}}.
        // The default (RESP2) path is unchanged (null->false, etc.).
        let bulk = |s: &str| RespFrame::BulkString(Some(s.as_bytes().to_vec()));
        let mut store = Store::new();
        eval_script(
            b"redis.call('zadd','z','1.5','m'); redis.call('hset','h','f','v'); \
              redis.call('sadd','s','a','b'); return 1",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        let run = |store: &mut Store, src: &[u8]| eval_script(src, &[], &[], store, 0).unwrap();

        // RESP3: null -> nil; RESP2 (default): null -> false (a boolean).
        assert_eq!(
            run(
                &mut store,
                b"redis.setresp(3); return type(redis.call('get','nokey'))"
            ),
            bulk("nil")
        );
        assert_eq!(
            run(&mut store, b"return type(redis.call('get','nokey'))"),
            bulk("boolean")
        );
        // RESP3 Double -> {double = n}.
        assert_eq!(
            run(
                &mut store,
                b"redis.setresp(3); return tostring(redis.call('zscore','z','m').double)"
            ),
            bulk("1.5")
        );
        // RESP3 Map -> {map = {k = v}}.
        assert_eq!(
            run(
                &mut store,
                b"redis.setresp(3); return redis.call('hgetall','h').map.f"
            ),
            bulk("v")
        );
        // RESP3 Set -> {set = {member = true}}.
        assert_eq!(
            run(
                &mut store,
                b"redis.setresp(3); return redis.call('smembers','s').set.a == true and 'Y' or 'N'"
            ),
            bulk("Y")
        );
        // RESP2 default: ZSCORE is a plain string, HGETALL has no `.map` field.
        assert_eq!(
            run(&mut store, b"return type(redis.call('zscore','z','m'))"),
            bulk("string")
        );
        assert_eq!(
            run(
                &mut store,
                b"return redis.call('hgetall','h').map and 'Y' or 'N'"
            ),
            bulk("N")
        );
        // setresp is per-script: a later script defaults back to RESP2.
        assert_eq!(
            run(&mut store, b"return type(redis.call('get','nokey'))"),
            bulk("boolean")
        );
    }

    #[test]
    fn lua_boolean_return_uses_resp3_under_setresp3_0gz4g() {
        // (frankenredis-0gz4g) Upstream luaReplyToRedisReply uses addReplyBool
        // for a Lua boolean once the script is on RESP3: a `#t`/`#f` Bool frame
        // for a RESP3 caller, downgraded to `:1`/`:0` for RESP2. Without
        // setresp(3) the historical mapping holds (true->:1, false->nil).
        let eval = |resp: i64, src: &[u8]| {
            let mut store = Store::new();
            store.dispatch_client_ctx.resp_protocol_version = resp;
            eval_script(src, &[], &[], &mut store, 0).unwrap()
        };
        // RESP3 caller + setresp(3): real Bool frame.
        assert_eq!(
            eval(3, b"redis.setresp(3); return true"),
            RespFrame::Bool(true)
        );
        assert_eq!(
            eval(3, b"redis.setresp(3); return false"),
            RespFrame::Bool(false)
        );
        // RESP2 caller + setresp(3): Bool downgrades to :1 / :0.
        assert_eq!(
            eval(2, b"redis.setresp(3); return true"),
            RespFrame::Integer(1)
        );
        assert_eq!(
            eval(2, b"redis.setresp(3); return false"),
            RespFrame::Integer(0)
        );
        // Default (no setresp): true -> :1, false -> nil, on both protocols.
        assert_eq!(eval(2, b"return true"), RespFrame::Integer(1));
        assert_eq!(eval(2, b"return false"), RespFrame::BulkString(None));
        assert_eq!(eval(3, b"return false"), RespFrame::BulkString(None));
        // Nested booleans convert recursively under setresp(3).
        assert_eq!(
            eval(3, b"redis.setresp(3); return {true, false, 1}"),
            RespFrame::Array(Some(vec![
                RespFrame::Bool(true),
                RespFrame::Bool(false),
                RespFrame::Integer(1),
            ]))
        );
    }

    #[test]
    fn eval_set_hint_emits_keys_not_values_e6ffo() {
        // (frankenredis-e6ffo) Upstream src/script_lua.c::
        // luaReplyToRedisReply iterates {set=t}'s inner table with
        // lua_next, discards the value, and emits the key as the set
        // member. fr previously iterated the array-part VALUES.
        //
        // Hash table → keys are strings.
        let mut store = Store::new();
        let frame = eval_script(b"return {set={a=true, b=true}}", &[], &[], &mut store, 0).unwrap();
        match frame {
            RespFrame::Array(Some(items)) => {
                assert_eq!(items.len(), 2);
                let mut got: Vec<String> = items
                    .iter()
                    .filter_map(|f| match f {
                        RespFrame::BulkString(Some(b)) => {
                            Some(String::from_utf8_lossy(b).into_owned())
                        }
                        _ => None,
                    })
                    .collect();
                got.sort();
                assert_eq!(got, vec!["a".to_string(), "b".to_string()]);
            }
            other => panic!("expected Array of 2 string keys, got {other:?}"),
        }

        // Positional table → keys are integer indices (1,2,3 — NOT
        // the string values "a","b","c").
        let mut store = Store::new();
        let frame =
            eval_script(b"return {set={\"a\",\"b\",\"c\"}}", &[], &[], &mut store, 0).unwrap();
        assert_eq!(
            frame,
            RespFrame::Array(Some(vec![
                RespFrame::Integer(1),
                RespFrame::Integer(2),
                RespFrame::Integer(3),
            ]))
        );

        // Mixed integer + string keys.
        let mut store = Store::new();
        let frame = eval_script(
            b"return {set={[\"x\"]=true, [10]=true, [\"y\"]=true}}",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        match frame {
            RespFrame::Array(Some(items)) => {
                assert_eq!(items.len(), 3);
                let has_x = items
                    .iter()
                    .any(|f| matches!(f, RespFrame::BulkString(Some(s)) if s == b"x"));
                let has_y = items
                    .iter()
                    .any(|f| matches!(f, RespFrame::BulkString(Some(s)) if s == b"y"));
                let has_10 = items.iter().any(|f| matches!(f, RespFrame::Integer(10)));
                assert!(has_x && has_y && has_10, "items={items:?}");
            }
            other => panic!("expected Array of 3 keys, got {other:?}"),
        }

        // Empty set → empty array.
        let mut store = Store::new();
        let frame = eval_script(b"return {set={}}", &[], &[], &mut store, 0).unwrap();
        assert_eq!(frame, RespFrame::Array(Some(vec![])));
    }

    #[test]
    fn eval_map_hint_flattens_to_array_for_resp2_jp7gs() {
        // (frankenredis-jp7gs) Upstream src/script_lua.c::luaReplyToRedisReply
        // only emits RESP3 Map frames when the calling connection is on
        // RESP3; RESP2 clients receive the equivalent flat 2N Array via
        // the auto-downconvert step in addReplyMapLen.
        //
        // RESP2 (default): {map={a=1,b=2}} -> flat 4-element Array.
        let mut store = Store::new();
        let frame = eval_script(b"return {map={a=1, b=2}}", &[], &[], &mut store, 0).unwrap();
        match frame {
            RespFrame::Array(Some(items)) => {
                assert_eq!(
                    items.len(),
                    4,
                    "expected flat 4-element array, got {items:?}"
                );
                // Key ordering depends on Lua's hash iteration but for two
                // string keys the entries must still be paired k/v.
                assert!(
                    matches!(&items[0], RespFrame::BulkString(Some(s)) if s == b"a" || s == b"b")
                );
                assert!(
                    matches!(&items[2], RespFrame::BulkString(Some(s)) if s == b"a" || s == b"b")
                );
            }
            other => panic!("expected Array, got {other:?}"),
        }

        // RESP3: same script returns a Map(2) frame, no downconvert.
        let mut store = Store::new();
        store.dispatch_client_ctx.resp_protocol_version = 3;
        let frame = eval_script(b"return {map={a=1, b=2}}", &[], &[], &mut store, 0).unwrap();
        match frame {
            RespFrame::Map(Some(entries)) => {
                assert_eq!(entries.len(), 2, "expected 2-entry map, got {entries:?}");
            }
            other => panic!("expected Map, got {other:?}"),
        }

        // Nested map inside an array element also downconverts in RESP2.
        let mut store = Store::new();
        let frame = eval_script(
            b"return {map={a={map={x=1}}, b=2}}",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        match frame {
            RespFrame::Array(Some(items)) => {
                assert_eq!(items.len(), 4);
                // One of the values is itself a flat 2-element array
                // ({map={x=1}} downconverted).
                let any_nested = items
                    .iter()
                    .any(|v| matches!(v, RespFrame::Array(Some(inner)) if inner.len() == 2));
                assert!(
                    any_nested,
                    "expected one nested 2-element array, got {items:?}"
                );
            }
            other => panic!("expected Array, got {other:?}"),
        }

        // Plain arrays without map/set hints unchanged in both modes.
        let mut store = Store::new();
        let frame = eval_script(b"return {1, 2, 3}", &[], &[], &mut store, 0).unwrap();
        assert!(matches!(frame, RespFrame::Array(Some(ref v)) if v.len() == 3));
        let mut store = Store::new();
        store.dispatch_client_ctx.resp_protocol_version = 3;
        let frame = eval_script(b"return {1, 2, 3}", &[], &[], &mut store, 0).unwrap();
        assert!(matches!(frame, RespFrame::Array(Some(ref v)) if v.len() == 3));
    }

    #[test]
    fn lua_to_resp_non_finite_numbers_match_upstream_llong_min_cast() {
        // Pins frankenredis-luanonfinite. Upstream
        // src/script_lua.c::luaReplyToRedisReply (line 622) does
        //   addReplyLongLong(c, (long long)lua_tonumber(lua, -1));
        // The C cast `(long long)x` for non-finite double is UB, but
        // on x86-64 GCC it consistently returns LLONG_MIN
        // (-9223372036854775808). fr's previous `*n as i64` saturated
        // differently: +inf → i64::MAX, -inf → i64::MIN, NaN → 0,
        // which made math.huge / 1/0 / 0/0 / -1/0 each return a
        // different integer than vendored.
        let mut store = Store::new();

        for src in [
            b"return math.huge".as_slice(),
            b"return 1/0".as_slice(),
            b"return -1/0".as_slice(),
            b"return -math.huge".as_slice(),
            b"return 0/0".as_slice(),
            b"return math.huge / 2".as_slice(),
            b"return math.huge - math.huge".as_slice(),
        ] {
            let frame = eval_script(src, &[], &[], &mut store, 0).unwrap();
            assert_eq!(
                frame,
                RespFrame::Integer(i64::MIN),
                "src = {:?}",
                String::from_utf8_lossy(src)
            );
        }

        // Finite numbers still truncate per upstream (long long)cast.
        for (src, expected) in [
            (b"return 3.14".as_slice(), 3),
            (b"return -3.14".as_slice(), -3),
            (b"return 1e9".as_slice(), 1_000_000_000),
            (b"return 0.5".as_slice(), 0),
            (b"return -0.5".as_slice(), 0),
            (b"return 0".as_slice(), 0),
        ] {
            let frame = eval_script(src, &[], &[], &mut store, 0).unwrap();
            assert_eq!(
                frame,
                RespFrame::Integer(expected),
                "src = {:?}",
                String::from_utf8_lossy(src)
            );
        }
    }

    #[test]
    fn lua_tonumber_strtoul_wrap_and_resp_saturation_8reid() {
        // Pins frankenredis-8reid. Two reinforcing divergences:
        //
        // 1. luaB_tonumber('-N', base) with base != 10 dispatches to
        //    strtoul, which parses the digits as unsigned and applies
        //    unsigned negation: tonumber('-5', 16) is
        //    ((unsigned long)-5) cast to a Lua number — about
        //    1.8446744073709552e19, NOT -5.
        //
        // 2. addReplyLongLong(c, (long long)lua_tonumber(L, -1))
        //    further casts that out-of-range double back to long
        //    long. The C cast is UB, but x86-64 cvttsd2si returns
        //    LLONG_MIN for any finite double outside [INT64_MIN,
        //    INT64_MAX + 1). Rust's `as i64` saturates to i64::MAX
        //    instead, so EVAL replies diverge unless lua_to_resp
        //    mimics the C cast.
        //
        // For base 10 (explicit or implicit) the upstream code path
        // is strtod / luaO_str2d, which preserves signed values:
        // tonumber('-1', 10) is -1, not a wrap.
        let mut store = Store::new();

        // strtoul-wrap branch: tonumber('-5', 16) round-trips
        // through lua_to_resp as INT64_MIN.
        for src in [
            b"return tonumber('-5', 16)".as_slice(),
            b"return tonumber('-1', 16)".as_slice(),
            b"return tonumber('-FF', 16)".as_slice(),
            b"return tonumber('-10', 2)".as_slice(),
            b"return tonumber('-1', 8)".as_slice(),
        ] {
            let frame = eval_script(src, &[], &[], &mut store, 0).unwrap();
            assert_eq!(
                frame,
                RespFrame::Integer(i64::MIN),
                "src = {:?}",
                String::from_utf8_lossy(src)
            );
        }

        // tostring of the wrapped value emits the f64 in upstream's
        // %.14g format: "1.844674407371e+19".
        let frame = eval_script(
            b"return tostring(tonumber('-5', 16))",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(
            frame,
            RespFrame::BulkString(Some(b"1.844674407371e+19".to_vec()))
        );

        // Base 10 (explicit or implicit) preserves signed values.
        for (src, expected) in [
            (b"return tonumber('-1', 10)".as_slice(), -1),
            (b"return tonumber('-255')".as_slice(), -255),
            (b"return tonumber('-0xff')".as_slice(), -255),
        ] {
            let frame = eval_script(src, &[], &[], &mut store, 0).unwrap();
            assert_eq!(
                frame,
                RespFrame::Integer(expected),
                "src = {:?}",
                String::from_utf8_lossy(src)
            );
        }

        // Non-negative inputs still parse normally.
        for (src, expected) in [
            (b"return tonumber('5', 16)".as_slice(), 5),
            (b"return tonumber('FF', 16)".as_slice(), 255),
            (b"return tonumber('10', 2)".as_slice(), 2),
        ] {
            let frame = eval_script(src, &[], &[], &mut store, 0).unwrap();
            assert_eq!(
                frame,
                RespFrame::Integer(expected),
                "src = {:?}",
                String::from_utf8_lossy(src)
            );
        }

        // Direct out-of-range double also hits lua_to_resp's
        // cvttsd2si mimic.
        let frame = eval_script(b"return 1.8446744073709552e19", &[], &[], &mut store, 0).unwrap();
        assert_eq!(frame, RespFrame::Integer(i64::MIN));
    }

    #[test]
    fn redis_call_rejects_non_string_or_number_args_with_upstream_wording() {
        // Pins frankenredis-redisargtype. Upstream
        // src/script_lua.c::luaArgsToRedisArgv calls lua_tolstring on
        // each arg; lua_tolstring returns NULL for nil / boolean /
        // table / function / thread / userdata, and the loop bails
        // with a single unified error:
        //   'Lua redis lib command arguments must be strings or integers'
        // fr previously had per-type wordings AND silently coerced
        // LuaValue::Bool to '1'/'0', so `redis.call('SET', k, true)`
        // returned OK on fr but errored on vendored.
        let mut store = Store::new();

        let expected = "Lua redis lib command arguments must be strings or integers";
        let cases: &[&[u8]] = &[
            b"return redis.call('SET', 'k', true)",
            b"return redis.call('SET', 'k', false)",
            b"return redis.call('SET', 'k', nil)",
            b"return redis.call('SET', 'k', {})",
            b"return redis.call('SET', 'k', {1,2})",
            b"return redis.call('SET', 'k', function() end)",
        ];
        for src in cases {
            let err = eval_script(src, &[], &[], &mut store, 0).expect_err(&format!(
                "expected error for {:?}",
                String::from_utf8_lossy(src)
            ));
            assert!(
                err.contains(expected),
                "wrong wording for {:?}: {err}",
                String::from_utf8_lossy(src)
            );
        }

        // Numbers and strings still work (regression check).
        let frame = eval_script(
            b"redis.call('SET', 'k', 12345) return redis.call('GET', 'k')",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"12345".to_vec())));
        let frame = eval_script(
            b"redis.call('SET', 'k', 'hello') return redis.call('GET', 'k')",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"hello".to_vec())));
    }

    #[test]
    fn lua_concat_error_carries_accessor_context() {
        // Pins frankenredis-22c3u. Lua 5.1 emits accessor-aware wording
        // for concat-of-non-string-non-number when no __concat handler
        // is found: `attempt to concatenate <accessor> (a TYPE value)`
        // where accessor is local/upvalue/global/field 'NAME' / field
        // '?'. Anonymous call sites (paren expr, call result) fall back
        // to `attempt to concatenate a TYPE value`.
        let mut store = Store::new();

        // local on LHS: tracked as 'local' inside the current function.
        let err = eval_script(b"local t = {}; return t .. 'x'", &[], &[], &mut store, 0)
            .expect_err("expected concat error");
        assert!(
            err.contains("attempt to concatenate local 't' (a table value)"),
            "wrong wording for local LHS: {err:?}"
        );

        // local on RHS still produces the accessor label.
        let err = eval_script(b"local t = {}; return 'x' .. t", &[], &[], &mut store, 0)
            .expect_err("expected concat error");
        assert!(
            err.contains("attempt to concatenate local 't' (a table value)"),
            "wrong wording for local RHS: {err:?}"
        );

        // Field-access prefix.
        let err = eval_script(
            b"local obj = {fld = {}}; return obj.fld .. 'x'",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect_err("expected concat error");
        assert!(
            err.contains("attempt to concatenate field 'fld' (a table value)"),
            "wrong wording for field: {err:?}"
        );

        // Numeric-index access → field '?'.
        let err = eval_script(
            b"local arr = {{}}; return arr[1] .. 'x'",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect_err("expected concat error");
        assert!(
            err.contains("attempt to concatenate field '?' (a table value)"),
            "wrong wording for numeric index: {err:?}"
        );

        // String-literal index resolves to the field name.
        let err = eval_script(
            b"local obj = {fld = {}}; return obj['fld'] .. 'x'",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect_err("expected concat error");
        assert!(
            err.contains("attempt to concatenate field 'fld' (a table value)"),
            "wrong wording for string-index: {err:?}"
        );

        // Call result has no accessor — falls back to anonymous form.
        let err = eval_script(
            b"local function g() return {} end; return g() .. 'x'",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect_err("expected concat error");
        assert!(
            err.contains("attempt to concatenate a table value"),
            "wrong wording for call result: {err:?}"
        );

        // Paren expression (nil): anonymous form with the actual type.
        let err = eval_script(b"return (nil) .. 'x'", &[], &[], &mut store, 0)
            .expect_err("expected concat error for nil");
        assert!(
            err.contains("attempt to concatenate a nil value"),
            "wrong wording for nil: {err:?}"
        );
    }

    #[test]
    fn lua_builtin_missing_arg_errors_match_upstream_wording() {
        // Pins frankenredis-nf29w. Lua 5.1's luaL_checktype /
        // luaL_checkany differentiate a MISSING arg slot ("got no
        // value") from an explicit nil arg ("got nil"). They also
        // route the function name through luaL_argerror, which uses
        // the AST callsite name or '?' depending on invocation context.
        // Covers the high-traffic builtins: assert, xpcall, rawget,
        // setmetatable, getmetatable.
        let mut store = Store::new();

        // assert() with no args.
        let frame = eval_script(
            b"local ok, err = pcall(assert); return err",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        let body = match frame {
            RespFrame::BulkString(Some(b)) => b,
            other => panic!("expected bulk string, got {other:?}"),
        };
        assert_eq!(body, b"bad argument #1 to '?' (value expected)");

        // assert() direct call uses 'assert' as the name.
        let err =
            eval_script(b"return assert()", &[], &[], &mut store, 0).expect_err("expected error");
        assert!(
            err.contains("user_script:1: bad argument #1 to 'assert' (value expected)"),
            "assert direct: {err:?}"
        );

        // xpcall(f) with no msgh.
        let frame = eval_script(
            b"local ok, err = pcall(xpcall, function() end); return err",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        let body = match frame {
            RespFrame::BulkString(Some(b)) => b,
            other => panic!("expected bulk string, got {other:?}"),
        };
        assert_eq!(body, b"bad argument #2 to '?' (value expected)");

        // rawget() with no args → "table expected, got no value".
        let frame = eval_script(
            b"local ok, err = pcall(rawget); return err",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        let body = match frame {
            RespFrame::BulkString(Some(b)) => b,
            other => panic!("expected bulk string, got {other:?}"),
        };
        assert_eq!(
            body,
            b"bad argument #1 to '?' (table expected, got no value)"
        );

        // rawget(t) with no key → "value expected" at slot 2.
        let frame = eval_script(
            b"local ok, err = pcall(rawget, {}); return err",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        let body = match frame {
            RespFrame::BulkString(Some(b)) => b,
            other => panic!("expected bulk string, got {other:?}"),
        };
        assert_eq!(body, b"bad argument #2 to '?' (value expected)");

        // setmetatable() with no args.
        let frame = eval_script(
            b"local ok, err = pcall(setmetatable); return err",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        let body = match frame {
            RespFrame::BulkString(Some(b)) => b,
            other => panic!("expected bulk string, got {other:?}"),
        };
        assert_eq!(
            body,
            b"bad argument #1 to '?' (table expected, got no value)"
        );

        // getmetatable() with no args.
        let frame = eval_script(
            b"local ok, err = pcall(getmetatable); return err",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        let body = match frame {
            RespFrame::BulkString(Some(b)) => b,
            other => panic!("expected bulk string, got {other:?}"),
        };
        assert_eq!(body, b"bad argument #1 to '?' (value expected)");

        // Regression: good args still work.
        let frame = eval_script(
            b"local t = {x = 1}; return rawget(t, 'x')",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::Integer(1));
    }

    #[test]
    fn lua_tostring_type_with_no_args_error_value_expected() {
        // Pins frankenredis-6iqkt. Lua 5.1 lbaselib.c::luaB_tostring
        // and luaB_type both call luaL_checkany before any other
        // logic, so a missing arg raises
        // "bad argument #1 to '?' (value expected)" via pcall callback
        // or "user_script:1: bad argument #1 to '<name>' (value expected)"
        // when called directly.
        let mut store = Store::new();

        // Direct call: name resolves to 'tostring' / 'type'.
        let err =
            eval_script(b"return tostring()", &[], &[], &mut store, 0).expect_err("expected error");
        assert!(
            err.contains("user_script:1: bad argument #1 to 'tostring' (value expected)"),
            "tostring direct: {err:?}"
        );

        let err =
            eval_script(b"return type()", &[], &[], &mut store, 0).expect_err("expected error");
        assert!(
            err.contains("user_script:1: bad argument #1 to 'type' (value expected)"),
            "type direct: {err:?}"
        );

        // pcall callback: name is '?' with no prefix.
        let frame = eval_script(
            b"local ok, err = pcall(tostring); return err",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        let body = match frame {
            RespFrame::BulkString(Some(b)) => b,
            other => panic!("expected bulk string, got {other:?}"),
        };
        assert_eq!(body, b"bad argument #1 to '?' (value expected)");

        // Explicit nil arg is NOT missing — tostring(nil) returns "nil".
        let frame = eval_script(b"return tostring(nil)", &[], &[], &mut store, 0).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"nil".to_vec())));

        // Happy paths.
        let frame = eval_script(b"return tostring(42)", &[], &[], &mut store, 0).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"42".to_vec())));

        let frame = eval_script(b"return type({})", &[], &[], &mut store, 0).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"table".to_vec())));
    }

    #[test]
    fn lua_builtin_argerror_uses_callsite_name_or_questionmark() {
        // Pins frankenredis-557p3. Lua 5.1's luaL_argerror uses
        // lua_getinfo("n.name") to derive the function name reported in
        // bad-argument errors:
        //   - Direct call `select(...)`  → name 'select', user_script:1: prefix
        //   - Aliased  `local f=select; f(...)` → name 'f', same prefix
        //   - Field    `t.s(...)`               → name 's', same prefix
        //   - Indirect via pcall callback       → name '?', no prefix (no
        //                                          Lua activation record)
        let mut store = Store::new();

        // Direct call.
        let err = eval_script(b"return select(0, 'a')", &[], &[], &mut store, 0)
            .expect_err("expected error");
        assert!(
            err.contains("user_script:1: bad argument #1 to 'select' (index out of range)"),
            "direct call wording: {err:?}"
        );

        // Local alias surfaces the local variable's name.
        let err = eval_script(
            b"local f = select; return f(0, 'a')",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect_err("expected error");
        assert!(
            err.contains("user_script:1: bad argument #1 to 'f' (index out of range)"),
            "alias wording: {err:?}"
        );

        // Field call uses the field name.
        let err = eval_script(
            b"local t = {s = select}; return t.s(0, 'a')",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect_err("expected error");
        assert!(
            err.contains("user_script:1: bad argument #1 to 's' (index out of range)"),
            "field-call wording: {err:?}"
        );

        // pcall(select, ...) loses the AST context: name is '?' and
        // no user_script:1: prefix.
        let frame = eval_script(
            b"local ok, err = pcall(select, 0, 'a'); return err",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        let body = match frame {
            RespFrame::BulkString(Some(b)) => b,
            other => panic!("expected bulk string, got {other:?}"),
        };
        let s = String::from_utf8_lossy(&body);
        assert!(
            s == "bad argument #1 to '?' (index out of range)",
            "pcall-callback wording: {s:?}"
        );
    }

    #[test]
    fn lua_loadstring_runtime_errors_carry_chunk_label_prefix() {
        // Pins frankenredis-ycaog. Lua 5.1 / vendored Redis 7.2.4 use
        // the loadstring chunk's label (`[string "SOURCE"]` or the
        // `=NAME` / `@NAME` form) for ALL runtime errors raised from
        // inside that chunk, including from nested function definitions.
        let mut store = Store::new();

        // Direct error: nonexistent global access in the chunk.
        let frame = eval_script(
            b"local f = loadstring('return undef'); local ok, err = pcall(f); return err",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        let body = match frame {
            RespFrame::BulkString(Some(b)) => b,
            other => panic!("expected bulk string, got {other:?}"),
        };
        let s = String::from_utf8_lossy(&body);
        assert!(
            s.starts_with("[string \"return undef\"]:1: ") && s.contains("undef"),
            "default chunk label missing: {s:?}"
        );

        // Custom chunk name.
        let frame = eval_script(
            b"local f = loadstring('return undef', 'mychunk'); local ok, err = pcall(f); return err",
            &[], &[], &mut store, 0,
        ).unwrap();
        let body = match frame {
            RespFrame::BulkString(Some(b)) => b,
            other => panic!("expected bulk string, got {other:?}"),
        };
        let s = String::from_utf8_lossy(&body);
        assert!(
            s.starts_with("[string \"mychunk\"]:1: "),
            "named chunk label missing: {s:?}"
        );

        // `=NAME` prefix strips the brackets.
        let frame = eval_script(
            b"local f = loadstring('return undef', '=mn'); local ok, err = pcall(f); return err",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        let body = match frame {
            RespFrame::BulkString(Some(b)) => b,
            other => panic!("expected bulk string, got {other:?}"),
        };
        let s = String::from_utf8_lossy(&body);
        assert!(
            s.starts_with("mn:1: "),
            "= prefix should strip brackets: {s:?}"
        );

        // Nested function defined inside the chunk inherits the label.
        let frame = eval_script(
            b"local f = loadstring('local function inner() return undef end; return inner()', 'mn'); local ok, err = pcall(f); return err",
            &[], &[], &mut store, 0,
        ).unwrap();
        let body = match frame {
            RespFrame::BulkString(Some(b)) => b,
            other => panic!("expected bulk string, got {other:?}"),
        };
        let s = String::from_utf8_lossy(&body);
        assert!(
            s.starts_with("[string \"mn\"]:1: "),
            "nested function should inherit chunk label: {s:?}"
        );

        // Errors raised OUTSIDE any loaded chunk still use user_script:.
        let frame = eval_script(
            b"local ok, err = pcall(function() return undef end); return err",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        let body = match frame {
            RespFrame::BulkString(Some(b)) => b,
            other => panic!("expected bulk string, got {other:?}"),
        };
        let s = String::from_utf8_lossy(&body);
        assert!(
            s.starts_with("user_script:1: "),
            "outer error should keep user_script: prefix: {s:?}"
        );
    }

    #[test]
    fn lua_select_bad_argument_errors_match_upstream_wording() {
        // Pins frankenredis-w3wkp. Lua 5.1's lbaselib.c::luaB_select
        // uses luaL_argerror, which produces:
        //   "user_script:1: bad argument #1 to 'select' (REASON)"
        // where REASON is one of:
        //   - "number expected, got TYPE" for non-coercible-to-number args
        //   - "index out of range" for 0, < -arg_count, or NaN/-inf
        // Fractional indices truncate toward zero (1.5 → 1).
        let mut store = Store::new();

        // Direct call: NaN, zero, large negative all trip "index out of range".
        for (src, _why) in [
            (b"return select(0, 'a')" as &[u8], "zero"),
            (b"return select(-5, 'a', 'b')", "neg-out-of-range"),
            (b"return select(0/0, 'a')", "nan"),
        ] {
            let err = eval_script(src, &[], &[], &mut store, 0).expect_err(_why);
            assert!(
                err.contains("user_script:1: bad argument #1 to 'select' (index out of range)"),
                "wrong wording for {_why}: {err:?}"
            );
        }

        // Non-numeric arg types produce "number expected, got TYPE".
        for (src, want_ty) in [
            (b"return select('xyz', 'a')" as &[u8], "string"),
            (b"return select({}, 'a')", "table"),
            (b"return select(true, 'a')", "boolean"),
        ] {
            let err = eval_script(src, &[], &[], &mut store, 0).expect_err("bad arg");
            let want = format!(
                "user_script:1: bad argument #1 to 'select' (number expected, got {want_ty})"
            );
            assert!(
                err.contains(&want),
                "wrong wording for non-numeric {want_ty}: {err:?}"
            );
        }

        // Fractional truncation: select(1.5, 'a', 'b') is select(1, ...).
        let frame = eval_script(b"return select(1.5, 'a', 'b')", &[], &[], &mut store, 0).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"a".to_vec())));

        // Happy paths still work.
        let frame =
            eval_script(b"return select(2, 'a', 'b', 'c')", &[], &[], &mut store, 0).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"b".to_vec())));

        let frame = eval_script(
            b"return select('#', 'a', 'b', 'c')",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::Integer(3));

        let frame =
            eval_script(b"return select(-1, 'a', 'b', 'c')", &[], &[], &mut store, 0).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"c".to_vec())));
    }

    #[test]
    fn lua_g_global_table_exposes_sandboxed_globals() {
        // Pins frankenredis-u24vv. Lua 5.1 / vendored Redis 7.2.4 expose
        // the script's globals table as `_G`. The sandbox enforces:
        //  - `_G` is a table; `type(_G) == 'table'`.
        //  - `_G._G == _G` self-reference.
        //  - Known globals like `_G.tostring` resolve to the same
        //    function as the bare name.
        //  - Missing globals (`_G.undef`) raise the same
        //    "Script attempted to access nonexistent global variable"
        //    error as bare-name reads.
        //  - Writes raise "Attempt to modify a readonly table".
        //  - pairs(_G) iterates every registered global.
        let mut store = Store::new();

        let frame = eval_script(b"return type(_G)", &[], &[], &mut store, 0).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"table".to_vec())));

        let frame = eval_script(b"return tostring(_G._G == _G)", &[], &[], &mut store, 0).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"true".to_vec())));

        let frame = eval_script(b"return type(_G.tostring)", &[], &[], &mut store, 0).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"function".to_vec())));

        let err = eval_script(b"return _G.undef_xyz", &[], &[], &mut store, 0)
            .expect_err("expected nonexistent-global error for _G.undef_xyz");
        assert!(
            err.contains("Script attempted to access nonexistent global variable")
                && err.contains("undef_xyz"),
            "wrong wording for _G.undef: {err:?}"
        );

        let err = eval_script(b"_G.x = 1", &[], &[], &mut store, 0)
            .expect_err("expected readonly error for _G assignment");
        assert!(
            err.contains("Attempt to modify a readonly table"),
            "wrong wording for _G assign: {err:?}"
        );

        // pairs(_G) iterates the snapshot — must include many globals
        // (math, string, redis, _G itself, etc.). Lua boolean `true`
        // returned to Redis becomes Integer(1).
        let frame = eval_script(
            b"local count = 0; for k, v in pairs(_G) do count = count + 1 end; return count > 20",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::Integer(1));
    }

    #[test]
    fn lua_getfenv_setfenv_sandbox_surface_cp1gs() {
        // (frankenredis-cp1gs) Redis 7.2.4 exposes Lua 5.1 getfenv and
        // setfenv in the sandbox. setfenv must affect subsequent
        // unresolved global reads/writes, not just exist as a stub.
        let mut store = Store::new();

        let frame = eval_script(
            b"return type(getfenv)..':'..type(setfenv)..':'..type(getfenv(0))..':'..tostring(getfenv(0) == _G)",
            &[], &[], &mut store, 0,
        )
        .unwrap();
        assert_eq!(
            frame,
            RespFrame::BulkString(Some(b"function:function:table:true".to_vec()))
        );

        let frame = eval_script(
            b"setfenv(1, {answer = 42}); return answer",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::Integer(42));

        let frame = eval_script(
            b"local env = {answer = 41}; setfenv(1, env); answer = answer + 1; return env.answer",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::Integer(42));

        let frame = eval_script(
            b"local env = {answer = 99}; local f = function() return answer end; setfenv(f, env); return tostring(getfenv(f) == env)..':'..f()",
            &[], &[], &mut store, 0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"true:99".to_vec())));

        let frame = eval_script(
            b"local ok, e = pcall(setfenv, 2, {}); return tostring(ok)..':'..tostring(e)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(
            frame,
            RespFrame::BulkString(Some(
                b"false:bad argument #1 to '?' (invalid level)".to_vec(),
            ))
        );
    }

    #[test]
    fn lua_tostring_dispatches_to_metamethod_for_tables() {
        // Pins frankenredis-2ddgn (sub-bead of frankenredis-00gjf).
        // Lua 5.1 lbaselib.c::luaB_tostring consults __tostring via
        // luaL_callmeta BEFORE doing the default "table: 0x..." form.
        // Non-string return values are passed through as-is — vendored
        // does not coerce __tostring output to string. Lua 5.1's `#`
        // operator does NOT call __len on tables (only userdata), so
        // fr's existing length behaviour is left untouched.
        let mut store = Store::new();

        // Custom __tostring is used.
        let frame = eval_script(
            b"local t = setmetatable({}, {__tostring=function() return 'custom' end}); return tostring(t)",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"custom".to_vec())));

        // The table itself is passed as the single arg.
        let frame = eval_script(
            b"local t = setmetatable({}, {__tostring=function(self) return type(self) end}); return tostring(t)",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"table".to_vec())));

        // No __tostring → standard "table: 0x..." form (just check the
        // prefix; the hash suffix is non-deterministic).
        let frame = eval_script(
            b"local t = {}; return tostring(t):sub(1, 7)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"table: ".to_vec())));

        // Numbers / strings keep their existing tostring path.
        let frame = eval_script(b"return tostring(42)", &[], &[], &mut store, 0).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"42".to_vec())));

        // # on tables ignores __len (matches Lua 5.1; __len fires for
        // userdata, which Redis 7.2 scripts can't create).
        let frame = eval_script(
            b"local t = setmetatable({1,2,3,4,5,6,7,8,9,10}, {__len=function() return 42 end}); return #t",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::Integer(10));
    }

    #[test]
    fn lua_comparison_metamethods_dispatch_on_matching_table_types() {
        // Pins frankenredis-ijlzv (sub-bead of frankenredis-00gjf).
        // Lua 5.1 dispatches ==/~= to __eq only when both operands are
        // tables that share a __eq metamethod (vendored detects shared
        // metatable). </>/<=/>= dispatch to __lt/__le on same-type
        // operands; > and >= swap args, and <= falls back to
        // `not __lt(b, a)` when __le is missing.
        let mut store = Store::new();

        // Shared metatable: __eq fires.
        let frame = eval_script(
            b"local mt={__eq=function(a,b) return a.id==b.id end}; local a=setmetatable({id=1},mt); local b=setmetatable({id=1},mt); return tostring(a==b)",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"true".to_vec())));

        // Different metatables (even with __eq each) → __eq NOT fired.
        let frame = eval_script(
            b"local a=setmetatable({}, {__eq=function() return true end}); local b=setmetatable({}, {__eq=function() return true end}); return tostring(a==b)",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"false".to_vec())));

        // Raw equal (same table identity) bypasses __eq entirely.
        let frame = eval_script(
            b"local hit=0; local mt={__eq=function() hit=hit+1; return false end}; local a=setmetatable({}, mt); return tostring(a==a)..','..tostring(hit)",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"true,0".to_vec())));

        // ~= inverts __eq result.
        let frame = eval_script(
            b"local mt={__eq=function() return true end}; local a=setmetatable({}, mt); local b=setmetatable({}, mt); return tostring(a~=b)",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"false".to_vec())));

        // < on tables dispatches __lt.
        let frame = eval_script(
            b"local mt={__lt=function(x,y) return x.v<y.v end}; local a=setmetatable({v=1},mt); local b=setmetatable({v=2},mt); return tostring(a<b)..','..tostring(b<a)",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"true,false".to_vec())));

        // > swaps the args fed to __lt.
        let frame = eval_script(
            b"local seen={}; local mt={__lt=function(x,y) table.insert(seen,x.tag..'<'..y.tag); return false end}; local a=setmetatable({tag='A'},mt); local b=setmetatable({tag='B'},mt); local r=a>b; return seen[1]..':'..tostring(r)",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"B<A:false".to_vec())));

        // <= dispatches __le when present.
        let frame = eval_script(
            b"local mt={__le=function(x,y) return x.v<=y.v end}; local a=setmetatable({v=1},mt); local b=setmetatable({v=1},mt); return tostring(a<=b)",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"true".to_vec())));

        // <= falls back to `not __lt(b, a)` when only __lt is defined.
        let frame = eval_script(
            b"local mt={__lt=function(x,y) return x.v<y.v end}; local a=setmetatable({v=1},mt); local b=setmetatable({v=2},mt); return tostring(a<=b)",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"true".to_vec())));

        // Cross-type comparison (table vs number) still errors even
        // when the table has __lt.
        let err = eval_script(
            b"local t = setmetatable({}, {__lt=function() return true end}); return t < 1",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect_err("expected error for cross-type compare");
        assert!(
            err.contains("attempt to compare") && err.contains("table") && err.contains("number"),
            "wrong wording for cross-type compare: {err:?}"
        );

        // Non-callable __lt raises the standard call error.
        let err = eval_script(
            b"local mt={__lt=42}; local a=setmetatable({}, mt); local b=setmetatable({}, mt); return a < b",
            &[], &[], &mut store, 0,
        ).expect_err("expected error for non-callable __lt");
        assert!(
            err.contains("attempt to call") && err.contains("number"),
            "non-callable __lt should raise call-error: {err:?}"
        );

        // Plain number compare keeps working with zero overhead.
        let frame = eval_script(
            b"return tostring(1 < 2) .. ',' .. tostring(3 >= 3)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"true,true".to_vec())));
    }

    #[test]
    fn lua_arithmetic_metamethods_dispatch_on_non_numeric_operands() {
        // Pins frankenredis-mdxsk (sub-bead of frankenredis-00gjf).
        // Lua 5.1 dispatches arithmetic operators (+, -, *, /, %, ^,
        // unary -) to __add/__sub/__mul/__div/__mod/__pow/__unm when
        // at least one operand fails the implicit number coercion.
        let mut store = Store::new();

        // __add fires; args are (table, number).
        let frame = eval_script(
            b"local t = setmetatable({}, {__add=function(a, b) return type(a) .. '+' .. type(b) end}); return t + 1",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"table+number".to_vec())));

        // RHS-only metatable still fires.
        let frame = eval_script(
            b"local t = setmetatable({}, {__add=function(a, b) return type(a) .. '+' .. type(b) end}); return 1 + t",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"number+table".to_vec())));

        // Other binary arithmetic ops.
        for (src, want) in [
            (
                b"local t = setmetatable({}, {__sub=function() return 's' end}); return t - 1"
                    as &[u8],
                b"s" as &[u8],
            ),
            (
                b"local t = setmetatable({}, {__mul=function() return 'm' end}); return t * 2",
                b"m",
            ),
            (
                b"local t = setmetatable({}, {__div=function() return 'd' end}); return t / 2",
                b"d",
            ),
            (
                b"local t = setmetatable({}, {__mod=function() return 'o' end}); return t % 2",
                b"o",
            ),
            (
                b"local t = setmetatable({}, {__pow=function() return 'p' end}); return t ^ 2",
                b"p",
            ),
        ] {
            let frame = eval_script(src, &[], &[], &mut store, 0).unwrap();
            assert_eq!(
                frame,
                RespFrame::BulkString(Some(want.to_vec())),
                "wrong dispatch for {:?}",
                String::from_utf8_lossy(src)
            );
        }

        // __unm (unary minus) receives the operand as its sole arg.
        let frame = eval_script(
            b"local t = setmetatable({}, {__unm=function(a) return type(a) end}); return -t",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"table".to_vec())));

        // LHS metatable wins when both have __add.
        let frame = eval_script(
            b"local L = setmetatable({}, {__add=function() return 'L' end}); local R = setmetatable({}, {__add=function() return 'R' end}); return L + R",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"L".to_vec())));

        // Numeric strings still trigger metamethod dispatch when the
        // OTHER operand fails to_number (the str is numeric, table is not).
        let frame = eval_script(
            b"local t = setmetatable({}, {__add=function() return 'META' end}); return '1' + t",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"META".to_vec())));

        // Pure-number arithmetic doesn't pay the metamethod-lookup cost.
        let frame = eval_script(b"return 2 + 3", &[], &[], &mut store, 0).unwrap();
        assert_eq!(frame, RespFrame::Integer(5));

        // Non-callable __add raises the standard "attempt to call" error.
        let err = eval_script(
            b"local t = setmetatable({}, {__add='nope'}); return t + 1",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect_err("expected error for string-__add");
        assert!(
            err.contains("attempt to call") && err.contains("string"),
            "non-callable __add should raise call-error: {err:?}"
        );
    }

    #[test]
    fn lua_concat_metamethod_dispatches_on_table_operands() {
        // Pins frankenredis-hqevr (sub-bead of frankenredis-00gjf).
        // Lua 5.1's `..` operator invokes __concat(left, right) when
        // at least one operand is not a string/number and either has
        // a __concat metamethod. LHS metatable is checked first.
        let mut store = Store::new();

        // LHS table provides __concat.
        let frame = eval_script(
            b"local t = setmetatable({}, {__concat=function(a, b) return 'lhs_' .. tostring(b) end}); return t .. 'x'",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"lhs_x".to_vec())));

        // RHS table provides __concat — args still in (left, right) order.
        let frame = eval_script(
            b"local t = setmetatable({}, {__concat=function(a, b) return 'rhs_' .. tostring(a) end}); return 'x' .. t",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"rhs_x".to_vec())));

        // Both have __concat → LHS wins.
        let frame = eval_script(
            b"local L = setmetatable({}, {__concat=function() return 'L' end}); local R = setmetatable({}, {__concat=function() return 'R' end}); return L .. R",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"L".to_vec())));

        // Types reported in handler args match the actual operand types.
        let frame = eval_script(
            b"local t = setmetatable({}, {__concat=function(a, b) return type(a) .. '+' .. type(b) end}); return t .. 42",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"table+number".to_vec())));

        // Multi-return: only the first value is used (matches Lua 5.1).
        let frame = eval_script(
            b"local t = setmetatable({}, {__concat=function() return 'A', 'B', 'C' end}); return t .. 'x' .. 'y'",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"A".to_vec())));

        // No metamethod present → standard concat error.
        let err = eval_script(b"local t = {}; return t .. 'x'", &[], &[], &mut store, 0)
            .expect_err("expected error for plain-table concat");
        assert!(
            err.contains("attempt to concatenate") && err.contains("table value"),
            "wrong wording for plain-table concat: {err:?}"
        );

        // Non-callable __concat → call_function emits "attempt to call
        // a TYPE value" (matches vendored).
        let err = eval_script(
            b"local t = setmetatable({}, {__concat='nope'}); return t .. 'x'",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect_err("expected error for string-__concat");
        assert!(
            err.contains("attempt to call") && err.contains("string"),
            "non-callable __concat should raise call-error: {err:?}"
        );
    }

    #[test]
    fn lua_newindex_metamethod_intercepts_table_assignment() {
        // Pins frankenredis-9f16h (sub-bead of frankenredis-00gjf).
        // Lua 5.1 invokes __newindex(table, key, value) for assignments
        // to MISSING keys; existing keys bypass the metamethod entirely.
        // __newindex can be a table (cascade) or a function (callback);
        // non-nil non-callable handlers raise the standard
        // "attempt to index a TYPE value" error. rawset always bypasses.
        let mut store = Store::new();

        // Function handler captures the assignment.
        let frame = eval_script(
            b"local stored = {}; local t = setmetatable({}, {__newindex=function(_, k, v) stored[k] = v end}); t.x = 1; return stored.x",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::Integer(1));

        // Existing keys bypass __newindex (handler must NOT fire).
        let frame = eval_script(
            b"local hit = 0; local t = setmetatable({x=10}, {__newindex=function() hit=hit+1 end}); t.x = 99; return tostring(hit)..','..tostring(t.x)",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"0,99".to_vec())));

        // Table handler cascades writes into the delegate.
        let frame = eval_script(
            b"local back = {}; local t = setmetatable({}, {__newindex=back}); t.x = 1; return tostring(back.x)..','..tostring(rawget(t, 'x'))",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"1,nil".to_vec())));

        // rawset always bypasses __newindex.
        let frame = eval_script(
            b"local hit = 0; local t = setmetatable({}, {__newindex=function() hit=hit+1 end}); rawset(t, 'x', 1); return tostring(hit)..','..tostring(t.x)",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"0,1".to_vec())));

        // Non-callable __newindex (e.g. string) errors per vendored.
        let err = eval_script(
            b"local t = setmetatable({}, {__newindex='nope'}); t.x = 1",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect_err("expected error for string-__newindex");
        assert!(
            err.contains("attempt to index") && err.contains("string"),
            "wrong wording for non-callable __newindex: {err:?}"
        );

        // Chained table __newindex: outer → mid → back; only back gets it.
        let frame = eval_script(
            b"local back = {}; local mid = setmetatable({}, {__newindex=back}); local outer = setmetatable({}, {__newindex=mid}); outer.x = 1; return tostring(back.x)..','..tostring(rawget(mid, 'x'))..','..tostring(rawget(outer, 'x'))",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"1,nil,nil".to_vec())));

        // Bracket-style assignment also triggers __newindex.
        let frame = eval_script(
            b"local stored = {}; local t = setmetatable({}, {__newindex=function(_, k, v) stored[k] = v end}); t['k'] = 'v'; return stored.k",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"v".to_vec())));
    }

    #[test]
    fn lua_call_metamethod_makes_tables_callable() {
        // Pins frankenredis-2c7hj (sub-bead of frankenredis-00gjf).
        // Lua 5.1 lets `t(args...)` invoke `t`'s metatable __call as
        // `__call(t, args...)` whenever __call is a function. A
        // non-function __call (table/string/number/bool/nil) still
        // produces the standard "attempt to call ... (a table value)"
        // error — vendored does NOT recursively chain through a
        // table-valued __call.
        let mut store = Store::new();

        // Basic call: table with __call function.
        let frame = eval_script(
            b"local t = setmetatable({}, {__call=function(_, a) return a*2 end}); return t(21)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::Integer(42));

        // The table itself is passed as the first arg (self).
        let frame = eval_script(
            b"local t = setmetatable({x=99}, {__call=function(self) return self.x end}); return t()",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::Integer(99));

        // Multi-arg passed through after the implicit table.
        let frame = eval_script(
            b"local t = setmetatable({}, {__call=function(_, a, b, c) return a+b+c end}); return t(10, 20, 30)",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::Integer(60));

        // No metatable at all → unchanged "attempt to call ..." error.
        let err = eval_script(b"local t = {}; return t()", &[], &[], &mut store, 0)
            .expect_err("expected error for plain table call");
        assert!(
            err.contains("attempt to call") && err.contains("table value"),
            "wrong error wording for plain-table call: {err:?}"
        );

        // __call=non_function → same error wording, NOT chained.
        let err = eval_script(
            b"local t = setmetatable({}, {__call='nope'}); return t()",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect_err("expected error for string-__call");
        assert!(
            err.contains("attempt to call") && err.contains("table value"),
            "non-function __call should NOT chain: {err:?}"
        );

        // __call=table_with_callable_call → still errors (no recursive chain).
        let err = eval_script(
            b"local t2 = setmetatable({}, {__call=function() return 'inner' end}); local t1 = setmetatable({}, {__call=t2}); return t1()",
            &[], &[], &mut store, 0,
        )
        .expect_err("expected error for table-valued __call");
        assert!(
            err.contains("attempt to call") && err.contains("table value"),
            "table-valued __call should NOT chain: {err:?}"
        );

        // pcall sees the callable-table-as-function semantics.
        let frame = eval_script(
            b"local t = setmetatable({}, {__call=function() return 'p' end}); local ok, v = pcall(t); return tostring(ok)..':'..tostring(v)",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"true:p".to_vec())));
    }

    #[test]
    fn lua_index_metamethod_as_function_is_invoked() {
        // Pins frankenredis-vhbp3 (sub-bead of frankenredis-00gjf
        // metamethods epic). Lua 5.1 invokes __index(table, key) when a
        // raw lookup misses and the metatable's __index slot is callable.
        let mut store = Store::new();

        // Basic invocation: missing key → __index function called with
        // (table, key) and result returned.
        let frame = eval_script(
            b"local t = setmetatable({}, {__index=function(_, k) return 'meta_' .. k end}); return t.foo",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"meta_foo".to_vec())));

        // Existing keys bypass __index.
        let frame = eval_script(
            b"local t = setmetatable({x=10}, {__index=function(_, k) return 99 end}); return t.x",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::Integer(10));

        // Bracket-style indexing also triggers __index.
        let frame = eval_script(
            b"local t = setmetatable({}, {__index=function(_, k) return 'b_' .. k end}); return t['hi']",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"b_hi".to_vec())));

        // Method dispatch: __index returns a function that is then
        // called as `t:method()` — the self arg is the original table.
        let frame = eval_script(
            b"local t = setmetatable({}, {__index=function(_, k) return function(self) return 'm_' .. k end end}); return t:greet()",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"m_greet".to_vec())));

        // rawget bypasses __index entirely.
        let frame = eval_script(
            b"local t = setmetatable({}, {__index=function(_, k) return 'meta' end}); return tostring(rawget(t, 'foo'))",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"nil".to_vec())));

        // Chained metatables: table-__index then function-__index.
        let frame = eval_script(
            b"local a = setmetatable({}, {__index=function(_, k) return 'fn_' .. k end}); local b = {x=1}; setmetatable(b, {__index=a}); return b.foo",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"fn_foo".to_vec())));

        // Multi-return: only the first value is used (matches Lua 5.1).
        let frame = eval_script(
            b"local t = setmetatable({}, {__index=function(_, k) return 1, 2, 3 end}); return t.foo",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::Integer(1));

        // __index function returns nil → script sees nil.
        let frame = eval_script(
            b"local t = setmetatable({}, {__index=function() return nil end}); return tostring(t.foo)",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"nil".to_vec())));
    }

    #[test]
    fn lua_table_constructor_expands_trailing_multi_values() {
        // Pins frankenredis-d4vlx. Lua 5.1 table constructors expand
        // the LAST FIELD's multi-value (varargs or call result) into
        // multiple positional entries; non-last positions take only
        // the first value.
        let mut store = Store::new();

        // Last-positional `...` expands to all varargs.
        let frame = eval_script(
            b"local function f(...) local t = {...}; return #t end; return f('a','b','c')",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::Integer(3));

        // Last-positional `...` after explicit fields: existing entries
        // plus the expanded varargs.
        let frame = eval_script(
            b"local function f(...) local t = {1, 2, ...}; return #t end; return f('a','b','c')",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::Integer(5));

        // Non-last `...` takes only the first value.
        let frame = eval_script(
            b"local function f(...) local t = {..., 99}; return #t end; return f('a','b','c')",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::Integer(2));

        // Last-positional function call expands all return values.
        let frame = eval_script(
            b"local function g() return 1, 2, 3 end; local t = {g()}; return #t",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::Integer(3));

        // Non-last function call takes only the first return value.
        let frame = eval_script(
            b"local function g() return 1, 2, 3 end; local t = {g(), 99}; return #t",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::Integer(2));

        // Method call as last positional also expands (single value
        // for string:upper, which only returns one value).
        let frame = eval_script(
            b"local s = 'abc'; local t = {'x', s:upper()}; return tostring(t[1])..','..tostring(t[2])",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"x,ABC".to_vec())));

        // Named fields don't count toward # but coexist with positional
        // varargs expansion.
        let frame = eval_script(
            b"local function f(...) local t = {x=1, ...}; return #t..':'..tostring(t.x) end; return f('a','b','c')",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"3:1".to_vec())));

        // Empty varargs yields an empty table.
        let frame = eval_script(
            b"local function f(...) local t = {...}; return #t end; return f()",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::Integer(0));

        // When `...` is NOT the last field (a named field follows it),
        // only the first vararg is taken.
        let frame = eval_script(
            b"local function f(...) local t = {..., x=1}; return #t..':'..tostring(t.x) end; return f('a','b','c')",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"1:1".to_vec())));
    }

    #[test]
    fn lua_loadstring_parses_chunks_into_callable_functions() {
        // Pins frankenredis-cfflo. Lua 5.1's loadstring(s [, chunkname])
        // parses `s` as a chunk and returns the resulting function (or
        // nil + errmsg on parse failure). `load(non_function)` errors
        // with vendored's "bad argument #1 to '?' ..." wording.
        let mut store = Store::new();

        // Success path: simple chunk returning a constant.
        let frame = eval_script(
            b"local f = loadstring('return 42'); return f()",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::Integer(42));

        // Success path: chunk that uses varargs.
        let frame = eval_script(
            b"local f = loadstring('local a,b=...; return a+b'); return f(2, 3)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::Integer(5));

        // Parse failure: returns nil + a chunk-labelled error string.
        let frame = eval_script(
            b"local f, err = loadstring('!!!'); return tostring(f) .. ';' .. tostring(err)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        let body = match frame {
            RespFrame::BulkString(Some(b)) => b,
            other => panic!("expected bulk string, got {other:?}"),
        };
        let s = String::from_utf8_lossy(&body);
        assert!(s.starts_with("nil;"), "expected nil prefix, got {s:?}");
        assert!(
            s.contains("[string \"!!!\"]:1:"),
            "expected default chunk label, got {s:?}"
        );

        // Custom chunk name produces `[string "NAME"]:1:` brackets.
        let frame = eval_script(
            b"local f, err = loadstring('!!!', 'myname'); return tostring(err)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        let body = match frame {
            RespFrame::BulkString(Some(b)) => b,
            other => panic!("expected bulk string, got {other:?}"),
        };
        let s = String::from_utf8_lossy(&body);
        assert!(
            s.contains("[string \"myname\"]:1:"),
            "expected bracketed chunk label, got {s:?}"
        );

        // `=NAME` and `@NAME` prefixes strip the brackets.
        for prefix in ["=", "@"] {
            let src =
                format!("local f, err = loadstring('!!!', '{prefix}myname'); return tostring(err)");
            let frame = eval_script(src.as_bytes(), &[], &[], &mut store, 0).unwrap();
            let body = match frame {
                RespFrame::BulkString(Some(b)) => b,
                other => panic!("expected bulk string, got {other:?}"),
            };
            let s = String::from_utf8_lossy(&body);
            assert!(
                s.contains("myname:1:") && !s.contains("[string"),
                "expected unbracketed label for prefix {prefix:?}, got {s:?}"
            );
        }

        // Multi-line source: default chunk label truncates after the
        // first line with `...`.
        let frame = eval_script(
            b"local f, err = loadstring('return 1\nreturn 2\n!!!'); return tostring(err)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        let body = match frame {
            RespFrame::BulkString(Some(b)) => b,
            other => panic!("expected bulk string, got {other:?}"),
        };
        let s = String::from_utf8_lossy(&body);
        assert!(
            s.contains("[string \"return 1...\"]"),
            "expected truncated default label, got {s:?}"
        );

        // type(loadstring) / type(load) are both 'function'.
        let frame = eval_script(b"return type(loadstring)", &[], &[], &mut store, 0).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"function".to_vec())));
        let frame = eval_script(b"return type(load)", &[], &[], &mut store, 0).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"function".to_vec())));

        // load(non_function) — direct call carries the user_script:1:
        // prefix and reports the function name as 'load'; calls via
        // pcall lose the call site so the name reports as '?' with no
        // prefix. (frankenredis-4zmde — live probe vs vendored 7.2.4
        // confirmed both shapes.)
        //
        // pcall-invoked: '?' name, no prefix.
        let frame = eval_script(
            b"local ok, err = pcall(load, 'src'); return tostring(err)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        let body = match frame {
            RespFrame::BulkString(Some(b)) => b,
            other => panic!("expected bulk string, got {other:?}"),
        };
        let s = String::from_utf8_lossy(&body);
        assert!(
            s.contains("bad argument #1 to '?'") && s.contains("got string"),
            "expected pcall bad-argument wording, got {s:?}"
        );

        // Direct call: 'load' name with user_script:1: prefix.
        let frame = eval_script(b"return type(load(nil))", &[], &[], &mut store, 0);
        let err = frame.expect_err("direct load(nil) should error");
        assert!(
            err.contains("user_script:1: bad argument #1 to 'load'") && err.contains("got nil"),
            "expected direct-call wording, got {err:?}"
        );

        // load != loadstring (separate function values).
        let frame = eval_script(
            b"return tostring(load == loadstring)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"false".to_vec())));
    }

    #[test]
    fn lua_pcall_preserves_typed_error_values() {
        // Pins frankenredis-cxmsu. Lua 5.1 (vendored Redis 7.2.4) keeps
        // the original LuaValue passed to `error()` when its type is
        // not coercible to a string via lua_concat — i.e. bool, nil,
        // table, function, thread. Number errors with default level=1
        // are concatenated with the where-prefix and become a string;
        // level=0 preserves the number's type.
        let mut store = Store::new();

        // table is preserved with its fields and identity.
        let frame = eval_script(
            b"local t={code=42}; local ok,err=pcall(function() error(t) end); return type(err)..':'..tostring(err.code)..':'..tostring(err==t)",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(
            frame,
            RespFrame::BulkString(Some(b"table:42:true".to_vec()))
        );

        // boolean preserves type.
        let frame = eval_script(
            b"local ok,err=pcall(function() error(true) end); return type(err)..':'..tostring(err)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"boolean:true".to_vec())));

        // nil preserves type.
        let frame = eval_script(
            b"local ok,err=pcall(function() error(nil) end); return type(err)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"nil".to_vec())));

        // Default-level number is coerced to a prefixed string (Lua
        // 5.1's lua_isstring-on-number quirk).
        let frame = eval_script(
            b"local ok,err=pcall(function() error(42) end); return type(err)..':'..tostring(err)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(
            frame,
            RespFrame::BulkString(Some(b"string:user_script:1: 42".to_vec()))
        );

        // level=0 with a number preserves the number type.
        let frame = eval_script(
            b"local ok,err=pcall(function() error(42,0) end); return type(err)..':'..tostring(err)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"number:42".to_vec())));

        // Nested pcall: inner consumes its own typed error; outer's
        // typed error round-trips independently.
        let frame = eval_script(
            b"local ok,err=pcall(function() pcall(function() error({}) end); error({deep=true}) end); return type(err)..':'..tostring(err.deep)",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"table:true".to_vec())));

        // pending_error_value cleared by a successful pcall, so a later
        // pcall around a string error doesn't accidentally surface
        // stale typed-error state.
        let frame = eval_script(
            b"local ok = pcall(function() return 1 end); local ok2, err = pcall(function() error('x') end); return type(err)..':'..tostring(err)",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(
            frame,
            RespFrame::BulkString(Some(b"string:user_script:1: x".to_vec()))
        );

        // xpcall hands the typed value to the message handler.
        let frame = eval_script(
            b"local ok,err=xpcall(function() error({m='hi'}) end, function(e) return e end); return type(err)..':'..tostring(err.m)",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"table:hi".to_vec())));

        // Top-level escape (uncaught typed error) renders to a string
        // for the Redis error reply.
        let err = eval_script(b"error(true)", &[], &[], &mut store, 0)
            .expect_err("error(true) should escape script as an error");
        assert!(
            err.contains("true"),
            "top-level error(true) should render as 'true': {err:?}"
        );
    }

    #[test]
    fn lua_string_indexing_routes_through_string_library() {
        // Pins frankenredis-tbu4k. Lua 5.1 (vendored Redis 7.2.4) attaches
        // the 'string' library as the metatable __index for strings:
        //   `('abc').upper` returns string.upper (a function),
        //   `('abc').fld`   returns nil (no error),
        //   `('abc'):upper()` calls string.upper('abc') → "ABC".
        // The string-library function values share identity with the
        // ones registered in the `string` table: `string.upper == ('x').upper`.
        let mut store = Store::new();

        // s.upper resolves to a function; calling it via : returns "ABC".
        let frame = eval_script(
            b"local s = 'abc'; return s:upper()",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"ABC".to_vec())));

        // s.len via colon returns 3.
        let frame =
            eval_script(b"local s = 'abc'; return s:len()", &[], &[], &mut store, 0).unwrap();
        assert_eq!(frame, RespFrame::Integer(3));

        // Unknown field returns nil instead of erroring.
        let frame = eval_script(
            b"local s = 'abc'; return tostring(s.fld)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"nil".to_vec())));

        // Bracket-indexing with a string key also routes through the
        // string library.
        let frame = eval_script(
            b"local s = 'abc'; return type(s['upper'])",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"function".to_vec())));

        // Numeric indexing returns nil (string library is keyed by string).
        let frame = eval_script(
            b"local s = 'abc'; return tostring(s[1])",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"nil".to_vec())));

        // Identity: string.upper is the same value as ('x').upper.
        let frame = eval_script(
            b"return tostring(string.upper == ('x').upper)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"true".to_vec())));

        // Calling an unknown field still produces the "attempt to call
        // field 'fld' (a nil value)" wording (matches frankenredis-md71j).
        let err = eval_script(b"local s = 'abc'; s.fld()", &[], &[], &mut store, 0)
            .expect_err("expected error for s.fld()");
        assert!(
            err.contains("attempt to call field 'fld' (a nil value)"),
            "wrong wording for s.fld(): {err:?}"
        );

        // Receivers that are still non-indexable types (nil/bool/number)
        // remain errors — only Str routes through the string library.
        for src in [
            b"local x; return x.foo" as &[u8],
            b"return (true).foo",
            b"return (42).foo",
        ] {
            let err = eval_script(src, &[], &[], &mut store, 0).expect_err(&format!(
                "expected index error for {:?}",
                String::from_utf8_lossy(src)
            ));
            assert!(
                err.contains("attempt to index"),
                "wrong index error for {:?}: {err:?}",
                String::from_utf8_lossy(src)
            );
        }
    }

    #[test]
    fn lua_attempt_to_call_errors_carry_accessor_context() {
        // Pins frankenredis-md71j. Lua 5.1 (vendored Redis 7.2.4) emits
        // accessor-aware wording for non-callable call sites:
        //   'local x; x()'                  → "attempt to call local 'x' (a nil value)"
        //   'local x=1; local f=function() x() end; f()' → "attempt to call upvalue 'x' (a nil value)"
        //   'local t={}; t.f()'             → "attempt to call field 'f' (a nil value)"
        //   "local t={}; t['foo']()"        → "attempt to call field 'foo' (a nil value)"
        //   'local t={}; t[1]()'            → "attempt to call field '?' (a nil value)"
        //   'local t={}; t:m()'             → "attempt to call method 'm' (a nil value)"
        //   '(nil)()'                       → "attempt to call a nil value"
        //   "(function() return nil end)()()" → "attempt to call a nil value"
        let mut store = Store::new();
        let cases: &[(&[u8], &str)] = &[
            (b"local x; x()", "attempt to call local 'x' (a nil value)"),
            (
                b"local x = 42; x()",
                "attempt to call local 'x' (a number value)",
            ),
            (
                b"local x; local f = function() x() end; f()",
                "attempt to call upvalue 'x' (a nil value)",
            ),
            (
                b"local t = {}; t.f()",
                "attempt to call field 'f' (a nil value)",
            ),
            (
                b"local t = {f = true}; t.f()",
                "attempt to call field 'f' (a boolean value)",
            ),
            (
                b"local t = {}; t['foo']()",
                "attempt to call field 'foo' (a nil value)",
            ),
            (
                b"local t = {}; t[1]()",
                "attempt to call field '?' (a nil value)",
            ),
            (
                b"local t = {}; t[true]()",
                "attempt to call field '?' (a nil value)",
            ),
            (
                b"local t = {}; t:m()",
                "attempt to call method 'm' (a nil value)",
            ),
            (
                b"local t = {m = 42}; t:m()",
                "attempt to call method 'm' (a number value)",
            ),
            (b"(nil)()", "attempt to call a nil value"),
            (
                b"local function g() return nil end; g()()",
                "attempt to call a nil value",
            ),
        ];
        for (src, expected) in cases {
            let err = eval_script(src, &[], &[], &mut store, 0).expect_err(&format!(
                "expected error for {:?}",
                String::from_utf8_lossy(src)
            ));
            assert!(
                err.contains(expected),
                "wrong wording for {:?}: got {err:?}, expected to contain {expected:?}",
                String::from_utf8_lossy(src)
            );
            // Every runtime call error carries the source-location prefix.
            assert!(
                err.contains("user_script:"),
                "missing user_script: prefix for {:?}: {err:?}",
                String::from_utf8_lossy(src)
            );
        }

        // Callable values still execute normally (regression check).
        let frame = eval_script(
            b"local function f(x) return x + 1 end; return f(41)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::Integer(42));
    }

    #[test]
    fn lua_local_slot_resolution_preserves_lexical_semantics_v0u4b() {
        let mut store = Store::new();
        let frame = eval_script(
            br#"
local x = 'outer'
local outer = function() return x end
local t = {}
local sum = 0
for i = 1, 100 do
    t[i] = i
    sum = sum + t[i]
end
do
    local x = x .. ':inner'
    local function g(n)
        if n <= 1 then return n end
        return g(n - 1) + n
    end
    local loaded = loadstring('local a = 2; local b = a + 3; return b')
    return outer() .. ':' .. x .. ':' .. sum .. ':' .. t[100] .. ':' .. g(4) .. ':' .. loaded()
end
"#,
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(
            frame,
            RespFrame::BulkString(Some(b"outer:outer:inner:5050:100:10:5".to_vec()))
        );
    }

    #[test]
    fn lua_compare_concat_unary_errors_match_upstream_wording() {
        // Pins frankenredis-7w22v. Upstream Lua 5.1 (vendored Redis 7.2.4)
        // emits per-type wording for unary/concat/comparison failures.
        //   '-nil'        → "attempt to perform arithmetic on a nil value"
        //   '-\"abc\"'    → "attempt to perform arithmetic on a string value"
        //   '#nil'        → "attempt to get length of a nil value"
        //   '#42'         → "attempt to get length of a number value"
        //   '\"a\"..nil'  → "attempt to concatenate a nil value"
        //   '\"a\"..{}'   → "attempt to concatenate a table value"
        //   '1<\"a\"'     → "attempt to compare number with string"
        //   'true<false'  → "attempt to compare two boolean values"
        //   'nil<nil'     → "attempt to compare two nil values"
        // All carry the standard "user_script:1: " prefix.
        let mut store = Store::new();
        let cases: &[(&[u8], &str)] = &[
            (
                b"local v = -nil",
                "user_script:1: attempt to perform arithmetic on a nil value",
            ),
            (
                b"local v = -'abc'",
                "user_script:1: attempt to perform arithmetic on a string value",
            ),
            (
                b"local v = #nil",
                "user_script:1: attempt to get length of a nil value",
            ),
            (
                b"local v = #42",
                "user_script:1: attempt to get length of a number value",
            ),
            (
                b"local v = #true",
                "user_script:1: attempt to get length of a boolean value",
            ),
            (
                b"local v = 'a' .. nil",
                "user_script:1: attempt to concatenate a nil value",
            ),
            (
                b"local v = 'a' .. true",
                "user_script:1: attempt to concatenate a boolean value",
            ),
            (
                b"local v = 'a' .. {}",
                "user_script:1: attempt to concatenate a table value",
            ),
            (
                b"local v = nil .. 'a'",
                "user_script:1: attempt to concatenate a nil value",
            ),
            (
                b"local v = 1 < 'a'",
                "user_script:1: attempt to compare number with string",
            ),
            (
                b"local v = 1 < nil",
                "user_script:1: attempt to compare number with nil",
            ),
            (
                b"local v = 'a' < {}",
                "user_script:1: attempt to compare string with table",
            ),
            (
                b"local v = true < false",
                "user_script:1: attempt to compare two boolean values",
            ),
            (
                b"local v = nil < nil",
                "user_script:1: attempt to compare two nil values",
            ),
            (
                b"local v = ({}) < ({})",
                "user_script:1: attempt to compare two table values",
            ),
        ];
        for (src, expected) in cases {
            let err = eval_script(src, &[], &[], &mut store, 0).expect_err(&format!(
                "expected error for {:?}",
                String::from_utf8_lossy(src)
            ));
            assert!(
                err.contains(expected),
                "wrong wording for {:?}: got {err:?}, expected to contain {expected:?}",
                String::from_utf8_lossy(src)
            );
        }

        // Numeric strings still coerce for unary minus, and string..number
        // still concatenates (regression checks for Lua semantics).
        let frame = eval_script(b"return tostring(-'123')", &[], &[], &mut store, 0).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"-123".to_vec())));
        let frame = eval_script(b"return 'a' .. 1", &[], &[], &mut store, 0).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"a1".to_vec())));
        // Length-of-string still works.
        let frame = eval_script(b"return #'abc'", &[], &[], &mut store, 0).unwrap();
        assert_eq!(frame, RespFrame::Integer(3));
    }

    #[test]
    fn lua_to_resp_recognises_resp3_type_hint_tables() {
        // Pins frankenredis-vr8rghint. Upstream src/script_lua.c::
        // luaReplyToRedisReply checks for {map=...}, {set=...},
        // {double=...}, {big_number=...}, {verbatim_string=...} hint
        // tables AFTER ok/err but before the array-iteration fallback.
        // fr was returning empty arrays for ALL of them.
        //
        // (frankenredis-jp7gs) The Map frame is RESP3-only; RESP2
        // clients see it as a flat 2N array. Pin both shapes here so
        // any future refactor that drops the RESP3 hint detection or
        // the RESP2 downconvert is caught.
        let mut store = Store::new();
        store.dispatch_client_ctx.resp_protocol_version = 3;

        // {map = {a=1, b=2}} → Map frame with 2 pairs (RESP3).
        let frame = eval_script(b"return {map = {a=1, b=2}}", &[], &[], &mut store, 0)
            .expect("map hint should not error");
        match frame {
            RespFrame::Map(Some(pairs)) => {
                let mut keys: Vec<String> = pairs
                    .iter()
                    .filter_map(|(k, _)| match k {
                        RespFrame::BulkString(Some(b)) => {
                            Some(String::from_utf8_lossy(b).into_owned())
                        }
                        _ => None,
                    })
                    .collect();
                keys.sort();
                assert_eq!(keys, vec!["a".to_string(), "b".to_string()]);
            }
            other => panic!("expected Map for map hint, got {other:?}"),
        }

        // Same script over a RESP2 client downconverts to a 4-element
        // flat array (frankenredis-jp7gs).
        let mut store2 = Store::new();
        let frame = eval_script(b"return {map = {a=1, b=2}}", &[], &[], &mut store2, 0)
            .expect("map hint resp2 should not error");
        match frame {
            RespFrame::Array(Some(items)) => {
                assert_eq!(
                    items.len(),
                    4,
                    "expected flat 4-element array, got {items:?}"
                );
            }
            other => panic!("expected Array for resp2 map hint, got {other:?}"),
        }

        // {set = {1, 2, 3}} → Array of the inner array's values.
        let frame = eval_script(b"return {set = {1, 2, 3}}", &[], &[], &mut store, 0)
            .expect("set hint should not error");
        assert_eq!(
            frame,
            RespFrame::Array(Some(vec![
                RespFrame::Integer(1),
                RespFrame::Integer(2),
                RespFrame::Integer(3),
            ]))
        );

        // {double = 3.14} → RESP3 Double frame (frankenredis-aae3d); the
        // RESP2 client downconverts it to the equivalent bulk string.
        let frame = eval_script(b"return {double = 3.14}", &[], &[], &mut store, 0)
            .expect("double hint should not error");
        assert_eq!(frame, RespFrame::Double("3.14".to_string()));
        let mut store_dbl_resp2 = Store::new();
        let frame = eval_script(b"return {double = 3.14}", &[], &[], &mut store_dbl_resp2, 0)
            .expect("double hint resp2 should not error");
        assert_eq!(frame, RespFrame::BulkString(Some(b"3.14".to_vec())));
        // Large magnitudes use d2string scientific form, not Rust Display.
        let frame = eval_script(b"return {double = 1e20}", &[], &[], &mut store, 0)
            .expect("double hint 1e20 should not error");
        assert_eq!(frame, RespFrame::Double("1e+20".to_string()));

        // {big_number = "12345..."} → RESP3 Big Number frame; RESP2 client
        // downconverts to the equivalent bulk string. (frankenredis-h2uga)
        let frame = eval_script(
            b"return {big_number = '1234567890123456789012345'}",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("big_number hint should not error");
        assert_eq!(
            frame,
            RespFrame::BigNumber("1234567890123456789012345".to_string())
        );
        let mut store_bn_resp2 = Store::new();
        let frame = eval_script(
            b"return {big_number = '1234567890123456789012345'}",
            &[],
            &[],
            &mut store_bn_resp2,
            0,
        )
        .expect("big_number hint resp2 should not error");
        assert_eq!(
            frame,
            RespFrame::BulkString(Some(b"1234567890123456789012345".to_vec()))
        );
        let frame = eval_script(
            b"return {big_number = '12\\r34\\n56'}",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("big_number hint with CR/LF should not error");
        assert_eq!(frame, RespFrame::BigNumber("12 34 56".to_string()));
        let mut store_bn_crlf_resp2 = Store::new();
        let frame = eval_script(
            b"return {big_number = '12\\r34\\n56'}",
            &[],
            &[],
            &mut store_bn_crlf_resp2,
            0,
        )
        .expect("big_number hint with CR/LF resp2 should not error");
        assert_eq!(frame, RespFrame::BulkString(Some(b"12 34 56".to_vec())));

        // {verbatim_string = {format='txt', string='hi'}} → BulkString of `string`.
        let frame = eval_script(
            b"return {verbatim_string = {format='txt', string='hi'}}",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("verbatim_string hint should not error");
        assert_eq!(frame, RespFrame::BulkString(Some(b"hi".to_vec())));

        // err and ok still take precedence over the type hints.
        let frame = eval_script(
            b"return {err = 'oops', map = {a = 1}}",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("err+map should not error before reply");
        assert_eq!(frame, RespFrame::Error("oops".to_string()));
        let frame = eval_script(
            b"return {ok = 'good', map = {a = 1}}",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::SimpleString("good".to_string()));

        // Bare string-keyed tables without any hint key still produce
        // an empty array (no behavior change for the non-hint path).
        let frame = eval_script(b"return {a = 1, b = 2}", &[], &[], &mut store, 0).unwrap();
        assert_eq!(frame, RespFrame::Array(Some(Vec::new())));
    }

    #[test]
    fn tonumber_accepts_hex_strings_via_lua_5_1_str2d_fallback() {
        // Pins frankenredis-luatonumhex. Lua 5.1's lobject.c::luaO_str2d
        // tries strtod first and, when that stops at 'x'/'X' or fails,
        // re-parses the input as `strtoul(s, ..., 16)`. So
        // `tonumber("0xFF")` → 255, `tonumber("-0xff")` → -255, and
        // whitespace gets stripped just like for a decimal string.
        // fr was returning nil for any 0x-prefixed string regardless of
        // case or sign.
        let mut store = Store::new();

        let cases: &[(&[u8], LuaValue)] = &[
            (b"return tonumber('0xFF')", LuaValue::Number(255.0)),
            (b"return tonumber('0xff')", LuaValue::Number(255.0)),
            (b"return tonumber('0X1A')", LuaValue::Number(26.0)),
            (b"return tonumber('-0xff')", LuaValue::Number(-255.0)),
            (b"return tonumber('+0x10')", LuaValue::Number(16.0)),
            (b"return tonumber(' 0xFF ')", LuaValue::Number(255.0)),
            (b"return tonumber('0x0')", LuaValue::Number(0.0)),
            (b"return tonumber('0xDEAD')", LuaValue::Number(57005.0)),
        ];
        for (src, expected) in cases {
            let frame = eval_script(src, &[], &[], &mut store, 0)
                .unwrap_or_else(|e| panic!("eval {:?} failed: {e}", String::from_utf8_lossy(src)));
            let LuaValue::Number(want) = expected else {
                unreachable!()
            };
            match frame {
                RespFrame::Integer(got) => assert_eq!(
                    got as f64,
                    *want,
                    "src = {:?}",
                    String::from_utf8_lossy(src)
                ),
                RespFrame::BulkString(Some(bytes)) => {
                    let s = String::from_utf8_lossy(&bytes);
                    let got: f64 = s.parse().expect("numeric reply");
                    assert_eq!(got, *want, "src = {:?}", String::from_utf8_lossy(src));
                }
                other => panic!(
                    "expected number reply for {:?}, got {other:?}",
                    String::from_utf8_lossy(src)
                ),
            }
        }

        // Malformed hex still returns nil, matching upstream.
        // Note: `0xFF.5` is NOT malformed — it's a valid C99 hex float
        // (FF.5 in hex = 255 + 5/16 = 255.3125), so upstream's strtod
        // accepts it. (frankenredis-83zqp updated this pin.)
        for src in [
            b"return tonumber('0x') == nil".as_slice(),
            b"return tonumber('0xZ') == nil".as_slice(),
            b"return tonumber('xyz') == nil".as_slice(),
        ] {
            let frame = eval_script(src, &[], &[], &mut store, 0)
                .unwrap_or_else(|e| panic!("eval {:?} failed: {e}", String::from_utf8_lossy(src)));
            assert_eq!(
                frame,
                RespFrame::Integer(1),
                "src = {:?}",
                String::from_utf8_lossy(src)
            );
        }

        // Decimal / scientific tonumber paths remain unaffected (use
        // integer-valued cases so Lua-to-Resp returns Integer cleanly).
        for (src, expected) in [
            (b"return tonumber('42')".as_slice(), 42),
            (b"return tonumber('1.5e2')".as_slice(), 150),
            (b"return tonumber('-7')".as_slice(), -7),
        ] {
            let frame = eval_script(src, &[], &[], &mut store, 0).unwrap();
            assert_eq!(
                frame,
                RespFrame::Integer(expected),
                "src = {:?}",
                String::from_utf8_lossy(src)
            );
        }
    }

    #[test]
    fn lexer_parses_hex_literals_like_lua_5_1() {
        // Pins frankenredis-luahexlit. Lua 5.1 (the dialect Redis
        // embeds) accepts hex integer literals via `0x` / `0X`. fr's
        // lexer had a hex block guarded by `self.pos - start >= 2`,
        // which never fired because only the leading `0` had been
        // consumed at that point — so `0xFF` parsed as Number(0)
        // followed by Name("xFF") and `bit.band(0xFF, 0)` errored
        // with "expected RParen, got Name(\"xFF\")" instead of
        // returning 0.
        let mut store = Store::new();
        let cases: &[(&[u8], i64)] = &[
            (b"return 0xFF", 255),
            (b"return 0xff", 255),
            (b"return 0X1A", 26),
            (b"return 0x0", 0),
            (b"return 0xDEAD", 57005),
            (b"return 0xFF + 0x0F", 270),
            (b"return 0x10 * 0x10", 256),
            (b"return 0x100 - 1", 255),
            // Verify the lex split bug is gone: `bit.band(0xFF, 0x0F)`
            // previously errored "expected RParen, got Name('xFF')"
            // before we ever reached the bit-library lookup. We can't
            // assert on bit.band's result here (it isn't part of fr's
            // sandbox), but we can confirm the parser at least gets
            // past the call-args boundary via a synthetic 2-arg call.
            (
                b"local function f(a,b) return a + b end return f(0xFF, 0x0F)",
                270,
            ),
        ];
        for (src, expected) in cases {
            let frame = eval_script(src, &[], &[], &mut store, 0)
                .unwrap_or_else(|e| panic!("eval {:?} failed: {e}", String::from_utf8_lossy(src)));
            match frame {
                RespFrame::Integer(got) => {
                    assert_eq!(got, *expected, "src = {:?}", String::from_utf8_lossy(src))
                }
                other => panic!(
                    "expected Integer({expected}) for {:?}, got {other:?}",
                    String::from_utf8_lossy(src)
                ),
            }
        }

        // Negative hex via unary minus must still parse cleanly.
        let frame = eval_script(b"return -0xff", &[], &[], &mut store, 0).unwrap();
        assert_eq!(frame, RespFrame::Integer(-255));

        // Malformed `0x` (no hex digits) errors instead of silently
        // splitting like before.
        let err = eval_script(b"return 0x", &[], &[], &mut store, 0)
            .expect_err("0x with no hex digits should error");
        assert!(
            err.contains("malformed number"),
            "expected malformed-number error, got {err}"
        );
    }

    #[test]
    fn parser_rejects_non_name_statement_starts_with_unexpected_symbol_cdfpx() {
        // (frankenredis-cdfpx) Upstream lparser.c restricts statement
        // starts to keywords, Name, or '(' for a parenthesized
        // prefixexp. Numbers / strings / true / false / nil tokens
        // cannot start a statement; vendored rejects them at
        // statement-start with "unexpected symbol near '<X>'". fr
        // previously routed them into parse_suffixed_expr and only
        // failed later with the assignment wording "'=' expected
        // near '<eof>'", which leaked through loadstring('123') and
        // similar shapes.
        let cases: &[(&str, &str)] = &[
            ("123", "unexpected symbol near '123'"),
            ("1.5", "unexpected symbol near '1.5'"),
            ("true", "unexpected symbol near 'true'"),
            ("false", "unexpected symbol near 'false'"),
            ("nil", "unexpected symbol near 'nil'"),
        ];
        for (src, expected) in cases {
            let err =
                compile_check(src.as_bytes()).expect_err(&format!("expected error for {src:?}"));
            assert_eq!(err, *expected, "wrong wording for {src:?}: {err}");
        }
        // Bare Names continue to use the '=' expected wording (covered
        // by parser_rejects_bare_identifier_statements_with_upstream_wording).
        let err = compile_check(b"foo").expect_err("foo should error");
        assert_eq!(err, "'=' expected near '<eof>'");
        // Function calls and parenthesized prefixexps remain valid.
        for src in ["f()", "(function() end)()", "f(1)", "obj:m()"] {
            compile_check(src.as_bytes()).unwrap_or_else(|e| panic!("{src:?} should compile: {e}"));
        }
    }

    #[test]
    fn compiled_chunk_cache_reuses_ast_but_rebinds_call_state_45ywg() {
        let script = b"local arg = ARGV[1]; return arg";
        let first = super::compile_lua_chunk_cached(script).expect("compile first");
        let second = super::compile_lua_chunk_cached(script).expect("compile second");
        assert!(
            std::rc::Rc::ptr_eq(&first, &second),
            "expected repeated compile to reuse cached AST"
        );

        let mut store = Store::new();
        let one = eval_script(script, &[], &[b"one".to_vec()], &mut store, 0).unwrap();
        let two = eval_script(script, &[], &[b"two".to_vec()], &mut store, 0).unwrap();
        assert_eq!(one, RespFrame::BulkString(Some(b"one".to_vec())));
        assert_eq!(two, RespFrame::BulkString(Some(b"two".to_vec())));
    }

    #[test]
    fn parser_rejects_bare_identifier_statements_with_upstream_wording() {
        // Pins frankenredis-luabarestmt. Lua's grammar allows expression
        // statements only for function calls; bare `var` (Name / Field /
        // Index) is not a valid statement. Upstream Lua surfaces this as
        // "'=' expected near '<token>'" because after parsing the var
        // it's expecting the start of an assignment. fr's parser was
        // wrapping any suffixed expression in Stmt::Expression, so
        // SCRIPT LOAD / EVAL of `'invalid lua'`, `'foo bar'`, `'foo'`
        // were silently accepted (LOAD returned a SHA, EVAL returned
        // nil) instead of erroring like vendored 7.2.4.
        for (src, expected_near) in [
            ("invalid lua", "'=' expected near 'lua'"),
            ("foo bar", "'=' expected near 'bar'"),
            ("a b c d", "'=' expected near 'b'"),
            ("foo", "'=' expected near '<eof>'"),
            ("redis", "'=' expected near '<eof>'"),
            ("a.b", "'=' expected near '<eof>'"),
            ("t[1]", "'=' expected near '<eof>'"),
        ] {
            let err =
                compile_check(src.as_bytes()).expect_err(&format!("expected error for {src:?}"));
            assert_eq!(err, expected_near, "wrong wording for {src:?}: {err}");
        }

        // Regression: legitimate function-call statements still parse.
        for src in [
            "redis.call('PING')",
            "print('hello')",
            "f(1, 2, 3)",
            "obj:method(1)",
            "f 'string-arg'",
            "f { a = 1 }",
            "return 1",
            "local x = 1",
            "a = 1",
            "a, b = 1, 2",
            "if true then end",
            "for i = 1, 5 do end",
            "do return 1 end",
        ] {
            compile_check(src.as_bytes()).unwrap_or_else(|e| panic!("{src:?} should compile: {e}"));
        }
    }

    #[test]
    fn function_decl_errors_on_missing_table_path() {
        // (frankenredis-dfly7) When `function a.b.c() end` references an
        // unbound global `a`, vendored's sandbox emits "Script attempted
        // to access nonexistent global variable 'a'". fr previously
        // pinned the bypass wording "attempt to index a nil value"; the
        // sandbox-aware fix routes through the same wording as
        // Expr::Name evaluation under globals_locked.
        let mut store = Store::new();
        let err = eval_script(b"function a.b.c() return 1 end", &[], &[], &mut store, 0)
            .expect_err("expected error");
        assert!(
            err.contains(
                "user_script:1: Script attempted to access nonexistent global variable 'a'"
            ),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn set_nested_field_ignores_degenerate_name_paths_without_panicking() {
        let mut store = Store::new();
        let mut state = LuaState::new(&mut store, 0);

        state
            .set_nested_field(&[], LuaValue::Number(1.0))
            .expect("empty path should be ignored");
        state
            .set_nested_field(&["root".to_string()], LuaValue::Number(1.0))
            .expect("single-name path should be ignored");
    }

    #[test]
    fn set_nested_field_rebuilds_parent_chain_without_unwrap_shortcuts() {
        let mut store = Store::new();
        let mut state = LuaState::new(&mut store, 0);
        let parent = LuaTable::new();
        let child = LuaTable::new();
        parent.set(
            LuaValue::Str(b"child".to_vec()),
            LuaValue::Table(child.clone()),
        );
        state
            .globals
            .insert("root".to_string(), LuaValue::Table(parent.clone()));

        state
            .set_nested_field(
                &["root".to_string(), "child".to_string(), "leaf".to_string()],
                LuaValue::Number(7.0),
            )
            .expect("nested assignment should succeed");

        let root = state.globals.get("root").expect("root table");
        let LuaValue::Table(root_table) = root else {
            panic!("root should remain a table");
        };
        let LuaValue::Table(updated_child) = root_table.get(&LuaValue::Str(b"child".to_vec()))
        else {
            panic!("child should remain a table");
        };
        let leaf = updated_child.get(&LuaValue::Str(b"leaf".to_vec()));
        assert!(
            matches!(leaf, LuaValue::Number(n) if (n - 7.0).abs() < f64::EPSILON),
            "leaf should be 7.0, got: {:?}",
            leaf
        );
    }

    #[test]
    fn redis_breakpoint_returns_false_without_debugger() {
        let mut store = Store::new();
        let result = eval_script(
            b"if redis.breakpoint() then return 1 else return 0 end",
            &[],
            &[],
            &mut store,
            0,
        );

        assert_eq!(result, Ok(RespFrame::Integer(0)));
    }

    #[test]
    fn redis_debug_is_a_noop_without_debugger() {
        let mut store = Store::new();
        let result = eval_script(
            b"redis.debug('hello', 42, true) return 1",
            &[],
            &[],
            &mut store,
            0,
        );

        assert_eq!(result, Ok(RespFrame::Integer(1)));
    }

    #[test]
    fn redis_call_config_get_returns_keyed_lua_table() {
        let mut store = Store::new();
        let result = eval_script(
            b"local cfg = redis.call('CONFIG', 'GET', 'maxmemory-policy') return cjson.encode(cfg)",
            &[],
            &[],
            &mut store,
            0,
        );

        assert_eq!(
            result,
            Ok(RespFrame::BulkString(Some(
                b"{\"maxmemory-policy\":\"noeviction\"}".to_vec()
            )))
        );
    }

    #[test]
    fn transaction_control_commands_reject_from_scripts_after_arity_validation() {
        let mut store = Store::new();

        let wrong_multi = eval_script(
            b"return redis.call('MULTI', 'extra')",
            &[],
            &[],
            &mut store,
            0,
        );
        assert_eq!(
            wrong_multi,
            Err("ERR Wrong number of args calling Redis command from script".to_string())
        );

        let multi = eval_script(b"return redis.call('MULTI')", &[], &[], &mut store, 0);
        assert_eq!(multi, Err(SCRIPT_NOSCRIPT_ERROR.to_string()));

        let wrong_exec = eval_script(
            b"return redis.call('EXEC', 'extra')",
            &[],
            &[],
            &mut store,
            0,
        );
        assert_eq!(
            wrong_exec,
            Err("ERR Wrong number of args calling Redis command from script".to_string())
        );

        let exec = eval_script(b"return redis.call('EXEC')", &[], &[], &mut store, 0);
        assert_eq!(exec, Err(SCRIPT_NOSCRIPT_ERROR.to_string()));

        let wrong_discard = eval_script(
            b"return redis.call('DISCARD', 'extra')",
            &[],
            &[],
            &mut store,
            0,
        );
        assert_eq!(
            wrong_discard,
            Err("ERR Wrong number of args calling Redis command from script".to_string())
        );

        let discard = eval_script(b"return redis.call('DISCARD')", &[], &[], &mut store, 0);
        assert_eq!(discard, Err(SCRIPT_NOSCRIPT_ERROR.to_string()));

        let wrong_watch = eval_script(b"return redis.call('WATCH')", &[], &[], &mut store, 0);
        assert_eq!(
            wrong_watch,
            Err("ERR Wrong number of args calling Redis command from script".to_string())
        );

        let watch = eval_script(
            b"return redis.call('WATCH', 'k1', 'k2')",
            &[],
            &[],
            &mut store,
            0,
        );
        assert_eq!(watch, Err(SCRIPT_NOSCRIPT_ERROR.to_string()));

        let wrong_unwatch = eval_script(
            b"return redis.call('UNWATCH', 'extra')",
            &[],
            &[],
            &mut store,
            0,
        );
        assert_eq!(
            wrong_unwatch,
            Err("ERR Wrong number of args calling Redis command from script".to_string())
        );

        let unwatch = eval_script(b"return redis.call('UNWATCH')", &[], &[], &mut store, 0);
        assert_eq!(unwatch, Err(SCRIPT_NOSCRIPT_ERROR.to_string()));

        let pcall = eval_script(
            b"local reply = redis.pcall('MULTI'); return reply.err",
            &[],
            &[],
            &mut store,
            0,
        );
        assert_eq!(
            pcall,
            Ok(RespFrame::BulkString(Some(
                SCRIPT_NOSCRIPT_ERROR.as_bytes().to_vec()
            )))
        );
    }

    #[test]
    fn acl_admin_subcommands_reject_from_scripts_after_validation() {
        let mut store = Store::new();

        let acl_arity = eval_script(b"return redis.call('ACL')", &[], &[], &mut store, 0);
        assert_eq!(
            acl_arity,
            Err("ERR Wrong number of args calling Redis command from script".to_string())
        );

        let whoami_arity = eval_script(
            b"return redis.call('ACL', 'WHOAMI', 'extra')",
            &[],
            &[],
            &mut store,
            0,
        );
        assert_eq!(
            whoami_arity,
            Err("ERR Wrong number of args calling Redis command from script".to_string())
        );

        let genpass_bits = eval_script(
            b"return redis.call('ACL', 'GENPASS', '0')",
            &[],
            &[],
            &mut store,
            0,
        );
        assert_eq!(
            genpass_bits,
            Err("ERR ACL GENPASS argument must be the number of bits for the output password, a positive number up to 4096".to_string())
        );

        let log_count = eval_script(
            b"return redis.call('ACL', 'LOG', 'foo')",
            &[],
            &[],
            &mut store,
            0,
        );
        assert_eq!(
            log_count,
            Err("ERR value is not an integer or out of range".to_string())
        );

        let help = eval_script(
            b"local reply = redis.call('ACL', 'HELP'); return reply[1]",
            &[],
            &[],
            &mut store,
            0,
        );
        // (frankenredis-sebba) Each ACL HELP entry is a SimpleString
        // (upstream addReplyStatus). The Lua resp-to-value conversion
        // wraps SimpleString as `{ok = "..."}`, and the script's
        // `return reply[1]` re-emits that table as a SimpleString
        // ("+...\r\n") on the wire. The previous BulkString expectation
        // matched fr's pre-sebba acl_help_frame, which used BulkString
        // instead of upstream's SimpleString.
        assert_eq!(
            help,
            Ok(RespFrame::SimpleString(
                "ACL <subcommand> [<arg> [value] [opt] ...]. Subcommands are:".to_string()
            ))
        );

        let help_arity = eval_script(
            b"return redis.call('ACL', 'HELP', 'extra')",
            &[],
            &[],
            &mut store,
            0,
        );
        assert_eq!(
            help_arity,
            Err("ERR Wrong number of args calling Redis command from script".to_string())
        );

        // (frankenredis-sebba) Length must equal upstream's 29
        // (1 header + 26 from upstream acl.c::ACL_CMD_HELP + 2 from
        // networking.c::addReplyHelp footer). Previously fr-command's
        // acl_help_frame returned 28 entries (the GENPASS two-line
        // description had been collapsed to one line). The wire-side
        // ACL HELP in fr-runtime already had 29 entries, so the two
        // dispatch paths had drifted.
        let help_len = eval_script(
            b"return #redis.call('ACL', 'HELP')",
            &[],
            &[],
            &mut store,
            0,
        );
        assert_eq!(help_len, Ok(RespFrame::Integer(29)));
        // Pin the GENPASS two-line description (the entry that was
        // previously collapsed).
        let genpass_line2 = eval_script(
            b"local r = redis.call('ACL', 'HELP'); for i,e in ipairs(r) do if e.ok and e.ok:find('be used to specify a different size', 1, true) then return i end end; return -1",
            &[],
            &[],
            &mut store,
            0,
        );
        // The line "    be used to specify a different size." is the
        // second part of GENPASS's description; in upstream it sits at
        // position 13 (after GENPASS at 11 and "Generate a secure
        // 256-bit user password..." at 12).
        assert_eq!(genpass_line2, Ok(RespFrame::Integer(13)));

        for script in [
            b"return redis.call('ACL', 'WHOAMI')".as_slice(),
            b"return redis.call('ACL', 'LIST')".as_slice(),
            b"return redis.call('ACL', 'USERS')".as_slice(),
            b"return redis.call('ACL', 'SETUSER', 'alice')".as_slice(),
            b"return redis.call('ACL', 'DELUSER', 'alice')".as_slice(),
            b"return redis.call('ACL', 'GETUSER', 'alice')".as_slice(),
            b"return redis.call('ACL', 'CAT')".as_slice(),
            b"return redis.call('ACL', 'CAT', 'read')".as_slice(),
            b"return redis.call('ACL', 'GENPASS')".as_slice(),
            b"return redis.call('ACL', 'LOG')".as_slice(),
            b"return redis.call('ACL', 'LOG', 'RESET')".as_slice(),
            b"return redis.call('ACL', 'SAVE')".as_slice(),
            b"return redis.call('ACL', 'LOAD')".as_slice(),
            b"return redis.call('ACL', 'DRYRUN', 'alice', 'GET', 'k')".as_slice(),
        ] {
            let err = eval_script(script, &[], &[], &mut store, 0);
            assert_eq!(err, Err(SCRIPT_NOSCRIPT_ERROR.to_string()));
        }

        let pcall = eval_script(
            b"local reply = redis.pcall('ACL', 'WHOAMI'); return reply.err",
            &[],
            &[],
            &mut store,
            0,
        );
        assert_eq!(
            pcall,
            Ok(RespFrame::BulkString(Some(
                SCRIPT_NOSCRIPT_ERROR.as_bytes().to_vec()
            )))
        );
    }

    #[test]
    fn auth_and_hello_reject_from_scripts_after_validation() {
        let mut store = Store::new();

        let auth_arity = eval_script(b"return redis.call('AUTH')", &[], &[], &mut store, 0);
        assert_eq!(
            auth_arity,
            Err("ERR Wrong number of args calling Redis command from script".to_string())
        );

        for script in [
            b"return redis.call('AUTH', 'secret')".as_slice(),
            b"return redis.call('AUTH', 'alice', 'secret')".as_slice(),
            b"return redis.call('HELLO')".as_slice(),
            b"return redis.call('HELLO', '2')".as_slice(),
            b"return redis.call('HELLO', '3', 'AUTH', 'alice', 'secret')".as_slice(),
            b"return redis.call('HELLO', '3', 'SETNAME', 'client1')".as_slice(),
            b"return redis.call('HELLO', '3', 'AUTH', 'alice', 'secret', 'SETNAME', 'client1')"
                .as_slice(),
        ] {
            let err = eval_script(script, &[], &[], &mut store, 0);
            assert_eq!(err, Err(SCRIPT_NOSCRIPT_ERROR.to_string()));
        }

        let hello_integer = eval_script(
            b"return redis.call('HELLO', 'wat')",
            &[],
            &[],
            &mut store,
            0,
        );
        assert_eq!(
            hello_integer,
            Err("ERR value is not an integer or out of range".to_string())
        );

        let hello_proto = eval_script(b"return redis.call('HELLO', '4')", &[], &[], &mut store, 0);
        assert_eq!(
            hello_proto,
            Err("NOPROTO unsupported protocol version '4'".to_string())
        );

        let hello_auth_syntax = eval_script(
            b"return redis.call('HELLO', '3', 'AUTH', 'alice')",
            &[],
            &[],
            &mut store,
            0,
        );
        assert_eq!(hello_auth_syntax, Err("ERR syntax error".to_string()));

        let hello_setname_syntax = eval_script(
            b"return redis.call('HELLO', '3', 'SETNAME')",
            &[],
            &[],
            &mut store,
            0,
        );
        assert_eq!(hello_setname_syntax, Err("ERR syntax error".to_string()));

        let hello_setname_invalid = eval_script(
            b"return redis.call('HELLO', '3', 'SETNAME', 'bad\\nname')",
            &[],
            &[],
            &mut store,
            0,
        );
        assert_eq!(
            hello_setname_invalid,
            Err(
                "ERR Client names cannot contain spaces, newlines or special characters."
                    .to_string()
            )
        );

        let hello_unknown_option = eval_script(
            b"return redis.call('HELLO', '3', 'BOGUS')",
            &[],
            &[],
            &mut store,
            0,
        );
        assert_eq!(hello_unknown_option, Err("ERR syntax error".to_string()));

        let pcall = eval_script(
            b"local reply = redis.pcall('HELLO'); return reply.err",
            &[],
            &[],
            &mut store,
            0,
        );
        assert_eq!(
            pcall,
            Ok(RespFrame::BulkString(Some(
                SCRIPT_NOSCRIPT_ERROR.as_bytes().to_vec()
            )))
        );
    }

    #[test]
    fn sync_rejects_from_scripts_after_arity_validation() {
        let mut store = Store::new();

        let arity = eval_script(
            b"return redis.call('SYNC', 'extra')",
            &[],
            &[],
            &mut store,
            0,
        );
        assert_eq!(
            arity,
            Err("ERR Wrong number of args calling Redis command from script".to_string())
        );

        let sync = eval_script(b"return redis.call('SYNC')", &[], &[], &mut store, 0);
        assert_eq!(sync, Err(SCRIPT_NOSCRIPT_ERROR.to_string()));

        let pcall = eval_script(
            b"local reply = redis.pcall('SYNC'); return reply.err",
            &[],
            &[],
            &mut store,
            0,
        );
        assert_eq!(
            pcall,
            Ok(RespFrame::BulkString(Some(
                SCRIPT_NOSCRIPT_ERROR.as_bytes().to_vec()
            )))
        );
    }

    #[test]
    fn cjson_encode_escapes_object_keys() {
        let table = LuaTable::new();
        table.set(
            LuaValue::Str(b"say\"hi\\there\n".to_vec()),
            LuaValue::Number(1.0),
        );

        assert_eq!(
            lua_value_to_json(&LuaValue::Table(table)).expect("encode"),
            "{\"say\\\"hi\\\\there\\n\":1}"
        );
    }

    #[test]
    fn lua_number_equality_is_exact() {
        assert!(lua_raw_equal(
            &LuaValue::Number(1.0),
            &LuaValue::Number(1.0)
        ));
        assert!(!lua_raw_equal(
            &LuaValue::Number(1.0),
            &LuaValue::Number(1.0 + f64::EPSILON)
        ));
        assert!(!lua_raw_equal(
            &LuaValue::Number(f64::NAN),
            &LuaValue::Number(f64::NAN)
        ));
    }

    #[test]
    fn lua_table_numeric_keys_do_not_use_epsilon_matching() {
        let table = LuaTable::new();
        table.set(LuaValue::Number(1.0), LuaValue::Str(b"exact".to_vec()));

        let exact = table.get(&LuaValue::Number(1.0));
        assert!(matches!(exact, LuaValue::Str(ref bytes) if bytes == b"exact"));

        let near = table.get(&LuaValue::Number(1.0 + f64::EPSILON));
        assert!(matches!(near, LuaValue::Nil));
    }

    #[test]
    fn tonumber_rejects_out_of_range_base_without_panicking() {
        let mut store = Store::new();
        let result = eval_script(b"return tonumber('10', 1)", &[], &[], &mut store, 0);

        assert!(matches!(result, Err(ref err) if err.contains("base out of range")));
    }

    #[test]
    fn tonumber_truncates_float_base_5qv1n() {
        // (frankenredis-5qv1n) Upstream luaL_checkint truncates the
        // base via (int)d, so tonumber('10', 2.5) is equivalent to
        // tonumber('10', 2) and returns 2 (binary "10" -> 2). Pre-fix
        // fr rejected with "base out of range" for any non-integer
        // base.
        let mut store = Store::new();
        let result = eval_script(b"return tonumber('10', 2.5)", &[], &[], &mut store, 0)
            .expect("base truncates to int");
        assert_eq!(result, RespFrame::Integer(2));
    }

    #[test]
    fn tonumber_rejects_non_numeric_base() {
        let mut store = Store::new();
        let result = eval_script(b"return tonumber('10', 'x')", &[], &[], &mut store, 0);

        assert!(matches!(result, Err(ref err) if err.contains("base out of range")));
    }

    #[test]
    fn tonumber_accepts_valid_explicit_base() {
        let mut store = Store::new();
        let result = eval_script(b"return tonumber('10', 16)", &[], &[], &mut store, 0);

        assert!(matches!(result, Ok(RespFrame::Integer(16))));
    }

    #[test]
    fn nested_table_field_assignment_writes_back_through_parent_chain() {
        let mut store = Store::new();
        let result = eval_script(
            b"local t = { a = { b = 1 } }\nt.a.b = 42\nreturn t.a.b",
            &[],
            &[],
            &mut store,
            0,
        );

        assert!(matches!(result, Ok(RespFrame::Integer(42))));
    }

    #[test]
    fn nested_table_index_assignment_writes_back_through_parent_chain() {
        let mut store = Store::new();
        let result = eval_script(
            b"local t = { { 1, 2 } }\nt[1][2] = 99\nreturn t[1][2]",
            &[],
            &[],
            &mut store,
            0,
        );

        assert!(matches!(result, Ok(RespFrame::Integer(99))));
    }

    #[test]
    fn select_negative_index_counts_from_tail() {
        let mut store = Store::new();
        let result = eval_script(b"return select(-1, 'a', 'b', 'c')", &[], &[], &mut store, 0);

        assert!(matches!(result, Ok(RespFrame::BulkString(Some(ref bytes))) if bytes == b"c"));
    }

    #[test]
    fn empty_while_loop_hits_iteration_limit() {
        let mut store = Store::new();
        let result = eval_script(b"while true do end", &[], &[], &mut store, 0);
        match result {
            Ok(RespFrame::Error(msg)) => {
                assert!(msg.contains("iteration count"), "Unexpected error: {}", msg)
            }
            Err(e) => assert!(
                e.contains("iteration count"),
                "Unexpected string error: {}",
                e
            ),
            other => unreachable!("Expected iteration limit error, got {other:?}"),
        }
    }

    #[test]
    fn select_zero_index_errors() {
        let mut store = Store::new();
        let result = eval_script(b"return select(0, 'a', 'b', 'c')", &[], &[], &mut store, 0);

        assert!(matches!(result, Err(ref err) if err.contains("index out of range")));
    }

    #[test]
    fn next_rejects_non_table_argument() {
        let mut store = Store::new();
        let result = eval_script(b"return next(42)", &[], &[], &mut store, 0);

        assert!(matches!(result, Err(ref err) if err.contains("bad argument #1 to 'next'")));
    }

    #[test]
    fn next_rejects_invalid_key() {
        let mut store = Store::new();
        let result = eval_script(
            b"local t = {a = 1}\nreturn next(t, 'missing')",
            &[],
            &[],
            &mut store,
            0,
        );

        assert!(matches!(result, Err(ref err) if err.contains("invalid key to 'next'")));
    }

    #[test]
    fn rawget_rejects_non_table_argument() {
        let mut store = Store::new();
        let result = eval_script(b"return rawget(42, 'x')", &[], &[], &mut store, 0);

        assert!(matches!(result, Err(ref err) if err.contains("bad argument #1 to 'rawget'")));
    }

    #[test]
    fn rawset_rejects_non_table_argument() {
        let mut store = Store::new();
        let result = eval_script(b"return rawset(42, 'x', 1)", &[], &[], &mut store, 0);

        assert!(matches!(result, Err(ref err) if err.contains("bad argument #1 to 'rawset'")));
    }

    // (frankenredis-sxqtm) Lua 5.1 supports function values as table
    // keys via object identity. fr previously silently dropped the
    // entry (lua_raw_equal had no Function arm so two cloned function
    // references never matched on lookup). Pin the rawset/rawget +
    // pairs roundtrip, plus the `==` identity invariant.
    #[test]
    fn function_value_works_as_table_key_sxqtm() {
        let mut store = Store::new();
        let roundtrip = eval_script(
            b"local t={}; local k=function() end; rawset(t, k, 'v'); return rawget(t, k)",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("rawset/rawget roundtrip");
        assert_eq!(roundtrip, RespFrame::BulkString(Some(b"v".to_vec())));

        let pairs_count = eval_script(
            b"local t={}; local k=function() end; rawset(t, k, 'v'); local n=0; for kk,vv in pairs(t) do n=n+1 end; return n",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("pairs walk");
        assert_eq!(pairs_count, RespFrame::Integer(1));

        // `==` identity: two distinct function definitions are NOT equal,
        // but a function references itself == itself.
        let same = eval_script(
            b"local k=function() end; return tostring(k == k)",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("self-eq");
        assert_eq!(same, RespFrame::BulkString(Some(b"true".to_vec())));
        let diff = eval_script(
            b"local a=function() end; local b=function() end; return tostring(a == b)",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("distinct-funcs");
        assert_eq!(diff, RespFrame::BulkString(Some(b"false".to_vec())));
    }

    #[test]
    fn pairs_rejects_non_table_argument() {
        let mut store = Store::new();
        let result = eval_script(b"return pairs(42)", &[], &[], &mut store, 0);

        assert!(matches!(result, Err(ref err) if err.contains("bad argument #1 to 'pairs'")));
    }

    #[test]
    fn ipairs_rejects_non_table_argument() {
        let mut store = Store::new();
        let result = eval_script(b"return ipairs(42)", &[], &[], &mut store, 0);

        assert!(matches!(result, Err(ref err) if err.contains("bad argument #1 to 'ipairs'")));
    }

    #[test]
    fn table_insert_rejects_non_table_argument() {
        let mut store = Store::new();
        let result = eval_script(b"table.insert(42, 'x')", &[], &[], &mut store, 0);

        assert!(matches!(result, Err(ref err) if err.contains("bad argument #1 to 'insert'")));
    }

    #[test]
    fn table_insert_accepts_out_of_bounds_position_sparse_growth() {
        // (frankenredis-jwkhc) Redis's vendored Lua has the
        // `luaL_argcheck(... position out of bounds)` stripped from
        // ltablib.c::tinsert — sparse / non-contiguous positions are
        // accepted and the array grows to fit. fr previously errored.
        let mut store = Store::new();
        let r = eval_script(
            b"local t = {1, 2}; table.insert(t, 4, 3); return t[4]",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("insert pos > #t+1");
        assert!(matches!(r, RespFrame::Integer(3)));
    }

    #[test]
    fn table_insert_rejects_non_numeric_position() {
        let mut store = Store::new();
        let result = eval_script(
            b"local t = {1, 2}\ntable.insert(t, 'x', 3)",
            &[],
            &[],
            &mut store,
            0,
        );

        assert!(matches!(result, Err(ref err) if err.contains("bad argument #2 to 'insert'")));
    }

    #[test]
    fn table_remove_rejects_non_table_argument() {
        let mut store = Store::new();
        let result = eval_script(b"return table.remove(42)", &[], &[], &mut store, 0);

        assert!(matches!(result, Err(ref err) if err.contains("bad argument #1 to 'remove'")));
    }

    #[test]
    fn table_remove_rejects_non_numeric_position() {
        let mut store = Store::new();
        let result = eval_script(
            b"local t = {1, 2}\nreturn table.remove(t, 'x')",
            &[],
            &[],
            &mut store,
            0,
        );

        assert!(matches!(result, Err(ref err) if err.contains("bad argument #2 to 'remove'")));
    }

    /// (frankenredis-2hgg1) Redis's vendored Lua 5.1's
    /// ltablib.c::tremove pushes ZERO Lua values when the position is
    /// out of bounds (the predicate `1 <= pos && pos <= e`). Pre-fix fr
    /// pushed a single Lua nil, changing the return arity from 0 to 1.
    /// Verified against vendored Redis 7.2.4 via differential probe.
    #[test]
    fn table_remove_out_of_bounds_returns_no_values_per_redis_lua_2hgg1() {
        let mut store = Store::new();

        let r = eval_script(
            b"return select('#', table.remove({}))",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("empty table");
        assert_eq!(r, RespFrame::Integer(0));

        let r = eval_script(
            b"return select('#', table.remove({1,2}, 100))",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("oob positive");
        assert_eq!(r, RespFrame::Integer(0));

        let r = eval_script(
            b"return select('#', table.remove({1,2}, 0))",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("zero pos");
        assert_eq!(r, RespFrame::Integer(0));

        let r = eval_script(
            b"return select('#', table.remove({1,2}, -1))",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("negative pos");
        assert_eq!(r, RespFrame::Integer(0));

        // In-bounds still returns 1 value.
        let r = eval_script(
            b"return select('#', table.remove({1,2}))",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("default last");
        assert_eq!(r, RespFrame::Integer(1));

        // In-bounds remove from position 1 shifts the rest left; the
        // trailing slot t[#t] becomes nil and is dropped by Redis's
        // RESP array conversion (it truncates at the first nil per
        // Lua's table-to-array protocol).
        let r = eval_script(
            b"local t={1,2,3}; local r=table.remove(t,1); return {r, t[1], t[2], t[3]}",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("shift left");
        assert_eq!(
            r,
            RespFrame::Array(Some(vec![
                RespFrame::Integer(1),
                RespFrame::Integer(2),
                RespFrame::Integer(3),
            ]))
        );
    }

    #[test]
    fn table_concat_rejects_non_table_argument() {
        let mut store = Store::new();
        let result = eval_script(b"return table.concat(42, ',')", &[], &[], &mut store, 0);

        assert!(matches!(result, Err(ref err) if err.contains("bad argument #1 to 'concat'")));
    }

    #[test]
    fn table_concat_rejects_non_numeric_start() {
        let mut store = Store::new();
        let result = eval_script(
            b"return table.concat({'a', 'b'}, ',', 'x')",
            &[],
            &[],
            &mut store,
            0,
        );

        assert!(matches!(result, Err(ref err) if err.contains("bad argument #3 to 'concat'")));
    }

    #[test]
    fn table_concat_rejects_non_numeric_end() {
        let mut store = Store::new();
        let result = eval_script(
            b"return table.concat({'a', 'b'}, ',', 1, 'x')",
            &[],
            &[],
            &mut store,
            0,
        );

        assert!(matches!(result, Err(ref err) if err.contains("bad argument #4 to 'concat'")));
    }

    #[test]
    fn table_concat_reads_raw_signed_integer_keys_ozc36() {
        let mut store = Store::new();
        let frame = eval_script(
            b"local t = {'one'}; t[-1] = 'neg'; t[0] = 'zero'; return table.concat(t, ',', -1, 1)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();

        assert_eq!(frame, RespFrame::BulkString(Some(b"neg,zero,one".to_vec())));
    }

    #[test]
    fn unpack_rejects_non_table_argument() {
        let mut store = Store::new();
        let result = eval_script(b"return unpack(42)", &[], &[], &mut store, 0);

        assert!(matches!(result, Err(ref err) if err.contains("bad argument #1 to 'unpack'")));
    }

    #[test]
    fn unpack_rejects_non_numeric_start() {
        let mut store = Store::new();
        let result = eval_script(b"return unpack({10, 20}, 'x')", &[], &[], &mut store, 0);

        assert!(matches!(result, Err(ref err) if err.contains("bad argument #2 to 'unpack'")));
    }

    #[test]
    fn unpack_rejects_non_numeric_end() {
        let mut store = Store::new();
        let result = eval_script(b"return unpack({10, 20}, 1, 'x')", &[], &[], &mut store, 0);

        assert!(matches!(result, Err(ref err) if err.contains("bad argument #3 to 'unpack'")));
    }

    #[test]
    fn unpack_reads_raw_integer_keys_across_signed_range_53i6p() {
        let mut store = Store::new();
        let frame = eval_script(
            b"local t = {10}; t[-1] = 'neg'; t[0] = 'zero'; t[3] = 'three'; local a,b,c,d,e = unpack(t, -1, 3); return tostring(a)..':'..tostring(b)..':'..tostring(c)..':'..tostring(d)..':'..tostring(e)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();

        assert_eq!(
            frame,
            RespFrame::BulkString(Some(b"neg:zero:10:nil:three".to_vec()))
        );
    }

    #[test]
    fn cjson_encode_sorts_string_hash_keys() {
        let table = LuaTable::new();
        table.set(LuaValue::Str(b"z".to_vec()), LuaValue::Number(1.0));
        table.set(LuaValue::Str(b"a".to_vec()), LuaValue::Number(2.0));

        assert_eq!(
            lua_value_to_json(&LuaValue::Table(table)).expect("encode"),
            "{\"a\":2,\"z\":1}"
        );
    }

    #[test]
    fn cjson_encode_escapes_all_json_control_characters() {
        assert_eq!(
            lua_value_to_json(&LuaValue::Str(b"\x08\x0c\x01".to_vec())).expect("encode"),
            "\"\\b\\f\\u0001\""
        );
    }

    #[test]
    fn cjson_encode_matches_upstream_number_and_slash_t6bqz() {
        // (frankenredis-t6bqz) cjson.encode must:
        // 1. Preserve -0 sign (upstream routes through %.14g).
        // 2. Render large integer-valued doubles via %.14g, losing
        //    precision past 14 significant digits.
        // 3. Escape forward slash as \\/.
        assert_eq!(
            lua_value_to_json(&LuaValue::Number(-0.0)).expect("neg zero"),
            "-0"
        );
        assert_eq!(
            lua_value_to_json(&LuaValue::Number(0.0)).expect("pos zero"),
            "0"
        );
        // 2^53 = 9_007_199_254_740_992. %.14g rounds the trailing
        // digits and switches to scientific notation.
        assert_eq!(
            lua_value_to_json(&LuaValue::Number((1u64 << 53) as f64)).expect("2^53"),
            "9.007199254741e+15"
        );
        // 2^53+1 is not exactly representable as f64 — it rounds to
        // 2^53, so the encoded value is the same.
        assert_eq!(
            lua_value_to_json(&LuaValue::Number(((1u64 << 53) + 1) as f64)).expect("2^53+1"),
            "9.007199254741e+15"
        );
        // Small integers still emit as bare decimals (matching %.14g).
        assert_eq!(lua_value_to_json(&LuaValue::Number(1.0)).expect("one"), "1");
        assert_eq!(
            lua_value_to_json(&LuaValue::Number(123.0)).expect("123"),
            "123"
        );
        // Forward slash escape inside strings.
        assert_eq!(
            lua_value_to_json(&LuaValue::Str(b"hello /world".to_vec())).expect("slash"),
            "\"hello \\/world\""
        );
        assert_eq!(
            lua_value_to_json(&LuaValue::Str(b"//".to_vec())).expect("dbl slash"),
            "\"\\/\\/\""
        );
    }

    #[test]
    fn cjson_decode_unescapes_slash_and_rejects_stray_commas_4h221() {
        // (frankenredis-4h221) cjson.decode parity with vendored:
        // 1. `\/` unescapes to `/` (symmetric with the always-on
        //    encode-side escape pinned by frankenredis-t6bqz).
        // 2. Empty slices around commas (`[1,]`, `[,1]`, `[1,,2]`,
        //    `{"a":1,}`) must raise — vendored's lua_cjson rejects
        //    each with "Expected value but found T_COMMA".
        let mut store = Store::new();
        // Slash unescape — direct + via decode-then-toString.
        let frame = eval_script(b"return cjson.decode('\"\\/\"')", &[], &[], &mut store, 0)
            .expect("slash decode");
        match frame {
            RespFrame::BulkString(Some(bytes)) => assert_eq!(bytes, b"/"),
            other => panic!("expected /, got {other:?}"),
        }
        // Strict comma rejection — each variant should round-trip to
        // `pcall(...)` returning false.
        for bad in [
            r#"return tostring(pcall(cjson.decode, '[1,]'))"#,
            r#"return tostring(pcall(cjson.decode, '[,1]'))"#,
            r#"return tostring(pcall(cjson.decode, '[1,,2]'))"#,
            r#"return tostring(pcall(cjson.decode, '{"a":1,}'))"#,
        ] {
            let frame = eval_script(bad.as_bytes(), &[], &[], &mut store, 0)
                .unwrap_or_else(|e| panic!("{bad} unexpected error: {e}"));
            match frame {
                RespFrame::BulkString(Some(bytes)) => assert_eq!(
                    bytes,
                    b"false",
                    "{bad}: expected pcall to return false (got {})",
                    String::from_utf8_lossy(&bytes)
                ),
                other => panic!("{bad}: expected bulkstring, got {other:?}"),
            }
        }
        // Direct call (no pcall) must surface a Lua error.
        let err = eval_script(b"return cjson.decode('[1,]')", &[], &[], &mut store, 0)
            .expect_err("trailing comma direct call must error");
        assert!(
            err.contains("Expected value"),
            "error must mention Expected value: {err}"
        );
    }

    #[test]
    fn cjson_decode_rejects_malformed_json_like_lua_cjson_qatse() {
        // (frankenredis-qatse) The decoder must reject malformed JSON
        // with lua-cjson's token-specific wording instead of accepting
        // trailing commas, missing values, or non-string object keys.
        let mut store = Store::new();
        let cases: &[(&[u8], &str)] = &[
            (
                br#"return cjson.decode('[1,2,]')"#,
                "user_script:1: Expected value but found T_ARR_END at character 6",
            ),
            (
                br#"return cjson.decode('{"a":1,}')"#,
                "user_script:1: Expected object key string but found T_OBJ_END at character 8",
            ),
            (
                br#"return cjson.decode('{1:2}')"#,
                "user_script:1: Expected object key string but found T_NUMBER at character 2",
            ),
            (
                br#"return cjson.decode('{"a":}')"#,
                "user_script:1: Expected value but found T_OBJ_END at character 6",
            ),
            (
                br#"return cjson.decode('[1,2 3]')"#,
                "user_script:1: Expected comma or array end but found T_NUMBER at character 6",
            ),
            (
                br#"return cjson.decode('"abc')"#,
                "user_script:1: Expected value but found unexpected end of string at character 5",
            ),
            (
                br#"return cjson.decode('abc')"#,
                "user_script:1: Expected value but found invalid token at character 1",
            ),
        ];
        for (script, expected) in cases {
            let err = eval_script(script, &[], &[], &mut store, 0).unwrap_err();
            assert_eq!(&err, expected, "script={}", String::from_utf8_lossy(script));
        }

        let frame = eval_script(
            br#"return cjson.decode('{"a":[1,true,false,null],"b":"\/"}').b"#,
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("valid nested json still decodes");
        assert_eq!(frame, RespFrame::BulkString(Some(b"/".to_vec())));
    }

    #[test]
    fn lua_cmsgpack_pack_unpack_roundtrips_7gtvz() {
        // (frankenredis-7gtvz) Redis exposes bundled lua-cmsgpack in
        // the Lua sandbox. Pin the public table plus core MessagePack
        // stream, array, map, and offset-unpack behavior.
        let mut store = Store::new();

        let frame = eval_script(
            b"return type(cmsgpack)..':'..type(cmsgpack.pack)..':'..type(cmsgpack.unpack)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(
            frame,
            RespFrame::BulkString(Some(b"table:function:function".to_vec()))
        );

        let frame = eval_script(
            b"return cmsgpack._NAME..'|'..cmsgpack._VERSION..'|'..cmsgpack._COPYRIGHT..'|'..cmsgpack._DESCRIPTION",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(
            frame,
            RespFrame::BulkString(Some(
                b"cmsgpack|lua-cmsgpack 0.4.0|Copyright (C) 2012, Salvatore Sanfilippo|MessagePack C implementation for Lua".to_vec()
            ))
        );

        let frame = eval_script(
            b"return string.byte(cmsgpack.pack(1), 1)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::Integer(1));

        let frame = eval_script(
            b"local p=cmsgpack.pack(128); local a,b=string.byte(p,1,2); return tostring(a)..':'..tostring(b)..':'..tostring(#p)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"204:128:2".to_vec())));

        let frame = eval_script(
            b"local p=cmsgpack.pack(65536); local a,b,c,d,e=string.byte(p,1,5); return tostring(a)..':'..tostring(b)..':'..tostring(c)..':'..tostring(d)..':'..tostring(e)..':'..tostring(#p)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(
            frame,
            RespFrame::BulkString(Some(b"206:0:1:0:0:5".to_vec()))
        );

        let frame = eval_script(
            b"local p=cmsgpack.pack(4294967296); return tostring(string.byte(p,1))..':'..tostring(#p)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"207:9".to_vec())));

        let frame = eval_script(
            b"local a=cmsgpack.pack(1.5); local b=cmsgpack.pack(1.1); local c=cmsgpack.pack(math.huge); return tostring(string.byte(a,1))..':'..tostring(#a)..':'..tostring(string.byte(b,1))..':'..tostring(#b)..':'..tostring(string.byte(c,1))..':'..tostring(#c)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(
            frame,
            RespFrame::BulkString(Some(b"202:5:203:9:202:5".to_vec()))
        );

        let frame = eval_script(
            b"local t=cmsgpack.unpack(cmsgpack.pack({1,2,3})); return cjson.encode(t)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"[1,2,3]".to_vec())));

        let frame = eval_script(
            b"local m=cmsgpack.unpack(cmsgpack.pack({a='x', b=2})); return m.a..':'..m.b",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"x:2".to_vec())));

        let frame = eval_script(
            b"local a,b,c=cmsgpack.unpack(cmsgpack.pack(1,'x',true)); return tostring(a)..':'..b..':'..tostring(c)",
            &[], &[], &mut store, 0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"1:x:true".to_vec())));

        let frame = eval_script(
            b"local off,v=cmsgpack.unpack_one(cmsgpack.pack(1,2)); return tostring(off)..':'..tostring(v)",
            &[], &[], &mut store, 0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"1:1".to_vec())));

        let frame = eval_script(
            b"local ok,e=pcall(cmsgpack.unpack, string.char(0xc1)); return tostring(e)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(
            frame,
            RespFrame::BulkString(Some(b"Bad data format in input.".to_vec()))
        );
    }

    #[test]
    fn lua_struct_pack_unpack_size_surface_oybyb() {
        // (frankenredis-oybyb) Redis exposes lua-struct as a global
        // sandbox library for fixed-width binary packing.
        let mut store = Store::new();

        let frame = eval_script(
            b"return type(struct)..':'..type(struct.pack)..':'..type(struct.unpack)..':'..type(struct.size)",
            &[], &[], &mut store, 0,
        )
        .unwrap();
        assert_eq!(
            frame,
            RespFrame::BulkString(Some(b"table:function:function:function".to_vec()))
        );

        let frame = eval_script(
            b"local n,pos=struct.unpack('>I2', string.char(0x12,0x34)); return tostring(n)..':'..tostring(pos)",
            &[], &[], &mut store, 0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"4660:3".to_vec())));

        let frame = eval_script(
            b"local b=struct.pack('>i2Bc0s', -2, 3, 'abc', 'z'); local i,s,z,pos=struct.unpack('>i2Bc0s', b); return tostring(i)..':'..s..':'..z..':'..tostring(pos)",
            &[], &[], &mut store, 0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"-2:abc:z:9".to_vec())));

        let frame = eval_script(
            b"return tostring(struct.size('!4bi'))..':'..tostring(struct.size('>i2xB'))",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"8:4".to_vec())));

        let frame = eval_script(
            b"local ok,e=pcall(struct.unpack, 'c0', ''); return tostring(ok)..':'..tostring(e)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(
            frame,
            RespFrame::BulkString(Some(b"false:format 'c0' needs a previous size".to_vec()))
        );

        let frame = eval_script(
            b"local ok,e=pcall(struct.size, 's'); return tostring(ok)..':'..tostring(e)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(
            frame,
            RespFrame::BulkString(Some(
                b"false:bad argument #1 to '?' (option 's' has no fixed size)".to_vec()
            ))
        );

        let frame = eval_script(
            b"local ok,e=pcall(struct.unpack, 'bc0', string.char(255)); return tostring(ok)..':'..tostring(e)",
            &[], &[], &mut store, 0,
        )
        .unwrap();
        assert_eq!(
            frame,
            RespFrame::BulkString(Some(
                b"false:bad argument #2 to '?' (data string too short)".to_vec()
            ))
        );

        let frame = eval_script(
            b"local f='b'..string.char(0)..'!3B'; local p=struct.pack(f, 7, 8); local n,pos=struct.unpack(f, p..string.char(9)); return tostring(struct.size(f))..':'..tostring(#p)..':'..tostring(n)..':'..tostring(pos)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"1:1:7:2".to_vec())));
    }

    #[test]
    fn cjson_null_decodes_as_userdata_sentinel_v29t6() {
        // (frankenredis-v29t6) Redis-bundled lua-cjson represents JSON
        // null as cjson.null, a lightuserdata sentinel distinct from
        // Lua nil and round-trippable through cjson.encode.
        let mut store = Store::new();

        let frame = eval_script(
            b"return type(cjson.null)..':'..type(cjson.decode('null'))..':'..tostring(cjson.decode('null') == cjson.null)",
            &[], &[], &mut store, 0,
        )
        .unwrap();
        assert_eq!(
            frame,
            RespFrame::BulkString(Some(b"userdata:userdata:true".to_vec()))
        );

        let frame =
            eval_script(b"return cjson.encode(cjson.null)", &[], &[], &mut store, 0).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"null".to_vec())));

        let frame = eval_script(
            b"return cjson.encode(cjson.decode('[null,true]'))",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"[null,true]".to_vec())));

        let frame = eval_script(
            b"return cjson.encode(cjson.decode('{\"a\":null}'))",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"{\"a\":null}".to_vec())));
    }

    #[test]
    fn lua_newproxy_userdata_metatable_surface_2wn7j() {
        // (frankenredis-2wn7j) Redis exposes Lua 5.1's newproxy helper.
        // It creates userdata proxies, optionally with a fresh metatable
        // or with the metatable copied from an existing valid proxy.
        let mut store = Store::new();

        let frame = eval_script(
            b"return type(newproxy)..':'..type(newproxy())..':'..tostring(getmetatable(newproxy()) == nil)",
            &[], &[], &mut store, 0,
        )
        .unwrap();
        assert_eq!(
            frame,
            RespFrame::BulkString(Some(b"function:userdata:true".to_vec()))
        );

        let frame = eval_script(
            b"local u=newproxy(true); local mt=getmetatable(u); rawset(mt, 'answer', 42); return type(mt)..':'..tostring(getmetatable(u).answer)",
            &[], &[], &mut store, 0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"table:42".to_vec())));

        let frame = eval_script(
            b"local u=newproxy(true); rawset(getmetatable(u), '__tostring', function() return 'proxy' end); local v=newproxy(u); return tostring(v)..':'..tostring(u==v)..':'..tostring(getmetatable(u)==getmetatable(v))",
            &[], &[], &mut store, 0,
        )
        .unwrap();
        assert_eq!(
            frame,
            RespFrame::BulkString(Some(b"proxy:false:true".to_vec()))
        );

        let frame = eval_script(
            b"local ok,e=pcall(newproxy, {}); return tostring(ok)..':'..tostring(e)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(
            frame,
            RespFrame::BulkString(Some(
                b"false:bad argument #1 to '?' (boolean or proxy expected)".to_vec()
            ))
        );
    }

    #[test]
    fn cjson_encode_rejects_nan_inf_function_thread_match_upstream() {
        // (frankenredis-bum6y) Redis-bundled cjson rejects NaN/+-Inf and
        // unsupported types with specific wordings. Verified against
        // vendored Redis 7.2.4 on :16380.
        for n in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(
                lua_value_to_json(&LuaValue::Number(n)).unwrap_err(),
                "Cannot serialise number: must not be NaN or Inf",
                "n = {n}"
            );
        }
        assert_eq!(
            lua_value_to_json(&LuaValue::RustFunction("noop".to_string())).unwrap_err(),
            "Cannot serialise function: type not supported"
        );
        // Wrap an unsupported value inside a table; encoder must propagate
        // the same error string rather than fall through to 'null'.
        let t = LuaTable::new();
        t.inner.borrow_mut().array.push(LuaValue::Number(1.0));
        t.inner.borrow_mut().array.push(LuaValue::Number(f64::NAN));
        assert_eq!(
            lua_value_to_json(&LuaValue::Table(t)).unwrap_err(),
            "Cannot serialise number: must not be NaN or Inf"
        );
        // Finite numbers, strings, booleans, nil must still encode.
        assert_eq!(
            lua_value_to_json(&LuaValue::Number(1.5)).expect("finite"),
            "1.5"
        );
        assert_eq!(
            lua_value_to_json(&LuaValue::Bool(true)).expect("bool"),
            "true"
        );
        assert_eq!(lua_value_to_json(&LuaValue::Nil).expect("nil"), "null");
    }

    /// (frankenredis-whyor) Lua 5.1.5 llex.c::read_string treats
    /// unrecognized escape sequences (anything outside
    /// abfnrtv\\\"'\\n\\r and digit escapes) by *dropping the
    /// backslash* and keeping the next byte verbatim. Vendored Redis
    /// 7.2.4 ships Lua 5.1.5 unchanged, so `'\\xff'` parses to the
    /// 3-byte string "xff" (not the 1-byte 0xFF — Lua 5.1 has no hex
    /// escape syntax). Pre-fix fr preserved both bytes (`\\` + `x`),
    /// which broke any script using \\xNN-style literals (e.g.
    /// redis.sha1hex on binary payloads).
    #[test]
    fn lua_string_escape_unknown_drops_backslash_per_lua_5_1_5_whyor() {
        let mut store = Store::new();
        let len = eval_script(
            b"return string.len('\\x00\\x01\\xff')",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("string.len");
        assert_eq!(len, RespFrame::Integer(9), "literal 9 chars: x00x01xff");

        let first_byte = eval_script(b"return string.byte('\\xff')", &[], &[], &mut store, 0)
            .expect("string.byte");
        assert_eq!(
            first_byte,
            RespFrame::Integer(120),
            "'\\xff' parses as 'xff'; first byte is ASCII 'x' (120)"
        );

        let known = eval_script(
            b"return {string.byte('\\a'), string.byte('\\b'), string.byte('\\f'), string.byte('\\v')}",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("named escapes");
        assert_eq!(
            known,
            RespFrame::Array(Some(vec![
                RespFrame::Integer(7),
                RespFrame::Integer(8),
                RespFrame::Integer(12),
                RespFrame::Integer(11),
            ])),
            "\\a \\b \\f \\v must resolve to 0x07 0x08 0x0c 0x0b"
        );

        // (frankenredis-8xuri) Mirror Lua 5.1.5 llex.c::luaX_lexerror
        // near-suffix: "escape sequence too large near '<delim>'"
        // where <delim> is the opening quote rendering. Pin both
        // ' and " variants.
        let too_big_single = eval_script(b"return '\\256'", &[], &[], &mut store, 0);
        match too_big_single {
            Err(msg) => assert!(
                msg.contains("escape sequence too large near '''"),
                "expected too-large + near suffix for single quote, got {msg:?}"
            ),
            Ok(other) => panic!("\\256 must error, got {other:?}"),
        }
        let too_big_double = eval_script(b"return \"\\256\"", &[], &[], &mut store, 0);
        match too_big_double {
            Err(msg) => assert!(
                msg.contains("escape sequence too large near '\"'"),
                "expected too-large + near suffix for double quote, got {msg:?}"
            ),
            Ok(other) => panic!("\\256 must error, got {other:?}"),
        }

        // SHA1 over the parsed bytes: hash of 9-byte "x00x01xff" must
        // match what vendored returns for the same literal source.
        let sha = eval_script(
            b"return redis.sha1hex('\\x00\\x01\\xff')",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("sha1hex");
        assert_eq!(
            sha,
            RespFrame::BulkString(Some(b"8e4d7558499d94310d39f185778734645ca58577".to_vec())),
            "sha1 over literal \"x00x01xff\" must match vendored Redis 7.2.4"
        );
    }

    #[test]
    fn string_format_no_args_does_not_panic_and_matches_upstream() {
        // (frankenredis-be7o1) Regression: string.format() with no args
        // panicked on `args[1..]` slice, crashing the server. Must error
        // cleanly with the standard luaL_checkstring wording.
        let mut store = Store::new();
        let err = eval_script(b"return string.format()", &[], &[], &mut store, 0).unwrap_err();
        assert_eq!(
            err,
            "user_script:1: bad argument #1 to 'format' (string expected, got no value)"
        );
        // Server is still alive — issue a second command on the same Store.
        let r2 = eval_script(b"return 1", &[], &[], &mut store, 0).expect("eval after");
        assert!(matches!(r2, RespFrame::Integer(1)));
    }

    #[test]
    fn string_format_validates_specs_and_args_match_upstream() {
        // (frankenredis-be7o1) Pin error wording for the four newly-tightened
        // string.format paths vs vendored Redis 7.2.4 (:16380).
        let mut store = Store::new();
        let cases: &[(&[u8], &str)] = &[
            (
                b"return string.format('%d')",
                "user_script:1: bad argument #2 to 'format' (no value)",
            ),
            (
                b"return string.format('%d', 'abc')",
                "user_script:1: bad argument #2 to 'format' (number expected, got string)",
            ),
            (
                b"return string.format('%d', true)",
                "user_script:1: bad argument #2 to 'format' (number expected, got boolean)",
            ),
            (
                b"return string.format('%d', nil)",
                "user_script:1: bad argument #2 to 'format' (number expected, got nil)",
            ),
            (
                b"return string.format('%d', {})",
                "user_script:1: bad argument #2 to 'format' (number expected, got table)",
            ),
            (
                b"return string.format('%K', 5)",
                "user_script:1: invalid option '%K' to 'format'",
            ),
        ];
        for (script, expected) in cases {
            let err = eval_script(script, &[], &[], &mut store, 0).unwrap_err();
            assert_eq!(
                err,
                *expected,
                "script {:?}",
                std::str::from_utf8(script).unwrap()
            );
        }
        // Happy paths still work.
        for (script, expected) in &[
            (b"return string.format('%d', 42)".as_slice(), "42"),
            (b"return string.format('%d', '42')", "42"),
            (b"return string.format('%d', 0x10)", "16"),
            (b"return string.format('%d', 1.7)", "1"),
            (b"return string.format('%x', 255)", "ff"),
            (b"return string.format('%-5d|', 42)", "42   |"),
            (b"return string.format('%%')", "%"),
        ] {
            let r = eval_script(script, &[], &[], &mut store, 0).expect("happy");
            match r {
                RespFrame::BulkString(Some(bytes)) => {
                    assert_eq!(
                        std::str::from_utf8(&bytes).unwrap(),
                        *expected,
                        "script {:?}",
                        std::str::from_utf8(script).unwrap()
                    );
                }
                other => panic!(
                    "script {:?}: expected bulk string, got {other:?}",
                    std::str::from_utf8(script).unwrap()
                ),
            }
        }
    }

    #[test]
    fn string_format_c_specifier_ascii_path() {
        // (frankenredis-be7o1) Pin the common ASCII case for %c. The
        // high-byte modulo-256 wrap path (and vendored's quirk of emitting
        // 0 bytes for %c 256) requires fr's lua_string_format to return
        // Vec<u8> instead of String — out of scope for this DOS-focused fix.
        let mut store = Store::new();
        let r =
            eval_script(b"return string.format('%c', 65)", &[], &[], &mut store, 0).expect("c 65");
        let RespFrame::BulkString(Some(bytes)) = r else {
            panic!("expected bulk")
        };
        assert_eq!(bytes, b"A".to_vec());
    }

    #[test]
    fn lua_stdlib_builtins_reject_missing_args_match_upstream() {
        // (frankenredis-3osi6) Verifies that ~27 Lua stdlib builtins which
        // were silently accepting no-arg/nil-arg calls now raise with the
        // exact upstream luaL_checkX wording.
        let mut store = Store::new();
        let cases: &[(&[u8], &str)] = &[
            // math.* — unary number
            (
                b"return math.abs()",
                "user_script:1: bad argument #1 to 'abs' (number expected, got no value)",
            ),
            (
                b"return math.ceil()",
                "user_script:1: bad argument #1 to 'ceil' (number expected, got no value)",
            ),
            (
                b"return math.floor()",
                "user_script:1: bad argument #1 to 'floor' (number expected, got no value)",
            ),
            (
                b"return math.sqrt()",
                "user_script:1: bad argument #1 to 'sqrt' (number expected, got no value)",
            ),
            (
                b"return math.exp()",
                "user_script:1: bad argument #1 to 'exp' (number expected, got no value)",
            ),
            (
                b"return math.log()",
                "user_script:1: bad argument #1 to 'log' (number expected, got no value)",
            ),
            (
                b"return math.log10()",
                "user_script:1: bad argument #1 to 'log10' (number expected, got no value)",
            ),
            (
                b"return math.sin()",
                "user_script:1: bad argument #1 to 'sin' (number expected, got no value)",
            ),
            (
                b"return math.cos()",
                "user_script:1: bad argument #1 to 'cos' (number expected, got no value)",
            ),
            (
                b"return math.tan()",
                "user_script:1: bad argument #1 to 'tan' (number expected, got no value)",
            ),
            (
                b"return math.deg()",
                "user_script:1: bad argument #1 to 'deg' (number expected, got no value)",
            ),
            (
                b"return math.rad()",
                "user_script:1: bad argument #1 to 'rad' (number expected, got no value)",
            ),
            (
                b"return math.modf()",
                "user_script:1: bad argument #1 to 'modf' (number expected, got no value)",
            ),
            (
                b"return math.frexp()",
                "user_script:1: bad argument #1 to 'frexp' (number expected, got no value)",
            ),
            // math.* — arg-#2 missing
            (
                b"return math.fmod()",
                "user_script:1: bad argument #2 to 'fmod' (number expected, got no value)",
            ),
            (
                b"return math.fmod(1)",
                "user_script:1: bad argument #2 to 'fmod' (number expected, got no value)",
            ),
            (
                b"return math.pow(1)",
                "user_script:1: bad argument #2 to 'pow' (number expected, got no value)",
            ),
            (
                b"return math.atan2(1)",
                "user_script:1: bad argument #2 to 'atan2' (number expected, got no value)",
            ),
            (
                b"return math.ldexp(1)",
                "user_script:1: bad argument #2 to 'ldexp' (number expected, got no value)",
            ),
            // string.* — arg #1 string missing
            (
                b"return string.len()",
                "user_script:1: bad argument #1 to 'len' (string expected, got no value)",
            ),
            (
                b"return string.lower()",
                "user_script:1: bad argument #1 to 'lower' (string expected, got no value)",
            ),
            (
                b"return string.upper()",
                "user_script:1: bad argument #1 to 'upper' (string expected, got no value)",
            ),
            (
                b"return string.reverse()",
                "user_script:1: bad argument #1 to 'reverse' (string expected, got no value)",
            ),
            (
                b"return string.rep()",
                "user_script:1: bad argument #1 to 'rep' (string expected, got no value)",
            ),
            (
                b"return string.byte()",
                "user_script:1: bad argument #1 to 'byte' (string expected, got no value)",
            ),
            (
                b"return string.find()",
                "user_script:1: bad argument #1 to 'find' (string expected, got no value)",
            ),
            (
                b"return string.match()",
                "user_script:1: bad argument #1 to 'match' (string expected, got no value)",
            ),
            (
                b"return string.gmatch()",
                "user_script:1: bad argument #1 to 'gmatch' (string expected, got no value)",
            ),
            (
                b"return string.gsub()",
                "user_script:1: bad argument #1 to 'gsub' (string expected, got no value)",
            ),
            // string.* — arg-#2 missing
            (
                b"return string.sub('abc')",
                "user_script:1: bad argument #2 to 'sub' (number expected, got no value)",
            ),
            (
                b"return string.rep('a')",
                "user_script:1: bad argument #2 to 'rep' (number expected, got no value)",
            ),
            (
                b"return string.find('abc')",
                "user_script:1: bad argument #2 to 'find' (string expected, got no value)",
            ),
            (
                b"return string.match('abc')",
                "user_script:1: bad argument #2 to 'match' (string expected, got no value)",
            ),
            (
                b"return string.gmatch('abc')",
                "user_script:1: bad argument #2 to 'gmatch' (string expected, got no value)",
            ),
            (
                b"return string.gsub('abc')",
                "user_script:1: bad argument #2 to 'gsub' (string expected, got no value)",
            ),
            // table.*
            (
                b"return table.sort()",
                "user_script:1: bad argument #1 to 'sort' (table expected, got no value)",
            ),
            (
                b"return table.maxn()",
                "user_script:1: bad argument #1 to 'maxn' (table expected, got no value)",
            ),
            // base
            (
                b"return tonumber()",
                "user_script:1: bad argument #1 to 'tonumber' (value expected)",
            ),
        ];
        for (script, expected) in cases {
            let err = eval_script(script, &[], &[], &mut store, 0).unwrap_err();
            assert_eq!(
                err,
                *expected,
                "script {:?}",
                std::str::from_utf8(script).unwrap()
            );
        }
        // Type-mismatch sample: math.floor(true) reports 'got boolean'.
        let err = eval_script(b"return math.floor(true)", &[], &[], &mut store, 0).unwrap_err();
        assert_eq!(
            err,
            "user_script:1: bad argument #1 to 'floor' (number expected, got boolean)"
        );
        // Happy-path regressions: each function still works correctly.
        let happy_pairs: &[(&[u8], RespFrame)] = &[
            (b"return math.floor(3.7)", RespFrame::Integer(3)),
            (b"return math.abs(-5)", RespFrame::Integer(5)),
            (b"return math.fmod(7,3)", RespFrame::Integer(1)),
            (b"return math.pow(2,3)", RespFrame::Integer(8)),
            (b"return string.len('abc')", RespFrame::Integer(3)),
            (
                b"return string.upper('abc')",
                RespFrame::BulkString(Some(b"ABC".to_vec())),
            ),
        ];
        for (script, expected) in happy_pairs {
            let r = eval_script(script, &[], &[], &mut store, 0).expect("happy");
            assert_eq!(
                r,
                *expected,
                "script {:?}",
                std::str::from_utf8(script).unwrap()
            );
        }
        // Server still alive after all the error cases: a follow-up eval works.
        let r = eval_script(b"return 1 + 1", &[], &[], &mut store, 0).expect("after");
        assert!(matches!(r, RespFrame::Integer(2)));
    }

    #[test]
    fn table_constructor_preserves_nil_holes_for_border_and_concat_y0ri2() {
        // (frankenredis-y0ri2) Lua 5.1 table constructor pre-allocates a
        // slot for every positional field, even nil. fr previously routed
        // through LuaTable::set, which drops a nil at array.len()+1,
        // losing the slot. Both `#t` and `table.concat`'s default `last`
        // depend on the slot being present.
        let mut store = Store::new();
        for (script, expected) in &[
            // (frankenredis-y0ri2) `#{1,nil,3}` should return 3 — array
            // part is [1,nil,3], last slot is non-nil so luaH_getn returns
            // sizearray directly.
            (b"return #{1,nil,3}".as_ref(), "3"),
            // Last slot non-nil: returns sizearray = 4.
            (b"return #{1,2,nil,4}".as_ref(), "4"),
            // Last slot non-nil: returns sizearray = 3 even with leading nils.
            (b"return #{nil,nil,3}".as_ref(), "3"),
            // Last slot nil: binary border search returns 0 (no non-nil at
            // any index where t[i+1]=nil).
            (b"return #{nil,nil,3,nil}".as_ref(), "0"),
            // Last slot nil: binary border search returns 2 (t[2]=2, t[3]=nil).
            (b"return #{1,2,nil}".as_ref(), "2"),
        ] {
            let r = eval_script(script, &[], &[], &mut store, 0).unwrap_or_else(|e| {
                panic!("script {:?} failed: {}", String::from_utf8_lossy(script), e)
            });
            match r {
                RespFrame::Integer(n) => assert_eq!(
                    n.to_string(),
                    *expected,
                    "script {:?} expected {} got {}",
                    String::from_utf8_lossy(script),
                    expected,
                    n
                ),
                other => panic!(
                    "script {:?} expected integer {}, got {:?}",
                    String::from_utf8_lossy(script),
                    expected,
                    other
                ),
            }
        }
        // (frankenredis-y0ri2) table.concat with no explicit `last` now sees
        // the nil hole and raises 'invalid value (nil) at index 2 in table
        // for concat' to match vendored Redis 7.2.4. Previously fr returned
        // "1" because array.len() truncated at the first nil.
        let err = eval_script(
            b"return table.concat({1,nil,3}, ',')",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap_err();
        assert_eq!(
            err,
            "user_script:1: invalid value (nil) at index 2 in table for 'concat'"
        );

        // (frankenredis-y0ri2) ipairs still stops at the first nil per Lua
        // 5.1 ref §5.4 — array=[1,2,Nil,4] iterates only (1,1) and (2,2)
        // even though slot 4 is non-nil. Previously fr's array was [1,2]
        // and stopping was incidental; the explicit nil check now keeps
        // the behavior correct.
        let r = eval_script(
            b"local r={}; for i,v in ipairs({1,2,nil,4}) do r[#r+1]=v end; return r",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        match r {
            RespFrame::Array(Some(items)) => {
                let nums: Vec<i64> = items
                    .iter()
                    .filter_map(|f| match f {
                        RespFrame::Integer(n) => Some(*n),
                        _ => None,
                    })
                    .collect();
                assert_eq!(nums, vec![1, 2], "ipairs must stop at first nil");
            }
            other => panic!("expected Array, got {:?}", other),
        }

        // (frankenredis-y0ri2) `next` / `pairs` skip nil array entries.
        // Vendored emits keys {1, 3, 5} for `pairs({1,nil,3,nil,5})`;
        // fr now matches after teaching next() to advance past nil slots
        // in the array part (preserved nil slots would otherwise yield
        // spurious (2, nil) and (4, nil) pairs).
        let r = eval_script(
            b"local r={}; for k,_ in pairs({1,nil,3,nil,5}) do r[#r+1]=k end; table.sort(r); return r",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        match r {
            RespFrame::Array(Some(items)) => {
                let nums: Vec<i64> = items
                    .iter()
                    .filter_map(|f| match f {
                        RespFrame::Integer(n) => Some(*n),
                        _ => None,
                    })
                    .collect();
                assert_eq!(nums, vec![1, 3, 5], "pairs must skip nil array slots");
            }
            other => panic!("expected Array, got {:?}", other),
        }

        // next(t, prev) where prev's successor is nil must skip ahead to
        // the next non-nil slot — for {1,nil,3}, next(t,1) returns key=3.
        // (EVAL top-level return discards values beyond the first per
        // Redis's lua-to-RESP conversion, so we only check the key.)
        let r = eval_script(b"return ({next({1,nil,3}, 1)})[1]", &[], &[], &mut store, 0).unwrap();
        match r {
            RespFrame::Integer(n) => assert_eq!(n, 3, "next must skip nil slot to key 3"),
            other => panic!("expected Integer(3), got {:?}", other),
        }
    }

    #[test]
    fn table_concat_matches_upstream_lua_5_1() {
        // (frankenredis-jwkhc) table.concat semantics audit vs vendored 7.2.4.
        let mut store = Store::new();

        // nil separator coerces to empty string, not the literal "nil".
        let r = eval_script(
            b"return table.concat({1,2,3}, nil)",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("nil sep");
        assert_eq!(r, RespFrame::BulkString(Some(b"123".to_vec())));

        // missing separator equally OK.
        let r = eval_script(b"return table.concat({1,2,3})", &[], &[], &mut store, 0)
            .expect("default sep");
        assert_eq!(r, RespFrame::BulkString(Some(b"123".to_vec())));

        // nil hole inside an explicit iteration range raises with the
        // upstream wording. (We use an explicit range here because fr's
        // # operator returns a shorter length for sparse tables than
        // vendored's binary-search boundary, which would otherwise mask
        // the nil hole when relying on the default end argument.)
        let err = eval_script(
            b"return table.concat({1,nil,3}, '', 1, 3)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap_err();
        assert_eq!(
            err,
            "user_script:1: invalid value (nil) at index 2 in table for 'concat'"
        );

        // Out-of-range explicit end raises.
        let err = eval_script(
            b"return table.concat({1,2,3}, '', 1, 5)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap_err();
        assert_eq!(
            err,
            "user_script:1: invalid value (nil) at index 4 in table for 'concat'"
        );

        // Out-of-range explicit start raises.
        let err = eval_script(
            b"return table.concat({1,2,3}, '', 0, 3)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap_err();
        assert_eq!(
            err,
            "user_script:1: invalid value (nil) at index 0 in table for 'concat'"
        );

        // Mid-range nil hole inside an explicit range raises at that index.
        let err = eval_script(
            b"return table.concat({1,2,3,nil,5}, '', 1, 5)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap_err();
        assert_eq!(
            err,
            "user_script:1: invalid value (nil) at index 4 in table for 'concat'"
        );

        // Empty range is empty (Lua: when start > end, no iteration).
        let r = eval_script(
            b"return table.concat({1,2,3}, '-', 2, 1)",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("empty range");
        assert_eq!(r, RespFrame::BulkString(Some(Vec::new())));

        // Empty table is empty.
        let r =
            eval_script(b"return table.concat({})", &[], &[], &mut store, 0).expect("empty table");
        assert_eq!(r, RespFrame::BulkString(Some(Vec::new())));

        // Non-string non-number entry raises with the type name.
        let err = eval_script(
            b"return table.concat({1, true, 3})",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap_err();
        assert_eq!(
            err,
            "user_script:1: invalid value (boolean) at index 2 in table for 'concat'"
        );
    }

    #[test]
    fn table_insert_wrong_arity_and_sparse_grow_match_upstream() {
        // (frankenredis-jwkhc) Upstream: 1-arg call errors; 2-arg appends;
        // 3-arg with out-of-bounds pos grows the array sparsely.
        let mut store = Store::new();

        // 1-arg call → upstream raises explicit "wrong number of arguments".
        let err = eval_script(b"table.insert({})", &[], &[], &mut store, 0).unwrap_err();
        assert_eq!(err, "user_script:1: wrong number of arguments to 'insert'");

        // 4+ args also rejected with same wording.
        let err = eval_script(
            b"table.insert({}, 1, 'x', 'extra')",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap_err();
        assert_eq!(err, "user_script:1: wrong number of arguments to 'insert'");

        // 2-arg append still works.
        let r = eval_script(
            b"local t = {1, 2}; table.insert(t, 3); return t[3]",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("append");
        assert!(matches!(r, RespFrame::Integer(3)));

        // 3-arg insert at #t+1 = append boundary.
        let r = eval_script(
            b"local t = {1, 2}; table.insert(t, 3, 'x'); return t[3]",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("boundary insert");
        assert_eq!(r, RespFrame::BulkString(Some(b"x".to_vec())));

        // 3-arg insert at sparse pos grows.
        let r = eval_script(
            b"local t = {1, 2}; table.insert(t, 10, 'x'); return t[10]",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("sparse");
        assert_eq!(r, RespFrame::BulkString(Some(b"x".to_vec())));

        // 3-arg insert at pos 0 stores in hash part.
        let r = eval_script(
            b"local t = {1, 2}; table.insert(t, 0, 'x'); return t[0]",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("pos 0");
        assert_eq!(r, RespFrame::BulkString(Some(b"x".to_vec())));
    }

    #[test]
    fn lua_base_lib_parity_uyj7c() {
        // (frankenredis-uyj7c) Pin error wording / arg validation for the
        // base-library divergences uncovered after the lua_stdlib audit.
        let mut store = Store::new();

        // 1. rawlen must NOT be a global (Lua 5.1, no rawlen).
        let err = eval_script(b"return rawlen('abc')", &[], &[], &mut store, 0).unwrap_err();
        assert!(
            err.contains("nonexistent global variable 'rawlen'"),
            "rawlen err: {err}"
        );

        // 2-4. rawset arg checks.
        let cases: &[(&[u8], &str)] = &[
            (
                b"return rawset()",
                "user_script:1: bad argument #1 to 'rawset' (table expected, got no value)",
            ),
            (
                b"return rawset({}, 1)",
                "user_script:1: bad argument #3 to 'rawset' (value expected)",
            ),
            (b"return rawset({}, nil, 1)", "table index is nil"),
            // 5. rawequal arg checks.
            (
                b"return rawequal()",
                "user_script:1: bad argument #1 to 'rawequal' (value expected)",
            ),
            (
                b"return rawequal(1)",
                "user_script:1: bad argument #2 to 'rawequal' (value expected)",
            ),
            // 6. loadstring no-arg and nil-arg wording.
            (
                b"return loadstring()",
                "user_script:1: bad argument #1 to 'loadstring' (string expected, got no value)",
            ),
            (
                b"return loadstring(nil)",
                "user_script:1: bad argument #1 to 'loadstring' (string expected, got nil)",
            ),
            // 7. collectgarbage invalid option.
            (
                b"return collectgarbage('unknown')",
                "user_script:1: bad argument #1 to 'collectgarbage' (invalid option 'unknown')",
            ),
        ];
        for (script, expected) in cases {
            let err = eval_script(script, &[], &[], &mut store, 0).unwrap_err();
            assert_eq!(
                err,
                *expected,
                "script {:?}",
                std::str::from_utf8(script).unwrap()
            );
        }

        // 8. error() level semantics. Only level==1 prepends the source
        // location; level==0 and level>=2 and level<0 don't.
        for (script, expected) in &[
            (b"error('x', 0)".as_slice(), "x"),
            (b"error('x', 1)", "user_script:1: x"),
            (b"error('x', 2)", "x"),
            (b"error('x', 5)", "x"),
            (b"error('x', -1)", "x"),
            (b"error('x')", "user_script:1: x"), // default level=1
        ] {
            let err = eval_script(script, &[], &[], &mut store, 0).unwrap_err();
            assert_eq!(
                err,
                *expected,
                "script {:?}",
                std::str::from_utf8(script).unwrap()
            );
        }

        // 9. Happy paths still work.
        let r =
            eval_script(b"return rawequal(1, 1)", &[], &[], &mut store, 0).expect("rawequal ok");
        assert_eq!(r, RespFrame::Integer(1));
        let r = eval_script(
            b"local t={}; rawset(t, 'k', 42); return t.k",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("rawset ok");
        assert_eq!(r, RespFrame::Integer(42));
        let r =
            eval_script(b"return collectgarbage('count')", &[], &[], &mut store, 0).expect("count");
        assert_eq!(r, RespFrame::Integer(32));
    }

    #[test]
    fn cjson_decode_understands_control_character_escapes() {
        match json_to_lua_value("\"\\b\\f\\u0001\"") {
            Ok(LuaValue::Str(bytes)) => assert_eq!(bytes, vec![0x08, 0x0C, 0x01]),
            other => unreachable!("unexpected decode result: {other:?}"),
        }
    }

    #[test]
    fn coroutine_misuse_errors_match_upstream_pending_bw15() {
        let mut store = Store::new();
        for (script, expected) in [
            (
                b"return coroutine.create(nil)".as_slice(),
                "user_script:1: bad argument #1 to 'create' (Lua function expected)",
            ),
            (
                b"return coroutine.wrap(nil)".as_slice(),
                "user_script:1: bad argument #1 to 'wrap' (Lua function expected)",
            ),
            (
                b"return coroutine.resume(nil)".as_slice(),
                "user_script:1: bad argument #1 to 'resume' (coroutine expected)",
            ),
            (
                b"return coroutine.status(nil)".as_slice(),
                "user_script:1: bad argument #1 to 'status' (coroutine expected)",
            ),
            (
                b"return coroutine.yield()".as_slice(),
                "attempt to yield across metamethod/C-call boundary",
            ),
        ] {
            let result = eval_script(script, &[], &[], &mut store, 0);
            assert_eq!(result, Err(expected.to_string()), "script {script:?}");
        }
    }

    #[test]
    fn coroutine_table_is_accessible() {
        let mut store = Store::new();
        let result = eval_script(b"return type(coroutine)", &[], &[], &mut store, 0);
        assert_eq!(result, Ok(RespFrame::BulkString(Some(b"table".to_vec()))));
    }

    #[test]
    fn coroutine_running_returns_nil_in_main_thread() {
        let mut store = Store::new();
        let result = eval_script(b"return coroutine.running()", &[], &[], &mut store, 0);
        assert_eq!(result, Ok(RespFrame::BulkString(None)));
    }

    #[test]
    fn coroutine_resume_yields_then_completes_with_status_transitions() {
        let mut store = Store::new();
        let script = b"
            local co = coroutine.create(function(x)
                coroutine.yield(x * 2)
                return x * 3
            end)
            local s1 = coroutine.status(co)
            local ok1, first = coroutine.resume(co, 7)
            local s2 = coroutine.status(co)
            local ok2, second = coroutine.resume(co)
            local s3 = coroutine.status(co)
            return {s1, ok1, first, s2, ok2, second, s3}
        ";
        let result = eval_script(script, &[], &[], &mut store, 0);
        assert_eq!(
            result,
            Ok(RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"suspended".to_vec())),
                RespFrame::Integer(1),
                RespFrame::Integer(14),
                RespFrame::BulkString(Some(b"suspended".to_vec())),
                RespFrame::Integer(1),
                RespFrame::Integer(21),
                RespFrame::BulkString(Some(b"dead".to_vec())),
            ])))
        );
    }

    #[test]
    fn coroutine_wrap_resumes_same_thread_after_yield() {
        let mut store = Store::new();
        let script = b"
            local f = coroutine.wrap(function(x)
                coroutine.yield(x * 2)
                return x * 3
            end)
            return {f(4), f(4)}
        ";
        let result = eval_script(script, &[], &[], &mut store, 0);
        assert_eq!(
            result,
            Ok(RespFrame::Array(Some(vec![
                RespFrame::Integer(8),
                RespFrame::Integer(12),
            ])))
        );
    }

    #[test]
    fn coroutine_yield_in_local_assign_receives_resume_values() {
        let mut store = Store::new();
        let script = b"
            local co = coroutine.create(function(a)
                local b = coroutine.yield(a + 1)
                return b * 2
            end)
            local ok1, v1 = coroutine.resume(co, 10)
            local ok2, v2 = coroutine.resume(co, 5)
            return v1 .. ':' .. v2
        ";
        let result = eval_script(script, &[], &[], &mut store, 0);
        assert_eq!(result, Ok(RespFrame::BulkString(Some(b"11:10".to_vec()))));
    }

    #[test]
    fn coroutine_yield_in_return_stmt_returns_resume_values() {
        let mut store = Store::new();
        let script = b"
            local co = coroutine.create(function()
                return coroutine.yield()
            end)
            local ok1, first = coroutine.resume(co)
            local ok2, second = coroutine.resume(co, 'done')
            return {ok1, tostring(first), ok2, second}
        ";
        let result = eval_script(script, &[], &[], &mut store, 0);
        assert_eq!(
            result,
            Ok(RespFrame::Array(Some(vec![
                RespFrame::Integer(1),
                RespFrame::BulkString(Some(b"nil".to_vec())),
                RespFrame::Integer(1),
                RespFrame::BulkString(Some(b"done".to_vec())),
            ])))
        );
    }

    #[test]
    fn coroutine_yield_in_if_condition_truthy_runs_then_branch_7lmle() {
        let mut store = Store::new();
        let script = b"
            local co = coroutine.create(function(a)
                if coroutine.yield(a + 1) then
                    return 'truthy'
                else
                    return 'falsy'
                end
            end)
            local ok1, v1 = coroutine.resume(co, 10)
            local ok2, v2 = coroutine.resume(co, true)
            return v1 .. ':' .. v2
        ";
        let result = eval_script(script, &[], &[], &mut store, 0);
        assert_eq!(result, Ok(RespFrame::BulkString(Some(b"11:truthy".to_vec()))));
    }

    #[test]
    fn coroutine_yield_in_if_condition_falsy_falls_through_to_else_7lmle() {
        let mut store = Store::new();
        // Resume value nil is falsy, so the else branch must run.
        let script = b"
            local co = coroutine.create(function(a)
                if coroutine.yield(a + 1) then
                    return 'truthy'
                else
                    return 'falsy'
                end
            end)
            local ok1, v1 = coroutine.resume(co, 10)
            local ok2, v2 = coroutine.resume(co)
            return v1 .. ':' .. v2
        ";
        let result = eval_script(script, &[], &[], &mut store, 0);
        assert_eq!(result, Ok(RespFrame::BulkString(Some(b"11:falsy".to_vec()))));
    }

    #[test]
    fn coroutine_yield_in_chained_elseif_conditions_resume_each_7lmle() {
        // Verified against redis 7.2.4: `if coroutine.yield(1) ... elseif
        // coroutine.yield(2) ...` yields 1, then on a falsy resume yields 2,
        // then on a truthy resume runs that elseif branch. Result:
        // "1,true,2,true,b".
        let mut store = Store::new();
        let script = b"
            local co = coroutine.create(function()
                if coroutine.yield(1) then return 'a'
                elseif coroutine.yield(2) then return 'b' end
            end)
            local o1,y1 = coroutine.resume(co)
            local o2,y2 = coroutine.resume(co, false)
            local o3,y3 = coroutine.resume(co, true)
            return tostring(y1)..','..tostring(o2)..','..tostring(y2)..','..tostring(o3)..','..tostring(y3)
        ";
        let result = eval_script(script, &[], &[], &mut store, 0);
        assert_eq!(
            result,
            Ok(RespFrame::BulkString(Some(b"1,true,2,true,b".to_vec())))
        );
    }

    #[test]
    fn coroutine_yield_chain_falls_through_to_else_and_past_if_7lmle() {
        // Verified against redis 7.2.4.
        let mut store = Store::new();
        // 3-level chain, both yields falsy -> else branch runs: "1,2,true,c".
        let script_else = b"
            local co=coroutine.create(function()
                if coroutine.yield(1) then return 'a'
                elseif coroutine.yield(2) then return 'b'
                else return 'c' end
            end)
            local _,y1=coroutine.resume(co)
            local _,y2=coroutine.resume(co,false)
            local o3,y3=coroutine.resume(co,false)
            return tostring(y1)..','..tostring(y2)..','..tostring(o3)..','..tostring(y3)
        ";
        assert_eq!(
            eval_script(script_else, &[], &[], &mut store, 0),
            Ok(RespFrame::BulkString(Some(b"1,2,true,c".to_vec())))
        );
        // Chain with no else and no match -> execution continues past the if.
        let script_fall = b"
            local co=coroutine.create(function()
                if coroutine.yield(1) then return 'a' elseif coroutine.yield(2) then return 'b' end
                return 'fell'
            end)
            coroutine.resume(co); coroutine.resume(co,false)
            local o,v=coroutine.resume(co,false)
            return tostring(o)..','..tostring(v)
        ";
        assert_eq!(
            eval_script(script_fall, &[], &[], &mut store, 0),
            Ok(RespFrame::BulkString(Some(b"true,fell".to_vec())))
        );
    }

    #[test]
    fn coroutine_yield_as_while_condition_resumes_each_iteration_7lmle() {
        // Verified against redis 7.2.4: `while coroutine.yield(n) do n=n+1 end`
        // yields n each iteration; a truthy resume continues the loop, a falsy
        // resume exits and runs the trailing return. Result: "0,1,2,done:2".
        let mut store = Store::new();
        let script = b"
            local co=coroutine.create(function()
                local n=0
                while coroutine.yield(n) do n=n+1 end
                return 'done:'..n
            end)
            local _,a=coroutine.resume(co)
            local _,b=coroutine.resume(co,true)
            local _,c=coroutine.resume(co,true)
            local _,d=coroutine.resume(co,false)
            return tostring(a)..','..tostring(b)..','..tostring(c)..','..tostring(d)
        ";
        assert_eq!(
            eval_script(script, &[], &[], &mut store, 0),
            Ok(RespFrame::BulkString(Some(b"0,1,2,done:2".to_vec())))
        );
    }

    #[test]
    fn coroutine_yield_as_while_condition_break_exits_loop_7lmle() {
        // Verified against redis 7.2.4: a `break` in the body of a
        // yield-conditioned while exits the loop and runs trailing code.
        // Result: "0,1,after:2".
        let mut store = Store::new();
        let script = b"
            local co=coroutine.create(function()
                local n=0
                while coroutine.yield(n) do n=n+1; if n==2 then break end end
                return 'after:'..n
            end)
            local _,a=coroutine.resume(co)
            local _,b=coroutine.resume(co,true)
            local _,c=coroutine.resume(co,true)
            return tostring(a)..','..tostring(b)..','..tostring(c)
        ";
        assert_eq!(
            eval_script(script, &[], &[], &mut store, 0),
            Ok(RespFrame::BulkString(Some(b"0,1,after:2".to_vec())))
        );
    }

    #[test]
    fn coroutine_yield_as_repeat_until_condition_resumes_each_iteration_7lmle() {
        // Verified against redis 7.2.4: `repeat n=n+1 until coroutine.yield(n)`
        // runs the body then yields n; a falsy resume loops, a truthy resume
        // exits. Result: "1,2,done:2".
        let mut store = Store::new();
        let script = b"
            local co=coroutine.create(function()
                local n=0
                repeat n=n+1 until coroutine.yield(n)
                return 'done:'..n
            end)
            local _,a=coroutine.resume(co)
            local _,b=coroutine.resume(co,false)
            local _,c=coroutine.resume(co,true)
            return tostring(a)..','..tostring(b)..','..tostring(c)
        ";
        assert_eq!(
            eval_script(script, &[], &[], &mut store, 0),
            Ok(RespFrame::BulkString(Some(b"1,2,done:2".to_vec())))
        );
    }

    #[test]
    fn coroutine_yield_repeat_until_return_in_body_completes_7lmle() {
        // Verified against redis 7.2.4: a `return` in the repeat body completes
        // the coroutine before the until-condition is reached. Result:
        // "1,true,ret:2".
        let mut store = Store::new();
        let script = b"
            local co=coroutine.create(function()
                local n=0
                repeat n=n+1; if n==2 then return 'ret:'..n end until coroutine.yield(n)
                return 'done:'..n
            end)
            local _,a=coroutine.resume(co)
            local o2,b=coroutine.resume(co,false)
            return tostring(a)..','..tostring(o2)..','..tostring(b)
        ";
        assert_eq!(
            eval_script(script, &[], &[], &mut store, 0),
            Ok(RespFrame::BulkString(Some(b"1,true,ret:2".to_vec())))
        );
    }

    #[test]
    fn coroutine_yield_as_generic_for_iterator_resumes_with_triple_7lmle() {
        // Verified against redis 7.2.4: `for x in coroutine.yield("need") do ...`
        // suspends at the iterator expression; the resume value(s) become the
        // iterator triple (fn, state, control) and the loop runs. With the
        // resumed iterator below (control gets set to the returned value, not an
        // index) only the first element is produced. Result: "need,true,sum:10".
        let mut store = Store::new();
        let script = b"
            local co=coroutine.create(function()
                local sum=0
                for x in coroutine.yield('need') do sum=sum+x end
                return 'sum:'..sum
            end)
            local _,req=coroutine.resume(co)
            local data={10,20,30}
            local function it2(_, prev) prev=prev+1; if prev>3 then return nil end; return data[prev] end
            local o2,res=coroutine.resume(co, it2, nil, 0)
            return tostring(req)..','..tostring(o2)..','..tostring(res)
        ";
        assert_eq!(
            eval_script(script, &[], &[], &mut store, 0),
            Ok(RespFrame::BulkString(Some(b"need,true,sum:10".to_vec())))
        );
    }

    #[test]
    fn coroutine_yield_inside_for_loop_resumes_each_iteration() {
        let mut store = Store::new();
        let script = b"
            local co = coroutine.wrap(function()
                for i = 1, 3 do
                    coroutine.yield(i)
                end
            end)
            return co() .. co() .. co()
        ";
        let result = eval_script(script, &[], &[], &mut store, 0);
        assert_eq!(result, Ok(RespFrame::BulkString(Some(b"123".to_vec()))));
    }

    #[test]
    fn coroutine_yield_inside_pcall_errors_per_lua_5_1_semantics() {
        // (frankenredis-ztawj) Lua 5.1 doesn't allow yielding across
        // a pcall boundary. fr's nested_exec_stmts_depth check
        // catches this case (pcall's inner function call bumps
        // depth via exec_stmts) so the yield surfaces as an
        // error rather than silently dropping the rest of the
        // pcall'd body on resume.
        let mut store = Store::new();
        let script = b"
            local co = coroutine.create(function()
                pcall(function() coroutine.yield(1) end)
                return 'after-pcall'
            end)
            local ok, err = coroutine.resume(co)
            return {tostring(ok), err}
        ";
        let result = eval_script(script, &[], &[], &mut store, 0);
        // After the depth check, yield-inside-pcall errors. pcall
        // catches the error and returns (false, msg). The body
        // then continues to 'return after-pcall' — so resume
        // completes successfully with that value. Pin both halves.
        assert_eq!(
            result,
            Ok(RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"true".to_vec())),
                RespFrame::BulkString(Some(b"after-pcall".to_vec())),
            ])))
        );
    }

    #[test]
    fn coroutine_yield_sentinel_cannot_be_forged_via_user_error() {
        // (frankenredis-sjuu1) The yield mechanism uses an error
        // sentinel string '__frankenredis_lua_coroutine_yield__'.
        // A user script can pass that exact string to error() and
        // produce an Err with closely related payload — without the
        // prior gate it could reach exec_coroutine_stmts' yield arm
        // with pending_yield == None and fake an empty yield.
        //
        // After the fix, the spoofed sentinel is treated as a normal
        // error: coroutine.resume returns (false, source-prefixed
        // sentinel_string) and the coroutine transitions to dead.
        // Pin both halves.
        let mut store = Store::new();
        let script = b"
            local co = coroutine.create(function()
                error('__frankenredis_lua_coroutine_yield__')
            end)
            local ok, err = coroutine.resume(co)
            local s = coroutine.status(co)
            return {tostring(ok), err, s}
        ";
        let result = eval_script(script, &[], &[], &mut store, 0);
        assert_eq!(
            result,
            Ok(RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"false".to_vec())),
                RespFrame::BulkString(Some(
                    b"user_script:1: __frankenredis_lua_coroutine_yield__".to_vec()
                )),
                RespFrame::BulkString(Some(b"dead".to_vec())),
            ])))
        );
    }

    #[test]
    fn pcall_does_not_re_raise_user_forged_yield_sentinel() {
        // (frankenredis-sjuu1) pcall's sentinel re-raise must also
        // gate on pending_yield being set. A user script that wraps
        // a forged sentinel in pcall must see the normal
        // source-prefixed (false, sentinel_string) result, not have
        // pcall re-raise.
        let mut store = Store::new();
        let script = b"
            local ok, err = pcall(function()
                error('__frankenredis_lua_coroutine_yield__')
            end)
            return {tostring(ok), err}
        ";
        let result = eval_script(script, &[], &[], &mut store, 0);
        assert_eq!(
            result,
            Ok(RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"false".to_vec())),
                RespFrame::BulkString(Some(
                    b"user_script:1: __frankenredis_lua_coroutine_yield__".to_vec()
                )),
            ])))
        );
    }

    #[test]
    fn lua_error_default_level_includes_source_prefix() {
        // Redis/Lua error(msg) reports the current script source in
        // the error object. error(msg, 0) intentionally suppresses
        // that source prefix; pcall must expose the same distinction
        // instead of returning raw msg for every level.
        let mut store = Store::new();
        let default_err = eval_script(b"return error('boom')", &[], &[], &mut store, 0)
            .expect_err("default error level should raise");
        assert_eq!(default_err, "user_script:1: boom");

        let level_zero_err = eval_script(b"return error('boom', 0)", &[], &[], &mut store, 0)
            .expect_err("level 0 error should raise");
        assert_eq!(level_zero_err, "boom");
    }

    #[test]
    fn lua_pcall_exposes_error_level_source_prefix() {
        let mut store = Store::new();
        let default_script = b"local ok, err = pcall(function() error('boom') end); return err";
        let default_result = eval_script(default_script, &[], &[], &mut store, 0);
        assert_eq!(
            default_result,
            Ok(RespFrame::BulkString(Some(b"user_script:1: boom".to_vec())))
        );

        let level_zero_script =
            b"local ok, err = pcall(function() error('boom', 0) end); return err";
        let level_zero_result = eval_script(level_zero_script, &[], &[], &mut store, 0);
        assert_eq!(
            level_zero_result,
            Ok(RespFrame::BulkString(Some(b"boom".to_vec())))
        );
    }

    #[test]
    fn os_clock_reports_elapsed_script_time() {
        fn one_number(values: &[LuaValue]) -> Option<f64> {
            match values {
                [LuaValue::Number(value)] => Some(*value),
                _ => None,
            }
        }

        let mut store = Store::new();
        let mut state = LuaState::new(&mut store, 0);
        let mut env = Env::new();
        let mut no_args = Vec::new();
        let initial = state
            .call_builtin("os.clock", &mut no_args, &mut env)
            .unwrap_or_default();

        let mut accumulator = 0_u64;
        for value in 0..100_000 {
            accumulator = std::hint::black_box(accumulator.wrapping_add(value));
        }
        std::hint::black_box(accumulator);

        let later = state
            .call_builtin("os.clock", &mut no_args, &mut env)
            .unwrap_or_default();

        let first = one_number(initial.as_slice()).unwrap_or(f64::NAN);
        let second = one_number(later.as_slice()).unwrap_or(f64::NAN);
        assert!(first.is_finite(), "os.clock returned {initial:?}");
        assert!(second.is_finite(), "os.clock returned {later:?}");
        assert!(first >= 0.0);
        assert!(second > first);
    }

    #[test]
    fn setmetatable_and_getmetatable_work() {
        let mut store = Store::new();
        let script =
            b"local t = {}; local mt = {x=42}; setmetatable(t, mt); return getmetatable(t).x";
        let result = eval_script(script, &[], &[], &mut store, 0);
        assert_eq!(result, Ok(RespFrame::Integer(42)));
    }

    #[test]
    fn metatable_index_fallback() {
        let mut store = Store::new();
        let script = b"local base = {greeting = 'hello'}; local t = {}; setmetatable(t, {__index = base}); return t.greeting";
        let result = eval_script(script, &[], &[], &mut store, 0);
        assert_eq!(result, Ok(RespFrame::BulkString(Some(b"hello".to_vec()))));
    }

    #[test]
    fn metatable_index_chain() {
        let mut store = Store::new();
        let script = b"local a = {x=1}; local b = {}; setmetatable(b, {__index=a}); local c = {}; setmetatable(c, {__index=b}); return c.x";
        let result = eval_script(script, &[], &[], &mut store, 0);
        assert_eq!(result, Ok(RespFrame::Integer(1)));
    }

    #[test]
    fn metatable_index_does_not_override_existing() {
        let mut store = Store::new();
        let script =
            b"local base = {x=1}; local t = {x=2}; setmetatable(t, {__index=base}); return t.x";
        let result = eval_script(script, &[], &[], &mut store, 0);
        assert_eq!(result, Ok(RespFrame::Integer(2)));
    }

    #[test]
    fn setmetatable_nil_removes_metatable() {
        let mut store = Store::new();
        let script =
            b"local t = {}; setmetatable(t, {__index={x=1}}); setmetatable(t, nil); return t.x";
        let result = eval_script(script, &[], &[], &mut store, 0);
        assert_eq!(result, Ok(RespFrame::BulkString(None)));
    }

    #[test]
    fn string_rep_huge_returns_empty_for_empty_source() {
        // (frankenredis-jwkhc) Upstream Lua: string.rep('', N) is always
        // empty regardless of N, including math.huge / 2^31-1. The
        // overflow guard must short-circuit on s.len() == 0; previously
        // fr returned 'string length overflow' for huge n_val even when
        // the multiplication trivially yields 0.
        let mut store = Store::new();
        let script = b"return string.rep('', math.huge)";
        let r = eval_script(script, &[], &[], &mut store, 0).expect("empty rep huge");
        assert_eq!(r, RespFrame::BulkString(Some(Vec::new())));
    }

    #[test]
    fn math_random_does_not_panic_on_max_range() {
        let mut store = Store::new();
        // This should not panic with a modulo by zero.
        let script = b"return math.random(-9223372036854775808, 9223372036854775807)";
        let result = eval_script(script, &[], &[], &mut store, 0);
        assert!(result.is_ok());
    }

    #[test]
    fn redis_error_and_status_reply_reject_extra_arity_20ggg() {
        // (frankenredis-20ggg) Upstream luaRedisReturnSingleFieldTable
        // checks lua_gettop(lua) != 1 and emits "wrong number or type
        // of arguments". fr previously took args.first() and silently
        // dropped extras.
        let mut store = Store::new();
        for body in &[
            b"return redis.error_reply('a','b')".as_slice(),
            b"return redis.status_reply('a','b')".as_slice(),
            b"return redis.error_reply('a','b','c')".as_slice(),
            b"return redis.error_reply()".as_slice(),
            b"return redis.status_reply()".as_slice(),
        ] {
            let result = eval_script(body, &[], &[], &mut store, 0).expect("eval");
            let RespFrame::Error(msg) = result else {
                panic!(
                    "expected error for {:?}, got {result:?}",
                    String::from_utf8_lossy(body)
                );
            };
            assert!(
                msg.contains("wrong number or type of arguments"),
                "expected wrong-args wording for {:?}, got {msg}",
                String::from_utf8_lossy(body),
            );
        }

        // Sanity: exactly-one-string args still produce the table.
        let ok = eval_script(b"return redis.status_reply('OK')", &[], &[], &mut store, 0)
            .expect("status_reply ok");
        assert_eq!(ok, RespFrame::SimpleString("OK".to_string()));
    }

    #[test]
    fn redis_version_globals_match_upstream_luaver() {
        // (frankenredis-luaver) Upstream script_lua.c exposes
        // redis.REDIS_VERSION (string) and redis.REDIS_VERSION_NUM =
        // (major<<16)|(minor<<8)|patch. For the 7.2.4 compat target that is
        // 0x070204 = 459268. fr previously exposed neither (scripts saw nil).
        let mut store = Store::new();
        let ver = eval_script(b"return redis.REDIS_VERSION", &[], &[], &mut store, 0)
            .expect("REDIS_VERSION");
        assert_eq!(
            ver,
            RespFrame::BulkString(Some(fr_store::REDIS_COMPAT_VERSION.as_bytes().to_vec()))
        );
        let num = eval_script(b"return redis.REDIS_VERSION_NUM", &[], &[], &mut store, 0)
            .expect("REDIS_VERSION_NUM");
        assert_eq!(num, RespFrame::Integer(459_268));
        let ty = eval_script(
            b"return type(redis.REDIS_VERSION_NUM)",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("type");
        assert_eq!(ty, RespFrame::BulkString(Some(b"number".to_vec())));
    }

    #[test]
    fn redis_log_rejects_out_of_range_levels_20ggg() {
        // (frankenredis-20ggg) Upstream luaLogCommand bounds-checks
        // level against LL_DEBUG..LL_WARNING (0..=3) and emits
        // "Invalid debug level." via luaError, which propagates from
        // the RustFunction handler as a Rust Err.
        let mut store = Store::new();
        for body in &[
            b"redis.log(-1, 'msg') return 1".as_slice(),
            b"redis.log(4, 'msg') return 1".as_slice(),
            b"redis.log(99, 'msg') return 1".as_slice(),
            b"redis.log(0/0, 'msg') return 1".as_slice(), // NaN
        ] {
            let result = eval_script(body, &[], &[], &mut store, 0);
            let err = result.expect_err(&format!(
                "expected luaError for {:?}",
                String::from_utf8_lossy(body)
            ));
            assert!(
                err.contains("Invalid debug level."),
                "expected debug-level error for {:?}, got {err}",
                String::from_utf8_lossy(body),
            );
        }
        // In-range levels still accepted.
        for body in &[
            b"redis.log(0, 'msg') return 1".as_slice(),
            b"redis.log(3, 'msg') return 1".as_slice(),
            b"redis.log(redis.LOG_WARNING, 'msg') return 1".as_slice(),
        ] {
            let result = eval_script(body, &[], &[], &mut store, 0).expect("eval");
            assert_eq!(result, RespFrame::Integer(1));
        }
    }

    #[test]
    fn string_gsub_invalid_capture_index_127za() {
        // (frankenredis-127za) Upstream lstrlib.c::add_s raises
        // "invalid capture index" when the replacement string contains
        // %N where N exceeds the pattern's capture count. fr previously
        // silently skipped the reference, so gsub('abc', '(.)', '%5')
        // erased every byte instead of erroring.
        let mut store = Store::new();
        // 1-capture pattern, %5 referenced.
        let err = eval_script(
            b"return string.gsub('abc', '(.)', '%5')",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect_err("expected invalid capture");
        assert_eq!(err, "user_script:1: invalid capture index");

        // 2-capture pattern, %3 referenced.
        let err = eval_script(
            b"return string.gsub('a1', '(%a)(%d)', '%3')",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect_err("expected invalid capture");
        assert_eq!(err, "user_script:1: invalid capture index");

        // 0-capture pattern, %1: upstream special-cases this to mean
        // "whole match" (push_onecapture when i == 0 and level == 0).
        let r = eval_script(
            b"return string.gsub('abc', '.', '%1')",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("%1 with 0 captures = whole match");
        let s = match r {
            RespFrame::Array(Some(rows)) => match rows.first() {
                Some(RespFrame::BulkString(Some(bytes))) => bytes.clone(),
                _ => Vec::new(),
            },
            RespFrame::BulkString(Some(bytes)) => bytes,
            _ => Vec::new(),
        };
        assert_eq!(s, b"abc");

        // %0 (whole match) always valid even with no captures.
        let r = eval_script(
            b"return string.gsub('abc', '.', '<%0>')",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("valid %0");
        // gsub returns two values; first is the substituted string.
        match r {
            RespFrame::Array(Some(rows)) => match rows.first() {
                Some(RespFrame::BulkString(Some(bytes))) => {
                    assert_eq!(bytes, b"<a><b><c>");
                }
                other => panic!("expected bulk string, got {other:?}"),
            },
            RespFrame::BulkString(Some(bytes)) => assert_eq!(bytes, b"<a><b><c>"),
            other => panic!("unexpected reply shape: {other:?}"),
        }

        // %1 with a valid 1-capture pattern still works.
        let r = eval_script(
            b"return string.gsub('hello', '(l)', '<%1>')",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("valid %1");
        let s = match r {
            RespFrame::Array(Some(rows)) => match rows.first() {
                Some(RespFrame::BulkString(Some(bytes))) => bytes.clone(),
                _ => Vec::new(),
            },
            RespFrame::BulkString(Some(bytes)) => bytes,
            _ => Vec::new(),
        };
        assert_eq!(s, b"he<l><l>o");
    }

    #[test]
    fn math_min_max_wrong_type_arg_wording_a6r5p() {
        // (frankenredis-a6r5p) Upstream math_min/math_max iterate args
        // via luaL_checknumber, surfacing "bad argument #N to '<min|
        // max>' (number expected, got <type>)" with the user_script:1:
        // prefix. fr previously emitted a generic "bad argument to
        // 'math.min'" / "bad argument to 'math.max'" for any non-numeric
        // arg past index 0.
        let mut store = Store::new();
        for (body, fname, idx, ty) in &[
            (b"return math.min(1, 'a')".as_slice(), "min", 2, "string"),
            (b"return math.max(1, 'a')".as_slice(), "max", 2, "string"),
            (b"return math.min({}, 1)".as_slice(), "min", 1, "table"),
            (b"return math.max(nil, 1)".as_slice(), "max", 1, "nil"),
            (
                b"return math.min(1, 2, true)".as_slice(),
                "min",
                3,
                "boolean",
            ),
        ] {
            let err = eval_script(body, &[], &[], &mut store, 0).expect_err(&format!(
                "expected wrong-type error for {:?}",
                String::from_utf8_lossy(body)
            ));
            let expected = format!(
                "user_script:1: bad argument #{idx} to '{fname}' (number expected, got {ty})"
            );
            assert_eq!(
                err,
                expected,
                "wrong error for {:?}",
                String::from_utf8_lossy(body)
            );
        }

        // No-arg form continues to report arg #1 with "got no value".
        for fname in &["min", "max"] {
            let body = format!("return math.{fname}()");
            let err =
                eval_script(body.as_bytes(), &[], &[], &mut store, 0).expect_err("no-arg error");
            assert_eq!(
                err,
                format!(
                    "user_script:1: bad argument #1 to '{fname}' (number expected, got no value)"
                ),
            );
        }

        // Valid calls still work.
        let r =
            eval_script(b"return math.min(3, 1, 2)", &[], &[], &mut store, 0).expect("valid min");
        assert_eq!(r, RespFrame::Integer(1));
        let r =
            eval_script(b"return math.max(3, 1, 2)", &[], &[], &mut store, 0).expect("valid max");
        assert_eq!(r, RespFrame::Integer(3));
    }

    #[test]
    fn assert_and_xpcall_handler_error_parity_l4k9y() {
        // (frankenredis-l4k9y) Two distinct gaps from the rolling probe sweep:
        //   - assert prepends "user_script:1: " (via luaL_error in upstream).
        //   - xpcall returns "error in error handling" when the message
        //     handler itself raises, matching LUA_ERRERR from lua_pcall.
        let mut store = Store::new();

        // assert with custom message — wrapped via pcall.
        let r = eval_script(
            b"local ok,err=pcall(function() assert(false, 'failed') end) return tostring(ok)..':'..tostring(err)",
            &[], &[], &mut store, 0,
        ).expect("assert pcall");
        assert_eq!(
            r,
            RespFrame::BulkString(Some(b"false:user_script:1: failed".to_vec())),
        );

        // assert with no message defaults to "assertion failed!".
        let r = eval_script(
            b"local ok,err=pcall(function() assert(false) end) return tostring(ok)..':'..tostring(err)",
            &[], &[], &mut store, 0,
        ).expect("assert pcall no msg");
        assert_eq!(
            r,
            RespFrame::BulkString(Some(b"false:user_script:1: assertion failed!".to_vec())),
        );

        // assert with nil — same default message.
        let r = eval_script(
            b"local ok,err=pcall(function() assert(nil) end) return tostring(ok)..':'..tostring(err)",
            &[], &[], &mut store, 0,
        ).expect("assert pcall nil");
        assert_eq!(
            r,
            RespFrame::BulkString(Some(b"false:user_script:1: assertion failed!".to_vec())),
        );

        // assert(truthy) returns its args unchanged.
        let r = eval_script(b"return assert(42, 'msg')", &[], &[], &mut store, 0)
            .expect("assert truthy");
        // 42 is returned; multi-return collapses through eval_script.
        match r {
            RespFrame::Integer(n) => assert_eq!(n, 42),
            other => panic!("expected Integer(42), got {other:?}"),
        }

        // xpcall: when the message handler errors, vendored returns
        // "error in error handling" (LUA_ERRERR canonical message).
        let r = eval_script(
            b"local ok, err = xpcall(function() error('a') end, function() error('b') end) return tostring(ok)..':'..tostring(err)",
            &[], &[], &mut store, 0,
        ).expect("xpcall");
        assert_eq!(
            r,
            RespFrame::BulkString(Some(b"false:error in error handling".to_vec())),
        );

        // xpcall: when the handler succeeds, its return value is used.
        let r = eval_script(
            b"local ok, err = xpcall(function() error('boom') end, function(e) return 'handled:'..e end) return tostring(ok)..':'..tostring(err)",
            &[], &[], &mut store, 0,
        ).expect("xpcall handler ok");
        let RespFrame::BulkString(Some(bytes)) = r else {
            panic!("expected bulk string");
        };
        let s = String::from_utf8(bytes).unwrap();
        assert!(
            s.starts_with("false:handled:"),
            "expected handler result prefix, got {s}",
        );
    }

    #[test]
    fn lua_stdlib_distinguishes_no_value_from_nil_with_prefix_kqd16() {
        // (frankenredis-kqd16) Upstream luaL_argerror prepends
        // "user_script:1: " and distinguishes "got no value" (absent)
        // from "got nil" (explicit). fr previously dropped the prefix
        // on table-related builtins and collapsed both cases to nil.
        let mut store = Store::new();

        // No-arg cases — every builtin reports "got no value" with prefix.
        let no_arg: &[(&[u8], &str)] = &[
            (b"return ipairs()", "ipairs"),
            (b"return pairs()", "pairs"),
            (b"return next()", "next"),
            (b"return table.concat()", "concat"),
            (b"return table.insert()", "insert"),
            (b"return table.remove()", "remove"),
        ];
        for (body, fname) in no_arg {
            let err = eval_script(body, &[], &[], &mut store, 0).expect_err(&format!(
                "expected no-value error for {:?}",
                String::from_utf8_lossy(body)
            ));
            let expected = format!(
                "user_script:1: bad argument #1 to '{fname}' (table expected, got no value)"
            );
            assert_eq!(
                err,
                expected,
                "wrong error for {:?}",
                String::from_utf8_lossy(body)
            );
        }

        // Explicit-nil cases — same builtins report "got nil" with prefix.
        let nil_arg: &[(&[u8], &str)] = &[
            (b"return ipairs(nil)", "ipairs"),
            (b"return pairs(nil)", "pairs"),
            (b"return next(nil)", "next"),
            (b"return table.concat(nil)", "concat"),
            (b"return table.insert(nil)", "insert"),
            (b"return table.remove(nil)", "remove"),
        ];
        for (body, fname) in nil_arg {
            let err = eval_script(body, &[], &[], &mut store, 0).expect_err(&format!(
                "expected nil error for {:?}",
                String::from_utf8_lossy(body)
            ));
            let expected =
                format!("user_script:1: bad argument #1 to '{fname}' (table expected, got nil)");
            assert_eq!(
                err,
                expected,
                "wrong error for {:?}",
                String::from_utf8_lossy(body)
            );
        }

        // Wrong-type still reports the actual type with prefix.
        let err = eval_script(b"return ipairs(42)", &[], &[], &mut store, 0)
            .expect_err("expected type error");
        assert_eq!(
            err,
            "user_script:1: bad argument #1 to 'ipairs' (table expected, got number)",
        );
    }

    #[test]
    fn tonumber_base_edge_cases_5qv1n() {
        // (frankenredis-5qv1n) Upstream lbaselib.c::luaB_tonumber:
        //   - strtoul accepts "0x"/"0X" prefix when base is 16.
        //   - luaL_checkint truncates float base (10.5 -> 10).
        //   - Explicit nil base behaves like no base (default 10).
        //   - Out-of-range base errors carry user_script:1: prefix.
        let mut store = Store::new();

        // 0x prefix accepted in base 16.
        for body in &[
            b"return tonumber('0xff', 16)".as_slice(),
            b"return tonumber('0Xff', 16)".as_slice(),
            b"return tonumber('0xFF', 16)".as_slice(),
            b"return tonumber('ff', 16)".as_slice(),
            b"return tonumber('FF', 16)".as_slice(),
        ] {
            let r = eval_script(body, &[], &[], &mut store, 0).unwrap_or_else(|_| {
                panic!("expected number for {:?}", String::from_utf8_lossy(body))
            });
            assert_eq!(
                r,
                RespFrame::Integer(255),
                "wrong result for {:?}",
                String::from_utf8_lossy(body),
            );
        }

        // Negative hex string with explicit base wraps via strtoul.
        // Pinned in detail by frankenredis-8reid — round-trips to
        // INT64_MIN through lua_to_resp's cvttsd2si mimic.
        let r =
            eval_script(b"return tonumber('-ff', 16)", &[], &[], &mut store, 0).expect("neg hex");
        assert_eq!(r, RespFrame::Integer(i64::MIN));

        // Float base truncates to integer.
        let r = eval_script(b"return tonumber('10', 10.5)", &[], &[], &mut store, 0)
            .expect("float base");
        assert_eq!(r, RespFrame::Integer(10));

        // Explicit nil base defaults to no base (string-as-decimal).
        let r =
            eval_script(b"return tonumber('10', nil)", &[], &[], &mut store, 0).expect("nil base");
        assert_eq!(r, RespFrame::Integer(10));

        // Base out of range: 1, 37, -1, 0 all error with the prefix.
        for body in &[
            b"return tonumber('0', 1)".as_slice(),
            b"return tonumber('0', 37)".as_slice(),
            b"return tonumber('0', -1)".as_slice(),
            b"return tonumber('0', 0)".as_slice(),
        ] {
            let err = eval_script(body, &[], &[], &mut store, 0).expect_err(&format!(
                "expected base-out-of-range for {:?}",
                String::from_utf8_lossy(body)
            ));
            assert_eq!(
                err,
                "user_script:1: bad argument #2 to 'tonumber' (base out of range)",
                "wrong error for {:?}",
                String::from_utf8_lossy(body),
            );
        }
    }

    #[test]
    fn string_format_unmatched_pct_and_q_type_check_xpopu() {
        // (frankenredis-xpopu) Upstream string.format raises:
        //   - "bad argument #N to 'format' (no value)" when the format
        //     string ends with a bare `%`.
        //   - "bad argument #N to 'format' (string expected, got T)" for
        //     %q applied to nil/bool/table/function/etc. (numbers and
        //     strings continue to work — addquoted coerces numbers via
        //     luaL_checklstring/lua_tolstring).
        let mut store = Store::new();
        let pct_at_end = eval_script(b"return string.format('%')", &[], &[], &mut store, 0)
            .expect_err("expected no-value error");
        assert_eq!(
            pct_at_end,
            "user_script:1: bad argument #2 to 'format' (no value)",
        );

        // Trailing `%` after some content also catches.
        let pct_at_end2 = eval_script(b"return string.format('hello %')", &[], &[], &mut store, 0)
            .expect_err("expected no-value error");
        assert_eq!(
            pct_at_end2,
            "user_script:1: bad argument #2 to 'format' (no value)",
        );

        // %q with non-string args.
        for (body, ty) in &[
            (b"return string.format('%q', nil)".as_slice(), "nil"),
            (b"return string.format('%q', true)".as_slice(), "boolean"),
            (b"return string.format('%q', {})".as_slice(), "table"),
            (
                b"return string.format('%q', function() end)".as_slice(),
                "function",
            ),
        ] {
            let err = eval_script(body, &[], &[], &mut store, 0).expect_err(&format!(
                "expected type-error for {:?}",
                String::from_utf8_lossy(body)
            ));
            let expected =
                format!("user_script:1: bad argument #2 to 'format' (string expected, got {ty})");
            assert_eq!(
                err,
                expected,
                "wrong error for {:?}",
                String::from_utf8_lossy(body)
            );
        }

        // %q with strings and numbers still works.
        let r = eval_script(b"return string.format('%q', 'hi')", &[], &[], &mut store, 0)
            .expect("string %q ok");
        assert_eq!(r, RespFrame::BulkString(Some(b"\"hi\"".to_vec())));
        let r = eval_script(b"return string.format('%q', 42)", &[], &[], &mut store, 0)
            .expect("number %q ok");
        assert_eq!(r, RespFrame::BulkString(Some(b"\"42\"".to_vec())));

        // Escaped %% continues to render a literal %.
        let r = eval_script(b"return string.format('100%%')", &[], &[], &mut store, 0)
            .expect("literal %% ok");
        assert_eq!(r, RespFrame::BulkString(Some(b"100%".to_vec())));
    }

    #[test]
    fn lua_table_sort_comparator_and_default_compare_b2cmq() {
        // (frankenredis-b2cmq) Lua 5.1's table.sort validates the
        // comparator (must be callable) and propagates compare errors
        // for incomparable element pairs. Probed against vendored
        // Redis 7.2.4 on :16380.
        let mut store = Store::new();

        // Non-function comparator rejected with the luaL_argerror
        // wording (not the generic "attempt to call" message).
        let err = eval_script(
            b"local t={1,2,3}; table.sort(t, 'bad'); return t",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect_err("string comparator");
        assert_eq!(
            err,
            "user_script:1: bad argument #2 to 'sort' (function expected, got string)"
        );

        // Mixed-type sort propagates the underlying compare error
        // (no user_script:1 prefix — C-level error origin).
        let r = eval_script(
            b"local t={'a',{b=2}}; local ok,e=pcall(table.sort, t); return tostring(e)",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("mixed types");
        assert_eq!(
            r,
            RespFrame::BulkString(Some(b"attempt to compare table with string".to_vec()))
        );
        // Direct (not via pcall) — same bare wording.
        let err = eval_script(
            b"local t={'a',{b=2}}; table.sort(t); return t",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect_err("direct mixed");
        assert_eq!(err, "attempt to compare table with string");

        // Numbers + strings together still raise from the default
        // sort (not silently treated as equal).
        let r = eval_script(
            b"local t={1,'a',2}; local ok,e=pcall(table.sort, t); return tostring(e)",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("number+string mix");
        assert!(
            matches!(&r, RespFrame::BulkString(Some(b)) if String::from_utf8_lossy(b).contains("attempt to compare")),
            "expected compare-error, got {r:?}"
        );

        // Positive controls: pure numeric / pure string still sort.
        let r = eval_script(
            b"local t={3,1,2}; table.sort(t); return t[1]..t[2]..t[3]",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("numeric sort");
        assert_eq!(r, RespFrame::BulkString(Some(b"123".to_vec())));
        let r = eval_script(
            b"local t={'c','a','b'}; table.sort(t); return t[1]..t[2]..t[3]",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("string sort");
        assert_eq!(r, RespFrame::BulkString(Some(b"abc".to_vec())));

        // Custom callable still works.
        let r = eval_script(
            b"local t={3,1,2}; table.sort(t, function(a,b) return a>b end); return t[1]..t[2]..t[3]",
            &[], &[], &mut store, 0,
        ).expect("custom comparator");
        assert_eq!(r, RespFrame::BulkString(Some(b"321".to_vec())));

        // Pcall-indirect variant uses '?' name.
        let r = eval_script(
            b"local ok,e=pcall(table.sort, {1,2}, 'bad'); return tostring(e)",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("pcall sort bad cmp");
        assert_eq!(
            r,
            RespFrame::BulkString(Some(
                b"bad argument #2 to '?' (function expected, got string)".to_vec()
            ))
        );
    }

    #[test]
    fn lua_string_format_integer_precision_3pug0() {
        // (frankenredis-3pug0) C printf treats precision on integer
        // conversions as a minimum digit count (zero-padded); an
        // explicit precision suppresses the 0-flag for width padding.
        // Probed against vendored Redis 7.2.4 on :16380.
        let mut store = Store::new();
        let cases: &[(&[u8], &str)] = &[
            (b"return string.format('%10.5d', 42)", "     00042"),
            (b"return string.format('%5.3d', 42)", "  042"),
            (b"return string.format('%.5d', 42)", "00042"),
            (b"return string.format('%.0d', 0)", ""),
            (b"return string.format('%.0d', 5)", "5"),
            (b"return string.format('%05.3d', 42)", "  042"), // precision suppresses 0-flag
            (b"return string.format('%5d', 42)", "   42"),
            (b"return string.format('%d', 42)", "42"),
            (b"return string.format('%.4x', 255)", "00ff"),
            (b"return string.format('%.4X', 255)", "00FF"),
            (b"return string.format('%#.4x', 255)", "0x00ff"),
            (b"return string.format('%.6o', 8)", "000010"),
            (b"return string.format('%5.4d', -42)", "-0042"),
            (b"return string.format('%.5u', 42)", "00042"),
            (b"return string.format('%.0x', 0)", ""),
            (b"return string.format('%.0o', 0)", ""),
            (b"return string.format('%.0u', 0)", ""),
        ];
        for (body, expected) in cases {
            let r = eval_script(body, &[], &[], &mut store, 0)
                .unwrap_or_else(|e| panic!("eval {:?} failed: {e}", String::from_utf8_lossy(body)));
            assert_eq!(
                r,
                RespFrame::BulkString(Some(expected.as_bytes().to_vec())),
                "wrong output for {:?}",
                String::from_utf8_lossy(body),
            );
        }
    }

    #[test]
    fn lua_pattern_balanced_match_and_capture_validation_skwin() {
        // (frankenredis-skwin) %bxy balanced-match pattern + capture
        // balance validation. Probed against vendored Redis 7.2.4.
        let mut store = Store::new();

        // %b() matches a balanced parenthesized substring.
        let r = eval_script(
            b"return string.match('(a)(b)(c)', '%b()')",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("balanced ()");
        assert_eq!(r, RespFrame::BulkString(Some(b"(a)".to_vec())));

        // %b[] matches a balanced bracketed substring — and the inner
        // bracket-class parser must NOT consume the trailing ']' as part
        // of a set.
        let r = eval_script(
            b"return string.match('foo[bar]baz', '%b[]')",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("balanced []");
        assert_eq!(r, RespFrame::BulkString(Some(b"[bar]".to_vec())));

        // Degenerate: %bxy where open == close just matches a 2-char run.
        let r = eval_script(
            b"return string.match('xx', '%bxx')",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("degenerate balanced");
        assert_eq!(r, RespFrame::BulkString(Some(b"xx".to_vec())));

        // No balanced match → nil.
        let r = eval_script(
            b"return string.match('(unbal', '%b()')",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("unbalanced");
        assert_eq!(r, RespFrame::BulkString(None));

        // Nested balanced.
        let r = eval_script(
            b"return string.match('(a(b)c)d', '%b()')",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("nested balanced");
        assert_eq!(r, RespFrame::BulkString(Some(b"(a(b)c)".to_vec())));

        // Capture validation: unclosed '(' raises "unfinished capture".
        let err = eval_script(
            b"return string.match('abc', '(.*')",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect_err("unfinished");
        assert_eq!(err, "user_script:1: unfinished capture");

        // Extra ')' raises "invalid pattern capture".
        let err = eval_script(
            b"return string.match('abc', '.*)')",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect_err("invalid capture");
        assert_eq!(err, "user_script:1: invalid pattern capture");
    }

    #[test]
    fn lua_cjson_decodes_utf16_surrogate_pair_ljxmd() {
        // (frankenredis-ljxmd) Lua-bundled cjson combines UTF-16
        // surrogate pairs into single supplementary codepoints during
        // \u escape decoding. Probed against vendored Redis 7.2.4.
        //
        // (frankenredis-whyor) Source bytes must use `\\u` so the Lua
        // lexer sees `\u` as a literal backslash+u pair — vendored
        // Lua 5.1.5 drops unknown single-backslash escapes (`'\u'` →
        // `'u'`), so a single backslash in the Rust source bytes would
        // produce a JSON literal with no `\u` escapes for cjson to
        // decode. Doubling the backslash sends real `\u` to cjson.
        let mut store = Store::new();

        // U+1F600 (grinning face) encodes as the surrogate pair
        // D83D / DE00 — the resulting UTF-8 is the 4-byte sequence
        // F0 9F 98 80.
        let r = eval_script(
            b"return cjson.decode('\"\\\\uD83D\\\\uDE00\"')",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("surrogate pair");
        assert_eq!(r, RespFrame::BulkString(Some(vec![0xF0, 0x9F, 0x98, 0x80])));

        // Basic single-codepoint escapes still work.
        let r = eval_script(
            b"return cjson.decode('\"\\\\u0041\"')",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("ASCII u-escape");
        assert_eq!(r, RespFrame::BulkString(Some(b"A".to_vec())));

        // Another supplementary plane test: U+10000 → D800 DC00 → F0 90 80 80
        let r = eval_script(
            b"return cjson.decode('\"\\\\uD800\\\\uDC00\"')",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("U+10000");
        assert_eq!(r, RespFrame::BulkString(Some(vec![0xF0, 0x90, 0x80, 0x80])));
    }

    #[test]
    fn lua_cjson_sparse_array_and_decode_args_pt4d4() {
        // (frankenredis-pt4d4) Lua-bundled cjson encodes tables with
        // positive-integer keys as arrays (with null padding for gaps)
        // under default sparse-safe=10 / sparse-ratio=2 thresholds;
        // it raises on bool/table keys; decode arg-validates strictly.
        let mut store = Store::new();

        // Sparse array encoding (gaps → null).
        let r = eval_script(b"return cjson.encode({1,nil,3})", &[], &[], &mut store, 0)
            .expect("sparse encode 1,nil,3");
        assert_eq!(r, RespFrame::BulkString(Some(b"[1,null,3]".to_vec())));

        let r = eval_script(
            b"local x={1}; x[4]='gap'; return cjson.encode(x)",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("sparse encode trailing gap");
        assert_eq!(
            r,
            RespFrame::BulkString(Some(b"[1,null,null,\"gap\"]".to_vec()))
        );

        // Boolean key rejection.
        let err = eval_script(b"return cjson.encode({[true]=1})", &[], &[], &mut store, 0)
            .expect_err("bool key");
        assert!(
            err.contains("Cannot serialise boolean: table key must be a number or string"),
            "wrong: {err}"
        );

        // decode arg validation.
        let err = eval_script(b"return cjson.decode()", &[], &[], &mut store, 0)
            .expect_err("decode no args");
        assert!(
            err.contains("bad argument #1 to 'decode' (expected 1 argument)"),
            "wrong: {err}"
        );
        let err = eval_script(b"return cjson.decode(nil)", &[], &[], &mut store, 0)
            .expect_err("decode nil");
        assert!(
            err.contains("bad argument #1 to 'decode' (string expected, got nil)"),
            "wrong: {err}"
        );
        let err = eval_script(b"return cjson.decode('')", &[], &[], &mut store, 0)
            .expect_err("decode empty");
        assert!(
            err.contains("Expected value but found T_END at character 1"),
            "wrong: {err}"
        );

        // Regressions: normal cases still round-trip.
        let r = eval_script(b"return cjson.encode({1,2,3})", &[], &[], &mut store, 0)
            .expect("plain array");
        assert_eq!(r, RespFrame::BulkString(Some(b"[1,2,3]".to_vec())));
        let r = eval_script(b"return cjson.encode({a=1,b=2})", &[], &[], &mut store, 0)
            .expect("plain object");
        // Order is not guaranteed by HashMap iteration; just check it's
        // one of the two valid shapes.
        let bytes = match r {
            RespFrame::BulkString(Some(b)) => b,
            _ => panic!("expected bulk"),
        };
        assert!(
            bytes == b"{\"a\":1,\"b\":2}" || bytes == b"{\"b\":2,\"a\":1}",
            "got {:?}",
            String::from_utf8_lossy(&bytes),
        );
        // Empty table still encodes as object (cjson default).
        let r =
            eval_script(b"return cjson.encode({})", &[], &[], &mut store, 0).expect("empty table");
        assert_eq!(r, RespFrame::BulkString(Some(b"{}".to_vec())));
        // Sparse-ratio guard: max_int_key way larger than count →
        // upstream raises 'excessively sparse array' under its default
        // encode_sparse_convert=false setting.
        let err = eval_script(b"return cjson.encode({[100]='x'})", &[], &[], &mut store, 0)
            .expect_err("sparse rejected");
        assert!(
            err.contains("Cannot serialise table: excessively sparse array"),
            "wrong: {err}"
        );
    }

    #[test]
    fn lua_bit_library_parity_d3ovh() {
        // (frankenredis-d3ovh) LuaJIT bit.* error wording, arity checks,
        // and float→int rounding. Probed against vendored Redis 7.2.4.
        let mut store = Store::new();

        // Positive controls.
        for (body, expected) in &[
            (b"return bit.band(0xFF, 0x0F)".as_slice(), 15i64),
            (b"return bit.bor(0xF0, 0x0F)", 255),
            (b"return bit.bxor(0xFF, 0x55)", 170),
            (b"return bit.lshift(1, 4)", 16),
            (b"return bit.rshift(0x10, 4)", 1),
            (b"return bit.bnot(0)", -1),
            // Banker's rounding: tobit(1.5) → 2 (round to even),
            // band(2, 1) = 0. fr previously returned 1.
            (b"return bit.band(1.5, 1)", 0),
            (b"return bit.tobit(1.5)", 2),
            (b"return bit.tobit(2.5)", 2),
            (b"return bit.tobit(-1.5)", -2),
        ] {
            let r = eval_script(body, &[], &[], &mut store, 0)
                .unwrap_or_else(|e| panic!("eval {:?} failed: {e}", String::from_utf8_lossy(body)));
            assert_eq!(
                r,
                RespFrame::Integer(*expected),
                "wrong value for {:?}",
                String::from_utf8_lossy(body),
            );
        }

        // Missing-arg / type errors with upstream wording.
        let err =
            eval_script(b"return bit.band()", &[], &[], &mut store, 0).expect_err("band() no args");
        assert!(
            err.contains("bad argument #1 to 'band' (number expected, got no value)"),
            "wrong: {err}"
        );
        let err = eval_script(b"return bit.lshift()", &[], &[], &mut store, 0)
            .expect_err("lshift() no args");
        assert!(
            err.contains("bad argument #1 to 'lshift' (number expected, got no value)"),
            "wrong: {err}"
        );
        let err = eval_script(b"return bit.lshift(1)", &[], &[], &mut store, 0)
            .expect_err("lshift(1) missing #2");
        assert!(
            err.contains("bad argument #2 to 'lshift' (number expected, got no value)"),
            "wrong: {err}"
        );
        let err = eval_script(b"return bit.band(nil, 1)", &[], &[], &mut store, 0)
            .expect_err("band(nil, 1)");
        assert!(
            err.contains("bad argument #1 to 'band' (number expected, got nil)"),
            "wrong: {err}"
        );
        let err = eval_script(b"return bit.band({}, 1)", &[], &[], &mut store, 0)
            .expect_err("band({}, 1)");
        assert!(
            err.contains("bad argument #1 to 'band' (number expected, got table)"),
            "wrong: {err}"
        );
        let err = eval_script(b"return bit.tohex('abc')", &[], &[], &mut store, 0)
            .expect_err("tohex('abc')");
        assert!(
            err.contains("bad argument #1 to 'tohex' (number expected, got string)"),
            "wrong: {err}"
        );

        // Indirect pcall drops prefix and uses '?' name.
        let r = eval_script(
            b"local ok,e=pcall(bit.band); return tostring(e)",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("pcall(bit.band)");
        assert_eq!(
            r,
            RespFrame::BulkString(Some(
                b"bad argument #1 to '?' (number expected, got no value)".to_vec()
            ))
        );
    }

    #[test]
    fn lua_parser_errors_use_upstream_wording_i0h24() {
        // (frankenredis-i0h24) Lua 5.1's parser emits diagnostics shaped
        // as "X expected near Y" / "unexpected symbol near 'X'" / etc.
        // fr previously emitted Rust-debug-style wording. Probed against
        // vendored Redis 7.2.4 on :16380.
        let mut store = Store::new();
        let cases: &[(&[u8], &str)] = &[
            // self.expect() now produces upstream wording for all
            // keyword-expected and punctuation-expected paths.
            (b"function f end", "'(' expected near 'end'"),
            (b"if true do end", "'then' expected near 'do'"),
            (b"if true print('x') end", "'then' expected near 'print'"),
            (b"repeat end", "'until' expected near 'end'"),
            (b"do", "'end' expected near '<eof>'"),
            // name-expected paths use the '<name>' literal slot.
            (b"local nil = 1", "'<name>' expected near 'nil'"),
            (b"function 1() end", "'<name>' expected near '1'"),
            // unexpected symbol in expression-start.
            (b"if then end", "unexpected symbol near 'then'"),
            (b"while do end", "unexpected symbol near 'do'"),
            (b"for x= do", "unexpected symbol near 'do'"),
            (b"return 1+", "unexpected symbol near '<eof>'"),
            (b"(((", "unexpected symbol near '<eof>'"),
            (b"::label::", "unexpected symbol near ':'"),
            (b"[", "unexpected symbol near '['"),
            // Top-of-chunk "extra tokens" → '<eof>' expected near 'X'.
            (
                b"elseif x then return end",
                "'<eof>' expected near 'elseif'",
            ),
            (b"else return end", "'<eof>' expected near 'else'"),
            (b"end", "'<eof>' expected near 'end'"),
            // `break` outside a loop now raises.
            (b"break", "no loop to break near '<eof>'"),
        ];
        for (body, expected) in cases {
            let err =
                eval_script(body, &[], &[], &mut store, 0).expect_err("parser error expected");
            assert!(
                err.contains(expected),
                "wrong error for {:?}: got {err:?}",
                String::from_utf8_lossy(body),
            );
        }

        // Regression: break inside a loop still parses.
        let r = eval_script(
            b"for i=1,3 do if i==2 then break end end; return 'ok'",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("break-in-loop");
        assert_eq!(r, RespFrame::BulkString(Some(b"ok".to_vec())));
    }

    #[test]
    fn lua_tonumber_numeric_arg_with_base_s99a4() {
        // (frankenredis-s99a4) Upstream luaB_tonumber routes arg 1
        // through luaL_checkstring when an explicit base is given —
        // numbers get coerced to their printed form and the result is
        // strtoul-parsed in that base. fr was short-circuiting on
        // LuaValue::Number and returning the number unchanged.
        let mut store = Store::new();

        // Base 3: "5" is not a valid base-3 digit, parser fails → nil.
        let r =
            eval_script(b"return tonumber(5, 3)", &[], &[], &mut store, 0).expect("tonumber(5,3)");
        assert_eq!(r, RespFrame::BulkString(None));

        // Base 2: "7" is not a valid base-2 digit → nil.
        let r =
            eval_script(b"return tonumber(7, 2)", &[], &[], &mut store, 0).expect("tonumber(7,2)");
        assert_eq!(r, RespFrame::BulkString(None));

        // Base 16: "15" parses as 0x15 = 21.
        let r = eval_script(b"return tonumber(15, 16)", &[], &[], &mut store, 0)
            .expect("tonumber(15,16)");
        assert_eq!(r, RespFrame::Integer(21));

        // No base: number passes through unchanged.
        let r = eval_script(b"return tonumber(15)", &[], &[], &mut store, 0).expect("tonumber(15)");
        assert_eq!(r, RespFrame::Integer(15));

        // Nil base behaves like no base.
        let r = eval_script(b"return tonumber(15, nil)", &[], &[], &mut store, 0)
            .expect("tonumber(15, nil)");
        assert_eq!(r, RespFrame::Integer(15));

        // Float base 3.7 → truncated to 3, then "5" fails base-3 parse.
        let r = eval_script(b"return tonumber(5, 3.7)", &[], &[], &mut store, 0)
            .expect("tonumber(5, 3.7)");
        assert_eq!(r, RespFrame::BulkString(None));

        // String path unchanged.
        let r = eval_script(b"return tonumber('15', 16)", &[], &[], &mut store, 0)
            .expect("tonumber('15',16)");
        assert_eq!(r, RespFrame::Integer(21));
    }

    #[test]
    fn lua_assert_tonumber_select_indirect_pcall_j438k() {
        // (frankenredis-j438k) Three builtins were missed by the broader
        // i18ug refactor — assert raised via hand-rolled luaL_error
        // string with unconditional prefix, tonumber hand-rolled its
        // luaL_argerror messages, and select used type_name() instead of
        // lua_arg_got_label() for its missing-arg case. Probed against
        // vendored Redis 7.2.4 on :16380.
        let mut store = Store::new();
        let cases: &[(&[u8], &str)] = &[
            // assert via pcall: no prefix.
            (
                b"local ok,e=pcall(assert, false); return tostring(e)",
                "assertion failed!",
            ),
            (
                b"local ok,e=pcall(assert, false, 'msg'); return tostring(e)",
                "msg",
            ),
            (
                b"local ok,e=pcall(assert, nil); return tostring(e)",
                "assertion failed!",
            ),
            (
                b"local ok,e=pcall(assert, nil, 'x'); return tostring(e)",
                "x",
            ),
            // assert called from inside a Lua function: prefix added.
            (
                b"local ok,e=pcall(function() assert(false) end); return tostring(e)",
                "user_script:1: assertion failed!",
            ),
            // tonumber via pcall: '?' name, no prefix.
            (
                b"local ok,e=pcall(tonumber); return tostring(e)",
                "bad argument #1 to '?' (value expected)",
            ),
            // tonumber direct: 'tonumber' name with prefix.
            (
                b"local ok,e=pcall(function() return tonumber() end); return tostring(e)",
                "user_script:1: bad argument #1 to 'tonumber' (value expected)",
            ),
            // select with no args reports 'got no value' not 'got nil'.
            (
                b"local ok,e=pcall(function() return select() end); return tostring(e)",
                "user_script:1: bad argument #1 to 'select' (number expected, got no value)",
            ),
        ];
        for (body, expected) in cases {
            let r = eval_script(body, &[], &[], &mut store, 0)
                .unwrap_or_else(|e| panic!("eval {:?} failed: {e}", String::from_utf8_lossy(body)));
            assert_eq!(
                r,
                RespFrame::BulkString(Some(expected.as_bytes().to_vec())),
                "wrong output for {:?}",
                String::from_utf8_lossy(body),
            );
        }
    }

    #[test]
    fn lua_builtin_argerror_drops_prefix_for_indirect_pcall_i18ug() {
        // (frankenredis-i18ug) When a C builtin is invoked indirectly via
        // pcall(fn, args), Lua 5.1's lua_getinfo("n.name") returns NULL
        // for the C closure and luaL_where(L,1) returns the empty string.
        // The resulting luaL_argerror reads "bad argument #N to '?' (...)"
        // with no user_script:1: prefix. fr's bad-arg helpers were
        // hardcoding the builtin's internal name and always prefixing.
        // Probed against vendored Redis 7.2.4 on :16380.
        let mut store = Store::new();
        let cases: &[(&[u8], &str)] = &[
            (
                b"local ok,e=pcall(ipairs, nil); return tostring(e)",
                "bad argument #1 to '?' (table expected, got nil)",
            ),
            (
                b"local ok,e=pcall(pairs, nil); return tostring(e)",
                "bad argument #1 to '?' (table expected, got nil)",
            ),
            (
                b"local ok,e=pcall(next, nil); return tostring(e)",
                "bad argument #1 to '?' (table expected, got nil)",
            ),
            (
                b"local ok,e=pcall(table.insert, nil); return tostring(e)",
                "bad argument #1 to '?' (table expected, got nil)",
            ),
            (
                b"local ok,e=pcall(table.remove, nil); return tostring(e)",
                "bad argument #1 to '?' (table expected, got nil)",
            ),
            (
                b"local ok,e=pcall(unpack, nil); return tostring(e)",
                "bad argument #1 to '?' (table expected, got nil)",
            ),
            (
                b"local ok,e=pcall(rawset, nil); return tostring(e)",
                "bad argument #1 to '?' (table expected, got nil)",
            ),
            (
                b"local ok,e=pcall(rawequal); return tostring(e)",
                "bad argument #1 to '?' (value expected)",
            ),
            (
                b"local ok,e=pcall(math.floor, 'abc'); return tostring(e)",
                "bad argument #1 to '?' (number expected, got string)",
            ),
            (
                b"local ok,e=pcall(math.abs, 'abc'); return tostring(e)",
                "bad argument #1 to '?' (number expected, got string)",
            ),
        ];
        for (body, expected) in cases {
            let r = eval_script(body, &[], &[], &mut store, 0)
                .unwrap_or_else(|e| panic!("eval {:?} failed: {e}", String::from_utf8_lossy(body)));
            assert_eq!(
                r,
                RespFrame::BulkString(Some(expected.as_bytes().to_vec())),
                "wrong output for {:?}",
                String::from_utf8_lossy(body),
            );
        }

        // Regression: direct AST call still uses the prefix and the
        // call-site name (which equals the internal name for unaliased
        // calls).
        let err =
            eval_script(b"return ipairs(nil)", &[], &[], &mut store, 0).expect_err("direct ipairs");
        assert_eq!(
            err,
            "user_script:1: bad argument #1 to 'ipairs' (table expected, got nil)"
        );
    }

    #[test]
    fn lua_numeric_for_loop_error_wording_7vqyo() {
        // (frankenredis-7vqyo) Lua 5.1's luaV_execute raises numeric-for
        // type errors via luaG_runerror which prepends 'user_script:1: '
        // and names the initial slot "initial value", not "start".
        // Probed against vendored Redis 7.2.4 on :16380.
        let mut store = Store::new();
        let cases: &[(&[u8], &str)] = &[
            (
                b"for i='a','b' do end",
                "user_script:1: 'for' initial value must be a number",
            ),
            (
                b"for i=1,'b' do end",
                "user_script:1: 'for' limit must be a number",
            ),
            (
                b"for i=1,3,'abc' do end",
                "user_script:1: 'for' step must be a number",
            ),
        ];
        for (body, expected) in cases {
            let err = eval_script(body, &[], &[], &mut store, 0).expect_err("for-loop type error");
            assert_eq!(
                err,
                *expected,
                "wrong error for {:?}",
                String::from_utf8_lossy(body),
            );
        }
    }

    #[test]
    fn lua_attempt_to_x_errors_include_accessor_label_9ckvq() {
        // (frankenredis-9ckvq) Lua 5.1's lvm.c::luaG_typeerror reports
        // the variable name of the offending operand alongside the type.
        // The Concat and Call paths in fr already do this; verify Index
        // (Field and Index forms), unary -, unary #, and binary
        // arithmetic now do too. Probed against vendored Redis 7.2.4.
        let mut store = Store::new();

        let cases: &[(&[u8], &str)] = &[
            (
                b"local t=nil; return t.field",
                "user_script:1: attempt to index local 't' (a nil value)",
            ),
            (
                b"local t=nil; return t[1]",
                "user_script:1: attempt to index local 't' (a nil value)",
            ),
            (
                b"local mylocal=nil; return mylocal.f",
                "user_script:1: attempt to index local 'mylocal' (a nil value)",
            ),
            (
                b"local t={a=nil}; return t.a.b",
                "user_script:1: attempt to index field 'a' (a nil value)",
            ),
            (
                b"local t={}; return t.missing.deep",
                "user_script:1: attempt to index field 'missing' (a nil value)",
            ),
            (
                b"local b=true; return b.f",
                "user_script:1: attempt to index local 'b' (a boolean value)",
            ),
            (
                b"local nm=5; return nm.f",
                "user_script:1: attempt to index local 'nm' (a number value)",
            ),
            (
                b"local x=nil; return x+1",
                "user_script:1: attempt to perform arithmetic on local 'x' (a nil value)",
            ),
            (
                b"local y=nil; return 1+y",
                "user_script:1: attempt to perform arithmetic on local 'y' (a nil value)",
            ),
            (
                b"local z=nil; return -z",
                "user_script:1: attempt to perform arithmetic on local 'z' (a nil value)",
            ),
            (
                b"local s=nil; return #s",
                "user_script:1: attempt to get length of local 's' (a nil value)",
            ),
            (
                b"local t={}; return #t.missing",
                "user_script:1: attempt to get length of field 'missing' (a nil value)",
            ),
            (
                b"local t={}; return t+1",
                "user_script:1: attempt to perform arithmetic on local 't' (a table value)",
            ),
            (
                b"local n='abc'; return n+1",
                "user_script:1: attempt to perform arithmetic on local 'n' (a string value)",
            ),
        ];
        for (body, expected) in cases {
            let err = eval_script(body, &[], &[], &mut store, 0).expect_err("error expected");
            assert_eq!(
                err,
                *expected,
                "wrong error for {:?}",
                String::from_utf8_lossy(body),
            );
        }

        // Regression: no syntactic label available → fall back to the
        // unlabeled wording (vendored does the same when the operand has
        // no resolvable variable, e.g. a function-call result).
        let err = eval_script(
            b"return (function() return nil end)() + 1",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect_err("no label");
        assert_eq!(
            err,
            "user_script:1: attempt to perform arithmetic on a nil value"
        );
    }

    #[test]
    fn lua_metatable_protection_and_setmetatable_arg_check_fnh42() {
        // (frankenredis-fnh42) Lua 5.1's luaB_setmetatable and
        // luaB_getmetatable honor the __metatable field of an existing
        // metatable. Probed against vendored Redis 7.2.4 on :16380.
        let mut store = Store::new();

        // getmetatable returns __metatable when present.
        let r = eval_script(
            b"return getmetatable(setmetatable({}, {__metatable='locked'}))",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("getmetatable masked");
        assert_eq!(r, RespFrame::BulkString(Some(b"locked".to_vec())));

        // getmetatable still returns the real metatable when
        // __metatable is absent (existing behavior must not regress).
        let r = eval_script(
            b"local mt={}; setmetatable({}, mt); return type(getmetatable(setmetatable({},mt)))",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("getmetatable plain");
        assert_eq!(r, RespFrame::BulkString(Some(b"table".to_vec())));

        // setmetatable on a protected table errors. Called directly
        // via pcall, the where(1) prefix is empty (C frame).
        let r = eval_script(
            b"local t=setmetatable({},{__metatable='x'}); local ok,e=pcall(setmetatable, t, {}); return tostring(e)",
            &[], &[], &mut store, 0,
        ).expect("setmetatable protected direct");
        assert_eq!(
            r,
            RespFrame::BulkString(Some(b"cannot change a protected metatable".to_vec()))
        );

        // Same error from inside a Lua function — prefix added.
        let r = eval_script(
            b"local t=setmetatable({},{__metatable='x'}); local ok,e=pcall(function() setmetatable(t, {}) end); return tostring(e)",
            &[], &[], &mut store, 0,
        ).expect("setmetatable protected wrapped");
        assert_eq!(
            r,
            RespFrame::BulkString(Some(
                b"user_script:1: cannot change a protected metatable".to_vec()
            ))
        );

        // setmetatable({}) with only one arg raises "nil or table
        // expected" rather than silently treating arg #2 as nil.
        let r = eval_script(
            b"local ok,e=pcall(setmetatable, {}); return tostring(e)",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("setmetatable arity");
        assert_eq!(
            r,
            RespFrame::BulkString(Some(
                b"bad argument #2 to '?' (nil or table expected)".to_vec()
            ))
        );

        // Explicit nil still clears the metatable (regression guard).
        let r = eval_script(
            b"local t=setmetatable({},{__index=function() return 1 end}); setmetatable(t, nil); return tostring(getmetatable(t))",
            &[], &[], &mut store, 0,
        ).expect("setmetatable nil clears");
        assert_eq!(r, RespFrame::BulkString(Some(b"nil".to_vec())));
    }

    #[test]
    fn lua_error_where_prefix_respects_lua_vs_c_frames_0k259() {
        // (frankenredis-0k259) Lua 5.1's luaB_error only prepends the
        // luaL_where(L, level) source-location when the requested level
        // is a Lua-function frame. C frames (pcall/xpcall/error itself)
        // produce an empty where-prefix. fr previously assumed level==1
        // always meant 'user_script:1' regardless of which kind of frame
        // it pointed to; for direct invocations like `pcall(error, X)`
        // that yields the wrong prefix.
        let mut store = Store::new();

        // pcall(error, 'msg'): level=1 default → caller is pcall (C) →
        // no prefix. fr previously incorrectly emitted user_script:1: msg.
        let r = eval_script(
            b"local ok,err=pcall(error, 'msg'); return type(err)..':'..tostring(err)",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("pcall(error,'msg')");
        assert_eq!(r, RespFrame::BulkString(Some(b"string:msg".to_vec())));

        // pcall(error, 42): same — number coerced to string, no prefix.
        let r = eval_script(
            b"local ok,err=pcall(error, 42); return type(err)..':'..tostring(err)",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("pcall(error,42)");
        assert_eq!(r, RespFrame::BulkString(Some(b"string:42".to_vec())));

        // pcall(error, 'msg', 2): level 2 walks past pcall (C) to the
        // script chunk (Lua) → prefix added.
        let r = eval_script(
            b"local ok,err=pcall(error, 'msg', 2); return type(err)..':'..tostring(err)",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("pcall(error,'msg',2)");
        assert_eq!(
            r,
            RespFrame::BulkString(Some(b"string:user_script:1: msg".to_vec()))
        );

        // error('msg') inside a Lua function: prefix added (Lua frame
        // at level 1).
        let r = eval_script(
            b"local ok,err=pcall(function() error('msg') end); return type(err)..':'..tostring(err)",
            &[], &[], &mut store, 0,
        ).expect("error('msg') from fn");
        assert_eq!(
            r,
            RespFrame::BulkString(Some(b"string:user_script:1: msg".to_vec()))
        );

        // error(42, 0) inside a Lua function: level 0 = no prefix,
        // value retains its number type.
        let r = eval_script(
            b"local ok,err=pcall(function() error(42, 0) end); return type(err)..':'..tostring(err)",
            &[], &[], &mut store, 0,
        ).expect("error(42,0) from fn");
        assert_eq!(r, RespFrame::BulkString(Some(b"number:42".to_vec())));

        // error('msg', 0): level 0 = no prefix; string passes through
        // unchanged.
        let r = eval_script(
            b"local ok,err=pcall(function() error('msg', 0) end); return type(err)..':'..tostring(err)",
            &[], &[], &mut store, 0,
        ).expect("error('msg',0)");
        assert_eq!(r, RespFrame::BulkString(Some(b"string:msg".to_vec())));

        // Coroutine body is a Lua frame at level 1 — prefix added.
        let r = eval_script(
            b"local co=coroutine.create(function() error('x') end); local ok,err=coroutine.resume(co); return type(err)..':'..tostring(err)",
            &[], &[], &mut store, 0,
        ).expect("coroutine error");
        assert_eq!(
            r,
            RespFrame::BulkString(Some(b"string:user_script:1: x".to_vec()))
        );
    }

    #[test]
    fn lua_table_rejects_nil_and_nan_keys_tb9vb() {
        // (frankenredis-tb9vb) Lua 5.1's luaV_settable raises at runtime
        // when an assignment uses a nil or NaN key. Positive/negative
        // infinity are valid keys (NaN is rejected because NaN != NaN
        // breaks the table's internal equality). fr previously dropped
        // these silently. Probed against vendored Redis 7.2.4 on :16380.
        let mut store = Store::new();

        // Direct t[nil]=1 syntax raises before storing.
        let err =
            eval_script(b"local t={} t[nil]=1", &[], &[], &mut store, 0).expect_err("nil key");
        assert_eq!(err, "user_script:1: table index is nil");

        // Direct t[0/0]=1 syntax raises with NaN message.
        let err =
            eval_script(b"local t={} t[0/0]=1", &[], &[], &mut store, 0).expect_err("NaN key");
        assert_eq!(err, "user_script:1: table index is NaN");

        // -NaN must also be rejected (sign bit doesn't help).
        let err =
            eval_script(b"local t={} t[-(0/0)]=1", &[], &[], &mut store, 0).expect_err("-NaN key");
        assert_eq!(err, "user_script:1: table index is NaN");

        // Table constructor {[nil]=1} raises at construction time.
        let err =
            eval_script(b"return {[nil]=1}", &[], &[], &mut store, 0).expect_err("ctor nil key");
        assert_eq!(err, "user_script:1: table index is nil");

        // Table constructor {[0/0]=1} raises at construction time.
        let err =
            eval_script(b"return {[0/0]=1}", &[], &[], &mut store, 0).expect_err("ctor NaN key");
        assert_eq!(err, "user_script:1: table index is NaN");

        // Positive infinity is a valid key — must NOT raise.
        let r = eval_script(
            b"local t={} t[1/0]=42 return t[1/0]",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("inf key ok");
        assert_eq!(r, RespFrame::Integer(42));

        // Negative infinity is a valid key — must NOT raise.
        let r = eval_script(
            b"local t={} t[-1/0]=42 return t[-1/0]",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("-inf key ok");
        assert_eq!(r, RespFrame::Integer(42));
    }

    #[test]
    fn lua_pattern_frontier_matcher_3zxc1() {
        // (frankenredis-3zxc1) Lua 5.1's pattern engine supports %f[set]
        // as a zero-width assertion: the previous byte does NOT match
        // [set] but the current byte DOES (with '\0' as the sentinel
        // before position 0 and past end-of-string). fr previously
        // returned no-match for any pattern containing %f.
        //
        // Cases below were probed live against vendored Redis 7.2.4
        // on :16380 and pin both the matching behavior and the
        // upstream malformed-pattern wording.
        let mut store = Store::new();

        // Zero-width match at start: 'T' is %a, "before pos 0" is \0.
        let r = eval_script(
            b"return string.find('THE (QUICK)', '%f[%a]')",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("frontier find");
        assert_eq!(r, RespFrame::Integer(1));

        // Multiple matches via gsub at every word boundary.
        let r = eval_script(
            b"return string.gsub('THE QUICK BROWN', '%f[%a]', '|')",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("frontier gsub many");
        // Returns (result, count) — the script returns the string only,
        // which is the first multi-return value.
        if let RespFrame::BulkString(Some(bytes)) = r {
            assert_eq!(bytes, b"|THE |QUICK |BROWN");
        } else {
            panic!("expected bulk string, got {r:?}");
        }

        let r = eval_script(
            b"return string.gsub('a b c', '%f[%a]', '!')",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("frontier gsub a b c");
        assert_eq!(r, RespFrame::BulkString(Some(b"!a !b !c".to_vec())));

        // match returns the captured word at the first word boundary.
        let r = eval_script(
            b"return string.match('Hello World', '%f[%w]%w+')",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("frontier match");
        assert_eq!(r, RespFrame::BulkString(Some(b"Hello".to_vec())));

        // Anchored frontier still works.
        let r = eval_script(
            b"return string.find('abc', '^%f[%a]')",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("anchored frontier");
        assert_eq!(r, RespFrame::Integer(1));

        // Malformed: %f without trailing [set] must raise the upstream
        // message verbatim.
        for body in [
            b"return string.find('hello', '%f')".as_slice(),
            b"return string.find('hello', '%fx')".as_slice(),
            b"return string.gsub('hello', '%f', 'X')".as_slice(),
            b"return string.match('hello', '%f')".as_slice(),
        ] {
            let err = eval_script(body, &[], &[], &mut store, 0)
                .expect_err("expected missing-bracket error");
            assert_eq!(
                err,
                "user_script:1: missing '[' after '%f' in pattern",
                "wrong error for {:?}",
                String::from_utf8_lossy(body),
            );
        }
    }

    #[test]
    fn string_format_inf_nan_and_neg_unsigned_t1ah8() {
        // (frankenredis-t1ah8) Upstream string.format relies on C printf
        // which has well-defined output for non-finite floats and for
        // negative inputs to unsigned conversions. fr previously emitted
        // garbage like "NaNe+2147483647" for inf, "NaN" instead of "-nan",
        // and 0 / -1 for %x/%u on negative inputs. Pin the upstream
        // wording (probed against vendored Redis 7.2.4 on :16380).
        let mut store = Store::new();
        let cases: &[(&[u8], &str)] = &[
            // Positive infinity → lowercase "inf" for lowercase specs,
            // "INF" for uppercase. %f already worked; cover all four.
            (b"return string.format('%e', 1/0)", "inf"),
            (b"return string.format('%E', 1/0)", "INF"),
            (b"return string.format('%g', 1/0)", "inf"),
            (b"return string.format('%G', 1/0)", "INF"),
            (b"return string.format('%f', 1/0)", "inf"),
            // Negative infinity.
            (b"return string.format('%e', -1/0)", "-inf"),
            (b"return string.format('%g', -1/0)", "-inf"),
            (b"return string.format('%f', -1/0)", "-inf"),
            // NaN: 0/0 produces a sign-negative NaN on x86-64, matching
            // glibc's "-nan" / "-NAN" output.
            (b"return string.format('%e', 0/0)", "-nan"),
            (b"return string.format('%g', 0/0)", "-nan"),
            (b"return string.format('%f', 0/0)", "-nan"),
            (b"return string.format('%E', 0/0)", "-NAN"),
            (b"return string.format('%G', 0/0)", "-NAN"),
            // Negative integer to %x/%X/%o/%u: must recover the unsigned
            // two's complement bit pattern.
            (b"return string.format('%x', -1)", "ffffffffffffffff"),
            (b"return string.format('%x', -255)", "ffffffffffffff01"),
            (b"return string.format('%X', -1)", "FFFFFFFFFFFFFFFF"),
            (b"return string.format('%o', -1)", "1777777777777777777777"),
            (b"return string.format('%u', -1)", "18446744073709551615"),
            (b"return string.format('%u', -42)", "18446744073709551574"),
            // Finite values must still round-trip correctly.
            (b"return string.format('%e', 1.5)", "1.500000e+00"),
            (b"return string.format('%g', 1.5)", "1.5"),
            (b"return string.format('%x', 255)", "ff"),
            (b"return string.format('%u', 42)", "42"),
            (b"return string.format('%d', 42)", "42"),
        ];
        for (body, expected) in cases {
            let r = eval_script(body, &[], &[], &mut store, 0)
                .unwrap_or_else(|e| panic!("eval {:?} failed: {e}", String::from_utf8_lossy(body)));
            assert_eq!(
                r,
                RespFrame::BulkString(Some(expected.as_bytes().to_vec())),
                "wrong output for {:?}",
                String::from_utf8_lossy(body),
            );
        }
    }

    #[test]
    fn lua_pattern_validate_catches_malformed_patterns_vfv8s() {
        // (frankenredis-vfv8s) Upstream lstrlib.c raises two classes of
        // pattern-malformed errors at match time. fr previously silently
        // returned no-match (or, for gsub, the unchanged source string)
        // for both. Pin the upstream wording for find/match/gmatch/gsub.
        let mut store = Store::new();
        let trailing_pct: &[&[u8]] = &[
            b"return string.find('hello', '%')",
            b"return string.match('hello', '%')",
            b"for w in string.gmatch('hello', '%') do end return 1",
            b"return string.gsub('hello', '%', 'X')",
        ];
        for body in trailing_pct {
            let err = eval_script(body, &[], &[], &mut store, 0).expect_err(&format!(
                "expected malformed-pattern error for {:?}",
                String::from_utf8_lossy(body)
            ));
            assert_eq!(
                err,
                "user_script:1: malformed pattern (ends with '%')",
                "wrong error for {:?}",
                String::from_utf8_lossy(body),
            );
        }
        let missing_bracket: &[&[u8]] = &[
            b"return string.find('hello', '[abc')",
            b"return string.match('hello', '[abc')",
            b"for w in string.gmatch('hello', '[abc') do end return 1",
            b"return string.gsub('hello', '[abc', 'X')",
        ];
        for body in missing_bracket {
            let err = eval_script(body, &[], &[], &mut store, 0).expect_err(&format!(
                "expected missing-] error for {:?}",
                String::from_utf8_lossy(body)
            ));
            assert_eq!(
                err,
                "user_script:1: malformed pattern (missing ']')",
                "wrong error for {:?}",
                String::from_utf8_lossy(body),
            );
        }
        // Plain mode of string.find is exempt — pattern bytes are literal.
        // The exact reply shape after Lua's multi-return collapses through
        // the RESP wrapper is non-trivial; only assert that the call
        // doesn't raise the pattern-malformed error.
        let _ = eval_script(
            b"return string.find('a%b', '%', 1, true)",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("plain find must not validate pattern");

        // Valid patterns continue to match without error.
        let _ = eval_script(
            b"return string.find('hello123', '%d+')",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("valid pattern must match");
        let _ = eval_script(
            b"return string.gsub('hello', '(l)', '<%1>')",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("valid gsub must match");
    }

    #[test]
    fn lua_string_format_flag_chars_capped_at_5_b8y0g() {
        // (frankenredis-b8y0g) Upstream lstrlib.c::scanformat bounds
        // flag consumption via `(p - strfrmt) >= sizeof(FLAGS)` where
        // FLAGS = "-+ #0" (5 chars + null = 6 bytes). 6+ consumed
        // flag chars raises 'invalid format (repeated flags)'. fr
        // previously accepted any number of repeated flags silently.
        let mut store = Store::new();

        // Boundary: 5 distinct flags still works.
        let r = eval_script(
            b"return string.format('%-+ #0d', 1)",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("5 flag chars must work");
        assert_eq!(r, RespFrame::BulkString(Some(b"+1".to_vec())));

        // 6 flag chars (one repeated) raises the upstream wording.
        let r = eval_script(
            b"local ok,e = pcall(string.format, '%-+ #00d', 1); return tostring(ok)..':'..tostring(e)",
            &[], &[], &mut store, 0,
        ).expect("pcall wrapper");
        assert_eq!(
            r,
            RespFrame::BulkString(Some(b"false:invalid format (repeated flags)".to_vec()))
        );

        // Direct call (no pcall) carries the user_script:1: prefix.
        let err = eval_script(
            b"return string.format('%-+ #00d', 1)",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect_err("direct call must raise");
        assert!(
            err.contains("user_script:1: invalid format (repeated flags)"),
            "got: {err}"
        );
    }

    #[test]
    fn lua_string_format_width_precision_capped_at_99_94zyf() {
        // (frankenredis-94zyf) Upstream lstrlib.c::scanformat reads
        // at most 2 digits for both width and precision; a third
        // digit raises 'invalid format (width or precision too long)'.
        // fr previously accepted arbitrary-length runs.
        let mut store = Store::new();

        // Boundary: 99 still works.
        let _ = eval_script(b"return string.format('%.99d', 1)", &[], &[], &mut store, 0)
            .expect("precision=99 must work");
        let _ = eval_script(b"return string.format('%99d', 1)", &[], &[], &mut store, 0)
            .expect("width=99 must work");

        // precision=100 errors under pcall with the anonymous-C wording.
        let r = eval_script(
            b"local ok,e = pcall(string.format, '%.100d', 1); return tostring(ok)..':'..tostring(e)",
            &[], &[], &mut store, 0,
        ).expect("pcall wrapper");
        assert_eq!(
            r,
            RespFrame::BulkString(Some(
                b"false:invalid format (width or precision too long)".to_vec()
            ))
        );

        // width=100 same.
        let r = eval_script(
            b"local ok,e = pcall(string.format, '%100d', 1); return tostring(ok)..':'..tostring(e)",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("pcall wrapper");
        assert_eq!(
            r,
            RespFrame::BulkString(Some(
                b"false:invalid format (width or precision too long)".to_vec()
            ))
        );

        // Larger values (999, 1000) still error with the same wording.
        let r = eval_script(
            b"local ok,e = pcall(string.format, '%.999d', 1); return tostring(ok)..':'..tostring(e)",
            &[], &[], &mut store, 0,
        ).expect("pcall wrapper");
        assert_eq!(
            r,
            RespFrame::BulkString(Some(
                b"false:invalid format (width or precision too long)".to_vec()
            ))
        );

        // Direct call (no pcall) keeps the user_script:1: prefix.
        let err = eval_script(
            b"return string.format('%.100d', 1)",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect_err("direct call must raise");
        assert!(
            err.contains("user_script:1: invalid format (width or precision too long)"),
            "got: {err}"
        );
    }

    #[test]
    fn lua_table_getn_validates_arg_ian3l() {
        // (frankenredis-ian3l) Upstream ltablib.c::getn uses
        // luaL_checktype(LUA_TTABLE) and errors with
        // 'bad argument #1 (table expected, got TYPE)' on any
        // non-table arg. fr previously silently returned 0.
        let mut store = Store::new();

        // Valid table arg.
        let r = eval_script(b"return table.getn({1,2,3})", &[], &[], &mut store, 0)
            .expect("valid getn");
        assert_eq!(r, RespFrame::Integer(3));

        // Bad-type args raise the anonymous-C pcall wording.
        let r = eval_script(
            b"local ok,e = pcall(table.getn, nil); return tostring(ok)..':'..tostring(e)",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("pcall wrapper");
        assert_eq!(
            r,
            RespFrame::BulkString(Some(
                b"false:bad argument #1 to '?' (table expected, got nil)".to_vec()
            ))
        );
        let r = eval_script(
            b"local ok,e = pcall(table.getn); return tostring(ok)..':'..tostring(e)",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("pcall wrapper");
        assert_eq!(
            r,
            RespFrame::BulkString(Some(
                b"false:bad argument #1 to '?' (table expected, got no value)".to_vec()
            ))
        );
        let r = eval_script(
            b"local ok,e = pcall(table.getn, 42); return tostring(ok)..':'..tostring(e)",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("pcall wrapper");
        assert_eq!(
            r,
            RespFrame::BulkString(Some(
                b"false:bad argument #1 to '?' (table expected, got number)".to_vec()
            ))
        );
        let r = eval_script(
            b"local ok,e = pcall(table.getn, 'abc'); return tostring(ok)..':'..tostring(e)",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("pcall wrapper");
        assert_eq!(
            r,
            RespFrame::BulkString(Some(
                b"false:bad argument #1 to '?' (table expected, got string)".to_vec()
            ))
        );
    }

    #[test]
    fn lua_string_gsub_repl_non_digit_after_percent_emits_literal_6oe9g() {
        // (frankenredis-6oe9g) Upstream lstrlib.c::add_s emits the
        // literal char after % for any non-digit non-'%' suffix in
        // the gsub replacement string (e.g. '%w' -> 'w', '%a' -> 'a').
        // fr previously pushed both '%' and the following char,
        // leaking the original '%w'.
        let mut store = Store::new();

        // %w → w (one w per match).
        let r = eval_script(
            b"return string.gsub('abc', '(.)', '%w')",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("gsub %w");
        // Note: gsub returns (s, n). EVAL surfaces multi-return as
        // top-of-stack first, so this returns just the string.
        assert_eq!(r, RespFrame::BulkString(Some(b"www".to_vec())));

        // %x → x, %A → A — same rule for any non-digit, non-%.
        let r = eval_script(
            b"return string.gsub('abc', '.', '%x')",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("gsub %x");
        assert_eq!(r, RespFrame::BulkString(Some(b"xxx".to_vec())));

        let r = eval_script(
            b"return string.gsub('abc', '.', '%A')",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("gsub %A");
        assert_eq!(r, RespFrame::BulkString(Some(b"AAA".to_vec())));

        // %! → ! (non-letter punctuation after %).
        let r = eval_script(
            b"return string.gsub('abc', '.', '%!')",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("gsub %!");
        assert_eq!(r, RespFrame::BulkString(Some(b"!!!".to_vec())));

        // Numeric capture refs still work — %1 with captures.
        let r = eval_script(
            b"return string.gsub('abc', '(.)', '<%1>')",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("gsub %1");
        assert_eq!(r, RespFrame::BulkString(Some(b"<a><b><c>".to_vec())));

        // %% still emits a literal %.
        let r = eval_script(
            b"return string.gsub('a', '.', '%%')",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("gsub %%");
        assert_eq!(r, RespFrame::BulkString(Some(b"%".to_vec())));
    }

    #[test]
    fn lua_cyclic_newindex_chain_raises_loop_in_settable_fhf2s() {
        // (frankenredis-fhf2s) Write-side counterpart to 91w0c:
        // upstream lvm.c::luaV_settable raises 'loop in settable'
        // on MAXTAGLOOP exhaustion. fr previously emitted a bespoke
        // '__newindex cascade exceeded depth limit' string.
        let mut store = Store::new();

        let r = eval_script(
            b"local t={}; setmetatable(t,{__newindex=t}); local ok,e = pcall(function() t.x=1 end); return tostring(ok)..':'..tostring(e)",
            &[], &[], &mut store, 0,
        ).expect("pcall wrapper");
        let body = match r {
            RespFrame::BulkString(Some(b)) => String::from_utf8(b).unwrap(),
            other => panic!("unexpected reply: {other:?}"),
        };
        assert!(body.starts_with("false:"), "got {body}");
        assert!(body.contains("loop in settable"), "got {body}");

        // Mutual cycle a→b→a.
        let r = eval_script(
            b"local a={}; local b={}; setmetatable(a,{__newindex=b}); setmetatable(b,{__newindex=a}); local ok,e = pcall(function() a.x=1 end); return tostring(ok)..':'..tostring(e)",
            &[], &[], &mut store, 0,
        ).expect("pcall wrapper");
        let body = match r {
            RespFrame::BulkString(Some(b)) => String::from_utf8(b).unwrap(),
            other => panic!("unexpected reply: {other:?}"),
        };
        assert!(body.contains("loop in settable"), "got {body}");

        // Non-cyclic write still succeeds — single hop through an
        // __newindex table writes into the target.
        let r = eval_script(
            b"local proxy={}; local t = setmetatable({}, {__newindex=proxy}); t.x = 42; return proxy.x",
            &[], &[], &mut store, 0,
        ).expect("__newindex proxy must write through");
        assert_eq!(r, RespFrame::Integer(42));
    }

    #[test]
    fn lua_string_gsub_validates_n_arg_mzjqw() {
        // (frankenredis-mzjqw) Upstream lstrlib.c::str_gsub uses
        // luaL_optinteger for the n arg, raising 'bad argument #4
        // (number expected, got TYPE)' for non-number-convertible
        // values. fr previously silently defaulted to unlimited.
        let mut store = Store::new();

        // Numeric / numeric-string n still work.
        let r = eval_script(
            b"local s,n = string.gsub('aaaa', 'a', 'X', 2); return s..':'..n",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("numeric n");
        assert_eq!(r, RespFrame::BulkString(Some(b"XXaa:2".to_vec())));

        let r = eval_script(
            b"local s,n = string.gsub('aaaa', 'a', 'X', '2'); return s..':'..n",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("numeric-string n");
        assert_eq!(r, RespFrame::BulkString(Some(b"XXaa:2".to_vec())));

        // Bad-n types raise the anonymous-C pcall wording.
        let r = eval_script(
            b"local ok,e = pcall(string.gsub, 'aaaa', 'a', 'X', 'bad'); return tostring(ok)..':'..tostring(e)",
            &[], &[], &mut store, 0,
        ).expect("pcall wrapper");
        assert_eq!(
            r,
            RespFrame::BulkString(Some(
                b"false:bad argument #4 to '?' (number expected, got string)".to_vec()
            ))
        );
        let r = eval_script(
            b"local ok,e = pcall(string.gsub, 'aaaa', 'a', 'X', {}); return tostring(ok)..':'..tostring(e)",
            &[], &[], &mut store, 0,
        ).expect("pcall wrapper");
        assert_eq!(
            r,
            RespFrame::BulkString(Some(
                b"false:bad argument #4 to '?' (number expected, got table)".to_vec()
            ))
        );
        let r = eval_script(
            b"local ok,e = pcall(string.gsub, 'aaaa', 'a', 'X', true); return tostring(ok)..':'..tostring(e)",
            &[], &[], &mut store, 0,
        ).expect("pcall wrapper");
        assert_eq!(
            r,
            RespFrame::BulkString(Some(
                b"false:bad argument #4 to '?' (number expected, got boolean)".to_vec()
            ))
        );
    }

    #[test]
    fn lua_cyclic_index_chain_raises_loop_in_gettable_91w0c() {
        // (frankenredis-91w0c) Upstream lvm.c::luaV_gettable raises
        // 'loop in gettable' when the __index chain depth limit
        // (MAXTAGLOOP = 2000 in 5.1) is exhausted. fr previously
        // returned nil silently after 16 hops.
        let mut store = Store::new();

        // Self-cycle: setmetatable(t, {__index=t}).
        let r = eval_script(
            b"local t = {}; setmetatable(t, {__index=t}); local ok,e = pcall(function() return t.x end); return tostring(ok)..':'..tostring(e)",
            &[], &[], &mut store, 0,
        ).expect("pcall wrapper");
        let body = match r {
            RespFrame::BulkString(Some(b)) => String::from_utf8(b).unwrap(),
            other => panic!("unexpected reply: {other:?}"),
        };
        assert!(body.starts_with("false:"), "got {body}");
        assert!(body.contains("loop in gettable"), "got {body}");

        // Mutual cycle: a.__index=b, b.__index=a.
        let r = eval_script(
            b"local a = {}; local b = {}; setmetatable(a, {__index=b}); setmetatable(b, {__index=a}); local ok,e = pcall(function() return a.x end); return tostring(ok)..':'..tostring(e)",
            &[], &[], &mut store, 0,
        ).expect("pcall wrapper");
        let body = match r {
            RespFrame::BulkString(Some(b)) => String::from_utf8(b).unwrap(),
            other => panic!("unexpected reply: {other:?}"),
        };
        assert!(body.starts_with("false:"), "got {body}");
        assert!(body.contains("loop in gettable"), "got {body}");

        // Non-cyclic chains still resolve normally — a single hop
        // through an __index table.
        let r = eval_script(
            b"local fallback = {x=42}; local t = setmetatable({}, {__index=fallback}); return t.x",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("__index fallback must work");
        assert_eq!(r, RespFrame::Integer(42));

        // __index function returning nil still yields nil (no loop).
        let r = eval_script(
            b"local t = setmetatable({}, {__index=function() return nil end}); return tostring(t.x)",
            &[], &[], &mut store, 0,
        ).expect("__index function returning nil must yield nil string");
        assert_eq!(r, RespFrame::BulkString(Some(b"nil".to_vec())));
    }

    #[test]
    fn lua_string_format_q_escapes_nul_as_three_digit_octal_0en30() {
        // (frankenredis-0en30) Upstream Lua 5.1.5 lstrlib.c::addquoted
        // emits the NUL byte as the three-digit zero-padded \\000 form
        // so a subsequent digit can't be misread as part of the escape.
        // fr previously emitted the ambiguous "\\0" form.
        let mut store = Store::new();

        let r = eval_script(
            b"return string.format('%q', '\\0')",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("nul-only quote");
        assert_eq!(r, RespFrame::BulkString(Some(b"\"\\000\"".to_vec())));

        let r = eval_script(
            b"return string.format('%q', 'a\\0b')",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("nul-in-middle quote");
        assert_eq!(r, RespFrame::BulkString(Some(b"\"a\\000b\"".to_vec())));

        // NUL followed by digit is the disambiguation case the
        // three-digit form preserves.
        let r = eval_script(
            b"return string.format('%q', '\\0' .. '1')",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("nul-then-digit quote");
        assert_eq!(r, RespFrame::BulkString(Some(b"\"\\0001\"".to_vec())));

        // Non-nul escapes unchanged.
        let r = eval_script(
            b"return string.format('%q', 'a\\rb')",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("cr escape");
        assert_eq!(r, RespFrame::BulkString(Some(b"\"a\\rb\"".to_vec())));
    }

    #[test]
    fn lua_string_format_q_preserves_high_bytes_7xudk() {
        let mut store = Store::new();

        for (byte, expected) in [
            (128_u8, b"\"\x80\"".to_vec()),
            (200_u8, b"\"\xC8\"".to_vec()),
            (255_u8, b"\"\xFF\"".to_vec()),
        ] {
            let script = format!("return string.format('%q', string.char({byte}))");
            let r = eval_script(script.as_bytes(), &[], &[], &mut store, 0)
                .expect("high-byte %q must format");
            assert_eq!(
                r,
                RespFrame::BulkString(Some(expected)),
                "wrong %q output for byte {byte}"
            );
        }
    }

    #[test]
    fn lua_math_randomseed_validates_arg_4xjb0() {
        // (frankenredis-4xjb0) Upstream lmathlib.c uses luaL_checkint
        // for the seed arg, raising 'bad argument #1 (number expected,
        // got TYPE)' on missing/non-numeric values. fr previously
        // silently no-op'd, masking the error.
        let mut store = Store::new();

        // Numeric / numeric-string seeds still work.
        eval_script(b"math.randomseed(42) return 1", &[], &[], &mut store, 0)
            .expect("numeric seed");
        eval_script(b"math.randomseed('42') return 1", &[], &[], &mut store, 0)
            .expect("numeric-string seed");

        // Bad args raise the anonymous-C pcall wording.
        let r = eval_script(
            b"local ok,e = pcall(math.randomseed, nil); return tostring(ok)..':'..tostring(e)",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("pcall wrapper");
        assert_eq!(
            r,
            RespFrame::BulkString(Some(
                b"false:bad argument #1 to '?' (number expected, got nil)".to_vec()
            ))
        );
        let r = eval_script(
            b"local ok,e = pcall(math.randomseed); return tostring(ok)..':'..tostring(e)",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("pcall wrapper");
        assert_eq!(
            r,
            RespFrame::BulkString(Some(
                b"false:bad argument #1 to '?' (number expected, got no value)".to_vec()
            ))
        );
        let r = eval_script(
            b"local ok,e = pcall(math.randomseed, {}); return tostring(ok)..':'..tostring(e)",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("pcall wrapper");
        assert_eq!(
            r,
            RespFrame::BulkString(Some(
                b"false:bad argument #1 to '?' (number expected, got table)".to_vec()
            ))
        );
        let r = eval_script(
            b"local ok,e = pcall(math.randomseed, true); return tostring(ok)..':'..tostring(e)",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("pcall wrapper");
        assert_eq!(
            r,
            RespFrame::BulkString(Some(
                b"false:bad argument #1 to '?' (number expected, got boolean)".to_vec()
            ))
        );
        let r = eval_script(
            b"local ok,e = pcall(math.randomseed, 'bad'); return tostring(ok)..':'..tostring(e)",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("pcall wrapper");
        assert_eq!(
            r,
            RespFrame::BulkString(Some(
                b"false:bad argument #1 to '?' (number expected, got string)".to_vec()
            ))
        );
    }

    #[test]
    fn lua_string_sub_validates_j_arg_v2ipw() {
        // (frankenredis-v2ipw) Same fix pattern as izta5 but for
        // string.sub's third arg (j, end position). Upstream
        // luaB_sub uses luaL_optinteger which raises 'bad argument
        // #3 (number expected, got TYPE)' for non-number-convertible
        // values. fr previously silently defaulted to -1.
        let mut store = Store::new();

        // Numeric / numeric-string / nil / missing still work.
        let r = eval_script(b"return string.sub('hello', 1, 3)", &[], &[], &mut store, 0)
            .expect("numeric j");
        assert_eq!(r, RespFrame::BulkString(Some(b"hel".to_vec())));
        let r = eval_script(
            b"return string.sub('hello', 1, '3')",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("numeric-string j");
        assert_eq!(r, RespFrame::BulkString(Some(b"hel".to_vec())));
        let r = eval_script(
            b"return string.sub('hello', 1, nil)",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("nil j");
        assert_eq!(r, RespFrame::BulkString(Some(b"hello".to_vec())));
        let r = eval_script(b"return string.sub('hello', 1)", &[], &[], &mut store, 0)
            .expect("missing j");
        assert_eq!(r, RespFrame::BulkString(Some(b"hello".to_vec())));

        // Bad-j types raise the anonymous-C pcall wording.
        let r = eval_script(
            b"local ok,e = pcall(string.sub,'hello',1,'bad'); return tostring(ok)..':'..tostring(e)",
            &[], &[], &mut store, 0,
        ).expect("pcall wrapper");
        assert_eq!(
            r,
            RespFrame::BulkString(Some(
                b"false:bad argument #3 to '?' (number expected, got string)".to_vec()
            ))
        );
        let r = eval_script(
            b"local ok,e = pcall(string.sub,'hello',1,{}); return tostring(ok)..':'..tostring(e)",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("pcall wrapper");
        assert_eq!(
            r,
            RespFrame::BulkString(Some(
                b"false:bad argument #3 to '?' (number expected, got table)".to_vec()
            ))
        );
        let r = eval_script(
            b"local ok,e = pcall(string.sub,'hello',1,true); return tostring(ok)..':'..tostring(e)",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("pcall wrapper");
        assert_eq!(
            r,
            RespFrame::BulkString(Some(
                b"false:bad argument #3 to '?' (number expected, got boolean)".to_vec()
            ))
        );
    }

    #[test]
    fn lua_string_find_match_validate_init_arg_izta5() {
        // (frankenredis-izta5) Upstream luaB_str_find_aux uses
        // luaL_optinteger for the init arg; non-number-convertible
        // values raise 'bad argument #3 (number expected, got TYPE)'.
        // fr previously coerced via to_number() and silently
        // defaulted to 1 for bogus inputs, masking the error.
        let mut store = Store::new();

        // Numeric and numeric-string still work (Lua coerces "2"→2).
        let r = eval_script(
            b"return string.match('hello', 'l', 2)",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("numeric init must succeed");
        assert_eq!(r, RespFrame::BulkString(Some(b"l".to_vec())));
        let r = eval_script(
            b"return string.match('hello', 'l', '2')",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("numeric-string init must succeed");
        assert_eq!(r, RespFrame::BulkString(Some(b"l".to_vec())));

        // Bad-init cases (string / table / bool) raise the upstream
        // anonymous-C wording when wrapped in pcall.
        let r = eval_script(
            b"local ok,e = pcall(string.match, 'h', 'h', 'bad'); return tostring(ok)..':'..tostring(e)",
            &[], &[], &mut store, 0,
        ).expect("pcall wrapper must not bubble");
        assert_eq!(
            r,
            RespFrame::BulkString(Some(
                b"false:bad argument #3 to '?' (number expected, got string)".to_vec()
            ))
        );
        let r = eval_script(
            b"local ok,e = pcall(string.match, 'h', 'h', {}); return tostring(ok)..':'..tostring(e)",
            &[], &[], &mut store, 0,
        ).expect("pcall wrapper must not bubble");
        assert_eq!(
            r,
            RespFrame::BulkString(Some(
                b"false:bad argument #3 to '?' (number expected, got table)".to_vec()
            ))
        );
        let r = eval_script(
            b"local ok,e = pcall(string.match, 'h', 'h', true); return tostring(ok)..':'..tostring(e)",
            &[], &[], &mut store, 0,
        ).expect("pcall wrapper must not bubble");
        assert_eq!(
            r,
            RespFrame::BulkString(Some(
                b"false:bad argument #3 to '?' (number expected, got boolean)".to_vec()
            ))
        );

        // string.find shares the validation.
        let r = eval_script(
            b"local ok,e = pcall(string.find, 'h', 'h', 'bad'); return tostring(ok)..':'..tostring(e)",
            &[], &[], &mut store, 0,
        ).expect("pcall wrapper must not bubble");
        assert_eq!(
            r,
            RespFrame::BulkString(Some(
                b"false:bad argument #3 to '?' (number expected, got string)".to_vec()
            ))
        );
    }

    #[test]
    fn lua_double_nan_renders_lowercase_per_printf_g_s964e() {
        // (frankenredis-s964e) Vendored renders {double=x} via C
        // printf '%g' which emits lowercase 'nan' for NaN. fr's
        // lua_to_resp Table arm used Rust's default Display for
        // f64 which emits 'NaN'. Pin lowercase parity for NaN; inf
        // / -inf already match (both formatters lowercase).
        let mut store = Store::new();
        let r =
            eval_script(b"return {double=0/0}", &[], &[], &mut store, 0).expect("double nan reply");
        assert_eq!(r, RespFrame::BulkString(Some(b"nan".to_vec())));

        let r = eval_script(b"return {double=1/0}", &[], &[], &mut store, 0)
            .expect("double +inf reply");
        assert_eq!(r, RespFrame::BulkString(Some(b"inf".to_vec())));

        let r = eval_script(b"return {double=-1/0}", &[], &[], &mut store, 0)
            .expect("double -inf reply");
        assert_eq!(r, RespFrame::BulkString(Some(b"-inf".to_vec())));

        let r =
            eval_script(b"return {double=3.14}", &[], &[], &mut store, 0).expect("regular double");
        assert_eq!(r, RespFrame::BulkString(Some(b"3.14".to_vec())));
    }

    #[test]
    fn lua_verbatim_string_requires_format_and_string_subfields_eojcu() {
        // (frankenredis-eojcu) Vendored script_lua.c rejects the
        // verbatim_string hint when either 'format' or 'string'
        // subfield is missing/non-string and falls through to the
        // generic table serialisation — yielding an empty array for
        // hint-only tables. fr previously emitted the BulkString
        // whenever 'string' was a string, ignoring 'format'.
        let mut store = Store::new();

        // Both subfields present and string — emits BulkString.
        let r = eval_script(
            b"return {verbatim_string={format='txt', string='hi'}}",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("valid verbatim_string");
        assert_eq!(r, RespFrame::BulkString(Some(b"hi".to_vec())));

        // Format missing — fall through, empty array.
        let r = eval_script(
            b"return {verbatim_string={string='hi'}}",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("verbatim_string missing format");
        assert_eq!(r, RespFrame::Array(Some(vec![])));

        // String missing — fall through, empty array.
        let r = eval_script(
            b"return {verbatim_string={format='txt'}}",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("verbatim_string missing string");
        assert_eq!(r, RespFrame::Array(Some(vec![])));

        // Format is non-string (number) — fall through.
        let r = eval_script(
            b"return {verbatim_string={format=42, string='hi'}}",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("verbatim_string number format");
        assert_eq!(r, RespFrame::Array(Some(vec![])));

        // verbatim_string itself is not a table — fall through.
        let r = eval_script(b"return {verbatim_string='hi'}", &[], &[], &mut store, 0)
            .expect("verbatim_string not a table");
        assert_eq!(r, RespFrame::Array(Some(vec![])));
    }

    #[test]
    fn lua_reply_table_prefers_err_over_ok_ly7jr() {
        // (frankenredis-ly7jr) Upstream script_lua.c::luaReplyToRedisReply
        // checks the 'err' field BEFORE 'ok' so a table carrying both
        // fields collapses to an error reply. fr previously checked
        // ok first and emitted a status reply, swallowing the err
        // side.
        let mut store = Store::new();

        // Both fields present, ok first in source: err still wins.
        let r = eval_script(b"return {ok='okay', err='oops'}", &[], &[], &mut store, 0)
            .expect("ok+err return must reach the wire as an error frame");
        assert_eq!(r, RespFrame::Error("oops".to_string()));

        // Both fields present, err first in source: err still wins
        // (priority is field-driven, not iteration-order driven).
        let r = eval_script(b"return {err='oops', ok='okay'}", &[], &[], &mut store, 0)
            .expect("err+ok return must reach the wire as an error frame");
        assert_eq!(r, RespFrame::Error("oops".to_string()));

        // err only — unchanged.
        let r = eval_script(b"return {err='just_err'}", &[], &[], &mut store, 0)
            .expect("err-only must reach the wire as an error frame");
        assert_eq!(r, RespFrame::Error("just_err".to_string()));

        // ok only — unchanged.
        let r = eval_script(b"return {ok='just_ok'}", &[], &[], &mut store, 0)
            .expect("ok-only must reach the wire as a status frame");
        assert_eq!(r, RespFrame::SimpleString("just_ok".to_string()));

        // Non-string err/ok values fall through to the regular table
        // serialisation path; neither field triggers the special arms.
        let r = eval_script(b"return {ok=1, err=2}", &[], &[], &mut store, 0)
            .expect("numeric ok/err must not trigger the special arms");
        // Upstream collapses a table whose array part is empty to an
        // empty array; the hash entries (ok/err) are discarded by
        // luaReplyToRedisReply because it only converts integer-keyed
        // entries. fr's lua_to_resp does the same — verify it's not
        // accidentally returning the numeric err/ok values.
        assert!(matches!(
            r,
            RespFrame::Array(_) | RespFrame::BulkString(None)
        ));
    }

    #[test]
    fn lua_table_foreach_foreachi_match_lua_5_1_stdlib_1ohjy() {
        // (frankenredis-1ohjy) Lua 5.1 ltablib.c exposes table.foreach
        // and table.foreachi; both were deprecated/removed in 5.2+
        // but Redis pins Lua 5.1 so vendored 7.2.4 exposes them in
        // the sandbox. fr's table library omitted both, so any script
        // calling them errored with 'attempt to call field <name> (a
        // nil value)'. Pin parity with the upstream contract.
        let mut store = Store::new();

        // Basic iteration: foreach over a sequence-only table emits
        // 1-indexed numeric keys with values.
        let r = eval_script(
            b"local t={'a','b'}; local r=''; table.foreach(t, function(k,v) r=r..k..v end); return r",
            &[], &[], &mut store, 0,
        ).expect("foreach basic must not error");
        assert_eq!(r, RespFrame::BulkString(Some(b"1a2b".to_vec())));

        // foreachi over the same shape: same emission order, called
        // with (i, t[i]).
        let r = eval_script(
            b"local t={'a','b'}; local r=''; table.foreachi(t, function(i,v) r=r..i..v end); return r",
            &[], &[], &mut store, 0,
        ).expect("foreachi basic must not error");
        assert_eq!(r, RespFrame::BulkString(Some(b"1a2b".to_vec())));

        // Early-return short circuits and returns the non-nil value.
        let r = eval_script(
            b"return tostring(table.foreach({1,2,3}, function(k,v) if v==2 then return 'STOP' end end))",
            &[], &[], &mut store, 0,
        ).expect("foreach early-return must not error");
        assert_eq!(r, RespFrame::BulkString(Some(b"STOP".to_vec())));

        let r = eval_script(
            b"return tostring(table.foreachi({10,20,30}, function(i,v) if i==2 then return 99 end end))",
            &[], &[], &mut store, 0,
        ).expect("foreachi early-return must not error");
        assert_eq!(r, RespFrame::BulkString(Some(b"99".to_vec())));

        // foreach skips nil holes in the array part (lua_next
        // semantics) and continues into the hash part.
        let r = eval_script(
            b"local t={1,nil,3,x='X'}; local r=''; table.foreach(t,function(k,v) r=r..tostring(k)..tostring(v)..';' end); return r",
            &[], &[], &mut store, 0,
        ).expect("foreach holes must not error");
        // The first three emissions are deterministic (array part 1,
        // 3); the hash part (only 'x') follows. Verify the array
        // segment is present and the hash entry appears after it.
        let body = match r {
            RespFrame::BulkString(Some(b)) => String::from_utf8(b).unwrap(),
            other => panic!("unexpected reply: {other:?}"),
        };
        assert!(body.starts_with("11;33;"), "got: {body}");
        assert!(body.contains("xX;"), "got: {body}");

        // Bad table arg: missing/nil errors with the upstream-style
        // 'table expected, got TYPE' (anonymous-C-function shape via
        // pcall — name renders as '?').
        let err = eval_script(
            b"local ok,e = pcall(table.foreach, nil, function() end); return tostring(ok)..':'..tostring(e)",
            &[], &[], &mut store, 0,
        ).expect("pcall wrapper must not bubble");
        assert_eq!(
            err,
            RespFrame::BulkString(Some(
                b"false:bad argument #1 to '?' (table expected, got nil)".to_vec()
            ))
        );

        // Bad function arg: same anonymous-C-function shape, arg #2.
        let err = eval_script(
            b"local ok,e = pcall(table.foreach, {1}, nil); return tostring(ok)..':'..tostring(e)",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("pcall wrapper must not bubble");
        assert_eq!(
            err,
            RespFrame::BulkString(Some(
                b"false:bad argument #2 to '?' (function expected, got nil)".to_vec()
            ))
        );
        let err = eval_script(
            b"local ok,e = pcall(table.foreachi, {1}, nil); return tostring(ok)..':'..tostring(e)",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("pcall wrapper must not bubble");
        assert_eq!(
            err,
            RespFrame::BulkString(Some(
                b"false:bad argument #2 to '?' (function expected, got nil)".to_vec()
            ))
        );
    }

    #[test]
    fn lua_gmatch_iterator_callable_outside_for_in_8vp9w() {
        // (frankenredis-8vp9w) Lua 5.1 gmatch returns an iterator
        // function with the source/pattern/position captured as an
        // upvalue closure. fr previously returned a (RustFunction,
        // state_table, nil) triple whose iterator builtin read state
        // from args[0] — the for-in path passed state explicitly, so
        // it worked, but direct calls like `gmatch(s,p)()` saw args[0]
        // as nil and silently returned nil. The fix attaches a __call
        // metatable to the state table itself; both for-in dispatch
        // and direct calls route through metatable_call_handler which
        // prepends the table as the first arg.
        let mut store = Store::new();
        let direct = eval_script(
            b"return string.gmatch('abc def', '%a+')()",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("direct gmatch call must not error");
        assert_eq!(
            direct,
            RespFrame::BulkString(Some(b"abc".to_vec())),
            "gmatch()() must return first match",
        );
        let three = eval_script(
            b"local g=string.gmatch('a b c', '%a'); return g()..g()..g()",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("three successive gmatch calls must not error");
        assert_eq!(
            three,
            RespFrame::BulkString(Some(b"abc".to_vec())),
            "three successive g() calls must return successive matches",
        );
        let exhausted = eval_script(
            b"local g=string.gmatch('a', '%a'); g(); return g()",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("exhausted gmatch must return nil cleanly");
        assert_eq!(
            exhausted,
            RespFrame::BulkString(None),
            "exhausted gmatch must return nil",
        );
        // For-in dispatch must still produce all matches.
        let for_in = eval_script(
            b"local out={}; for w in string.gmatch('one two three', '%a+') do out[#out+1]=w end return table.concat(out, ',')",
            &[], &[], &mut store, 0,
        ).expect("for-in gmatch must not error");
        assert_eq!(
            for_in,
            RespFrame::BulkString(Some(b"one,two,three".to_vec())),
            "for-in gmatch must collect every match",
        );
    }

    #[test]
    fn table_concat_rejects_non_string_separator_a3ksp() {
        // (frankenredis-a3ksp) Upstream ltablib.c::tconcat uses
        // luaL_optlstring(L, 2, "", &lsep) which requires the
        // separator to be a string or number (or absent/nil).
        // Anything else raises "bad argument #2 to 'concat' (string
        // expected, got <type>)" with the user_script:1: prefix.
        let mut store = Store::new();
        let cases: &[(&[u8], &str)] = &[
            (b"return table.concat({1,2,3}, {})", "table"),
            (b"return table.concat({1,2,3}, function() end)", "function"),
            (b"return table.concat({1,2,3}, true)", "boolean"),
            (b"return table.concat({1,2,3}, false)", "boolean"),
        ];
        for (body, ty) in cases {
            let err = eval_script(body, &[], &[], &mut store, 0).expect_err(&format!(
                "expected concat error for {:?}",
                String::from_utf8_lossy(body)
            ));
            let expected =
                format!("user_script:1: bad argument #2 to 'concat' (string expected, got {ty})");
            assert_eq!(
                err,
                expected,
                "wrong error for {:?}",
                String::from_utf8_lossy(body)
            );
        }

        // Number separator coerces to its string representation.
        let ok = eval_script(b"return table.concat({1,2,3}, 5)", &[], &[], &mut store, 0)
            .expect("number sep should work");
        assert_eq!(ok, RespFrame::BulkString(Some(b"15253".to_vec())));

        // String separator works.
        let ok = eval_script(
            b"return table.concat({1,2,3}, ', ')",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("string sep should work");
        assert_eq!(ok, RespFrame::BulkString(Some(b"1, 2, 3".to_vec())));

        // Missing/nil sep -> empty separator (unchanged jwkhc behavior).
        let ok =
            eval_script(b"return table.concat({1,2,3})", &[], &[], &mut store, 0).expect("nil sep");
        assert_eq!(ok, RespFrame::BulkString(Some(b"123".to_vec())));
    }

    #[test]
    fn math_random_arity_and_error_reporting_nwmly() {
        // (frankenredis-nwmly) Upstream lmathlib.c::math_random:
        //   0 args  -> float in [0,1)
        //   1 arg   -> int [1,u]; luaL_argcheck(_, 1<=u, 1, ...)
        //   2 args  -> int [l,u]; luaL_argcheck(_, l<=u, 2, ...)
        //   3+ args -> luaL_error "wrong number of arguments"
        // fr previously reported arg #1 for the 2-arg case, omitted
        // the user_script:1: prefix, and silently ignored extras.
        let mut store = Store::new();

        // 1-arg, m<1 -> arg #1 with prefix.
        let err = eval_script(
            b"math.randomseed(1); return math.random(0)",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect_err("expected interval-empty error");
        assert_eq!(
            err,
            "user_script:1: bad argument #1 to 'random' (interval is empty)",
        );

        // 2-arg, m>n -> arg #2 with prefix.
        let err = eval_script(
            b"math.randomseed(1); return math.random(5, 1)",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect_err("expected interval-empty error");
        assert_eq!(
            err,
            "user_script:1: bad argument #2 to 'random' (interval is empty)",
        );

        // 3+ args -> wrong number of arguments.
        let err = eval_script(b"return math.random(1, 2, 3)", &[], &[], &mut store, 0)
            .expect_err("expected wrong-number-of-args error");
        assert_eq!(err, "user_script:1: wrong number of arguments");

        let err = eval_script(b"return math.random(1, 2, 3, 4)", &[], &[], &mut store, 0)
            .expect_err("expected wrong-number-of-args error");
        assert_eq!(err, "user_script:1: wrong number of arguments");

        // Valid calls still produce values in the expected range.
        let r = eval_script(
            b"math.randomseed(42); local v = math.random(1, 10); return v",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("valid call");
        let RespFrame::Integer(n) = r else {
            panic!("expected integer, got {r:?}");
        };
        assert!((1..=10).contains(&n), "math.random(1,10) returned {n}");

        // 1-arg valid call.
        let r = eval_script(
            b"math.randomseed(42); local v = math.random(5); return v",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("valid 1-arg call");
        let RespFrame::Integer(n) = r else {
            panic!("expected integer, got {r:?}");
        };
        assert!((1..=5).contains(&n), "math.random(5) returned {n}");
    }

    #[test]
    fn math_random_matches_vendored_redislrand48_lwj8o() {
        // (frankenredis-lwj8o) Pinned vendored-redis 7.2.4 outputs for the
        // same seeds. Vendored overrides Lua's rand()/srand() with the
        // 48-bit redisLrand48/redisSrand48 in rand.c (NOT glibc rand),
        // which makes math.random platform-independent. These exact values
        // were captured from redis-server 7.2.4 with the corresponding
        // EVAL strings — if this test regresses, our RedisLrand48 has
        // drifted from upstream.
        let mut store = Store::new();
        // (seed, body, expected) for math.random(1,100).
        let cases_2arg: &[(i32, i64)] = &[(1, 5), (42, 75), (100, 26), (12345, 23), (999, 11)];
        for (seed, expected) in cases_2arg {
            let body = format!("math.randomseed({seed}); return math.random(1,100)");
            let r = eval_script(body.as_bytes(), &[], &[], &mut store, 0)
                .expect("valid math.random call");
            let RespFrame::Integer(n) = r else {
                panic!("expected integer, got {r:?}");
            };
            assert_eq!(
                n, *expected,
                "seed={seed} math.random(1,100): got {n}, expected {expected}",
            );
        }
        // 1-arg math.random(50).
        let cases_1arg: &[(i32, i64)] = &[(1, 3), (42, 38), (100, 13), (999, 6)];
        for (seed, expected) in cases_1arg {
            let body = format!("math.randomseed({seed}); return math.random(50)");
            let r = eval_script(body.as_bytes(), &[], &[], &mut store, 0)
                .expect("valid math.random(50) call");
            let RespFrame::Integer(n) = r else {
                panic!("expected integer, got {r:?}");
            };
            assert_eq!(
                n, *expected,
                "seed={seed} math.random(50): got {n}, expected {expected}",
            );
        }
        // Sequence of 3 draws from seed=42: vendored emits 75, 35, 12.
        let r = eval_script(
            b"math.randomseed(42); return {math.random(1,100), math.random(1,100), math.random(1,100)}",
            &[], &[], &mut store, 0,
        )
        .expect("valid sequence call");
        let RespFrame::Array(Some(items)) = r else {
            panic!("expected array, got {r:?}");
        };
        let mut nums = Vec::new();
        for item in &items {
            if let RespFrame::Integer(n) = item {
                nums.push(*n);
            } else {
                panic!("expected integer item, got {item:?}");
            }
        }
        assert_eq!(nums, vec![75i64, 35, 12], "seed=42 3-draw sequence");
    }

    /// (frankenredis-ii6en) string.byte's optional `i` and `j` index
    /// args go through luaL_optint in vendored Redis 7.2.4 — present-
    /// but-non-numeric args raise 'bad argument #N to byte (number
    /// expected, got TYPE)'. fr previously routed through
    /// to_number().unwrap_or(default), silently swallowing the type
    /// error.
    #[test]
    fn lua_string_byte_validates_index_args_per_vendored_ii6en() {
        let mut store = Store::new();

        // Numeric strings still coerce (matches vendored).
        let r = eval_script(b"return string.byte('abc', '2')", &[], &[], &mut store, 0)
            .expect("string-numeric index coerces");
        assert_eq!(r, RespFrame::Integer(98));

        // Non-numeric string → error with #2.
        let err = eval_script(b"return string.byte('abc', 'xy')", &[], &[], &mut store, 0)
            .expect_err("non-numeric index #2");
        assert!(
            err.contains("bad argument #2 to 'byte' (number expected, got string)"),
            "wrong error: {err:?}"
        );

        // Boolean → error.
        let err = eval_script(
            b"local ok,e=pcall(string.byte, 'abc', true) return tostring(e)",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("pcall returns the error string");
        assert_eq!(
            err,
            RespFrame::BulkString(Some(
                b"bad argument #2 to '?' (number expected, got boolean)".to_vec()
            ))
        );

        // Table → error at #3 (j arg).
        let err = eval_script(
            b"local ok,e=pcall(string.byte, 'abc', 1, {}) return tostring(e)",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("pcall returns the error string");
        assert_eq!(
            err,
            RespFrame::BulkString(Some(
                b"bad argument #3 to '?' (number expected, got table)".to_vec()
            ))
        );

        // Missing args still use defaults — no error.
        let r = eval_script(b"return string.byte('abc')", &[], &[], &mut store, 0)
            .expect("no extra args");
        assert_eq!(r, RespFrame::Integer(97));

        // Explicit nil also uses default (luaL_optint default branch).
        let r = eval_script(b"return string.byte('abc', nil)", &[], &[], &mut store, 0)
            .expect("nil index uses default");
        assert_eq!(r, RespFrame::Integer(97));
    }

    #[test]
    fn string_char_invalid_value_includes_user_script_prefix_uni2j() {
        // (frankenredis-uni2j) Upstream luaL_argerror prepends the
        // source location "user_script:1: " before "bad argument #N to
        // 'char' (invalid value)". fr previously emitted the bare
        // wording for both the out-of-range path (256, -1) and the
        // type-error path.
        let mut store = Store::new();
        for body in &[
            b"return string.char(256)".as_slice(),
            b"return string.char(-1)".as_slice(),
            b"return string.char(99999)".as_slice(),
        ] {
            let err = eval_script(body, &[], &[], &mut store, 0).expect_err(&format!(
                "expected error for {:?}",
                String::from_utf8_lossy(body)
            ));
            assert!(
                err.starts_with("user_script:1: bad argument #1 to 'char' (invalid value)"),
                "missing prefix for {:?}: {err}",
                String::from_utf8_lossy(body),
            );
        }
        // Wrong-type also prefixed.
        let err = eval_script(b"return string.char({})", &[], &[], &mut store, 0)
            .expect_err("expected error");
        assert!(
            err.starts_with("user_script:1: bad argument #1 to 'char' (number expected"),
            "missing prefix for type-error: {err}",
        );
        // Valid call still works.
        let ok = eval_script(b"return string.char(72, 73)", &[], &[], &mut store, 0)
            .expect("valid string.char");
        assert_eq!(ok, RespFrame::BulkString(Some(b"HI".to_vec())));
    }

    #[test]
    fn redis_sha1hex_uses_lua_tolstring_coercion_20ggg() {
        // (frankenredis-20ggg) Upstream luaRedisSha1hexCommand calls
        // lua_tolstring which returns NULL for nil/bool/table/etc., so
        // those types hash the empty byte string. fr previously used
        // to_display_string which rendered bool as "true"/"false" etc.
        let sha1_empty = "da39a3ee5e6b4b0d3255bfef95601890afd80709";
        let mut store = Store::new();
        for body in &[
            b"return redis.sha1hex(true)".as_slice(),
            b"return redis.sha1hex(false)".as_slice(),
            b"return redis.sha1hex(nil)".as_slice(),
            b"return redis.sha1hex({1,2,3})".as_slice(),
        ] {
            let result = eval_script(body, &[], &[], &mut store, 0).expect("eval");
            let RespFrame::BulkString(Some(bytes)) = result else {
                panic!(
                    "expected bulk string for {:?}, got {result:?}",
                    String::from_utf8_lossy(body)
                );
            };
            let hex = String::from_utf8(bytes).unwrap();
            assert_eq!(
                hex,
                sha1_empty,
                "non-string types must hash to empty for {:?}",
                String::from_utf8_lossy(body),
            );
        }
        // Strings and numbers continue to hash their byte representation.
        let ok =
            eval_script(b"return redis.sha1hex('hello')", &[], &[], &mut store, 0).expect("eval");
        let RespFrame::BulkString(Some(bytes)) = ok else {
            panic!("expected sha bulk")
        };
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d", // sha1("hello")
        );
    }

    #[test]
    fn redis_log_error_carries_err_prefix_r9f5y() {
        // (frankenredis-r9f5y) Upstream luaPushError stores the
        // "ERR " prefix as part of the err table's err field, so
        // pcall sees the prefix verbatim. fr's redis.log handler
        // previously stored the bare wording; the direct-call wrapper
        // auto-prepended "ERR " but the pcall path did not.
        let mut store = Store::new();
        for (body, want_substr) in &[
            (
                b"local ok,e = pcall(redis.log); return tostring(e)".as_slice(),
                "ERR redis.log() requires two arguments or more.",
            ),
            (
                b"local ok,e = pcall(redis.log, 'bad', 'm'); return tostring(e)".as_slice(),
                "ERR First argument must be a number (log level).",
            ),
            (
                b"local ok,e = pcall(redis.log, -1, 'm'); return tostring(e)".as_slice(),
                "ERR Invalid debug level.",
            ),
            (
                b"local ok,e = pcall(redis.log, 99, 'm'); return tostring(e)".as_slice(),
                "ERR Invalid debug level.",
            ),
            (
                b"local ok,e = pcall(redis.log, {}, 'm'); return tostring(e)".as_slice(),
                "ERR First argument must be a number (log level).",
            ),
            (
                b"local ok,e = pcall(redis.log, nil, 'm'); return tostring(e)".as_slice(),
                "ERR First argument must be a number (log level).",
            ),
        ] {
            let frame = eval_script(body, &[], &[], &mut store, 0)
                .unwrap_or_else(|e| panic!("eval {:?} failed: {e}", String::from_utf8_lossy(body)));
            let RespFrame::BulkString(Some(bytes)) = frame else {
                panic!(
                    "expected bulk string for {:?}, got {frame:?}",
                    String::from_utf8_lossy(body)
                );
            };
            let s = String::from_utf8_lossy(&bytes);
            assert_eq!(
                s,
                *want_substr,
                "body={:?} got {s}",
                String::from_utf8_lossy(body)
            );
        }
    }

    #[test]
    fn redis_sha1hex_rejects_wrong_arity_f5rgn() {
        // (frankenredis-f5rgn) Upstream luaRedisSha1hexCommand has
        // `if (argc != 1) luaPushError(... "wrong number of arguments")
        // lua_error()`. Both argc=0 and argc>1 raise the same error.
        // fr previously rejected argc=0 but silently sha1'd args[0]
        // for argc>1.
        let mut store = Store::new();
        for body in &[
            b"return redis.sha1hex()".as_slice(),
            b"return redis.sha1hex('a','b')",
            b"return redis.sha1hex('a','b','c')",
        ] {
            let err =
                eval_script(body, &[], &[], &mut store, 0).expect_err("expected wrong-arity error");
            assert!(
                err.contains("wrong number of arguments"),
                "body={:?} got {err:?}",
                String::from_utf8_lossy(body)
            );
        }

        // pcall-caught propagation.
        let caught = eval_script(
            b"local ok,err = pcall(redis.sha1hex,'a','b'); return tostring(err)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        let RespFrame::BulkString(Some(bytes)) = caught else {
            panic!("expected bulk")
        };
        let s = String::from_utf8(bytes).unwrap();
        assert!(
            s.contains("wrong number of arguments"),
            "pcall caught: {s:?}"
        );
    }

    #[test]
    fn lua_pattern_back_references_match_captured_substring_53u08() {
        // (frankenredis-53u08) Upstream Lua patterns support %1-%9 as
        // back-references — the N-th capture's bytes must match at the
        // current string position. fr previously routed %N through
        // lua_class_match which treated it as the literal digit char.
        let mut store = Store::new();

        // Simple back-reference.
        let frame = eval_script(
            b"return string.match('abcabc', '(abc)%1')",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"abc".to_vec())));

        // Capture differs from the second occurrence.
        let frame = eval_script(
            b"return string.match('abcxyz', '(abc)%1')",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::BulkString(None));

        // Two captures: %1 and %2 refer to first/second captures.
        // Direct test: 'abxxabxx' matches (ab)(xx)%1%2 ⇒ first capture 'ab'.
        let frame = eval_script(
            b"return string.match('abxxabxx', '(ab)(xx)%1%2')",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"ab".to_vec())));

        // Out-of-range index → currently silently fails to match in
        // fr (upstream raises "invalid capture index" at match time,
        // which requires threading errors through lua_pat_match — see
        // bead deferral note). Pin the current behavior; flip the
        // assertion when the error-threading lands.
        let frame = eval_script(
            b"return string.match('abc', '(abc)%5')",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::BulkString(None));

        // gsub also honors %N in the pattern.
        // Vendored behavior: 'aaa-aaa' matches → 'M'. 'bbb-bbb' matches → 'M'.
        // For 'xyz-zyx' the greedy %w+ then back-ref finds only the
        // single-char palindrome 'z-z' (positions 2-4 inside 'xyz-zyx'),
        // yielding 'xy' + 'M' + 'yx' = 'xyMyx'.
        let frame = eval_script(
            b"return string.gsub('aaa-aaa,bbb-bbb,xyz-zyx', '(%w+)-%1', 'M')",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"M,M,xyMyx".to_vec())));
    }

    #[test]
    fn assert_second_arg_uses_default_for_nil_and_argerror_for_bad_type_wmu03() {
        // (frankenredis-wmu03) Upstream luaB_assert uses
        // luaL_optstring(L, 2, "assertion failed!"): nil/absent →
        // default; string/number → coerce; other types → bad-argument.
        let mut store = Store::new();

        // Default message for nil/absent.
        for body in &[
            b"local ok,e=pcall(assert,false,nil); return tostring(e)".as_slice(),
            b"local ok,e=pcall(assert,nil,nil); return tostring(e)",
            b"local ok,e=pcall(assert,false); return tostring(e)",
            b"local ok,e=pcall(assert,nil); return tostring(e)",
        ] {
            let frame = eval_script(body, &[], &[], &mut store, 0).unwrap();
            let RespFrame::BulkString(Some(bytes)) = frame else {
                panic!("expected bulk for {:?}", String::from_utf8_lossy(body))
            };
            assert_eq!(
                String::from_utf8(bytes).unwrap(),
                "assertion failed!",
                "body={:?}",
                String::from_utf8_lossy(body)
            );
        }

        // String/number arg 2: passed through verbatim.
        let frame = eval_script(
            b"local ok,e=pcall(assert,nil,'custom'); return tostring(e)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"custom".to_vec())));

        let frame = eval_script(
            b"local ok,e=pcall(assert,nil,42); return tostring(e)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"42".to_vec())));

        // Non-string/number arg 2: bad-argument error with pcall shape.
        let cases: &[(&[u8], &str)] = &[
            (
                b"local ok,e=pcall(assert,nil,true); return tostring(e)",
                "bad argument #2 to '?' (string expected, got boolean)",
            ),
            (
                b"local ok,e=pcall(assert,nil,false); return tostring(e)",
                "bad argument #2 to '?' (string expected, got boolean)",
            ),
            (
                b"local ok,e=pcall(assert,nil,{}); return tostring(e)",
                "bad argument #2 to '?' (string expected, got table)",
            ),
        ];
        for (body, expected) in cases {
            let frame = eval_script(body, &[], &[], &mut store, 0).unwrap();
            let RespFrame::BulkString(Some(bytes)) = frame else {
                panic!("expected bulk for {:?}", String::from_utf8_lossy(body))
            };
            assert_eq!(
                String::from_utf8(bytes).unwrap(),
                *expected,
                "body={:?}",
                String::from_utf8_lossy(body)
            );
        }

        // Truthy first arg: pass through (regression).
        let frame = eval_script(b"return assert(42)", &[], &[], &mut store, 0).unwrap();
        assert_eq!(frame, RespFrame::Integer(42));
        let frame = eval_script(b"return assert('hi')", &[], &[], &mut store, 0).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"hi".to_vec())));
    }

    #[test]
    fn method_call_non_table_receiver_errors_at_index_step_aaudb() {
        // (frankenredis-aaudb) Upstream Lua 5.1 grammar: `x:foo(...)`
        // is sugar for `x.foo(x, ...)`. Indexing x with "foo" happens
        // FIRST — if x isn't a table (and has no __index meta), the
        // indexing step fails before the call is attempted. The error
        // carries the receiver's accessor context.
        let mut store = Store::new();
        let cases: &[(&[u8], &str)] = &[
            (
                b"local x = 1; x:foo()",
                "user_script:1: attempt to index local 'x' (a number value)",
            ),
            (
                b"local x = nil; x:foo()",
                "user_script:1: attempt to index local 'x' (a nil value)",
            ),
            (
                b"local x = true; x:foo()",
                "user_script:1: attempt to index local 'x' (a boolean value)",
            ),
            (
                b"local t = {a = 1}; t.a:foo()",
                "user_script:1: attempt to index field 'a' (a number value)",
            ),
        ];
        for (body, expected) in cases {
            let err = eval_script(body, &[], &[], &mut store, 0).unwrap_err();
            assert!(
                err.contains(expected),
                "body={:?} got {err:?}",
                String::from_utf8_lossy(body)
            );
        }

        // Sanity: Table receivers with missing method key continue to
        // emit "attempt to call method 'm' (a nil value)" — that's the
        // correct path because indexing the table succeeds (returns nil)
        // and the CALL step is what fails.
        let err = eval_script(b"local t = {}; t:m()", &[], &[], &mut store, 0).unwrap_err();
        assert!(
            err.contains("attempt to call method 'm' (a nil value)"),
            "got {err:?}"
        );

        // Sanity: String receivers route through the string library.
        let frame = eval_script(b"return ('abc'):upper()", &[], &[], &mut store, 0).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"ABC".to_vec())));
    }

    #[test]
    fn call_function_bad_callable_uses_caller_frame_kind_4ovjf() {
        // (frankenredis-4ovjf) Upstream luaG_addinfo derives the
        // source:line prefix from the immediate Lua caller's frame:
        //   - Caller is a Lua function or top-level chunk → prefix.
        //   - Caller is a C frame (pcall, internal RustFunction)    → no prefix.
        //
        // fr previously checked self.current_invocation_name.is_some(),
        // which is wrong: pcall clears the name, so even when the
        // inner Lua function body subsequently triggers an iterator
        // call to nil, fr dropped the prefix. The new check inspects
        // the kind-stack BEFORE pushing.
        let mut store = Store::new();
        let cases: &[(&[u8], &str)] = &[
            // Inside a Lua function called via pcall — Lua caller → prefix.
            (
                b"local ok,e=pcall(function() for k,v in nil do end end); return tostring(e)",
                "user_script:1: attempt to call a nil value",
            ),
            (
                b"local ok,e=pcall(function() for k,v in 1 do end end); return tostring(e)",
                "user_script:1: attempt to call a number value",
            ),
            (
                b"local ok,e=pcall(function() for k,v in 'abc' do end end); return tostring(e)",
                "user_script:1: attempt to call a string value",
            ),
            // pcall directly invoking a non-callable — C caller → no prefix.
            // (Already pinned by frankenredis-o3epl; preserved here for clarity.)
            (
                b"local ok,e=pcall(nil); return tostring(e)",
                "attempt to call a nil value",
            ),
            (
                b"local ok,e=pcall(1); return tostring(e)",
                "attempt to call a number value",
            ),
        ];
        for (body, expected) in cases {
            let frame = eval_script(body, &[], &[], &mut store, 0).unwrap();
            let RespFrame::BulkString(Some(bytes)) = frame else {
                panic!("expected bulk for {:?}", String::from_utf8_lossy(body))
            };
            assert_eq!(
                String::from_utf8(bytes).unwrap(),
                *expected,
                "body={:?}",
                String::from_utf8_lossy(body)
            );
        }
    }

    #[test]
    fn lua_parser_rejects_suffix_on_literal_primaries_oee7k() {
        // (frankenredis-oee7k) Upstream Lua 5.1 grammar splits
        // primaryexp (Name | `(`exp`)`) from simpleexp (literals);
        // only primaryexp can carry `.`, `[`, `:`, or call args. fr
        // previously routed every literal through suffix parsing, so
        // `1[2]`, `'abc':upper()`, etc. parsed (and ran).
        let mut store = Store::new();

        // All these used to silently parse and either runtime-error
        // or even succeed (`'abc':upper()` returned 'ABC'). They now
        // surface at compile time via the chunk-level <eof>-expected
        // check (frankenredis-yunl8 follow-up).
        let cases: &[&[u8]] = &[
            b"return 1[2]",
            b"return 1.0[2]",
            b"return true[1]",
            b"return nil[1]",
            b"return 'a'[1]",
            b"return true.foo",
            b"return nil.foo",
            b"return 1()",
            b"return true()",
            b"return nil()",
            b"return 'a'()",
            b"return 'abc':upper()",
        ];
        for body in cases {
            let err = eval_script(body, &[], &[], &mut store, 0).unwrap_err();
            assert!(
                err.contains("'<eof>' expected near"),
                "body={:?} expected compile-time error, got {err:?}",
                String::from_utf8_lossy(body)
            );
        }

        // Sanity: suffix is fine on Name and parenthesized expression.
        let frame = eval_script(b"return (1)", &[], &[], &mut store, 0).unwrap();
        assert_eq!(frame, RespFrame::Integer(1));
        let frame = eval_script(b"return ('abc'):upper()", &[], &[], &mut store, 0).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"ABC".to_vec())));
        let frame = eval_script(b"local t={a=1}; return t.a", &[], &[], &mut store, 0).unwrap();
        assert_eq!(frame, RespFrame::Integer(1));
        let frame = eval_script(
            b"local function f() return 'x' end; return f()",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"x".to_vec())));
        let frame = eval_script(
            b"local function f() return {1,2,3} end; return f()[2]",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::Integer(2));
    }

    #[test]
    fn lua_long_string_and_comment_level_brackets_h1vbd() {
        // (frankenredis-h1vbd) Upstream Lua 5.1 lexer supports level
        // markers in long strings (`[=[…]=]`, `[==[…]==]`, etc.) and
        // long comments (`--[=[…]=]`). The level counts `=` signs
        // between brackets, allowing nested content with matching
        // closing brackets.
        let mut store = Store::new();

        let cases: &[(&[u8], &[u8])] = &[
            (b"return [=[abc]=]", b"abc"),
            (b"return [==[ab]=cd]==]", b"ab]=cd"),
            (b"return [===[deep]===]", b"deep"),
            // Level 0 still works.
            (b"return [[basic]]", b"basic"),
            // Embedded brackets at lower levels survive.
            (
                b"return [=[contains [[brackets]]]=]",
                b"contains [[brackets]]",
            ),
        ];
        for (body, expected) in cases {
            let frame = eval_script(body, &[], &[], &mut store, 0).unwrap();
            assert_eq!(
                frame,
                RespFrame::BulkString(Some(expected.to_vec())),
                "body={:?}",
                String::from_utf8_lossy(body)
            );
        }

        // Long comments with level markers should be skipped.
        let frame = eval_script(b"--[=[ level 1 ]=] return 2", &[], &[], &mut store, 0).unwrap();
        assert_eq!(frame, RespFrame::Integer(2));
        let frame = eval_script(b"--[==[ level 2 ]==] return 3", &[], &[], &mut store, 0).unwrap();
        assert_eq!(frame, RespFrame::Integer(3));

        // Unterminated long comment surfaces the upstream wording.
        let err = eval_script(b"--[[ unterminated", &[], &[], &mut store, 0).unwrap_err();
        assert!(
            err.contains("unfinished long comment near '<eof>'"),
            "got {err:?}"
        );
        let err = eval_script(b"--[=[ unterminated", &[], &[], &mut store, 0).unwrap_err();
        assert!(
            err.contains("unfinished long comment near '<eof>'"),
            "got {err:?}"
        );
    }

    #[test]
    fn lua_parser_lexer_error_wording_matches_upstream_yunl8() {
        // (frankenredis-yunl8) Four upstream-parity fixes:
        //   1. read_string EOF → "unfinished string near '<eof>'"
        //   2. read_long_string EOF → "unfinished long string near '<eof>'"
        //   3. parse_call_args bad token → "function arguments expected near '<token>'"
        //   4. parse_block exits after `return` so trailing tokens
        //      surface via the chunk-level '<eof>' expected check.
        let mut store = Store::new();
        let cases: &[(&[u8], &str)] = &[
            (b"return 'unterminated", "unfinished string near '<eof>'"),
            (b"return \"unterminated", "unfinished string near '<eof>'"),
            (
                b"return [[unterminated",
                "unfinished long string near '<eof>'",
            ),
            (b"return (1+1))", "'<eof>' expected near ')'"),
            (
                b"return foo:bar",
                "function arguments expected near '<eof>'",
            ),
        ];
        for (body, expected_msg) in cases {
            let err = eval_script(body, &[], &[], &mut store, 0).unwrap_err();
            assert!(
                err.contains(expected_msg),
                "body={:?} got {err:?} expected to contain {expected_msg:?}",
                String::from_utf8_lossy(body)
            );
        }

        // Sanity: valid programs still execute.
        let frame = eval_script(b"return (1+1)", &[], &[], &mut store, 0).unwrap();
        assert_eq!(frame, RespFrame::Integer(2));
        let frame = eval_script(
            b"local function f() return 1 end; return f()",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::Integer(1));
    }

    #[test]
    fn lua_lexer_malformed_number_wording_matches_upstream_5ife7() {
        // (frankenredis-5ife7) Lua 5.1's llex.c::lex_number scans a
        // number token greedily (including trailing alphanumeric junk)
        // and emits "malformed number near '<lexeme>'" when the lexeme
        // fails to parse. fr previously surfaced Rust's stdlib
        // "invalid float literal" verbatim for any non-numeric trail
        // (multiple dots, incomplete scientific notation, alpha
        // trailers) — and also stopped its hex lexer at the first
        // non-hex byte, so `0xG` emitted "...near '0x'" instead of
        // "...near '0xG'".
        let mut store = Store::new();
        let cases: &[(&[u8], &str)] = &[
            (b"return 1..1", "malformed number near '1..1'"),
            (b"return 1.2.3", "malformed number near '1.2.3'"),
            (b"return 1..", "malformed number near '1..'"),
            (b"return 1e", "malformed number near '1e'"),
            (b"return 1e+", "malformed number near '1e+'"),
            (b"return 1ea", "malformed number near '1ea'"),
            (b"return 1abc", "malformed number near '1abc'"),
            (b"return 0xG", "malformed number near '0xG'"),
            (b"return 0x", "malformed number near '0x'"),
        ];
        for (body, expected_msg) in cases {
            let err = eval_script(body, &[], &[], &mut store, 0).unwrap_err();
            assert!(
                err.contains(expected_msg),
                "body={:?} got {err:?} expected to contain {expected_msg:?}",
                String::from_utf8_lossy(body)
            );
        }

        // Sanity: valid number literals still work.
        for (body, expected) in &[
            (b"return 1+1".as_slice(), RespFrame::Integer(2)),
            (b"return 1e3", RespFrame::Integer(1000)),
            (b"return 0xFF", RespFrame::Integer(255)),
            (b"return 1.5+0.5", RespFrame::Integer(2)),
        ] {
            let got = eval_script(body, &[], &[], &mut store, 0).unwrap();
            assert_eq!(&got, expected, "body={:?}", String::from_utf8_lossy(body));
        }
    }

    #[test]
    fn coroutine_argerror_pcall_shape_96j2u() {
        // (frankenredis-96j2u) coroutine.create/wrap/resume/status now
        // route their bad-argument errors through lua_format_argerror
        // so pcall(coroutine.X,…) emits `bad argument #1 to '?' (…)`
        // without the user_script:1: prefix.
        let mut store = Store::new();
        let cases: &[(&[u8], &str)] = &[
            (
                b"local ok,e=pcall(coroutine.create); return tostring(e)",
                "bad argument #1 to '?' (Lua function expected)",
            ),
            (
                b"local ok,e=pcall(coroutine.create,'x'); return tostring(e)",
                "bad argument #1 to '?' (Lua function expected)",
            ),
            (
                b"local ok,e=pcall(coroutine.create,1); return tostring(e)",
                "bad argument #1 to '?' (Lua function expected)",
            ),
            (
                b"local ok,e=pcall(coroutine.wrap); return tostring(e)",
                "bad argument #1 to '?' (Lua function expected)",
            ),
            (
                b"local ok,e=pcall(coroutine.wrap,nil); return tostring(e)",
                "bad argument #1 to '?' (Lua function expected)",
            ),
            (
                b"local ok,e=pcall(coroutine.resume); return tostring(e)",
                "bad argument #1 to '?' (coroutine expected)",
            ),
            (
                b"local ok,e=pcall(coroutine.resume,1); return tostring(e)",
                "bad argument #1 to '?' (coroutine expected)",
            ),
            (
                b"local ok,e=pcall(coroutine.status); return tostring(e)",
                "bad argument #1 to '?' (coroutine expected)",
            ),
            (
                b"local ok,e=pcall(coroutine.status,1); return tostring(e)",
                "bad argument #1 to '?' (coroutine expected)",
            ),
        ];
        for (body, expected) in cases {
            let frame = eval_script(body, &[], &[], &mut store, 0).unwrap();
            let RespFrame::BulkString(Some(bytes)) = frame else {
                panic!("expected bulk for {:?}", String::from_utf8_lossy(body))
            };
            assert_eq!(
                String::from_utf8(bytes).unwrap(),
                *expected,
                "body={:?}",
                String::from_utf8_lossy(body)
            );
        }

        // Direct-call regressions: named/prefixed shape preserved.
        for (body, fname, msg) in &[
            (
                b"return coroutine.create(nil)".as_slice(),
                "create",
                "Lua function expected",
            ),
            (
                b"return coroutine.wrap(nil)",
                "wrap",
                "Lua function expected",
            ),
            (
                b"return coroutine.resume(nil)",
                "resume",
                "coroutine expected",
            ),
            (
                b"return coroutine.status(nil)",
                "status",
                "coroutine expected",
            ),
        ] {
            let err = eval_script(body, &[], &[], &mut store, 0).unwrap_err();
            let expected = format!("user_script:1: bad argument #1 to '{fname}' ({msg})");
            assert!(err.contains(&expected), "got {err:?} expected {expected:?}");
        }
    }

    #[test]
    fn cjson_encode_decode_arg_validation_and_pcall_shape_u4mn6() {
        // (frankenredis-u4mn6) cjson.encode now requires exactly one
        // argument (previously fr silently encoded nil → "null"). All
        // four argerror sites (encode missing-arg + serialise fail,
        // decode missing-arg + bad-type) honor the dual direct/pcall
        // shape via lua_format_argerror.
        let mut store = Store::new();

        let cases: &[(&[u8], &str)] = &[
            (
                b"local ok,e=pcall(cjson.encode); return tostring(e)",
                "bad argument #1 to '?' (expected 1 argument)",
            ),
            (
                b"local ok,e=pcall(cjson.encode,function() end); return tostring(e)",
                "Cannot serialise function: type not supported",
            ),
            // (frankenredis-yovmj) Extra args must also be rejected —
            // vendored's luaL_argcheck(L, lua_gettop(L) == 1, ...)
            // raises the same "expected 1 argument" wording when
            // top != 1, whether top is 0 or 2+.
            (
                b"local ok,e=pcall(cjson.encode, 1, 2); return tostring(e)",
                "bad argument #1 to '?' (expected 1 argument)",
            ),
            (
                b"local ok,e=pcall(cjson.encode, 'a', 'b', 'c'); return tostring(e)",
                "bad argument #1 to '?' (expected 1 argument)",
            ),
            (
                b"local ok,e=pcall(cjson.decode); return tostring(e)",
                "bad argument #1 to '?' (expected 1 argument)",
            ),
            (
                b"local ok,e=pcall(cjson.decode,nil); return tostring(e)",
                "bad argument #1 to '?' (string expected, got nil)",
            ),
            (
                b"local ok,e=pcall(cjson.decode,true); return tostring(e)",
                "bad argument #1 to '?' (string expected, got boolean)",
            ),
            (
                b"local ok,e=pcall(cjson.decode,{}); return tostring(e)",
                "bad argument #1 to '?' (string expected, got table)",
            ),
        ];
        for (body, expected) in cases {
            let frame = eval_script(body, &[], &[], &mut store, 0).unwrap();
            let RespFrame::BulkString(Some(bytes)) = frame else {
                panic!("expected bulk for {:?}", String::from_utf8_lossy(body))
            };
            assert_eq!(
                String::from_utf8(bytes).unwrap(),
                *expected,
                "body={:?}",
                String::from_utf8_lossy(body)
            );
        }

        // Direct-call regressions: named/prefixed shape preserved.
        let err = eval_script(b"return cjson.encode()", &[], &[], &mut store, 0).unwrap_err();
        assert!(
            err.contains("user_script:1: bad argument #1 to 'encode' (expected 1 argument)"),
            "got {err:?}"
        );
        let err = eval_script(
            b"return cjson.encode(function() end)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap_err();
        assert!(
            err.contains("user_script:1: Cannot serialise function: type not supported"),
            "got {err:?}"
        );
        let err = eval_script(b"return cjson.decode()", &[], &[], &mut store, 0).unwrap_err();
        assert!(
            err.contains("user_script:1: bad argument #1 to 'decode' (expected 1 argument)"),
            "got {err:?}"
        );

        // Sanity: valid usage still works.
        let frame = eval_script(b"return cjson.encode(1)", &[], &[], &mut store, 0).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"1".to_vec())));
        let frame = eval_script(b"return cjson.decode('1')", &[], &[], &mut store, 0).unwrap();
        assert_eq!(frame, RespFrame::Integer(1));
    }

    #[test]
    fn newindex_attempt_to_index_carries_accessor_label_ct3ir() {
        // (frankenredis-ct3ir) Newindex on a non-table value must pick
        // up the accessor context (local 'x' / field 'f' / global 'g')
        // just like reads do. Previously fr emitted the bare wording.
        let mut store = Store::new();

        let cases: &[(&[u8], &str)] = &[
            (
                b"local ok,e=pcall(function() local x = nil; x.a = 1 end); return tostring(e)",
                "user_script:1: attempt to index local 'x' (a nil value)",
            ),
            (
                b"local ok,e=pcall(function() local x = 1; x.a = 1 end); return tostring(e)",
                "user_script:1: attempt to index local 'x' (a number value)",
            ),
            (
                b"local ok,e=pcall(function() local x = 'abc'; x.a = 1 end); return tostring(e)",
                "user_script:1: attempt to index local 'x' (a string value)",
            ),
            (
                b"local ok,e=pcall(function() local x = true; x.a = 1 end); return tostring(e)",
                "user_script:1: attempt to index local 'x' (a boolean value)",
            ),
            // Field-style accessor on a nested non-table.
            (
                b"local ok,e=pcall(function() local t={a=nil}; t.a.b = 1 end); return tostring(e)",
                "user_script:1: attempt to index field 'a' (a nil value)",
            ),
        ];
        for (body, expected) in cases {
            let frame = eval_script(body, &[], &[], &mut store, 0).unwrap();
            let RespFrame::BulkString(Some(bytes)) = frame else {
                panic!("expected bulk for {:?}", String::from_utf8_lossy(body))
            };
            assert_eq!(
                String::from_utf8(bytes).unwrap(),
                *expected,
                "body={:?}",
                String::from_utf8_lossy(body)
            );
        }
    }

    #[test]
    fn table_insert_concat_luaerror_pcall_shape_toecv() {
        // (frankenredis-toecv) luaL_error wording in table.insert
        // (wrong-arity) and table.concat (invalid-value) now drops the
        // user_script:1: prefix when invoked via pcall(C-builtin).
        let mut store = Store::new();
        let cases: &[(&[u8], &str)] = &[
            (
                b"local ok,e=pcall(table.insert,{1,2,3}); return tostring(e)",
                "wrong number of arguments to 'insert'",
            ),
            (
                b"local ok,e=pcall(table.insert,{1,2,3},1,2,3); return tostring(e)",
                "wrong number of arguments to 'insert'",
            ),
            (
                b"local ok,e=pcall(table.concat,{},'a',1,2,3,4); return tostring(e)",
                "invalid value (nil) at index 1 in table for 'concat'",
            ),
            (
                b"local ok,e=pcall(table.concat,{1,true,3}); return tostring(e)",
                "invalid value (boolean) at index 2 in table for 'concat'",
            ),
            (
                b"local ok,e=pcall(table.concat,{1,nil,3},'',1,3); return tostring(e)",
                "invalid value (nil) at index 2 in table for 'concat'",
            ),
        ];
        for (body, expected) in cases {
            let frame = eval_script(body, &[], &[], &mut store, 0).unwrap();
            let RespFrame::BulkString(Some(bytes)) = frame else {
                panic!("expected bulk for {:?}", String::from_utf8_lossy(body))
            };
            assert_eq!(
                String::from_utf8(bytes).unwrap(),
                *expected,
                "body={:?}",
                String::from_utf8_lossy(body)
            );
        }

        // Direct-call regressions: named/prefixed shape preserved.
        let err =
            eval_script(b"return table.insert({1,2,3})", &[], &[], &mut store, 0).unwrap_err();
        assert!(
            err.contains("user_script:1: wrong number of arguments to 'insert'"),
            "got {err:?}"
        );
        let err = eval_script(
            b"return table.concat({1,nil,3},'',1,3)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap_err();
        assert!(
            err.contains("user_script:1: invalid value (nil) at index 2 in table for 'concat'"),
            "got {err:?}"
        );
    }

    #[test]
    fn collectgarbage_error_pcall_shape_kx6jm() {
        // (frankenredis-kx6jm) collectgarbage's two error sites
        // (string-expected and invalid-option) now route through
        // lua_format_argerror so pcall(collectgarbage,…) emits the
        // anonymous-C-function shape.
        let mut store = Store::new();
        let cases: &[(&[u8], &str)] = &[
            (
                b"local ok,e=pcall(collectgarbage,'invalid'); return tostring(e)",
                "bad argument #1 to '?' (invalid option 'invalid')",
            ),
            (
                b"local ok,e=pcall(collectgarbage,'whatever'); return tostring(e)",
                "bad argument #1 to '?' (invalid option 'whatever')",
            ),
            (
                b"local ok,e=pcall(collectgarbage,true); return tostring(e)",
                "bad argument #1 to '?' (string expected, got boolean)",
            ),
            (
                b"local ok,e=pcall(collectgarbage,{}); return tostring(e)",
                "bad argument #1 to '?' (string expected, got table)",
            ),
        ];
        for (body, expected) in cases {
            let frame = eval_script(body, &[], &[], &mut store, 0).unwrap();
            let RespFrame::BulkString(Some(bytes)) = frame else {
                panic!("expected bulk for {:?}", String::from_utf8_lossy(body))
            };
            assert_eq!(
                String::from_utf8(bytes).unwrap(),
                *expected,
                "body={:?}",
                String::from_utf8_lossy(body)
            );
        }

        // Direct-call regression: named/prefixed shape preserved.
        let err =
            eval_script(b"return collectgarbage('invalid')", &[], &[], &mut store, 0).unwrap_err();
        assert!(
            err.contains(
                "user_script:1: bad argument #1 to 'collectgarbage' (invalid option 'invalid')"
            ),
            "got {err:?}"
        );

        // Sanity: known options still work.
        let frame =
            eval_script(b"return collectgarbage('count')", &[], &[], &mut store, 0).unwrap();
        assert!(matches!(frame, RespFrame::Integer(_)), "got {frame:?}");
    }

    #[test]
    fn math_random_and_attempt_to_call_errors_pcall_shape_o3epl() {
        // (frankenredis-o3epl) Three error sites now honor the dual
        // direct/pcall shape:
        //   1. math.random's 4 luaL_argerror calls (interval-empty
        //      and number-expected) via lua_format_argerror.
        //   2. math.random's wrong-arity luaL_error (no name, only
        //      prefix toggles).
        //   3. call_function fallback "attempt to call a TYPE value"
        //      for Nil/Thread/other when current_invocation_name is
        //      None (target invoked directly by pcall).
        let mut store = Store::new();
        let cases: &[(&[u8], &str)] = &[
            (
                b"local ok,e=pcall(math.random,0); return tostring(e)",
                "bad argument #1 to '?' (interval is empty)",
            ),
            (
                b"local ok,e=pcall(math.random,5,1); return tostring(e)",
                "bad argument #2 to '?' (interval is empty)",
            ),
            (
                b"local ok,e=pcall(math.random,'x'); return tostring(e)",
                "bad argument #1 to '?' (number expected, got string)",
            ),
            (
                b"local ok,e=pcall(math.random,1,'y'); return tostring(e)",
                "bad argument #2 to '?' (number expected, got string)",
            ),
            (
                b"local ok,e=pcall(math.random,1,2,3); return tostring(e)",
                "wrong number of arguments",
            ),
            (
                b"local ok,e=pcall(math.pi); return tostring(e)",
                "attempt to call a number value",
            ),
            (
                b"local ok,e=pcall(nil); return tostring(e)",
                "attempt to call a nil value",
            ),
            (
                b"local ok,e=pcall(true); return tostring(e)",
                "attempt to call a boolean value",
            ),
            (
                b"local ok,e=pcall('abc'); return tostring(e)",
                "attempt to call a string value",
            ),
            (
                b"local ok,e=pcall({}); return tostring(e)",
                "attempt to call a table value",
            ),
        ];
        for (body, expected) in cases {
            let frame = eval_script(body, &[], &[], &mut store, 0).unwrap();
            let RespFrame::BulkString(Some(bytes)) = frame else {
                panic!("expected bulk for {:?}", String::from_utf8_lossy(body))
            };
            assert_eq!(
                String::from_utf8(bytes).unwrap(),
                *expected,
                "body={:?}",
                String::from_utf8_lossy(body)
            );
        }

        // Direct-call regressions — named/prefixed shape preserved.
        let err = eval_script(b"return math.random(0)", &[], &[], &mut store, 0).unwrap_err();
        assert!(
            err.contains("user_script:1: bad argument #1 to 'random' (interval is empty)"),
            "got {err:?}"
        );
        let err = eval_script(b"return math.random(1,2,3)", &[], &[], &mut store, 0).unwrap_err();
        assert!(
            err.contains("user_script:1: wrong number of arguments"),
            "got {err:?}"
        );
        // Direct attempt-to-call: gets the field-aware wording (a
        // separate, longer-standing fr feature), not the bare wording.
        let err = eval_script(b"return math.pi()", &[], &[], &mut store, 0).unwrap_err();
        assert!(
            err.contains("attempt to call") && err.contains("number"),
            "got {err:?}"
        );
    }

    #[test]
    fn lua_pattern_errors_pcall_shape_drops_prefix_uqnq6() {
        // (frankenredis-uqnq6) Pattern-validation errors raised by
        // string.find/match (and the eager validation in gmatch/gsub)
        // must drop the user_script:1: prefix when the host C-builtin
        // is invoked directly by pcall, matching luaL_error's
        // luaL_where(L,1) behavior over a C pcall frame.
        let mut store = Store::new();
        let cases: &[(&[u8], &str)] = &[
            (
                b"local ok,e=pcall(string.find,'abc','('); return tostring(e)",
                "unfinished capture",
            ),
            (
                b"local ok,e=pcall(string.find,'abc','['); return tostring(e)",
                "malformed pattern (missing ']')",
            ),
            (
                b"local ok,e=pcall(string.find,'abc','%'); return tostring(e)",
                "malformed pattern (ends with '%')",
            ),
            (
                b"local ok,e=pcall(string.match,'abc','('); return tostring(e)",
                "unfinished capture",
            ),
            (
                b"local ok,e=pcall(string.match,'abc','%'); return tostring(e)",
                "malformed pattern (ends with '%')",
            ),
        ];
        for (body, expected) in cases {
            let frame = eval_script(body, &[], &[], &mut store, 0).unwrap();
            let RespFrame::BulkString(Some(bytes)) = frame else {
                panic!("expected bulk for {:?}", String::from_utf8_lossy(body))
            };
            assert_eq!(
                String::from_utf8(bytes).unwrap(),
                *expected,
                "body={:?}",
                String::from_utf8_lossy(body)
            );
        }

        // Direct-call regression: prefix preserved.
        let err =
            eval_script(b"return string.find('abc','(')", &[], &[], &mut store, 0).unwrap_err();
        assert!(
            err.contains("user_script:1: unfinished capture"),
            "got {err:?}"
        );

        // Lua-wrapped pcall: prefix reappears (Lua frame above C frame).
        let frame = eval_script(
            b"local f=function() return string.find('abc','(') end; local ok,e=pcall(f); return tostring(e)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        let RespFrame::BulkString(Some(bytes)) = frame else {
            panic!("expected bulk")
        };
        let s = String::from_utf8(bytes).unwrap();
        assert!(
            s.contains("user_script:1: unfinished capture"),
            "lua-wrapped pcall got {s:?}"
        );
    }

    #[test]
    fn redis_set_repl_and_setresp_errors_carry_err_prefix_lrrr4() {
        // (frankenredis-lrrr4) Upstream's luaPushError stamps "ERR "
        // on the err-table body for both redis.set_repl and redis.setresp
        // error paths (arity + value-out-of-range). fr previously
        // emitted bare wording, breaking pcall callers that compare
        // r.err prefixes.
        let mut store = Store::new();
        let cases: &[(&[u8], &str)] = &[
            (
                b"local ok,e=pcall(redis.set_repl); return tostring(e)",
                "ERR redis.set_repl() requires one argument.",
            ),
            (
                b"local ok,e=pcall(redis.set_repl,99); return tostring(e)",
                "ERR Invalid replication flags. Use REPL_AOF, REPL_REPLICA, REPL_ALL or REPL_NONE.",
            ),
            (
                b"local ok,e=pcall(redis.setresp); return tostring(e)",
                "ERR redis.setresp() requires one argument.",
            ),
            (
                b"local ok,e=pcall(redis.setresp,'x'); return tostring(e)",
                "ERR RESP version must be 2 or 3.",
            ),
            (
                b"local ok,e=pcall(redis.setresp,5); return tostring(e)",
                "ERR RESP version must be 2 or 3.",
            ),
        ];
        for (body, expected) in cases {
            let frame = eval_script(body, &[], &[], &mut store, 0).unwrap();
            let RespFrame::BulkString(Some(bytes)) = frame else {
                panic!("expected bulk for {:?}", String::from_utf8_lossy(body))
            };
            assert_eq!(
                String::from_utf8(bytes).unwrap(),
                *expected,
                "body={:?}",
                String::from_utf8_lossy(body)
            );
        }
    }

    #[test]
    fn string_gsub_validates_repl_type_upfront_tfob7() {
        // (frankenredis-tfob7) Upstream lstrlib.c:str_gsub runs
        // luaL_argcheck on the repl type at function entry, so passing
        // nil or boolean errors regardless of whether the pattern
        // matches. fr previously deferred the check to the per-match
        // dispatch — `string.gsub('a','b')` with no repl returned 'a'
        // because the pattern never matched and the type check never
        // fired.
        let mut store = Store::new();

        // pcall(C-builtin) shape: '?' name, no user_script:1: prefix.
        let cases: &[(&[u8], &str)] = &[
            (
                b"local ok,e=pcall(string.gsub,'a','b'); return tostring(e)",
                "bad argument #3 to '?' (string/function/table expected)",
            ),
            (
                b"local ok,e=pcall(string.gsub,'a','b',true); return tostring(e)",
                "bad argument #3 to '?' (string/function/table expected)",
            ),
            (
                b"local ok,e=pcall(string.gsub,'a','b',false); return tostring(e)",
                "bad argument #3 to '?' (string/function/table expected)",
            ),
            (
                b"local ok,e=pcall(string.gsub,'a','b',nil); return tostring(e)",
                "bad argument #3 to '?' (string/function/table expected)",
            ),
        ];
        for (body, expected) in cases {
            let frame = eval_script(body, &[], &[], &mut store, 0).unwrap();
            let RespFrame::BulkString(Some(bytes)) = frame else {
                panic!("expected bulk for {:?}", String::from_utf8_lossy(body))
            };
            assert_eq!(
                String::from_utf8(bytes).unwrap(),
                *expected,
                "body={:?}",
                String::from_utf8_lossy(body)
            );
        }

        // Direct-call (named/prefixed shape).
        let err = eval_script(b"return string.gsub('a','b')", &[], &[], &mut store, 0).unwrap_err();
        assert!(
            err.contains(
                "user_script:1: bad argument #3 to 'gsub' (string/function/table expected)"
            ),
            "got {err:?}"
        );

        // Regression: valid repls still work — at script top-level
        // Lua returns only the first value (the result string).
        let frame = eval_script(
            b"return string.gsub('aaa','a','x')",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"xxx".to_vec())));

        // Number repl is accepted (coerced to string).
        let frame =
            eval_script(b"return string.gsub('aaa','a',1)", &[], &[], &mut store, 0).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"111".to_vec())));

        // Multi-return reaches inner Lua code.
        let frame = eval_script(
            b"local s,n=string.gsub('aaa','a','x'); return s..'-'..n",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"xxx-3".to_vec())));
    }

    #[test]
    fn string_char_and_math_min_max_pcall_shape_drops_prefix_ymb2q() {
        // (frankenredis-ymb2q) Same dual-shape pattern as frankenredis-fllxr
        // — pcall directly invoking a C-builtin yields '?' as the function
        // name and drops the user_script:1: prefix. The three handlers
        // string.char, math.max, math.min previously hardcoded the named
        // shape and now route through lua_format_argerror.
        let mut store = Store::new();

        let cases: &[(&[u8], &str)] = &[
            (
                b"local ok,e=pcall(string.char,'x'); return tostring(e)",
                "bad argument #1 to '?' (number expected, got string)",
            ),
            (
                b"local ok,e=pcall(string.char,300); return tostring(e)",
                "bad argument #1 to '?' (invalid value)",
            ),
            (
                b"local ok,e=pcall(string.char,-1); return tostring(e)",
                "bad argument #1 to '?' (invalid value)",
            ),
            (
                b"local ok,e=pcall(math.max); return tostring(e)",
                "bad argument #1 to '?' (number expected, got no value)",
            ),
            (
                b"local ok,e=pcall(math.min); return tostring(e)",
                "bad argument #1 to '?' (number expected, got no value)",
            ),
        ];

        for (body, expected) in cases {
            let frame = eval_script(body, &[], &[], &mut store, 0).unwrap();
            let RespFrame::BulkString(Some(bytes)) = frame else {
                panic!("expected bulk for {:?}", String::from_utf8_lossy(body))
            };
            assert_eq!(
                String::from_utf8(bytes).unwrap(),
                *expected,
                "body={:?}",
                String::from_utf8_lossy(body)
            );
        }

        // Direct-call regressions — named/prefixed shape preserved.
        let err = eval_script(b"return string.char(300)", &[], &[], &mut store, 0).unwrap_err();
        assert!(
            err.contains("user_script:1: bad argument #1 to 'char' (invalid value)"),
            "got {err:?}"
        );
        let err = eval_script(b"return math.max()", &[], &[], &mut store, 0).unwrap_err();
        assert!(
            err.contains("user_script:1: bad argument #1 to 'max' (number expected, got no value)"),
            "got {err:?}"
        );
        let err = eval_script(b"return math.min()", &[], &[], &mut store, 0).unwrap_err();
        assert!(
            err.contains("user_script:1: bad argument #1 to 'min' (number expected, got no value)"),
            "got {err:?}"
        );
    }

    #[test]
    fn string_format_pcall_shape_drops_prefix_and_uses_anonymous_name_fllxr() {
        // (frankenredis-fllxr) Upstream's luaL_argerror uses lua_getinfo
        // "n.name" which returns NULL for C-functions invoked directly
        // by pcall (no Lua-side caller frame above), yielding `'?'` as
        // the function name and dropping the source:line prefix. The
        // luaL_error path (invalid option) keeps the 'format' name but
        // still drops the prefix because luaL_where(L,1) over a C
        // pcall frame returns an empty string.
        let mut store = Store::new();

        let cases: &[(&[u8], &str)] = &[
            (
                b"local ok,e=pcall(string.format,'%d','hi'); return tostring(e)",
                "bad argument #2 to '?' (number expected, got string)",
            ),
            (
                b"local ok,e=pcall(string.format,'%q',nil); return tostring(e)",
                "bad argument #2 to '?' (string expected, got nil)",
            ),
            (
                b"local ok,e=pcall(string.format,'%s'); return tostring(e)",
                "bad argument #2 to '?' (no value)",
            ),
            (
                b"local ok,e=pcall(string.format,'%d'); return tostring(e)",
                "bad argument #2 to '?' (no value)",
            ),
            (
                b"local ok,e=pcall(string.format,nil); return tostring(e)",
                "bad argument #1 to '?' (string expected, got nil)",
            ),
            (
                b"local ok,e=pcall(string.format,true); return tostring(e)",
                "bad argument #1 to '?' (string expected, got boolean)",
            ),
            (
                b"local ok,e=pcall(string.format,{}); return tostring(e)",
                "bad argument #1 to '?' (string expected, got table)",
            ),
            (
                b"local ok,e=pcall(string.format); return tostring(e)",
                "bad argument #1 to '?' (string expected, got no value)",
            ),
            (
                b"local ok,e=pcall(string.format,'%'); return tostring(e)",
                "bad argument #2 to '?' (no value)",
            ),
            (
                b"local ok,e=pcall(string.format,'%K',5); return tostring(e)",
                "invalid option '%K' to 'format'",
            ),
        ];

        for (body, expected) in cases {
            let frame = eval_script(body, &[], &[], &mut store, 0).unwrap();
            let RespFrame::BulkString(Some(bytes)) = frame else {
                panic!("expected bulk for {:?}", String::from_utf8_lossy(body))
            };
            assert_eq!(
                String::from_utf8(bytes).unwrap(),
                *expected,
                "body={:?}",
                String::from_utf8_lossy(body)
            );
        }

        // Regression: direct calls (named-shape) still match the
        // user_script:1: bad argument #N to 'format' (...) wording.
        let err =
            eval_script(b"return string.format('%d','hi')", &[], &[], &mut store, 0).unwrap_err();
        assert!(
            err.contains(
                "user_script:1: bad argument #2 to 'format' (number expected, got string)"
            ),
            "direct call got {err:?}"
        );
        let err =
            eval_script(b"return string.format('%K',5)", &[], &[], &mut store, 0).unwrap_err();
        assert!(
            err.contains("user_script:1: invalid option '%K' to 'format'"),
            "direct call got {err:?}"
        );

        // Lua-wrapped pcall: function wraps the call, so the C-builtin
        // is one frame deep — the prefix should reappear.
        let frame = eval_script(
            b"local f=function() return string.format('%d','hi') end; local ok,e=pcall(f); return tostring(e)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        let RespFrame::BulkString(Some(bytes)) = frame else {
            panic!("expected bulk")
        };
        let s = String::from_utf8(bytes).unwrap();
        assert!(
            s.contains("user_script:1: bad argument #2 to 'format' (number expected, got string)"),
            "lua-wrapped pcall got {s:?}"
        );
    }

    #[test]
    fn redis_call_arg_validation_errors_carry_err_prefix_and_pcall_packages_sj52g() {
        // (frankenredis-sj52g) Upstream luaRedisGenericCommand routes
        // both the empty-args branch and the per-arg lua_tolstring
        // rejection through luaPushError, which prepends "ERR " and
        // also packages the error into a `{err = STR}` table for the
        // is_pcall path. fr previously returned bare strings and
        // bypassed the table-form entirely for arg-validation failures.
        let mut store = Store::new();

        // redis.call → pcall-caught error: should carry ERR prefix.
        for body in &[
            b"local ok,e=pcall(redis.call); return tostring(e)".as_slice(),
            b"local ok,e=pcall(redis.call,'SET','k',true); return tostring(e)",
            b"local ok,e=pcall(redis.call,'SET','k',nil); return tostring(e)",
            b"local ok,e=pcall(redis.call,'SET','k',{}); return tostring(e)",
        ] {
            let frame = eval_script(body, &[], &[], &mut store, 0).unwrap();
            let RespFrame::BulkString(Some(bytes)) = frame else {
                panic!("expected bulk for {:?}", String::from_utf8_lossy(body))
            };
            let s = String::from_utf8(bytes).unwrap();
            assert!(
                s.starts_with("ERR "),
                "body={:?} got {s:?}",
                String::from_utf8_lossy(body)
            );
            assert!(
                s.contains("Please specify at least one argument for this redis lib call")
                    || s.contains("Lua redis lib command arguments must be strings or integers"),
                "body={:?} got {s:?}",
                String::from_utf8_lossy(body)
            );
        }

        // redis.pcall arg-validation → packaged into r.err table form.
        let frame = eval_script(
            b"local r=redis.pcall(); return tostring(r.err)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(
            frame,
            RespFrame::BulkString(Some(
                b"ERR Please specify at least one argument for this redis lib call".to_vec()
            ))
        );

        let frame = eval_script(
            b"local r=redis.pcall('SET','k',true); return tostring(r.err)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(
            frame,
            RespFrame::BulkString(Some(
                b"ERR Lua redis lib command arguments must be strings or integers".to_vec()
            ))
        );

        let frame = eval_script(
            b"local r=redis.pcall('SET','k',{}); return tostring(r.err)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(
            frame,
            RespFrame::BulkString(Some(
                b"ERR Lua redis lib command arguments must be strings or integers".to_vec()
            ))
        );

        // redis.call direct (no pcall) should still surface the error
        // at top-level with the ERR prefix preserved.
        let err = eval_script(
            b"return redis.call('SET','k',true)",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect_err("expected error");
        assert!(err.starts_with("ERR "), "got {err:?}");
        assert!(
            err.contains("Lua redis lib command arguments must be strings or integers"),
            "got {err:?}"
        );
    }

    #[test]
    fn redis_acl_check_cmd_coerces_numbers_and_prefixes_err_vqjp9() {
        // (frankenredis-vqjp9) Upstream script_lua.c::luaRedisAclCheckCmdCommand
        // accepts numeric args (lua_tolstring coerces "123" → "123" and
        // "3.14" → "3.14") and looks them up as command names; only
        // bool/nil/table arguments hit the type-rejection branch. All
        // three error strings carry the "ERR " prefix (luaPushError on
        // upstream stores the prefix in the err table's err field).
        let mut store = Store::new();

        // Numeric coercion path: 123 should be coerced to "123" and
        // rejected as unknown command — NOT as a type error.
        let caught = eval_script(
            b"local ok,err=pcall(redis.acl_check_cmd,123); return tostring(err)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        let RespFrame::BulkString(Some(bytes)) = caught else {
            panic!("expected bulk")
        };
        let s = String::from_utf8(bytes).unwrap();
        assert_eq!(s, "ERR Invalid command passed to redis.acl_check_cmd()");

        // Negative integer coerces to "-5" → unknown.
        let caught = eval_script(
            b"local ok,err=pcall(redis.acl_check_cmd,-5); return tostring(err)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        let RespFrame::BulkString(Some(bytes)) = caught else {
            panic!("expected bulk")
        };
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "ERR Invalid command passed to redis.acl_check_cmd()"
        );

        // Float with fraction coerces to "3.14" → unknown.
        let caught = eval_script(
            b"local ok,err=pcall(redis.acl_check_cmd,3.14); return tostring(err)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        let RespFrame::BulkString(Some(bytes)) = caught else {
            panic!("expected bulk")
        };
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "ERR Invalid command passed to redis.acl_check_cmd()"
        );

        // String unknown command — same error.
        let caught = eval_script(
            b"local ok,err=pcall(redis.acl_check_cmd,'NOTACMD'); return tostring(err)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        let RespFrame::BulkString(Some(bytes)) = caught else {
            panic!("expected bulk")
        };
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "ERR Invalid command passed to redis.acl_check_cmd()"
        );

        // bool/nil/table: type rejection with ERR prefix.
        for body in &[
            b"local ok,err=pcall(redis.acl_check_cmd,true); return tostring(err)".as_slice(),
            b"local ok,err=pcall(redis.acl_check_cmd,false); return tostring(err)",
            b"local ok,err=pcall(redis.acl_check_cmd,nil); return tostring(err)",
            b"local ok,err=pcall(redis.acl_check_cmd,{}); return tostring(err)",
        ] {
            let caught = eval_script(body, &[], &[], &mut store, 0).unwrap();
            let RespFrame::BulkString(Some(bytes)) = caught else {
                panic!("expected bulk for body={:?}", String::from_utf8_lossy(body))
            };
            assert_eq!(
                String::from_utf8(bytes).unwrap(),
                "ERR Lua redis lib command arguments must be strings or integers",
                "body={:?}",
                String::from_utf8_lossy(body)
            );
        }

        // Empty args: ERR prefix.
        let caught = eval_script(
            b"local ok,err=pcall(redis.acl_check_cmd); return tostring(err)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        let RespFrame::BulkString(Some(bytes)) = caught else {
            panic!("expected bulk")
        };
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "ERR Please specify at least one argument for this redis lib call"
        );

        // Sanity: known command returns true.
        let ok = eval_script(
            b"return redis.acl_check_cmd('GET')",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(ok, RespFrame::Integer(1));
    }

    #[test]
    fn lua_error_unwraps_table_with_err_string_field_vkqn0() {
        // (frankenredis-vkqn0) Upstream Redis hooks Lua's error() so
        // that a `{err = STRING}` table is unwrapped to the bare err
        // field — both pcall and the top-level reply path see the
        // string, not the table. The top-level reply also skips the
        // "ERR " auto-prefix (the err field IS the complete body).
        let mut store = Store::new();

        // pcall: the second return value should be the string, not the table.
        let frame = eval_script(
            b"local ok,e = pcall(error, {err='x'}); return type(e)..'-'..tostring(e)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"string-x".to_vec())));

        // pcall with extra fields: still unwraps via the err field.
        let frame = eval_script(
            b"local ok,e = pcall(error, {err='x', foo='y'}); return type(e)..'-'..tostring(e)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"string-x".to_vec())));

        // pcall without an err field: NO unwrap, the table stays.
        let frame = eval_script(
            b"local ok,e = pcall(error, {foo='bar'}); return type(e)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"table".to_vec())));

        // pcall with a non-string err field: NO unwrap.
        let frame = eval_script(
            b"local ok,e = pcall(error, {err=42}); return type(e)",
            &[],
            &[],
            &mut store,
            0,
        )
        .unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"table".to_vec())));

        // Existing typed-error regressions still pass.
        for body in &[
            (
                b"local ok,e = pcall(error, 42); return type(e)..'-'..tostring(e)".as_slice(),
                "string-42",
            ),
            (
                b"local ok,e = pcall(error, true); return type(e)..'-'..tostring(e)",
                "boolean-true",
            ),
            (
                b"local ok,e = pcall(error, nil); return type(e)..'-'..tostring(e)",
                "nil-nil",
            ),
        ] {
            let frame = eval_script(body.0, &[], &[], &mut store, 0).unwrap();
            assert_eq!(
                frame,
                RespFrame::BulkString(Some(body.1.as_bytes().to_vec())),
                "src = {:?}",
                String::from_utf8_lossy(body.0)
            );
        }

        // Direct uncaught error({err=string}) emits Err(marker + body).
        // The fr-command::lib eval_script_error_reply wrapper strips
        // the marker and uses the body verbatim — covered in the
        // dispatch_argv integration tests below.
    }

    #[test]
    fn lua_arithmetic_coerces_hex_string_literals_83zqp() {
        // (frankenredis-83zqp) Lua 5.1's lua_tonumber funnels through
        // strtod, which accepts C99 hex floats. fr previously rejected
        // arithmetic on hex string literals because to_number used
        // Rust's f64::FromStr (decimal-only).
        let mut store = Store::new();

        // Hex int: 0x10 == 16, + 1 = 17.
        let frame = eval_script(b"return '0x10' + 1", &[], &[], &mut store, 0).unwrap();
        assert_eq!(frame, RespFrame::Integer(17));

        // Hex with binary exponent: 0x1.8p2 == 1.5 * 4 = 6, + 1 = 7.
        let frame = eval_script(b"return '0x1.8p2' + 1", &[], &[], &mut store, 0).unwrap();
        assert_eq!(frame, RespFrame::Integer(7));

        // Hex int with leading/trailing whitespace.
        let frame = eval_script(b"return '  0x10  ' + 1", &[], &[], &mut store, 0).unwrap();
        assert_eq!(frame, RespFrame::Integer(17));

        // Decimal still works (regression).
        let frame = eval_script(b"return '1e5' + 1", &[], &[], &mut store, 0).unwrap();
        assert_eq!(frame, RespFrame::Integer(100001));
        let frame = eval_script(b"return '5' + 1", &[], &[], &mut store, 0).unwrap();
        assert_eq!(frame, RespFrame::Integer(6));

        // Invalid still errors.
        let err = eval_script(b"return '5x' + 1", &[], &[], &mut store, 0).unwrap_err();
        assert!(
            err.contains("attempt to perform arithmetic on a string value"),
            "got {err:?}"
        );

        // tonumber on hex literals also works (it routes through the
        // same to_number path).
        let frame = eval_script(b"return tonumber('0x10')", &[], &[], &mut store, 0).unwrap();
        assert_eq!(frame, RespFrame::Integer(16));
    }
}
