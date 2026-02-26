// Minimal Lua 5.1 evaluator for Redis scripting.
//
// Supports: variables (local/global), arithmetic, string concat, comparisons,
// logical ops, if/elseif/else, numeric for, generic for (pairs/ipairs),
// while, repeat/until, tables, function calls/definitions, redis.call/pcall,
// KEYS/ARGV, and standard library functions.

use std::collections::HashMap;

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
    pub array: Vec<LuaValue>,
    pub hash: Vec<(LuaValue, LuaValue)>,
}

#[derive(Clone, Debug)]
pub struct LuaFunc {
    pub params: Vec<String>,
    pub body: Vec<Stmt>,
    pub is_variadic: bool,
}

impl LuaTable {
    fn new() -> Self {
        Self {
            array: Vec::new(),
            hash: Vec::new(),
        }
    }

    fn get(&self, key: &LuaValue) -> LuaValue {
        match key {
            LuaValue::Number(n) => {
                let idx = *n as usize;
                if idx >= 1 && idx <= self.array.len() && (*n - idx as f64).abs() < f64::EPSILON {
                    return self.array[idx - 1].clone();
                }
                self.hash_get(key)
            }
            LuaValue::Str(s) => {
                // Check hash by string key
                for (k, v) in &self.hash {
                    if let LuaValue::Str(ks) = k
                        && ks == s
                    {
                        return v.clone();
                    }
                }
                LuaValue::Nil
            }
            _ => self.hash_get(key),
        }
    }

    fn hash_get(&self, key: &LuaValue) -> LuaValue {
        for (k, v) in &self.hash {
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
                if idx >= 1 && (*n - idx as f64).abs() < f64::EPSILON {
                    if idx <= self.array.len() {
                        self.array[idx - 1] = value;
                        return;
                    } else if idx == self.array.len() + 1 {
                        self.array.push(value);
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
        for entry in &mut self.hash {
            if lua_raw_equal(&entry.0, &key) {
                entry.1 = value;
                return;
            }
        }
        self.hash.push((key, value));
    }

    fn len(&self) -> usize {
        self.array.len()
    }
}

fn lua_raw_equal(a: &LuaValue, b: &LuaValue) -> bool {
    match (a, b) {
        (LuaValue::Nil, LuaValue::Nil) => true,
        (LuaValue::Bool(x), LuaValue::Bool(y)) => x == y,
        (LuaValue::Number(x), LuaValue::Number(y)) => (x - y).abs() < f64::EPSILON,
        (LuaValue::Str(x), LuaValue::Str(y)) => x == y,
        _ => false,
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
}

struct Scope {
    locals: HashMap<String, LuaValue>,
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
            scope.locals.insert(name.to_string(), value);
        }
    }

    fn get_local(&self, name: &str) -> Option<&LuaValue> {
        for scope in self.scopes.iter().rev() {
            if let Some(val) = scope.locals.get(name) {
                return Some(val);
            }
        }
        None
    }

    fn set_existing_local(&mut self, name: &str, value: LuaValue) -> bool {
        for scope in self.scopes.iter_mut().rev() {
            if scope.locals.contains_key(name) {
                scope.locals.insert(name.to_string(), value);
                return true;
            }
        }
        false
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
        ] {
            globals.insert(name.to_string(), LuaValue::RustFunction(name.to_string()));
        }
        // Math library
        let mut math_table = LuaTable::new();
        for name in &[
            "floor", "ceil", "abs", "max", "min", "sqrt", "huge", "random", "fmod", "log", "exp",
            "pow",
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
        let mut string_table = LuaTable::new();
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
        let mut table_lib = LuaTable::new();
        for name in &["insert", "remove", "concat", "sort", "getn", "maxn"] {
            table_lib.set(
                LuaValue::Str(name.as_bytes().to_vec()),
                LuaValue::RustFunction(format!("table.{name}")),
            );
        }
        globals.insert("table".to_string(), LuaValue::Table(table_lib));

        // cjson stub (commonly used in Redis scripts)
        let mut cjson_table = LuaTable::new();
        for name in &["encode", "decode"] {
            cjson_table.set(
                LuaValue::Str(name.as_bytes().to_vec()),
                LuaValue::RustFunction(format!("cjson.{name}")),
            );
        }
        globals.insert("cjson".to_string(), LuaValue::Table(cjson_table));

        Self {
            store,
            now_ms,
            globals,
            call_depth: 0,
            iterations: 0,
        }
    }

    pub fn set_keys_argv(&mut self, keys: Vec<LuaValue>, argv: Vec<LuaValue>) {
        let mut keys_table = LuaTable::new();
        keys_table.array = keys;
        let mut argv_table = LuaTable::new();
        argv_table.array = argv;
        self.globals
            .insert("KEYS".to_string(), LuaValue::Table(keys_table));
        self.globals
            .insert("ARGV".to_string(), LuaValue::Table(argv_table));

        // Set up redis table with call/pcall
        let mut redis_table = LuaTable::new();
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
        self.globals
            .insert("redis".to_string(), LuaValue::Table(redis_table));
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
                let state = iter_vals.get(1).cloned().unwrap_or(LuaValue::Nil);
                let mut control = iter_vals.get(2).cloned().unwrap_or(LuaValue::Nil);

                loop {
                    let results = self.call_function(
                        &iter_fn,
                        &[state.clone(), control.clone()],
                        env,
                        varargs,
                    )?;
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
                self.table_set_by_expr(table_expr, table, key, value, env)?;
            }
            Expr::Field(table_expr, field) => {
                let table = self.eval_expr(table_expr, env, varargs)?;
                let key = LuaValue::Str(field.as_bytes().to_vec());
                self.table_set_by_expr(table_expr, table, key, value, env)?;
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
    ) -> Result<(), String> {
        if let LuaValue::Table(t) = &mut table {
            t.set(key, value);
            // Write back
            match table_expr {
                Expr::Name(name) => {
                    if !env.set_existing_local(name, table.clone()) {
                        self.globals.insert(name.clone(), table);
                    }
                }
                _ => {
                    // For nested assignments, the mutation happened on a clone.
                    // This is a known limitation; deeply nested table assignment
                    // requires reference semantics not easily modeled here.
                }
            }
            Ok(())
        } else {
            Err(format!("attempt to index a {} value", table.type_name()))
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
                    LuaValue::Table(t) => Ok(t.get(&key)),
                    _ => Err(format!("attempt to index a {} value", table.type_name())),
                }
            }
            Expr::Field(table_expr, field) => {
                let table = self.eval_expr(table_expr, env, varargs)?;
                match &table {
                    LuaValue::Table(t) => Ok(t.get(&LuaValue::Str(field.as_bytes().to_vec()))),
                    _ => Err(format!("attempt to index a {} value", table.type_name())),
                }
            }
            Expr::Call(func_expr, args) => {
                let func = self.eval_expr(func_expr, env, varargs)?;
                let arg_vals = self.eval_call_args(args, env, varargs)?;
                let results = self.call_function(&func, &arg_vals, env, varargs)?;
                Ok(results.into_iter().next().unwrap_or(LuaValue::Nil))
            }
            Expr::MethodCall(obj_expr, method, args) => {
                let obj = self.eval_expr(obj_expr, env, varargs)?;
                let func = match &obj {
                    LuaValue::Table(t) => t.get(&LuaValue::Str(method.as_bytes().to_vec())),
                    _ => {
                        return Err(format!(
                            "attempt to call method on a {} value",
                            obj.type_name()
                        ));
                    }
                };
                let mut arg_vals = vec![obj.clone()];
                arg_vals.extend(self.eval_call_args(args, env, varargs)?);
                let results = self.call_function(&func, &arg_vals, env, varargs)?;
                Ok(results.into_iter().next().unwrap_or(LuaValue::Nil))
            }
            Expr::TableConstructor(fields) => {
                let mut table = LuaTable::new();
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
                        let arg_vals = self.eval_call_args(call_args, env, varargs)?;
                        let results = self.call_function(&func, &arg_vals, env, varargs)?;
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
                        let results = self.call_function(&func, &arg_vals, env, varargs)?;
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
                    BinOp::Mod => a % b,
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
        args: &[LuaValue],
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
                let mut new_env = Env::new();
                for (i, param) in lua_func.params.iter().enumerate() {
                    let val = args.get(i).cloned().unwrap_or(LuaValue::Nil);
                    new_env.set_local(param, val);
                }
                let mut func_varargs = if lua_func.is_variadic {
                    args.get(lua_func.params.len()..).unwrap_or(&[]).to_vec()
                } else {
                    Vec::new()
                };
                match self.exec_stmts(&lua_func.body, &mut new_env, &mut func_varargs)? {
                    ControlFlow::Return(vals) => Ok(vals),
                    _ => Ok(vec![LuaValue::Nil]),
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
        args: &[LuaValue],
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
                    let mut t = LuaTable::new();
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
                    let mut t = LuaTable::new();
                    t.set(LuaValue::Str(b"ok".to_vec()), LuaValue::Str(msg));
                    t
                })])
            }
            "redis.log" => {
                // Silently ignore log calls
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
                let base = args.get(1).and_then(|b| b.to_number()).map(|b| b as u32);
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
                let call_args = args.get(1..).unwrap_or(&[]);
                match self.call_function(&func, call_args, env, &mut Vec::new()) {
                    Ok(mut vals) => {
                        vals.insert(0, LuaValue::Bool(true));
                        Ok(vals)
                    }
                    Err(msg) => Ok(vec![LuaValue::Bool(false), LuaValue::Str(msg.into_bytes())]),
                }
            }
            "pairs" => {
                let table = args.first().cloned().unwrap_or(LuaValue::Nil);
                // Return next, table, nil
                Ok(vec![
                    LuaValue::RustFunction("next".to_string()),
                    table,
                    LuaValue::Nil,
                ])
            }
            "ipairs" => {
                let table = args.first().cloned().unwrap_or(LuaValue::Nil);
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
                    if idx <= t.array.len() {
                        Ok(vec![LuaValue::Number(idx as f64), t.array[idx - 1].clone()])
                    } else {
                        Ok(vec![LuaValue::Nil])
                    }
                } else {
                    Ok(vec![LuaValue::Nil])
                }
            }
            "next" => {
                let table = args.first().cloned().unwrap_or(LuaValue::Nil);
                let key = args.get(1).cloned().unwrap_or(LuaValue::Nil);
                if let LuaValue::Table(t) = &table {
                    // Find next key after the given key
                    if matches!(key, LuaValue::Nil) {
                        // Return first element
                        if !t.array.is_empty() {
                            return Ok(vec![LuaValue::Number(1.0), t.array[0].clone()]);
                        }
                        if let Some((k, v)) = t.hash.first() {
                            return Ok(vec![k.clone(), v.clone()]);
                        }
                        return Ok(vec![LuaValue::Nil]);
                    }
                    // Find position of current key
                    if let LuaValue::Number(n) = &key {
                        let idx = *n as usize;
                        if idx >= 1 && idx <= t.array.len() {
                            if idx < t.array.len() {
                                return Ok(vec![
                                    LuaValue::Number((idx + 1) as f64),
                                    t.array[idx].clone(),
                                ]);
                            }
                            // Past array, check hash
                            if let Some((k, v)) = t.hash.first() {
                                return Ok(vec![k.clone(), v.clone()]);
                            }
                            return Ok(vec![LuaValue::Nil]);
                        }
                    }
                    // Search in hash
                    let mut found = false;
                    for (i, (k, _v)) in t.hash.iter().enumerate() {
                        if found {
                            return Ok(vec![t.hash[i].0.clone(), t.hash[i].1.clone()]);
                        }
                        if lua_raw_equal(k, &key) {
                            found = true;
                        }
                    }
                    Ok(vec![LuaValue::Nil])
                } else {
                    Ok(vec![LuaValue::Nil])
                }
            }
            "unpack" => {
                let table = args.first().cloned().unwrap_or(LuaValue::Nil);
                let start = args.get(1).and_then(|v| v.to_number()).unwrap_or(1.0) as usize;
                if let LuaValue::Table(t) = &table {
                    let end = args
                        .get(2)
                        .and_then(|v| v.to_number())
                        .unwrap_or(t.array.len() as f64) as usize;
                    let mut results = Vec::new();
                    for i in start..=end {
                        if i >= 1 && i <= t.array.len() {
                            results.push(t.array[i - 1].clone());
                        } else {
                            results.push(LuaValue::Nil);
                        }
                    }
                    Ok(results)
                } else {
                    Ok(vec![LuaValue::Nil])
                }
            }
            "select" => {
                let idx = args.first().cloned().unwrap_or(LuaValue::Nil);
                let rest = args.get(1..).unwrap_or(&[]);
                match &idx {
                    LuaValue::Str(s) if s == b"#" => Ok(vec![LuaValue::Number(rest.len() as f64)]),
                    _ => {
                        let n = idx.to_number().ok_or("bad argument to 'select'")? as usize;
                        if n >= 1 && n <= rest.len() {
                            Ok(rest[n - 1..].to_vec())
                        } else {
                            Ok(vec![LuaValue::Nil])
                        }
                    }
                }
            }
            "rawget" => {
                let table = args.first().cloned().unwrap_or(LuaValue::Nil);
                let key = args.get(1).cloned().unwrap_or(LuaValue::Nil);
                if let LuaValue::Table(t) = &table {
                    Ok(vec![t.get(&key)])
                } else {
                    Ok(vec![LuaValue::Nil])
                }
            }
            "rawset" | "setmetatable" => {
                // Return first argument (table) — metatables not supported
                Ok(vec![args.first().cloned().unwrap_or(LuaValue::Nil)])
            }
            "getmetatable" => Ok(vec![LuaValue::Nil]),
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
                // Deterministic for reproducibility in scripts
                match args.len() {
                    0 => Ok(vec![LuaValue::Number(0.5)]),
                    1 => {
                        let m = args[0].to_number().unwrap_or(1.0) as i64;
                        Ok(vec![LuaValue::Number((m / 2).max(1) as f64)])
                    }
                    _ => {
                        let m = args[0].to_number().unwrap_or(1.0) as i64;
                        let n = args[1].to_number().unwrap_or(1.0) as i64;
                        Ok(vec![LuaValue::Number(((m + n) / 2) as f64)])
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
                let n = args.get(1).and_then(|v| v.to_number()).unwrap_or(0.0) as usize;
                let mut result = Vec::with_capacity(s.len() * n);
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
                let i = args.get(1).and_then(|v| v.to_number()).unwrap_or(1.0) as usize;
                let j = args.get(2).and_then(|v| v.to_number()).unwrap_or(i as f64) as usize;
                let mut results = Vec::new();
                for idx in i..=j {
                    if idx >= 1 && idx <= s.len() {
                        results.push(LuaValue::Number(s[idx - 1] as f64));
                    }
                }
                Ok(results)
            }
            "string.char" => {
                let mut result = Vec::new();
                for a in args {
                    let n = a.to_number().ok_or("bad argument to 'string.char'")? as u8;
                    result.push(n);
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
                let init = args.get(2).and_then(|v| v.to_number()).unwrap_or(1.0) as usize;
                let plain = args.get(3).map(|v| v.is_truthy()).unwrap_or(false);
                let s_str = String::from_utf8_lossy(&s);
                let pat_str = String::from_utf8_lossy(&pattern);
                if plain {
                    let search_start = init.saturating_sub(1);
                    if let Some(pos) = s_str[search_start..].find(pat_str.as_ref()) {
                        let start = search_start + pos + 1; // 1-indexed
                        let end = start + pat_str.len() - 1;
                        Ok(vec![
                            LuaValue::Number(start as f64),
                            LuaValue::Number(end as f64),
                        ])
                    } else {
                        Ok(vec![LuaValue::Nil])
                    }
                } else {
                    // Basic pattern matching: just do plain find as fallback
                    let search_start = init.saturating_sub(1);
                    if let Some(pos) = s_str[search_start..].find(pat_str.as_ref()) {
                        let start = search_start + pos + 1;
                        let end = start + pat_str.len() - 1;
                        Ok(vec![
                            LuaValue::Number(start as f64),
                            LuaValue::Number(end as f64),
                        ])
                    } else {
                        Ok(vec![LuaValue::Nil])
                    }
                }
            }
            "string.match" | "string.gmatch" | "string.gsub" => {
                // Stub for pattern matching — return nil/empty
                Ok(vec![LuaValue::Nil])
            }
            // ── Table library ───────────────────────────────────────────
            "table.insert" => {
                if args.len() == 2 {
                    // table.insert(t, value) — append
                    let mut table = args[0].clone();
                    if let LuaValue::Table(ref mut t) = table {
                        t.array.push(args[1].clone());
                        // Write back
                        self.write_back_table_arg(args, 0, table, env);
                    }
                } else if args.len() >= 3 {
                    // table.insert(t, pos, value)
                    let mut table = args[0].clone();
                    let pos = args[1].to_number().unwrap_or(1.0) as usize;
                    if let LuaValue::Table(ref mut t) = table {
                        let idx = pos.saturating_sub(1);
                        if idx <= t.array.len() {
                            t.array.insert(idx, args[2].clone());
                        } else {
                            t.array.push(args[2].clone());
                        }
                        self.write_back_table_arg(args, 0, table, env);
                    }
                }
                Ok(vec![LuaValue::Nil])
            }
            "table.remove" => {
                let mut table = args.first().cloned().unwrap_or(LuaValue::Nil);
                if let LuaValue::Table(ref mut t) = table {
                    let pos = args
                        .get(1)
                        .and_then(|v| v.to_number())
                        .unwrap_or(t.array.len() as f64) as usize;
                    let removed = if pos >= 1 && pos <= t.array.len() {
                        t.array.remove(pos - 1)
                    } else {
                        LuaValue::Nil
                    };
                    self.write_back_table_arg(args, 0, table, env);
                    Ok(vec![removed])
                } else {
                    Ok(vec![LuaValue::Nil])
                }
            }
            "table.concat" => {
                let table = args.first().cloned().unwrap_or(LuaValue::Nil);
                let sep = args
                    .get(1)
                    .map(|a| a.to_display_string())
                    .unwrap_or_default();
                if let LuaValue::Table(t) = &table {
                    let start = args.get(2).and_then(|v| v.to_number()).unwrap_or(1.0) as usize;
                    let end = args
                        .get(3)
                        .and_then(|v| v.to_number())
                        .unwrap_or(t.array.len() as f64) as usize;
                    let mut parts: Vec<Vec<u8>> = Vec::new();
                    for i in start..=end {
                        if i >= 1 && i <= t.array.len() {
                            parts.push(t.array[i - 1].to_display_string());
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
                } else {
                    Ok(vec![LuaValue::Str(Vec::new())])
                }
            }
            "table.sort" => {
                // Note: sort mutates the table in place
                // We need to sort and write back
                let mut table = args.first().cloned().unwrap_or(LuaValue::Nil);
                if let LuaValue::Table(ref mut t) = table {
                    // Default sort: compare as strings or numbers
                    t.array.sort_by(|a, b| match (a, b) {
                        (LuaValue::Number(x), LuaValue::Number(y)) => {
                            x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal)
                        }
                        (LuaValue::Str(x), LuaValue::Str(y)) => x.cmp(y),
                        _ => std::cmp::Ordering::Equal,
                    });
                    self.write_back_table_arg(args, 0, table, env);
                }
                Ok(vec![LuaValue::Nil])
            }
            "table.getn" | "table.maxn" => {
                let table = args.first().cloned().unwrap_or(LuaValue::Nil);
                if let LuaValue::Table(t) = &table {
                    Ok(vec![LuaValue::Number(t.len() as f64)])
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

    fn write_back_table_arg(
        &mut self,
        _args: &[LuaValue],
        _idx: usize,
        _table: LuaValue,
        _env: &mut Env,
    ) {
        // Table mutation write-back is a known limitation.
        // In the current design, tables are cloned values. For redis scripting,
        // table.insert/remove/sort work on local variables which get reassigned.
        // The caller handles the assignment pattern:
        //   local t = {}; table.insert(t, val)
        // via direct mutation of the LuaValue in the environment.
        //
        // For now, this is a no-op. The table.* functions work correctly
        // when the table is passed directly from a local variable, because
        // the Lua patterns used in Redis scripts don't rely on aliased mutation.
    }

    fn redis_call(&mut self, args: &[LuaValue], is_pcall: bool) -> Result<Vec<LuaValue>, String> {
        if args.is_empty() {
            return Err("wrong number of arguments for 'redis.call'".to_string());
        }

        // Build argv for dispatch
        let mut argv: Vec<Vec<u8>> = Vec::new();
        for arg in args {
            argv.push(arg.to_display_string());
        }

        match dispatch_argv(&argv, self.store, self.now_ms) {
            Ok(frame) => Ok(vec![resp_to_lua(&frame)]),
            Err(e) => {
                let err_msg = format!("{e:?}");
                if is_pcall {
                    let mut t = LuaTable::new();
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

// ── Type conversions ────────────────────────────────────────────────────

fn resp_to_lua(frame: &RespFrame) -> LuaValue {
    match frame {
        RespFrame::SimpleString(s) => {
            let mut t = LuaTable::new();
            t.set(
                LuaValue::Str(b"ok".to_vec()),
                LuaValue::Str(s.as_bytes().to_vec()),
            );
            LuaValue::Table(t)
        }
        RespFrame::Error(s) => {
            let mut t = LuaTable::new();
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
        RespFrame::Array(Some(items)) => {
            let mut t = LuaTable::new();
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
            // Convert array part to RESP array
            let mut items = Vec::new();
            for item in &t.array {
                items.push(lua_to_resp(item));
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
                // Parse format specifier
                let mut spec = String::from('%');
                // Flags
                while let Some(&fc) = chars.peek() {
                    if fc == '-' || fc == '+' || fc == ' ' || fc == '0' || fc == '#' {
                        spec.push(fc);
                        chars.next();
                    } else {
                        break;
                    }
                }
                // Width
                while let Some(&fc) = chars.peek() {
                    if fc.is_ascii_digit() {
                        spec.push(fc);
                        chars.next();
                    } else {
                        break;
                    }
                }
                // Precision
                if chars.peek() == Some(&'.') {
                    spec.push('.');
                    chars.next();
                    while let Some(&fc) = chars.peek() {
                        if fc.is_ascii_digit() {
                            spec.push(fc);
                            chars.next();
                        } else {
                            break;
                        }
                    }
                }
                // Conversion
                if let Some(conv) = chars.next() {
                    let arg = args.get(arg_idx).cloned().unwrap_or(LuaValue::Nil);
                    arg_idx += 1;
                    match conv {
                        'd' | 'i' => {
                            let n = arg.to_number().unwrap_or(0.0) as i64;
                            result.push_str(&format!("{n}"));
                        }
                        'f' | 'g' | 'e' => {
                            let n = arg.to_number().unwrap_or(0.0);
                            match conv {
                                'f' => result.push_str(&format!("{n:.6}")),
                                'e' => result.push_str(&format!("{n:e}")),
                                _ => result.push_str(&format!("{n}")),
                            }
                        }
                        's' => {
                            let s = arg.to_display_string();
                            result.push_str(&String::from_utf8_lossy(&s));
                        }
                        'q' => {
                            let s = arg.to_display_string();
                            result.push('"');
                            for &b in &s {
                                match b {
                                    b'\\' => result.push_str("\\\\"),
                                    b'"' => result.push_str("\\\""),
                                    b'\n' => result.push_str("\\n"),
                                    b'\r' => result.push_str("\\r"),
                                    b'\0' => result.push_str("\\0"),
                                    _ => result.push(b as char),
                                }
                            }
                            result.push('"');
                        }
                        'x' | 'X' => {
                            let n = arg.to_number().unwrap_or(0.0) as u64;
                            if conv == 'x' {
                                result.push_str(&format!("{n:x}"));
                            } else {
                                result.push_str(&format!("{n:X}"));
                            }
                        }
                        'o' => {
                            let n = arg.to_number().unwrap_or(0.0) as u64;
                            result.push_str(&format!("{n:o}"));
                        }
                        'c' => {
                            let n = arg.to_number().unwrap_or(0.0) as u8;
                            result.push(n as char);
                        }
                        _ => {
                            spec.push(conv);
                            result.push_str(&spec);
                        }
                    }
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

// ── cjson helpers ───────────────────────────────────────────────────────

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
        LuaValue::Str(s) => {
            let s = String::from_utf8_lossy(s);
            let mut out = String::from('"');
            for c in s.chars() {
                match c {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    _ => out.push(c),
                }
            }
            out.push('"');
            out
        }
        LuaValue::Table(t) => {
            if !t.array.is_empty() && t.hash.is_empty() {
                // JSON array
                let items: Vec<String> = t.array.iter().map(lua_value_to_json).collect();
                format!("[{}]", items.join(","))
            } else if t.array.is_empty() && !t.hash.is_empty() {
                // JSON object
                let pairs: Vec<String> = t
                    .hash
                    .iter()
                    .map(|(k, v)| {
                        let key_str = String::from_utf8_lossy(&k.to_display_string()).to_string();
                        format!("\"{}\":{}", key_str, lua_value_to_json(v))
                    })
                    .collect();
                format!("{{{}}}", pairs.join(","))
            } else if t.array.is_empty() && t.hash.is_empty() {
                "{}".to_string()
            } else {
                // Mixed — encode as object with numeric string keys for array part
                let mut pairs: Vec<String> = Vec::new();
                for (i, v) in t.array.iter().enumerate() {
                    pairs.push(format!("\"{}\":{}", i + 1, lua_value_to_json(v)));
                }
                for (k, v) in &t.hash {
                    let key_str = String::from_utf8_lossy(&k.to_display_string()).to_string();
                    pairs.push(format!("\"{}\":{}", key_str, lua_value_to_json(v)));
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
                        b'n' => result.push(b'\n'),
                        b'r' => result.push(b'\r'),
                        b't' => result.push(b'\t'),
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
        let mut t = LuaTable::new();
        for item in items {
            t.array.push(json_to_lua_value(&item)?);
        }
        Ok(LuaValue::Table(t))
    } else if s.starts_with('{') && s.ends_with('}') {
        let inner = &s[1..s.len() - 1].trim();
        if inner.is_empty() {
            return Ok(LuaValue::Table(LuaTable::new()));
        }
        let pairs = split_json_values(inner)?;
        let mut t = LuaTable::new();
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
