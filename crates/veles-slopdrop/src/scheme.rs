//! The base library's image language, revived (PLAN-slopdrop-art.md,
//! wrap-up round). slopdrop's old bot sent `magick_scheme` programs to
//! magick.buttes.org, which is gone:
//!
//! ```text
//! (let ((cbimg (image https://…)) (x (width cbimg)) (y (height cbimg))
//!       (half (crop cbimg 0 0 (/ x 2) y)))
//!   (composite cbimg (flop half) (/ x 2) 0))
//! ```
//!
//! Here a program is read and run over the thread's pictures, and its
//! value — a picture — is attached like any other image macro. The
//! language is the subset the base library writes: `let` (each binding
//! sees the ones before it, as the library assumes), `if`, `begin`,
//! `and`/`or`/`not`, arithmetic and comparisons, and the image procedures
//! in [`call`]. There is no `lambda` and no `define`, so a program cannot
//! loop; it is still bounded in steps, nesting, image operations and the
//! pixels they make.
//!
//! slopdrop (`tcl/magick.tcl`) records each `magick_scheme` call as
//! `{kind magick scheme <program> id <n>}` and returns the handle
//! `magick:<n>`, so procs that feed one result into the next
//! (`(image [magick_id $img])`) name an earlier program; only the programs
//! no other one names are drawn ([`roots`]).
//!
//! What may be fetched is unchanged: only a picture the thread shows
//! (`magick::fetch`). The library's blank helper canvases on dead hosts
//! (`white100x100.png`, …) are made here ([`canvas_for`]).

use std::collections::HashMap;
use std::rc::Rc;

use image::{imageops, DynamicImage, Rgba, RgbaImage};

use crate::magick::MAX_SIDE;

/// A program longer than this is not read.
pub const MAX_PROGRAM_BYTES: usize = 64 * 1024;
/// At most this many `magick_scheme` programs in one reply.
pub const MAX_PROGRAMS: usize = 16;
/// Lists nest at most this deep, and a program names others at most this
/// deep.
const MAX_DEPTH: usize = 64;
/// Evaluation steps for one drawing, its named programs included.
const MAX_STEPS: usize = 20_000;
/// Image operations for one drawing.
const MAX_IMAGE_OPS: usize = 40;
/// Pixels all of one drawing's image operations may make together.
const MAX_PIXELS: u64 = 24_000_000;
/// At most this many different pictures of the thread in one drawing.
pub const MAX_SOURCES: usize = 4;

/// One recorded program: its handle number and its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program {
    pub id: String,
    pub text: String,
}

/// One drawing: the program drawn and every program it names, the root
/// first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drawing {
    pub programs: Vec<Program>,
}

impl Drawing {
    pub fn root(&self) -> &Program {
        &self.programs[0]
    }

    /// Every URL the programs mention: what may need fetching (only
    /// those the thread shows are).
    pub fn urls(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for p in &self.programs {
            let mut rest = p.text.as_str();
            while let Some(i) = rest.find("http") {
                let tail = &rest[i..];
                if !(tail.starts_with("http://") || tail.starts_with("https://")) {
                    rest = &tail[4..];
                    continue;
                }
                let end = tail
                    .find(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | '"' | '\\'))
                    .unwrap_or(tail.len());
                let url = &tail[..end];
                if !out.iter().any(|u| u == url) {
                    out.push(url.to_string());
                }
                rest = &tail[end..];
            }
        }
        out
    }
}

/// The handle slopdrop returns for program `id`.
pub fn handle(id: &str) -> String {
    format!("magick:{id}")
}

/// The handle number `s` names, when it is one (`magick:12`).
fn handle_id(s: &str) -> Option<&str> {
    let id = s.strip_prefix("magick:")?;
    (!id.is_empty() && id.len() <= 4 && id.bytes().all(|b| b.is_ascii_digit())).then_some(id)
}

/// The ids a program's text names by handle.
fn named(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("magick:") {
        let tail = &rest[i + "magick:".len()..];
        let n = tail.bytes().take_while(u8::is_ascii_digit).count();
        if n > 0 && n <= 4 {
            out.push(tail[..n].to_string());
        }
        rest = &tail[n..];
    }
    out
}

/// The drawings in one reply's programs: each program no other one names,
/// with the programs it reaches (in the order recorded). A program that
/// only names itself in a loop is no drawing.
pub fn roots(programs: &[Program]) -> Vec<Drawing> {
    let by_id: HashMap<&str, &Program> = programs.iter().map(|p| (p.id.as_str(), p)).collect();
    let mut referenced = std::collections::HashSet::new();
    for p in programs {
        for id in named(&p.text) {
            if id != p.id {
                referenced.insert(id);
            }
        }
    }
    programs
        .iter()
        .filter(|p| !referenced.contains(&p.id))
        .map(|root| {
            let mut seen: Vec<&str> = vec![root.id.as_str()];
            let mut i = 0;
            while i < seen.len() {
                if let Some(p) = by_id.get(seen[i]) {
                    for id in named(&p.text) {
                        if let Some(q) = by_id.get(id.as_str()) {
                            if !seen.contains(&q.id.as_str()) {
                                seen.push(q.id.as_str());
                            }
                        }
                    }
                }
                i += 1;
            }
            Drawing {
                programs: seen
                    .iter()
                    .filter_map(|id| by_id.get(id).map(|p| (*p).clone()))
                    .collect(),
            }
        })
        .collect()
}

// ── reading ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum Expr {
    Num(f64),
    Str(String),
    Sym(String),
    List(Vec<Expr>),
}

/// Every expression in `src` (a program is usually one).
fn read(src: &str) -> Result<Vec<Expr>, String> {
    if src.len() > MAX_PROGRAM_BYTES {
        return Err(format!(
            "a program is at most {} KB",
            MAX_PROGRAM_BYTES / 1024
        ));
    }
    let chars: Vec<char> = src.chars().collect();
    let mut pos = 0;
    let mut out = Vec::new();
    loop {
        skip_space(&chars, &mut pos);
        if pos >= chars.len() {
            return Ok(out);
        }
        out.push(read_one(&chars, &mut pos, 0)?);
    }
}

fn skip_space(c: &[char], pos: &mut usize) {
    while *pos < c.len() {
        if c[*pos].is_whitespace() {
            *pos += 1;
        } else if c[*pos] == ';' {
            while *pos < c.len() && c[*pos] != '\n' {
                *pos += 1;
            }
        } else {
            break;
        }
    }
}

fn read_one(c: &[char], pos: &mut usize, depth: usize) -> Result<Expr, String> {
    if depth > MAX_DEPTH {
        return Err(format!("lists nest more than {MAX_DEPTH} deep"));
    }
    skip_space(c, pos);
    match c.get(*pos) {
        None => Err("the program ends inside a list (a missing \")\")".to_string()),
        Some('(') => {
            *pos += 1;
            let mut items = Vec::new();
            loop {
                skip_space(c, pos);
                match c.get(*pos) {
                    None => {
                        return Err("the program ends inside a list (a missing \")\")".to_string())
                    }
                    Some(')') => {
                        *pos += 1;
                        return Ok(Expr::List(items));
                    }
                    _ => items.push(read_one(c, pos, depth + 1)?),
                }
            }
        }
        Some(')') => Err("a \")\" with no \"(\"".to_string()),
        Some('"') => {
            *pos += 1;
            let mut s = String::new();
            loop {
                match c.get(*pos) {
                    None => return Err("a string with no closing quote".to_string()),
                    Some('"') => {
                        *pos += 1;
                        return Ok(Expr::Str(s));
                    }
                    Some('\\') => {
                        *pos += 1;
                        match c.get(*pos) {
                            Some('n') => s.push('\n'),
                            Some('t') => s.push('\t'),
                            Some(&ch) => s.push(ch),
                            None => return Err("a string with no closing quote".to_string()),
                        }
                        *pos += 1;
                    }
                    Some(&ch) => {
                        s.push(ch);
                        *pos += 1;
                    }
                }
            }
        }
        Some(_) => {
            let start = *pos;
            while *pos < c.len() && !c[*pos].is_whitespace() && !matches!(c[*pos], '(' | ')' | '"')
            {
                *pos += 1;
            }
            let tok: String = c[start..*pos].iter().collect();
            Ok(number(&tok).map(Expr::Num).unwrap_or(Expr::Sym(tok)))
        }
    }
}

/// A token that is a number: digits with an optional sign, point and
/// exponent (never `inf` or `nan`).
fn number(tok: &str) -> Option<f64> {
    let body = tok.strip_prefix(['-', '+']).unwrap_or(tok);
    let first = body.chars().next()?;
    if !(first.is_ascii_digit() || (first == '.' && body.len() > 1)) {
        return None;
    }
    tok.parse::<f64>().ok().filter(|v| v.is_finite())
}

// ── running ─────────────────────────────────────────────────────────

#[derive(Clone)]
enum Val {
    Num(f64),
    Bool(bool),
    Str(String),
    Img(Rc<RgbaImage>),
}

impl Val {
    fn kind(&self) -> &'static str {
        match self {
            Val::Num(_) => "a number",
            Val::Bool(_) => "a boolean",
            Val::Str(_) => "a string",
            Val::Img(_) => "a picture",
        }
    }
}

/// What a drawing came to: the picture and the words written on it.
pub struct Drawn {
    pub image: RgbaImage,
    /// Every text `annotate` or `caption` wrote, one per line.
    pub words: String,
}

impl std::fmt::Debug for Drawn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (w, h) = self.image.dimensions();
        write!(f, "Drawn({w}×{h}, words {:?})", self.words)
    }
}

struct Run<'a> {
    /// Every program of the drawing by id, read once.
    programs: HashMap<String, Rc<Vec<Expr>>>,
    /// The fetched pictures by URL (or why they could not be).
    sources: &'a HashMap<String, Result<Vec<u8>, String>>,
    decoded: HashMap<String, Rc<RgbaImage>>,
    /// A named program's value, once run.
    results: HashMap<String, Val>,
    /// The programs being run, to refuse one that names itself.
    running: Vec<String>,
    steps: usize,
    ops: usize,
    pixels: u64,
    rng: u64,
    words: Vec<String>,
}

/// Run a drawing: its root program's value must be a picture. `sources`
/// are the thread's pictures fetched for it. CPU-bound: call it off the
/// async workers.
pub fn run(
    drawing: &Drawing,
    sources: &HashMap<String, Result<Vec<u8>, String>>,
) -> Result<Drawn, String> {
    let mut programs = HashMap::new();
    for p in &drawing.programs {
        let exprs = read(&p.text).map_err(|e| format!("magick:{}: {e}", p.id))?;
        programs.insert(p.id.clone(), Rc::new(exprs));
    }
    // `rand` is the same for the same programs, so a drawing an owner
    // approved or deleted is the drawing they saw.
    let mut seed: u64 = 0xcbf2_9ce4_8422_2325;
    for p in &drawing.programs {
        for b in p.text.bytes() {
            seed = (seed ^ b as u64).wrapping_mul(0x0100_0000_01b3);
        }
    }
    let mut r = Run {
        programs,
        sources,
        decoded: HashMap::new(),
        results: HashMap::new(),
        running: Vec::new(),
        steps: 0,
        ops: 0,
        pixels: 0,
        rng: seed | 1,
        words: Vec::new(),
    };
    match r.program(&drawing.root().id, 0)? {
        Val::Img(img) => Ok(Drawn {
            image: Rc::try_unwrap(img).unwrap_or_else(|rc| (*rc).clone()),
            words: r.words.join("\n"),
        }),
        other => Err(format!(
            "the program's value is {}, not a picture",
            other.kind()
        )),
    }
}

type Env = Vec<(String, Val)>;

impl Run<'_> {
    /// The value of the program `magick:<id>`: its last expression.
    fn program(&mut self, id: &str, depth: usize) -> Result<Val, String> {
        if let Some(v) = self.results.get(id) {
            return Ok(v.clone());
        }
        if self.running.iter().any(|r| r == id) {
            return Err(format!("magick:{id} names itself"));
        }
        if depth > MAX_DEPTH {
            return Err(format!(
                "programs name each other more than {MAX_DEPTH} deep"
            ));
        }
        let exprs = self
            .programs
            .get(id)
            .cloned()
            .ok_or_else(|| format!("no program magick:{id} in this reply"))?;
        self.running.push(id.to_string());
        let mut env = Env::new();
        let mut last = None;
        for e in exprs.iter() {
            last = Some(self.eval(e, &mut env, depth)?);
        }
        self.running.pop();
        let v = last.ok_or_else(|| format!("magick:{id} is empty"))?;
        self.results.insert(id.to_string(), v.clone());
        Ok(v)
    }

    fn eval(&mut self, e: &Expr, env: &mut Env, depth: usize) -> Result<Val, String> {
        self.steps += 1;
        if self.steps > MAX_STEPS {
            return Err(format!("the program runs more than {MAX_STEPS} steps"));
        }
        if depth > MAX_DEPTH {
            return Err(format!("the program nests more than {MAX_DEPTH} deep"));
        }
        let list = match e {
            Expr::Num(n) => return Ok(Val::Num(*n)),
            Expr::Str(s) => return Ok(Val::Str(s.clone())),
            Expr::Sym(s) => {
                return Ok(match s.as_str() {
                    "#t" => Val::Bool(true),
                    "#f" => Val::Bool(false),
                    // Names fold case, as in the Scheme the old service
                    // ran (R5RS): `recurse_eyes` binds `I` and reads `i`.
                    _ => env
                        .iter()
                        .rev()
                        .find(|(n, _)| n.eq_ignore_ascii_case(s))
                        .map(|(_, v)| v.clone())
                        // An unbound word is itself: a bare URL or a
                        // colour name, as the library writes them.
                        .unwrap_or_else(|| Val::Str(s.clone())),
                });
            }
            Expr::List(l) => l,
        };
        let Some((head, args)) = list.split_first() else {
            return Err("() is not a procedure call".to_string());
        };
        let Expr::Sym(name) = head else {
            return Err("a call must start with a procedure's name".to_string());
        };
        let name = name.to_ascii_lowercase();
        match name.as_str() {
            "let" | "let*" => {
                let Some((Expr::List(binds), body)) = args.split_first() else {
                    return Err("let: (let ((name value) …) body …)".to_string());
                };
                let mark = env.len();
                for b in binds {
                    let Expr::List(pair) = b else {
                        return Err("let: each binding is (name value)".to_string());
                    };
                    let [Expr::Sym(n), v] = pair.as_slice() else {
                        return Err("let: each binding is (name value)".to_string());
                    };
                    let v = self.eval(v, env, depth + 1)?;
                    env.push((n.clone(), v));
                }
                let out = self.body(body, env, depth);
                env.truncate(mark);
                out
            }
            "begin" => self.body(args, env, depth),
            "if" => {
                if args.len() < 2 || args.len() > 3 {
                    return Err("if: (if test then else)".to_string());
                }
                if truthy(&self.eval(&args[0], env, depth + 1)?) {
                    self.eval(&args[1], env, depth + 1)
                } else if let Some(e) = args.get(2) {
                    self.eval(e, env, depth + 1)
                } else {
                    Ok(Val::Bool(false))
                }
            }
            "and" => {
                let mut v = Val::Bool(true);
                for a in args {
                    v = self.eval(a, env, depth + 1)?;
                    if !truthy(&v) {
                        break;
                    }
                }
                Ok(v)
            }
            "or" => {
                let mut v = Val::Bool(false);
                for a in args {
                    v = self.eval(a, env, depth + 1)?;
                    if truthy(&v) {
                        break;
                    }
                }
                Ok(v)
            }
            _ => {
                let mut vals = Vec::with_capacity(args.len());
                for a in args {
                    vals.push(self.eval(a, env, depth + 1)?);
                }
                self.call(&name, vals, depth)
            }
        }
    }

    fn body(&mut self, body: &[Expr], env: &mut Env, depth: usize) -> Result<Val, String> {
        let mut last = None;
        for e in body {
            last = Some(self.eval(e, env, depth + 1)?);
        }
        last.ok_or_else(|| "a body with nothing in it".to_string())
    }

    /// A picture from what `(image x)` was given: a picture, a handle of
    /// an earlier program, a program as text, a blank canvas the library
    /// names, or a picture of the thread.
    fn image(&mut self, v: Val, depth: usize) -> Result<Rc<RgbaImage>, String> {
        let s = match v {
            Val::Img(i) => return Ok(i),
            Val::Str(s) => s,
            other => return Err(format!("image: {} is not a picture", other.kind())),
        };
        let s = s.trim();
        if let Some(id) = handle_id(s) {
            let v = self.program(id, depth + 1)?;
            return self.image(v, depth + 1);
        }
        if s.starts_with('(') {
            // A program pasted in as text (`scheme_escape`d into a
            // string): run it here.
            let exprs = read(s)?;
            let mut env = Env::new();
            let mut last = None;
            for e in &exprs {
                last = Some(self.eval(e, &mut env, depth + 1)?);
            }
            let v = last.ok_or_else(|| "image: an empty program".to_string())?;
            return self.image(v, depth + 1);
        }
        if let Some(img) = self.decoded.get(s) {
            return Ok(img.clone());
        }
        let img = if let Some(c) = canvas_for(s) {
            c
        } else {
            match self.sources.get(s) {
                Some(Ok(bytes)) => crate::magick::decode(bytes)?,
                Some(Err(e)) => return Err(e.clone()),
                None => {
                    let shown: String = s.chars().take(80).collect();
                    return Err(format!("image: {shown} is not a picture the thread shows"));
                }
            }
        };
        self.made(&img)?;
        let img = Rc::new(img);
        self.decoded.insert(s.to_string(), img.clone());
        Ok(img)
    }

    /// Count one image operation's output against the drawing's budget.
    fn made(&mut self, img: &RgbaImage) -> Result<(), String> {
        self.ops += 1;
        self.pixels += img.width() as u64 * img.height() as u64;
        if self.ops > MAX_IMAGE_OPS {
            return Err(format!(
                "the program does more than {MAX_IMAGE_OPS} image operations"
            ));
        }
        if self.pixels > MAX_PIXELS {
            return Err("the program draws too much (pixels)".to_string());
        }
        Ok(())
    }

    fn call(&mut self, name: &str, args: Vec<Val>, depth: usize) -> Result<Val, String> {
        let arity = |lo: usize, hi: usize| -> Result<(), String> {
            if args.len() < lo || args.len() > hi {
                return Err(if lo == hi {
                    format!("{name} takes {lo} argument(s), not {}", args.len())
                } else {
                    format!("{name} takes {lo}–{hi} arguments, not {}", args.len())
                });
            }
            Ok(())
        };
        let num = |i: usize| -> Result<f64, String> {
            match args.get(i) {
                Some(Val::Num(n)) => Ok(*n),
                Some(other) => Err(format!("{name}: {} is not a number", other.kind())),
                None => Err(format!("{name}: a missing argument")),
            }
        };
        let side = |i: usize| -> Result<u32, String> {
            let v = num(i)?;
            if !(0.5..=(4 * MAX_SIDE) as f64).contains(&v) {
                return Err(format!("{name}: {v} is no size"));
            }
            Ok((v.round() as u32).clamp(1, MAX_SIDE))
        };
        let text = |i: usize| -> Result<String, String> {
            match args.get(i) {
                Some(Val::Str(s)) => Ok(s.chars().take(500).collect()),
                Some(Val::Num(n)) => Ok(fmt_num(*n)),
                Some(other) => Err(format!("{name}: {} is not text", other.kind())),
                None => Ok(String::new()),
            }
        };
        let fold = |f: fn(f64, f64) -> f64, unit: f64| -> Result<Val, String> {
            let mut acc = unit;
            for i in 0..args.len() {
                acc = f(acc, num(i)?);
            }
            Ok(Val::Num(acc))
        };
        match name {
            // ── numbers ──
            "+" => fold(|a, b| a + b, 0.0),
            "*" => fold(|a, b| a * b, 1.0),
            "-" | "/" => {
                arity(1, 64)?;
                let first = num(0)?;
                let div = name == "/";
                if args.len() == 1 {
                    return if div {
                        if first == 0.0 {
                            Err("/: division by zero".to_string())
                        } else {
                            Ok(Val::Num(1.0 / first))
                        }
                    } else {
                        Ok(Val::Num(-first))
                    };
                }
                let mut acc = first;
                for i in 1..args.len() {
                    let b = num(i)?;
                    if div {
                        if b == 0.0 {
                            return Err("/: division by zero".to_string());
                        }
                        acc /= b;
                    } else {
                        acc -= b;
                    }
                }
                Ok(Val::Num(acc))
            }
            "min" | "max" => {
                arity(1, 64)?;
                let mut acc = num(0)?;
                for i in 1..args.len() {
                    let b = num(i)?;
                    acc = if name == "min" {
                        acc.min(b)
                    } else {
                        acc.max(b)
                    };
                }
                Ok(Val::Num(acc))
            }
            "int" | "truncate" => {
                arity(1, 1)?;
                Ok(Val::Num(num(0)?.trunc()))
            }
            "floor" | "ceiling" | "round" | "abs" => {
                arity(1, 1)?;
                let v = num(0)?;
                Ok(Val::Num(match name {
                    "floor" => v.floor(),
                    "ceiling" => v.ceil(),
                    "round" => v.round(),
                    _ => v.abs(),
                }))
            }
            "sqrt" => {
                arity(1, 1)?;
                let v = num(0)?;
                if v < 0.0 {
                    return Err("sqrt: a negative number".to_string());
                }
                Ok(Val::Num(v.sqrt()))
            }
            "pow" | "expt" => {
                arity(2, 2)?;
                let v = num(0)?.powf(num(1)?);
                if !v.is_finite() {
                    return Err(format!("{name}: the result is not a number"));
                }
                Ok(Val::Num(v))
            }
            "modulo" | "remainder" | "quotient" => {
                arity(2, 2)?;
                let (a, b) = (num(0)?, num(1)?);
                if b == 0.0 {
                    return Err(format!("{name}: division by zero"));
                }
                Ok(Val::Num(match name {
                    "modulo" => a.rem_euclid(b),
                    "remainder" => a % b,
                    _ => (a / b).trunc(),
                }))
            }
            "rand" | "random" => {
                arity(0, 1)?;
                self.rng ^= self.rng << 13;
                self.rng ^= self.rng >> 7;
                self.rng ^= self.rng << 17;
                let unit = (self.rng >> 11) as f64 / (1u64 << 53) as f64;
                Ok(Val::Num(match args.first() {
                    None => unit,
                    Some(_) => (unit * num(0)?).floor(),
                }))
            }
            "=" | "<" | ">" | "<=" | ">=" => {
                arity(2, 64)?;
                let mut ok = true;
                for i in 1..args.len() {
                    let (a, b) = (num(i - 1)?, num(i)?);
                    ok &= match name {
                        "=" => a == b,
                        "<" => a < b,
                        ">" => a > b,
                        "<=" => a <= b,
                        _ => a >= b,
                    };
                }
                Ok(Val::Bool(ok))
            }
            "not" => {
                arity(1, 1)?;
                Ok(Val::Bool(!truthy(&args[0])))
            }
            // ── pictures ──
            "image" => {
                arity(1, 1)?;
                let v = args.into_iter().next().unwrap_or(Val::Bool(false));
                Ok(Val::Img(self.image(v, depth)?))
            }
            "width" | "height" => {
                arity(1, 1)?;
                let img = self.image(args[0].clone(), depth)?;
                Ok(Val::Num(if name == "width" {
                    img.width() as f64
                } else {
                    img.height() as f64
                }))
            }
            "canvas" => {
                arity(2, 3)?;
                let (w, h) = (side(0)?, side(1)?);
                let c = match args.get(2) {
                    Some(v) => colour_val(v)?,
                    None => Rgba([0, 0, 0, 0]),
                };
                self.out(RgbaImage::from_pixel(w, h, c))
            }
            "noop" => {
                arity(1, 1)?;
                Ok(Val::Img(self.image(args[0].clone(), depth)?))
            }
            _ => {
                let Some(first) = args.first().cloned() else {
                    return Err(format!("{name}: unknown procedure, or no picture given"));
                };
                if !is_image_op(name) {
                    return Err(format!("unknown procedure \"{name}\""));
                }
                let img = self.image(first, depth)?;
                let (w, h) = img.dimensions();
                let out = match name {
                    "resize" => {
                        arity(3, 3)?;
                        imageops::resize(
                            &*img,
                            side(1)?,
                            side(2)?,
                            imageops::FilterType::CatmullRom,
                        )
                    }
                    "resize_to_fit" => {
                        arity(3, 3)?;
                        let (bw, bh) = (side(1)?, side(2)?);
                        let f = (bw as f64 / w as f64).min(bh as f64 / h as f64);
                        let nw = ((w as f64 * f).round() as u32).clamp(1, MAX_SIDE);
                        let nh = ((h as f64 * f).round() as u32).clamp(1, MAX_SIDE);
                        imageops::resize(&*img, nw, nh, imageops::FilterType::CatmullRom)
                    }
                    "scale" => {
                        arity(2, 2)?;
                        let pct = num(1)?;
                        if !(1.0..=400.0).contains(&pct) {
                            return Err(format!("scale: {pct} is outside 1–400 (percent)"));
                        }
                        let (nw, nh) = crate::magick::fit_dims(
                            ((w as f64 * pct / 100.0).round() as u32).max(1),
                            ((h as f64 * pct / 100.0).round() as u32).max(1),
                            MAX_SIDE,
                        );
                        imageops::resize(&*img, nw, nh, imageops::FilterType::CatmullRom)
                    }
                    "crop" => {
                        arity(5, 5)?;
                        let (x, y) = (num(1)?.max(0.0).round(), num(2)?.max(0.0).round());
                        if x >= w as f64 || y >= h as f64 {
                            return Err(format!("crop: {x},{y} is outside the {w}×{h} picture"));
                        }
                        let (x, y) = (x as u32, y as u32);
                        let (cw, ch) = (side(3)?.min(w - x), side(4)?.min(h - y));
                        imageops::crop_imm(&*img, x, y, cw, ch).to_image()
                    }
                    "rotate" => {
                        arity(2, 2)?;
                        rotate(&img, num(1)?)
                    }
                    "flip" => {
                        arity(1, 1)?;
                        imageops::flip_vertical(&*img)
                    }
                    "flop" => {
                        arity(1, 1)?;
                        imageops::flip_horizontal(&*img)
                    }
                    "grayscale" | "greyscale" => {
                        arity(1, 1)?;
                        let mut g = DynamicImage::ImageLuma8(imageops::grayscale(&*img)).to_rgba8();
                        for (o, i) in g.pixels_mut().zip(img.pixels()) {
                            o.0[3] = i.0[3];
                        }
                        g
                    }
                    "invert" | "negate" => {
                        arity(1, 1)?;
                        let mut out = (*img).clone();
                        imageops::invert(&mut out);
                        out
                    }
                    "blur" => {
                        arity(2, 2)?;
                        let s = num(1)?;
                        if !(0.1..=20.0).contains(&s) {
                            return Err(format!("blur: {s} is outside 0.1–20"));
                        }
                        imageops::fast_blur(&*img, s as f32)
                    }
                    "sharpen" => {
                        arity(1, 1)?;
                        imageops::unsharpen(&*img, 1.0, 5)
                    }
                    "border" => {
                        arity(2, 3)?;
                        let px = num(1)?.round();
                        if !(1.0..=200.0).contains(&px) {
                            return Err(format!("border: {px} is outside 1–200"));
                        }
                        let px = px as u32;
                        let c = match args.get(2) {
                            Some(v) => colour_val(v)?,
                            None => Rgba([0, 0, 0, 255]),
                        };
                        let mut out = RgbaImage::from_pixel(w + 2 * px, h + 2 * px, c);
                        imageops::overlay(&mut out, &*img, px as i64, px as i64);
                        crate::magick::fit(out, MAX_SIDE)
                    }
                    "composite" | "overlay" => {
                        let (x, y) = if name == "overlay" {
                            arity(2, 2)?;
                            (0, 0)
                        } else {
                            arity(4, 4)?;
                            (offset(num(2)?)?, offset(num(3)?)?)
                        };
                        let top = self.image(args[1].clone(), depth)?;
                        let mut out = (*img).clone();
                        imageops::overlay(&mut out, &*top, x, y);
                        out
                    }
                    "annotate" => {
                        // (annotate picture size x y colour text)
                        arity(6, 6)?;
                        let size = num(1)?;
                        if !(1.0..=400.0).contains(&size) {
                            return Err(format!("annotate: size {size} is outside 1–400"));
                        }
                        let s = ((size / 16.0).round() as u32).max(1);
                        let (x, y) = (offset(num(2)?)?, offset(num(3)?)?);
                        let c = colour_val(&args[4])?;
                        let t = text(5)?;
                        let mut out = (*img).clone();
                        for (i, line) in t.split('\n').enumerate() {
                            let y = y + i as i64 * (16 * s + s) as i64;
                            crate::magick::draw_text(&mut out, line, x, y, s, c, None);
                        }
                        if !t.trim().is_empty() {
                            self.words.push(t.trim().to_string());
                        }
                        out
                    }
                    "caption" => {
                        arity(2, 3)?;
                        let (top, bottom) = (text(1)?, text(2)?);
                        let mut out = (*img).clone();
                        crate::magick::caption(&mut out, &top, &bottom);
                        let w = format!("{top}\n{bottom}").trim().to_string();
                        if !w.is_empty() {
                            self.words.push(w);
                        }
                        out
                    }
                    _ => unreachable!("is_image_op lists every arm"),
                };
                self.out(out)
            }
        }
    }

    fn out(&mut self, img: RgbaImage) -> Result<Val, String> {
        self.made(&img)?;
        Ok(Val::Img(Rc::new(img)))
    }
}

fn is_image_op(name: &str) -> bool {
    matches!(
        name,
        "resize"
            | "resize_to_fit"
            | "scale"
            | "crop"
            | "rotate"
            | "flip"
            | "flop"
            | "grayscale"
            | "greyscale"
            | "invert"
            | "negate"
            | "blur"
            | "sharpen"
            | "border"
            | "composite"
            | "overlay"
            | "annotate"
            | "caption"
    )
}

/// Only `#f` is false, as in Scheme.
fn truthy(v: &Val) -> bool {
    !matches!(v, Val::Bool(false))
}

fn offset(v: f64) -> Result<i64, String> {
    if v.abs() > (8 * MAX_SIDE) as f64 {
        return Err(format!("{v} is no place on a picture"));
    }
    Ok(v.round() as i64)
}

fn fmt_num(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

fn colour_val(v: &Val) -> Result<Rgba<u8>, String> {
    match v {
        Val::Str(s) if s.trim().eq_ignore_ascii_case("transparent") || s.trim() == "none" => {
            Ok(Rgba([0, 0, 0, 0]))
        }
        Val::Str(s) => crate::magick::colour(s).map(|[r, g, b]| Rgba([r, g, b, 255])),
        other => Err(format!("{} is not a colour", other.kind())),
    }
}

/// The base library's blank helper canvases, on hosts long gone, made
/// here instead of fetched.
fn canvas_for(url: &str) -> Option<RgbaImage> {
    const WHITE: Rgba<u8> = Rgba([255, 255, 255, 255]);
    const BLACK: Rgba<u8> = Rgba([0, 0, 0, 255]);
    const CLEAR: Rgba<u8> = Rgba([0, 0, 0, 0]);
    let bare = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))?;
    let (c, side) = match bare {
        "churchturing.org/x/white100x100.png" | "churchturing.org/x/100x100white.png" => {
            (WHITE, 100)
        }
        "churchturing.org/x/trans100x100.png" => (CLEAR, 100),
        "churchburning.org/w/whitepixel.png" | "churchturing.org/w/whitepixel.png" => (WHITE, 1),
        "churchturing.org/x/1blackpixel.png" => (BLACK, 1),
        _ => return None,
    };
    Some(RgbaImage::from_pixel(side, side, c))
}

/// `img` turned `deg` degrees clockwise. A quarter turn is exact; any
/// other angle grows the canvas to hold the turned picture (transparent
/// corners), sampled bilinearly, and never past [`MAX_SIDE`].
fn rotate(img: &RgbaImage, deg: f64) -> RgbaImage {
    let d = deg.rem_euclid(360.0);
    let near = |a: f64| (d - a).abs() < 1e-9;
    if near(0.0) || near(360.0) {
        return img.clone();
    }
    if near(90.0) {
        return imageops::rotate90(img);
    }
    if near(180.0) {
        return imageops::rotate180(img);
    }
    if near(270.0) {
        return imageops::rotate270(img);
    }
    let (sin, cos) = d.to_radians().sin_cos();
    let bounds = |w: f64, h: f64| (w * cos.abs() + h * sin.abs(), w * sin.abs() + h * cos.abs());
    let (bw, bh) = bounds(img.width() as f64, img.height() as f64);
    let f = (MAX_SIDE as f64 / bw.max(bh)).min(1.0);
    let src = if f < 1.0 {
        let (nw, nh) = (
            ((img.width() as f64 * f) as u32).max(1),
            ((img.height() as f64 * f) as u32).max(1),
        );
        imageops::resize(img, nw, nh, imageops::FilterType::Triangle)
    } else {
        img.clone()
    };
    let (w, h) = (src.width() as f64, src.height() as f64);
    let (ow, oh) = bounds(w, h);
    let (ow, oh) = (
        (ow.ceil() as u32).clamp(1, MAX_SIDE),
        (oh.ceil() as u32).clamp(1, MAX_SIDE),
    );
    let mut out = RgbaImage::new(ow, oh);
    let (cx, cy) = (ow as f64 / 2.0, oh as f64 / 2.0);
    let px = |x: i64, y: i64| -> [f64; 4] {
        if x < 0 || y < 0 || x >= src.width() as i64 || y >= src.height() as i64 {
            return [0.0; 4];
        }
        let p = src.get_pixel(x as u32, y as u32).0;
        let a = p[3] as f64 / 255.0;
        [p[0] as f64 * a, p[1] as f64 * a, p[2] as f64 * a, a]
    };
    for (x, y, o) in out.enumerate_pixels_mut() {
        let (dx, dy) = (x as f64 + 0.5 - cx, y as f64 + 0.5 - cy);
        // The point of the source that lands here: the turn undone.
        let sx = cos * dx + sin * dy + w / 2.0 - 0.5;
        let sy = -sin * dx + cos * dy + h / 2.0 - 0.5;
        let (x0, y0) = (sx.floor(), sy.floor());
        let (fx, fy) = (sx - x0, sy - y0);
        let (x0, y0) = (x0 as i64, y0 as i64);
        let mut acc = [0.0f64; 4];
        for (xx, yy, wgt) in [
            (x0, y0, (1.0 - fx) * (1.0 - fy)),
            (x0 + 1, y0, fx * (1.0 - fy)),
            (x0, y0 + 1, (1.0 - fx) * fy),
            (x0 + 1, y0 + 1, fx * fy),
        ] {
            let p = px(xx, yy);
            for k in 0..4 {
                acc[k] += p[k] * wgt;
            }
        }
        if acc[3] > 0.0 {
            *o = Rgba([
                (acc[0] / acc[3]).round().clamp(0.0, 255.0) as u8,
                (acc[1] / acc[3]).round().clamp(0.0, 255.0) as u8,
                (acc[2] / acc[3]).round().clamp(0.0, 255.0) as u8,
                (acc[3] * 255.0).round().clamp(0.0, 255.0) as u8,
            ]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_of(img: &RgbaImage) -> Vec<u8> {
        let mut out = Vec::new();
        DynamicImage::ImageRgba8(img.clone())
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    fn one(text: &str) -> Drawing {
        Drawing {
            programs: vec![Program {
                id: "1".into(),
                text: text.into(),
            }],
        }
    }

    /// A 300×200 red picture with a blue 10×10 top-left corner, as the thread's
    /// `https://cdn.test/p@jpeg`.
    fn sources() -> HashMap<String, Result<Vec<u8>, String>> {
        let mut src = RgbaImage::from_pixel(300, 200, Rgba([200, 30, 30, 255]));
        for (x, y) in (0..10).flat_map(|x| (0..10).map(move |y| (x, y))) {
            src.put_pixel(x, y, Rgba([0, 0, 255, 255]));
        }
        HashMap::from([("https://cdn.test/p@jpeg".to_string(), Ok(png_of(&src)))])
    }

    fn draw(text: &str) -> Result<Drawn, String> {
        run(&one(text), &sources())
    }

    /// The reader takes what the base library writes: nested lists,
    /// strings with escaped quotes and real newlines, numbers with signs
    /// and points, bare URLs as words, and comments.
    #[test]
    fn a_program_is_read_as_the_library_writes_it() {
        let e =
            read("(annotate (image http://a/b.png) 72 -3 3.5 Black \"say \\\"hi\\\"\nthere\") ; c")
                .unwrap();
        assert_eq!(
            e,
            vec![Expr::List(vec![
                Expr::Sym("annotate".into()),
                Expr::List(vec![
                    Expr::Sym("image".into()),
                    Expr::Sym("http://a/b.png".into())
                ]),
                Expr::Num(72.0),
                Expr::Num(-3.0),
                Expr::Num(3.5),
                Expr::Sym("Black".into()),
                Expr::Str("say \"hi\"\nthere".into()),
            ])]
        );
        assert_eq!(
            read("- +").unwrap(),
            vec![Expr::Sym("-".into()), Expr::Sym("+".into())]
        );
        assert_eq!(read("inf nan").unwrap()[0], Expr::Sym("inf".into()));
        for (bad, why) in [
            ("(a (b)", "missing"),
            ("a)", "no \"(\""),
            ("\"open", "closing quote"),
        ] {
            let e = read(bad).unwrap_err();
            assert!(e.contains(why), "{bad}: {e}");
        }
        let deep = "(".repeat(MAX_DEPTH + 2) + &")".repeat(MAX_DEPTH + 2);
        assert!(read(&deep).unwrap_err().contains("nest"));
    }

    /// The library's own idioms draw what they say: `mirror_left`
    /// (let, width, crop, flop, composite at half the width), a turn by
    /// any angle, `magick-concat-gen` on its blank white canvas, and a
    /// numeric `let` whose bindings see the ones before.
    #[test]
    fn the_librarys_programs_draw_what_they_say() {
        let mirror = draw(
            "(let ((cbimg (image https://cdn.test/p@jpeg)) (x (width cbimg)) (y (height cbimg)) \
             (half (crop cbimg 0 0 (/ x 2) y)) (flop (flop half))) (composite cbimg flop (/ x 2) 0))",
        )
        .unwrap()
        .image;
        assert_eq!(mirror.dimensions(), (300, 200));
        // Names fold case, as in the Scheme the library was written for.
        let folded =
            draw("(let ((I (image https://cdn.test/p@jpeg))) (RESIZE i (/ (width i) 3) 10))");
        assert_eq!(folded.unwrap().image.dimensions(), (100, 10));
        assert_eq!(
            mirror.get_pixel(0, 0).0,
            [0, 0, 255, 255],
            "the left half kept"
        );
        assert_eq!(
            mirror.get_pixel(299, 0).0,
            [0, 0, 255, 255],
            "and mirrored right"
        );

        // A quarter turn the exact way and any other angle sampled: the
        // canvas grows to hold the turned picture.
        let q = draw("(rotate (image https://cdn.test/p@jpeg) -90)")
            .unwrap()
            .image;
        assert_eq!(q.dimensions(), (200, 300));
        let t = draw("(rotate (image https://cdn.test/p@jpeg) 30)")
            .unwrap()
            .image;
        let (w, h) = t.dimensions();
        assert!(
            (358..=361).contains(&w) && (322..=325).contains(&h),
            "{w}×{h}"
        );
        assert_eq!(t.get_pixel(0, 0).0[3], 0, "a transparent corner");
        assert_eq!(
            t.get_pixel(w / 2, h / 2).0,
            [200, 30, 30, 255],
            "the middle is the picture"
        );
        // Clockwise: 30° turns the top-left corner (blue) down and right
        // of where a counter-clockwise turn would put it.
        let near90 = draw("(rotate (image https://cdn.test/p@jpeg) 90.0001)")
            .unwrap()
            .image;
        let blue = near90
            .enumerate_pixels()
            .filter(|(_, _, p)| p.0[2] > 150 && p.0[0] < 100)
            .map(|(x, y, _)| (x, y))
            .next()
            .expect("the blue corner survives");
        assert!(
            blue.0 > near90.width() / 2 && blue.1 < near90.height() / 2,
            "{blue:?}: top right, as a clockwise turn puts it"
        );

        let concat = draw(
            "(let ((i1 (image https://cdn.test/p@jpeg)) (i2 (resize i1 100 50)) \
             (w (+ (width i1) (width i2))) (h (if (> (height i1) (height i2)) (height i1) (height i2))) \
             (canvas (resize (image http://churchturing.org/x/100x100white.png) w h))) \
             (composite (composite canvas i1 0 0) i2 (width i1) (/ (- h 50) 2)))",
        )
        .unwrap()
        .image;
        assert_eq!(concat.dimensions(), (400, 200));
        assert_eq!(
            concat.get_pixel(350, 10).0,
            [255, 255, 255, 255],
            "the white canvas"
        );
        assert_eq!(
            concat.get_pixel(350, 100).0[..3],
            [200, 30, 30],
            "the small copy"
        );
    }

    /// `annotate` writes its text where it is told, in its colour, and the
    /// words it wrote are kept for the word checks, as `caption`'s are.
    #[test]
    fn written_words_are_drawn_and_kept() {
        let d = draw(
            "(caption (annotate (image https://cdn.test/p@jpeg) 32 10 10 white \"hello\nthere\") \"TOP\")",
        )
        .unwrap();
        assert_eq!(d.words, "hello\nthere\nTOP");
        let white = d
            .image
            .enumerate_pixels()
            .filter(|(x, y, p)| *x < 100 && (10..80).contains(y) && p.0 == [255, 255, 255, 255])
            .count();
        assert!(white > 50, "white letters at 10,10: {white}");
    }

    /// Programs name each other by handle: the drawing runs the named
    /// one (once), only the program no other names is drawn, and a
    /// program that names itself is refused.
    #[test]
    fn a_handle_runs_the_program_it_names() {
        let progs = vec![
            Program {
                id: "1".into(),
                text: "(flop (image https://cdn.test/p@jpeg))".into(),
            },
            Program {
                id: "2".into(),
                text: "(resize (image \"magick:1\") 30 20)".into(),
            },
            Program {
                id: "3".into(),
                text: "(grayscale (image magick:2))".into(),
            },
            Program {
                id: "4".into(),
                text: "(image https://cdn.test/p@jpeg)".into(),
            },
        ];
        let r = roots(&progs);
        assert_eq!(r.len(), 2, "{r:?}");
        assert_eq!(r[0].root().id, "3");
        assert_eq!(
            r[0].programs
                .iter()
                .map(|p| p.id.as_str())
                .collect::<Vec<_>>(),
            vec!["3", "2", "1"]
        );
        assert_eq!(r[1].root().id, "4");
        let d = run(&r[0], &sources()).unwrap().image;
        assert_eq!(d.dimensions(), (30, 20));
        let p = d.get_pixel(5, 5).0;
        assert!(p[0] == p[1] && p[1] == p[2], "grey: {p:?}");

        let looped = vec![
            Program {
                id: "1".into(),
                text: "(image magick:2)".into(),
            },
            Program {
                id: "2".into(),
                text: "(image magick:1)".into(),
            },
        ];
        assert!(roots(&looped).is_empty(), "a loop has no root");
        let selfish = one("(flop (image magick:1))");
        assert!(run(&selfish, &sources())
            .unwrap_err()
            .contains("names itself"));
        // A program pasted in as a string runs too (`scheme_escape`).
        let pasted = draw("(width (image \"(resize (image https://cdn.test/p@jpeg) 50 40)\"))");
        assert!(
            pasted.unwrap_err().contains("not a picture"),
            "a number is no drawing"
        );
        assert_eq!(
            draw("(image \"(resize (image \\\"https://cdn.test/p@jpeg\\\") 50 40)\")")
                .unwrap()
                .image
                .dimensions(),
            (50, 40)
        );
    }

    /// Only the thread's pictures (and the library's blank canvases) are
    /// pictures; a program is bounded in steps, operations and pixels,
    /// and every misuse is an error naming it, never a panic.
    #[test]
    fn a_program_stays_inside_its_bounds() {
        for (prog, why) in [
            (
                "(image https://elsewhere.test/x.png)",
                "not a picture the thread shows",
            ),
            (
                "(image http://magick.buttes.org/temp/1.jpeg)",
                "not a picture the thread shows",
            ),
            ("(+ 1 2)", "not a picture"),
            (
                "(explode (image https://cdn.test/p@jpeg))",
                "unknown procedure",
            ),
            ("(resize (image https://cdn.test/p@jpeg) 10)", "takes 3"),
            ("(resize (image https://cdn.test/p@jpeg) 0 10)", "no size"),
            (
                "(crop (image https://cdn.test/p@jpeg) 400 0 5 5)",
                "outside",
            ),
            ("(/ 1 0)", "division by zero"),
            (
                "(annotate (image https://cdn.test/p@jpeg) 12 0 0 chartreuse \"x\")",
                "colour",
            ),
            (
                "(composite (image https://cdn.test/p@jpeg) 5 0 0)",
                "not a picture",
            ),
            ("()", "not a procedure"),
            ("(let ((x)) x)", "binding"),
        ] {
            let e = draw(prog).err().unwrap_or_else(|| panic!("{prog} drew"));
            assert!(e.contains(why), "{prog}: {e}");
        }
        // Reading the picture and forty flops are one too many operations.
        let mut p = "(image https://cdn.test/p@jpeg)".to_string();
        for _ in 0..MAX_IMAGE_OPS {
            p = format!("(flop {p})");
        }
        assert!(draw(&p).unwrap_err().contains("image operations"));
        // Big canvases run out of pixels before operations.
        let big = "(let ((a (canvas 2000 2000)) (b (canvas 2000 2000)) (c (canvas 2000 2000)) \
                   (d (canvas 2000 2000)) (e (canvas 2000 2000)) (f (canvas 2000 2000)) \
                   (g (canvas 2000 2000))) g)";
        assert!(draw(big).unwrap_err().contains("pixels"));
        // Arithmetic alone is bounded by steps.
        let sum = format!("(begin (+ {}) (canvas 1 1))", "1 ".repeat(MAX_STEPS));
        assert!(draw(&sum).unwrap_err().contains("steps"));
        // The same program draws the same picture, `rand` included.
        let r = "(rotate (image https://cdn.test/p@jpeg) (rand 360))";
        assert_eq!(draw(r).unwrap().image, draw(r).unwrap().image);
    }

    /// The URLs a drawing may need fetched are its programs' URL words,
    /// each once.
    #[test]
    fn a_drawing_names_the_urls_it_needs() {
        let d = Drawing {
            programs: vec![
                Program {
                    id: "1".into(),
                    text: "(image https://a.test/x@jpeg) (image \"https://b.test/y\")".into(),
                },
                Program {
                    id: "2".into(),
                    text: "(image \\\"https://a.test/x@jpeg\\\") http://c.test/z".into(),
                },
            ],
        };
        assert_eq!(
            d.urls(),
            vec![
                "https://a.test/x@jpeg",
                "https://b.test/y",
                "http://c.test/z"
            ]
        );
    }
}
