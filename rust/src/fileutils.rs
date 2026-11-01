//! GNU fileutils-style commands over a tree with packed directories: a path
//! ROOT/A/B/name reads through ROOT/A/B.sqlar when the plain directory
//! ROOT/A/B/ holds no file of that name, so a packed tree is listed, read,
//! copied, moved and removed as if it were plain. Options and output formats
//! follow GNU coreutils 9; sorting is by bytes, as GNU does with LC_ALL=C.
//! Commands that change a container (rm, mv, cp into one) bump its epoch, so
//! Unison sees the change; don't run them while Unison writes the same tree.
use crate::container::{Apply, Container, Member, OpenOptions, Put};
use crate::native::{self as n, Kind, Stat, basename, concat, dirname};
use crate::{Error, Result};
use std::fs;
use std::io::{Read, Write};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};

static STATUS: AtomicI32 = AtomicI32::new(0);
static SHOW_RETAINED: AtomicBool = AtomicBool::new(false);
static TEMPS: AtomicUsize = AtomicUsize::new(0);
static PROG: Mutex<String> = Mutex::new(String::new());

pub fn status() -> i32 {
    STATUS.load(Ordering::SeqCst)
}

fn set_status(s: i32) {
    STATUS.store(s, Ordering::SeqCst);
}

fn set_prog(p: &str) {
    *PROG.lock().unwrap() = p.to_string();
}

fn err(m: String) {
    let p = PROG.lock().unwrap();
    let _ = std::io::stdout().flush();
    eprintln!("{}: {m}", if p.is_empty() { "packdir" } else { p.as_str() });
    set_status(1);
}

pub fn report(e: &Error) {
    err(e.0.clone());
}

fn quote(s: &str) -> String {
    format!("'{s}'")
}

// --- The packed view --------------------------------------------------

fn companion(name: &str) -> bool {
    name.ends_with(".sqlar") || name.ends_with(".sqlar-journal") || name.ends_with(".sqlar.repack")
        || name.ends_with(".sqlar-wal") || name.ends_with(".sqlar-shm") || name.ends_with(".db-log")
        || name == ".unison-packed" || name.contains(".sqlar.damaged-")
}

fn container_of(dir: &str) -> Option<String> {
    let c = format!("{dir}.sqlar");
    n::lstat_opt(&c).filter(|s| s.kind == Kind::Reg).map(|_| c)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum EntryKind {
    Dir,
    File,
    Link,
}

#[derive(Clone, PartialEq, Debug)]
enum Where {
    Plain,
    Member(String, String), // container, member name
}

#[derive(Clone, Debug)]
struct Entry {
    path: String,
    kind: EntryKind,
    size: i64,
    mode: u32,
    mtime_ns: i64,
    uid: u32,
    gid: u32,
    nlink: u32,
    place: Where,
}

fn with_container<T>(c: &str, read_only: bool, f: impl FnOnce(&Container) -> Result<T>) -> Result<T> {
    let k = Container::open(c.as_ref(), &OpenOptions { read_only, ..OpenOptions::default() })?;
    let r = f(&k);
    k.close()?;
    r
}

/// Members of the container standing for `dir`, with their modes.
fn members(dir: &str) -> Result<Vec<(String, Member, u32)>> {
    let Some(c) = container_of(dir) else { return Ok(Vec::new()) };
    let show = SHOW_RETAINED.load(Ordering::SeqCst);
    with_container(&c, true, |k| {
        let modes = k.modes()?;
        Ok(k.members()?
            .into_iter()
            .filter(|m| !m.retained || show)
            .map(|m| {
                let mode = modes.get(&m.name).copied().unwrap_or(0o100644);
                (c.clone(), m, mode)
            })
            .collect())
    })
}

fn plain_entry(p: &str, s: &Stat) -> Entry {
    Entry {
        path: p.to_string(),
        kind: match s.kind {
            Kind::Dir => EntryKind::Dir,
            Kind::Link => EntryKind::Link,
            _ => EntryKind::File,
        },
        size: s.size,
        mode: s.mode,
        mtime_ns: s.mtime_ns,
        uid: s.uid,
        gid: s.gid,
        nlink: 1,
        place: Where::Plain,
    }
}

fn member_entry(p: &str, c: &str, m: &Member, mode: u32) -> Result<Entry> {
    let cs = n::stat(c, true)?;
    Ok(Entry {
        path: p.to_string(),
        kind: EntryKind::File,
        size: m.size,
        mode: mode & 0o7777,
        mtime_ns: m.mtime_ns,
        uid: cs.uid,
        gid: cs.gid,
        nlink: 1,
        place: Where::Member(c.to_string(), m.name.clone()),
    })
}

/// What `p` is in the packed view: a member wins over a plain file of the
/// same name beside its container, as in Unison.
fn lookup(p: &str) -> Result<Option<Entry>> {
    let (dir, name) = (dirname(p), basename(p));
    if let Some((c, m, mode)) = members(&dir)?.iter().find(|(_, m, _)| m.name == name) {
        return Ok(Some(member_entry(p, c, m, *mode)?));
    }
    if let Some(s) = n::lstat_opt(p) {
        return Ok(Some(plain_entry(p, &s)));
    }
    if let Some(c) = container_of(p) {
        let s = n::stat(&c, true)?;
        return Ok(Some(Entry { kind: EntryKind::Dir, mode: 0o755, ..plain_entry(p, &s) }));
    }
    Ok(None)
}

fn is_dir(p: &str) -> Result<bool> {
    Ok(lookup(p)?.is_some_and(|e| e.kind == EntryKind::Dir))
}

/// Names under a directory: plain entries (a container X.sqlar as X, its
/// companions hidden) and the members of the directory's own container.
fn children(dir: &str) -> Result<Vec<String>> {
    let plain = n::readdir(dir).unwrap_or_default();
    let mut names: Vec<String> = plain
        .into_iter()
        .filter_map(|name| {
            if let Some(stem) = name.strip_suffix(".sqlar") {
                Some(stem.to_string())
            } else if companion(&name) {
                None
            } else {
                Some(name)
            }
        })
        .collect();
    names.extend(members(dir)?.into_iter().map(|(_, m, _)| m.name));
    names.sort();
    names.dedup();
    Ok(names)
}

fn read(e: &Entry) -> Result<Vec<u8>> {
    match &e.place {
        Where::Plain => fs::read(&e.path).map_err(|x| Error(format!("{}: {x}", e.path))),
        Where::Member(c, name) => with_container(c, true, |k| k.read(name)),
    }
}

// --- Options ----------------------------------------------------------

type Opts = Vec<(String, Option<String>)>;

/// Long options that take a value, as --name=VALUE or --name VALUE.
const LONG_VALUE: [&str; 6] = ["format", "printf", "sort", "time-style", "max-depth", "target-directory"];

/// GNU-style arguments: -abc bundles, --long and --long=VALUE, -- ends options.
fn parse(short_value: &[char], args: &[String]) -> (Opts, Vec<String>) {
    let (mut opts, mut pos) = (Vec::new(), Vec::new());
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        i += 1;
        if a == "--" {
            pos.extend_from_slice(&args[i..]);
            break;
        } else if a.len() > 2 && a.starts_with("--") {
            let body = &a[2..];
            if let Some((k, v)) = body.split_once('=') {
                opts.push((k.to_string(), Some(v.to_string())));
            } else if LONG_VALUE.contains(&body) && i < args.len() {
                opts.push((body.to_string(), Some(args[i].clone())));
                i += 1;
            } else {
                opts.push((body.to_string(), None));
            }
        } else if a.len() > 1 && a.starts_with('-') {
            let chars: Vec<char> = a.chars().collect();
            let mut j = 1;
            while j < chars.len() {
                let c = chars[j];
                if short_value.contains(&c) {
                    if j + 1 < chars.len() {
                        opts.push((c.to_string(), Some(chars[j + 1..].iter().collect())));
                    } else if i < args.len() {
                        opts.push((c.to_string(), Some(args[i].clone())));
                        i += 1;
                    } else {
                        err(format!("option requires an argument -- '{c}'"));
                    }
                    break;
                }
                opts.push((c.to_string(), None));
                j += 1;
            }
        } else {
            pos.push(a.clone());
        }
    }
    (opts, pos)
}

fn has(opts: &Opts, names: &[&str]) -> bool {
    opts.iter().any(|(k, _)| names.contains(&k.as_str()))
}

fn value(opts: &Opts, names: &[&str]) -> Option<String> {
    opts.iter().fold(None, |acc, (k, v)| if names.contains(&k.as_str()) { v.clone() } else { acc })
}

// --- Formatting -------------------------------------------------------

fn mode_string(kind: EntryKind, mode: u32) -> String {
    let mut b = [b'-'; 10];
    b[0] = match kind {
        EntryKind::Dir => b'd',
        EntryKind::Link => b'l',
        EntryKind::File => b'-',
    };
    for (i, c) in b"rwxrwxrwx".iter().enumerate() {
        if mode & (1 << (8 - i)) != 0 {
            b[i + 1] = *c;
        }
    }
    for (mask, pos, lo, up) in [(0o4000, 3, b's', b'S'), (0o2000, 6, b's', b'S'), (0o1000, 9, b't', b'T')] {
        if mode & mask != 0 {
            b[pos] = if b[pos] == b'x' { lo } else { up };
        }
    }
    String::from_utf8_lossy(&b).into_owned()
}

fn user(uid: u32) -> String {
    // SAFETY: the result, if any, is read at once; the commands use one thread.
    unsafe {
        let p = libc::getpwuid(uid);
        if p.is_null() { uid.to_string() } else { std::ffi::CStr::from_ptr((*p).pw_name).to_string_lossy().into_owned() }
    }
}

fn group(gid: u32) -> String {
    // SAFETY: as in [user].
    unsafe {
        let p = libc::getgrgid(gid);
        if p.is_null() { gid.to_string() } else { std::ffi::CStr::from_ptr((*p).gr_name).to_string_lossy().into_owned() }
    }
}

fn localtime(secs: i64) -> libc::tm {
    let t = secs as libc::time_t;
    // SAFETY: an all-zero tm is valid, and localtime_r fills it in.
    unsafe {
        let mut tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        tm
    }
}

/// GNU's full-iso: 2026-10-31 13:52:01.123456789 -0700
fn full_iso(ns: i64) -> String {
    let (s, frac) = (ns.div_euclid(1_000_000_000), ns.rem_euclid(1_000_000_000));
    let tm = localtime(s);
    let off = tm.tm_gmtoff;
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{frac:09} {}{:02}{:02}",
        tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec,
        if off < 0 { '-' } else { '+' }, off.abs() / 3600, off.abs() % 3600 / 60
    )
}

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// GNU's default: "Oct 31 13:52" within six months, else "Oct 31  2025".
fn ls_date(ns: i64) -> String {
    let s = ns as f64 / 1e9;
    let tm = localtime(s.floor() as i64);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
    let month = MONTHS[tm.tm_mon as usize];
    if s > now - 15778476.0 && s <= now + 60.0 {
        format!("{month} {:2} {:02}:{:02}", tm.tm_mday, tm.tm_hour, tm.tm_min)
    } else {
        format!("{month} {:2}  {}", tm.tm_mday, tm.tm_year + 1900)
    }
}

/// GNU's -h: powers of 1024, rounded up; one decimal below 10.
fn human(n: i64) -> String {
    let units = ["", "K", "M", "G", "T", "P", "E"];
    let (mut v, mut i) = (n as f64, 0);
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{v:.0}")
    } else if v < 10.0 {
        let d = (v * 10.0).ceil() / 10.0;
        if d >= 10.0 { format!("{:.0}{}", v.ceil(), units[i]) } else { format!("{d:.1}{}", units[i]) }
    } else {
        let c = v.ceil();
        if c >= 1024.0 && i < units.len() - 1 { format!("1.0{}", units[i + 1]) } else { format!("{c:.0}{}", units[i]) }
    }
}

fn out(s: &[u8]) {
    let _ = std::io::stdout().write_all(s);
}

fn line(s: &str) {
    out(format!("{s}\n").as_bytes());
}

// --- ls ---------------------------------------------------------------

struct Ls {
    long: bool,
    all: bool,
    almost: bool,
    full: bool,
    hum: bool,
    rev: bool,
    one: bool,
    no_owner: bool,
    numeric: bool,
    recursive: bool,
    by: u8, // 0 name, 1 time, 2 size, 3 none
}

impl Ls {
    fn sort(&self, mut es: Vec<Entry>) -> Vec<Entry> {
        match self.by {
            0 => es.sort_by_key(|e| basename(&e.path)),
            1 => es.sort_by(|a, b| b.mtime_ns.cmp(&a.mtime_ns).then_with(|| a.path.cmp(&b.path))),
            2 => es.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.path.cmp(&b.path))),
            _ => {}
        }
        if self.rev {
            es.reverse();
        }
        es
    }

    fn print_long(&self, es: &[Entry], name_of: &dyn Fn(&Entry) -> String) {
        let size = |e: &Entry| if self.hum { human(e.size) } else { e.size.to_string() };
        let width = |f: &dyn Fn(&Entry) -> String| es.iter().map(|e| f(e).len()).max().unwrap_or(0);
        let wl = width(&|e| e.nlink.to_string());
        let wu = width(&|e| user(e.uid));
        let wg = width(&|e| group(e.gid));
        let ws = width(&size);
        for e in es {
            let mut parts = vec![mode_string(e.kind, e.mode), format!("{:>wl$}", e.nlink)];
            if !self.no_owner {
                parts.push(format!("{:<wu$}", if self.numeric { e.uid.to_string() } else { user(e.uid) }));
            }
            parts.push(format!("{:<wg$}", if self.numeric { e.gid.to_string() } else { group(e.gid) }));
            parts.push(format!("{:>ws$}", size(e)));
            parts.push(if self.full { full_iso(e.mtime_ns) } else { ls_date(e.mtime_ns) });
            parts.push(name_of(e));
            line(&parts.join(" "));
        }
    }

    fn print_entries(&self, es: &[Entry], name_of: &dyn Fn(&Entry) -> String) {
        // SAFETY: isatty only looks at the descriptor.
        let tty = unsafe { libc::isatty(1) } == 1;
        if self.long {
            self.print_long(es, name_of);
        } else if self.one || !tty {
            for e in es {
                line(&name_of(e));
            }
        } else {
            let names: Vec<String> = es.iter().map(name_of).collect();
            let width: usize = std::env::var("COLUMNS").ok().and_then(|c| c.parse().ok()).unwrap_or(80);
            let cw = 2 + names.iter().map(|n| n.len()).max().unwrap_or(0);
            let cols = (width / cw).max(1);
            let count = names.len();
            let rows = count.div_ceil(cols);
            for r in 0..rows {
                let mut text = String::new();
                for c in 0..cols {
                    let i = c * rows + r;
                    if i < count {
                        if (c + 1) * rows + r < count {
                            text.push_str(&format!("{:<cw$}", names[i]));
                        } else {
                            text.push_str(&names[i]);
                        }
                    }
                }
                line(&text);
            }
        }
    }

    fn total(&self, es: &[Entry]) {
        if self.long {
            let k: i64 = es.iter().map(|e| (e.size + 4095) / 4096).sum();
            line(&format!("total {}", if self.hum { human(k * 4096) } else { (k * 4).to_string() }));
        }
    }

    fn dir_entries(&self, d: &str) -> Result<Vec<Entry>> {
        let mut es = Vec::new();
        if self.all {
            for name in [".", ".."] {
                let target = if name == "." { d.to_string() } else { dirname(d) };
                if let Some(e) = lookup(&target)? {
                    es.push(Entry { path: concat(d, name), ..e });
                }
            }
        }
        for name in children(d)? {
            if (self.all || self.almost || !name.starts_with('.'))
                && let Some(e) = lookup(&concat(d, &name))? {
                    es.push(e);
                }
        }
        Ok(self.sort(es))
    }

    fn show(&self, d: &str, header: bool, first: &mut bool) -> Result<()> {
        if !*first {
            line("");
        }
        *first = false;
        if header {
            line(&format!("{d}:"));
        }
        let es = self.dir_entries(d)?;
        self.total(&es);
        self.print_entries(&es, &|e| basename(&e.path));
        if self.recursive {
            for e in &es {
                let name = basename(&e.path);
                if e.kind == EntryKind::Dir && name != "." && name != ".." {
                    self.show(&e.path, header, first)?;
                }
            }
        }
        Ok(())
    }
}

fn ls(args: &[String]) -> Result<()> {
    set_prog("ls");
    let (opts, mut paths) = parse(&[], args);
    let o = |k: &[&str]| has(&opts, k);
    let by = match value(&opts, &["sort"]).as_deref() {
        Some("time") => 1,
        Some("size") => 2,
        Some("none") => 3,
        Some("name") | None => {
            if o(&["t"]) { 1 } else if o(&["S"]) { 2 } else if o(&["U", "f"]) { 3 } else { 0 }
        }
        Some(w) => {
            err(format!("invalid argument '{w}' for '--sort'"));
            0
        }
    };
    let directory = o(&["d", "directory"]);
    SHOW_RETAINED.store(o(&["retained"]), Ordering::SeqCst);
    let l = Ls {
        long: o(&["l", "full-time"]) || o(&["g", "n"]),
        all: o(&["a", "all"]),
        almost: o(&["A", "almost-all"]),
        full: o(&["full-time"]) || value(&opts, &["time-style"]).as_deref() == Some("full-iso"),
        hum: o(&["h", "human-readable"]),
        rev: o(&["r", "reverse"]),
        one: o(&["1"]),
        no_owner: o(&["g"]),
        numeric: o(&["n"]),
        recursive: o(&["R", "recursive"]),
        by,
    };
    if paths.is_empty() {
        paths.push(".".into());
    }
    let mut entries = Vec::new();
    for p in &paths {
        match lookup(p)? {
            Some(e) => entries.push(Entry { path: p.clone(), ..e }),
            None => {
                err(format!("cannot access {}: No such file or directory", quote(p)));
                set_status(2);
            }
        }
    }
    let header = l.recursive || entries.len() > 1;
    let (files, dirs): (Vec<Entry>, Vec<Entry>) = entries.into_iter().partition(|e| directory || e.kind != EntryKind::Dir);
    let (files, dirs) = (l.sort(files), l.sort(dirs));
    if !files.is_empty() {
        l.print_entries(&files, &|e| e.path.clone());
    }
    let mut first = files.is_empty();
    for e in &dirs {
        l.show(&e.path, header, &mut first)?;
    }
    Ok(())
}

// --- cat --------------------------------------------------------------

fn cat(args: &[String]) -> Result<()> {
    set_prog("cat");
    let (opts, mut paths) = parse(&[], args);
    let number = has(&opts, &["n", "number"]);
    let nonblank = has(&opts, &["b", "number-nonblank"]);
    if paths.is_empty() {
        paths.push("-".into());
    }
    let mut count = 0;
    for p in &paths {
        let d = if p == "-" {
            let mut b = Vec::new();
            std::io::stdin().read_to_end(&mut b)?;
            b
        } else {
            match lookup(p)? {
                None => {
                    err(format!("{p}: No such file or directory"));
                    continue;
                }
                Some(e) if e.kind == EntryKind::Dir => {
                    err(format!("{p}: Is a directory"));
                    continue;
                }
                Some(e) => read(&e)?,
            }
        };
        if !(number || nonblank) {
            out(&d);
            continue;
        }
        let ls: Vec<&[u8]> = d.split(|c| *c == b'\n').collect();
        let n = ls.len();
        for (i, l) in ls.iter().enumerate() {
            if i == n - 1 && l.is_empty() {
                continue;
            }
            if nonblank && l.is_empty() {
                out(b"\n");
            } else {
                count += 1;
                out(format!("{count:6}\t").as_bytes());
                out(l);
                if i < n - 1 {
                    out(b"\n");
                }
            }
        }
    }
    Ok(())
}

// --- stat -------------------------------------------------------------

fn escape_printf(s: &str) -> String {
    let (mut b, mut it) = (String::new(), s.chars().peekable());
    while let Some(c) = it.next() {
        if c == '\\' && it.peek().is_some() {
            b.push(match it.next().unwrap() {
                'n' => '\n',
                't' => '\t',
                x => x,
            });
        } else {
            b.push(c);
        }
    }
    b
}

fn stat(args: &[String]) -> Result<()> {
    set_prog("stat");
    let (opts, paths) = parse(&['c'], args);
    let (format, newline) = match (value(&opts, &["c", "format"]), value(&opts, &["printf"])) {
        (Some(f), _) => (f, true),
        (None, Some(f)) => (escape_printf(&f), false),
        _ => ("  File: %n\n  Size: %s\tType: %F\nAccess: (%04a/%A)  Uid: (%u/%U)   Gid: (%g/%G)\nModify: %y".to_string(), true),
    };
    let f: Vec<char> = format.chars().collect();
    for p in &paths {
        let Some(e) = lookup(p)? else {
            err(format!("cannot stat {}: No such file or directory", quote(p)));
            continue;
        };
        let mut b = String::new();
        let mut i = 0;
        while i < f.len() {
            if f[i] == '%' && i + 1 < f.len() {
                // %04a style: an optional width with zero fill
                let mut j = i + 1;
                while j < f.len() && f[j].is_ascii_digit() {
                    j += 1;
                }
                let width: String = f[i + 1..j].iter().collect();
                let c = if j < f.len() { f[j] } else { '%' };
                let s = match c {
                    'n' => p.clone(),
                    's' => e.size.to_string(),
                    'a' => format!("{:o}", e.mode),
                    'A' => mode_string(e.kind, e.mode),
                    'y' => full_iso(e.mtime_ns),
                    'Y' => (e.mtime_ns / 1_000_000_000).to_string(),
                    'F' => match e.kind {
                        EntryKind::Dir => "directory".into(),
                        EntryKind::Link => "symbolic link".into(),
                        EntryKind::File if e.size == 0 => "regular empty file".into(),
                        EntryKind::File => "regular file".to_string(),
                    },
                    'U' => user(e.uid),
                    'G' => group(e.gid),
                    'u' => e.uid.to_string(),
                    'g' => e.gid.to_string(),
                    'h' => e.nlink.to_string(),
                    '%' => "%".into(),
                    c => format!("?{c}"),
                };
                if !width.is_empty() {
                    let w: usize = width.parse().unwrap_or(0);
                    if s.len() < w {
                        let fill = if width.starts_with('0') { '0' } else { ' ' };
                        b.extend(std::iter::repeat_n(fill, w - s.len()));
                    }
                }
                b.push_str(&s);
                i = j;
            } else {
                b.push(f[i]);
            }
            i += 1;
        }
        out(b.as_bytes());
        if newline {
            line("");
        }
    }
    Ok(())
}

// --- du ---------------------------------------------------------------

struct Du {
    bytes: bool,
    hum: bool,
    summ: bool,
    all: bool,
    max_depth: usize,
}

impl Du {
    fn fmt(&self, n: i64) -> String {
        if self.hum {
            human(n)
        } else if self.bytes {
            n.to_string()
        } else {
            ((n + 1023) / 1024).to_string()
        }
    }

    fn walk(&self, p: &str, depth: usize) -> Result<i64> {
        match lookup(p)? {
            None => {
                err(format!("cannot access {}: No such file or directory", quote(p)));
                Ok(0)
            }
            Some(e) if e.kind != EntryKind::Dir => {
                if (self.all || depth == 0) && depth <= self.max_depth && !(self.summ && depth > 0) {
                    line(&format!("{}\t{p}", self.fmt(e.size)));
                }
                Ok(if self.bytes || self.hum { e.size } else { (e.size + 1023) / 1024 * 1024 })
            }
            Some(_) => {
                let mut s = 0;
                for name in children(p)? {
                    s += self.walk(&concat(p, &name), depth + 1)?;
                }
                if depth <= self.max_depth && !(self.summ && depth > 0) {
                    line(&format!("{}\t{p}", self.fmt(s)));
                }
                Ok(s)
            }
        }
    }
}

/// Sizes are apparent sizes (members have no blocks of their own): -b is
/// bytes, otherwise 1024-byte units rounded up per file, as GNU du
/// --apparent-size reports them. Directories themselves count 0.
fn du(args: &[String]) -> Result<()> {
    set_prog("du");
    let (opts, mut paths) = parse(&['d'], args);
    let o = |k: &[&str]| has(&opts, k);
    let max_depth = match value(&opts, &["d", "max-depth"]) {
        Some(d) => d.parse().map_err(|_| Error("int_of_string".into()))?,
        None => usize::MAX,
    };
    let d = Du { bytes: o(&["b", "bytes"]), hum: o(&["h", "human-readable"]), summ: o(&["s", "summarize"]), all: o(&["a", "all"]), max_depth };
    if paths.is_empty() {
        paths.push(".".into());
    }
    let mut grand = 0;
    for p in &paths {
        grand += d.walk(p, 0)?;
    }
    if o(&["c", "total"]) {
        line(&format!("{}\ttotal", d.fmt(grand)));
    }
    Ok(())
}

// --- find -------------------------------------------------------------

/// fnmatch-style patterns: * ? and [...]
fn glob(p: &[u8], s: &[u8]) -> bool {
    let Some(&c) = p.first() else { return s.is_empty() };
    match c {
        b'*' => glob(&p[1..], s) || (!s.is_empty() && glob(p, &s[1..])),
        b'?' => !s.is_empty() && glob(&p[1..], &s[1..]),
        b'[' if p[1..].contains(&b']') => {
            let close = 1 + p[1..].iter().position(|x| *x == b']').unwrap();
            let mut set = &p[1..close];
            let neg = !set.is_empty() && (set[0] == b'!' || set[0] == b'^');
            if neg {
                set = &set[1..];
            }
            let Some(&ch) = s.first() else { return false };
            let member = (0..set.len()).any(|k| (k + 2 < set.len() && set[k + 1] == b'-' && ch >= set[k] && ch <= set[k + 2]) || set[k] == ch);
            member != neg && glob(&p[close + 1..], &s[1..])
        }
        c => !s.is_empty() && s[0] == c && glob(&p[1..], &s[1..]),
    }
}

struct Find {
    name: Option<String>,
    kind: Option<String>,
    maxd: usize,
    mind: usize,
}

impl Find {
    fn walk(&self, p: &str, depth: usize) -> Result<()> {
        let Some(e) = lookup(p)? else {
            err(format!("{}: No such file or directory", quote(p)));
            return Ok(());
        };
        let kind_ok = match self.kind.as_deref() {
            Some("f") => e.kind == EntryKind::File,
            Some("d") => e.kind == EntryKind::Dir,
            Some("l") => e.kind == EntryKind::Link,
            Some(t) => {
                err(format!("unknown argument to -type: {t}"));
                std::process::exit(1);
            }
            None => true,
        };
        if depth >= self.mind && self.name.as_ref().is_none_or(|pat| glob(pat.as_bytes(), basename(p).as_bytes())) && kind_ok {
            line(p);
        }
        if e.kind == EntryKind::Dir && depth < self.maxd {
            for name in children(p)? {
                self.walk(&concat(p, &name), depth + 1)?;
            }
        }
        Ok(())
    }
}

fn find(args: &[String]) -> Result<()> {
    set_prog("find");
    let split = args.iter().position(|a| a.starts_with('-')).unwrap_or(args.len());
    let (paths, expr) = args.split_at(split);
    let mut f = Find { name: None, kind: None, maxd: usize::MAX, mind: 0 };
    let number = |s: &str| s.parse::<usize>().map_err(|_| Error("int_of_string".into()));
    let mut i = 0;
    while i < expr.len() {
        match (expr[i].as_str(), expr.get(i + 1)) {
            ("-name", Some(v)) => f.name = Some(v.clone()),
            ("-type", Some(v)) => f.kind = Some(v.clone()),
            ("-maxdepth", Some(v)) => f.maxd = number(v)?,
            ("-mindepth", Some(v)) => f.mind = number(v)?,
            ("-print", _) => {
                i += 1;
                continue;
            }
            (a, _) => {
                err(format!("unknown predicate {}", quote(a)));
                std::process::exit(1);
            }
        }
        i += 2;
    }
    if paths.is_empty() {
        f.walk(".", 0)
    } else {
        paths.iter().try_for_each(|p| f.walk(p, 0))
    }
}

// --- rm ---------------------------------------------------------------

struct Rm {
    force: bool,
    recursive: bool,
    dirs: bool,
    verbose: bool,
}

impl Rm {
    fn say(&self, m: String) {
        if self.verbose {
            line(&m);
        }
    }

    fn remove(&self, p: &str) -> Result<()> {
        let Some(e) = lookup(p)? else {
            if !self.force {
                err(format!("cannot remove {}: No such file or directory", quote(p)));
            }
            return Ok(());
        };
        if let Where::Member(c, name) = &e.place {
            with_container(c, false, |k| k.remove(std::slice::from_ref(name)))?;
            self.say(format!("removed {}", quote(p)));
        } else if e.kind == EntryKind::Dir {
            if !self.recursive && !self.dirs {
                err(format!("cannot remove {}: Is a directory", quote(p)));
                return Ok(());
            }
            if self.recursive {
                for name in children(p)? {
                    self.remove(&concat(p, &name))?;
                }
            }
            // An emptied container goes with its directory.
            if let Some(c) = container_of(p) {
                if self.recursive || with_container(&c, true, |k| k.members())?.is_empty() {
                    fs::remove_file(&c)?;
                    for x in ["-journal", "-wal", "-shm"] {
                        let _ = fs::remove_file(format!("{c}{x}"));
                    }
                } else {
                    err(format!("cannot remove {}: Directory not empty", quote(p)));
                }
            }
            if n::exists(p)
                && let Err(x) = fs::remove_dir(p) {
                    if x.raw_os_error() == Some(libc::ENOTEMPTY) {
                        err(format!("cannot remove {}: Directory not empty", quote(p)));
                    } else {
                        return Err(x.into());
                    }
                }
            if !n::exists(p) {
                self.say(format!("removed directory {}", quote(p)));
            }
        } else {
            fs::remove_file(p)?;
            self.say(format!("removed {}", quote(p)));
        }
        Ok(())
    }
}

fn rm(args: &[String]) -> Result<()> {
    set_prog("rm");
    let (opts, paths) = parse(&[], args);
    let r = Rm {
        force: has(&opts, &["f", "force"]),
        recursive: has(&opts, &["r", "R", "recursive"]),
        dirs: has(&opts, &["d", "dir"]),
        verbose: has(&opts, &["v", "verbose"]),
    };
    if paths.is_empty() {
        err("missing operand".into());
        std::process::exit(1);
    }
    paths.iter().try_for_each(|p| r.remove(p))
}

// --- cp and mv --------------------------------------------------------

fn temp_file(data: &[u8]) -> Result<String> {
    let name = format!("packdir-cp{}-{}", std::process::id(), TEMPS.fetch_add(1, Ordering::SeqCst));
    let path = std::env::temp_dir().join(name).to_string_lossy().into_owned();
    fs::write(&path, data)?;
    Ok(path)
}

fn store_member(c: &str, dst: &str, source: &str) -> Result<()> {
    let put = Put { member: basename(dst), source: source.to_string() };
    with_container(c, false, |k| k.apply(Apply::default(), &[put], &[]).map(|_| ()))
}

/// Write `e`'s bytes to `dst` in the packed view: a member when dst's
/// directory is packed, else a plain file.
fn put(preserve: bool, e: &Entry, dst: &str) -> Result<()> {
    match container_of(&dirname(dst)) {
        Some(c) => {
            // Into a packed directory: a member, which wins over a plain file
            // of the same name beside the container. Members are staged
            // through a plain file.
            let plain = e.place == Where::Plain;
            let staged = if plain {
                None
            } else {
                let t = temp_file(&read(e)?)?;
                n::chmod(&t, e.mode)?;
                n::utimens(&t, e.mtime_ns, e.mtime_ns)?;
                Some(t)
            };
            if !preserve && plain {
                // Without -p a copy gets the time of copying, like cp.
                let t = temp_file(&read(e)?)?;
                store_member(&c, dst, &t)?;
                fs::remove_file(t)?;
            } else {
                store_member(&c, dst, staged.as_deref().unwrap_or(&e.path))?;
            }
            if let Some(t) = staged {
                fs::remove_file(t)?;
            }
        }
        None => {
            fs::write(dst, read(e)?).map_err(|x| Error(format!("{dst}: {x}")))?;
            if preserve {
                n::chmod(dst, e.mode)?;
                n::utimens(dst, e.mtime_ns, e.mtime_ns)?;
            }
        }
    }
    Ok(())
}

struct CpMv {
    do_move: bool,
    preserve: bool,
    recursive: bool,
    noclobber: bool,
    verbose: bool,
}

impl CpMv {
    fn copy(&self, src: &str, dst: &str) -> Result<()> {
        let Some(e) = lookup(src)? else {
            err(format!("cannot stat {}: No such file or directory", quote(src)));
            return Ok(());
        };
        if e.kind == EntryKind::Dir {
            if self.do_move && e.place == Where::Plain && container_of(src).is_none() {
                fs::rename(src, dst)?;
                if self.verbose {
                    line(&format!("renamed {} -> {}", quote(src), quote(dst)));
                }
            } else if !self.recursive && !self.do_move {
                err(format!("-r not specified; omitting directory {}", quote(src)));
            } else {
                if !is_dir(dst)? {
                    fs::create_dir(dst)?;
                }
                for name in children(src)? {
                    self.copy(&concat(src, &name), &concat(dst, &name))?;
                }
                if self.do_move {
                    rm(&["-r".into(), "-f".into(), src.to_string()])?;
                }
            }
            return Ok(());
        }
        if self.noclobber && lookup(dst)?.is_some() {
            return Ok(());
        }
        let target = if self.do_move { container_of(&dirname(dst)) } else { None };
        match (&e.place, target) {
            (Where::Member(c, name), Some(t)) if *c == t => {
                with_container(c, false, |k| k.rename(name, &basename(dst)))?;
            }
            _ => {
                put(self.preserve, &e, dst)?;
                if self.do_move {
                    rm(&["-f".into(), src.to_string()])?;
                }
            }
        }
        if self.verbose {
            line(&format!("{}{} -> {}", if self.do_move { "renamed " } else { "" }, quote(src), quote(dst)));
        }
        Ok(())
    }
}

fn cp_mv(do_move: bool, args: &[String]) -> Result<()> {
    set_prog(if do_move { "mv" } else { "cp" });
    let (opts, paths) = parse(&['t'], args);
    let o = |k: &[&str]| has(&opts, k);
    let (srcs, dest) = match value(&opts, &["t", "target-directory"]) {
        Some(t) => (paths, t),
        None if paths.len() >= 2 => {
            let (d, rs) = paths.split_last().unwrap();
            (rs.to_vec(), d.clone())
        }
        None => {
            err("missing destination file operand".into());
            std::process::exit(1);
        }
    };
    let into = is_dir(&dest)?;
    if srcs.len() > 1 && !into {
        err(format!("target {} is not a directory", quote(&dest)));
        std::process::exit(1);
    }
    let c = CpMv {
        do_move,
        preserve: do_move || o(&["p", "a", "archive", "preserve"]),
        recursive: o(&["r", "R", "a", "recursive", "archive"]),
        noclobber: o(&["n", "no-clobber"]),
        verbose: o(&["v", "verbose"]),
    };
    for s in &srcs {
        let to = if into { concat(&dest, &basename(s)) } else { dest.clone() };
        c.copy(s, &to)?;
    }
    Ok(())
}

fn cp(args: &[String]) -> Result<()> {
    cp_mv(false, args)
}

fn mv(args: &[String]) -> Result<()> {
    cp_mv(true, args)
}

// --- mkdir ------------------------------------------------------------

struct Mkdir {
    parents: bool,
    verbose: bool,
    packed: bool,
}

impl Mkdir {
    fn make(&self, p: &str) -> Result<()> {
        if self.packed {
            with_container(&format!("{p}.sqlar"), false, |_| Ok(()))?;
        } else {
            fs::create_dir(p).map_err(|e| Error(format!("{p}: {e}")))?;
        }
        if self.verbose {
            line(&format!("mkdir: created directory {}", quote(p)));
        }
        Ok(())
    }

    fn make_one(&self, p: &str) -> Result<()> {
        let up = dirname(p);
        let fail = |why: &str| err(format!("cannot create directory {}: {why}", quote(p)));
        match lookup(&up)? {
            None => fail("No such file or directory"),
            Some(e) if e.kind == EntryKind::Dir && (container_of(&up).is_none() || n::exists(&up)) => self.make(p)?,
            Some(_) => fail("Not a directory"),
        }
        Ok(())
    }

    fn go(&self, p: &str) -> Result<()> {
        let up = dirname(p);
        match lookup(p)? {
            Some(e) if e.kind == EntryKind::Dir && self.parents => Ok(()),
            Some(_) => {
                err(format!("cannot create directory {}: File exists", quote(p)));
                Ok(())
            }
            None if self.parents && lookup(&up)?.is_none() => {
                self.go(&up)?;
                if status() == 0 { self.make_one(p) } else { Ok(()) }
            }
            None => self.make_one(p),
        }
    }
}

/// A plain directory, as GNU's; --packed makes an empty container instead, so
/// files copied in become members. A member cannot hold a directory.
fn mkdir(args: &[String]) -> Result<()> {
    set_prog("mkdir");
    let (opts, paths) = parse(&[], args);
    let m = Mkdir { parents: has(&opts, &["p", "parents"]), verbose: has(&opts, &["v", "verbose"]), packed: has(&opts, &["packed"]) };
    if paths.is_empty() {
        err("missing operand".into());
        std::process::exit(1);
    }
    paths.iter().try_for_each(|p| m.go(p))
}

pub type Command = fn(&[String]) -> Result<()>;

pub fn command(name: &str) -> Option<Command> {
    Some(match name {
        "ls" => ls,
        "cat" => cat,
        "stat" => stat,
        "du" => du,
        "find" => find,
        "rm" => rm,
        "cp" => cp,
        "mv" => mv,
        "mkdir" => mkdir,
        _ => return None,
    })
}
