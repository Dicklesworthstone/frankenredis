// Minimal Lua 5.1 evaluator for Redis scripting.
//
// Supports: variables (local/global), arithmetic, string concat, comparisons,
// logical ops, if/elseif/else, numeric for, generic for (pairs/ipairs),
// while, repeat/until, tables, function calls/definitions, redis.call/pcall,
// KEYS/ARGV, and standard library functions.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use fr_protocol::RespFrame;
use fr_store::Store;

use crate::dispatch_argv;

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

fn lua_raw_equal(a: &LuaValue, b: &LuaValue) -> bool {
    match (a, b) {
        (LuaValue::Nil, LuaValue::Nil) => true,
        (LuaValue::Bool(x), LuaValue::Bool(y)) => x == y,
        (LuaValue::Number(x), LuaValue::Number(y)) => x == y,
        (LuaValue::Str(x), LuaValue::Str(y)) => x == y,
        _ => false,
    }
}

fn lua_bad_table_arg(function: &str, index: usize, value: &LuaValue) -> String {
    format!(
        "bad argument #{index} to '{function}' (table expected, got {})",
        value.type_name()
    )
}

fn lua_bad_number_arg(function: &str, index: usize, value: &LuaValue) -> String {
    format!(
        "bad argument #{index} to '{function}' (number expected, got {})",
        value.type_name()
    )
}

fn lua_table_arg<'a>(
    function: &str,
    index: usize,
    value: &'a LuaValue,
) -> Result<&'a LuaTable, String> {
    match value {
        LuaValue::Table(table) => Ok(table),
        _ => Err(lua_bad_table_arg(function, index, value)),
    }
}

fn lua_required_integer_arg(function: &str, index: usize, value: &LuaValue) -> Result<i64, String> {
    match value.to_number() {
        Some(number) if number.is_finite() => Ok(number as i64),
        _ => Err(lua_bad_number_arg(function, index, value)),
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
        match self {
            LuaValue::Nil => Err("Lua nil can't be used as a Redis argument".to_string()),
            LuaValue::Bool(b) => {
                if *b {
                    Ok(b"1".to_vec())
                } else {
                    Ok(b"0".to_vec())
                }
            }
            LuaValue::Number(n) => {
                if *n == (*n as i64) as f64 && n.is_finite() {
                    Ok(format!("{}", *n as i64).into_bytes())
                } else {
                    Ok(format!("{n}").into_bytes())
                }
            }
            LuaValue::Str(s) => Ok(s.clone()),
            LuaValue::Table(_) => Err("Lua table can't be used as a Redis argument".to_string()),
            LuaValue::Function(_) | LuaValue::RustFunction(_) => {
                Err("Lua function can't be used as a Redis argument".to_string())
            }
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
                if *n == (*n as i64) as f64 && n.is_finite() {
                    format!("{}", *n as i64).into_bytes()
                } else {
                    format!("{n}").into_bytes()
                }
            }
            LuaValue::Str(s) => s.clone(),
            LuaValue::Table(_) => b"table".to_vec(),
            LuaValue::Function(_) | LuaValue::RustFunction(_) => b"function".to_vec(),
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
                while let Some(d) = self.peek_byte() {
                    if d.is_ascii_digit() || d == b'.' {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                // Handle hex 0x prefix
                if self.pos - start >= 2
                    && self.src[start] == b'0'
                    && (self.src[start + 1] == b'x' || self.src[start + 1] == b'X')
                {
                    while let Some(d) = self.peek_byte() {
                        if d.is_ascii_hexdigit() {
                            self.pos += 1;
                        } else {
                            break;
                        }
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
                let n = if s.starts_with("0x") || s.starts_with("0X") {
                    i64::from_str_radix(&s[2..], 16)
                        .map(|i| i as f64)
                        .map_err(|e| e.to_string())?
                } else {
                    s.parse::<f64>().map_err(|e| e.to_string())?
                };
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
                    Err("unexpected character '~'".to_string())
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
                Err(format!("unexpected character '{}'", b as char))
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
            let rhs = self.parse_expr_list()?;
            Ok(Stmt::Assign(lhs, rhs))
        } else {
            // Expression statement (must be a call)
            Ok(Stmt::Expression(expr))
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

enum ControlFlow {
    None,
    Return(Vec<LuaValue>),
    Break,
}

pub struct LuaState<'a> {
    pub store: &'a mut Store,
    pub now_ms: u64,
    globals: HashMap<String, LuaValue>,
    call_depth: usize,
    iterations: u64,
    rng_seed: u64,
}

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

struct Env {
    scopes: Vec<Scope>,
}

impl Env {
    fn new() -> Self {
        Self {
            scopes: vec![Scope::new()],
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
        Self {
            scopes: captured
                .iter()
                .map(|locals| Scope {
                    locals: locals.clone(),
                })
                .collect(),
        }
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
            "rawlen",
            "setmetatable",
            "getmetatable",
            "assert",
            "print",
            "xpcall",
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

        let rng_seed = store.rng_seed;
        Self {
            store,
            now_ms,
            globals,
            call_depth: 0,
            iterations: 0,
            rng_seed,
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
        // Replication mode constants
        redis_table.set(LuaValue::Str(b"REPL_NONE".to_vec()), LuaValue::Number(0.0));
        redis_table.set(LuaValue::Str(b"REPL_SLAVE".to_vec()), LuaValue::Number(1.0));
        redis_table.set(LuaValue::Str(b"REPL_AOF".to_vec()), LuaValue::Number(2.0));
        redis_table.set(LuaValue::Str(b"REPL_ALL".to_vec()), LuaValue::Number(3.0));
        self.globals
            .insert("redis".to_string(), LuaValue::Table(redis_table));

        // Set up os table (Redis Lua only exposes os.clock)
        let os_table = LuaTable::new();
        os_table.set(
            LuaValue::Str(b"clock".to_vec()),
            LuaValue::RustFunction("os.clock".to_string()),
        );
        self.globals
            .insert("os".to_string(), LuaValue::Table(os_table));

        // Coroutine stubs — Redis does not support coroutines in EVAL scripts.
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
        let mut env = Env::new();
        let mut varargs = Vec::new();
        match self.exec_block(&stmts, &mut env, &mut varargs)? {
            ControlFlow::Return(vals) => Ok(vals.into_iter().next().unwrap_or(LuaValue::Nil)),
            _ => Ok(LuaValue::Nil),
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
        for stmt in stmts {
            self.iterations += 1;
            if self.iterations > MAX_ITERATIONS {
                return Err("script exceeded maximum iteration count".to_string());
            }
            let cf = self.exec_stmt(stmt, env, varargs)?;
            match cf {
                ControlFlow::None => {}
                other => return Ok(other),
            }
        }
        Ok(ControlFlow::None)
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
                self.eval_expr(expr, env, varargs)?;
                Ok(ControlFlow::None)
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
                if st == 0.0 {
                    return Err("'for' step is zero".to_string());
                }
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
                });
                if names.len() == 1 {
                    self.globals.insert(names[0].clone(), func);
                } else {
                    // Nested field assignment: a.b.c = func
                    self.set_nested_field(names, func);
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
                });
                env.set_local(name, func);
                Ok(ControlFlow::None)
            }
        }
    }

    fn set_nested_field(&mut self, names: &[String], value: LuaValue) {
        if names.len() < 2 {
            return;
        }
        let root_name = &names[0];
        let mut current = self
            .globals
            .get(root_name)
            .cloned()
            .unwrap_or(LuaValue::Nil);
        // Navigate to the parent table
        let mut path: Vec<LuaValue> = vec![current.clone()];
        for name in &names[1..names.len() - 1] {
            current = match &current {
                LuaValue::Table(t) => t.get(&LuaValue::Str(name.as_bytes().to_vec())),
                _ => LuaValue::Nil,
            };
            path.push(current.clone());
        }
        // Set the value in the innermost table
        let last_field = names.last().unwrap();
        if let LuaValue::Table(t) = path.last_mut().unwrap() {
            t.set(LuaValue::Str(last_field.as_bytes().to_vec()), value);
            // Rebuild the chain
            let mut val = path.pop().unwrap();
            for i in (0..names.len() - 2).rev() {
                if let LuaValue::Table(parent) = &mut path[i] {
                    parent.set(LuaValue::Str(names[i + 1].as_bytes().to_vec()), val);
                    val = path[i].clone();
                }
            }
            self.globals.insert(root_name.clone(), val);
        }
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
                    self.globals.insert(name.clone(), value);
                }
            }
            Expr::Index(table_expr, key_expr) => {
                let table = self.eval_expr(table_expr, env, varargs)?;
                let key = self.eval_expr(key_expr, env, varargs)?;
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
            t.set(key, value);
            self.write_back_table_expr(table_expr, table, env, varargs)?;
            Ok(())
        } else {
            Err(format!("attempt to index a {} value", table.type_name()))
        }
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
                        self.eval_binop(&lv, op, &rv)
                    }
                }
            }
            Expr::UnaryOp(op, inner) => {
                let val = self.eval_expr(inner, env, varargs)?;
                match op {
                    UnaryOp::Neg => {
                        let n = val
                            .to_number()
                            .ok_or("attempt to perform arithmetic on a non-number")?;
                        Ok(LuaValue::Number(-n))
                    }
                    UnaryOp::Not => Ok(LuaValue::Bool(!val.is_truthy())),
                    UnaryOp::Len => match &val {
                        LuaValue::Str(s) => Ok(LuaValue::Number(s.len() as f64)),
                        LuaValue::Table(t) => Ok(LuaValue::Number(t.len() as f64)),
                        _ => Err(format!(
                            "attempt to get length of a {} value",
                            val.type_name()
                        )),
                    },
                }
            }
            Expr::Index(table_expr, key_expr) => {
                let table = self.eval_expr(table_expr, env, varargs)?;
                let key = self.eval_expr(key_expr, env, varargs)?;
                match &table {
                    LuaValue::Table(t) => Ok(t.get_with_index(&key)),
                    _ => Err(format!("attempt to index a {} value", table.type_name())),
                }
            }
            Expr::Field(table_expr, field) => {
                let table = self.eval_expr(table_expr, env, varargs)?;
                match &table {
                    LuaValue::Table(t) => {
                        Ok(t.get_with_index(&LuaValue::Str(field.as_bytes().to_vec())))
                    }
                    _ => Err(format!("attempt to index a {} value", table.type_name())),
                }
            }
            Expr::Call(func_expr, args) => {
                let func = self.eval_expr(func_expr, env, varargs)?;
                let mut arg_vals = self.eval_call_args(args, env, varargs)?;
                let results = self.call_function(&func, &mut arg_vals, env, varargs)?;
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
                        t.get_with_index(&LuaValue::Str(method.as_bytes().to_vec()))
                    }
                    _ => {
                        return Err(format!(
                            "attempt to call method on a {} value",
                            obj.type_name()
                        ));
                    }
                };
                let mut arg_vals = vec![obj.clone()];
                arg_vals.extend(self.eval_call_args(args, env, varargs)?);
                let results = self.call_function(&func, &mut arg_vals, env, varargs)?;
                Ok(results.into_iter().next().unwrap_or(LuaValue::Nil))
            }
            Expr::TableConstructor(fields) => {
                let table = LuaTable::new();
                let mut auto_idx = 1usize;
                for field in fields {
                    match field {
                        TableField::Positional(expr) => {
                            let val = self.eval_expr(expr, env, varargs)?;
                            table.set(LuaValue::Number(auto_idx as f64), val);
                            auto_idx += 1;
                        }
                        TableField::Named(name, expr) => {
                            let val = self.eval_expr(expr, env, varargs)?;
                            table.set(LuaValue::Str(name.as_bytes().to_vec()), val);
                        }
                        TableField::Index(key_expr, val_expr) => {
                            let key = self.eval_expr(key_expr, env, varargs)?;
                            let val = self.eval_expr(val_expr, env, varargs)?;
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
                        let results = self.call_function(&func, &mut arg_vals, env, varargs)?;
                        vals.extend(results);
                    }
                    Expr::MethodCall(obj_expr, method, call_args) => {
                        let obj = self.eval_expr(obj_expr, env, varargs)?;
                        let func = match &obj {
                            LuaValue::Table(t) => t.get(&LuaValue::Str(method.as_bytes().to_vec())),
                            _ => LuaValue::Nil,
                        };
                        let mut arg_vals = vec![obj];
                        arg_vals.extend(self.eval_call_args(call_args, env, varargs)?);
                        let results = self.call_function(&func, &mut arg_vals, env, varargs)?;
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

    fn eval_binop(&self, lv: &LuaValue, op: &BinOp, rv: &LuaValue) -> Result<LuaValue, String> {
        match op {
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod | BinOp::Pow => {
                let a = lv
                    .to_number()
                    .ok_or("attempt to perform arithmetic on a non-number")?;
                let b = rv
                    .to_number()
                    .ok_or("attempt to perform arithmetic on a non-number")?;
                let result = match op {
                    BinOp::Add => a + b,
                    BinOp::Sub => a - b,
                    BinOp::Mul => a * b,
                    BinOp::Div => a / b,
                    BinOp::Mod => a - (a / b).floor() * b,
                    BinOp::Pow => a.powf(b),
                    _ => unreachable!(),
                };
                Ok(LuaValue::Number(result))
            }
            BinOp::Concat => {
                let mut a = lv.to_display_string();
                a.extend_from_slice(&rv.to_display_string());
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
                        _ => unreachable!(),
                    },
                    (LuaValue::Str(a), LuaValue::Str(b)) => match op {
                        BinOp::Lt => a < b,
                        BinOp::Gt => a > b,
                        BinOp::Le => a <= b,
                        BinOp::Ge => a >= b,
                        _ => unreachable!(),
                    },
                    _ => {
                        return Err("attempt to compare incompatible types".to_string());
                    }
                };
                Ok(LuaValue::Bool(result))
            }
            BinOp::And | BinOp::Or => unreachable!("handled in eval_expr"),
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
        let result = match func {
            LuaValue::RustFunction(name) => self.call_builtin(name, args, env),
            LuaValue::Function(lua_func) => {
                // Start with captured upvalues (if any) for lexical scoping
                let mut new_env = match &lua_func.captured_env {
                    Some(captured) => Env::from_captured(captured),
                    None => Env::new(),
                };
                // Push a new scope for function parameters/locals
                new_env.push_scope();
                // For `local function f(x) ... end`, inject the function
                // into its own scope so self-recursion works.
                if let Some(name) = &lua_func.self_name {
                    new_env.set_local(name, func.clone());
                }
                for (i, param) in lua_func.params.iter().enumerate() {
                    let val = args.get(i).cloned().unwrap_or(LuaValue::Nil);
                    new_env.set_local(param, val);
                }
                let mut func_varargs = if lua_func.is_variadic {
                    args.get(lua_func.params.len()..).unwrap_or(&[]).to_vec()
                } else {
                    Vec::new()
                };
                match self.exec_stmts(&lua_func.body, &mut new_env, &mut func_varargs) {
                    Ok(ControlFlow::Return(vals)) => Ok(vals),
                    Ok(_) => Ok(vec![LuaValue::Nil]),
                    Err(e) => Err(e),
                }
            }
            LuaValue::Nil => Err("attempt to call a nil value".to_string()),
            other => Err(format!("attempt to call a {} value", other.type_name())),
        };
        self.call_depth -= 1;
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
                let msg = args
                    .first()
                    .map(|a| a.to_display_string())
                    .unwrap_or_default();
                let msg_str = String::from_utf8_lossy(&msg).to_string();
                Ok(vec![LuaValue::Table({
                    let t = LuaTable::new();
                    t.set(
                        LuaValue::Str(b"err".to_vec()),
                        LuaValue::Str(msg_str.into_bytes()),
                    );
                    t
                })])
            }
            "redis.status_reply" => {
                let msg = args
                    .first()
                    .map(|a| a.to_display_string())
                    .unwrap_or_default();
                Ok(vec![LuaValue::Table({
                    let t = LuaTable::new();
                    t.set(LuaValue::Str(b"ok".to_vec()), LuaValue::Str(msg));
                    t
                })])
            }
            "redis.log" => {
                // Silently ignore log calls
                Ok(vec![LuaValue::Nil])
            }
            "redis.replicate_commands" => {
                // No-op: effects replication was removed in Redis 7.0+
                // Always returns true for compatibility
                Ok(vec![LuaValue::Bool(true)])
            }
            "redis.set_repl" => {
                // No-op: replication control stub
                Ok(vec![LuaValue::Nil])
            }
            "redis.breakpoint" => {
                // No-op: debugging stub
                Ok(vec![LuaValue::Nil])
            }
            "redis.debug" => {
                // No-op: debugging stub
                Ok(vec![LuaValue::Nil])
            }
            "redis.sha1hex" => {
                let data = args
                    .first()
                    .map(|a| a.to_display_string())
                    .unwrap_or_default();
                let hex = fr_store::sha1_hex_public(&data);
                Ok(vec![LuaValue::Str(hex.into_bytes())])
            }
            "tonumber" => {
                let val = args.first().cloned().unwrap_or(LuaValue::Nil);
                let base = match args.get(1) {
                    Some(base_value) => {
                        let Some(base) = base_value.to_number() else {
                            return Err(
                                "bad argument #2 to 'tonumber' (base out of range)".to_string()
                            );
                        };
                        if !base.is_finite() || base.fract() != 0.0 {
                            return Err(
                                "bad argument #2 to 'tonumber' (base out of range)".to_string()
                            );
                        }
                        let base = base as u32;
                        if !(2..=36).contains(&base) {
                            return Err(
                                "bad argument #2 to 'tonumber' (base out of range)".to_string()
                            );
                        }
                        Some(base)
                    }
                    None => None,
                };
                match &val {
                    LuaValue::Number(n) => Ok(vec![LuaValue::Number(*n)]),
                    LuaValue::Str(s) => {
                        let s_str = std::str::from_utf8(s).unwrap_or("");
                        let trimmed = s_str.trim();
                        if let Some(base) = base {
                            match i64::from_str_radix(trimmed, base) {
                                Ok(n) => Ok(vec![LuaValue::Number(n as f64)]),
                                Err(_) => Ok(vec![LuaValue::Nil]),
                            }
                        } else {
                            match trimmed.parse::<f64>() {
                                Ok(n) => Ok(vec![LuaValue::Number(n)]),
                                Err(_) => Ok(vec![LuaValue::Nil]),
                            }
                        }
                    }
                    _ => Ok(vec![LuaValue::Nil]),
                }
            }
            "tostring" => {
                let val = args.first().cloned().unwrap_or(LuaValue::Nil);
                Ok(vec![LuaValue::Str(val.to_display_string())])
            }
            "type" => {
                let val = args.first().cloned().unwrap_or(LuaValue::Nil);
                Ok(vec![LuaValue::Str(val.type_name().as_bytes().to_vec())])
            }
            "error" => {
                let msg = args
                    .first()
                    .map(|a| a.to_display_string())
                    .unwrap_or_default();
                Err(String::from_utf8_lossy(&msg).to_string())
            }
            "assert" => {
                let val = args.first().cloned().unwrap_or(LuaValue::Nil);
                if val.is_truthy() {
                    Ok(args.to_vec())
                } else {
                    let msg = args
                        .get(1)
                        .map(|a| String::from_utf8_lossy(&a.to_display_string()).to_string())
                        .unwrap_or_else(|| "assertion failed!".to_string());
                    Err(msg)
                }
            }
            "pcall" => {
                let func = args.first().cloned().unwrap_or(LuaValue::Nil);
                let mut call_args_vec = args.get(1..).unwrap_or(&[]).to_vec();
                match self.call_function(&func, &mut call_args_vec, env, &mut Vec::new()) {
                    Ok(vals) => {
                        let mut new_vals = Vec::with_capacity(vals.len() + 1);
                        new_vals.push(LuaValue::Bool(true));
                        new_vals.extend(vals);
                        Ok(new_vals)
                    }
                    Err(msg) => Ok(vec![LuaValue::Bool(false), LuaValue::Str(msg.into_bytes())]),
                }
            }
            "xpcall" => {
                // xpcall(f, msgh, ...) — like pcall but with error handler
                let func = args.first().cloned().unwrap_or(LuaValue::Nil);
                let err_handler = args.get(1).cloned().unwrap_or(LuaValue::Nil);
                let mut call_args_vec = args.get(2..).unwrap_or(&[]).to_vec();
                match self.call_function(&func, &mut call_args_vec, env, &mut Vec::new()) {
                    Ok(vals) => {
                        let mut new_vals = Vec::with_capacity(vals.len() + 1);
                        new_vals.push(LuaValue::Bool(true));
                        new_vals.extend(vals);
                        Ok(new_vals)
                    }
                    Err(msg) => {
                        // Call error handler with the error message
                        let err_val = LuaValue::Str(msg.into_bytes());
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
                            Err(_) => Ok(vec![LuaValue::Bool(false), err_val]),
                        }
                    }
                }
            }
            "pairs" => {
                let table = args.first().cloned().unwrap_or(LuaValue::Nil);
                if !matches!(table, LuaValue::Table(_)) {
                    return Err(lua_bad_table_arg("pairs", 1, &table));
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
                    return Err(lua_bad_table_arg("ipairs", 1, &table));
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
                    return Err(lua_bad_table_arg("next", 1, &table));
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
                let table = args.first().cloned().unwrap_or(LuaValue::Nil);
                let t = lua_table_arg("unpack", 1, &table)?;
                let start = lua_optional_integer_arg("unpack", 2, args.get(1), 1)? as usize;
                let end = lua_optional_integer_arg(
                    "unpack",
                    3,
                    args.get(2),
                    t.inner.borrow().array.len() as i64,
                )? as usize;
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
                let idx = args.first().cloned().unwrap_or(LuaValue::Nil);
                let rest = args.get(1..).unwrap_or(&[]);
                match &idx {
                    LuaValue::Str(s) if s == b"#" => Ok(vec![LuaValue::Number(rest.len() as f64)]),
                    _ => {
                        let raw_index = idx.to_number().ok_or("bad argument to 'select'")?;
                        if !raw_index.is_finite() || raw_index.fract() != 0.0 {
                            return Err("bad argument to 'select'".to_string());
                        }

                        let arg_count = rest.len() as i64;
                        let index = raw_index as i64;
                        if index == 0 || index < -arg_count {
                            return Err(
                                "bad argument #1 to 'select' (index out of range)".to_string()
                            );
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
                let table = args.first().cloned().unwrap_or(LuaValue::Nil);
                let key = args.get(1).cloned().unwrap_or(LuaValue::Nil);
                let LuaValue::Table(t) = &table else {
                    return Err(lua_bad_table_arg("rawget", 1, &table));
                };
                Ok(vec![t.get(&key)])
            }
            "rawset" => {
                // rawset(table, key, value) — set and return table
                if !matches!(args.first(), Some(LuaValue::Table(_))) {
                    let v = args.first().cloned().unwrap_or(LuaValue::Nil);
                    return Err(lua_bad_table_arg("rawset", 1, &v));
                }
                if args.len() >= 3 {
                    let key = args[1].clone();
                    let val = args[2].clone();
                    if let LuaValue::Table(ref mut t) = args[0] {
                        t.set(key, val);
                    }
                }
                // Return the mutated table — clone from args[0] which has the mutation.
                // (args[0] must remain intact for the write-back at the call site.)
                Ok(vec![args[0].clone()])
            }
            "setmetatable" => {
                let table = args.first().cloned().unwrap_or(LuaValue::Nil);
                let LuaValue::Table(t) = &table else {
                    return Err("bad argument #1 to 'setmetatable' (table expected)".to_string());
                };
                let mt_arg = args.get(1).cloned().unwrap_or(LuaValue::Nil);
                match mt_arg {
                    LuaValue::Table(mt) => {
                        t.inner.borrow_mut().metatable = Some(mt);
                    }
                    LuaValue::Nil => {
                        t.inner.borrow_mut().metatable = None;
                    }
                    _ => {
                        return Err(
                            "bad argument #2 to 'setmetatable' (nil or table expected)".to_string(),
                        );
                    }
                }
                Ok(vec![table])
            }
            "getmetatable" => {
                let val = args.first().cloned().unwrap_or(LuaValue::Nil);
                match &val {
                    LuaValue::Table(t) => {
                        let inner = t.inner.borrow();
                        match &inner.metatable {
                            Some(mt) => Ok(vec![LuaValue::Table(mt.clone())]),
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
                let n = args.first().and_then(|v| v.to_number()).unwrap_or(0.0);
                Ok(vec![LuaValue::Number(n.floor())])
            }
            "math.ceil" => {
                let n = args.first().and_then(|v| v.to_number()).unwrap_or(0.0);
                Ok(vec![LuaValue::Number(n.ceil())])
            }
            "math.abs" => {
                let n = args.first().and_then(|v| v.to_number()).unwrap_or(0.0);
                Ok(vec![LuaValue::Number(n.abs())])
            }
            "math.max" => {
                let mut max = f64::NEG_INFINITY;
                for a in args {
                    let n = a.to_number().ok_or("bad argument to 'math.max'")?;
                    if n > max {
                        max = n;
                    }
                }
                Ok(vec![LuaValue::Number(max)])
            }
            "math.min" => {
                let mut min = f64::INFINITY;
                for a in args {
                    let n = a.to_number().ok_or("bad argument to 'math.min'")?;
                    if n < min {
                        min = n;
                    }
                }
                Ok(vec![LuaValue::Number(min)])
            }
            "math.sqrt" => {
                let n = args.first().and_then(|v| v.to_number()).unwrap_or(0.0);
                Ok(vec![LuaValue::Number(n.sqrt())])
            }
            "math.random" => {
                let r = self.next_rand();
                match args.len() {
                    0 => {
                        let f = (r as f64) / (u64::MAX as f64 + 1.0);
                        Ok(vec![LuaValue::Number(f)])
                    }
                    1 => {
                        let m = args[0]
                            .to_number()
                            .ok_or("bad argument #1 to 'random' (number expected)")?
                            as i64;
                        if m < 1 {
                            return Err(
                                "bad argument #1 to 'random' (interval is empty)".to_string()
                            );
                        }
                        let val = (r % (m as u64)) + 1;
                        Ok(vec![LuaValue::Number(val as f64)])
                    }
                    _ => {
                        let m = args[0]
                            .to_number()
                            .ok_or("bad argument #1 to 'random' (number expected)")?
                            as i64;
                        let n = args[1]
                            .to_number()
                            .ok_or("bad argument #2 to 'random' (number expected)")?
                            as i64;
                        if m > n {
                            return Err(
                                "bad argument #1 to 'random' (interval is empty)".to_string()
                            );
                        }
                        let range = (n as i128 - m as i128 + 1);
                        let val = m as i128 + (r as i128 % range);
                        Ok(vec![LuaValue::Number(val as f64)])
                    }
                }
            }
            "math.fmod" => {
                let a = args.first().and_then(|v| v.to_number()).unwrap_or(0.0);
                let b = args.get(1).and_then(|v| v.to_number()).unwrap_or(1.0);
                Ok(vec![LuaValue::Number(a % b)])
            }
            "math.log" => {
                let n = args.first().and_then(|v| v.to_number()).unwrap_or(1.0);
                Ok(vec![LuaValue::Number(n.ln())])
            }
            "math.exp" => {
                let n = args.first().and_then(|v| v.to_number()).unwrap_or(0.0);
                Ok(vec![LuaValue::Number(n.exp())])
            }
            "math.pow" => {
                let a = args.first().and_then(|v| v.to_number()).unwrap_or(0.0);
                let b = args.get(1).and_then(|v| v.to_number()).unwrap_or(0.0);
                Ok(vec![LuaValue::Number(a.powf(b))])
            }
            "math.sin" => {
                let n = args.first().and_then(|v| v.to_number()).unwrap_or(0.0);
                Ok(vec![LuaValue::Number(n.sin())])
            }
            "math.cos" => {
                let n = args.first().and_then(|v| v.to_number()).unwrap_or(0.0);
                Ok(vec![LuaValue::Number(n.cos())])
            }
            "math.tan" => {
                let n = args.first().and_then(|v| v.to_number()).unwrap_or(0.0);
                Ok(vec![LuaValue::Number(n.tan())])
            }
            "math.asin" => {
                let n = args.first().and_then(|v| v.to_number()).unwrap_or(0.0);
                Ok(vec![LuaValue::Number(n.asin())])
            }
            "math.acos" => {
                let n = args.first().and_then(|v| v.to_number()).unwrap_or(0.0);
                Ok(vec![LuaValue::Number(n.acos())])
            }
            "math.atan" => {
                let n = args.first().and_then(|v| v.to_number()).unwrap_or(0.0);
                Ok(vec![LuaValue::Number(n.atan())])
            }
            "math.atan2" => {
                let y = args.first().and_then(|v| v.to_number()).unwrap_or(0.0);
                let x = args.get(1).and_then(|v| v.to_number()).unwrap_or(1.0);
                Ok(vec![LuaValue::Number(y.atan2(x))])
            }
            "math.log10" => {
                let n = args.first().and_then(|v| v.to_number()).unwrap_or(0.0);
                Ok(vec![LuaValue::Number(n.log10())])
            }
            "math.modf" => {
                let n = args.first().and_then(|v| v.to_number()).unwrap_or(0.0);
                let trunc = n.trunc();
                let frac = n - trunc;
                Ok(vec![LuaValue::Number(trunc), LuaValue::Number(frac)])
            }
            "math.frexp" => {
                let n = args.first().and_then(|v| v.to_number()).unwrap_or(0.0);
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
                let m = args.first().and_then(|v| v.to_number()).unwrap_or(0.0);
                let e = args.get(1).and_then(|v| v.to_number()).unwrap_or(0.0) as i32;
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
                // Returns CPU time in seconds (approximation using wall clock)
                // Redis Lua provides this for basic timing
                Ok(vec![LuaValue::Number(0.0)])
            }
            // ── Coroutine stubs (not supported, matches Redis) ──────────
            "coroutine.create"
            | "coroutine.resume"
            | "coroutine.yield"
            | "coroutine.status"
            | "coroutine.wrap"
            | "coroutine.running" => {
                Err("attempt to call a nil value".to_string())
            }
            // ── String library ──────────────────────────────────────────
            "string.len" => {
                let s = args
                    .first()
                    .map(|a| a.to_display_string())
                    .unwrap_or_default();
                Ok(vec![LuaValue::Number(s.len() as f64)])
            }
            "string.sub" => {
                let s = args
                    .first()
                    .map(|a| a.to_display_string())
                    .unwrap_or_default();
                let len = s.len() as i64;
                let mut i = args.get(1).and_then(|v| v.to_number()).unwrap_or(1.0) as i64;
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
                let s = args
                    .first()
                    .map(|a| a.to_display_string())
                    .unwrap_or_default();
                let n_val = args.get(1).and_then(|v| v.to_number()).unwrap_or(0.0);
                if n_val < 0.0 {
                    return Ok(vec![LuaValue::Str(Vec::new())]);
                }
                if n_val > 512.0 * 1024.0 * 1024.0 {
                    return Err("string length overflow".to_string());
                }
                let n = n_val as usize;

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
                let s = args
                    .first()
                    .map(|a| a.to_display_string())
                    .unwrap_or_default();
                Ok(vec![LuaValue::Str(s.to_ascii_lowercase())])
            }
            "string.upper" => {
                let s = args
                    .first()
                    .map(|a| a.to_display_string())
                    .unwrap_or_default();
                Ok(vec![LuaValue::Str(s.to_ascii_uppercase())])
            }
            "string.reverse" => {
                let mut s = args
                    .first()
                    .map(|a| a.to_display_string())
                    .unwrap_or_default();
                s.reverse();
                Ok(vec![LuaValue::Str(s)])
            }
            "string.byte" => {
                let s = args
                    .first()
                    .map(|a| a.to_display_string())
                    .unwrap_or_default();
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
                let mut result = Vec::new();
                for (i, a) in args.iter().enumerate() {
                    let n = a.to_number().ok_or("bad argument to 'string.char'")? as i64;
                    if !(0..=255).contains(&n) {
                        return Err(format!("bad argument #{} to 'char' (invalid value)", i + 1));
                    }
                    result.push(n as u8);
                }
                Ok(vec![LuaValue::Str(result)])
            }
            "string.format" => {
                let fmt = args
                    .first()
                    .map(|a| a.to_display_string())
                    .unwrap_or_default();
                let fmt_str = String::from_utf8_lossy(&fmt).to_string();
                let result = lua_string_format(&fmt_str, &args[1..])?;
                Ok(vec![LuaValue::Str(result.into_bytes())])
            }
            "string.find" => {
                let s = args
                    .first()
                    .map(|a| a.to_display_string())
                    .unwrap_or_default();
                let pattern = args
                    .get(1)
                    .map(|a| a.to_display_string())
                    .unwrap_or_default();
                let init_raw = args.get(2).and_then(|v| v.to_number()).unwrap_or(1.0) as i64;
                let init = if init_raw < 0 {
                    (s.len() as i64 + init_raw).max(0) as usize
                } else {
                    (init_raw as usize).saturating_sub(1)
                };
                let init = init.min(s.len());
                let plain = args.get(3).map(|v| v.is_truthy()).unwrap_or(false);
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
                let s = args
                    .first()
                    .map(|a| a.to_display_string())
                    .unwrap_or_default();
                let pattern = args
                    .get(1)
                    .map(|a| a.to_display_string())
                    .unwrap_or_default();
                let init_raw = args.get(2).and_then(|v| v.to_number()).unwrap_or(1.0) as i64;
                let init = if init_raw < 0 {
                    (s.len() as i64 + init_raw).max(0) as usize
                } else {
                    (init_raw as usize).saturating_sub(1)
                };
                if let Some(m) = lua_pattern_find(&s, &pattern, init) {
                    Ok(lua_match_captures(&s, &m))
                } else {
                    Ok(vec![LuaValue::Nil])
                }
            }
            "string.gmatch" => {
                // Returns an iterator function. Each call returns next match.
                // We collect all matches and return a closure-like iterator via a table.
                let s = args
                    .first()
                    .map(|a| a.to_display_string())
                    .unwrap_or_default();
                let pattern = args
                    .get(1)
                    .map(|a| a.to_display_string())
                    .unwrap_or_default();
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
                let s = args
                    .first()
                    .map(|a| a.to_display_string())
                    .unwrap_or_default();
                let pattern = args
                    .get(1)
                    .map(|a| a.to_display_string())
                    .unwrap_or_default();
                let repl = args
                    .get(2)
                    .map(|a| a.to_display_string())
                    .unwrap_or_default();
                let max_n = args.get(3).and_then(|v| v.to_number()).map(|n| n as usize);
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
                        // Append text before match
                        result.extend_from_slice(&s[pos..m.start]);
                        // Append replacement
                        result.extend_from_slice(&lua_gsub_replace(&s, &m, &repl));
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
                // Append remaining text
                if pos <= s.len() {
                    result.extend_from_slice(&s[pos..]);
                }
                Ok(vec![LuaValue::Str(result), LuaValue::Number(count as f64)])
            }
            // ── Table library ───────────────────────────────────────────
            "table.insert" => {
                let table = args.first().cloned().unwrap_or(LuaValue::Nil);
                let LuaValue::Table(_) = &table else {
                    return Err(lua_bad_table_arg("insert", 1, &table));
                };
                if args.len() == 2 {
                    // table.insert(t, value) — append
                    let val = args[1].clone();
                    if let LuaValue::Table(ref mut t) = args[0] {
                        t.inner.borrow_mut().array.push(val);
                    }
                } else if args.len() >= 3 {
                    // table.insert(t, pos, value)
                    let pos = lua_required_integer_arg("insert", 2, &args[1])? as usize;
                    let val = args[2].clone();
                    if let LuaValue::Table(ref mut t) = args[0] {
                        if pos < 1 || pos > t.inner.borrow().array.len() + 1 {
                            return Err(
                                "bad argument #2 to 'insert' (position out of bounds)".to_string()
                            );
                        }
                        let idx = pos.saturating_sub(1);
                        t.inner.borrow_mut().array.insert(idx, val);
                    }
                }
                Ok(vec![LuaValue::Nil])
            }
            "table.remove" => {
                let table = args.first().cloned().unwrap_or(LuaValue::Nil);
                if !matches!(table, LuaValue::Table(_)) {
                    return Err(lua_bad_table_arg("remove", 1, &table));
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
                let table = args.first().cloned().unwrap_or(LuaValue::Nil);
                let t = lua_table_arg("concat", 1, &table)?;
                let sep = args
                    .get(1)
                    .map(|a| a.to_display_string())
                    .unwrap_or_default();
                let start = lua_optional_integer_arg("concat", 3, args.get(2), 1)? as usize;
                let end = lua_optional_integer_arg(
                    "concat",
                    4,
                    args.get(3),
                    t.inner.borrow().array.len() as i64,
                )? as usize;
                let mut parts: Vec<Vec<u8>> = Vec::new();
                for i in start..=end {
                    if i >= 1 && i <= t.inner.borrow().array.len() {
                        parts.push(t.inner.borrow().array[i - 1].to_display_string());
                    }
                }
                let mut result = Vec::new();
                for (i, part) in parts.iter().enumerate() {
                    if i > 0 {
                        result.extend_from_slice(&sep);
                    }
                    result.extend_from_slice(part);
                }
                Ok(vec![LuaValue::Str(result)])
            }
            "table.sort" => {
                if !args.is_empty() {
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
                // Returns the largest positive numeric key in the table
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
            // ── cjson library ───────────────────────────────────────────
            "cjson.encode" => {
                let val = args.first().cloned().unwrap_or(LuaValue::Nil);
                let json = lua_value_to_json(&val);
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
            _ => Err(format!("attempt to call unknown built-in '{name}'")),
        }
    }

    fn redis_call(&mut self, args: &[LuaValue], is_pcall: bool) -> Result<Vec<LuaValue>, String> {
        if args.is_empty() {
            return Err("wrong number of arguments for 'redis.call'".to_string());
        }

        // Build argv for dispatch
        let mut argv: Vec<Vec<u8>> = Vec::new();
        for arg in args {
            argv.push(arg.to_redis_arg()?);
        }

        match dispatch_argv(&argv, self.store, self.now_ms) {
            Ok(frame) => Ok(vec![resp_to_lua(&frame)]),
            Err(e) => {
                let err_msg = match e.to_resp() {
                    RespFrame::Error(msg) => msg,
                    _ => format!("{e:?}"),
                };
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
fn lua_gsub_replace(s: &[u8], m: &LuaPatMatch, repl: &[u8]) -> Vec<u8> {
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
    result
}

// ── Type conversions ────────────────────────────────────────────────────

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
        RespFrame::Array(Some(items)) | RespFrame::Sequence(items) => {
            let t = LuaTable::new();
            for (i, item) in items.iter().enumerate() {
                t.set(LuaValue::Number((i + 1) as f64), resp_to_lua(item));
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
            // Redis converts Lua numbers to integers (truncated)
            RespFrame::Integer(*n as i64)
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
        LuaValue::Function(_) | LuaValue::RustFunction(_) => RespFrame::BulkString(None),
    }
}

// ── string.format implementation ────────────────────────────────────────

fn lua_string_format(fmt: &str, args: &[LuaValue]) -> Result<String, String> {
    let mut result = String::new();
    let mut arg_idx = 0;
    let mut chars = fmt.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '%' {
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
                            return Err(format!(
                                "bad argument #{} to 'format' (no value)",
                                arg_idx + 1
                            ));
                        }
                    };
                    arg_idx += 1;
                    let formatted = match conv {
                        'd' | 'i' | 'u' => {
                            let n = arg.to_number().unwrap_or(0.0) as i64;
                            let s = if show_sign && n >= 0 {
                                format!("+{n}")
                            } else if space_sign && n >= 0 {
                                format!(" {n}")
                            } else {
                                format!("{n}")
                            };
                            lua_fmt_pad(&s, width, left_align, if zero_pad { '0' } else { ' ' })
                        }
                        'f' => {
                            let n = arg.to_number().unwrap_or(0.0);
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
                        'e' | 'E' => {
                            let n = arg.to_number().unwrap_or(0.0);
                            let prec = precision.unwrap_or(6);
                            let s = lua_fmt_scientific(n, prec, conv == 'E');
                            let s = if show_sign && n >= 0.0 {
                                format!("+{s}")
                            } else {
                                s
                            };
                            lua_fmt_pad(&s, width, left_align, ' ')
                        }
                        'g' | 'G' => {
                            let n = arg.to_number().unwrap_or(0.0);
                            let prec = precision.unwrap_or(6).max(1);
                            let s = lua_fmt_g(n, prec, conv == 'G');
                            let s = if show_sign && n >= 0.0 {
                                format!("+{s}")
                            } else {
                                s
                            };
                            lua_fmt_pad(&s, width, left_align, ' ')
                        }
                        's' => {
                            let s = arg.to_display_string();
                            let mut s = String::from_utf8_lossy(&s).to_string();
                            if let Some(prec) = precision {
                                s.truncate(prec);
                            }
                            lua_fmt_pad(&s, width, left_align, ' ')
                        }
                        'q' => {
                            let s = arg.to_display_string();
                            let mut q = String::new();
                            q.push('"');
                            for &b in &s {
                                match b {
                                    b'\\' => q.push_str("\\\\"),
                                    b'"' => q.push_str("\\\""),
                                    b'\n' => q.push_str("\\n"),
                                    b'\r' => q.push_str("\\r"),
                                    b'\0' => q.push_str("\\0"),
                                    _ => q.push(b as char),
                                }
                            }
                            q.push('"');
                            q
                        }
                        'x' | 'X' => {
                            let n = arg.to_number().unwrap_or(0.0) as u64;
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
                            let n = arg.to_number().unwrap_or(0.0) as u64;
                            let s = if alt_form {
                                format!("0{n:o}")
                            } else {
                                format!("{n:o}")
                            };
                            lua_fmt_pad(&s, width, left_align, if zero_pad { '0' } else { ' ' })
                        }
                        'c' => {
                            let n = arg.to_number().unwrap_or(0.0) as u8;
                            String::from(n as char)
                        }
                        _ => {
                            format!("%{conv}")
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
        lua_fmt_scientific(n, prec.saturating_sub(1), upper)
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

fn lua_value_to_json(val: &LuaValue) -> String {
    match val {
        LuaValue::Nil => "null".to_string(),
        LuaValue::Bool(b) => if *b { "true" } else { "false" }.to_string(),
        LuaValue::Number(n) => {
            if *n == (*n as i64) as f64 && n.is_finite() {
                format!("{}", *n as i64)
            } else {
                format!("{n}")
            }
        }
        LuaValue::Str(s) => json_escape_bytes(s),
        LuaValue::Table(t) => {
            if !t.inner.borrow().array.is_empty() && t.hash_is_empty() {
                // JSON array
                let items: Vec<String> = t
                    .inner
                    .borrow()
                    .array
                    .iter()
                    .map(lua_value_to_json)
                    .collect();
                format!("[{}]", items.join(","))
            } else if t.inner.borrow().array.is_empty() && !t.hash_is_empty() {
                // JSON object
                let hash_pairs = t.hash_pairs();
                let pairs: Vec<String> = hash_pairs
                    .iter()
                    .map(|(k, v)| {
                        let key_json = json_escape_bytes(&k.to_display_string());
                        format!("{key_json}:{}", lua_value_to_json(v))
                    })
                    .collect();
                format!("{{{}}}", pairs.join(","))
            } else if t.inner.borrow().array.is_empty() && t.hash_is_empty() {
                "{}".to_string()
            } else {
                // Mixed — encode as object with numeric string keys for array part
                let mut pairs: Vec<String> = Vec::new();
                for (i, v) in t.inner.borrow().array.iter().enumerate() {
                    pairs.push(format!("\"{}\":{}", i + 1, lua_value_to_json(v)));
                }
                for (k, v) in &t.hash_pairs() {
                    let key_json = json_escape_bytes(&k.to_display_string());
                    pairs.push(format!("{key_json}:{}", lua_value_to_json(v)));
                }
                format!("{{{}}}", pairs.join(","))
            }
        }
        LuaValue::Function(_) | LuaValue::RustFunction(_) => "null".to_string(),
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
    let mut state = LuaState::new(store, now_ms);

    let keys_vals: Vec<LuaValue> = keys.iter().map(|k| LuaValue::Str(k.clone())).collect();
    let argv_vals: Vec<LuaValue> = argv.iter().map(|a| LuaValue::Str(a.clone())).collect();
    state.set_keys_argv(keys_vals, argv_vals);

    let result = state.execute(script)?;
    Ok(lua_to_resp(&result))
}

#[cfg(test)]
mod tests {
    use fr_protocol::RespFrame;
    use fr_store::Store;

    use super::{
        LuaTable, LuaValue, eval_script, json_to_lua_value, lua_raw_equal, lua_value_to_json,
    };

    #[test]
    fn cjson_encode_escapes_object_keys() {
        let table = LuaTable::new();
        table.set(
            LuaValue::Str(b"say\"hi\\there\n".to_vec()),
            LuaValue::Number(1.0),
        );

        assert_eq!(
            lua_value_to_json(&LuaValue::Table(table)),
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
    fn tonumber_rejects_non_integer_base() {
        let mut store = Store::new();
        let result = eval_script(b"return tonumber('10', 2.5)", &[], &[], &mut store, 0);

        assert!(matches!(result, Err(ref err) if err.contains("base out of range")));
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
            other => panic!("Expected iteration limit error, got {:?}", other),
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
    fn table_insert_rejects_out_of_bounds_position() {
        let mut store = Store::new();
        let result = eval_script(
            b"local t = {1, 2}\ntable.insert(t, 4, 3)",
            &[],
            &[],
            &mut store,
            0,
        );

        assert!(matches!(result, Err(ref err) if err.contains("position out of bounds")));
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
            lua_value_to_json(&LuaValue::Table(table)),
            "{\"a\":2,\"z\":1}"
        );
    }

    #[test]
    fn cjson_encode_escapes_all_json_control_characters() {
        assert_eq!(
            lua_value_to_json(&LuaValue::Str(b"\x08\x0c\x01".to_vec())),
            "\"\\b\\f\\u0001\""
        );
    }

    #[test]
    fn cjson_decode_understands_control_character_escapes() {
        match json_to_lua_value("\"\\b\\f\\u0001\"") {
            Ok(LuaValue::Str(bytes)) => assert_eq!(bytes, vec![0x08, 0x0C, 0x01]),
            other => panic!("unexpected decode result: {other:?}"),
        }
    }

    #[test]
    fn coroutine_stubs_return_error() {
        let mut store = Store::new();
        for func in &["create", "resume", "yield", "status", "wrap", "running"] {
            let script = format!("return coroutine.{func}()").into_bytes();
            let result = eval_script(&script, &[], &[], &mut store, 0);
            assert!(
                result.is_err(),
                "coroutine.{func}() should return an error, got: {result:?}"
            );
        }
    }

    #[test]
    fn coroutine_table_is_accessible() {
        let mut store = Store::new();
        let result = eval_script(b"return type(coroutine)", &[], &[], &mut store, 0);
        assert_eq!(
            result,
            Ok(RespFrame::BulkString(Some(b"table".to_vec())))
        );
    }

    #[test]
    fn setmetatable_and_getmetatable_work() {
        let mut store = Store::new();
        let script = b"local t = {}; local mt = {x=42}; setmetatable(t, mt); return getmetatable(t).x";
        let result = eval_script(script, &[], &[], &mut store, 0);
        assert_eq!(result, Ok(RespFrame::Integer(42)));
    }

    #[test]
    fn metatable_index_fallback() {
        let mut store = Store::new();
        let script = b"local base = {greeting = 'hello'}; local t = {}; setmetatable(t, {__index = base}); return t.greeting";
        let result = eval_script(script, &[], &[], &mut store, 0);
        assert_eq!(
            result,
            Ok(RespFrame::BulkString(Some(b"hello".to_vec())))
        );
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
        let script = b"local base = {x=1}; local t = {x=2}; setmetatable(t, {__index=base}); return t.x";
        let result = eval_script(script, &[], &[], &mut store, 0);
        assert_eq!(result, Ok(RespFrame::Integer(2)));
    }

    #[test]
    fn setmetatable_nil_removes_metatable() {
        let mut store = Store::new();
        let script = b"local t = {}; setmetatable(t, {__index={x=1}}); setmetatable(t, nil); return t.x";
        let result = eval_script(script, &[], &[], &mut store, 0);
        assert_eq!(result, Ok(RespFrame::BulkString(None)));
    }

    #[test]
    fn string_rep_huge_does_not_loop_forever() {
        let mut store = Store::new();
        // This should error out quickly rather than looping forever.
        let script = b"return string.rep('', math.huge)";
        let result = eval_script(script, &[], &[], &mut store, 0);
        assert!(result.is_err());
    }

    #[test]
    fn math_random_does_not_panic_on_max_range() {
        let mut store = Store::new();
        // This should not panic with a modulo by zero.
        let script = b"return math.random(-9223372036854775808, 9223372036854775807)";
        let result = eval_script(script, &[], &[], &mut store, 0);
        assert!(result.is_ok());
    }
}
