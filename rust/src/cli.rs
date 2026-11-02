//! The packdir command line, the same as the OCaml build's:
//!
//!   packdir pack-tree ROOT OUT [--depth N] [--workers N] [--wal] [--batch-mib M]
//!                              [--engine sqlite|turso] [--mvcc] [--writers N] [--clones]
//!   packdir list C.sqlar
//!   packdir verify C.sqlar...
//!   packdir restore C.sqlar OUT_DIR [--skip-retained]
//!   packdir unpack-tree OUT DEST [--skip-retained]
//!   packdir repack C.sqlar... [--drop-retained] [--engine sqlite|turso]
//!   packdir ls|cat|stat|du|find|rm|cp|mv|mkdir [GNU options] PATH...
//!
//! Workers are threads here and processes in OCaml: the same containers go to
//! the same worker, and a crash ends the whole run either way.
use crate::container::{self, Apply, ClonePut, Container, Member, OpenOptions, Put, WriteLock, default_engine, plan_deferred};
use crate::native::{self as n, Kind, concat, dirname};
use crate::{Error, Result, fileutils};
use firstpair_tursolite::{Engine, Journal};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::sync::Mutex;
use std::time::Instant;

fn prog() -> String {
    std::env::args().next().map(|a| n::basename(&a)).unwrap_or_else(|| "packdir".into())
}

fn usage() -> ! {
    eprintln!(
        "usage: packdir pack-tree ROOT OUT [--depth N] [--workers N] [--wal] [--batch-mib M]
                 [--engine sqlite|turso] [--mvcc] [--writers N] [--clones]
       packdir list C.sqlar
       packdir verify C.sqlar...
       packdir restore C.sqlar OUT_DIR [--skip-retained]
       packdir unpack-tree OUT DEST [--skip-retained]
       packdir repack C.sqlar... [--drop-retained] [--engine sqlite|turso]
       packdir ls|cat|stat|du|find|rm|cp|mv|mkdir [GNU options] PATH..."
    );
    std::process::exit(2)
}

fn dirs_at(depth: usize, root: &str, rel: &str) -> Result<Vec<String>> {
    if depth == 0 {
        return Ok(vec![rel.to_string()]);
    }
    let here = if rel.is_empty() { root.to_string() } else { concat(root, rel) };
    let mut names = n::readdir(&here)?;
    names.sort();
    let mut out = Vec::new();
    for name in names {
        let r = if rel.is_empty() { name } else { concat(rel, &name) };
        if n::lstat_opt(&concat(root, &r)).is_some_and(|s| s.kind == Kind::Dir) {
            out.extend(dirs_at(depth - 1, root, &r)?);
        }
    }
    Ok(out)
}

/// One writer per output disk: the transactions of all workers take this lock;
/// reading and hashing happen outside it. fcntl locks belong to the process,
/// so threads take the mutex first.
struct Lock {
    file: fs::File,
    turn: Mutex<()>,
}

impl Lock {
    fn new(out: &str) -> Result<Lock> {
        fs::create_dir_all(out)?;
        let path = concat(out, ".packdir-writer.lock");
        let file = fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
        Ok(Lock { file, turn: Mutex::new(()) })
    }

    fn with<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        let _turn = self.turn.lock().unwrap();
        // SAFETY: the descriptor stays open as long as self.
        if unsafe { libc::lockf(self.file.as_raw_fd(), libc::F_LOCK, 0) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let result = f();
        // SAFETY: as above.
        unsafe { libc::lockf(self.file.as_raw_fd(), libc::F_ULOCK, 0) };
        result
    }
}

type Stamp = (i64, i64, i64);
type State = HashMap<String, (Stamp, Vec<Member>)>;

const STATE_MAGIC: &str = "packdir-state/1";

fn state_file(out: &str) -> String {
    concat(out, ".packdir-state")
}

fn escape(s: &str) -> String {
    s.replace('%', "%25").replace('\t', "%09").replace('\n', "%0A")
}

fn unescape(s: &str) -> String {
    s.replace("%0A", "\n").replace("%09", "\t").replace("%25", "%")
}

fn parse_state(text: &str) -> Option<State> {
    let mut lines = text.lines();
    if lines.next()? != STATE_MAGIC {
        return None;
    }
    let mut state = State::new();
    let mut current: Option<String> = None;
    for line in lines {
        let f: Vec<&str> = line.split('\t').collect();
        match f.as_slice() {
            ["C", rel, ino, size, mtime] => {
                let rel = unescape(rel);
                state.insert(rel.clone(), ((ino.parse().ok()?, size.parse().ok()?, mtime.parse().ok()?), Vec::new()));
                current = Some(rel);
            }
            ["M", name, size, mtime_ns, fp, fpalg, sha256, retained] => {
                state.get_mut(current.as_ref()?)?.1.push(Member {
                    name: unescape(name),
                    size: size.parse().ok()?,
                    mtime_ns: mtime_ns.parse().ok()?,
                    fp: n::from_hex(fp).ok()?,
                    fpalg: fpalg.to_string(),
                    sha256: sha256.to_string(),
                    retained: *retained == "1",
                });
            }
            _ => return None,
        }
    }
    Some(state)
}

/// OUT/.packdir-state: for each container, its file's (inode, size, mtime)
/// after the last run and its members then. Losing it, or an older form, only
/// costs a full re-read.
fn load_state(out: &str) -> State {
    fs::read(state_file(out)).ok().and_then(|b| parse_state(&String::from_utf8_lossy(&b))).unwrap_or_default()
}

fn save_state(out: &str, state: &State) -> Result<()> {
    let temp = format!("{}.tmp", state_file(out));
    let mut w = std::io::BufWriter::new(fs::File::create(&temp)?);
    writeln!(w, "{STATE_MAGIC}")?;
    let mut rels: Vec<&String> = state.keys().collect();
    rels.sort();
    for rel in rels {
        let ((ino, size, mtime), members) = &state[rel];
        writeln!(w, "C\t{}\t{ino}\t{size}\t{mtime}", escape(rel))?;
        for m in members {
            writeln!(w, "M\t{}\t{}\t{}\t{}\t{}\t{}\t{}", escape(&m.name), m.size, m.mtime_ns, n::to_hex(&m.fp), m.fpalg, m.sha256, m.retained as u8)?;
        }
    }
    w.flush()?;
    drop(w);
    fs::rename(temp, state_file(out))?;
    Ok(())
}

/// OUT/.packdir-pending: containers written since the last barrier. Each is
/// added before its first write and the list is cleared only after the run's
/// final barrier and state save, so after a crash or power loss the next run
/// verifies exactly the containers that may not have reached the platter.
fn pending_file(out: &str) -> String {
    concat(out, ".packdir-pending")
}

fn load_pending(out: &str) -> HashSet<String> {
    fs::read_to_string(pending_file(out)).unwrap_or_default().lines().filter(|l| !l.is_empty()).map(String::from).collect()
}

fn add_pending(out: &str, rel: &str) -> Result<()> {
    let mut f = fs::OpenOptions::new().append(true).create(true).open(pending_file(out))?;
    f.write_all(format!("{rel}\n").as_bytes())?;
    f.sync_all()?;
    Ok(())
}

fn clear_pending(out: &str) -> Result<()> {
    let temp = format!("{}.tmp", pending_file(out));
    fs::File::create(&temp)?;
    fs::rename(temp, pending_file(out))?;
    Ok(())
}

pub struct PackOptions {
    pub depth: usize,
    pub journal: Journal,
    pub engine: Engine,
    pub batch_bytes: usize,
    pub workers: usize,
    pub writers: usize,
    pub barrier_bytes: usize,
}

#[derive(Default)]
struct Totals {
    stored: usize,
    retained: usize,
    bytes: usize,
    opened: usize,
    barriers: usize,
    scan_seconds: f64,
    updates: Vec<(String, Stamp, Vec<Member>)>,
}

struct Run<'a> {
    root: &'a str,
    out: &'a str,
    o: &'a PackOptions,
    state: &'a State,
    suspect: &'a HashSet<String>,
    lock: &'a Lock,
    /// --clones: per container, the clone group members stored after the
    /// parallel pass.
    defer: &'a Defer,
}

impl Run<'_> {
    fn barrier(&self, t: &mut Totals, since: &mut usize) -> Result<()> {
        self.lock.with(|| container::barrier(&concat(self.out, ".packdir-writer.lock")))?;
        t.barriers += 1;
        *since = 0;
        Ok(())
    }

    fn options(&self) -> OpenOptions {
        OpenOptions { journal: self.o.journal, engine: Some(self.o.engine), ..OpenOptions::default() }
    }

    fn deferred(&self, rel: &str) -> &[String] {
        self.defer.get(rel).map_or(&[], |v| v.as_slice())
    }

    fn one(&self, rel: &str, t: &mut Totals, since: &mut usize) -> Result<()> {
        let target = format!("{}.sqlar", concat(self.out, rel));
        // A plain directory beside the container is pre-existing content, never
        // touched. A file named like one of its files is not packed while the
        // bytes are the same (present-plain); once they differ, the newer bytes
        // are kept updated in the container beside the plain original.
        let plain = concat(self.out, rel);
        let mut skip = Vec::new();
        if n::is_dir(&plain) {
            println!("note: plain {rel}/ beside {rel}.sqlar is left as it is");
            for name in n::readdir(&plain)? {
                let mine = concat(&concat(self.root, rel), &name);
                let theirs = concat(&plain, &name);
                if n::exists(&mine) && !n::is_dir(&theirs) {
                    if n::stat(&mine, true)?.size == n::stat(&theirs, true)?.size && n::sha_file(&mine)? == n::sha_file(&theirs)? {
                        println!("present-plain: {rel}/{name}");
                        skip.push(name);
                    } else {
                        println!("packed-beside-plain: {rel}/{name} (the plain original differs and stays)");
                    }
                }
            }
        }
        // A container whose file is as recorded after the last run needs no
        // opening: its members come from the state, the role Unison's archive
        // plays. Only containers with something to store are opened.
        let stamp = || n::stat(&target, true).ok().map(|s| (s.ino, s.size, s.mtime_ns));
        let src = concat(self.root, rel);
        let begun = Instant::now();
        // Written since the last barrier of an earlier run, or changed outside
        // the tool: verified before it is trusted.
        let recorded = self.state.get(rel);
        let suspicious = n::exists(&target) && (self.suspect.contains(rel) || recorded.is_some_and(|(st, _)| Some(*st) != stamp()));
        let unchanged = !suspicious
            && match recorded {
                Some((st, known)) if Some(*st) == stamp() => {
                    let (puts, retain) = plan_deferred(&skip, self.deferred(rel), known, &src)?;
                    puts.is_empty() && retain.is_empty()
                }
                _ => false,
            };
        t.scan_seconds += begun.elapsed().as_secs_f64();
        if unchanged {
            return Ok(());
        }
        t.opened += 1;
        if suspicious {
            let c = Container::open(target.as_ref(), &self.options())?;
            let problems = c.verify();
            c.close()?;
            let problems = problems?;
            if !problems.is_empty() {
                // Never deleted: set aside, and the leaf packed again from the plain side.
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
                let aside = format!("{target}.damaged-{now}");
                self.lock.with(|| Ok(fs::rename(&target, &aside)?))?;
                println!("damaged: {rel} ({}); set aside as {aside} and packed again", problems.join("; "));
            }
        }
        self.lock.with(|| add_pending(self.out, rel))?;
        let c = self.lock.with(|| {
            fs::create_dir_all(dirname(&target))?;
            Container::open(target.as_ref(), &self.options())
        })?;
        // MVCC writes containers side by side; everything else takes turns.
        let take_turn = |f: &mut dyn FnMut() -> Result<()>| self.lock.with(f);
        let write_lock: WriteLock<'_> =
            if self.o.journal == Journal::Mvcc { None } else { Some(&take_turn) };
        let a = Apply { batch_bytes: self.o.batch_bytes, write_lock, writers: self.o.writers };
        let result = c.update_deferred(a, &skip, self.deferred(rel), &src).and_then(|r| Ok((r, c.members()?)));
        c.close()?;
        let ((stored, retained, bytes), members) = result?;
        t.stored += stored;
        t.retained += retained;
        t.bytes += bytes;
        *since += bytes;
        if *since >= self.o.barrier_bytes {
            self.barrier(t, since)?;
        }
        if let Some(st) = stamp() {
            t.updates.push((rel.to_string(), st, members));
        }
        Ok(())
    }
}

/// A clone group: (container rel, name) of each member, in path order.
type Group = Vec<(String, String)>;
/// Per container, the clone group members stored after the parallel pass.
type Defer = HashMap<String, Vec<String>>;

/// --clones: APFS clone groups among the files under the containers' leaves,
/// and, for each container, the group members that are new or changed since
/// the state: those are stored after the parallel pass, the first of a group
/// with no current member as data, the rest as references to its original.
fn clone_groups(root: &str, dirs: &[String], state: &State) -> Result<(Vec<Group>, Defer)> {
    let mut by_key: HashMap<(u64, u64), Group> = HashMap::new();
    for rel in dirs {
        let dir = concat(root, rel);
        let mut names = n::readdir(&dir).unwrap_or_default();
        names.sort();
        for name in names {
            if let Some(k) = n::clone_key(&concat(&dir, &name)) {
                by_key.entry(k).or_default().push((rel.clone(), name));
            }
        }
    }
    let mut groups: Vec<Group> = by_key
        .into_values()
        .filter(|g| g.len() > 1)
        .map(|mut g| {
            g.sort();
            g
        })
        .collect();
    groups.sort();
    let mut defer = Defer::new();
    for (rel, name) in groups.iter().flatten() {
        let st = n::stat(&concat(&concat(root, rel), name), true)?;
        let current = state.get(rel).is_some_and(|(_, ms)| {
            ms.iter().any(|m| m.name == *name && m.size == st.size && m.mtime_ns == st.mtime_ns && !m.retained)
        });
        if !current {
            defer.entry(rel.clone()).or_default().push(name.clone());
        }
    }
    Ok((groups, defer))
}

impl Run<'_> {
    /// A container of the serial pass, opened for writing and listed as pending.
    fn with_container<T>(&self, rel: &str, touched: &mut Vec<String>, f: impl FnOnce(&Container) -> Result<T>) -> Result<T> {
        let target = format!("{}.sqlar", concat(self.out, rel));
        self.lock.with(|| add_pending(self.out, rel))?;
        if !touched.iter().any(|r| r == rel) {
            touched.push(rel.to_string());
        }
        let c = self.lock.with(|| {
            fs::create_dir_all(dirname(&target))?;
            Container::open(target.as_ref(), &self.options())
        })?;
        let r = f(&c);
        let closed = c.close();
        let v = r?;
        closed?;
        Ok(v)
    }

    /// The serial pass for --clones. For each group with deferred members, the
    /// original is its first member held as current data, or a current clone's
    /// original; failing both, the first deferred member is stored as data and
    /// becomes the original. The other deferred members are stored as clones
    /// of it, a container at a time, and a container whose clones don't check
    /// out (a file changed since the scan) stores them as data instead.
    /// Returns the containers touched, and (stored, bytes, clones).
    fn store_clones(&self, groups: &[Group]) -> Result<(Vec<String>, (usize, usize, usize))> {
        let (root, out) = (self.root, self.out);
        let logical = |rel: &str, name: &str| concat(&concat(out, rel), name);
        type Info = (Vec<Member>, Vec<(String, String, String)>);
        let mut opened: HashMap<String, Info> = HashMap::new();
        let mut current = |rel: &str, name: &str| -> Result<Option<String>> {
            if !opened.contains_key(rel) {
                let path = format!("{}.sqlar", concat(out, rel));
                let info = if !n::exists(&path) {
                    (Vec::new(), Vec::new())
                } else {
                    let o = OpenOptions { create: false, read_only: true, engine: Some(self.o.engine), ..OpenOptions::default() };
                    let c = Container::open(path.as_ref(), &o)?;
                    let r = c.members().and_then(|ms| Ok((ms, c.clones()?)));
                    c.close()?;
                    r?
                };
                opened.insert(rel.to_string(), info);
            }
            let (ms, cs) = &opened[rel];
            let st = n::stat(&concat(&concat(root, rel), name), true)?;
            Ok(match ms.iter().find(|m| m.name == name) {
                Some(m) if m.size == st.size && m.mtime_ns == st.mtime_ns && !m.retained => {
                    match cs.iter().find(|(c, _, _)| c == name) {
                        Some((_, original, _)) => Some(container::normalize(&concat(&concat(out, rel), original))),
                        None => Some(logical(rel, name)),
                    }
                }
                _ => None,
            })
        };
        let mut data: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut clones: BTreeMap<String, Vec<ClonePut>> = BTreeMap::new();
        for group in groups {
            let mut deferred = Vec::new();
            for (rel, name) in group {
                if self.deferred(rel).contains(name) && current(rel, name)?.is_none() {
                    deferred.push((rel, name));
                }
            }
            if deferred.is_empty() {
                continue;
            }
            let mut found = None;
            for (rel, name) in group {
                if let Some(o) = current(rel, name)? {
                    found = Some(o);
                    break;
                }
            }
            let (original, rest) = match found {
                Some(o) => (o, &deferred[..]),
                None => {
                    let (rel, name) = deferred[0];
                    data.entry(rel.clone()).or_default().push(name.clone());
                    (logical(rel, name), &deferred[1..])
                }
            };
            for (rel, name) in rest {
                clones.entry((*rel).clone()).or_default().push(ClonePut {
                    clone: (*name).clone(),
                    file: concat(&concat(root, rel), name),
                    original: container::relative(&concat(out, rel), &original),
                });
            }
        }
        let (mut touched, mut stored, mut bytes, mut kept) = (Vec::new(), 0, 0, 0);
        // MVCC writes containers side by side; everything else takes turns.
        let take_turn = |f: &mut dyn FnMut() -> Result<()>| self.lock.with(f);
        let write_lock: WriteLock<'_> = if self.o.journal == Journal::Mvcc { None } else { Some(&take_turn) };
        let a = Apply { write_lock, ..Apply::default() };
        // (stored, bytes) of members stored as data.
        let store_data = |rel: &str, names: &[String], touched: &mut Vec<String>| -> Result<(usize, usize)> {
            let puts: Vec<Put> = names.iter().map(|name| Put { member: name.clone(), source: concat(&concat(root, rel), name) }).collect();
            Ok((puts.len(), self.with_container(rel, touched, |c| c.apply(a, &puts, &[]))?))
        };
        for (rel, names) in &data {
            let (s, b) = store_data(rel, names, &mut touched)?;
            stored += s;
            bytes += b;
        }
        for (rel, cps) in &clones {
            match self.with_container(rel, &mut touched, |c| c.apply_clones(a, cps)) {
                Ok(_) => {
                    stored += cps.len();
                    kept += cps.len();
                }
                Err(e) => {
                    println!("note: clones in {rel} stored as data ({e})");
                    let (s, b) = store_data(rel, &cps.iter().map(|cp| cp.clone.clone()).collect::<Vec<_>>(), &mut touched)?;
                    stored += s;
                    bytes += b;
                }
            }
        }
        Ok((touched, (stored, bytes, kept)))
    }
}

/// Keeps every directory `depth` levels under `root` as `out/<rel>.sqlar`.
/// Workers read and hash containers in parallel, one writes at a time unless
/// the journal is MVCC; a rerun stores only new and changed files and marks
/// the vanished ones retained. Prints the run's totals as one line of JSON.
pub fn pack_tree(root: &str, out: &str, o: &PackOptions) -> Result<()> {
    pack_tree_with_clones(root, out, o, false)
}

/// [pack_tree]; with `clones`, each APFS clone of another file in the tree
/// is kept as a reference to it, and the JSON line counts them.
pub fn pack_tree_with_clones(root: &str, out: &str, o: &PackOptions, clones: bool) -> Result<()> {
    let begun = Instant::now();
    let mut state = load_state(out);
    let suspect = load_pending(out);
    let lock = Lock::new(out)?;
    let dirs = dirs_at(o.depth, root, "")?;
    let (groups, defer) = if clones { clone_groups(root, &dirs, &state)? } else { (Vec::new(), Defer::new()) };
    let run = Run { root, out, o, state: &state, suspect: &suspect, lock: &lock, defer: &defer };
    let mut total = Totals::default();
    let mut since = 0;
    if o.workers <= 1 {
        for rel in &dirs {
            run.one(rel, &mut total, &mut since)?;
        }
    } else {
        let count = o.workers.min(dirs.len().max(1));
        let results: Vec<Result<Totals>> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..count)
                .map(|k| {
                    let (run, dirs) = (&run, &dirs);
                    s.spawn(move || {
                        let (mut t, mut since) = (Totals::default(), 0);
                        // Worker k takes containers k, k+workers, ...
                        for rel in dirs.iter().skip(k).step_by(o.workers) {
                            run.one(rel, &mut t, &mut since)?;
                        }
                        Ok(t)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().expect("worker panicked")).collect()
        });
        let mut failed = false;
        for r in results {
            match r {
                Ok(t) => {
                    total.stored += t.stored;
                    total.retained += t.retained;
                    total.bytes += t.bytes;
                    total.opened += t.opened;
                    total.barriers += t.barriers;
                    total.scan_seconds += t.scan_seconds;
                    total.updates.extend(t.updates);
                }
                Err(e) => {
                    eprintln!("{}: {e}", prog());
                    failed = true;
                }
            }
        }
        if failed {
            std::process::exit(1);
        }
    }
    let mut kept = 0;
    if !groups.is_empty() {
        let (touched, (s, b, k)) = run.store_clones(&groups)?;
        total.stored += s;
        total.bytes += b;
        kept = k;
        for rel in touched {
            let target = format!("{}.sqlar", concat(out, &rel));
            let c = Container::open(target.as_ref(), &OpenOptions { create: false, read_only: true, engine: Some(o.engine), ..OpenOptions::default() })?;
            let ms = c.members();
            c.close()?;
            let s = n::stat(&target, true)?;
            // After the parallel pass's entries, so these, newer, win.
            total.updates.push((rel, (s.ino, s.size, s.mtime_ns), ms?));
        }
    }
    let mut since = 0;
    // The final barrier, then the state; only then are the pending containers
    // known to be on the platter.
    run.barrier(&mut total, &mut since)?;
    let updates = std::mem::take(&mut total.updates);
    let mut barriers = total.barriers;
    for (rel, st, members) in updates {
        state.insert(rel, (st, members));
    }
    save_state(out, &state)?;
    lock.with(|| container::barrier(&concat(out, ".packdir-writer.lock")))?;
    barriers += 1;
    clear_pending(out)?;
    let dt = begun.elapsed().as_secs_f64();
    let kept = if clones { format!(", \"clones\": {kept}") } else { String::new() };
    println!(
        "{{\"barriers\": {barriers}, \"containers\": {}, \"opened\": {}, \"stored\": {}, \"retained\": {}, \"bytes\": {}, \"seconds\": {dt:.3}, \"scan_seconds\": {:.3}, \"mib_per_s\": {:.1}, \"files_per_s\": {:.1}{kept}}}",
        dirs.len(), total.opened, total.stored, total.retained, total.bytes, total.scan_seconds,
        total.bytes as f64 / 1048576.0 / dt, total.stored as f64 / dt
    );
    Ok(())
}

fn containers_under(dir: &str, rel: &str) -> Result<Vec<String>> {
    let mut names = n::readdir(dir)?;
    names.sort();
    let mut out = Vec::new();
    for name in names {
        let r = if rel.is_empty() { name.clone() } else { concat(rel, &name) };
        let p = concat(dir, &name);
        match n::stat(&p, false)?.kind {
            Kind::Dir => out.extend(containers_under(&p, &r)?),
            Kind::Reg if name.ends_with(".sqlar") => out.push(r[..r.len() - ".sqlar".len()].to_string()),
            _ => {}
        }
    }
    Ok(out)
}

fn int(s: &str) -> Result<usize> {
    s.parse().map_err(|_| Error("int_of_string".into()))
}

fn run(args: &[String]) -> Result<()> {
    let flag = |f: &str| args.iter().any(|a| a == f);
    let value = |f: &str, d: &str| -> String {
        args.windows(2).find(|w| w[0] == f).map_or(d.to_string(), |w| w[1].clone())
    };
    let mut pos: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if matches!(a, "--depth" | "--batch-mib" | "--workers" | "--barrier-gib" | "--engine" | "--writers") {
            i += 2;
            continue;
        }
        if !(a.len() > 2 && a.starts_with("--")) {
            pos.push(a);
        }
        i += 1;
    }
    let engine = match value("--engine", "").as_str() {
        "" => default_engine()?,
        e => Engine::parse(e)?,
    };
    let reading = OpenOptions { create: false, read_only: true, engine: Some(engine), ..OpenOptions::default() };
    let open_read = |path: &str| Container::open(path.as_ref(), &reading);
    match pos.as_slice() {
        ["pack-tree", root, out] => {
            let mvcc = flag("--mvcc");
            pack_tree_with_clones(root, out, &PackOptions {
                depth: int(&value("--depth", "2"))?,
                journal: if mvcc { Journal::Mvcc } else if flag("--wal") { Journal::Wal } else { Journal::Delete },
                engine: if mvcc { Engine::Turso } else { engine },
                batch_bytes: int(&value("--batch-mib", "256"))? * 1048576,
                workers: int(&value("--workers", "1"))?,
                writers: int(&value("--writers", "1"))?,
                barrier_bytes: int(&value("--barrier-gib", "4"))? * 1073741824,
            }, flag("--clones"))
        }
        ["list", path] => {
            let c = open_read(path)?;
            let mut text = String::new();
            for m in c.members()? {
                text.push_str(&format!("{}\t{}\t{}\t{}{}\n", m.name, m.size, m.mtime_ns, m.sha256, if m.retained { "\tretained" } else { "" }));
            }
            print!("{text}");
            c.close()
        }
        ["verify", paths @ ..] if !paths.is_empty() => {
            let mut bad = 0;
            for p in paths {
                let c = open_read(p)?;
                let problems = c.verify()?;
                for e in &problems {
                    println!("{p}: {e}");
                }
                if problems.is_empty() {
                    println!("{p}: ok, {} members", c.members()?.len());
                }
                c.close()?;
                bad += problems.len();
            }
            if bad > 0 {
                std::process::exit(1);
            }
            Ok(())
        }
        ["unpack-tree", out, dest] => {
            // Every container under OUT back to plain files under DEST:
            // OUT/<rel>.sqlar becomes DEST/<rel>/, with exact bytes, modes and
            // nanosecond mtimes.
            let skip = flag("--skip-retained");
            let found = containers_under(out, "")?;
            let (mut files, mut restored, mut jobs) = (0, container::Restored::new(), Vec::new());
            // Every file that holds data first, then the clones, from the
            // originals just restored.
            for rel in &found {
                let c = open_read(&format!("{}.sqlar", concat(out, rel)))?;
                let r = c.restore_data(skip, &concat(dest, rel));
                c.close()?;
                let (written, j) = r?;
                files += written.len() + j.len();
                restored.extend(written);
                jobs.extend(j);
            }
            let (cloned, copied) = container::restore_clones(&restored, &jobs)?;
            if jobs.is_empty() {
                println!("{{\"containers\": {}, \"files\": {files}}}", found.len());
            } else {
                println!("{{\"containers\": {}, \"files\": {files}, \"cloned\": {cloned}, \"copied\": {copied}}}", found.len());
            }
            Ok(())
        }
        ["repack", paths @ ..] if !paths.is_empty() => {
            for p in paths {
                let (kept, before, after) = container::repack(p, flag("--drop-retained"), Journal::Delete, Some(engine))?;
                println!("{p}: {kept} members, {before} -> {after} bytes");
            }
            Ok(())
        }
        ["restore", path, out] => {
            let c = open_read(path)?;
            c.restore(flag("--skip-retained"), out)?;
            c.close()
        }
        _ => usage(),
    }
}

pub fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // GNU-style file commands over packed trees: packdir ls -l ROOT ...
    if let Some(cmd) = args.first()
        && let Some(f) = fileutils::command(cmd) {
            if let Err(e) = f(&args[1..]) {
                fileutils::report(&e);
            }
            std::process::exit(fileutils::status());
        }
    if let Err(e) = run(&args) {
        eprintln!("{}: {e}", prog());
        std::process::exit(1);
    }
}
