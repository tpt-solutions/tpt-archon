//! Tokenizer and shared token-stream helpers used by every parse function.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use super::ast::{CmpOp, ParseError};

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Tok {
    Ident(String),
    Int(i64),
    Float(f32),
    Text(String),
    Star,
    Comma,
    Dot,
    Op(CmpOp),
    Param,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Eof,
}

/// Maximum recursive-descent nesting depth for expression/subquery parsing.
pub(super) const MAX_PARSE_DEPTH: u32 = 100;

// ---------------------------------------------------------------------------
// Lexer — internal helper used only by TokenStream::new to tokenize the input
// ---------------------------------------------------------------------------

struct Lexer<'a> {
    s: &'a str,
    pos: usize,
}

impl<'a> Lexer<'a> {
    fn new(s: &'a str) -> Self {
        Self { s, pos: 0 }
    }

    fn bytes(&self) -> &'a [u8] {
        self.s.as_bytes()
    }

    fn next_tok(&mut self) -> Result<Tok, ParseError> {
        let b = self.bytes();
        while self.pos < b.len() && b[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
        if self.pos >= b.len() {
            return Ok(Tok::Eof);
        }
        let c = b[self.pos];
        match c {
            b'*' => {
                self.pos += 1;
                Ok(Tok::Star)
            }
            b',' => {
                self.pos += 1;
                Ok(Tok::Comma)
            }
            b'(' => {
                self.pos += 1;
                Ok(Tok::LParen)
            }
            b')' => {
                self.pos += 1;
                Ok(Tok::RParen)
            }
            b'.' => {
                self.pos += 1;
                Ok(Tok::Dot)
            }
            b'[' => {
                self.pos += 1;
                Ok(Tok::LBracket)
            }
            b']' => {
                self.pos += 1;
                Ok(Tok::RBracket)
            }
            b'=' => {
                self.pos += 1;
                Ok(Tok::Op(CmpOp::Eq))
            }
            b'<' => {
                self.pos += 1;
                if self.pos < b.len() && b[self.pos] == b'=' {
                    self.pos += 1;
                    Ok(Tok::Op(CmpOp::Le))
                } else if self.pos < b.len() && b[self.pos] == b'>' {
                    self.pos += 1;
                    Ok(Tok::Op(CmpOp::Ne))
                } else {
                    Ok(Tok::Op(CmpOp::Lt))
                }
            }
            b'>' => {
                self.pos += 1;
                if self.pos < b.len() && b[self.pos] == b'=' {
                    self.pos += 1;
                    Ok(Tok::Op(CmpOp::Ge))
                } else {
                    Ok(Tok::Op(CmpOp::Gt))
                }
            }
            b'!' => {
                self.pos += 1;
                if self.pos < b.len() && b[self.pos] == b'=' {
                    self.pos += 1;
                    Ok(Tok::Op(CmpOp::Ne))
                } else {
                    Err(ParseError("unexpected '!'".to_string()))
                }
            }
            b'?' => {
                self.pos += 1;
                Ok(Tok::Param)
            }
            b'\'' => {
                self.pos += 1;
                let start = self.pos;
                while self.pos < b.len() && b[self.pos] != b'\'' {
                    self.pos += 1;
                }
                if self.pos >= b.len() {
                    return Err(ParseError("unterminated text literal".to_string()));
                }
                let text = self.s[start..self.pos].to_string();
                self.pos += 1;
                Ok(Tok::Text(text))
            }
            c if c.is_ascii_digit() || c == b'-' => {
                let start = self.pos;
                if c == b'-' {
                    self.pos += 1;
                }
                while self.pos < b.len() && b[self.pos].is_ascii_digit() {
                    self.pos += 1;
                }
                let mut is_float = false;
                if self.pos < b.len() && b[self.pos] == b'.' {
                    let dot_pos = self.pos;
                    self.pos += 1;
                    if self.pos < b.len() && b[self.pos].is_ascii_digit() {
                        while self.pos < b.len() && b[self.pos].is_ascii_digit() {
                            self.pos += 1;
                        }
                        if self.pos < b.len() && b[self.pos].is_ascii_alphabetic() {
                            self.pos = dot_pos;
                        } else {
                            is_float = true;
                        }
                    } else {
                        self.pos = dot_pos;
                    }
                }
                let text = &self.s[start..self.pos];
                if is_float {
                    text.parse::<f32>()
                        .map(Tok::Float)
                        .map_err(|_| ParseError("invalid float".to_string()))
                } else {
                    text.parse::<i64>()
                        .map(Tok::Int)
                        .map_err(|_| ParseError("invalid integer".to_string()))
                }
            }
            c if c.is_ascii_alphabetic() || c == b'_' => {
                let start = self.pos;
                while self.pos < b.len()
                    && (b[self.pos].is_ascii_alphanumeric() || b[self.pos] == b'_')
                {
                    self.pos += 1;
                }
                Ok(Tok::Ident(self.s[start..self.pos].to_string()))
            }
            other => Err(ParseError(alloc::format!(
                "unexpected character '{}'",
                other as char
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// TokenStream — the replacement for the old Lexer + push_back pattern
// ---------------------------------------------------------------------------

/// A pre-tokenized input stream with `peek()`/`next()` access and save/restore
/// backtracking.  Replaces the previous lossy `push_back` approach.
pub(super) struct TokenStream {
    tokens: Vec<Tok>,
    pos: usize,
    depth: u32,
}

impl TokenStream {
    /// Tokenizes `input` into a flat token vector.  Errors during tokenization
    /// (e.g. unterminated string literal) are returned immediately.
    pub(super) fn new(input: &str) -> Result<Self, ParseError> {
        let mut lx = Lexer::new(input);
        let mut tokens = Vec::new();
        loop {
            let tok = lx.next_tok()?;
            let owned = match tok {
                Tok::Ident(s) => Tok::Ident(s.to_string()),
                Tok::Int(v) => Tok::Int(v),
                Tok::Float(v) => Tok::Float(v),
                Tok::Text(s) => Tok::Text(s.to_string()),
                Tok::Star => Tok::Star,
                Tok::Comma => Tok::Comma,
                Tok::Dot => Tok::Dot,
                Tok::Op(op) => Tok::Op(op),
                Tok::Param => Tok::Param,
                Tok::LParen => Tok::LParen,
                Tok::RParen => Tok::RParen,
                Tok::LBracket => Tok::LBracket,
                Tok::RBracket => Tok::RBracket,
                Tok::Eof => Tok::Eof,
            };
            let is_eof = matches!(owned, Tok::Eof);
            tokens.push(owned);
            if is_eof {
                break;
            }
        }
        Ok(Self {
            tokens,
            pos: 0,
            depth: 0,
        })
    }

    /// Look at the current token without consuming it.
    pub(super) fn peek(&self) -> Tok {
        self.tokens[self.pos].clone()
    }

    /// Consume and return the current token.
    pub(super) fn next(&mut self) -> Tok {
        let tok = self.tokens[self.pos].clone();
        self.pos += 1;
        tok
    }

    /// Save the current position for later restoration.
    pub(super) fn save(&self) -> usize {
        self.pos
    }

    /// Restore to a previously saved position (backtrack).
    pub(super) fn restore(&mut self, pos: usize) {
        self.pos = pos;
    }

    /// Enter one level of recursive-descent expression/subquery parsing,
    /// failing once [`MAX_PARSE_DEPTH`] is exceeded.
    pub(super) fn enter_depth(&mut self) -> Result<(), ParseError> {
        self.depth += 1;
        if self.depth > MAX_PARSE_DEPTH {
            return Err(ParseError(
                "expression or subquery nesting too deep".to_string(),
            ));
        }
        Ok(())
    }

    /// Leave one level entered via [`Self::enter_depth`].
    pub(super) fn exit_depth(&mut self) {
        self.depth -= 1;
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub(super) fn eq_ignore_case(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .all(|(x, y)| x.eq_ignore_ascii_case(&y))
}

pub(super) fn expect_ident(ts: &mut TokenStream, what: &str) -> Result<String, ParseError> {
    match ts.next() {
        Tok::Ident(name) => {
            if let Tok::Dot = ts.peek() {
                ts.next();
                let field = expect_ident(ts, "field name after '.'")?;
                Ok(alloc::format!("{name}.{field}"))
            } else {
                Ok(name)
            }
        }
        _ => Err(ParseError(alloc::format!("expected {what}"))),
    }
}

pub(super) fn expect_kw(ts: &mut TokenStream, expected: &str) -> Result<(), ParseError> {
    match ts.next() {
        Tok::Ident(kw) if eq_ignore_case(&kw, expected) => Ok(()),
        _ => Err(ParseError(alloc::format!("expected {expected}"))),
    }
}

pub(super) fn is_kw(s: &str) -> bool {
    matches!(
        s.to_ascii_lowercase().as_str(),
        "select"
            | "insert"
            | "update"
            | "delete"
            | "from"
            | "where"
            | "set"
            | "order"
            | "limit"
            | "group"
            | "having"
            | "join"
            | "on"
            | "create"
            | "view"
            | "drop"
            | "begin"
            | "commit"
            | "rollback"
            | "not"
            | "exists"
            | "extract"
    )
}

pub(super) fn expect_int(ts: &mut TokenStream, what: &str) -> Result<i64, ParseError> {
    match ts.next() {
        Tok::Int(v) => Ok(v),
        _ => Err(ParseError(alloc::format!("expected {what}"))),
    }
}

pub(super) fn expect_tok(
    ts: &mut TokenStream,
    expected: Tok,
    what: &str,
) -> Result<(), ParseError> {
    let got = ts.next();
    if got == expected {
        Ok(())
    } else {
        Err(ParseError(alloc::format!("expected {what}")))
    }
}
