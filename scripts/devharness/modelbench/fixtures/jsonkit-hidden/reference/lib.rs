//! jsonkit reference implementation — the control for the layout audit.
//!
//! This file exists because the seed ships `todo!()` stubs: without a
//! known-good implementation there is nothing to rebuild in alternative
//! layouts, and the audit would be grading shapes with no way to know
//! the shapes are gradeable. It lives under `jsonkit-hidden/` for the
//! same reason `hidden.rs` does — a graded work tree must never see it.
//!
//! The layout audit (`../../layout-audit.sh`) rebuilds THIS file in the
//! shapes the task statement permits and asserts every one still scores
//! 45/45 visible + 36/36 hidden. If this file and the suites ever
//! disagree, the audit's control variant goes red first — fix the
//! reference, never loosen the assertion.
//!
//! SECTION MARKERS BELOW ARE LOAD-BEARING: the layout audit's split
//! variant cuts the file at `// ── item: NAME ──` lines and makes each
//! section its own module. Rules that keep every variant compiling:
//!   * every top-level item (and the private helpers it alone uses)
//!     lives inside its own marker section;
//!   * sections never name each other's private helpers;
//!   * cross-section calls go through the crate root (`crate::pointer`)
//!     so they resolve through whichever re-export lib.rs carries.

use std::collections::BTreeMap;

// Shared by several sections, so it lives in the preamble: the layout
// audit's split variant copies the preamble into every module it cuts.
fn err(message: &str, position: usize) -> JsonError {
    JsonError {
        message: message.to_string(),
        position,
    }
}

// ── item: Json ──
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(BTreeMap<String, Json>),
}

// ── item: JsonError ──
#[derive(Debug, Clone, PartialEq)]
pub struct JsonError {
    pub message: String,
    pub position: usize,
}

// ── item: parse ──
pub fn parse(src: &str) -> Result<Json, JsonError> {
    let mut p = Parser {
        b: src.as_bytes(),
        i: 0,
    };
    p.ws();
    let v = p.value()?;
    p.ws();
    if p.i < p.b.len() {
        return Err(err("trailing characters after value", p.i));
    }
    Ok(v)
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn value(&mut self) -> Result<Json, JsonError> {
        match self.peek() {
            Some(b'n') => self.literal("null", Json::Null),
            Some(b't') => self.literal("true", Json::Bool(true)),
            Some(b'f') => self.literal("false", Json::Bool(false)),
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b'[') => self.array(),
            Some(b'{') => self.object(),
            Some(c) if c == b'-' || c.is_ascii_digit() => self.number(),
            Some(_) => Err(err("unexpected character", self.i)),
            None => Err(err("unexpected end of input", self.i)),
        }
    }

    fn literal(&mut self, word: &str, v: Json) -> Result<Json, JsonError> {
        let start = self.i;
        if self.b.len() - self.i >= word.len() && &self.b[self.i..self.i + word.len()] == word.as_bytes() {
            self.i += word.len();
            Ok(v)
        } else {
            Err(err("invalid literal", start))
        }
    }

    fn array(&mut self) -> Result<Json, JsonError> {
        self.i += 1; // '['
        let mut out = Vec::new();
        self.ws();
        if self.peek() == Some(b']') {
            self.i += 1;
            return Ok(Json::Arr(out));
        }
        loop {
            self.ws();
            out.push(self.value()?);
            self.ws();
            match self.peek() {
                Some(b',') => {
                    self.i += 1;
                }
                Some(b']') => {
                    self.i += 1;
                    return Ok(Json::Arr(out));
                }
                Some(_) => return Err(err("expected ',' or ']'", self.i)),
                None => return Err(err("unterminated array", self.i)),
            }
        }
    }

    fn object(&mut self) -> Result<Json, JsonError> {
        self.i += 1; // '{'
        let mut out = BTreeMap::new();
        self.ws();
        if self.peek() == Some(b'}') {
            self.i += 1;
            return Ok(Json::Obj(out));
        }
        loop {
            self.ws();
            if self.peek() != Some(b'"') {
                return Err(err("expected object key", self.i));
            }
            let key = self.string()?;
            self.ws();
            if self.peek() != Some(b':') {
                return Err(err("expected ':'", self.i));
            }
            self.i += 1;
            self.ws();
            // BTreeMap::insert: the LAST duplicate key wins.
            out.insert(key, self.value()?);
            self.ws();
            match self.peek() {
                Some(b',') => {
                    self.i += 1;
                }
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Json::Obj(out));
                }
                Some(_) => return Err(err("expected ',' or '}'", self.i)),
                None => return Err(err("unterminated object", self.i)),
            }
        }
    }

    fn string(&mut self) -> Result<String, JsonError> {
        self.i += 1; // opening quote
        let mut out = String::new();
        loop {
            let Some(c) = self.peek() else {
                return Err(err("unterminated string", self.i));
            };
            match c {
                b'"' => {
                    self.i += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.i += 1;
                    let Some(e) = self.peek() else {
                        return Err(err("unterminated escape", self.i));
                    };
                    self.i += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            // Surrogate PAIRS: a high surrogate must be
                            // followed by \uDC00-\uDFFF; a lone
                            // surrogate is not a valid scalar value.
                            if (0xD800..=0xDBFF).contains(&hi) {
                                if self.peek() == Some(b'\\') && self.b.get(self.i + 1) == Some(&b'u') {
                                    self.i += 2;
                                    let lo = self.hex4()?;
                                    if !(0xDC00..=0xDFFF).contains(&lo) {
                                        return Err(err("invalid low surrogate", self.i - 4));
                                    }
                                    let cp = 0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00);
                                    out.push(
                                        char::from_u32(cp)
                                            .ok_or_else(|| err("invalid codepoint", self.i))?,
                                    );
                                } else {
                                    return Err(err("lone high surrogate", self.i));
                                }
                            } else if (0xDC00..=0xDFFF).contains(&hi) {
                                return Err(err("lone low surrogate", self.i - 4));
                            } else {
                                out.push(
                                    char::from_u32(hi)
                                        .ok_or_else(|| err("invalid codepoint", self.i))?,
                                );
                            }
                        }
                        _ => return Err(err("invalid escape", self.i - 1)),
                    }
                }
                _ if c < 0x20 => return Err(err("control character in string", self.i)),
                _ => {
                    // Consume one UTF-8 sequence verbatim.
                    let len = utf8_len(c);
                    if self.i + len > self.b.len() {
                        return Err(err("truncated UTF-8", self.i));
                    }
                    out.push_str(
                        std::str::from_utf8(&self.b[self.i..self.i + len])
                            .map_err(|_| err("invalid UTF-8", self.i))?,
                    );
                    self.i += len;
                }
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, JsonError> {
        if self.i + 4 > self.b.len() {
            return Err(err("truncated \\u escape", self.i));
        }
        let mut v: u32 = 0;
        for k in 0..4 {
            let d = self.b[self.i + k] as char;
            let d = d
                .to_digit(16)
                .ok_or_else(|| err("invalid hex in \\u escape", self.i + k))?;
            v = v * 16 + d;
        }
        self.i += 4;
        Ok(v)
    }

    fn number(&mut self) -> Result<Json, JsonError> {
        let start = self.i;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        // Integer part: 0 alone, or [1-9]digits. A leading zero
        // followed by another digit ("01") is rejected.
        match self.peek() {
            Some(b'0') => {
                self.i += 1;
                if self.peek().is_some_and(|c| c.is_ascii_digit()) {
                    return Err(err("leading zero", start));
                }
            }
            Some(c) if (b'1'..=b'9').contains(&c) => {
                while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                    self.i += 1;
                }
            }
            _ => return Err(err("invalid number", start)),
        }
        // Fraction: the dot needs at least one digit after it.
        if self.peek() == Some(b'.') {
            self.i += 1;
            if !self.peek().is_some_and(|c| c.is_ascii_digit()) {
                return Err(err("digit required after decimal point", start));
            }
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                self.i += 1;
            }
        }
        // Exponent: e/E, optional sign, at least one digit.
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            self.i += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.i += 1;
            }
            if !self.peek().is_some_and(|c| c.is_ascii_digit()) {
                return Err(err("digit required in exponent", start));
            }
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                self.i += 1;
            }
        }
        let text = std::str::from_utf8(&self.b[start..self.i]).expect("digits are ASCII");
        text.parse::<f64>()
            .map(Json::Num)
            .map_err(|_| err("invalid number", start))
    }
}

fn utf8_len(first: u8) -> usize {
    if first < 0x80 {
        1
    } else if first >> 5 == 0b110 {
        2
    } else if first >> 4 == 0b1110 {
        3
    } else {
        4
    }
}

// ── item: serialize ──
pub fn serialize(v: &Json) -> String {
    let mut out = String::new();
    write_json(v, &mut out);
    out
}

fn write_json(v: &Json, out: &mut String) {
    match v {
        Json::Null => out.push_str("null"),
        Json::Bool(true) => out.push_str("true"),
        Json::Bool(false) => out.push_str("false"),
        Json::Num(x) => write_num(*x, out),
        Json::Str(s) => write_str(s, out),
        Json::Arr(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json(item, out);
            }
            out.push(']');
        }
        Json::Obj(map) => {
            out.push('{');
            // BTreeMap iterates sorted — the spec's key order is free.
            for (i, (k, val)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_str(k, out);
                out.push(':');
                write_json(val, out);
            }
            out.push('}');
        }
    }
}

fn write_num(x: f64, out: &mut String) {
    // Integral values print without a trailing .0; 1e15 is
    // "1000000000000000", not "1e15".
    if x.fract() == 0.0 && x.abs() < 9.0e18 {
        out.push_str(&format!("{}", x as i64));
    } else {
        out.push_str(&format!("{x}"));
    }
}

fn write_str(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            // Non-ASCII goes out literally, not escaped.
            c => out.push(c),
        }
    }
    out.push('"');
}

// ── item: pointer ──
pub fn pointer<'a>(v: &'a Json, ptr: &str) -> Option<&'a Json> {
    if ptr.is_empty() {
        return Some(v);
    }
    if !ptr.starts_with('/') {
        return None;
    }
    let mut cur = v;
    for raw in ptr[1..].split('/') {
        // Unescape ~1 FIRST, then ~0: "~01" is "~1" literal, not "/".
        let token = raw.replace("~1", "/").replace("~0", "~");
        cur = match cur {
            Json::Obj(map) => map.get(&token)?,
            Json::Arr(items) => {
                // Decimal, no leading zeros; "-" is not a lookup index.
                if token.is_empty()
                    || token == "-"
                    || (token.len() > 1 && token.starts_with('0'))
                    || !token.bytes().all(|b| b.is_ascii_digit())
                {
                    return None;
                }
                items.get(token.parse::<usize>().ok()?)?
            }
            _ => return None,
        };
    }
    Some(cur)
}

// ── item: patch ──
pub fn patch(doc: &Json, ops: &Json) -> Result<Json, JsonError> {
    let Json::Arr(ops) = ops else {
        return Err(err("patch must be an array of operations", 0));
    };
    let mut doc = doc.clone();
    for op in ops {
        apply_op(&mut doc, op)?;
    }
    Ok(doc)
}

fn op_str<'a>(op: &'a Json, field: &str) -> Result<&'a str, JsonError> {
    match op {
        Json::Obj(map) => match map.get(field) {
            Some(Json::Str(s)) => Ok(s),
            _ => Err(err(&format!("operation missing '{field}'"), 0)),
        },
        _ => Err(err("operation must be an object", 0)),
    }
}

fn op_value<'a>(op: &'a Json, field: &str) -> Result<&'a Json, JsonError> {
    match op {
        Json::Obj(map) => map
            .get(field)
            .ok_or_else(|| err(&format!("operation missing '{field}'"), 0)),
        _ => Err(err("operation must be an object", 0)),
    }
}

fn apply_op(doc: &mut Json, op: &Json) -> Result<(), JsonError> {
    let kind = op_str(op, "op")?;
    match kind {
        "add" => {
            let path = op_str(op, "path")?;
            let value = op_value(op, "value")?.clone();
            add_at(doc, path, value)
        }
        "remove" => {
            let path = op_str(op, "path")?;
            remove_at(doc, path).map(|_| ())
        }
        "replace" => {
            let path = op_str(op, "path")?;
            let value = op_value(op, "value")?.clone();
            remove_at(doc, path)?;
            add_at(doc, path, value)
        }
        "move" => {
            let from = op_str(op, "from")?.to_string();
            let path = op_str(op, "path")?.to_string();
            // Moving a location into its own child loses the value
            // mid-flight; the spec rejects it outright.
            if path.starts_with(&from) && path[from.len()..].starts_with('/') {
                return Err(err("move target is inside move source", 0));
            }
            let value = remove_at(doc, &from)?;
            add_at(doc, &path, value)
        }
        "copy" => {
            let from = op_str(op, "from")?;
            let path = op_str(op, "path")?;
            // crate:: — the split variant moves pointer() into its own
            // module; the call must resolve through the root re-export.
            let value = crate::pointer(doc, from)
                .ok_or_else(|| err("copy source does not exist", 0))?
                .clone();
            add_at(doc, path, value)
        }
        "test" => {
            let path = op_str(op, "path")?;
            let value = op_value(op, "value")?;
            let actual =
                crate::pointer(doc, path).ok_or_else(|| err("test target does not exist", 0))?;
            if actual != value {
                return Err(err("test failed: values differ", 0));
            }
            Ok(())
        }
        other => Err(err(&format!("unknown op '{other}'"), 0)),
    }
}

/// Split a pointer into (parent pointer, last token).
fn split_path(path: &str) -> Result<(String, String), JsonError> {
    if path.is_empty() {
        return Err(err("empty path has no parent", 0));
    }
    if !path.starts_with('/') {
        return Err(err("path must start with '/'", 0));
    }
    let cut = path.rfind('/').expect("starts with '/'");
    let raw = &path[cut + 1..];
    Ok((path[..cut].to_string(), raw.replace("~1", "/").replace("~0", "~")))
}

fn add_at(doc: &mut Json, path: &str, value: Json) -> Result<(), JsonError> {
    if path.is_empty() {
        // RFC 6902: add with an empty path replaces the document.
        *doc = value;
        return Ok(());
    }
    let (parent_ptr, token) = split_path(path)?;
    let parent = doc
        .get_mut_pointer(&parent_ptr)
        .ok_or_else(|| err("add target parent does not exist", 0))?;
    match parent {
        Json::Obj(map) => {
            map.insert(token, value);
            Ok(())
        }
        Json::Arr(items) => {
            let idx = if token == "-" {
                items.len()
            } else {
                array_index(&token, items.len(), true)?
            };
            items.insert(idx, value);
            Ok(())
        }
        _ => Err(err("cannot add into a scalar", 0)),
    }
}

fn remove_at(doc: &mut Json, path: &str) -> Result<Json, JsonError> {
    if path.is_empty() {
        return Err(err("cannot remove the whole document", 0));
    }
    let (parent_ptr, token) = split_path(path)?;
    let parent = doc
        .get_mut_pointer(&parent_ptr)
        .ok_or_else(|| err("remove target parent does not exist", 0))?;
    match parent {
        Json::Obj(map) => map.remove(&token).ok_or_else(|| err("remove target does not exist", 0)),
        Json::Arr(items) => {
            let idx = array_index(&token, items.len(), false)?;
            if idx >= items.len() {
                return Err(err("remove index out of range", 0));
            }
            Ok(items.remove(idx))
        }
        _ => Err(err("cannot remove from a scalar", 0)),
    }
}

/// Array index grammar: decimal, no leading zeros. For `add`, one
/// past the end is legal (insert at tail); "-" is handled by callers.
fn array_index(token: &str, len: usize, allow_end: bool) -> Result<usize, JsonError> {
    if token.is_empty()
        || (token.len() > 1 && token.starts_with('0'))
        || !token.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(err("invalid array index", 0));
    }
    let idx: usize = token.parse().map_err(|_| err("array index too large", 0))?;
    let limit = if allow_end { len } else { len - 1 };
    if idx > limit {
        return Err(err("array index out of range", 0));
    }
    Ok(idx)
}

impl Json {
    /// Mutable walk for patch surgery — pointer() is the read-only
    /// twin; the escape and index grammar must stay identical.
    fn get_mut_pointer(&mut self, ptr: &str) -> Option<&mut Json> {
        if ptr.is_empty() {
            return Some(self);
        }
        if !ptr.starts_with('/') {
            return None;
        }
        let mut cur = self;
        for raw in ptr[1..].split('/') {
            let token = raw.replace("~1", "/").replace("~0", "~");
            cur = match cur {
                Json::Obj(map) => map.get_mut(&token)?,
                Json::Arr(items) => {
                    if token.is_empty()
                        || token == "-"
                        || (token.len() > 1 && token.starts_with('0'))
                        || !token.bytes().all(|b| b.is_ascii_digit())
                    {
                        return None;
                    }
                    items.get_mut(token.parse::<usize>().ok()?)?
                }
                _ => return None,
            };
        }
        Some(cur)
    }
}

// ── item: merge_patch ──
pub fn merge_patch(doc: &Json, patch: &Json) -> Json {
    // RFC 7386: a non-object patch REPLACES the target outright.
    let Json::Obj(patch_map) = patch else {
        return patch.clone();
    };
    let mut out = match doc {
        Json::Obj(map) => map.clone(),
        // Merging an object into a non-object starts from empty.
        _ => BTreeMap::new(),
    };
    for (k, v) in patch_map {
        if *v == Json::Null {
            // A null value in the patch DELETES the key.
            out.remove(k);
        } else {
            let base = out.get(k).cloned().unwrap_or(Json::Null);
            out.insert(k.clone(), merge_patch(&base, v));
        }
    }
    Json::Obj(out)
}
