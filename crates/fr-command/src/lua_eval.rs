// Minimal Lua 5.1 evaluator for Redis scripting.
//
// Supports: variables (local/global), arithmetic, string concat, comparisons,
// logical ops, if/elseif/else, numeric for, generic for (pairs/ipairs),
// while, repeat/until, tables, function calls/definitions, redis.call/pcall,
// KEYS/ARGV, and standard library functions.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Instant;

use fr_protocol::RespFrame;
use fr_store::{SCRIPT_PROPAGATE_ALL, SCRIPT_PROPAGATE_AOF, SCRIPT_PROPAGATE_REPLICA, Store};

use crate::{CommandError, SCRIPT_NOSCRIPT_ERROR, dispatch_argv, parse_i64_arg};

// ── Value type ──────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub enum LuaValue {
    Nil,
    Bool(bool),
    Number(f64),
    Str(Vec<u8>),
    Table(LuaTable),
    Function(LuaFunc),
    RustFunction(String), // name of built-in
    Coroutine(LuaCoroutine),
    WrappedCoroutine(LuaCoroutine),
}

#[derive(Clone, Debug)]
pub struct LuaTable {
    pub inner: Rc<RefCell<LuaTableInner>>,
}

#[derive(Clone, Debug)]
pub struct LuaTableInner {
    pub array: Vec<LuaValue>,
    pub string_hash: HashMap<Vec<u8>, LuaValue>,
    pub other_hash: Vec<(LuaValue, LuaValue)>,
    /// Set of keys in `other_hash` for fast O(1) existence checks.
    pub other_keys: HashSet<LuaHashKey>,
    /// Optional metatable for Lua 5.1 metamethods.
    pub metatable: Option<LuaTable>,
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
            LuaValue::Function(_) => "func".hash(state),
            LuaValue::RustFunction(n) => n.hash(state),
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

#[derive(Clone, Debug)]
pub struct LuaFunc {
    pub params: Vec<String>,
    pub body: Vec<Stmt>,
    pub is_variadic: bool,
    /// Captured lexical environment (upvalues) from function definition site.
    pub captured_env: Option<Vec<HashMap<String, Rc<RefCell<LuaValue>>>>>,
    /// For `local function f(x) ... end`, stores the name so the function
    /// can be injected into its own call scope for self-recursion.
    pub self_name: Option<String>,
    /// Source-location label this function uses for runtime error
    /// prefixes. Set by `loadstring`/`load` so chunk errors carry the
    /// chunk's name (e.g. `[string "src"]:1:`) instead of the outer
    /// script's `user_script:1:`. None means the function inherits the
    /// outer script's prefix. (frankenredis-ycaog)
    pub source_label: Option<String>,
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
        Self {
            inner: Rc::new(RefCell::new(LuaTableInner {
                array: Vec::new(),
                string_hash: HashMap::new(),
                other_hash: Vec::new(),
                other_keys: HashSet::new(),
                metatable: None,
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
    fn len(&self) -> usize {
        self.inner.borrow().len()
    }
    fn hash_pairs(&self) -> Vec<(LuaValue, LuaValue)> {
        self.inner.borrow().hash_pairs()
    }
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
}

/// Format the chunk-name prefix Lua 5.1 wraps around loadstring/load
/// parse errors. Mirrors luaO_chunkid in lobject.c:
///   - chunkname starts with '=' → strip the '=' and use as a literal label
///   - chunkname starts with '@' → strip the '@' and use as a literal label
///     (upstream uses this for filenames)
///   - any other chunkname → wrap in `[string "NAME"]`
///   - no chunkname → use the first line of the source, truncated with
///     `"..."` if the source spans multiple lines, wrapped in `[string "..."]`
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
        _ => false,
    }
}

/// (frankenredis-kqd16) Upstream luaL_argerror prepends "user_script:1: "
/// (the source-location prefix) before the argument template. Use the
/// got-label helper so missing args report "got no value" while explicit
/// LuaValue::Nil reports "got nil" — distinct upstream wordings.
fn lua_bad_table_arg(function: &str, index: usize, value: Option<&LuaValue>) -> String {
    format!(
        "user_script:1: bad argument #{index} to '{function}' (table expected, got {})",
        lua_arg_got_label(value)
    )
}

fn lua_bad_number_arg(function: &str, index: usize, value: Option<&LuaValue>) -> String {
    format!(
        "user_script:1: bad argument #{index} to '{function}' (number expected, got {})",
        lua_arg_got_label(value)
    )
}

fn lua_table_arg<'a>(
    function: &str,
    index: usize,
    value: Option<&'a LuaValue>,
) -> Result<&'a LuaTable, String> {
    match value {
        Some(LuaValue::Table(table)) => Ok(table),
        _ => Err(lua_bad_table_arg(function, index, value)),
    }
}

fn lua_required_integer_arg(function: &str, index: usize, value: &LuaValue) -> Result<i64, String> {
    match value.to_number() {
        Some(number) if number.is_finite() => Ok(number as i64),
        _ => Err(lua_bad_number_arg(function, index, Some(value))),
    }
}

fn lua_optional_integer_arg(
    function: &str,
    index: usize,
    value: Option<&LuaValue>,
    default: i64,
) -> Result<i64, String> {
    match value {
        None | Some(LuaValue::Nil) => Ok(default),
        Some(value) => lua_required_integer_arg(function, index, value),
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
        LuaValue::Number(n) if n.is_nan() => {
            Err("user_script:1: table index is NaN".to_string())
        }
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
fn lua_check_number(args: &[LuaValue], idx: usize, fname: &str) -> Result<f64, String> {
    let arg = args.get(idx);
    if let Some(v) = arg
        && let Some(n) = v.to_number()
    {
        return Ok(n);
    }
    Err(format!(
        "user_script:1: bad argument #{} to '{fname}' (number expected, got {})",
        idx + 1,
        lua_arg_got_label(arg)
    ))
}

// Mirror upstream luaL_checklstring. Numbers are coerced to their
// luaO_str2d-formatted string; everything else (nil, bool, table, function,
// thread) raises with the standard 'string expected, got <type>' wording.
fn lua_check_string(args: &[LuaValue], idx: usize, fname: &str) -> Result<Vec<u8>, String> {
    match args.get(idx) {
        Some(LuaValue::Str(b)) => Ok(b.clone()),
        Some(LuaValue::Number(n)) => {
            if *n == (*n as i64) as f64 && n.is_finite() {
                Ok(format!("{}", *n as i64).into_bytes())
            } else {
                Ok(lua_number_to_string(*n).into_bytes())
            }
        }
        other => Err(format!(
            "user_script:1: bad argument #{} to '{fname}' (string expected, got {})",
            idx + 1,
            lua_arg_got_label(other)
        )),
    }
}

// Mirror upstream luaL_checktype(L, idx, LUA_TTABLE). Returns a cloned
// LuaTable handle (refcounted) so the caller can borrow without lifetime
// shenanigans.
fn lua_check_table(args: &[LuaValue], idx: usize, fname: &str) -> Result<LuaTable, String> {
    match args.get(idx) {
        Some(LuaValue::Table(t)) => Ok(t.clone()),
        other => Err(format!(
            "user_script:1: bad argument #{} to '{fname}' (table expected, got {})",
            idx + 1,
            lua_arg_got_label(other)
        )),
    }
}

impl LuaValue {
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
            LuaValue::Coroutine(_) => "thread",
            LuaValue::WrappedCoroutine(_) => "function",
        }
    }

    fn to_number(&self) -> Option<f64> {
        match self {
            LuaValue::Number(n) => Some(*n),
            LuaValue::Str(s) => {
                let s = std::str::from_utf8(s).ok()?;
                s.trim().parse::<f64>().ok()
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
                    Ok(format!("{}", *n as i64).into_bytes())
                } else {
                    Ok(format!("{n}").into_bytes())
                }
            }
            LuaValue::Str(s) => Ok(s.clone()),
            _ => Err(
                "Lua redis lib command arguments must be strings or integers".to_string(),
            ),
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
                if !is_neg_zero
                    && !needs_scientific
                    && *n == (*n as i64) as f64
                    && n.is_finite()
                {
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
        Expr::Name(n) => n.clone(),
        Expr::VarArgs => "...".to_string(),
        Expr::Call(_, _) | Expr::MethodCall(_, _, _) => "()".to_string(),
        Expr::TableConstructor(_) => "{".to_string(),
        Expr::FunctionDef(_, _, _) => "function".to_string(),
        Expr::BinOp(left, _, _) | Expr::UnaryOp(_, left) => lua_lvalue_first_token(left),
        Expr::Index(left, _) | Expr::Field(left, _) => lua_lvalue_first_token(left),
    }
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

    fn skip_whitespace_and_comments(&mut self) {
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
                // Check for long comment --[[ ... ]]
                if self.pos + 1 < self.src.len()
                    && self.src[self.pos] == b'['
                    && self.src[self.pos + 1] == b'['
                {
                    self.pos += 2;
                    while self.pos + 1 < self.src.len() {
                        if self.src[self.pos] == b']' && self.src[self.pos + 1] == b']' {
                            self.pos += 2;
                            break;
                        }
                        self.pos += 1;
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
    }

    fn read_string(&mut self, delim: u8) -> Result<Vec<u8>, String> {
        let mut buf = Vec::new();
        loop {
            let Some(b) = self.advance() else {
                return Err("unterminated string".to_string());
            };
            if b == delim {
                return Ok(buf);
            }
            if b == b'\\' {
                let Some(esc) = self.advance() else {
                    return Err("unterminated escape".to_string());
                };
                match esc {
                    b'n' => buf.push(b'\n'),
                    b't' => buf.push(b'\t'),
                    b'r' => buf.push(b'\r'),
                    b'\\' => buf.push(b'\\'),
                    b'\'' => buf.push(b'\''),
                    b'"' => buf.push(b'"'),
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
                        buf.push(num as u8);
                    }
                    _ => {
                        buf.push(b'\\');
                        buf.push(esc);
                    }
                }
            } else {
                buf.push(b);
            }
        }
    }

    fn read_long_string(&mut self) -> Result<Vec<u8>, String> {
        // Already consumed [[
        let mut buf = Vec::new();
        // Skip first newline if present
        if self.peek_byte() == Some(b'\n') {
            self.pos += 1;
        }
        loop {
            if self.pos + 1 < self.src.len()
                && self.src[self.pos] == b']'
                && self.src[self.pos + 1] == b']'
            {
                self.pos += 2;
                return Ok(buf);
            }
            let Some(b) = self.advance() else {
                return Err("unterminated long string".to_string());
            };
            buf.push(b);
        }
    }

    fn next_token(&mut self) -> Result<Token, String> {
        self.skip_whitespace_and_comments();
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
                    let s = std::str::from_utf8(&self.src[start..self.pos])
                        .map_err(|_| "invalid number")?;
                    if s.len() <= 2 {
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
                let s = std::str::from_utf8(&self.src[start..self.pos])
                    .map_err(|_| "invalid number")?;
                let n = s.parse::<f64>().map_err(|e| e.to_string())?;
                Ok(Token::Number(n))
            }
            b'"' | b'\'' => {
                self.pos += 1;
                let s = self.read_string(b)?;
                Ok(Token::Str(s))
            }
            b'[' if self.pos + 1 < self.src.len() && self.src[self.pos + 1] == b'[' => {
                self.pos += 2;
                let s = self.read_long_string()?;
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

    fn tokenize_all(&mut self) -> Result<Vec<Token>, String> {
        let mut tokens = Vec::new();
        loop {
            let tok = self.next_token()?;
            if tok == Token::Eof {
                tokens.push(Token::Eof);
                break;
            }
            tokens.push(tok);
        }
        Ok(tokens)
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
    VarArgs,
    BinOp(Box<Expr>, BinOp, Box<Expr>),
    UnaryOp(UnaryOp, Box<Expr>),
    Index(Box<Expr>, Box<Expr>),
    Field(Box<Expr>, String),
    Call(Box<Expr>, Vec<Expr>),
    MethodCall(Box<Expr>, String, Vec<Expr>),
    TableConstructor(Vec<TableField>),
    FunctionDef(Vec<String>, bool, Vec<Stmt>),
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
    If(Vec<(Expr, Vec<Stmt>)>, Option<Vec<Stmt>>),
    NumericFor(String, Expr, Expr, Option<Expr>, Vec<Stmt>),
    GenericFor(Vec<String>, Vec<Expr>, Vec<Stmt>),
    While(Expr, Vec<Stmt>),
    Repeat(Vec<Stmt>, Expr),
    DoBlock(Vec<Stmt>),
    Return(Vec<Expr>),
    Break,
    FunctionDecl(Vec<String>, Vec<String>, bool, Vec<Stmt>),
    LocalFunctionDecl(String, Vec<String>, bool, Vec<Stmt>),
}

// ── Parser ──────────────────────────────────────────────────────────────

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, pos: 0 }
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
            Err(format!("expected {expected:?}, got {tok:?}"))
        }
    }

    fn check(&self, expected: &Token) -> bool {
        std::mem::discriminant(self.peek()) == std::mem::discriminant(expected)
    }

    fn parse_block(&mut self) -> Result<Vec<Stmt>, String> {
        let mut stmts = Vec::new();
        loop {
            // Skip semicolons
            while self.check(&Token::Semi) {
                self.advance();
            }
            match self.peek() {
                Token::End | Token::Else | Token::ElseIf | Token::Until | Token::Eof => break,
                _ => {
                    let stmt = self.parse_statement()?;
                    stmts.push(stmt);
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
                self.advance();
                let body = self.parse_block()?;
                self.expect(&Token::End)?;
                Ok(Stmt::DoBlock(body))
            }
            Token::Local => self.parse_local(),
            Token::Return => self.parse_return(),
            Token::Break => {
                self.advance();
                Ok(Stmt::Break)
            }
            Token::Function => self.parse_function_decl(),
            _ => self.parse_expr_or_assign(),
        }
    }

    fn parse_if(&mut self) -> Result<Stmt, String> {
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
        self.expect(&Token::End)?;
        Ok(Stmt::If(branches, else_body))
    }

    fn parse_while(&mut self) -> Result<Stmt, String> {
        self.advance(); // 'while'
        let cond = self.parse_expr()?;
        self.expect(&Token::Do)?;
        let body = self.parse_block()?;
        self.expect(&Token::End)?;
        Ok(Stmt::While(cond, body))
    }

    fn parse_repeat(&mut self) -> Result<Stmt, String> {
        self.advance(); // 'repeat'
        let body = self.parse_block()?;
        self.expect(&Token::Until)?;
        let cond = self.parse_expr()?;
        Ok(Stmt::Repeat(body, cond))
    }

    fn parse_for(&mut self) -> Result<Stmt, String> {
        self.advance(); // 'for'
        let name = match self.advance() {
            Token::Name(n) => n,
            t => return Err(format!("expected name in for, got {t:?}")),
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
            let body = self.parse_block()?;
            self.expect(&Token::End)?;
            Ok(Stmt::NumericFor(name, start, stop, step, body))
        } else {
            // Generic for: for name [, name ...] in explist do ... end
            let mut names = vec![name];
            while self.check(&Token::Comma) {
                self.advance();
                match self.advance() {
                    Token::Name(n) => names.push(n),
                    t => return Err(format!("expected name in for, got {t:?}")),
                }
            }
            self.expect(&Token::In)?;
            let exprs = self.parse_expr_list()?;
            self.expect(&Token::Do)?;
            let body = self.parse_block()?;
            self.expect(&Token::End)?;
            Ok(Stmt::GenericFor(names, exprs, body))
        }
    }

    fn parse_local(&mut self) -> Result<Stmt, String> {
        self.advance(); // 'local'
        if self.check(&Token::Function) {
            self.advance(); // 'function'
            let name = match self.advance() {
                Token::Name(n) => n,
                t => return Err(format!("expected function name, got {t:?}")),
            };
            let (params, is_variadic, body) = self.parse_func_body()?;
            return Ok(Stmt::LocalFunctionDecl(name, params, is_variadic, body));
        }

        let mut names = Vec::new();
        match self.advance() {
            Token::Name(n) => names.push(n),
            t => return Err(format!("expected name after local, got {t:?}")),
        }
        while self.check(&Token::Comma) {
            self.advance();
            match self.advance() {
                Token::Name(n) => names.push(n),
                t => return Err(format!("expected name, got {t:?}")),
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
        self.advance(); // 'function'
        let mut names = Vec::new();
        match self.advance() {
            Token::Name(n) => names.push(n),
            t => return Err(format!("expected function name, got {t:?}")),
        }
        while self.check(&Token::Dot) {
            self.advance();
            match self.advance() {
                Token::Name(n) => names.push(n),
                t => return Err(format!("expected name after '.', got {t:?}")),
            }
        }
        let (params, is_variadic, body) = self.parse_func_body()?;
        Ok(Stmt::FunctionDecl(names, params, is_variadic, body))
    }

    fn parse_func_body(&mut self) -> Result<(Vec<String>, bool, Vec<Stmt>), String> {
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
                    t => return Err(format!("expected parameter name, got {t:?}")),
                }
                if !self.check(&Token::Comma) {
                    break;
                }
                self.advance();
            }
        }
        self.expect(&Token::RParen)?;
        let body = self.parse_block()?;
        self.expect(&Token::End)?;
        Ok((params, is_variadic, body))
    }

    fn parse_expr_or_assign(&mut self) -> Result<Stmt, String> {
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
                    Expr::Name(_) | Expr::Index(_, _) | Expr::Field(_, _)
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
                _ => Err(format!("'=' expected near '{}'", token_display(self.peek()))),
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
        let mut expr = self.parse_primary()?;
        loop {
            match self.peek().clone() {
                Token::Dot => {
                    self.advance();
                    match self.advance() {
                        Token::Name(n) => expr = Expr::Field(Box::new(expr), n),
                        t => return Err(format!("expected field name, got {t:?}")),
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
                        t => return Err(format!("expected method name, got {t:?}")),
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
            _ => Err("expected function arguments".to_string()),
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
                self.advance();
                let (params, is_variadic, body) = self.parse_func_body()?;
                Ok(Expr::FunctionDef(params, is_variadic, body))
            }
            Token::Dots => {
                self.advance();
                Ok(Expr::VarArgs)
            }
            t => Err(format!("unexpected token in expression: {t:?}")),
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
    globals: HashMap<String, LuaValue>,
    /// (frankenredis-j02x9) Set to true at the start of execute(); once
    /// locked, any write to a top-level global raises "Attempt to modify
    /// a readonly table" and any read of an undefined global raises
    /// "Script attempted to access nonexistent global variable 'NAME'".
    /// Mirrors upstream script_lua.c::luaSetTableProtectionRecursively
    /// applied to the globals table after init, plus the
    /// luaProtectedTableError __index handler.
    globals_locked: bool,
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
    /// Depth of nested exec_stmts calls. The coroutine resume path
    /// (exec_coroutine_stmts → exec_stmt) keeps this at 0 for top-
    /// level body statements; any nested control-flow block (for,
    /// while, repeat, if-then, function-call body) bumps it via
    /// exec_stmts. coroutine.yield inspects it: yielding from a
    /// nested scope cannot be resumed by bw15's outer-stmt PC
    /// tracking, so the yield must error rather than silently drop
    /// iterations on resume. (frankenredis-ztawj)
    nested_exec_stmts_depth: usize,
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

#[derive(Clone, Debug)]
struct Scope {
    locals: HashMap<String, Rc<RefCell<LuaValue>>>,
}

impl Scope {
    fn new() -> Self {
        Self {
            locals: HashMap::new(),
        }
    }
}

#[derive(Clone, Debug)]
struct Env {
    scopes: Vec<Scope>,
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
            local_floor: 0,
        }
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
            scope
                .locals
                .insert(name.to_string(), Rc::new(RefCell::new(value)));
        }
    }

    fn get_local(&self, name: &str) -> Option<LuaValue> {
        for scope in self.scopes.iter().rev() {
            if let Some(value) = scope.locals.get(name) {
                return Some(value.borrow().clone());
            }
        }
        None
    }

    fn set_existing_local(&mut self, name: &str, value: LuaValue) -> bool {
        for scope in self.scopes.iter_mut().rev() {
            if let Some(existing) = scope.locals.get(name) {
                *existing.borrow_mut() = value;
                return true;
            }
        }
        false
    }

    /// Snapshot all current scope locals for upvalue capture.
    fn snapshot(&self) -> Vec<HashMap<String, Rc<RefCell<LuaValue>>>> {
        self.scopes.iter().map(|s| s.locals.clone()).collect()
    }

    /// Create an Env pre-loaded with captured upvalue scopes.
    fn from_captured(captured: &[HashMap<String, Rc<RefCell<LuaValue>>>]) -> Self {
        // (frankenredis-md71j) The captured scopes are upvalues from the
        // outer function; any scopes pushed AFTER this point belong to the
        // freshly-entered function body and are reported as "local".
        let local_floor = captured.len();
        Self {
            scopes: captured
                .iter()
                .map(|locals| Scope {
                    locals: locals.clone(),
                })
                .collect(),
            local_floor,
        }
    }

    /// Classify where `name` lives for error-message purposes:
    /// `Some(true)` if it's a true local of the current function, `Some(false)`
    /// if it's a captured upvalue, `None` if not in any scope.
    /// (frankenredis-md71j)
    fn classify_name(&self, name: &str) -> Option<bool> {
        for (idx, scope) in self.scopes.iter().enumerate().rev() {
            if scope.locals.contains_key(name) {
                return Some(idx >= self.local_floor);
            }
        }
        None
    }
}

impl<'a> LuaState<'a> {
    pub fn new(store: &'a mut Store, now_ms: u64) -> Self {
        let mut globals = HashMap::new();
        // Register built-in functions
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
            "assert",
            "xpcall",
            // (frankenredis-cfflo) loadstring/load parse a chunk of
            // source code and return a callable function. Both are part
            // of vendored Redis 7.2.4's Lua 5.1 sandbox surface; Redis
            // only blocks loadfile/dofile/io/os/require/print etc.
            "loadstring",
            "load",
            // (frankenredis-1khox) 'print' is *not* exposed in the
            // Redis 7.2 Lua sandbox (script_lua.c blocks it alongside
            // loadfile/dofile/io/os/require). The print RustFunction
            // dispatch handler remains in case internal callers want
            // it, but the global is not bound.
        ] {
            globals.insert(name.to_string(), LuaValue::RustFunction(name.to_string()));
        }
        // Math library
        let math_table = LuaTable::new();
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
            // (frankenredis-9dmqr) Trig helpers vendored Redis 7.2.4
            // exposes through Lua 5.1's lmathlib but fr was missing.
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

        // String library
        let string_table = LuaTable::new();
        for name in &[
            "sub", "len", "rep", "lower", "upper", "byte", "char", "reverse", "format", "find",
            "match", "gsub", "gmatch",
            // (frankenredis-dqbdr) Vendored Redis 7.2.4 exposes
            // string.dump from Lua 5.1's stdlib. The function has no
            // useful semantics in fr's tree-walking interpreter (no
            // bytecode form to serialize) so the dispatch handler
            // errors at call time, but `type(string.dump)` must still
            // return 'function' for scripts that probe the surface.
            "dump",
        ] {
            string_table.set(
                LuaValue::Str(name.as_bytes().to_vec()),
                LuaValue::RustFunction(format!("string.{name}")),
            );
        }
        globals.insert("string".to_string(), LuaValue::Table(string_table));

        // Table library
        let table_lib = LuaTable::new();
        for name in &["insert", "remove", "concat", "sort", "getn", "maxn"] {
            table_lib.set(
                LuaValue::Str(name.as_bytes().to_vec()),
                LuaValue::RustFunction(format!("table.{name}")),
            );
        }
        globals.insert("table".to_string(), LuaValue::Table(table_lib));

        // cjson library (commonly used in Redis scripts)
        let cjson_table = LuaTable::new();
        for name in &["encode", "decode"] {
            cjson_table.set(
                LuaValue::Str(name.as_bytes().to_vec()),
                LuaValue::RustFunction(format!("cjson.{name}")),
            );
        }
        globals.insert("cjson".to_string(), LuaValue::Table(cjson_table));

        // (frankenredis-vgnsc) Standard Lua 5.1 globals also exposed in
        // Redis 7.2.4's sandbox: _VERSION constant, rawequal /
        // gcinfo / collectgarbage function entries. fr's tree-walking
        // interpreter has no real Lua heap accounting so gcinfo /
        // collectgarbage('count') return a stable placeholder value
        // and the control variants of collectgarbage are no-ops.
        globals.insert(
            "_VERSION".to_string(),
            LuaValue::Str(b"Lua 5.1".to_vec()),
        );
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

        // (frankenredis-v95aj) Redis 7.2.4 exposes LuaJIT's bit library
        // as a global 'bit' table. Operations are 32-bit; numbers are
        // truncated to u32 before each op and the result is returned
        // as a Lua number (f64-representable for the 0..=u32::MAX range).
        let bit_table = LuaTable::new();
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

        let rng_seed = store.rng_seed;
        Self {
            store,
            now_ms,
            globals,
            globals_locked: false,
            call_depth: 0,
            lua_frame_kinds: Vec::new(),
            iterations: 0,
            rng_seed,
            script_started_at: Instant::now(),
            current_coroutine: None,
            pending_yield: None,
            pending_error_value: None,
            current_source_label: None,
            current_invocation_name: None,
            nested_exec_stmts_depth: 0,
            inside_bare_expression_stmt: false,
        }
    }

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

        // Set up redis table with call/pcall
        let redis_table = LuaTable::new();
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
        // Upstream script_lua.c registers `setresp` and
        // `acl_check_cmd` even though their effects are largely
        // no-ops in standalone mode — clients still expect them
        // to validate their arguments. (br-frankenredis-redislua)
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
        // Replication mode constants. Upstream server.h defines
        // PROPAGATE_AOF=1 and PROPAGATE_REPL=2; script_lua.c then
        // exports REPL_AOF=PROPAGATE_AOF and REPL_SLAVE=REPL_REPLICA=
        // PROPAGATE_REPL. fr previously had AOF and SLAVE/REPLICA
        // swapped. (br-frankenredis-replconst)
        redis_table.set(LuaValue::Str(b"REPL_NONE".to_vec()), LuaValue::Number(0.0));
        redis_table.set(LuaValue::Str(b"REPL_AOF".to_vec()), LuaValue::Number(1.0));
        redis_table.set(LuaValue::Str(b"REPL_SLAVE".to_vec()), LuaValue::Number(2.0));
        redis_table.set(
            LuaValue::Str(b"REPL_REPLICA".to_vec()),
            LuaValue::Number(2.0),
        );
        redis_table.set(LuaValue::Str(b"REPL_ALL".to_vec()), LuaValue::Number(3.0));
        self.globals
            .insert("redis".to_string(), LuaValue::Table(redis_table));

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
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize_all()?;
        let mut parser = Parser::new(tokens);
        let stmts = parser.parse_block()?;
        if !parser.check(&Token::Eof) {
            return Err(format!("unexpected token: {:?}", parser.peek()));
        }
        // (frankenredis-j02x9) Lock the globals table — from this point
        // forward any user-script write to globals raises a readonly-
        // table error and any read of an undefined global raises the
        // upstream sandbox error. Mirrors script_lua.c::
        // luaSetTableProtectionRecursively run after script env init.
        // (frankenredis-u24vv) Snapshot the globals into a `_G` table
        // right before locking, mirroring Lua 5.1 / vendored Redis 7.2.4
        // where the script's environment IS exposed as `_G`. Reads come
        // from the snapshot (since globals are readonly after locking,
        // the snapshot stays in sync); the metatable __index handler
        // emits the nonexistent-global error for missing keys, and the
        // __newindex handler emits the readonly-table error for writes.
        // `_G._G` self-references so scripts can detect the table.
        self.install_g_table();
        self.globals_locked = true;
        let mut env = Env::new();
        let mut varargs = Vec::new();
        // (frankenredis-0k259) The script top-level chunk is a Lua function
        // frame for the purposes of luaL_where; push before exec_block so
        // error(msg, N) at the bottom of the call stack can find it.
        self.lua_frame_kinds.push(true);
        let outcome = self.exec_block(&stmts, &mut env, &mut varargs);
        self.lua_frame_kinds.pop();
        match outcome {
            Ok(ControlFlow::Return(vals)) => {
                Ok(vals.into_iter().next().unwrap_or(LuaValue::Nil))
            }
            Ok(_) => Ok(LuaValue::Nil),
            // (frankenredis-cxmsu) An uncaught `error({...})` /
            // `error(true)` / `error(nil)` escapes through the
            // sentinel string. Convert to a sensible reply string at
            // the boundary — Redis cannot return a non-string error
            // to the wire (vendored Redis 7.2 actually crashes on the
            // table case; fr emits a tostring()-style representation).
            Err(msg) if msg == LUA_TYPED_ERROR_SENTINEL => {
                let val = self.pending_error_value.take().unwrap_or(LuaValue::Nil);
                let rendered = String::from_utf8_lossy(&val.to_display_string()).to_string();
                Err(rendered)
            }
            Err(msg) => Err(msg),
        }
    }

    fn exec_block(
        &mut self,
        stmts: &[Stmt],
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
        stmts: &[Stmt],
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
        for stmt in stmts {
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

    fn exec_stmt(
        &mut self,
        stmt: &Stmt,
        env: &mut Env,
        varargs: &mut Vec<LuaValue>,
    ) -> Result<ControlFlow, String> {
        match stmt {
            Stmt::Return(exprs) => {
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
                let vals = self.eval_expr_list(exprs, env, varargs)?;
                for (i, name) in names.iter().enumerate() {
                    let val = vals.get(i).cloned().unwrap_or(LuaValue::Nil);
                    env.set_local(name, val);
                }
                Ok(ControlFlow::None)
            }
            Stmt::Assign(lhs_list, rhs_list) => {
                let vals = self.eval_expr_list(rhs_list, env, varargs)?;
                for (i, lhs) in lhs_list.iter().enumerate() {
                    let val = vals.get(i).cloned().unwrap_or(LuaValue::Nil);
                    self.assign_to(lhs, val, env, varargs)?;
                }
                Ok(ControlFlow::None)
            }
            Stmt::If(branches, else_body) => {
                for (cond, body) in branches {
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
                    let cv = self.eval_expr(cond, env, varargs)?;
                    env.pop_scope();
                    match cf {
                        ControlFlow::Break => break,
                        ControlFlow::Return(v) => return Ok(ControlFlow::Return(v)),
                        ControlFlow::None => {}
                    }
                    if cv.is_truthy() {
                        break;
                    }
                }
                Ok(ControlFlow::None)
            }
            Stmt::NumericFor(name, start, stop, step, body) => {
                let s = self
                    .eval_expr(start, env, varargs)?
                    .to_number()
                    .ok_or("'for' start must be a number")?;
                let e = self
                    .eval_expr(stop, env, varargs)?
                    .to_number()
                    .ok_or("'for' limit must be a number")?;
                let st = match step {
                    Some(expr) => self
                        .eval_expr(expr, env, varargs)?
                        .to_number()
                        .ok_or("'for' step must be a number")?,
                    None => 1.0,
                };
                // (frankenredis-4hhz5) Lua 5.1 allows step=0; the body
                // either breaks/returns or the loop is infinite (caller's
                // responsibility, same as `while true do end`). Vendored
                // does not reject step=0 at the runtime layer.
                let mut i = s;
                loop {
                    self.iterations += 1;
                    if self.iterations > MAX_ITERATIONS {
                        return Err("script exceeded maximum iteration count".to_string());
                    }
                    if (st > 0.0 && i > e) || (st < 0.0 && i < e) {
                        break;
                    }
                    env.push_scope();
                    env.set_local(name, LuaValue::Number(i));
                    let cf = self.exec_stmts(body, env, varargs)?;
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
                let iter_vals = self.eval_expr_list(iter_exprs, env, varargs)?;
                let iter_fn = iter_vals.first().cloned().unwrap_or(LuaValue::Nil);
                let mut state = iter_vals.get(1).cloned().unwrap_or(LuaValue::Nil);
                let mut control = iter_vals.get(2).cloned().unwrap_or(LuaValue::Nil);

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
                        env.set_local(name, val);
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
            Stmt::DoBlock(body) => self.exec_block(body, env, varargs),
            Stmt::FunctionDecl(names, params, is_variadic, body) => {
                let func = LuaValue::Function(LuaFunc {
                    params: params.clone(),
                    body: body.clone(),
                    is_variadic: *is_variadic,
                    captured_env: Some(env.snapshot()),
                    self_name: None,
                    source_label: self.current_source_label.clone(),
                });
                if names.len() == 1 {
                    // (frankenredis-j02x9) `function f() end` is
                    // equivalent to `f = function() end`; both write
                    // to the globals table. Block once locked.
                    if self.globals_locked {
                        return Err("user_script:1: Attempt to modify a readonly table".to_string());
                    }
                    self.globals.insert(names[0].clone(), func);
                } else {
                    // Nested field assignment: a.b.c = func
                    self.set_nested_field(names, func)?;
                }
                Ok(ControlFlow::None)
            }
            Stmt::LocalFunctionDecl(name, params, is_variadic, body) => {
                let func = LuaValue::Function(LuaFunc {
                    params: params.clone(),
                    body: body.clone(),
                    is_variadic: *is_variadic,
                    captured_env: Some(env.snapshot()),
                    self_name: Some(name.clone()),
                    source_label: self.current_source_label.clone(),
                });
                env.set_local(name, func);
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
        let mut current = self
            .globals
            .get(root_name)
            .cloned()
            .unwrap_or(LuaValue::Nil);
        if !matches!(current, LuaValue::Table(_)) {
            return Err(format!("user_script:1: attempt to index a {} value", current.type_name()));
        }
        // Navigate to the parent table
        let mut path: Vec<LuaValue> = vec![current.clone()];
        for name in parent_fields {
            let next = match &current {
                LuaValue::Table(t) => t.get(&LuaValue::Str(name.as_bytes().to_vec())),
                other => {
                    return Err(format!("user_script:1: attempt to index a {} value", other.type_name()));
                }
            };
            if !matches!(next, LuaValue::Table(_)) {
                return Err(format!("user_script:1: attempt to index a {} value", next.type_name()));
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
            Expr::Name(name) => {
                if !env.set_existing_local(name, value.clone()) {
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
            Err(format!("user_script:1: attempt to index a {} value", table.type_name()))
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
                callable
                    @ (LuaValue::RustFunction(_)
                    | LuaValue::Function(_)
                    | LuaValue::WrappedCoroutine(_)) => {
                    let mut args =
                        vec![LuaValue::Table(current.clone()), key, value];
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
        Err("user_script:1: __newindex cascade exceeded depth limit".to_string())
    }

    fn write_back_table_expr(
        &mut self,
        table_expr: &Expr,
        table: LuaValue,
        env: &mut Env,
        varargs: &mut Vec<LuaValue>,
    ) -> Result<(), String> {
        match table_expr {
            Expr::Name(name) => {
                if !env.set_existing_local(name, table.clone()) {
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
            Expr::Name(name) => {
                if let Some(val) = env.get_local(name) {
                    Ok(val.clone())
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
                            let lhs_simple =
                                matches!(lv, LuaValue::Str(_) | LuaValue::Number(_));
                            let rhs_simple =
                                matches!(rv, LuaValue::Str(_) | LuaValue::Number(_));
                            if !(lhs_simple && rhs_simple) {
                                if let Some(handler) =
                                    self.lookup_binop_metamethod(&lv, &rv, "__concat")
                                {
                                    let mut args = vec![lv, rv];
                                    let results = self.call_function(
                                        &handler, &mut args, env, varargs,
                                    )?;
                                    return Ok(
                                        results.into_iter().next().unwrap_or(LuaValue::Nil),
                                    );
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
                                if let Some(handler) =
                                    self.lookup_binop_metamethod(&lv, &rv, name)
                                {
                                    let mut args = vec![lv, rv];
                                    let results = self.call_function(
                                        &handler, &mut args, env, varargs,
                                    )?;
                                    return Ok(
                                        results.into_iter().next().unwrap_or(LuaValue::Nil),
                                    );
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
                        if matches!(op, BinOp::Eq | BinOp::Ne) {
                            if let (LuaValue::Table(la), LuaValue::Table(rb)) = (&lv, &rv) {
                                if !Rc::ptr_eq(&la.inner, &rb.inner) {
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
                                        || (!matches!(eq_a, LuaValue::Nil)
                                            && lua_raw_equal(&eq_a, &eq_b));
                                    if !matches!(eq_a, LuaValue::Nil) && same_eq {
                                        let mut args = vec![lv.clone(), rv.clone()];
                                        let results = self.call_function(
                                            &eq_a, &mut args, env, varargs,
                                        )?;
                                        let raw = results
                                            .into_iter()
                                            .next()
                                            .unwrap_or(LuaValue::Nil);
                                        let is_eq = raw.is_truthy();
                                        return Ok(LuaValue::Bool(match op {
                                            BinOp::Eq => is_eq,
                                            BinOp::Ne => !is_eq,
                                            _ => unreachable!(),
                                        }));
                                    }
                                }
                            }
                        }
                        if matches!(op, BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge) {
                            let both_numbers = matches!(
                                (&lv, &rv),
                                (LuaValue::Number(_), LuaValue::Number(_))
                            );
                            let both_strings = matches!(
                                (&lv, &rv),
                                (LuaValue::Str(_), LuaValue::Str(_))
                            );
                            if !both_numbers && !both_strings {
                                if let (LuaValue::Table(_), LuaValue::Table(_)) = (&lv, &rv)
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
                                        let results = self.call_function(
                                            &handler, &mut args, env, varargs,
                                        )?;
                                        let raw = results
                                            .into_iter()
                                            .next()
                                            .unwrap_or(LuaValue::Nil);
                                        let truthy = raw.is_truthy();
                                        return Ok(LuaValue::Bool(
                                            if invert { !truthy } else { truthy },
                                        ));
                                    }
                                    // __le fallback: `a <= b` => `not (b < a)`.
                                    if matches!(op, BinOp::Le | BinOp::Ge) {
                                        if let Some(handler) =
                                            self.lookup_binop_metamethod(&lv, &rv, "__lt")
                                        {
                                            let mut args = match op {
                                                BinOp::Le => vec![rv.clone(), lv.clone()],
                                                BinOp::Ge => vec![lv.clone(), rv.clone()],
                                                _ => unreachable!(),
                                            };
                                            let results = self.call_function(
                                                &handler, &mut args, env, varargs,
                                            )?;
                                            let raw = results
                                                .into_iter()
                                                .next()
                                                .unwrap_or(LuaValue::Nil);
                                            return Ok(LuaValue::Bool(!raw.is_truthy()));
                                        }
                                    }
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
                        if val.to_number().is_none() {
                            if let LuaValue::Table(t) = &val {
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
                                    let results = self.call_function(
                                        &handler, &mut args, env, varargs,
                                    )?;
                                    return Ok(
                                        results.into_iter().next().unwrap_or(LuaValue::Nil),
                                    );
                                }
                            }
                        }
                        // (frankenredis-7w22v) Use the operand's actual
                        // type name to match Lua 5.1's "a string value" /
                        // "a boolean value" wording.
                        // (frankenredis-9ckvq) Label the operand by its
                        // syntactic accessor when available.
                        let n = val.to_number().ok_or_else(|| {
                            self.type_error_with_label(
                                "perform arithmetic on",
                                inner,
                                &val,
                                env,
                            )
                        })?;
                        Ok(LuaValue::Number(-n))
                    }
                    UnaryOp::Not => Ok(LuaValue::Bool(!val.is_truthy())),
                    UnaryOp::Len => match &val {
                        LuaValue::Str(s) => Ok(LuaValue::Number(s.len() as f64)),
                        LuaValue::Table(t) => Ok(LuaValue::Number(t.len() as f64)),
                        // (frankenredis-7w22v / frankenredis-9ckvq) Prepend
                        // user_script:1: prefix and label the bad operand.
                        _ => Err(self.type_error_with_label(
                            "get length of",
                            inner,
                            &val,
                            env,
                        )),
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
                    LuaValue::Table(t) => {
                        self.table_lookup_with_index_meta(t, &key, env, varargs)
                    }
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
                    LuaValue::Str(_) => Ok(self
                        .lookup_string_field(&LuaValue::Str(field.as_bytes().to_vec()))),
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
                #[allow(clippy::collapsible_if)]
                if let LuaValue::RustFunction(ref name) = func
                    && matches!(
                        name.as_str(),
                        "table.sort" | "table.insert" | "table.remove" | "rawset"
                    )
                    && let Some(Expr::Name(var_name)) = args.first()
                {
                    if !env.set_existing_local(var_name, arg_vals[0].clone()) {
                        self.globals.insert(var_name.clone(), arg_vals[0].clone());
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
                        return Err(format!(
                            "user_script:1: attempt to index a {} value",
                            obj.type_name()
                        ));
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
                                    table.set(LuaValue::Number(auto_idx as f64), val);
                                    auto_idx += 1;
                                }
                            } else {
                                let val = self.eval_expr(expr, env, varargs)?;
                                table.set(LuaValue::Number(auto_idx as f64), val);
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
            Expr::FunctionDef(params, is_variadic, body) => Ok(LuaValue::Function(LuaFunc {
                params: params.clone(),
                body: body.clone(),
                is_variadic: *is_variadic,
                captured_env: Some(env.snapshot()),
                self_name: None,
                source_label: self.current_source_label.clone(),
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
                            LuaValue::Str(_) => self
                                .lookup_string_field(&LuaValue::Str(method.as_bytes().to_vec())),
                            _ => LuaValue::Nil,
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
        stmts: &[Stmt],
        start_pc: usize,
        env: &mut Env,
        varargs: &mut Vec<LuaValue>,
    ) -> Result<CoroutineRun, String> {
        for (offset, stmt) in stmts.iter().enumerate().skip(start_pc) {
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

    fn resume_coroutine(
        &mut self,
        coroutine: &LuaCoroutine,
        args: &[LuaValue],
    ) -> Result<Vec<LuaValue>, String> {
        let (func, mut env, mut func_varargs, pc) = {
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
            if let Some(env) = inner.env.take() {
                (func, env, std::mem::take(&mut inner.varargs), pc)
            } else {
                let func_value = LuaValue::Function(func.clone());
                let (env, func_varargs) = Self::prepare_lua_function_env(func_value, &func, args);
                (func, env, func_varargs, pc)
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
        let run_result = self.exec_coroutine_stmts(&func.body, pc, &mut env, &mut func_varargs);
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
    /// Build the `_G` table that mirrors the post-init globals and
    /// install it under the "_G" key. Idempotent if called twice
    /// (replaces any existing _G entry). (frankenredis-u24vv)
    fn install_g_table(&mut self) {
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
            .insert("_G".to_string(), LuaValue::Table(g_table));
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
                callable
                    @ (LuaValue::RustFunction(_)
                    | LuaValue::Function(_)
                    | LuaValue::WrappedCoroutine(_)) => {
                    let mut args = vec![LuaValue::Table(current.clone()), key.clone()];
                    let results = self.call_function(&callable, &mut args, env, varargs)?;
                    return Ok(results.into_iter().next().unwrap_or(LuaValue::Nil));
                }
                _ => return Ok(LuaValue::Nil),
            }
        }
        Ok(LuaValue::Nil)
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
    fn format_builtin_argerror(&self, _fallback_name: &str, arg_idx: usize, reason: &str) -> String {
        match &self.current_invocation_name {
            Some(name) => format!(
                "user_script:1: bad argument #{arg_idx} to '{name}' ({reason})"
            ),
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
            Expr::Name(n) => Some(n.clone()),
            Expr::Field(_, f) => Some(f.clone()),
            Expr::Index(_, key) => match key.as_ref() {
                Expr::Str(s) if !s.is_empty() => {
                    std::str::from_utf8(s).ok().map(str::to_string)
                }
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
            let inv_name = self.ast_call_name(callee_expr, method_override);
            let prev = std::mem::replace(&mut self.current_invocation_name, inv_name);
            let result = self.call_function(func, args, env, varargs);
            self.current_invocation_name = prev;
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
            let result = self.call_function(func, args, env, varargs);
            self.current_invocation_name = prev;
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
            LuaValue::RustFunction(_)
            | LuaValue::Function(_)
            | LuaValue::WrappedCoroutine(_) => Some(handler),
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
        // (frankenredis-0k259) Push a frame for this call so error()/assert()
        // can walk `level` entries back through the kind stack to decide
        // whether to prepend the user_script:1 source-location prefix.
        // LuaValue::Function is the only kind that counts as a Lua frame.
        self.lua_frame_kinds.push(matches!(func, LuaValue::Function(_)));
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
            // Lua 5.1 wraps around every runtime error. (Vendored
            // also adds 'field NAME ' context when the call site is
            // a method-style access -- requires plumbing the field
            // name through; deferred as a separate follow-up.)
            LuaValue::Nil => Err("user_script:1: attempt to call a nil value".to_string()),
            LuaValue::Coroutine(_) => {
                Err("user_script:1: attempt to call a thread value".to_string())
            }
            other => Err(format!(
                "user_script:1: attempt to call a {} value",
                other.type_name()
            )),
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
                if args.len() < 2 {
                    return Err("redis.log() requires two arguments or more.".to_string());
                }
                let LuaValue::Number(level_f) = args[0] else {
                    return Err("First argument must be a number (log level).".to_string());
                };
                // Match upstream: level cast to int via lua_tonumber,
                // then bounds-check against LL_DEBUG (0) and LL_WARNING
                // (3). NaN, negative, or > LL_WARNING all raise the
                // same "Invalid debug level." error.
                let level_i = level_f as i64;
                if !level_f.is_finite() || level_i < 0 || level_i > 3 {
                    return Err("Invalid debug level.".to_string());
                }
                Ok(vec![LuaValue::Nil])
            }
            "redis.replicate_commands" => {
                // No-op: effects replication was removed in Redis 7.0+
                // Always returns true for compatibility
                Ok(vec![LuaValue::Bool(true)])
            }
            "redis.set_repl" => {
                if args.len() != 1 {
                    return Err("redis.set_repl() requires one argument.".to_string());
                }
                let Some(flags) = args[0].to_number() else {
                    return Err("Invalid replication flags. Use REPL_AOF, REPL_REPLICA, REPL_ALL or REPL_NONE.".to_string());
                };
                if !flags.is_finite() || flags.fract() != 0.0 {
                    return Err("Invalid replication flags. Use REPL_AOF, REPL_REPLICA, REPL_ALL or REPL_NONE.".to_string());
                }
                let flags = flags as i64;
                if flags & !(SCRIPT_PROPAGATE_AOF as i64 | SCRIPT_PROPAGATE_REPLICA as i64) != 0 {
                    return Err("Invalid replication flags. Use REPL_AOF, REPL_REPLICA, REPL_ALL or REPL_NONE.".to_string());
                }
                self.store.script_propagation_mode = flags as u8;
                Ok(vec![LuaValue::Nil])
            }
            "redis.breakpoint" => {
                // Redis returns false when the Lua debugger is inactive.
                Ok(vec![LuaValue::Bool(false)])
            }
            "redis.setresp" => {
                // Upstream script_lua.c::luaSetRespCommand requires
                // exactly one numeric argument; the value must be 2 or
                // 3. (br-frankenredis-redislua)
                if args.len() != 1 {
                    return Err("redis.setresp() requires one argument.".to_string());
                }
                let Some(version) = args[0].to_number() else {
                    return Err("RESP version must be 2 or 3.".to_string());
                };
                if !version.is_finite() || version.fract() != 0.0 {
                    return Err("RESP version must be 2 or 3.".to_string());
                }
                let v = version as i64;
                if v != 2 && v != 3 {
                    return Err("RESP version must be 2 or 3.".to_string());
                }
                // fr's per-script RESP propagation isn't tracked on
                // the LuaState today; the command's effect on reply
                // shape is a no-op for now but the validation matches
                // upstream so client error wording is correct.
                Ok(vec![LuaValue::Nil])
            }
            "redis.acl_check_cmd" => {
                // Upstream script_lua.c::luaRedisAclCheckCmdCommand
                // requires at least one string argument (the command
                // name); rejects unknown commands. (br-frankenredis-redislua)
                if args.is_empty() {
                    return Err(
                        "Please specify at least one argument for this redis lib call".to_string(),
                    );
                }
                let cmd_bytes = match &args[0] {
                    LuaValue::Str(b) => b.clone(),
                    _ => {
                        return Err(
                            "Lua redis lib command arguments must be strings or integers"
                                .to_string(),
                        );
                    }
                };
                if !crate::is_known_command(&cmd_bytes) {
                    return Err("Invalid command passed to redis.acl_check_cmd()".to_string());
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
                let Some(arg) = args.first() else {
                    return Err("wrong number of arguments".to_string());
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
                if args.is_empty() {
                    return Err(
                        "user_script:1: bad argument #1 to 'tonumber' (value expected)"
                            .to_string(),
                    );
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
                            return Err(
                                "user_script:1: bad argument #2 to 'tonumber' (base out of range)"
                                    .to_string()
                            );
                        };
                        if !base_f.is_finite() {
                            return Err(
                                "user_script:1: bad argument #2 to 'tonumber' (base out of range)"
                                    .to_string()
                            );
                        }
                        // luaL_checkint truncates floats — 10.5 -> 10,
                        // -1.9 -> -1. Match that here before bounds-checking.
                        let base = base_f as i64;
                        if !(2..=36).contains(&base) {
                            return Err(
                                "user_script:1: bad argument #2 to 'tonumber' (base out of range)"
                                    .to_string()
                            );
                        }
                        Some(base as u32)
                    }
                };
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
                            match i64::from_str_radix(stripped, base) {
                                Ok(n) => Ok(vec![LuaValue::Number((sign * n) as f64)]),
                                Err(_) => Ok(vec![LuaValue::Nil]),
                            }
                        } else {
                            match trimmed.parse::<f64>() {
                                Ok(n) => Ok(vec![LuaValue::Number(n)]),
                                Err(_) => {
                                    // Lua 5.1 lobject.c::luaO_str2d falls
                                    // back to a strtoul(s, ..., 16) parse
                                    // when strtod fails or stops at
                                    // 'x'/'X', so `tonumber("0xFF")` →
                                    // 255 and `tonumber("-0xff")` → -255.
                                    // fr was returning nil for any 0x-
                                    // prefixed string. (frankenredis-luatonumhex)
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
                        return Err(self.format_builtin_argerror(
                            "tostring",
                            1,
                            "value expected",
                        ));
                    }
                };
                if let LuaValue::Table(t) = &val {
                    let handler = {
                        let inner = t.inner.borrow();
                        inner
                            .metatable
                            .as_ref()
                            .map(|mt| mt.get(&LuaValue::Str(b"__tostring".to_vec())))
                            .unwrap_or(LuaValue::Nil)
                    };
                    if !matches!(handler, LuaValue::Nil) {
                        let mut meta_args = vec![val.clone()];
                        let results = self.call_function(
                            &handler,
                            &mut meta_args,
                            env,
                            &mut Vec::new(),
                        )?;
                        return Ok(vec![results.into_iter().next().unwrap_or(LuaValue::Nil)]);
                    }
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
                        return Err(self.format_builtin_argerror(
                            "type",
                            1,
                            "value expected",
                        ));
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
                let raw = args.first().cloned().unwrap_or(LuaValue::Nil);
                let level = lua_optional_integer_arg("error", 2, args.get(1), 1)?;
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
                Err(LUA_TYPED_ERROR_SENTINEL.to_string())
            }
            "assert" => {
                // (frankenredis-nf29w) Lua 5.1 lbaselib.c::luaB_assert
                // calls luaL_checkany first, so a zero-arg invocation
                // raises "bad argument #1 to ? (value expected)" rather
                // than "assertion failed!".
                if args.is_empty() {
                    return Err(self.format_builtin_argerror(
                        "assert",
                        1,
                        "value expected",
                    ));
                }
                let val = &args[0];
                if val.is_truthy() {
                    Ok(args.to_vec())
                } else {
                    // (frankenredis-l4k9y) Upstream luaL_error prepends
                    // "user_script:1: " (the source-location prefix) to
                    // the assert message. fr previously emitted the bare
                    // message, so a downstream pcall(...) on the failed
                    // assert lacked the prefix vendored callers rely on.
                    let msg = args
                        .get(1)
                        .map(|a| String::from_utf8_lossy(&a.to_display_string()).to_string())
                        .unwrap_or_else(|| "assertion failed!".to_string());
                    Err(format!("user_script:1: {msg}"))
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
                match Lexer::new(&src_bytes).tokenize_all().and_then(|tokens| {
                    let mut parser = Parser::new(tokens);
                    let stmts = parser.parse_block()?;
                    if !parser.check(&Token::Eof) {
                        return Err(format!("unexpected token: {:?}", parser.peek()));
                    }
                    Ok(stmts)
                }) {
                    Ok(body) => Ok(vec![LuaValue::Function(LuaFunc {
                        params: Vec::new(),
                        body,
                        is_variadic: true,
                        captured_env: Some(env.snapshot()),
                        self_name: None,
                        // (frankenredis-ycaog) Tag the chunk so runtime
                        // errors raised from inside it are reported with
                        // the loadstring chunk-label prefix instead of
                        // the outer script's `user_script:1:` prefix.
                        source_label: Some(chunk_label),
                    })]),
                    Err(msg) => Ok(vec![
                        LuaValue::Nil,
                        LuaValue::Str(
                            format!("{chunk_label}:1: {msg}").into_bytes(),
                        ),
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
                // accident (vendored: "bad argument #1 to 'load'
                // (function expected, got string)").
                let raw = args.first().cloned().unwrap_or(LuaValue::Nil);
                match &raw {
                    LuaValue::Function(_)
                    | LuaValue::RustFunction(_)
                    | LuaValue::WrappedCoroutine(_) => Err(
                        "load with a chunk-generator function is not yet implemented in fr; \
                         use loadstring(source) instead"
                            .to_string(),
                    ),
                    _ => Err(format!(
                        // (frankenredis-cfflo) Lua 5.1 marks C closures
                        // without a registered debug name with '?'. The
                        // base library's `load` is one of these. Match
                        // vendored's exact wording.
                        "bad argument #1 to '?' (function expected, got {})",
                        raw.type_name()
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
                let result = self.call_function(&func, &mut call_args_vec, env, &mut Vec::new());
                self.current_invocation_name = prev_inv;
                match result {
                    Ok(vals) => {
                        // Drop any stale typed-error stash; the protected
                        // call completed normally.
                        self.pending_error_value = None;
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
                    return Err(self.format_builtin_argerror(
                        "xpcall",
                        2,
                        "value expected",
                    ));
                }
                let func = args.first().cloned().unwrap_or(LuaValue::Nil);
                let err_handler = args.get(1).cloned().unwrap_or(LuaValue::Nil);
                let mut call_args_vec = args.get(2..).unwrap_or(&[]).to_vec();
                // (frankenredis-557p3) Same AST-context clear as pcall.
                let prev_inv = self.current_invocation_name.take();
                let result = self.call_function(&func, &mut call_args_vec, env, &mut Vec::new());
                self.current_invocation_name = prev_inv;
                match result {
                    Ok(vals) => {
                        self.pending_error_value = None;
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
                    return Err(lua_bad_table_arg("pairs", 1, args.first()));
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
                    return Err(lua_bad_table_arg("ipairs", 1, args.first()));
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
                        Ok(vec![
                            LuaValue::Number(idx as f64),
                            t.inner.borrow().array[idx - 1].clone(),
                        ])
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
                    return Err(lua_bad_table_arg("next", 1, args.first()));
                };

                // Find next key after the given key.
                if matches!(key, LuaValue::Nil) {
                    if !t.inner.borrow().array.is_empty() {
                        return Ok(vec![
                            LuaValue::Number(1.0),
                            t.inner.borrow().array[0].clone(),
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
                        if idx < t.inner.borrow().array.len() {
                            return Ok(vec![
                                LuaValue::Number((idx + 1) as f64),
                                t.inner.borrow().array[idx].clone(),
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
                let t = lua_table_arg("unpack", 1, args.first())?;
                let start = lua_optional_integer_arg("unpack", 2, args.get(1), 1)? as usize;
                let end = lua_optional_integer_arg(
                    "unpack",
                    3,
                    args.get(2),
                    t.inner.borrow().array.len() as i64,
                )? as usize;
                if start <= end && end.saturating_sub(start) >= 8000 {
                    return Err("too many results to unpack".to_string());
                }
                let mut results = Vec::new();
                for i in start..=end {
                    if i >= 1 && i <= t.inner.borrow().array.len() {
                        results.push(t.inner.borrow().array[i - 1].clone());
                    } else {
                        results.push(LuaValue::Nil);
                    }
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
                let idx = args.first().cloned().unwrap_or(LuaValue::Nil);
                let rest = args.get(1..).unwrap_or(&[]);
                match &idx {
                    LuaValue::Str(s) if s == b"#" => Ok(vec![LuaValue::Number(rest.len() as f64)]),
                    _ => {
                        let raw_index = idx.to_number().ok_or_else(|| {
                            self.format_builtin_argerror(
                                "select",
                                1,
                                &format!("number expected, got {}", idx.type_name()),
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
                    return Err(self.format_builtin_argerror(
                        "rawget",
                        2,
                        "value expected",
                    ));
                }
                let key = key_opt.cloned().unwrap();
                Ok(vec![table.get(&key)])
            }
            "rawset" => {
                // (frankenredis-uyj7c) Upstream luaB_rawset uses
                // luaL_checktype(L,1,LUA_TTABLE) then luaL_checkany(L,2)
                // then luaL_checkany(L,3) — all three are required, with
                // 'value expected' wording for the trailing args.
                if !matches!(args.first(), Some(LuaValue::Table(_))) {
                    return Err(format!(
                        "user_script:1: bad argument #1 to 'rawset' (table expected, got {})",
                        lua_arg_got_label(args.first())
                    ));
                }
                if args.get(1).is_none() {
                    return Err(
                        "user_script:1: bad argument #2 to 'rawset' (value expected)".to_string(),
                    );
                }
                if args.get(2).is_none() {
                    return Err(
                        "user_script:1: bad argument #3 to 'rawset' (value expected)".to_string(),
                    );
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
                let LuaValue::Table(t) = &table else { unreachable!() };
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
            "getmetatable" => {
                // (frankenredis-nf29w) Lua 5.1 luaB_getmetatable calls
                // luaL_checkany so a zero-arg call raises
                // "bad argument #1 to ? (value expected)".
                if args.is_empty() {
                    return Err(self.format_builtin_argerror(
                        "getmetatable",
                        1,
                        "value expected",
                    ));
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
                    _ => Ok(vec![LuaValue::Nil]),
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
                let n = lua_check_number(args, 0, "floor")?;
                Ok(vec![LuaValue::Number(n.floor())])
            }
            "math.ceil" => {
                let n = lua_check_number(args, 0, "ceil")?;
                Ok(vec![LuaValue::Number(n.ceil())])
            }
            "math.abs" => {
                let n = lua_check_number(args, 0, "abs")?;
                Ok(vec![LuaValue::Number(n.abs())])
            }
            "math.max" => {
                // (frankenredis-n4eln) Lua 5.1's lmathlib.c::math_max
                // uses luaL_checknumber on every arg starting at arg 1,
                // so calling with zero args raises 'bad argument #1 to
                // max (number expected, got no value)'.
                // (frankenredis-a6r5p) Wrong-type args at index > 0
                // route through the same luaL_argerror, so they need
                // the source-location prefix and the correct arg
                // index. Pre-fix fr emitted a generic "bad argument
                // to 'math.max'" for any non-numeric arg past #1.
                if args.is_empty() {
                    return Err(
                        "user_script:1: bad argument #1 to 'max' (number expected, got no value)"
                            .to_string(),
                    );
                }
                let mut max = f64::NEG_INFINITY;
                for (i, _) in args.iter().enumerate() {
                    let n = lua_check_number(args, i, "max")?;
                    if n > max {
                        max = n;
                    }
                }
                Ok(vec![LuaValue::Number(max)])
            }
            "math.min" => {
                // (frankenredis-a6r5p) Same template as math.max — see
                // comment above for the upstream wording rationale.
                if args.is_empty() {
                    return Err(
                        "user_script:1: bad argument #1 to 'min' (number expected, got no value)"
                            .to_string(),
                    );
                }
                let mut min = f64::INFINITY;
                for (i, _) in args.iter().enumerate() {
                    let n = lua_check_number(args, i, "min")?;
                    if n < min {
                        min = n;
                    }
                }
                Ok(vec![LuaValue::Number(min)])
            }
            "math.sqrt" => {
                let n = lua_check_number(args, 0, "sqrt")?;
                Ok(vec![LuaValue::Number(n.sqrt())])
            }
            "math.random" => {
                // (frankenredis-nwmly) Upstream lmathlib.c::math_random
                // dispatches on lua_gettop(L). 0 args -> float [0,1);
                // 1 arg -> int [1,u] with luaL_argcheck(_, 1<=u, 1, ...);
                // 2 args -> int [l,u] with luaL_argcheck(_, l<=u, 2, ...)
                // — note arg #2 is reported as the bad arg when l>u;
                // 3+ args -> luaL_error(_, "wrong number of arguments").
                // fr previously reported arg #1 for the 2-arg case,
                // omitted the user_script:1: prefix, and silently
                // accepted 3+ args.
                let r = self.next_rand();
                match args.len() {
                    0 => {
                        let f = (r as f64) / (u64::MAX as f64 + 1.0);
                        Ok(vec![LuaValue::Number(f)])
                    }
                    1 => {
                        let m = args[0]
                            .to_number()
                            .ok_or("user_script:1: bad argument #1 to 'random' (number expected)")?
                            as i64;
                        if m < 1 {
                            return Err(
                                "user_script:1: bad argument #1 to 'random' (interval is empty)"
                                    .to_string()
                            );
                        }
                        let val = (r % (m as u64)) + 1;
                        Ok(vec![LuaValue::Number(val as f64)])
                    }
                    2 => {
                        let m = args[0]
                            .to_number()
                            .ok_or("user_script:1: bad argument #1 to 'random' (number expected)")?
                            as i64;
                        let n = args[1]
                            .to_number()
                            .ok_or("user_script:1: bad argument #2 to 'random' (number expected)")?
                            as i64;
                        if m > n {
                            return Err(
                                "user_script:1: bad argument #2 to 'random' (interval is empty)"
                                    .to_string()
                            );
                        }
                        let range = n as i128 - m as i128 + 1;
                        let val = m as i128 + (r as i128 % range);
                        Ok(vec![LuaValue::Number(val as f64)])
                    }
                    _ => Err("user_script:1: wrong number of arguments".to_string()),
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
                let b = lua_check_number(args, 1, "fmod")?;
                let a = lua_check_number(args, 0, "fmod")?;
                Ok(vec![LuaValue::Number(a % b)])
            }
            "math.log" => {
                let n = lua_check_number(args, 0, "log")?;
                Ok(vec![LuaValue::Number(n.ln())])
            }
            "math.exp" => {
                let n = lua_check_number(args, 0, "exp")?;
                Ok(vec![LuaValue::Number(n.exp())])
            }
            "math.pow" => {
                let b = lua_check_number(args, 1, "pow")?;
                let a = lua_check_number(args, 0, "pow")?;
                Ok(vec![LuaValue::Number(a.powf(b))])
            }
            "math.sin" => {
                let n = lua_check_number(args, 0, "sin")?;
                Ok(vec![LuaValue::Number(n.sin())])
            }
            "math.cos" => {
                let n = lua_check_number(args, 0, "cos")?;
                Ok(vec![LuaValue::Number(n.cos())])
            }
            "math.tan" => {
                let n = lua_check_number(args, 0, "tan")?;
                Ok(vec![LuaValue::Number(n.tan())])
            }
            "math.asin" => {
                let n = lua_check_number(args, 0, "asin")?;
                Ok(vec![LuaValue::Number(n.asin())])
            }
            "math.acos" => {
                let n = lua_check_number(args, 0, "acos")?;
                Ok(vec![LuaValue::Number(n.acos())])
            }
            "math.atan" => {
                let n = lua_check_number(args, 0, "atan")?;
                Ok(vec![LuaValue::Number(n.atan())])
            }
            "math.atan2" => {
                let x = lua_check_number(args, 1, "atan2")?;
                let y = lua_check_number(args, 0, "atan2")?;
                Ok(vec![LuaValue::Number(y.atan2(x))])
            }
            // (frankenredis-9dmqr) Five additional math helpers Lua 5.1
            // exposes that fr's interpreter was missing dispatch arms
            // for. Rust's f64 stdlib supplies each operation directly.
            "math.deg" => {
                let n = lua_check_number(args, 0, "deg")?;
                Ok(vec![LuaValue::Number(n.to_degrees())])
            }
            "math.rad" => {
                let n = lua_check_number(args, 0, "rad")?;
                Ok(vec![LuaValue::Number(n.to_radians())])
            }
            "math.sinh" => {
                let n = lua_check_number(args, 0, "sinh")?;
                Ok(vec![LuaValue::Number(n.sinh())])
            }
            "math.cosh" => {
                let n = lua_check_number(args, 0, "cosh")?;
                Ok(vec![LuaValue::Number(n.cosh())])
            }
            "math.tanh" => {
                let n = lua_check_number(args, 0, "tanh")?;
                Ok(vec![LuaValue::Number(n.tanh())])
            }
            "math.log10" => {
                let n = lua_check_number(args, 0, "log10")?;
                Ok(vec![LuaValue::Number(n.log10())])
            }
            "math.modf" => {
                let n = lua_check_number(args, 0, "modf")?;
                let trunc = n.trunc();
                let frac = n - trunc;
                Ok(vec![LuaValue::Number(trunc), LuaValue::Number(frac)])
            }
            "math.frexp" => {
                let n = lua_check_number(args, 0, "frexp")?;
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
                let e = lua_check_number(args, 1, "ldexp")? as i32;
                let m = lua_check_number(args, 0, "ldexp")?;
                Ok(vec![LuaValue::Number(m * 2f64.powi(e))])
            }
            "math.randomseed" => {
                if let Some(arg) = args.first()
                    && let Some(n) = arg.to_number()
                {
                    self.rng_seed = n.to_bits();
                }
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
            "coroutine.create" => {
                if let Some(LuaValue::Function(func)) = args.first() {
                    Ok(vec![LuaValue::Coroutine(LuaCoroutine::new(func.clone()))])
                } else {
                    Err(
                        "user_script:1: bad argument #1 to 'create' (Lua function expected)"
                            .to_string(),
                    )
                }
            }
            "coroutine.wrap" => {
                if let Some(LuaValue::Function(func)) = args.first() {
                    Ok(vec![LuaValue::WrappedCoroutine(LuaCoroutine::new(
                        func.clone(),
                    ))])
                } else {
                    Err(
                        "user_script:1: bad argument #1 to 'wrap' (Lua function expected)"
                            .to_string(),
                    )
                }
            }
            "coroutine.resume" => match args.first() {
                Some(LuaValue::Coroutine(coroutine)) => {
                    self.resume_coroutine(coroutine, args.get(1..).unwrap_or(&[]))
                }
                _ => Err(
                    "user_script:1: bad argument #1 to 'resume' (coroutine expected)".to_string(),
                ),
            },
            "coroutine.status" => match args.first() {
                Some(LuaValue::Coroutine(coroutine)) => {
                    Ok(vec![LuaValue::Str(coroutine.status_name().to_vec())])
                }
                _ => Err(
                    "user_script:1: bad argument #1 to 'status' (coroutine expected)".to_string(),
                ),
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
                let s = lua_check_string(args, 0, "len")?;
                Ok(vec![LuaValue::Number(s.len() as f64)])
            }
            "string.sub" => {
                let s = lua_check_string(args, 0, "sub")?;
                let len = s.len() as i64;
                let mut i = lua_check_number(args, 1, "sub")? as i64;
                let mut j = args.get(2).and_then(|v| v.to_number()).unwrap_or(-1.0) as i64;
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
                let s = lua_check_string(args, 0, "rep")?;
                let n_val = lua_check_number(args, 1, "rep")?;
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
                let s = lua_check_string(args, 0, "lower")?;
                Ok(vec![LuaValue::Str(s.to_ascii_lowercase())])
            }
            "string.upper" => {
                let s = lua_check_string(args, 0, "upper")?;
                Ok(vec![LuaValue::Str(s.to_ascii_uppercase())])
            }
            "string.reverse" => {
                let mut s = lua_check_string(args, 0, "reverse")?;
                s.reverse();
                Ok(vec![LuaValue::Str(s)])
            }
            // (frankenredis-dqbdr) Lua 5.1 string.dump serialises a
            // function to its bytecode form. fr's tree-walking
            // interpreter has no bytecode representation, so the
            // function is registered (so `type(string.dump)` returns
            // 'function') but errors when invoked.
            "string.dump" => Err(
                "user_script:1: unable to dump given function".to_string(),
            ),
            "string.byte" => {
                let s = lua_check_string(args, 0, "byte")?;
                let len = s.len() as i64;
                let mut i = args.get(1).and_then(|v| v.to_number()).unwrap_or(1.0) as i64;
                let mut j = args.get(2).and_then(|v| v.to_number()).unwrap_or(i as f64) as i64;
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
                // argument-error template. fr previously emitted just
                // the bare "bad argument #N to 'char' (...)" wording
                // which the regression suite caught when the error
                // crossed back to the client envelope.
                let mut result = Vec::new();
                for (i, a) in args.iter().enumerate() {
                    let n = a
                        .to_number()
                        .ok_or_else(|| {
                            format!(
                                "user_script:1: bad argument #{} to 'char' (number expected, got {})",
                                i + 1,
                                a.type_name()
                            )
                        })? as i64;
                    if !(0..=255).contains(&n) {
                        return Err(format!(
                            "user_script:1: bad argument #{} to 'char' (invalid value)",
                            i + 1
                        ));
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
                        return Err(format!(
                            "user_script:1: bad argument #1 to 'format' (string expected, got {})",
                            other.type_name()
                        ));
                    }
                    None => {
                        return Err(
                            "user_script:1: bad argument #1 to 'format' (string expected, got no value)"
                                .to_string(),
                        );
                    }
                };
                let fmt_str = String::from_utf8_lossy(&fmt_bytes).to_string();
                let rest: &[LuaValue] = if args.is_empty() { &[] } else { &args[1..] };
                let result = lua_string_format(&fmt_str, rest)?;
                Ok(vec![LuaValue::Str(result.into_bytes())])
            }
            "string.find" => {
                let s = lua_check_string(args, 0, "find")?;
                let pattern = lua_check_string(args, 1, "find")?;
                let init_raw = args.get(2).and_then(|v| v.to_number()).unwrap_or(1.0) as i64;
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
                if !plain {
                    lua_pattern_validate(&pattern)?;
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
                let s = lua_check_string(args, 0, "match")?;
                let pattern = lua_check_string(args, 1, "match")?;
                let init_raw = args.get(2).and_then(|v| v.to_number()).unwrap_or(1.0) as i64;
                let init = if init_raw < 0 {
                    (s.len() as i64 + init_raw).max(0) as usize
                } else {
                    (init_raw as usize).saturating_sub(1)
                };
                // (frankenredis-vfv8s) Validate pattern eagerly.
                lua_pattern_validate(&pattern)?;
                if let Some(m) = lua_pattern_find(&s, &pattern, init) {
                    Ok(lua_match_captures(&s, &m))
                } else {
                    Ok(vec![LuaValue::Nil])
                }
            }
            "string.gmatch" => {
                // Returns an iterator function. Each call returns next match.
                // We collect all matches and return a closure-like iterator via a table.
                let s = lua_check_string(args, 0, "gmatch")?;
                let pattern = lua_check_string(args, 1, "gmatch")?;
                // (frankenredis-vfv8s) Validate pattern eagerly so the
                // iterator constructor surfaces malformed patterns the
                // same way upstream's gmatch does.
                lua_pattern_validate(&pattern)?;
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
                // Return a special iterator: we store matches in a table and return
                // an iterator function that pops values. For simplicity in our evaluator,
                // we return the first match's captures (single call pattern used in for loops
                // is handled by the generic-for which calls the iterator repeatedly).
                // Actually, gmatch returns an iterator function. We need a stateful closure.
                // Simplest approach: return a Rust function that internally tracks state.
                // For now, flatten to first match only if used as expression.
                // The for-in loop handles this via repeated calls.
                // We'll store all matches in the iterator's upvalue.
                // Return a table with __gmatch_data so the for loop can consume it.
                let iter_state = LuaTable::new();
                iter_state.set(
                    LuaValue::Str(b"__gmatch_data".to_vec()),
                    LuaValue::Table(result_table),
                );
                iter_state.set(
                    LuaValue::Str(b"__gmatch_idx".to_vec()),
                    LuaValue::Number(0.0),
                );
                Ok(vec![
                    LuaValue::RustFunction("__gmatch_iter".to_string()),
                    LuaValue::Table(iter_state),
                    LuaValue::Nil,
                ])
            }
            "string.gsub" => {
                let s = lua_check_string(args, 0, "gsub")?;
                let pattern = lua_check_string(args, 1, "gsub")?;
                let repl = args.get(2).cloned().unwrap_or(LuaValue::Nil);
                let max_n = args.get(3).and_then(|v| v.to_number()).map(|n| n as usize);
                // (frankenredis-vfv8s) Validate pattern eagerly.
                lua_pattern_validate(&pattern)?;
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
                            LuaValue::Str(repl_bytes) => {
                                lua_gsub_replace(&s, &m, repl_bytes)?
                            }
                            LuaValue::Table(t) => {
                                let key_bytes = lua_gsub_capture_key(&s, &m);
                                let key = LuaValue::Str(key_bytes);
                                let val = t.get(&key);
                                lua_gsub_normalise_repl(&s, &m, &val)?
                            }
                            LuaValue::Function(_) | LuaValue::RustFunction(_) | LuaValue::WrappedCoroutine(_) => {
                                let mut call_args = lua_gsub_capture_args(&s, &m);
                                let mut varargs = Vec::new();
                                let ret = self.call_function(
                                    &repl,
                                    &mut call_args,
                                    env,
                                    &mut varargs,
                                )?;
                                let first = ret.into_iter().next().unwrap_or(LuaValue::Nil);
                                lua_gsub_normalise_repl(&s, &m, &first)?
                            }
                            LuaValue::Number(_) => {
                                // Legacy: numeric repl coerces to string.
                                lua_gsub_replace(&s, &m, &repl.to_display_string())?
                            }
                            _ => {
                                return Err(format!(
                                    "user_script:1: bad argument #3 to 'gsub' (string/function/table expected, got {})",
                                    repl.type_name()
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
                let table = args.first().cloned().unwrap_or(LuaValue::Nil);
                let LuaValue::Table(_) = &table else {
                    return Err(lua_bad_table_arg("insert", 1, args.first()));
                };
                // (frankenredis-jwkhc) Upstream Redis-vendored ltablib.c
                // raises 'wrong number of arguments to insert' for any
                // shape other than (t, v) or (t, pos, v).
                if args.len() != 2 && args.len() != 3 {
                    return Err(
                        "user_script:1: wrong number of arguments to 'insert'".to_string(),
                    );
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
                    let pos_i = lua_required_integer_arg("insert", 2, &args[1])?;
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
                let table = args.first().cloned().unwrap_or(LuaValue::Nil);
                if !matches!(table, LuaValue::Table(_)) {
                    return Err(lua_bad_table_arg("remove", 1, args.first()));
                }
                let pos_arg = args.get(1).cloned();
                if let LuaValue::Table(ref mut t) = args[0] {
                    let pos = lua_optional_integer_arg(
                        "remove",
                        2,
                        pos_arg.as_ref(),
                        t.inner.borrow().array.len() as i64,
                    )? as usize;
                    let removed = if pos >= 1 && pos <= t.inner.borrow().array.len() {
                        t.inner.borrow_mut().array.remove(pos - 1)
                    } else {
                        LuaValue::Nil
                    };
                    return Ok(vec![removed]);
                }
                Ok(vec![LuaValue::Nil])
            }
            "table.concat" => {
                let t = lua_table_arg("concat", 1, args.first())?;
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
                let array_len = t.inner.borrow().array.len() as i64;
                let start = lua_optional_integer_arg("concat", 3, args.get(2), 1)?;
                let end =
                    lua_optional_integer_arg("concat", 4, args.get(3), array_len)?;
                // (frankenredis-jwkhc) Upstream ltablib.c::tconcat validates
                // each element in [start, end] and raises 'invalid value (nil
                // | <type>) at index N in table for concat' for any non-
                // string/non-number entry. fr previously silently dropped
                // out-of-range indices and nil holes.
                let mut result: Vec<u8> = Vec::new();
                if start <= end {
                    let mut first = true;
                    let array = &t.inner.borrow().array;
                    for i in start..=end {
                        let val: Option<&LuaValue> = if i >= 1 && (i as usize) <= array.len() {
                            Some(&array[(i - 1) as usize])
                        } else {
                            None
                        };
                        let bytes = match val {
                            Some(LuaValue::Str(b)) => b.clone(),
                            Some(LuaValue::Number(n)) => {
                                if *n == (*n as i64) as f64 && n.is_finite() {
                                    format!("{}", *n as i64).into_bytes()
                                } else {
                                    lua_number_to_string(*n).into_bytes()
                                }
                            }
                            Some(other) => {
                                return Err(format!(
                                    "user_script:1: invalid value ({}) at index {i} in table for 'concat'",
                                    other.type_name()
                                ));
                            }
                            None => {
                                return Err(format!(
                                    "user_script:1: invalid value (nil) at index {i} in table for 'concat'"
                                ));
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
                let _ = lua_check_table(args, 0, "sort")?;
                {
                    let comp_fn = args.get(1).cloned();
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
                        // Default sort: compare as strings or numbers
                        arr.sort_by(|a, b| match (a, b) {
                            (LuaValue::Number(x), LuaValue::Number(y)) => {
                                x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal)
                            }
                            (LuaValue::Str(x), LuaValue::Str(y)) => x.cmp(y),
                            _ => std::cmp::Ordering::Equal,
                        });
                    }
                    // Put array back
                    if let LuaValue::Table(ref mut t) = args[0] {
                        t.inner.borrow_mut().array = arr;
                    }
                }
                Ok(vec![LuaValue::Nil])
            }
            "table.getn" => {
                let table = args.first().cloned().unwrap_or(LuaValue::Nil);
                if let LuaValue::Table(t) = &table {
                    Ok(vec![LuaValue::Number(t.len() as f64)])
                } else {
                    Ok(vec![LuaValue::Number(0.0)])
                }
            }
            "table.maxn" => {
                // (frankenredis-3osi6) Upstream ltablib.c:maxn uses
                // luaL_checktype(L, 1, LUA_TTABLE).
                let _ = lua_check_table(args, 0, "maxn")?;
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
            // ── plain top-level globals (frankenredis-vgnsc) ──────────
            "rawequal" => {
                // (frankenredis-uyj7c) Upstream luaB_rawequal uses
                // luaL_checkany on both args — missing/explicit-nil call
                // raises 'bad argument #N to rawequal (value expected)'.
                if args.first().is_none() {
                    return Err(
                        "user_script:1: bad argument #1 to 'rawequal' (value expected)".to_string(),
                    );
                }
                if args.get(1).is_none() {
                    return Err(
                        "user_script:1: bad argument #2 to 'rawequal' (value expected)".to_string(),
                    );
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
                let opt = match args.first() {
                    Some(LuaValue::Str(s)) => String::from_utf8_lossy(s).to_string(),
                    Some(LuaValue::Number(n)) => format!("{n}"),
                    Some(LuaValue::Nil) | None => "collect".to_string(),
                    Some(other) => {
                        return Err(format!(
                            "user_script:1: bad argument #1 to 'collectgarbage' (string expected, got {})",
                            other.type_name()
                        ));
                    }
                };
                let known = matches!(
                    opt.as_str(),
                    "collect" | "stop" | "restart" | "step" | "setpause" | "setstepmul" | "count"
                );
                if !known {
                    return Err(format!(
                        "user_script:1: bad argument #1 to 'collectgarbage' (invalid option '{opt}')"
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
                let r = args.iter().try_fold(0xFFFF_FFFFu32, |acc, a| {
                    lua_value_to_u32(a).map(|v| acc & v)
                })?;
                Ok(vec![LuaValue::Number(r as i32 as f64)])
            }
            "bit.bor" => {
                let r = args
                    .iter()
                    .try_fold(0u32, |acc, a| lua_value_to_u32(a).map(|v| acc | v))?;
                Ok(vec![LuaValue::Number(r as i32 as f64)])
            }
            "bit.bxor" => {
                let r = args
                    .iter()
                    .try_fold(0u32, |acc, a| lua_value_to_u32(a).map(|v| acc ^ v))?;
                Ok(vec![LuaValue::Number(r as i32 as f64)])
            }
            "bit.bnot" => {
                let r = !lua_value_to_u32(args.first().unwrap_or(&LuaValue::Number(0.0)))?;
                Ok(vec![LuaValue::Number(r as i32 as f64)])
            }
            "bit.lshift" => {
                let x = lua_value_to_u32(args.first().unwrap_or(&LuaValue::Number(0.0)))?;
                let n =
                    lua_value_to_u32(args.get(1).unwrap_or(&LuaValue::Number(0.0)))? & 31;
                Ok(vec![LuaValue::Number((x << n) as i32 as f64)])
            }
            "bit.rshift" => {
                let x = lua_value_to_u32(args.first().unwrap_or(&LuaValue::Number(0.0)))?;
                let n =
                    lua_value_to_u32(args.get(1).unwrap_or(&LuaValue::Number(0.0)))? & 31;
                Ok(vec![LuaValue::Number((x >> n) as i32 as f64)])
            }
            "bit.arshift" => {
                let x = lua_value_to_u32(args.first().unwrap_or(&LuaValue::Number(0.0)))? as i32;
                let n =
                    lua_value_to_u32(args.get(1).unwrap_or(&LuaValue::Number(0.0)))? & 31;
                Ok(vec![LuaValue::Number((x >> n) as f64)])
            }
            "bit.rol" => {
                let x = lua_value_to_u32(args.first().unwrap_or(&LuaValue::Number(0.0)))?;
                let n =
                    lua_value_to_u32(args.get(1).unwrap_or(&LuaValue::Number(0.0)))? & 31;
                Ok(vec![LuaValue::Number(x.rotate_left(n) as i32 as f64)])
            }
            "bit.ror" => {
                let x = lua_value_to_u32(args.first().unwrap_or(&LuaValue::Number(0.0)))?;
                let n =
                    lua_value_to_u32(args.get(1).unwrap_or(&LuaValue::Number(0.0)))? & 31;
                Ok(vec![LuaValue::Number(x.rotate_right(n) as i32 as f64)])
            }
            "bit.bswap" => {
                let x = lua_value_to_u32(args.first().unwrap_or(&LuaValue::Number(0.0)))?;
                Ok(vec![LuaValue::Number(x.swap_bytes() as i32 as f64)])
            }
            "bit.tobit" => {
                let x = lua_value_to_u32(args.first().unwrap_or(&LuaValue::Number(0.0)))?;
                // LuaJIT bit.tobit returns a signed 32-bit value as
                // Lua number: the upper half of u32 maps to negative.
                Ok(vec![LuaValue::Number(x as i32 as f64)])
            }
            "bit.tohex" => {
                let x = lua_value_to_u32(args.first().unwrap_or(&LuaValue::Number(0.0)))?;
                // Second arg (optional): digit count; negative = upper case.
                let n = match args.get(1) {
                    Some(LuaValue::Number(f)) => *f as i32,
                    _ => 8,
                };
                let abs_n = n.unsigned_abs().min(8) as usize;
                let s = if n < 0 {
                    format!("{x:0width$X}", width = abs_n)
                } else {
                    format!("{x:0width$x}", width = abs_n)
                };
                let trimmed: String = if s.len() > abs_n {
                    s.chars().rev().take(abs_n).collect::<String>().chars().rev().collect()
                } else {
                    s
                };
                Ok(vec![LuaValue::Str(trimmed.into_bytes())])
            }
            // ── cjson library ───────────────────────────────────────────
            "cjson.encode" => {
                let val = args.first().cloned().unwrap_or(LuaValue::Nil);
                // (frankenredis-bum6y) Upstream cjson raises via luaL_error,
                // which auto-prepends 'user_script:N: '. fr's wrap does not
                // add it for non-runtime errors, so do it explicitly here
                // to match vendored verbatim.
                let json = lua_value_to_json(&val)
                    .map_err(|e| format!("user_script:1: {e}"))?;
                Ok(vec![LuaValue::Str(json.into_bytes())])
            }
            "cjson.decode" => {
                let data = args
                    .first()
                    .map(|a| a.to_display_string())
                    .unwrap_or_default();
                let s = String::from_utf8_lossy(&data).to_string();
                let val = json_to_lua_value(&s)?;
                Ok(vec![val])
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
        if args.is_empty() {
            // Upstream script_lua.c::luaRedisGenericCommand emits
            // 'Please specify at least one argument for this redis
            // lib call' when no command name is provided.
            // (br-frankenredis-replyargtype)
            return Err("Please specify at least one argument for this redis lib call".to_string());
        }

        // Build argv for dispatch
        let mut argv: Vec<Vec<u8>> = Vec::new();
        for arg in args {
            argv.push(arg.to_redis_arg()?);
        }

        let dirty_before = self.store.dirty;
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
                    let err_msg = if err_msg.starts_with("ERR unknown command ") {
                        "ERR Unknown Redis command called from script".to_string()
                    } else if err_msg.starts_with("ERR wrong number of arguments")
                        || err_msg
                            .starts_with("ERR Unknown subcommand or wrong number of arguments")
                    {
                        "ERR Wrong number of args calling Redis command from script".to_string()
                    } else {
                        err_msg
                    };
                    Err(err_msg)
                }
            }
        };

        match command_result {
            Ok(frame) => {
                let dirty_after = self.store.dirty;
                if dirty_after > dirty_before || command_may_propagate_from_script(&argv) {
                    self.store.record_script_propagation(&argv);
                }
                Ok(vec![resp_to_lua_command_result(&argv, &frame)])
            }
            Err(err_msg) => {
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
    let bulk = |s: &str| RespFrame::BulkString(Some(s.as_bytes().to_vec()));
    RespFrame::Array(Some(vec![
        bulk("ACL <subcommand> [<arg> [value] [opt] ...]. Subcommands are:"),
        bulk("CAT [<category>]"),
        bulk("    List all commands that belong to <category>, or all command categories"),
        bulk("    when no category is specified."),
        bulk("DELUSER <username> [<username> ...]"),
        bulk("    Delete a list of users."),
        bulk("DRYRUN <username> <command> [<arg> ...]"),
        bulk("    Test if a command would be allowed for the given user."),
        bulk("GENPASS [<bits>]"),
        bulk("    Generate a secure password."),
        bulk("GETUSER <username>"),
        bulk("    Get the user's details."),
        bulk("LIST"),
        bulk("    List users access rules in the ACL format."),
        bulk("LOAD"),
        bulk("    Reload users from the ACL file."),
        bulk("LOG [<count> | RESET]"),
        bulk("    List latest events denied because of ACLs."),
        bulk("SAVE"),
        bulk("    Save the current ACL rules to the ACL file."),
        bulk("SETUSER <username> <property> [<property> ...]"),
        bulk("    Create or modify a user with the specified properties."),
        bulk("USERS"),
        bulk("    List all usernames."),
        bulk("WHOAMI"),
        bulk("    Return the current connection username."),
        bulk("HELP"),
        bulk("    Print this help."),
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
/// (frankenredis-vfv8s)
fn lua_pattern_validate(pat: &[u8]) -> Result<(), String> {
    let mut i = 0;
    while i < pat.len() {
        match pat[i] {
            b'%' => {
                if i + 1 >= pat.len() {
                    return Err(
                        "user_script:1: malformed pattern (ends with '%')".to_string(),
                    );
                }
                // (frankenredis-3zxc1) Upstream lstrlib.c::do_match raises
                // luaL_error("missing '[' after '%%f' in pattern") when %f
                // is not immediately followed by a character class. Advance
                // only to the '[' so the next loop iteration validates the
                // [set] body just like any other bracket expression.
                if pat[i + 1] == b'f' {
                    if i + 2 >= pat.len() || pat[i + 2] != b'[' {
                        return Err(
                            "user_script:1: missing '[' after '%f' in pattern".to_string(),
                        );
                    }
                    i += 2;
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
                    return Err(
                        "user_script:1: malformed pattern (missing ']')".to_string(),
                    );
                }
                i = j;
            }
            _ => i += 1,
        }
    }
    Ok(())
}

fn lua_pattern_element_len(pat: &[u8], pi: usize) -> usize {
    if pi >= pat.len() {
        return 0;
    }
    match pat[pi] {
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
fn lua_gsub_normalise_repl(
    s: &[u8],
    m: &LuaPatMatch,
    val: &LuaValue,
) -> Result<Vec<u8>, String> {
    match val {
        LuaValue::Nil | LuaValue::Bool(false) => Ok(s[m.start..m.end].to_vec()),
        LuaValue::Bool(true) => Err(
            "user_script:1: invalid replacement value (a boolean)".to_string(),
        ),
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
                    return Err(
                        "user_script:1: invalid capture index".to_string(),
                    );
                }
                i += 2;
            } else if next == b'%' {
                result.push(b'%');
                i += 2;
            } else {
                result.push(repl[i]);
                i += 1;
            }
        } else {
            result.push(repl[i]);
            i += 1;
        }
    }
    Ok(result)
}

// ── Type conversions ────────────────────────────────────────────────────

fn resp_to_lua_command_result(argv: &[Vec<u8>], frame: &RespFrame) -> LuaValue {
    if config_get_returns_map_in_lua(argv)
        && let Some(table) = config_get_resp_to_lua_map(frame)
    {
        return LuaValue::Table(table);
    }
    resp_to_lua(frame)
}

fn config_get_returns_map_in_lua(argv: &[Vec<u8>]) -> bool {
    argv.len() >= 2
        && argv[0].eq_ignore_ascii_case(b"CONFIG")
        && argv[1].eq_ignore_ascii_case(b"GET")
}

fn config_get_resp_to_lua_map(frame: &RespFrame) -> Option<LuaTable> {
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
        table.set(LuaValue::Str(key), resp_to_lua(&chunk[1]));
    }

    Some(table)
}

fn resp_to_lua(frame: &RespFrame) -> LuaValue {
    match frame {
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
        RespFrame::BulkString(None) => LuaValue::Bool(false),
        RespFrame::BulkString(Some(data)) => LuaValue::Str(data.clone()),
        RespFrame::Array(None) => LuaValue::Bool(false),
        RespFrame::Array(Some(items)) | RespFrame::Push(items) | RespFrame::Sequence(items) => {
            let t = LuaTable::new();
            for (i, item) in items.iter().enumerate() {
                t.set(LuaValue::Number((i + 1) as f64), resp_to_lua(item));
            }
            LuaValue::Table(t)
        }
        // RESP3 Map: Lua scripts have no native map type, so we
        // flatten as a key-value alternating array, mirroring how
        // upstream's redis-server materializes a RESP3 map for a
        // RESP2 Lua callsite. (br-frankenredis-r80v / r72v)
        RespFrame::Map(None) => LuaValue::Bool(false),
        RespFrame::Map(Some(pairs)) => {
            let t = LuaTable::new();
            for (i, (k, v)) in pairs.iter().enumerate() {
                t.set(LuaValue::Number((2 * i + 1) as f64), resp_to_lua(k));
                t.set(LuaValue::Number((2 * i + 2) as f64), resp_to_lua(v));
            }
            LuaValue::Table(t)
        }
    }
}

pub fn lua_to_resp(val: &LuaValue) -> RespFrame {
    match val {
        LuaValue::Nil => RespFrame::BulkString(None),
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
            let i = if n.is_finite() {
                *n as i64
            } else {
                i64::MIN
            };
            RespFrame::Integer(i)
        }
        LuaValue::Str(s) => RespFrame::BulkString(Some(s.clone())),
        LuaValue::Table(t) => {
            // Check for special "ok" or "err" fields
            if let LuaValue::Str(ok) = t.get(&LuaValue::Str(b"ok".to_vec())) {
                return RespFrame::SimpleString(String::from_utf8_lossy(&ok).to_string());
            }
            if let LuaValue::Str(err) = t.get(&LuaValue::Str(b"err".to_vec())) {
                return RespFrame::Error(String::from_utf8_lossy(&err).to_string());
            }

            // Upstream src/script_lua.c::luaReplyToRedisReply checks for
            // RESP3 type-hint tables AFTER ok/err. fr was missing the
            // entire group, so map / set / double / big_number /
            // verbatim_string hint tables were all serialized as
            // empty arrays (no integer keys at top level).
            // (frankenredis-luaresp3hint)

            // {map = t}: emit a Map frame whose entries are the hash
            // pairs of the inner table. The fr-protocol layer flattens
            // Map → 2N alternating Array under RESP2 and emits the `%`
            // prefix under RESP3.
            let map_field = t.get(&LuaValue::Str(b"map".to_vec()));
            if let LuaValue::Table(inner) = map_field {
                let pairs = inner
                    .hash_pairs()
                    .into_iter()
                    .map(|(k, v)| (lua_to_resp(&k), lua_to_resp(&v)))
                    .collect();
                return RespFrame::Map(Some(pairs));
            }

            // {set = t}: emit the inner table's array part as an Array.
            // RESP2 wire is the same as for any array; RESP3's `~` Set
            // prefix isn't represented in fr's frame enum, so this is
            // a RESP2-correct approximation under RESP3 too.
            let set_field = t.get(&LuaValue::Str(b"set".to_vec()));
            if let LuaValue::Table(inner) = set_field {
                let items: Vec<RespFrame> = inner
                    .inner
                    .borrow()
                    .array
                    .iter()
                    .filter(|v| !matches!(v, LuaValue::Nil))
                    .map(lua_to_resp)
                    .collect();
                return RespFrame::Array(Some(items));
            }

            // {double = x}: emit BulkString of the double formatted
            // upstream-style. Lua's `%.17g`-equivalent rendering
            // matches what Redis surfaces, so use Rust's default
            // f64 Display which is round-trip safe.
            let double_field = t.get(&LuaValue::Str(b"double".to_vec()));
            if let LuaValue::Number(n) = double_field {
                let s = format!("{n}");
                return RespFrame::BulkString(Some(s.into_bytes()));
            }

            // {big_number = "..."}: emit BulkString of the bignum. RESP3
            // would prefix `(`, but fr-protocol has no BigNumber variant
            // and BulkString is the documented RESP2 fallback.
            let bn_field = t.get(&LuaValue::Str(b"big_number".to_vec()));
            if let LuaValue::Str(s) = bn_field {
                return RespFrame::BulkString(Some(s.clone()));
            }

            // {verbatim_string = {format = ..., string = ...}}:
            // emit BulkString of the inner `string` field. Upstream's
            // RESP3 `=<len>\r\n<fmt>:<payload>` framing isn't
            // representable in fr's frame enum; the BulkString of the
            // raw payload is the documented RESP2 fallback.
            let vs_field = t.get(&LuaValue::Str(b"verbatim_string".to_vec()));
            if let LuaValue::Table(inner) = vs_field
                && let LuaValue::Str(s) = inner.get(&LuaValue::Str(b"string".to_vec()))
            {
                return RespFrame::BulkString(Some(s));
            }

            // Convert array part to RESP array (stop at first nil, matching Redis)
            let mut items = Vec::new();
            for item in t.inner.borrow().array.clone() {
                if matches!(item, LuaValue::Nil) {
                    break;
                }
                items.push(lua_to_resp(&item));
            }
            RespFrame::Array(Some(items))
        }
        LuaValue::Function(_)
        | LuaValue::RustFunction(_)
        | LuaValue::Coroutine(_)
        | LuaValue::WrappedCoroutine(_) => RespFrame::BulkString(None),
    }
}

// ── string.format implementation ────────────────────────────────────────

fn lua_string_format(fmt: &str, args: &[LuaValue]) -> Result<String, String> {
    let mut result = String::new();
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
                return Err(format!(
                    "user_script:1: bad argument #{} to 'format' (no value)",
                    arg_idx + 2
                ));
            }
            if let Some(&next) = chars.peek() {
                if next == '%' {
                    chars.next();
                    result.push('%');
                    continue;
                }
                // Parse flags
                let mut left_align = false;
                let mut zero_pad = false;
                let mut show_sign = false;
                let mut space_sign = false;
                let mut alt_form = false;
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
                            return Err(format!(
                                "user_script:1: bad argument #{} to 'format' (no value)",
                                arg_idx + 2
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
                            Err(format!(
                                "user_script:1: bad argument #{} to 'format' (number expected, got {})",
                                arg_idx + 1,
                                v.type_name()
                            ))
                        }
                    };
                    let formatted = match conv {
                        'd' | 'i' => {
                            let n = require_number(&arg)? as i64;
                            let s = if show_sign && n >= 0 {
                                format!("+{n}")
                            } else if space_sign && n >= 0 {
                                format!(" {n}")
                            } else {
                                format!("{n}")
                            };
                            lua_fmt_pad(&s, width, left_align, if zero_pad { '0' } else { ' ' })
                        }
                        'u' => {
                            // (frankenredis-t1ah8) Upstream %u prints the unsigned
                            // bit pattern of the C `long`/`int` result of luaL_checkinteger.
                            // Going via `as i64 as u64` recovers the two's complement
                            // bit pattern for negatives (e.g. -1 -> 18446744073709551615).
                            // fr was rendering negatives as signed (-1) by sharing the
                            // %d arm.
                            let n = require_number(&arg)? as i64 as u64;
                            let s = format!("{n}");
                            lua_fmt_pad(&s, width, left_align, if zero_pad { '0' } else { ' ' })
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
                                lua_fmt_pad(&s, width, left_align, ' ')
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
                                lua_fmt_pad(&s, width, left_align, ' ')
                            } else {
                                let prec = precision.unwrap_or(6);
                                let s = lua_fmt_scientific(n, prec, conv == 'E');
                                let s = if show_sign && n >= 0.0 {
                                    format!("+{s}")
                                } else {
                                    s
                                };
                                lua_fmt_pad(&s, width, left_align, ' ')
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
                                lua_fmt_pad(&s, width, left_align, ' ')
                            } else {
                                let prec = precision.unwrap_or(6).max(1);
                                let s = lua_fmt_g(n, prec, conv == 'G');
                                let s = if show_sign && n >= 0.0 {
                                    format!("+{s}")
                                } else {
                                    s
                                };
                                lua_fmt_pad(&s, width, left_align, ' ')
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
                                    return Err(format!(
                                        "user_script:1: bad argument #{} to 'format' (string expected, got {})",
                                        arg_idx + 1,
                                        arg.type_name()
                                    ));
                                }
                            };
                            let mut s = String::from_utf8_lossy(&s).to_string();
                            if let Some(prec) = precision {
                                s.truncate(prec);
                            }
                            lua_fmt_pad(&s, width, left_align, ' ')
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
                                    return Err(format!(
                                        "user_script:1: bad argument #{} to 'format' (string expected, got {})",
                                        arg_idx + 1,
                                        arg.type_name()
                                    ));
                                }
                            };
                            let mut q = String::new();
                            q.push('"');
                            for &b in &s {
                                match b {
                                    b'\\' => q.push_str("\\\\"),
                                    b'"' => q.push_str("\\\""),
                                    b'\n' => q.push_str("\\\n"),
                                    b'\r' => q.push_str("\\r"),
                                    b'\0' => q.push_str("\\0"),
                                    _ => q.push(b as char),
                                }
                            }
                            q.push('"');
                            q
                        }
                        'x' | 'X' => {
                            // (frankenredis-t1ah8) Upstream luaL_checkinteger
                            // returns lua_Integer (C `ptrdiff_t`/long); %x/%X
                            // prints the unsigned bit pattern. `as u64` directly
                            // from f64 saturates negatives to 0; going through
                            // i64 first recovers the two's complement bits, so
                            // -1 -> ffffffffffffffff matching vendored.
                            let n = require_number(&arg)? as i64 as u64;
                            let s = if conv == 'x' {
                                if alt_form {
                                    format!("0x{n:x}")
                                } else {
                                    format!("{n:x}")
                                }
                            } else if alt_form {
                                format!("0X{n:X}")
                            } else {
                                format!("{n:X}")
                            };
                            lua_fmt_pad(&s, width, left_align, if zero_pad { '0' } else { ' ' })
                        }
                        'o' => {
                            // (frankenredis-t1ah8) Same fix as %x/%X — recover
                            // the unsigned bit pattern for negative inputs.
                            let n = require_number(&arg)? as i64 as u64;
                            let s = if alt_form {
                                format!("0{n:o}")
                            } else {
                                format!("{n:o}")
                            };
                            lua_fmt_pad(&s, width, left_align, if zero_pad { '0' } else { ' ' })
                        }
                        'c' => {
                            // (frankenredis-be7o1) Upstream printf %c casts to
                            // unsigned char with modulo-256 wrap; e.g. -1 -> 0xFF,
                            // 256 -> 0. Rust's 'as u8' saturates floats, so go via
                            // i64 first to recover the C wrap-around semantics.
                            let n = require_number(&arg)? as i64 as u8;
                            String::from(n as char)
                        }
                        _ => {
                            // (frankenredis-be7o1) Upstream lstrlib.c:str_format
                            // rejects unknown conversion specifiers via luaL_error
                            // with the verbatim wording below.
                            return Err(format!(
                                "user_script:1: invalid option '%{conv}' to 'format'"
                            ));
                        }
                    };
                    result.push_str(&formatted);
                }
            } else {
                result.push(c);
            }
        } else {
            result.push(c);
        }
    }
    Ok(result)
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
            if upper { "-INF".to_string() } else { "-inf".to_string() }
        } else if upper {
            "INF".to_string()
        } else {
            "inf".to_string()
        })
    } else if n.is_nan() {
        Some(if n.is_sign_negative() {
            if upper { "-NAN".to_string() } else { "-nan".to_string() }
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

// ── cjson helpers ───────────────────────────────────────────────────────

fn json_escape_bytes(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let mut out = String::from('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
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
        return if n > 0.0 { "inf".to_string() } else { "-inf".to_string() };
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
    if exponent < -4 || exponent >= PRECISION {
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

/// (frankenredis-v95aj) Normalise a LuaValue to u32 for bit library
/// ops. Mirrors LuaJIT bit.tobit: numbers are first cast to int32 via
/// truncate-toward-zero (matching f64 -> i32 semantics for finite
/// values), then reinterpreted as u32. Strings are parsed if they look
/// numeric, otherwise an error is raised.
fn lua_value_to_u32(val: &LuaValue) -> Result<u32, String> {
    match val {
        LuaValue::Number(f) => {
            if !f.is_finite() {
                return Err("bad argument to bit op (number expected, got NaN/inf)".to_string());
            }
            Ok((*f as i64 as i32) as u32)
        }
        LuaValue::Str(s) => {
            let text = String::from_utf8_lossy(s);
            text.trim()
                .parse::<i64>()
                .map(|n| n as i32 as u32)
                .or_else(|_| text.trim().parse::<f64>().map(|f| f as i64 as i32 as u32))
                .map_err(|_| format!("bad argument to bit op (number expected, got string '{text}')"))
        }
        LuaValue::Bool(b) => Ok(if *b { 1 } else { 0 }),
        LuaValue::Nil => {
            Err("bad argument to bit op (number expected, got nil)".to_string())
        }
        _ => Err("bad argument to bit op (number expected, got non-number)".to_string()),
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
            if *n == (*n as i64) as f64 {
                Ok(format!("{}", *n as i64))
            } else {
                Ok(format!("{n}"))
            }
        }
        LuaValue::Str(s) => Ok(json_escape_bytes(s)),
        LuaValue::Table(t) => {
            if !t.inner.borrow().array.is_empty() && t.hash_is_empty() {
                let items = t
                    .inner
                    .borrow()
                    .array
                    .iter()
                    .map(lua_value_to_json)
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(format!("[{}]", items.join(",")))
            } else if t.inner.borrow().array.is_empty() && !t.hash_is_empty() {
                let hash_pairs = t.hash_pairs();
                let pairs = hash_pairs
                    .iter()
                    .map(|(k, v)| {
                        let key_json = json_escape_bytes(&k.to_display_string());
                        lua_value_to_json(v).map(|v_json| format!("{key_json}:{v_json}"))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(format!("{{{}}}", pairs.join(",")))
            } else if t.inner.borrow().array.is_empty() && t.hash_is_empty() {
                Ok("{}".to_string())
            } else {
                let mut pairs: Vec<String> = Vec::new();
                for (i, v) in t.inner.borrow().array.iter().enumerate() {
                    pairs.push(format!("\"{}\":{}", i + 1, lua_value_to_json(v)?));
                }
                for (k, v) in &t.hash_pairs() {
                    let key_json = json_escape_bytes(&k.to_display_string());
                    pairs.push(format!("{key_json}:{}", lua_value_to_json(v)?));
                }
                Ok(format!("{{{}}}", pairs.join(",")))
            }
        }
        LuaValue::Function(_) | LuaValue::RustFunction(_) => {
            Err("Cannot serialise function: type not supported".to_string())
        }
        LuaValue::Coroutine(_) | LuaValue::WrappedCoroutine(_) => {
            Err("Cannot serialise thread: type not supported".to_string())
        }
    }
}

fn json_to_lua_value(s: &str) -> Result<LuaValue, String> {
    let s = s.trim();
    if s == "null" || s.is_empty() {
        Ok(LuaValue::Nil)
    } else if s == "true" {
        Ok(LuaValue::Bool(true))
    } else if s == "false" {
        Ok(LuaValue::Bool(false))
    } else if s.starts_with('"') && s.ends_with('"') {
        let inner = &s[1..s.len() - 1];
        // Basic unescape
        let mut result = Vec::new();
        let mut chars = inner.bytes().peekable();
        while let Some(b) = chars.next() {
            if b == b'\\' {
                if let Some(esc) = chars.next() {
                    match esc {
                        b'"' => result.push(b'"'),
                        b'\\' => result.push(b'\\'),
                        b'b' => result.push(0x08),
                        b'f' => result.push(0x0C),
                        b'n' => result.push(b'\n'),
                        b'r' => result.push(b'\r'),
                        b't' => result.push(b'\t'),
                        b'u' => {
                            let mut hex = [0u8; 4];
                            let mut read_len = 0usize;
                            let mut complete = true;
                            for digit in &mut hex {
                                if let Some(next) = chars.next() {
                                    *digit = next;
                                    read_len += 1;
                                } else {
                                    complete = false;
                                    break;
                                }
                            }
                            if complete
                                && let Ok(hex_str) = std::str::from_utf8(&hex)
                                && let Ok(codepoint) = u32::from_str_radix(hex_str, 16)
                                && let Some(decoded) = char::from_u32(codepoint)
                            {
                                let mut utf8 = [0u8; 4];
                                let encoded = decoded.encode_utf8(&mut utf8);
                                result.extend_from_slice(encoded.as_bytes());
                            } else {
                                result.extend_from_slice(br"\u");
                                result.extend_from_slice(&hex[..read_len]);
                            }
                        }
                        _ => {
                            result.push(b'\\');
                            result.push(esc);
                        }
                    }
                }
            } else {
                result.push(b);
            }
        }
        Ok(LuaValue::Str(result))
    } else if s.starts_with('[') && s.ends_with(']') {
        // Simple JSON array parser
        let inner = &s[1..s.len() - 1].trim();
        if inner.is_empty() {
            return Ok(LuaValue::Table(LuaTable::new()));
        }
        let items = split_json_values(inner)?;
        let t = LuaTable::new();
        for item in items {
            t.inner.borrow_mut().array.push(json_to_lua_value(&item)?);
        }
        Ok(LuaValue::Table(t))
    } else if s.starts_with('{') && s.ends_with('}') {
        let inner = &s[1..s.len() - 1].trim();
        if inner.is_empty() {
            return Ok(LuaValue::Table(LuaTable::new()));
        }
        let pairs = split_json_values(inner)?;
        let t = LuaTable::new();
        for pair in pairs {
            if let Some(colon_pos) = find_json_colon(&pair) {
                let key = pair[..colon_pos].trim();
                let val = pair[colon_pos + 1..].trim();
                let key_val = json_to_lua_value(key)?;
                let val_val = json_to_lua_value(val)?;
                t.set(key_val, val_val);
            }
        }
        Ok(LuaValue::Table(t))
    } else if let Ok(n) = s.parse::<f64>() {
        Ok(LuaValue::Number(n))
    } else {
        Err(format!("invalid JSON: {s}"))
    }
}

fn split_json_values(s: &str) -> Result<Vec<String>, String> {
    let mut items = Vec::new();
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape = false;
    let mut start = 0;

    for (i, &b) in s.as_bytes().iter().enumerate() {
        if escape {
            escape = false;
            continue;
        }
        if b == b'\\' && in_string {
            escape = true;
            continue;
        }
        if b == b'"' {
            in_string = !in_string;
            continue;
        }
        if in_string {
            continue;
        }
        match b {
            b'[' | b'{' => depth += 1,
            b']' | b'}' => depth -= 1,
            b',' if depth == 0 => {
                items.push(s[start..i].trim().to_string());
                start = i + 1;
            }
            _ => {}
        }
    }
    if start < s.len() {
        items.push(s[start..].trim().to_string());
    }
    Ok(items)
}

fn find_json_colon(s: &str) -> Option<usize> {
    let mut in_string = false;
    let mut escape = false;
    for (i, &b) in s.as_bytes().iter().enumerate() {
        if escape {
            escape = false;
            continue;
        }
        if b == b'\\' && in_string {
            escape = true;
            continue;
        }
        if b == b'"' {
            in_string = !in_string;
            continue;
        }
        if !in_string && b == b':' {
            return Some(i);
        }
    }
    None
}

// ── Public entry point ──────────────────────────────────────────────────

pub fn eval_script(
    script: &[u8],
    keys: &[Vec<u8>],
    argv: &[Vec<u8>],
    store: &mut Store,
    now_ms: u64,
) -> Result<RespFrame, String> {
    store.clear_script_propagation_state();
    store.script_propagation_mode = SCRIPT_PROPAGATE_ALL;
    let mut state = LuaState::new(store, now_ms);

    let keys_vals: Vec<LuaValue> = keys.iter().map(|k| LuaValue::Str(k.clone())).collect();
    let argv_vals: Vec<LuaValue> = argv.iter().map(|a| LuaValue::Str(a.clone())).collect();
    state.set_keys_argv(keys_vals, argv_vals);

    // Strip a Redis 7.0+ Lua shebang line if present; upstream Lua
    // parses `#!...\n` as a comment, but our minimal interpreter
    // doesn't. The flag-honouring side of the shebang is handled
    // upstream of this call in fr-command::eval_cmd. Replace the
    // shebang line with whitespace of the same length so reported
    // line numbers stay aligned with the user's script.
    // (br-frankenredis-r75v)
    let stripped: Vec<u8>;
    let executed_script: &[u8] = if script.starts_with(b"#!") {
        let line_end = script
            .iter()
            .position(|&b| b == b'\n')
            .unwrap_or(script.len());
        let mut tmp = Vec::with_capacity(script.len());
        tmp.extend(std::iter::repeat_n(b' ', line_end));
        tmp.extend_from_slice(&script[line_end..]);
        stripped = tmp;
        &stripped
    } else {
        script
    };

    let result = state.execute(executed_script)?;
    Ok(lua_to_resp(&result))
}

/// Lex+parse a script body without executing it. Returns the parser's
/// error message verbatim on failure. Mirrors the shebang-stripping
/// performed by `eval_script` so SCRIPT LOAD validates the same source
/// EVAL would later run. (frankenredis-scrldch)
pub fn compile_check(script: &[u8]) -> Result<(), String> {
    let stripped: Vec<u8>;
    let source: &[u8] = if script.starts_with(b"#!") {
        let line_end = script
            .iter()
            .position(|&b| b == b'\n')
            .unwrap_or(script.len());
        let mut tmp = Vec::with_capacity(script.len());
        tmp.extend(std::iter::repeat_n(b' ', line_end));
        tmp.extend_from_slice(&script[line_end..]);
        stripped = tmp;
        &stripped
    } else {
        script
    };
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize_all()?;
    let mut parser = Parser::new(tokens);
    let _ = parser.parse_block()?;
    if !parser.check(&Token::Eof) {
        return Err(format!("unexpected token: {:?}", parser.peek()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use fr_protocol::RespFrame;
    use fr_store::Store;

    use super::{
        Env, LuaState, LuaTable, LuaValue, SCRIPT_NOSCRIPT_ERROR, compile_check, eval_script,
        json_to_lua_value, lua_raw_equal, lua_value_to_json,
    };

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
        let err = eval_script(
            b"local t = {}; return t .. 'x'",
            &[], &[], &mut store, 0,
        ).expect_err("expected concat error");
        assert!(
            err.contains("attempt to concatenate local 't' (a table value)"),
            "wrong wording for local LHS: {err:?}"
        );

        // local on RHS still produces the accessor label.
        let err = eval_script(
            b"local t = {}; return 'x' .. t",
            &[], &[], &mut store, 0,
        ).expect_err("expected concat error");
        assert!(
            err.contains("attempt to concatenate local 't' (a table value)"),
            "wrong wording for local RHS: {err:?}"
        );

        // Field-access prefix.
        let err = eval_script(
            b"local obj = {fld = {}}; return obj.fld .. 'x'",
            &[], &[], &mut store, 0,
        ).expect_err("expected concat error");
        assert!(
            err.contains("attempt to concatenate field 'fld' (a table value)"),
            "wrong wording for field: {err:?}"
        );

        // Numeric-index access → field '?'.
        let err = eval_script(
            b"local arr = {{}}; return arr[1] .. 'x'",
            &[], &[], &mut store, 0,
        ).expect_err("expected concat error");
        assert!(
            err.contains("attempt to concatenate field '?' (a table value)"),
            "wrong wording for numeric index: {err:?}"
        );

        // String-literal index resolves to the field name.
        let err = eval_script(
            b"local obj = {fld = {}}; return obj['fld'] .. 'x'",
            &[], &[], &mut store, 0,
        ).expect_err("expected concat error");
        assert!(
            err.contains("attempt to concatenate field 'fld' (a table value)"),
            "wrong wording for string-index: {err:?}"
        );

        // Call result has no accessor — falls back to anonymous form.
        let err = eval_script(
            b"local function g() return {} end; return g() .. 'x'",
            &[], &[], &mut store, 0,
        ).expect_err("expected concat error");
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
            &[], &[], &mut store, 0,
        ).unwrap();
        let body = match frame {
            RespFrame::BulkString(Some(b)) => b,
            other => panic!("expected bulk string, got {other:?}"),
        };
        assert_eq!(body, b"bad argument #1 to '?' (value expected)");

        // assert() direct call uses 'assert' as the name.
        let err = eval_script(b"return assert()", &[], &[], &mut store, 0)
            .expect_err("expected error");
        assert!(
            err.contains("user_script:1: bad argument #1 to 'assert' (value expected)"),
            "assert direct: {err:?}"
        );

        // xpcall(f) with no msgh.
        let frame = eval_script(
            b"local ok, err = pcall(xpcall, function() end); return err",
            &[], &[], &mut store, 0,
        ).unwrap();
        let body = match frame {
            RespFrame::BulkString(Some(b)) => b,
            other => panic!("expected bulk string, got {other:?}"),
        };
        assert_eq!(body, b"bad argument #2 to '?' (value expected)");

        // rawget() with no args → "table expected, got no value".
        let frame = eval_script(
            b"local ok, err = pcall(rawget); return err",
            &[], &[], &mut store, 0,
        ).unwrap();
        let body = match frame {
            RespFrame::BulkString(Some(b)) => b,
            other => panic!("expected bulk string, got {other:?}"),
        };
        assert_eq!(body, b"bad argument #1 to '?' (table expected, got no value)");

        // rawget(t) with no key → "value expected" at slot 2.
        let frame = eval_script(
            b"local ok, err = pcall(rawget, {}); return err",
            &[], &[], &mut store, 0,
        ).unwrap();
        let body = match frame {
            RespFrame::BulkString(Some(b)) => b,
            other => panic!("expected bulk string, got {other:?}"),
        };
        assert_eq!(body, b"bad argument #2 to '?' (value expected)");

        // setmetatable() with no args.
        let frame = eval_script(
            b"local ok, err = pcall(setmetatable); return err",
            &[], &[], &mut store, 0,
        ).unwrap();
        let body = match frame {
            RespFrame::BulkString(Some(b)) => b,
            other => panic!("expected bulk string, got {other:?}"),
        };
        assert_eq!(body, b"bad argument #1 to '?' (table expected, got no value)");

        // getmetatable() with no args.
        let frame = eval_script(
            b"local ok, err = pcall(getmetatable); return err",
            &[], &[], &mut store, 0,
        ).unwrap();
        let body = match frame {
            RespFrame::BulkString(Some(b)) => b,
            other => panic!("expected bulk string, got {other:?}"),
        };
        assert_eq!(body, b"bad argument #1 to '?' (value expected)");

        // Regression: good args still work.
        let frame = eval_script(
            b"local t = {x = 1}; return rawget(t, 'x')",
            &[], &[], &mut store, 0,
        ).unwrap();
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
        let err = eval_script(
            b"return tostring()",
            &[], &[], &mut store, 0,
        ).expect_err("expected error");
        assert!(
            err.contains("user_script:1: bad argument #1 to 'tostring' (value expected)"),
            "tostring direct: {err:?}"
        );

        let err = eval_script(b"return type()", &[], &[], &mut store, 0)
            .expect_err("expected error");
        assert!(
            err.contains("user_script:1: bad argument #1 to 'type' (value expected)"),
            "type direct: {err:?}"
        );

        // pcall callback: name is '?' with no prefix.
        let frame = eval_script(
            b"local ok, err = pcall(tostring); return err",
            &[], &[], &mut store, 0,
        ).unwrap();
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
        let err = eval_script(
            b"return select(0, 'a')",
            &[], &[], &mut store, 0,
        ).expect_err("expected error");
        assert!(
            err.contains("user_script:1: bad argument #1 to 'select' (index out of range)"),
            "direct call wording: {err:?}"
        );

        // Local alias surfaces the local variable's name.
        let err = eval_script(
            b"local f = select; return f(0, 'a')",
            &[], &[], &mut store, 0,
        ).expect_err("expected error");
        assert!(
            err.contains("user_script:1: bad argument #1 to 'f' (index out of range)"),
            "alias wording: {err:?}"
        );

        // Field call uses the field name.
        let err = eval_script(
            b"local t = {s = select}; return t.s(0, 'a')",
            &[], &[], &mut store, 0,
        ).expect_err("expected error");
        assert!(
            err.contains("user_script:1: bad argument #1 to 's' (index out of range)"),
            "field-call wording: {err:?}"
        );

        // pcall(select, ...) loses the AST context: name is '?' and
        // no user_script:1: prefix.
        let frame = eval_script(
            b"local ok, err = pcall(select, 0, 'a'); return err",
            &[], &[], &mut store, 0,
        ).unwrap();
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
            &[], &[], &mut store, 0,
        ).unwrap();
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
            &[], &[], &mut store, 0,
        ).unwrap();
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
            &[], &[], &mut store, 0,
        ).unwrap();
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
        let frame =
            eval_script(b"return select(1.5, 'a', 'b')", &[], &[], &mut store, 0).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"a".to_vec())));

        // Happy paths still work.
        let frame = eval_script(
            b"return select(2, 'a', 'b', 'c')",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"b".to_vec())));

        let frame =
            eval_script(b"return select('#', 'a', 'b', 'c')", &[], &[], &mut store, 0).unwrap();
        assert_eq!(frame, RespFrame::Integer(3));

        let frame = eval_script(
            b"return select(-1, 'a', 'b', 'c')",
            &[], &[], &mut store, 0,
        ).unwrap();
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

        let frame = eval_script(
            b"return tostring(_G._G == _G)",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"true".to_vec())));

        let frame =
            eval_script(b"return type(_G.tostring)", &[], &[], &mut store, 0).unwrap();
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
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::Integer(1));
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
            &[], &[], &mut store, 0,
        ).unwrap();
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
            &[], &[], &mut store, 0,
        ).expect_err("expected error for cross-type compare");
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
            &[], &[], &mut store, 0,
        ).unwrap();
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
        assert_eq!(
            frame,
            RespFrame::BulkString(Some(b"table+number".to_vec()))
        );

        // RHS-only metatable still fires.
        let frame = eval_script(
            b"local t = setmetatable({}, {__add=function(a, b) return type(a) .. '+' .. type(b) end}); return 1 + t",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(
            frame,
            RespFrame::BulkString(Some(b"number+table".to_vec()))
        );

        // Other binary arithmetic ops.
        for (src, want) in [
            (b"local t = setmetatable({}, {__sub=function() return 's' end}); return t - 1" as &[u8], b"s" as &[u8]),
            (b"local t = setmetatable({}, {__mul=function() return 'm' end}); return t * 2", b"m"),
            (b"local t = setmetatable({}, {__div=function() return 'd' end}); return t / 2", b"d"),
            (b"local t = setmetatable({}, {__mod=function() return 'o' end}); return t % 2", b"o"),
            (b"local t = setmetatable({}, {__pow=function() return 'p' end}); return t ^ 2", b"p"),
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
            &[], &[], &mut store, 0,
        ).unwrap();
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
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"META".to_vec())));

        // Pure-number arithmetic doesn't pay the metamethod-lookup cost.
        let frame = eval_script(b"return 2 + 3", &[], &[], &mut store, 0).unwrap();
        assert_eq!(frame, RespFrame::Integer(5));

        // Non-callable __add raises the standard "attempt to call" error.
        let err = eval_script(
            b"local t = setmetatable({}, {__add='nope'}); return t + 1",
            &[], &[], &mut store, 0,
        ).expect_err("expected error for string-__add");
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
        assert_eq!(
            frame,
            RespFrame::BulkString(Some(b"table+number".to_vec()))
        );

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
            &[], &[], &mut store, 0,
        ).expect_err("expected error for string-__concat");
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
            &[], &[], &mut store, 0,
        ).expect_err("expected error for string-__newindex");
        assert!(
            err.contains("attempt to index") && err.contains("string"),
            "wrong wording for non-callable __newindex: {err:?}"
        );

        // Chained table __newindex: outer → mid → back; only back gets it.
        let frame = eval_script(
            b"local back = {}; local mid = setmetatable({}, {__newindex=back}); local outer = setmetatable({}, {__newindex=mid}); outer.x = 1; return tostring(back.x)..','..tostring(rawget(mid, 'x'))..','..tostring(rawget(outer, 'x'))",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(
            frame,
            RespFrame::BulkString(Some(b"1,nil,nil".to_vec()))
        );

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
            &[], &[], &mut store, 0,
        ).unwrap();
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
            &[], &[], &mut store, 0,
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
            &[], &[], &mut store, 0,
        ).unwrap();
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
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::Integer(3));

        // Last-positional `...` after explicit fields: existing entries
        // plus the expanded varargs.
        let frame = eval_script(
            b"local function f(...) local t = {1, 2, ...}; return #t end; return f('a','b','c')",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::Integer(5));

        // Non-last `...` takes only the first value.
        let frame = eval_script(
            b"local function f(...) local t = {..., 99}; return #t end; return f('a','b','c')",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::Integer(2));

        // Last-positional function call expands all return values.
        let frame = eval_script(
            b"local function g() return 1, 2, 3 end; local t = {g()}; return #t",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::Integer(3));

        // Non-last function call takes only the first return value.
        let frame = eval_script(
            b"local function g() return 1, 2, 3 end; local t = {g(), 99}; return #t",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::Integer(2));

        // Method call as last positional also expands (single value
        // for string:upper, which only returns one value).
        let frame = eval_script(
            b"local s = 'abc'; local t = {'x', s:upper()}; return tostring(t[1])..','..tostring(t[2])",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(
            frame,
            RespFrame::BulkString(Some(b"x,ABC".to_vec()))
        );

        // Named fields don't count toward # but coexist with positional
        // varargs expansion.
        let frame = eval_script(
            b"local function f(...) local t = {x=1, ...}; return #t..':'..tostring(t.x) end; return f('a','b','c')",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(
            frame,
            RespFrame::BulkString(Some(b"3:1".to_vec()))
        );

        // Empty varargs yields an empty table.
        let frame = eval_script(
            b"local function f(...) local t = {...}; return #t end; return f()",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::Integer(0));

        // When `...` is NOT the last field (a named field follows it),
        // only the first vararg is taken.
        let frame = eval_script(
            b"local function f(...) local t = {..., x=1}; return #t..':'..tostring(t.x) end; return f('a','b','c')",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(
            frame,
            RespFrame::BulkString(Some(b"1:1".to_vec()))
        );
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
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::Integer(42));

        // Success path: chunk that uses varargs.
        let frame = eval_script(
            b"local f = loadstring('local a,b=...; return a+b'); return f(2, 3)",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::Integer(5));

        // Parse failure: returns nil + a chunk-labelled error string.
        let frame = eval_script(
            b"local f, err = loadstring('!!!'); return tostring(f) .. ';' .. tostring(err)",
            &[], &[], &mut store, 0,
        ).unwrap();
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
            &[], &[], &mut store, 0,
        ).unwrap();
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
            let src = format!(
                "local f, err = loadstring('!!!', '{prefix}myname'); return tostring(err)"
            );
            let frame =
                eval_script(src.as_bytes(), &[], &[], &mut store, 0).unwrap();
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
            &[], &[], &mut store, 0,
        ).unwrap();
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

        // load(non_function) reports vendored's "bad argument #1 to '?'"
        // wording — Lua 5.1 marks C closures lacking debug names with '?'.
        let frame = eval_script(
            b"local ok, err = pcall(load, 'src'); return tostring(err)",
            &[], &[], &mut store, 0,
        ).unwrap();
        let body = match frame {
            RespFrame::BulkString(Some(b)) => b,
            other => panic!("expected bulk string, got {other:?}"),
        };
        let s = String::from_utf8_lossy(&body);
        assert!(
            s.contains("bad argument #1 to '?'") && s.contains("got string"),
            "expected vendored bad-argument wording, got {s:?}"
        );

        // load != loadstring (separate function values).
        let frame = eval_script(
            b"return tostring(load == loadstring)",
            &[], &[], &mut store, 0,
        ).unwrap();
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
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(
            frame,
            RespFrame::BulkString(Some(b"boolean:true".to_vec()))
        );

        // nil preserves type.
        let frame = eval_script(
            b"local ok,err=pcall(function() error(nil) end); return type(err)",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(frame, RespFrame::BulkString(Some(b"nil".to_vec())));

        // Default-level number is coerced to a prefixed string (Lua
        // 5.1's lua_isstring-on-number quirk).
        let frame = eval_script(
            b"local ok,err=pcall(function() error(42) end); return type(err)..':'..tostring(err)",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(
            frame,
            RespFrame::BulkString(Some(b"string:user_script:1: 42".to_vec()))
        );

        // level=0 with a number preserves the number type.
        let frame = eval_script(
            b"local ok,err=pcall(function() error(42,0) end); return type(err)..':'..tostring(err)",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(
            frame,
            RespFrame::BulkString(Some(b"number:42".to_vec()))
        );

        // Nested pcall: inner consumes its own typed error; outer's
        // typed error round-trips independently.
        let frame = eval_script(
            b"local ok,err=pcall(function() pcall(function() error({}) end); error({deep=true}) end); return type(err)..':'..tostring(err.deep)",
            &[], &[], &mut store, 0,
        ).unwrap();
        assert_eq!(
            frame,
            RespFrame::BulkString(Some(b"table:true".to_vec()))
        );

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
        assert_eq!(
            frame,
            RespFrame::BulkString(Some(b"table:hi".to_vec()))
        );

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
        let frame = eval_script(b"local s = 'abc'; return s:len()", &[], &[], &mut store, 0)
            .unwrap();
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
        let err = eval_script(
            b"local s = 'abc'; s.fld()",
            &[],
            &[],
            &mut store,
            0,
        )
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
            (
                b"local x; x()",
                "attempt to call local 'x' (a nil value)",
            ),
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
        // Pins frankenredis-luaresp3hint. Upstream src/script_lua.c::
        // luaReplyToRedisReply checks for {map=...}, {set=...},
        // {double=...}, {big_number=...}, {verbatim_string=...} hint
        // tables AFTER ok/err but before the array-iteration fallback.
        // fr was returning empty arrays for ALL of them.

        let mut store = Store::new();

        // {map = {a=1, b=2}} → Map frame with 2 pairs.
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

        // {double = 3.14} → BulkString of the formatted double.
        let frame = eval_script(b"return {double = 3.14}", &[], &[], &mut store, 0)
            .expect("double hint should not error");
        assert_eq!(
            frame,
            RespFrame::BulkString(Some(b"3.14".to_vec()))
        );

        // {big_number = "12345..."} → BulkString of the bignum.
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
            RespFrame::BulkString(Some(b"1234567890123456789012345".to_vec()))
        );

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
            let LuaValue::Number(want) = expected else { unreachable!() };
            match frame {
                RespFrame::Integer(got) => assert_eq!(got as f64, *want, "src = {:?}", String::from_utf8_lossy(src)),
                RespFrame::BulkString(Some(bytes)) => {
                    let s = String::from_utf8_lossy(&bytes);
                    let got: f64 = s.parse().expect("numeric reply");
                    assert_eq!(got, *want, "src = {:?}", String::from_utf8_lossy(src));
                }
                other => panic!("expected number reply for {:?}, got {other:?}", String::from_utf8_lossy(src)),
            }
        }

        // Malformed hex still returns nil, matching upstream.
        for src in [
            b"return tonumber('0x') == nil".as_slice(),
            b"return tonumber('0xZ') == nil".as_slice(),
            b"return tonumber('0xFF.5') == nil".as_slice(),
            b"return tonumber('xyz') == nil".as_slice(),
        ] {
            let frame = eval_script(src, &[], &[], &mut store, 0)
                .unwrap_or_else(|e| panic!("eval {:?} failed: {e}", String::from_utf8_lossy(src)));
            assert_eq!(frame, RespFrame::Integer(1), "src = {:?}", String::from_utf8_lossy(src));
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
            (b"local function f(a,b) return a + b end return f(0xFF, 0x0F)", 270),
        ];
        for (src, expected) in cases {
            let frame = eval_script(src, &[], &[], &mut store, 0)
                .unwrap_or_else(|e| panic!("eval {:?} failed: {e}", String::from_utf8_lossy(src)));
            match frame {
                RespFrame::Integer(got) => assert_eq!(got, *expected, "src = {:?}", String::from_utf8_lossy(src)),
                other => panic!("expected Integer({expected}) for {:?}, got {other:?}", String::from_utf8_lossy(src)),
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
            let err = compile_check(src.as_bytes())
                .expect_err(&format!("expected error for {src:?}"));
            assert_eq!(
                err, expected_near,
                "wrong wording for {src:?}: {err}"
            );
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
            compile_check(src.as_bytes())
                .unwrap_or_else(|e| panic!("{src:?} should compile: {e}"));
        }
    }

    #[test]
    fn function_decl_errors_on_missing_table_path() {
        let mut store = Store::new();
        let err = eval_script(b"function a.b.c() return 1 end", &[], &[], &mut store, 0)
            .expect_err("expected error");
        assert!(
            err.contains("user_script:1: attempt to index a nil value"),
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
            Err("ERR wrong number of arguments for 'multi' command".to_string())
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
            Err("ERR wrong number of arguments for 'exec' command".to_string())
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
            Err("ERR wrong number of arguments for 'discard' command".to_string())
        );

        let discard = eval_script(b"return redis.call('DISCARD')", &[], &[], &mut store, 0);
        assert_eq!(discard, Err(SCRIPT_NOSCRIPT_ERROR.to_string()));

        let wrong_watch = eval_script(b"return redis.call('WATCH')", &[], &[], &mut store, 0);
        assert_eq!(
            wrong_watch,
            Err("ERR wrong number of arguments for 'watch' command".to_string())
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
            Err("ERR wrong number of arguments for 'unwatch' command".to_string())
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
            Err("ERR wrong number of arguments for 'acl' command".to_string())
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
            Err("ERR wrong number of arguments for 'acl|whoami' command".to_string())
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
        assert_eq!(
            help,
            Ok(RespFrame::BulkString(Some(
                b"ACL <subcommand> [<arg> [value] [opt] ...]. Subcommands are:".to_vec()
            )))
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
            Err("ERR wrong number of arguments for 'acl|help' command".to_string())
        );

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
            Err("ERR wrong number of arguments for 'auth' command".to_string())
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
            Err("ERR wrong number of arguments for 'sync' command".to_string())
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
        let r = eval_script(b"return string.format('%c', 65)", &[], &[], &mut store, 0).expect("c 65");
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
            (b"return math.abs()",   "user_script:1: bad argument #1 to 'abs' (number expected, got no value)"),
            (b"return math.ceil()",  "user_script:1: bad argument #1 to 'ceil' (number expected, got no value)"),
            (b"return math.floor()", "user_script:1: bad argument #1 to 'floor' (number expected, got no value)"),
            (b"return math.sqrt()",  "user_script:1: bad argument #1 to 'sqrt' (number expected, got no value)"),
            (b"return math.exp()",   "user_script:1: bad argument #1 to 'exp' (number expected, got no value)"),
            (b"return math.log()",   "user_script:1: bad argument #1 to 'log' (number expected, got no value)"),
            (b"return math.log10()", "user_script:1: bad argument #1 to 'log10' (number expected, got no value)"),
            (b"return math.sin()",   "user_script:1: bad argument #1 to 'sin' (number expected, got no value)"),
            (b"return math.cos()",   "user_script:1: bad argument #1 to 'cos' (number expected, got no value)"),
            (b"return math.tan()",   "user_script:1: bad argument #1 to 'tan' (number expected, got no value)"),
            (b"return math.deg()",   "user_script:1: bad argument #1 to 'deg' (number expected, got no value)"),
            (b"return math.rad()",   "user_script:1: bad argument #1 to 'rad' (number expected, got no value)"),
            (b"return math.modf()",  "user_script:1: bad argument #1 to 'modf' (number expected, got no value)"),
            (b"return math.frexp()", "user_script:1: bad argument #1 to 'frexp' (number expected, got no value)"),
            // math.* — arg-#2 missing
            (b"return math.fmod()",  "user_script:1: bad argument #2 to 'fmod' (number expected, got no value)"),
            (b"return math.fmod(1)", "user_script:1: bad argument #2 to 'fmod' (number expected, got no value)"),
            (b"return math.pow(1)",  "user_script:1: bad argument #2 to 'pow' (number expected, got no value)"),
            (b"return math.atan2(1)","user_script:1: bad argument #2 to 'atan2' (number expected, got no value)"),
            (b"return math.ldexp(1)","user_script:1: bad argument #2 to 'ldexp' (number expected, got no value)"),
            // string.* — arg #1 string missing
            (b"return string.len()",     "user_script:1: bad argument #1 to 'len' (string expected, got no value)"),
            (b"return string.lower()",   "user_script:1: bad argument #1 to 'lower' (string expected, got no value)"),
            (b"return string.upper()",   "user_script:1: bad argument #1 to 'upper' (string expected, got no value)"),
            (b"return string.reverse()", "user_script:1: bad argument #1 to 'reverse' (string expected, got no value)"),
            (b"return string.rep()",     "user_script:1: bad argument #1 to 'rep' (string expected, got no value)"),
            (b"return string.byte()",    "user_script:1: bad argument #1 to 'byte' (string expected, got no value)"),
            (b"return string.find()",    "user_script:1: bad argument #1 to 'find' (string expected, got no value)"),
            (b"return string.match()",   "user_script:1: bad argument #1 to 'match' (string expected, got no value)"),
            (b"return string.gmatch()",  "user_script:1: bad argument #1 to 'gmatch' (string expected, got no value)"),
            (b"return string.gsub()",    "user_script:1: bad argument #1 to 'gsub' (string expected, got no value)"),
            // string.* — arg-#2 missing
            (b"return string.sub('abc')",   "user_script:1: bad argument #2 to 'sub' (number expected, got no value)"),
            (b"return string.rep('a')",     "user_script:1: bad argument #2 to 'rep' (number expected, got no value)"),
            (b"return string.find('abc')",  "user_script:1: bad argument #2 to 'find' (string expected, got no value)"),
            (b"return string.match('abc')", "user_script:1: bad argument #2 to 'match' (string expected, got no value)"),
            (b"return string.gmatch('abc')","user_script:1: bad argument #2 to 'gmatch' (string expected, got no value)"),
            (b"return string.gsub('abc')",  "user_script:1: bad argument #2 to 'gsub' (string expected, got no value)"),
            // table.*
            (b"return table.sort()",  "user_script:1: bad argument #1 to 'sort' (table expected, got no value)"),
            (b"return table.maxn()",  "user_script:1: bad argument #1 to 'maxn' (table expected, got no value)"),
            // base
            (b"return tonumber()",    "user_script:1: bad argument #1 to 'tonumber' (value expected)"),
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
            (b"return math.abs(-5)",    RespFrame::Integer(5)),
            (b"return math.fmod(7,3)",  RespFrame::Integer(1)),
            (b"return math.pow(2,3)",   RespFrame::Integer(8)),
            (b"return string.len('abc')", RespFrame::Integer(3)),
            (b"return string.upper('abc')", RespFrame::BulkString(Some(b"ABC".to_vec()))),
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
        let r = eval_script(
            b"return table.concat({1,2,3})",
            &[],
            &[],
            &mut store,
            0,
        )
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
        let r = eval_script(
            b"return table.concat({})",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("empty table");
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
        let err = eval_script(b"table.insert({}, 1, 'x', 'extra')", &[], &[], &mut store, 0)
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
        let r = eval_script(b"return rawequal(1, 1)", &[], &[], &mut store, 0).expect("rawequal ok");
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
        let r = eval_script(
            b"return collectgarbage('count')",
            &[],
            &[],
            &mut store,
            0,
        )
        .expect("count");
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
    fn coroutine_yield_in_local_assign_errors_instead_of_returning_nil() {
        // (frankenredis-gdbca) bw15's PC tracking advances next_pc
        // past the whole containing stmt, so 'local v =
        // coroutine.yield()' would have v unbound on resume —
        // producing nil where upstream Lua passes the resume args.
        // Until the resume mechanism captures yield-call return
        // values, yield-as-non-bare-stmt is rejected with the
        // upstream Lua 5.1 boundary wording.
        let mut store = Store::new();
        let script = b"
            local co = coroutine.create(function()
                local v = coroutine.yield()
                return v
            end)
            local ok, err = coroutine.resume(co)
            return {tostring(ok), err}
        ";
        let result = eval_script(script, &[], &[], &mut store, 0);
        assert_eq!(
            result,
            Ok(RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"false".to_vec())),
                RespFrame::BulkString(Some(
                    b"attempt to yield across metamethod/C-call boundary".to_vec()
                )),
            ])))
        );
    }

    #[test]
    fn coroutine_yield_in_return_stmt_errors() {
        // (frankenredis-gdbca) Same defensive gate covers
        // 'return coroutine.yield()' — the return stmt's eval_expr
        // would never reach the ControlFlow::Return on resume.
        let mut store = Store::new();
        let script = b"
            local co = coroutine.create(function()
                return coroutine.yield()
            end)
            local ok, err = coroutine.resume(co)
            return {tostring(ok), err}
        ";
        let result = eval_script(script, &[], &[], &mut store, 0);
        assert_eq!(
            result,
            Ok(RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"false".to_vec())),
                RespFrame::BulkString(Some(
                    b"attempt to yield across metamethod/C-call boundary".to_vec()
                )),
            ])))
        );
    }

    #[test]
    fn coroutine_yield_inside_for_loop_errors_instead_of_silently_dropping_iterations() {
        // (frankenredis-ztawj) bw15's resume_coroutine + exec_
        // coroutine_stmts only track next_pc at the outer-stmt
        // level, so a yield from inside a for-loop body cannot be
        // resumed correctly — the loop would be skipped entirely
        // on the next resume, silently dropping iterations 2..N.
        // The fix detects this via nested_exec_stmts_depth > 0 at
        // yield time and returns the upstream Lua 5.1 wording
        // 'attempt to yield across metamethod/C-call boundary'
        // instead of producing wrong results.
        let mut store = Store::new();
        let script = b"
            local co = coroutine.create(function()
                for i = 1, 3 do
                    coroutine.yield(i)
                end
            end)
            local ok, err = coroutine.resume(co)
            return {tostring(ok), err}
        ";
        let result = eval_script(script, &[], &[], &mut store, 0);
        assert_eq!(
            result,
            Ok(RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"false".to_vec())),
                RespFrame::BulkString(Some(
                    b"attempt to yield across metamethod/C-call boundary".to_vec()
                )),
            ])))
        );
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
                panic!("expected error for {:?}, got {result:?}", String::from_utf8_lossy(body));
            };
            assert!(
                msg.contains("wrong number or type of arguments"),
                "expected wrong-args wording for {:?}, got {msg}",
                String::from_utf8_lossy(body),
            );
        }

        // Sanity: exactly-one-string args still produce the table.
        let ok = eval_script(b"return redis.status_reply('OK')", &[], &[], &mut store, 0).expect("status_reply ok");
        assert_eq!(ok, RespFrame::SimpleString("OK".to_string()));
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
            b"redis.log(0/0, 'msg') return 1".as_slice(),  // NaN
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
            &[], &[], &mut store, 0,
        ).expect_err("expected invalid capture");
        assert_eq!(err, "user_script:1: invalid capture index");

        // 2-capture pattern, %3 referenced.
        let err = eval_script(
            b"return string.gsub('a1', '(%a)(%d)', '%3')",
            &[], &[], &mut store, 0,
        ).expect_err("expected invalid capture");
        assert_eq!(err, "user_script:1: invalid capture index");

        // 0-capture pattern, %1: upstream special-cases this to mean
        // "whole match" (push_onecapture when i == 0 and level == 0).
        let r = eval_script(
            b"return string.gsub('abc', '.', '%1')",
            &[], &[], &mut store, 0,
        ).expect("%1 with 0 captures = whole match");
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
            &[], &[], &mut store, 0,
        ).expect("valid %0");
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
            &[], &[], &mut store, 0,
        ).expect("valid %1");
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
            (b"return math.min(1, 2, true)".as_slice(), "min", 3, "boolean"),
        ] {
            let err = eval_script(body, &[], &[], &mut store, 0).expect_err(
                &format!("expected wrong-type error for {:?}", String::from_utf8_lossy(body)),
            );
            let expected = format!(
                "user_script:1: bad argument #{idx} to '{fname}' (number expected, got {ty})"
            );
            assert_eq!(err, expected, "wrong error for {:?}", String::from_utf8_lossy(body));
        }

        // No-arg form continues to report arg #1 with "got no value".
        for fname in &["min", "max"] {
            let body = format!("return math.{fname}()");
            let err = eval_script(body.as_bytes(), &[], &[], &mut store, 0)
                .expect_err("no-arg error");
            assert_eq!(
                err,
                format!(
                    "user_script:1: bad argument #1 to '{fname}' (number expected, got no value)"
                ),
            );
        }

        // Valid calls still work.
        let r = eval_script(b"return math.min(3, 1, 2)", &[], &[], &mut store, 0).expect("valid min");
        assert_eq!(r, RespFrame::Integer(1));
        let r = eval_script(b"return math.max(3, 1, 2)", &[], &[], &mut store, 0).expect("valid max");
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
        let r = eval_script(
            b"return assert(42, 'msg')",
            &[], &[], &mut store, 0,
        ).expect("assert truthy");
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
            let err = eval_script(body, &[], &[], &mut store, 0).expect_err(
                &format!("expected no-value error for {:?}", String::from_utf8_lossy(body)),
            );
            let expected = format!(
                "user_script:1: bad argument #1 to '{fname}' (table expected, got no value)"
            );
            assert_eq!(err, expected, "wrong error for {:?}", String::from_utf8_lossy(body));
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
            let err = eval_script(body, &[], &[], &mut store, 0).expect_err(
                &format!("expected nil error for {:?}", String::from_utf8_lossy(body)),
            );
            let expected = format!(
                "user_script:1: bad argument #1 to '{fname}' (table expected, got nil)"
            );
            assert_eq!(err, expected, "wrong error for {:?}", String::from_utf8_lossy(body));
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
            let r = eval_script(body, &[], &[], &mut store, 0).expect(
                &format!("expected number for {:?}", String::from_utf8_lossy(body)),
            );
            assert_eq!(
                r, RespFrame::Integer(255),
                "wrong result for {:?}",
                String::from_utf8_lossy(body),
            );
        }

        // Negative hex string with explicit base.
        let r = eval_script(b"return tonumber('-ff', 16)", &[], &[], &mut store, 0)
            .expect("neg hex");
        assert_eq!(r, RespFrame::Integer(-255));

        // Float base truncates to integer.
        let r = eval_script(b"return tonumber('10', 10.5)", &[], &[], &mut store, 0)
            .expect("float base");
        assert_eq!(r, RespFrame::Integer(10));

        // Explicit nil base defaults to no base (string-as-decimal).
        let r = eval_script(b"return tonumber('10', nil)", &[], &[], &mut store, 0)
            .expect("nil base");
        assert_eq!(r, RespFrame::Integer(10));

        // Base out of range: 1, 37, -1, 0 all error with the prefix.
        for body in &[
            b"return tonumber('0', 1)".as_slice(),
            b"return tonumber('0', 37)".as_slice(),
            b"return tonumber('0', -1)".as_slice(),
            b"return tonumber('0', 0)".as_slice(),
        ] {
            let err = eval_script(body, &[], &[], &mut store, 0).expect_err(
                &format!("expected base-out-of-range for {:?}", String::from_utf8_lossy(body)),
            );
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
            let err = eval_script(body, &[], &[], &mut store, 0).expect_err(
                &format!("expected type-error for {:?}", String::from_utf8_lossy(body)),
            );
            let expected = format!(
                "user_script:1: bad argument #2 to 'format' (string expected, got {ty})"
            );
            assert_eq!(err, expected, "wrong error for {:?}", String::from_utf8_lossy(body));
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
    fn lua_attempt_to_X_errors_include_accessor_label_9ckvq() {
        // (frankenredis-9ckvq) Lua 5.1's lvm.c::luaG_typeerror reports
        // the variable name of the offending operand alongside the type.
        // The Concat and Call paths in fr already do this; verify Index
        // (Field and Index forms), unary -, unary #, and binary
        // arithmetic now do too. Probed against vendored Redis 7.2.4.
        let mut store = Store::new();

        let cases: &[(&[u8], &str)] = &[
            (b"local t=nil; return t.field",
             "user_script:1: attempt to index local 't' (a nil value)"),
            (b"local t=nil; return t[1]",
             "user_script:1: attempt to index local 't' (a nil value)"),
            (b"local mylocal=nil; return mylocal.f",
             "user_script:1: attempt to index local 'mylocal' (a nil value)"),
            (b"local t={a=nil}; return t.a.b",
             "user_script:1: attempt to index field 'a' (a nil value)"),
            (b"local t={}; return t.missing.deep",
             "user_script:1: attempt to index field 'missing' (a nil value)"),
            (b"local b=true; return b.f",
             "user_script:1: attempt to index local 'b' (a boolean value)"),
            (b"local nm=5; return nm.f",
             "user_script:1: attempt to index local 'nm' (a number value)"),
            (b"local x=nil; return x+1",
             "user_script:1: attempt to perform arithmetic on local 'x' (a nil value)"),
            (b"local y=nil; return 1+y",
             "user_script:1: attempt to perform arithmetic on local 'y' (a nil value)"),
            (b"local z=nil; return -z",
             "user_script:1: attempt to perform arithmetic on local 'z' (a nil value)"),
            (b"local s=nil; return #s",
             "user_script:1: attempt to get length of local 's' (a nil value)"),
            (b"local t={}; return #t.missing",
             "user_script:1: attempt to get length of field 'missing' (a nil value)"),
            (b"local t={}; return t+1",
             "user_script:1: attempt to perform arithmetic on local 't' (a table value)"),
            (b"local n='abc'; return n+1",
             "user_script:1: attempt to perform arithmetic on local 'n' (a string value)"),
        ];
        for (body, expected) in cases {
            let err = eval_script(body, &[], &[], &mut store, 0)
                .expect_err("error expected");
            assert_eq!(
                err, *expected,
                "wrong error for {:?}",
                String::from_utf8_lossy(body),
            );
        }

        // Regression: no syntactic label available → fall back to the
        // unlabeled wording (vendored does the same when the operand has
        // no resolvable variable, e.g. a function-call result).
        let err = eval_script(
            b"return (function() return nil end)() + 1",
            &[], &[], &mut store, 0,
        ).expect_err("no label");
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
            &[], &[], &mut store, 0,
        ).expect("getmetatable masked");
        assert_eq!(r, RespFrame::BulkString(Some(b"locked".to_vec())));

        // getmetatable still returns the real metatable when
        // __metatable is absent (existing behavior must not regress).
        let r = eval_script(
            b"local mt={}; setmetatable({}, mt); return type(getmetatable(setmetatable({},mt)))",
            &[], &[], &mut store, 0,
        ).expect("getmetatable plain");
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
            &[], &[], &mut store, 0,
        ).expect("setmetatable arity");
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
            &[], &[], &mut store, 0,
        ).expect("pcall(error,'msg')");
        assert_eq!(r, RespFrame::BulkString(Some(b"string:msg".to_vec())));

        // pcall(error, 42): same — number coerced to string, no prefix.
        let r = eval_script(
            b"local ok,err=pcall(error, 42); return type(err)..':'..tostring(err)",
            &[], &[], &mut store, 0,
        ).expect("pcall(error,42)");
        assert_eq!(r, RespFrame::BulkString(Some(b"string:42".to_vec())));

        // pcall(error, 'msg', 2): level 2 walks past pcall (C) to the
        // script chunk (Lua) → prefix added.
        let r = eval_script(
            b"local ok,err=pcall(error, 'msg', 2); return type(err)..':'..tostring(err)",
            &[], &[], &mut store, 0,
        ).expect("pcall(error,'msg',2)");
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
        let err = eval_script(
            b"local t={} t[nil]=1",
            &[], &[], &mut store, 0,
        ).expect_err("nil key");
        assert_eq!(err, "user_script:1: table index is nil");

        // Direct t[0/0]=1 syntax raises with NaN message.
        let err = eval_script(
            b"local t={} t[0/0]=1",
            &[], &[], &mut store, 0,
        ).expect_err("NaN key");
        assert_eq!(err, "user_script:1: table index is NaN");

        // -NaN must also be rejected (sign bit doesn't help).
        let err = eval_script(
            b"local t={} t[-(0/0)]=1",
            &[], &[], &mut store, 0,
        ).expect_err("-NaN key");
        assert_eq!(err, "user_script:1: table index is NaN");

        // Table constructor {[nil]=1} raises at construction time.
        let err = eval_script(
            b"return {[nil]=1}",
            &[], &[], &mut store, 0,
        ).expect_err("ctor nil key");
        assert_eq!(err, "user_script:1: table index is nil");

        // Table constructor {[0/0]=1} raises at construction time.
        let err = eval_script(
            b"return {[0/0]=1}",
            &[], &[], &mut store, 0,
        ).expect_err("ctor NaN key");
        assert_eq!(err, "user_script:1: table index is NaN");

        // Positive infinity is a valid key — must NOT raise.
        let r = eval_script(
            b"local t={} t[1/0]=42 return t[1/0]",
            &[], &[], &mut store, 0,
        ).expect("inf key ok");
        assert_eq!(r, RespFrame::Integer(42));

        // Negative infinity is a valid key — must NOT raise.
        let r = eval_script(
            b"local t={} t[-1/0]=42 return t[-1/0]",
            &[], &[], &mut store, 0,
        ).expect("-inf key ok");
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
            &[], &[], &mut store, 0,
        ).expect("frontier find");
        assert_eq!(r, RespFrame::Integer(1));

        // Multiple matches via gsub at every word boundary.
        let r = eval_script(
            b"return string.gsub('THE QUICK BROWN', '%f[%a]', '|')",
            &[], &[], &mut store, 0,
        ).expect("frontier gsub many");
        // Returns (result, count) — the script returns the string only,
        // which is the first multi-return value.
        if let RespFrame::BulkString(Some(bytes)) = r {
            assert_eq!(bytes, b"|THE |QUICK |BROWN");
        } else {
            panic!("expected bulk string, got {r:?}");
        }

        let r = eval_script(
            b"return string.gsub('a b c', '%f[%a]', '!')",
            &[], &[], &mut store, 0,
        ).expect("frontier gsub a b c");
        assert_eq!(r, RespFrame::BulkString(Some(b"!a !b !c".to_vec())));

        // match returns the captured word at the first word boundary.
        let r = eval_script(
            b"return string.match('Hello World', '%f[%w]%w+')",
            &[], &[], &mut store, 0,
        ).expect("frontier match");
        assert_eq!(r, RespFrame::BulkString(Some(b"Hello".to_vec())));

        // Anchored frontier still works.
        let r = eval_script(
            b"return string.find('abc', '^%f[%a]')",
            &[], &[], &mut store, 0,
        ).expect("anchored frontier");
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
            let err = eval_script(body, &[], &[], &mut store, 0).expect_err(
                &format!("expected malformed-pattern error for {:?}", String::from_utf8_lossy(body)),
            );
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
            let err = eval_script(body, &[], &[], &mut store, 0).expect_err(
                &format!("expected missing-] error for {:?}", String::from_utf8_lossy(body)),
            );
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
            &[], &[], &mut store, 0,
        ).expect("plain find must not validate pattern");

        // Valid patterns continue to match without error.
        let _ = eval_script(
            b"return string.find('hello123', '%d+')",
            &[], &[], &mut store, 0,
        ).expect("valid pattern must match");
        let _ = eval_script(
            b"return string.gsub('hello', '(l)', '<%1>')",
            &[], &[], &mut store, 0,
        ).expect("valid gsub must match");
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
            let err = eval_script(body, &[], &[], &mut store, 0).expect_err(
                &format!("expected concat error for {:?}", String::from_utf8_lossy(body)),
            );
            let expected = format!(
                "user_script:1: bad argument #2 to 'concat' (string expected, got {ty})"
            );
            assert_eq!(err, expected, "wrong error for {:?}", String::from_utf8_lossy(body));
        }

        // Number separator coerces to its string representation.
        let ok = eval_script(
            b"return table.concat({1,2,3}, 5)",
            &[], &[], &mut store, 0,
        ).expect("number sep should work");
        assert_eq!(ok, RespFrame::BulkString(Some(b"15253".to_vec())));

        // String separator works.
        let ok = eval_script(
            b"return table.concat({1,2,3}, ', ')",
            &[], &[], &mut store, 0,
        ).expect("string sep should work");
        assert_eq!(ok, RespFrame::BulkString(Some(b"1, 2, 3".to_vec())));

        // Missing/nil sep -> empty separator (unchanged jwkhc behavior).
        let ok = eval_script(
            b"return table.concat({1,2,3})",
            &[], &[], &mut store, 0,
        ).expect("nil sep");
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
            &[], &[], &mut store, 0,
        ).expect_err("expected interval-empty error");
        assert_eq!(
            err,
            "user_script:1: bad argument #1 to 'random' (interval is empty)",
        );

        // 2-arg, m>n -> arg #2 with prefix.
        let err = eval_script(
            b"math.randomseed(1); return math.random(5, 1)",
            &[], &[], &mut store, 0,
        ).expect_err("expected interval-empty error");
        assert_eq!(
            err,
            "user_script:1: bad argument #2 to 'random' (interval is empty)",
        );

        // 3+ args -> wrong number of arguments.
        let err = eval_script(
            b"return math.random(1, 2, 3)",
            &[], &[], &mut store, 0,
        ).expect_err("expected wrong-number-of-args error");
        assert_eq!(err, "user_script:1: wrong number of arguments");

        let err = eval_script(
            b"return math.random(1, 2, 3, 4)",
            &[], &[], &mut store, 0,
        ).expect_err("expected wrong-number-of-args error");
        assert_eq!(err, "user_script:1: wrong number of arguments");

        // Valid calls still produce values in the expected range.
        let r = eval_script(
            b"math.randomseed(42); local v = math.random(1, 10); return v",
            &[], &[], &mut store, 0,
        ).expect("valid call");
        let RespFrame::Integer(n) = r else {
            panic!("expected integer, got {r:?}");
        };
        assert!((1..=10).contains(&n), "math.random(1,10) returned {n}");

        // 1-arg valid call.
        let r = eval_script(
            b"math.randomseed(42); local v = math.random(5); return v",
            &[], &[], &mut store, 0,
        ).expect("valid 1-arg call");
        let RespFrame::Integer(n) = r else {
            panic!("expected integer, got {r:?}");
        };
        assert!((1..=5).contains(&n), "math.random(5) returned {n}");
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
            let err = eval_script(body, &[], &[], &mut store, 0).expect_err(
                &format!("expected error for {:?}", String::from_utf8_lossy(body)),
            );
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
                panic!("expected bulk string for {:?}, got {result:?}", String::from_utf8_lossy(body));
            };
            let hex = String::from_utf8(bytes).unwrap();
            assert_eq!(
                hex, sha1_empty,
                "non-string types must hash to empty for {:?}",
                String::from_utf8_lossy(body),
            );
        }
        // Strings and numbers continue to hash their byte representation.
        let ok = eval_script(b"return redis.sha1hex('hello')", &[], &[], &mut store, 0).expect("eval");
        let RespFrame::BulkString(Some(bytes)) = ok else { panic!("expected sha bulk") };
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d", // sha1("hello")
        );
    }
}
