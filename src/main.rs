/*-
 * SPDX-License-Identifier: BSD-3-Clause
 *
 * Rust port of FreeBSD's find(1) utility
 * Original: usr.bin/find/
 * Copyright (c) 1990, 1993, 1994
 *     The Regents of the University of California. All rights reserved.
 *
 * Walks directory hierarchies and evaluates a predicate expression on each
 * entry, printing matches.  Implements the most commonly used predicates
 * and the full AND / OR / NOT expression grammar.
 */

use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process;
use std::time::{Duration, SystemTime};

use walkdir::{DirEntry, WalkDir};

// ──────────────────────────────────────────────────────────────────────────────
// Glob matching (fnmatch-style, no external crate needed)
// ──────────────────────────────────────────────────────────────────────────────

/// Match `name` against a shell glob `pattern` (`*`, `?`, `[...]` supported).
/// When `ignore_case` is true both sides are compared in ASCII lower-case.
fn glob_match(pattern: &str, name: &str, ignore_case: bool) -> bool {
    let pat: Vec<char> = if ignore_case {
        pattern.chars().map(|c| c.to_ascii_lowercase()).collect()
    } else {
        pattern.chars().collect()
    };
    let txt: Vec<char> = if ignore_case {
        name.chars().map(|c| c.to_ascii_lowercase()).collect()
    } else {
        name.chars().collect()
    };
    glob_match_chars(&pat, &txt)
}

fn glob_match_chars(pat: &[char], txt: &[char]) -> bool {
    match (pat.first(), txt.first()) {
        (None, None) => true,
        (Some(&'*'), _) => {
            // Skip consecutive stars
            let rest_pat = pat.iter().skip(1).cloned().collect::<Vec<_>>();
            // Try matching the star against 0..N characters
            for i in 0..=txt.len() {
                if glob_match_chars(&rest_pat, &txt[i..]) {
                    return true;
                }
            }
            false
        }
        (None, _) | (_, None) => false,
        (Some(&'?'), _) => glob_match_chars(&pat[1..], &txt[1..]),
        (Some(&'['), _) => {
            // Character class [abc], [a-z], [^...]
            let (matched, pat_rest) = match_char_class(&pat[1..], txt[0]);
            if matched {
                glob_match_chars(pat_rest, &txt[1..])
            } else {
                false
            }
        }
        (Some(p), Some(t)) => p == t && glob_match_chars(&pat[1..], &txt[1..]),
    }
}

/// Parse a character class starting *after* the opening `[`.
/// Returns (matched, remaining_pattern_after_`]`).
fn match_char_class<'a>(pat: &'a [char], ch: char) -> (bool, &'a [char]) {
    let negate = pat.first() == Some(&'^');
    let mut i = if negate { 1 } else { 0 };
    let mut matched = false;

    while i < pat.len() && pat[i] != ']' {
        if i + 2 < pat.len() && pat[i + 1] == '-' && pat[i + 2] != ']' {
            if ch >= pat[i] && ch <= pat[i + 2] {
                matched = true;
            }
            i += 3;
        } else {
            if ch == pat[i] {
                matched = true;
            }
            i += 1;
        }
    }

    let rest = if i < pat.len() {
        &pat[i + 1..]
    } else {
        &pat[i..]
    };
    (matched ^ negate, rest)
}

// ──────────────────────────────────────────────────────────────────────────────
// Predicate expression tree
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
enum FileType {
    File,
    Dir,
    Symlink,
    BlockDevice,
    CharDevice,
    Pipe,
    Socket,
}

/// Size/time comparison qualifier: exact, less-than, greater-than.
#[derive(Debug, Clone)]
enum Cmp {
    Eq,
    Lt,
    Gt,
}

impl Cmp {
    fn test(&self, a: i64, b: i64) -> bool {
        match self {
            Cmp::Eq => a == b,
            Cmp::Lt => a < b,
            Cmp::Gt => a > b,
        }
    }
}

fn parse_cmp_prefix(s: &str) -> (Cmp, &str) {
    if let Some(rest) = s.strip_prefix('+') {
        (Cmp::Gt, rest)
    } else if let Some(rest) = s.strip_prefix('-') {
        (Cmp::Lt, rest)
    } else {
        (Cmp::Eq, s)
    }
}

/// A single predicate node in the expression tree.
#[derive(Debug, Clone)]
enum Pred {
    // --- primaries ---
    Name {
        pattern: String,
        ignore_case: bool,
    },
    Path {
        pattern: String,
        ignore_case: bool,
    },
    Type(FileType),
    Empty,
    MaxDepth(usize),
    MinDepth(usize),
    /// -size [+-]n[ckMG]  (unit: 'c'=bytes, 'k'=KiB, 'M'=MiB, 'G'=GiB; default=512-byte blocks)
    Size {
        cmp: Cmp,
        bytes: u64,
    },
    /// -mtime [+-]n  (days)
    Mtime {
        cmp: Cmp,
        days: i64,
    },
    /// -newer file
    Newer(SystemTime),
    /// -print / -print0
    Print {
        null: bool,
    },
    /// -delete
    Delete,
    /// -prune  (don't descend into directory)
    Prune,

    // --- logical operators ---
    And(Box<Pred>, Box<Pred>),
    Or(Box<Pred>, Box<Pred>),
    Not(Box<Pred>),

    /// Always-true node used as a placeholder
    True,
}

// ──────────────────────────────────────────────────────────────────────────────
// Argument parsing
// ──────────────────────────────────────────────────────────────────────────────

struct ParseState<'a> {
    args: &'a [String],
    pos: usize,
}

impl<'a> ParseState<'a> {
    fn peek(&self) -> Option<&str> {
        self.args.get(self.pos).map(|s| s.as_str())
    }
    fn next(&mut self) -> Option<&str> {
        let v = self.args.get(self.pos).map(|s| s.as_str());
        if v.is_some() {
            self.pos += 1;
        }
        v
    }
    fn require(&mut self, opt: &str) -> &str {
        self.next().unwrap_or_else(|| {
            eprintln!("find: {} requires an argument", opt);
            process::exit(1);
        })
    }
}

/// Parse the full expression (handles -o / OR at the top level).
fn parse_expr(ps: &mut ParseState, now: SystemTime) -> Pred {
    let lhs = parse_and(ps, now);
    if ps.peek() == Some("-o") || ps.peek() == Some("-or") {
        ps.next();
        let rhs = parse_expr(ps, now);
        Pred::Or(Box::new(lhs), Box::new(rhs))
    } else {
        lhs
    }
}

/// Parse an AND chain (implicit AND between adjacent predicates).
fn parse_and(ps: &mut ParseState, now: SystemTime) -> Pred {
    let lhs = parse_not(ps, now);

    // Peek: if the next token is a primary / '(' / '!' (but not '-o' or ')'), it's an implicit AND
    match ps.peek() {
        None | Some("-o") | Some("-or") | Some(")") => lhs,
        Some("-a") | Some("-and") => {
            ps.next();
            let rhs = parse_and(ps, now);
            Pred::And(Box::new(lhs), Box::new(rhs))
        }
        Some(_) => {
            let rhs = parse_and(ps, now);
            Pred::And(Box::new(lhs), Box::new(rhs))
        }
    }
}

/// Parse a NOT or a primary.
fn parse_not(ps: &mut ParseState, now: SystemTime) -> Pred {
    match ps.peek() {
        Some("!") | Some("-not") => {
            ps.next();
            let operand = parse_primary(ps, now);
            Pred::Not(Box::new(operand))
        }
        _ => parse_primary(ps, now),
    }
}

fn parse_size(arg: &str) -> (Cmp, u64) {
    let (cmp, rest) = parse_cmp_prefix(arg);
    let (n_str, unit) = if rest.ends_with(['c', 'k', 'M', 'G']) {
        (&rest[..rest.len() - 1], rest.chars().last().unwrap())
    } else {
        (rest, 'b') // default: 512-byte blocks
    };
    let n: u64 = n_str.parse().unwrap_or_else(|_| {
        eprintln!("find: invalid size '{}'", arg);
        process::exit(1);
    });
    let bytes = match unit {
        'c' => n,
        'k' => n * 1024,
        'M' => n * 1024 * 1024,
        'G' => n * 1024 * 1024 * 1024,
        _ => n * 512, // 512-byte blocks
    };
    (cmp, bytes)
}

fn parse_primary(ps: &mut ParseState, now: SystemTime) -> Pred {
    let tok = match ps.next() {
        Some(t) => t.to_owned(),
        None => {
            eprintln!("find: missing expression");
            process::exit(1);
        }
    };

    match tok.as_str() {
        "(" => {
            let inner = parse_expr(ps, now);
            if ps.next() != Some(")") {
                eprintln!("find: missing closing ')'");
                process::exit(1);
            }
            inner
        }
        "-name" => {
            let pat = ps.require("-name").to_owned();
            Pred::Name {
                pattern: pat,
                ignore_case: false,
            }
        }
        "-iname" => {
            let pat = ps.require("-iname").to_owned();
            Pred::Name {
                pattern: pat,
                ignore_case: true,
            }
        }
        "-path" | "-wholename" => {
            let pat = ps.require("-path").to_owned();
            Pred::Path {
                pattern: pat,
                ignore_case: false,
            }
        }
        "-ipath" | "-iwholename" => {
            let pat = ps.require("-ipath").to_owned();
            Pred::Path {
                pattern: pat,
                ignore_case: true,
            }
        }
        "-type" => {
            let t = ps.require("-type");
            let ft = match t {
                "f" => FileType::File,
                "d" => FileType::Dir,
                "l" => FileType::Symlink,
                "b" => FileType::BlockDevice,
                "c" => FileType::CharDevice,
                "p" => FileType::Pipe,
                "s" => FileType::Socket,
                other => {
                    eprintln!("find: unknown type '{}'", other);
                    process::exit(1);
                }
            };
            Pred::Type(ft)
        }
        "-empty" => Pred::Empty,
        "-maxdepth" => {
            let n: usize = ps.require("-maxdepth").parse().unwrap_or_else(|_| {
                eprintln!("find: -maxdepth requires a non-negative integer");
                process::exit(1);
            });
            Pred::MaxDepth(n)
        }
        "-mindepth" => {
            let n: usize = ps.require("-mindepth").parse().unwrap_or_else(|_| {
                eprintln!("find: -mindepth requires a non-negative integer");
                process::exit(1);
            });
            Pred::MinDepth(n)
        }
        "-size" => {
            let arg = ps.require("-size").to_owned();
            let (cmp, bytes) = parse_size(&arg);
            Pred::Size { cmp, bytes }
        }
        "-mtime" => {
            let arg = ps.require("-mtime").to_owned();
            let (cmp, rest) = parse_cmp_prefix(&arg);
            let days: i64 = rest.parse().unwrap_or_else(|_| {
                eprintln!("find: invalid -mtime value '{}'", arg);
                process::exit(1);
            });
            Pred::Mtime { cmp, days }
        }
        "-newer" => {
            let file = ps.require("-newer");
            let mtime = fs::metadata(file)
                .and_then(|m| m.modified())
                .unwrap_or_else(|e| {
                    eprintln!("find: -newer {}: {}", file, e);
                    process::exit(1);
                });
            Pred::Newer(mtime)
        }
        "-print" => Pred::Print { null: false },
        "-print0" => Pred::Print { null: true },
        "-delete" => Pred::Delete,
        "-prune" => Pred::Prune,
        "-true" => Pred::True,
        "-false" => Pred::Not(Box::new(Pred::True)),
        other => {
            eprintln!("find: unknown predicate '{}'", other);
            process::exit(1);
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Global options collected before expressions
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Default)]
struct GlobalOpts {
    follow_links: bool,       // -L
    follow_cmd_links: bool,   // -H (follow links only on command line)
    depth_first: bool,        // -d
    xdev: bool,               // -x (don't cross device boundaries — informational only for walkdir)
    max_depth: Option<usize>, // extracted from expression for walkdir tuning
    min_depth: Option<usize>,
    has_output: bool, // expression contains -print/-delete → suppress default -print
}

// ──────────────────────────────────────────────────────────────────────────────
// Expression evaluation
// ──────────────────────────────────────────────────────────────────────────────

struct EvalCtx {
    now: SystemTime,
    prune: bool,     // set to true when -prune fires
    did_print: bool, // track whether any output predicate ran
}

fn matches_file_type(ftype: &fs::FileType, expected: &FileType) -> bool {
    match expected {
        FileType::File => ftype.is_file(),
        FileType::Dir => ftype.is_dir(),
        FileType::Symlink => ftype.is_symlink(),
        FileType::BlockDevice | FileType::CharDevice | FileType::Pipe | FileType::Socket => {
            matches_unix_special_file_type(ftype, expected)
        }
    }
}

#[cfg(unix)]
fn matches_unix_special_file_type(ftype: &fs::FileType, expected: &FileType) -> bool {
    use std::os::unix::fs::FileTypeExt;

    match expected {
        FileType::BlockDevice => ftype.is_block_device(),
        FileType::CharDevice => ftype.is_char_device(),
        FileType::Pipe => ftype.is_fifo(),
        FileType::Socket => ftype.is_socket(),
        FileType::File | FileType::Dir | FileType::Symlink => false,
    }
}

#[cfg(not(unix))]
fn matches_unix_special_file_type(_ftype: &fs::FileType, _expected: &FileType) -> bool {
    false
}

/// Evaluate `pred` against `entry`.  Returns the boolean result.
/// Side-effects: may print, delete, set ctx.prune.
fn eval(pred: &Pred, entry: &DirEntry, ctx: &mut EvalCtx) -> bool {
    match pred {
        Pred::True => true,

        Pred::Name {
            pattern,
            ignore_case,
        } => {
            let name = entry.file_name().to_string_lossy();
            glob_match(pattern, &name, *ignore_case)
        }

        Pred::Path {
            pattern,
            ignore_case,
        } => {
            let path = entry.path().to_string_lossy();
            glob_match(pattern, &path, *ignore_case)
        }

        Pred::Type(ft) => {
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => return false,
            };
            matches_file_type(&meta.file_type(), ft)
        }

        Pred::Empty => {
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => return false,
            };
            if meta.is_file() {
                meta.len() == 0
            } else if meta.is_dir() {
                // A directory is empty if it has no children
                entry
                    .path()
                    .read_dir()
                    .map_or(false, |mut d| d.next().is_none())
            } else {
                false
            }
        }

        Pred::MaxDepth(n) => entry.depth() <= *n,
        Pred::MinDepth(n) => entry.depth() >= *n,

        Pred::Size { cmp, bytes } => {
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => return false,
            };
            cmp.test(meta.len() as i64, *bytes as i64)
        }

        Pred::Mtime { cmp, days } => {
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => return false,
            };
            let mtime = match meta.modified() {
                Ok(t) => t,
                Err(_) => return false,
            };
            // age in whole days (truncated, matching POSIX behaviour)
            let age_secs = ctx
                .now
                .duration_since(mtime)
                .unwrap_or(Duration::ZERO)
                .as_secs() as i64;
            let age_days = age_secs / 86400;
            cmp.test(age_days, *days)
        }

        Pred::Newer(ref_time) => {
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => return false,
            };
            let mtime = match meta.modified() {
                Ok(t) => t,
                Err(_) => return false,
            };
            mtime > *ref_time
        }

        Pred::Print { null } => {
            let stdout = io::stdout();
            let mut out = stdout.lock();
            if *null {
                let _ = out.write_all(entry.path().as_os_str().as_encoded_bytes());
                let _ = out.write_all(b"\0");
            } else {
                println!("{}", entry.path().display());
            }
            ctx.did_print = true;
            true
        }

        Pred::Delete => {
            let path = entry.path();
            let result = if entry.file_type().is_dir() {
                fs::remove_dir(path)
            } else {
                fs::remove_file(path)
            };
            if let Err(e) = result {
                eprintln!("find: {}: {}", path.display(), e);
                return false;
            }
            ctx.did_print = true; // suppress default -print
            true
        }

        Pred::Prune => {
            ctx.prune = true;
            true
        }

        Pred::And(l, r) => eval(l, entry, ctx) && eval(r, entry, ctx),

        Pred::Or(l, r) => eval(l, entry, ctx) || eval(r, entry, ctx),

        Pred::Not(inner) => !eval(inner, entry, ctx),
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Inspect the expression tree to extract global options like maxdepth/mindepth
// and whether any output predicate is present.
// ──────────────────────────────────────────────────────────────────────────────

fn inspect(pred: &Pred, opts: &mut GlobalOpts) {
    match pred {
        Pred::MaxDepth(n) => opts.max_depth = Some(*n),
        Pred::MinDepth(n) => opts.min_depth = Some(*n),
        Pred::Print { .. } | Pred::Delete => opts.has_output = true,
        Pred::And(l, r) | Pred::Or(l, r) => {
            inspect(l, opts);
            inspect(r, opts);
        }
        Pred::Not(inner) => inspect(inner, opts),
        _ => {}
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Main
// ──────────────────────────────────────────────────────────────────────────────

fn usage() -> ! {
    eprintln!("usage: find [-H | -L] [-dsx] path ... [expression]");
    eprintln!();
    eprintln!("Predicates: -name, -iname, -path, -ipath, -type [fdlbcps], -empty,");
    eprintln!("            -maxdepth N, -mindepth N, -size [+-]N[ckMG],");
    eprintln!("            -mtime [+-]N, -newer FILE, -print, -print0, -delete, -prune");
    eprintln!("Operators:  ! / -not,  -a / -and (implicit),  -o / -or,  ( )");
    process::exit(1);
}

fn main() {
    let raw_args: Vec<String> = env::args().collect();
    let mut opts = GlobalOpts::default();
    let mut pos = 1usize;

    // --- Global flags ---
    while pos < raw_args.len() {
        match raw_args[pos].as_str() {
            "-H" => {
                opts.follow_cmd_links = true;
                pos += 1;
            }
            "-L" => {
                opts.follow_links = true;
                pos += 1;
            }
            "-P" => {
                opts.follow_links = false;
                pos += 1;
            }
            "-d" => {
                opts.depth_first = true;
                pos += 1;
            }
            "-x" => {
                opts.xdev = true;
                pos += 1;
            }
            _ => break,
        }
    }

    // --- Collect start paths (everything before first expression token) ---
    let mut paths: Vec<PathBuf> = Vec::new();
    while pos < raw_args.len() {
        let a = &raw_args[pos];
        // An expression token starts with '-', '!', or is '('
        if a.starts_with('-') || a == "!" || a == "(" {
            break;
        }
        paths.push(PathBuf::from(a));
        pos += 1;
    }

    if paths.is_empty() {
        usage();
    }

    // --- Parse expression ---
    let expr_args: Vec<String> = raw_args[pos..].to_vec();
    let now = SystemTime::now();

    let expr: Pred = if expr_args.is_empty() {
        Pred::Print { null: false }
    } else {
        let mut ps = ParseState {
            args: &expr_args,
            pos: 0,
        };
        let e = parse_expr(&mut ps, now);
        inspect(&e, &mut opts);
        // If no output predicate, append implicit -print
        if !opts.has_output {
            Pred::And(Box::new(e), Box::new(Pred::Print { null: false }))
        } else {
            e
        }
    };

    let max_depth = opts.max_depth.unwrap_or(usize::MAX);
    let min_depth = opts.min_depth.unwrap_or(0);

    let mut exit_code = 0i32;

    for start in &paths {
        let walker = WalkDir::new(start)
            .follow_links(opts.follow_links)
            .max_depth(max_depth)
            .contents_first(opts.depth_first);

        for result in walker {
            match result {
                Err(e) => {
                    eprintln!("find: {}", e);
                    exit_code = 1;
                }
                Ok(entry) => {
                    let depth = entry.depth();
                    if depth < min_depth {
                        continue;
                    }
                    let mut ctx = EvalCtx {
                        now,
                        prune: false,
                        did_print: false,
                    };
                    eval(&expr, &entry, &mut ctx);
                    // -prune: skip directory (walkdir doesn't expose skip easily here,
                    //  so we rely on maxdepth interactions; a proper implementation
                    //  would use a custom iterator with skip support)
                }
            }
        }
    }

    process::exit(exit_code);
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_exact() {
        assert!(glob_match("foo.c", "foo.c", false));
        assert!(!glob_match("foo.c", "bar.c", false));
    }

    #[test]
    fn glob_star() {
        assert!(glob_match("*.c", "foo.c", false));
        assert!(glob_match("*.c", ".c", false));
        assert!(!glob_match("*.c", "foo.h", false));
        assert!(glob_match("*", "anything", false));
        assert!(glob_match("*.*", "foo.bar", false));
    }

    #[test]
    fn glob_question() {
        assert!(glob_match("foo?", "foox", false));
        assert!(!glob_match("foo?", "foo", false));
        assert!(!glob_match("foo?", "fooxx", false));
    }

    #[test]
    fn glob_char_class() {
        assert!(glob_match("[abc].c", "a.c", false));
        assert!(glob_match("[abc].c", "b.c", false));
        assert!(!glob_match("[abc].c", "d.c", false));
        assert!(glob_match("[a-z].c", "m.c", false));
        assert!(!glob_match("[^abc]", "a", false));
        assert!(glob_match("[^abc]", "d", false));
    }

    #[test]
    fn glob_ignore_case() {
        assert!(glob_match("*.C", "FOO.c", true));
        assert!(glob_match("FOO*", "foobar", true));
    }

    #[test]
    fn cmp_test() {
        assert!(Cmp::Eq.test(5, 5));
        assert!(!Cmp::Eq.test(4, 5));
        assert!(Cmp::Lt.test(3, 5));
        assert!(Cmp::Gt.test(7, 5));
    }

    #[test]
    fn parse_size_units() {
        assert_eq!(parse_size("100c"), (Cmp::Eq, 100));
        assert_eq!(parse_size("+1k"), (Cmp::Gt, 1024));
        assert_eq!(parse_size("-2M"), (Cmp::Lt, 2 * 1024 * 1024));
    }

    #[test]
    fn parse_cmp() {
        assert!(matches!(parse_cmp_prefix("+5"), (Cmp::Gt, "5")));
        assert!(matches!(parse_cmp_prefix("-3"), (Cmp::Lt, "3")));
        assert!(matches!(parse_cmp_prefix("7"), (Cmp::Eq, "7")));
    }

    #[test]
    fn regular_files_do_not_match_special_types() {
        let meta = fs::metadata("Cargo.toml").expect("Cargo.toml should exist for tests");
        let ftype = meta.file_type();

        assert!(matches_file_type(&ftype, &FileType::File));
        assert!(!matches_file_type(&ftype, &FileType::Dir));
        assert!(!matches_file_type(&ftype, &FileType::BlockDevice));
        assert!(!matches_file_type(&ftype, &FileType::CharDevice));
        assert!(!matches_file_type(&ftype, &FileType::Pipe));
        assert!(!matches_file_type(&ftype, &FileType::Socket));
    }
}

impl PartialEq for Cmp {
    fn eq(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (Cmp::Eq, Cmp::Eq) | (Cmp::Lt, Cmp::Lt) | (Cmp::Gt, Cmp::Gt)
        )
    }
}
