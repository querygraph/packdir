//! A packed directory: the files of one directory D stored as members of one
//! SQLite archive D.sqlar. The standard sqlar table is what stock tools read;
//! unison_member keeps what a synchronizer needs (nanosecond mtime, MD5
//! fingerprint, SHA-256 as hex text, retained deletions); unison_container the
//! format and an epoch bumped by every committed batch. Blobs are stored raw.
//! The table names are part of the on-disk format, shared with the OCaml
//! build and with Semantic Unison's packed roots.
use crate::native::{self as n, Kind, Stat};
use crate::{Error, Result};
use firstpair_tursolite::{self as t, Connection, Database, Engine, Journal, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

pub const FORMAT: &str = "unison-packdir/1";
/// A container holding clone references (packdir_clone, packdir_cloned_by):
/// builds that predate clones refuse it rather than miss members.
pub const FORMAT_CLONES: &str = "unison-packdir/2";
/// Unison's archive fingerprint: MD5 of the bytes.
pub const FPALG: &str = "unison-md5/1";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sqlar(name TEXT PRIMARY KEY, mode INT, mtime INT, sz INT, data BLOB);
CREATE TABLE IF NOT EXISTS unison_member(name TEXT PRIMARY KEY REFERENCES sqlar(name),
  mtime_ns INT NOT NULL, fp BLOB NOT NULL, fpalg TEXT NOT NULL, sha256 BLOB NOT NULL,
  retained_deleted INT NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS unison_container(key TEXT PRIMARY KEY, value TEXT);
CREATE TABLE IF NOT EXISTS packdir_clone(name TEXT PRIMARY KEY, mode INT, mtime INT, sz INT,
  original TEXT NOT NULL, sha256 BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS packdir_cloned_by(name TEXT NOT NULL, clone TEXT NOT NULL,
  PRIMARY KEY (name, clone));
";

const BUMP: &str = "UPDATE unison_container SET value = CAST(value AS INTEGER) + 1 WHERE key = 'epoch'";

#[derive(Clone, Debug, PartialEq)]
pub struct Member {
    pub name: String,
    pub size: i64,
    pub mtime_ns: i64,
    pub fp: Vec<u8>,
    pub fpalg: String,
    pub sha256: String,
    pub retained: bool,
}

/// The engine when a caller names none: sqlite, or PACKDIR_ENGINE.
pub fn default_engine() -> Result<Engine> {
    match std::env::var("PACKDIR_ENGINE") {
        Ok(e) => Ok(Engine::parse(&e)?),
        Err(_) => Ok(Engine::Sqlite),
    }
}

#[derive(Clone, Debug)]
pub struct OpenOptions {
    pub journal: Journal,
    pub create: bool,
    /// A promise not to write: Turso then leaves the file's bytes alone.
    pub read_only: bool,
    /// None: [default_engine].
    pub engine: Option<Engine>,
}

impl Default for OpenOptions {
    fn default() -> OpenOptions {
        OpenOptions { journal: Journal::Delete, create: true, read_only: false, engine: None }
    }
}

pub struct Container {
    db: Option<Database>,
    conn: Option<Connection>,
    path: String,
    journal: Journal,
    /// The directory the container stands for (its path without .sqlar),
    /// which clone references are relative to.
    dir: String,
    /// It has the clone tables.
    clones: AtomicBool,
}

/// A member to store: its name, and the plain file holding its bytes.
#[derive(Clone, Debug, PartialEq)]
pub struct Put {
    pub member: String,
    pub source: String,
}

/// Takes turns on one disk; holds only the transactions.
pub type WriteLock<'a> = Option<&'a (dyn Fn(&mut dyn FnMut() -> Result<()>) -> Result<()> + Sync)>;

/// Limits on [Container::apply].
#[derive(Clone, Copy)]
pub struct Apply<'a> {
    pub batch_bytes: usize,
    /// Takes turns on one disk; holds only the transactions.
    pub write_lock: WriteLock<'a>,
    pub writers: usize,
}

impl Default for Apply<'_> {
    fn default() -> Self {
        Apply { batch_bytes: 256 * 1048576, write_lock: None, writers: 1 }
    }
}

fn locked(lock: WriteLock<'_>, f: &mut dyn FnMut() -> Result<()>) -> Result<()> {
    match lock {
        Some(l) => l(f),
        None => f(),
    }
}

struct Ready {
    put: Put,
    st: Stat,
    data: Vec<u8>,
    md5: Vec<u8>,
    sha: String,
}

/// Tests: exit inside a transaction.
fn crash_after() -> Option<usize> {
    std::env::var("PACKDIR_CRASH_AFTER").ok().and_then(|n| n.parse().ok())
}

fn read_put(p: &Put) -> Result<Ready> {
    if p.member.contains('/') {
        return Err(Error(format!("member name with a slash: {}", p.member)));
    }
    let st = n::stat(&p.source, true)?;
    if st.kind != Kind::Reg {
        return Err(Error(format!("{}: not a regular file", p.source)));
    }
    let data = fs::read(&p.source).map_err(|e| Error(format!("{}: {e}", p.source)))?;
    let again = n::stat(&p.source, true)?;
    // Reading moves atime; identity, size, mtime and ctime must hold.
    if data.len() as i64 != st.size || again.ino != st.ino || again.size != st.size
        || again.mtime_ns != st.mtime_ns || again.ctime_ns != st.ctime_ns
    {
        return Err(Error(format!("{}: changed while read", p.source)));
    }
    let md5 = n::md5_bytes(&data);
    let sha = n::sha_bytes(&data);
    Ok(Ready { put: p.clone(), st, data, md5, sha })
}

fn store(c: &Connection, r: &Ready) -> t::Result<()> {
    c.run("DELETE FROM packdir_clone WHERE name = ?", &[Value::text(&r.put.member)])?;
    c.run(
        "INSERT OR REPLACE INTO sqlar(name, mode, mtime, sz, data) VALUES (?, ?, ?, ?, ?)",
        &[
            Value::text(&r.put.member),
            Value::Integer((0o100000 | r.st.mode) as i64),
            Value::Integer(r.st.mtime_ns / 1_000_000_000),
            Value::Integer(r.st.size),
            Value::Blob(r.data.clone()),
        ],
    )?;
    c.run(
        "INSERT OR REPLACE INTO unison_member(name, mtime_ns, fp, fpalg, sha256, retained_deleted) VALUES (?, ?, ?, ?, ?, 0)",
        &[
            Value::text(&r.put.member),
            Value::Integer(r.st.mtime_ns),
            Value::Blob(r.md5.clone()),
            Value::text(FPALG),
            Value::Blob(r.sha.as_bytes().to_vec()),
        ],
    )
}

fn mark_retained(c: &Connection, names: &[String]) -> t::Result<()> {
    for name in names {
        c.run("UPDATE unison_member SET retained_deleted = 1 WHERE name = ?", &[Value::text(name)])?;
    }
    Ok(())
}

fn mark_clones(c: &Connection) -> t::Result<()> {
    c.run("UPDATE unison_container SET value = ? WHERE key = 'format'", &[Value::text(FORMAT_CLONES)])
}

fn batches(puts: &[Put], limit: usize) -> Result<Vec<Vec<Put>>> {
    let mut groups = Vec::new();
    let (mut cur, mut size) = (Vec::new(), 0);
    for p in puts {
        let len = n::stat(&p.source, true)?.size as usize;
        if !cur.is_empty() && size + len > limit {
            groups.push(std::mem::take(&mut cur));
            size = 0;
        }
        size += len;
        cur.push(p.clone());
    }
    if !cur.is_empty() {
        groups.push(cur);
    }
    Ok(groups)
}

/// One drive-cache flush for everything written to [path]'s disk so far:
/// F_FULLFSYNC empties the whole drive's write cache, not just one file's.
pub fn barrier(path: &str) -> Result<()> {
    n::fullfsync(&fs::File::open(path).map_err(|e| Error(format!("{path}: {e}")))?)
}

// Clones (docs/clones.md). A member that was an APFS clone of another is kept
// once: a packdir_clone row with its own mode, mtime and size, the path of its
// original relative to this container's directory, and the SHA-256 both
// share, and no sqlar row, so stock sqlite3 -Ax extracts every other member
// and skips it. The original's container lists its clones in
// packdir_cloned_by, and whatever changes or removes an original first gives
// those clones bytes of their own.

/// Lexical normalization of a path; absolute when it starts with /.
pub fn normalize(path: &str) -> String {
    let abs = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for p in path.split('/') {
        match p {
            "" | "." => {}
            ".." if parts.last().is_some_and(|x| *x != "..") => {
                parts.pop();
            }
            _ => parts.push(p),
        }
    }
    let s = parts.join("/");
    if abs {
        format!("/{s}")
    } else if s.is_empty() {
        ".".into()
    } else {
        s
    }
}

pub fn absolute(p: &str) -> String {
    if p.starts_with('/') {
        return normalize(p);
    }
    let cwd = std::env::current_dir().map(|d| d.to_string_lossy().into_owned()).unwrap_or_else(|_| ".".into());
    normalize(&n::concat(&cwd, p))
}

/// The path from directory `from` to `target`.
pub fn relative(from: &str, target: &str) -> String {
    let split = |p: &str| absolute(p).split('/').filter(|x| !x.is_empty()).map(String::from).collect::<Vec<_>>();
    let (a, b) = (split(from), split(target));
    let common = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let mut parts: Vec<String> = a[common..].iter().map(|_| "..".to_string()).collect();
    parts.extend(b[common..].iter().cloned());
    parts.join("/")
}

pub fn same_file(a: &str, b: &str) -> bool {
    absolute(a) == absolute(b)
}

/// The container path and member name a reference relative to `dir` names:
/// member names have no slash, so the container is always the parent's.
pub fn locate(dir: &str, rel: &str) -> (String, String) {
    let p = normalize(&n::concat(dir, rel));
    (format!("{}.sqlar", n::dirname(&p)), n::basename(&p))
}

/// `f` on the container at `path`: `c` itself when it is that file.
fn with_at<T>(c: &Container, path: &str, read_only: bool, f: impl FnOnce(&Container) -> Result<T>) -> Result<T> {
    if same_file(path, &c.path) {
        return f(c);
    }
    let k = Container::open(path.as_ref(), &OpenOptions { create: false, read_only, ..OpenOptions::default() })?;
    let r = f(&k);
    let closed = k.close();
    let v = r?;
    closed?;
    Ok(v)
}

/// A member to keep as a clone: the plain file `file`, an APFS clone of the
/// member `original` (relative to the container's directory).
#[derive(Clone, Debug, PartialEq)]
pub struct ClonePut {
    pub clone: String,
    pub file: String,
    pub original: String,
}

/// A clone to restore once the originals are on disk.
#[derive(Clone, Debug, PartialEq)]
pub struct CloneJob {
    pub target: String,
    /// The original's container (absolute) and member.
    pub source: (String, String),
    pub sha256: String,
    pub mode: u32,
    pub mtime_ns: i64,
}

/// What [Container::restore_data] wrote, as (container (absolute), member) ->
/// file: where [restore_clones] clones from.
pub type Restored = HashMap<(String, String), String>;

impl Container {
    /// Opens D.sqlar, creating it with its page size and vacuum mode. Turso has
    /// no rollback journal and cannot write auto-vacuum: what it writes has
    /// neither until it is sealed on close, and a container SQLite made with
    /// auto-vacuum stays SQLite's until [repack] with Turso rebuilds it.
    pub fn open(path: &Path, o: &OpenOptions) -> Result<Container> {
        Container::open_as(path, o, None)
    }

    /// [open], with clone references resolved as if the file were at
    /// `logical` (repack's fresh file stands for the old one).
    fn open_as(path: &Path, o: &OpenOptions, logical: Option<&str>) -> Result<Container> {
        let name = path.to_string_lossy().into_owned();
        let want = if o.journal == Journal::Mvcc {
            Engine::Turso
        } else {
            match o.engine {
                Some(e) => e,
                None => default_engine()?,
            }
        };
        let fresh = !path.exists();
        if fresh && !o.create {
            return Err(Error(format!("{name}: no such container")));
        }
        let resolved = t::resolve(path, want, o.read_only)?;
        // SQLite stays read-write: a hot journal needs rolling back; so does an
        // MVCC file Turso must recover.
        let unsealed = t::inspect(path)?.is_some_and(|h| h.mvcc());
        let read_only = o.read_only && resolved == Engine::Turso && !unsealed && !fresh;
        let db = Database::open(
            path,
            &t::Options::new()
                .engine(resolved)
                .journal(o.journal)
                .page_size(65536)
                .auto_vacuum(fresh && resolved == Engine::Sqlite)
                .read_only(read_only)
                .create(o.create),
        )?;
        let c = db.connect()?;
        if !read_only {
            // A rollback journal deleted at each commit; each transaction is
            // synced with plain fsync, and the drive's own cache is flushed by
            // [barrier] every few GiB. PACKDIR_SYNC, for benchmarks only:
            // "fullfsync" flushes the drive cache on every commit, "normal"
            // syncs less often.
            let sync = std::env::var("PACKDIR_SYNC").unwrap_or_default();
            let fullfsync = sync == "fullfsync";
            c.exec(if sync == "normal" { "PRAGMA synchronous = NORMAL" } else { "PRAGMA synchronous = FULL" })?;
            if resolved == Engine::Sqlite {
                c.exec(if fullfsync { "PRAGMA fullfsync = ON" } else { "PRAGMA fullfsync = OFF" })?;
                c.exec(if fullfsync { "PRAGMA checkpoint_fullfsync = ON" } else { "PRAGMA checkpoint_fullfsync = OFF" })?;
            }
            c.exec(SCHEMA.trim())?;
            let rows = c.query("SELECT value FROM unison_container WHERE key='format'", &[])?;
            match rows.as_slice() {
                [] => c.run("INSERT INTO unison_container VALUES ('format', ?), ('epoch', '0')", &[Value::text(FORMAT)])?,
                [r] if r.len() == 1 && (r[0] == Value::text(FORMAT) || r[0] == Value::text(FORMAT_CLONES)) => {}
                _ => {
                    drop(c);
                    db.close()?;
                    return Err(Error(format!("{name}: not a {FORMAT} container")));
                }
            }
        }
        let clones = !c.query("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'packdir_clone'", &[])?.is_empty();
        let logical = logical.unwrap_or(&name);
        let dir = logical.strip_suffix(".sqlar").unwrap_or(logical).to_string();
        Ok(Container { db: Some(db), conn: Some(c), path: name, journal: o.journal, dir, clones: AtomicBool::new(clones) })
    }

    fn c(&self) -> &Connection {
        self.conn.as_ref().expect("open container")
    }

    fn database(&self) -> &Database {
        self.db.as_ref().expect("open container")
    }

    fn has_clones(&self) -> bool {
        self.clones.load(Ordering::SeqCst)
    }

    /// Closes it. What Turso wrote is sealed, and only here: a container
    /// dropped without closing is left as after a crash, for the next open.
    pub fn close(mut self) -> Result<()> {
        if self.journal == Journal::Wal && self.database().engine() == Engine::Sqlite {
            self.c().exec("PRAGMA wal_checkpoint(TRUNCATE)")?;
        }
        self.conn = None;
        self.db.take().expect("open container").close()?;
        Ok(())
    }

    pub fn epoch(&self) -> Result<i64> {
        let rows = self.c().query("SELECT value FROM unison_container WHERE key='epoch'", &[])?;
        Ok(rows.first().and_then(|r| r[0].as_str().ok()).and_then(|s| s.parse().ok()).unwrap_or(0))
    }

    /// The member table: indexed queries, no member data read. Clones are
    /// members like any other.
    pub fn members(&self) -> Result<Vec<Member>> {
        let rows = |table: &str| -> Result<Vec<Member>> {
            let rows = self.c().query(
                &format!(
                    "SELECT m.name, s.sz, m.mtime_ns, m.fp, m.fpalg, m.sha256, m.retained_deleted \
                     FROM unison_member m JOIN {table} s ON s.name = m.name ORDER BY m.name"
                ),
                &[],
            )?;
            rows.iter()
                .map(|r| {
                    Ok(Member {
                        name: r[0].as_str()?,
                        size: r[1].as_i64()?,
                        mtime_ns: r[2].as_i64()?,
                        fp: r[3].as_bytes()?.to_vec(),
                        fpalg: r[4].as_str()?,
                        sha256: r[5].as_str()?,
                        retained: r[6].as_i64()? != 0,
                    })
                })
                .collect()
        };
        let mut all = rows("sqlar")?;
        if self.has_clones() {
            all.extend(rows("packdir_clone")?);
            // Both are sorted; a stable sort merges them.
            all.sort_by(|a, b| a.name.cmp(&b.name));
        }
        Ok(all)
    }

    /// Clone members: (name, original relative to the container's directory,
    /// shared SHA-256).
    pub fn clones(&self) -> Result<Vec<(String, String, String)>> {
        if !self.has_clones() {
            return Ok(Vec::new());
        }
        let rows = self.c().query("SELECT name, original, sha256 FROM packdir_clone ORDER BY name", &[])?;
        rows.iter().map(|r| Ok((r[0].as_str()?, r[1].as_str()?, r[2].as_str()?))).collect()
    }

    /// The original (relative to the container's directory) and SHA-256 of a
    /// clone member; None for any other name.
    pub fn clone_of(&self, name: &str) -> Result<Option<(String, String)>> {
        if !self.has_clones() {
            return Ok(None);
        }
        let rows = self.c().query("SELECT original, sha256 FROM packdir_clone WHERE name = ?", &[Value::text(name)])?;
        match rows.as_slice() {
            [r] => Ok(Some((r[0].as_str()?, r[1].as_str()?))),
            _ => Ok(None),
        }
    }

    fn data_of(&self, name: &str) -> Result<Option<Vec<u8>>> {
        let rows = self.c().query("SELECT data FROM sqlar WHERE name = ?", &[Value::text(name)])?;
        match rows.as_slice() {
            [r] => Ok(Some(r[0].as_bytes()?.to_vec())),
            _ => Ok(None),
        }
    }

    /// The bytes of a member that holds data.
    pub fn read_data(&self, name: &str) -> Result<Vec<u8>> {
        self.data_of(name)?.ok_or_else(|| Error(format!("{name}: no such member")))
    }

    /// A member's bytes; a clone's are its original's, SHA-256 checked.
    pub fn read(&self, name: &str) -> Result<Vec<u8>> {
        if let Some(d) = self.data_of(name)? {
            return Ok(d);
        }
        let Some((original, sha)) = self.clone_of(name)? else {
            return Err(Error(format!("{name}: no such member")));
        };
        let (path, member) = locate(&self.dir, &original);
        if !n::exists(&path) {
            return Err(Error(format!("{name}: clone of {original}, whose container is missing")));
        }
        let d = with_at(self, &path, true, |k| {
            k.data_of(&member)?.ok_or_else(|| Error(format!("{name}: clone of {original}, which is missing")))
        })?;
        if n::sha_bytes(&d) != sha {
            return Err(Error(format!("{name}: clone of {original}, which has changed")));
        }
        Ok(d)
    }

    /// The batches on [writers] threads, each with its own connection. Under
    /// MVCC the data transactions run concurrently; the epoch is one row they
    /// would all fight over, so each batch bumps it in a short transaction of
    /// its own, after its data, and a crash between the two leaves the data
    /// with the epoch behind. The file's stamp and the pending list, not the
    /// epoch, decide what is checked again. The retained marks come last.
    fn apply_concurrent(&self, writers: usize, groups: Vec<Vec<Put>>, retain: &[String]) -> Result<usize> {
        let queue = Mutex::new(groups.into_iter());
        let failure: Mutex<Option<Error>> = Mutex::new(None);
        let stored = AtomicUsize::new(0);
        let written = AtomicUsize::new(0);
        let crash = crash_after();
        let db = self.database();
        std::thread::scope(|s| {
            for _ in 0..writers {
                s.spawn(|| {
                    let work = || -> Result<()> {
                        let w = db.connect()?;
                        loop {
                            if failure.lock().unwrap().is_some() {
                                return Ok(());
                            }
                            let group = queue.lock().unwrap().next();
                            let Some(group) = group else { return Ok(()) };
                            let ready = group.iter().map(read_put).collect::<Result<Vec<_>>>()?;
                            w.transaction(|| {
                                for r in &ready {
                                    store(&w, r)?;
                                    if crash == Some(written.fetch_add(1, Ordering::SeqCst) + 1) {
                                        // SAFETY: ends the process; the crash under test.
                                        unsafe { libc::_exit(3) };
                                    }
                                }
                                Ok(())
                            })?;
                            w.transaction(|| w.run(BUMP, &[]))?;
                            stored.fetch_add(ready.iter().map(|r| r.data.len()).sum(), Ordering::SeqCst);
                        }
                    };
                    if let Err(e) = work() {
                        failure.lock().unwrap().get_or_insert(e);
                    }
                });
            }
        });
        if let Some(e) = failure.into_inner().unwrap() {
            return Err(e);
        }
        if !retain.is_empty() {
            let c = self.c();
            c.transaction(|| {
                mark_retained(c, retain)?;
                c.run(BUMP, &[])
            })?;
        }
        Ok(stored.into_inner())
    }

    /// One batch: every put and retained deletion in transactions of at most
    /// `batch_bytes` of member data, the epoch bumped in each; then the
    /// changed blobs are read back and their SHA-256 compared, before the
    /// caller may record them anywhere. Files are read and hashed outside
    /// `write_lock`. With MVCC and `writers` above one the batches of this
    /// container are written concurrently, each at most an equal share of its
    /// bytes. Members about to change first give their clones bytes of their
    /// own. Returns the bytes stored.
    pub fn apply(&self, a: Apply<'_>, puts: &[Put], retain: &[String]) -> Result<usize> {
        self.materialize(a.write_lock, &puts.iter().map(|p| p.member.clone()).collect::<Vec<_>>())?;
        let concurrent = a.writers > 1 && self.database().mvcc() && puts.len() > 1;
        let limit = if concurrent {
            let mut total = 0usize;
            for p in puts {
                total += n::stat(&p.source, true)?.size as usize;
            }
            a.batch_bytes.min(total.div_ceil(a.writers).max(1))
        } else {
            a.batch_bytes
        };
        let mut groups = batches(puts, limit)?;
        if groups.is_empty() {
            groups.push(Vec::new());
        }
        let stored = if concurrent {
            self.apply_concurrent(a.writers, groups, retain)?
        } else {
            let c = self.c();
            let crash = crash_after();
            let (mut stored, mut done, last) = (0, 0usize, groups.len() - 1);
            for (i, group) in groups.iter().enumerate() {
                let ready = group.iter().map(read_put).collect::<Result<Vec<_>>>()?;
                let before = done;
                let mut commit = || -> Result<()> {
                    c.transaction(|| {
                        done = before;
                        for r in &ready {
                            store(c, r)?;
                            done += 1;
                            if crash == Some(done) {
                                // SAFETY: ends the process; the crash under test.
                                unsafe { libc::_exit(3) };
                            }
                        }
                        if i == last {
                            mark_retained(c, retain)?;
                        }
                        c.run(BUMP, &[])
                    })?;
                    Ok(())
                };
                locked(a.write_lock, &mut commit)?;
                stored += ready.iter().map(|r| r.data.len()).sum::<usize>();
            }
            stored
        };
        // Read back what this batch wrote.
        for p in puts {
            let rows = self.c().query(
                "SELECT s.data, m.sha256 FROM sqlar s JOIN unison_member m ON m.name = s.name WHERE s.name = ?",
                &[Value::text(&p.member)],
            )?;
            let good = match rows.as_slice() {
                [r] => n::sha_bytes(r[0].as_bytes()?).as_bytes() == r[1].as_bytes()?,
                _ => false,
            };
            if !good {
                return Err(Error(format!("{}: {} did not read back", self.path, p.member)));
            }
        }
        Ok(stored)
    }

    /// Before `names` change or go: each clone that references one of them
    /// gets that original's current bytes as its own sqlar row, in its own
    /// container's transaction, and the back-references are dropped. A
    /// back-reference whose clone no longer names this original is stale and
    /// simply dropped.
    pub fn materialize(&self, write_lock: WriteLock<'_>, names: &[String]) -> Result<()> {
        if !self.has_clones() {
            return Ok(());
        }
        for name in names {
            let refs = self.c().query("SELECT clone FROM packdir_cloned_by WHERE name = ?", &[Value::text(name)])?;
            if refs.is_empty() {
                continue;
            }
            let mut data: Option<Vec<u8>> = None;
            for r in &refs {
                let (path, member) = locate(&self.dir, &r[0].as_str()?);
                if !n::exists(&path) {
                    continue;
                }
                with_at(self, &path, false, |k| {
                    let rows = k.c().query("SELECT mode, mtime, sz, original FROM packdir_clone WHERE name = ?", &[Value::text(&member)])?;
                    let [row] = rows.as_slice() else { return Ok(()) };
                    let (opath, oname) = locate(&k.dir, &row[3].as_str()?);
                    if !(same_file(&opath, &self.path) && oname == *name) {
                        return Ok(());
                    }
                    if data.is_none() {
                        data = Some(self.read_data(name)?);
                    }
                    let d = data.as_ref().expect("read above");
                    locked(write_lock, &mut || {
                        let w = k.c();
                        w.transaction(|| {
                            w.run(
                                "INSERT OR REPLACE INTO sqlar(name, mode, mtime, sz, data) VALUES (?, ?, ?, ?, ?)",
                                &[Value::text(&member), row[0].clone(), row[1].clone(), row[2].clone(), Value::Blob(d.clone())],
                            )?;
                            w.run("DELETE FROM packdir_clone WHERE name = ?", &[Value::text(&member)])?;
                            w.run(BUMP, &[])
                        })?;
                        Ok(())
                    })
                })?;
            }
            locked(write_lock, &mut || {
                let c = self.c();
                c.transaction(|| c.run("DELETE FROM packdir_cloned_by WHERE name = ?", &[Value::text(name)]))?;
                Ok(())
            })?;
        }
        Ok(())
    }

    /// Store `clones` as references. Each file is read and hashed; its
    /// original must be a member holding data with the same SHA-256. The
    /// original's container gets its back-reference first, in a transaction
    /// of its own; then this container gets every clone row in one. No
    /// transaction waits for another container. Returns the bytes not stored.
    pub fn apply_clones(&self, a: Apply<'_>, clones: &[ClonePut]) -> Result<usize> {
        if clones.is_empty() {
            return Ok(0);
        }
        let mut ready = Vec::new();
        for cp in clones {
            let r = read_put(&Put { member: cp.clone.clone(), source: cp.file.clone() })?;
            let (path, oname) = locate(&self.dir, &cp.original);
            if same_file(&path, &self.path) && oname == cp.clone {
                return Err(Error(format!("{}: a clone of itself", cp.clone)));
            }
            if !n::exists(&path) {
                return Err(Error(format!("{}: original {} has no container", cp.clone, cp.original)));
            }
            with_at(self, &path, false, |k| {
                let rows = k.c().query(
                    "SELECT m.sha256 FROM unison_member m JOIN sqlar s ON s.name = m.name WHERE m.name = ?",
                    &[Value::text(&oname)],
                )?;
                match rows.as_slice() {
                    [h] if h[0].as_bytes()? == r.sha.as_bytes() => {}
                    [_] => return Err(Error(format!("{}: differs from its original {}", cp.clone, cp.original))),
                    _ => return Err(Error(format!("{}: original {} holds no data", cp.clone, cp.original))),
                }
                let back = relative(&k.dir, &n::concat(&self.dir, &cp.clone));
                locked(a.write_lock, &mut || {
                    let w = k.c();
                    w.transaction(|| {
                        w.run("INSERT OR IGNORE INTO packdir_cloned_by(name, clone) VALUES (?, ?)", &[Value::text(&oname), Value::text(&back)])?;
                        mark_clones(w)
                    })?;
                    Ok(())
                })?;
                k.clones.store(true, Ordering::SeqCst);
                Ok(())
            })?;
            ready.push((r, cp));
        }
        // A member becoming a clone may itself be an original.
        self.materialize(a.write_lock, &clones.iter().map(|cp| cp.clone.clone()).collect::<Vec<_>>())?;
        locked(a.write_lock, &mut || {
            let c = self.c();
            c.transaction(|| {
                for (r, cp) in &ready {
                    c.run("DELETE FROM sqlar WHERE name = ?", &[Value::text(&cp.clone)])?;
                    c.run(
                        "INSERT OR REPLACE INTO packdir_clone(name, mode, mtime, sz, original, sha256) VALUES (?, ?, ?, ?, ?, ?)",
                        &[
                            Value::text(&cp.clone),
                            Value::Integer((0o100000 | r.st.mode) as i64),
                            Value::Integer(r.st.mtime_ns / 1_000_000_000),
                            Value::Integer(r.st.size),
                            Value::text(&cp.original),
                            Value::Blob(r.sha.as_bytes().to_vec()),
                        ],
                    )?;
                    c.run(
                        "INSERT OR REPLACE INTO unison_member(name, mtime_ns, fp, fpalg, sha256, retained_deleted) VALUES (?, ?, ?, ?, ?, 0)",
                        &[
                            Value::text(&cp.clone),
                            Value::Integer(r.st.mtime_ns),
                            Value::Blob(r.md5.clone()),
                            Value::text(FPALG),
                            Value::Blob(r.sha.as_bytes().to_vec()),
                        ],
                    )?;
                }
                mark_clones(c)?;
                c.run(BUMP, &[])
            })?;
            Ok(())
        })?;
        self.clones.store(true, Ordering::SeqCst);
        Ok(ready.iter().map(|(r, _)| r.data.len()).sum())
    }

    /// Problems found: the structure check, then every member's SHA-256 and
    /// the pairing of the two tables, then every clone against its original.
    pub fn verify(&self) -> Result<Vec<String>> {
        let mut problems = Vec::new();
        let rows = self.c().query("PRAGMA integrity_check", &[])?;
        if !(rows.len() == 1 && rows[0].len() == 1 && rows[0][0] == Value::text("ok")) {
            for r in &rows {
                problems.push(format!("integrity: {}", r[0].as_str()?));
            }
        }
        let rows = self.c().query("SELECT s.name, m.sha256, s.data, m.fp FROM sqlar s LEFT JOIN unison_member m ON m.name = s.name", &[])?;
        for r in &rows {
            let name = r[0].as_str()?;
            if r[1] == Value::Null {
                problems.push(format!("{name}: no unison_member row"));
                continue;
            }
            let d = r[2].as_bytes()?;
            if n::sha_bytes(d).as_bytes() != r[1].as_bytes()? {
                problems.push(format!("{name}: SHA-256 mismatch"));
            } else if n::md5_bytes(d) != r[3].as_bytes()? {
                problems.push(format!("{name}: fingerprint mismatch"));
            }
        }
        let without_data = if self.has_clones() {
            "SELECT m.name FROM unison_member m LEFT JOIN sqlar s ON s.name = m.name \
             WHERE s.name IS NULL AND m.name NOT IN (SELECT name FROM packdir_clone)"
        } else {
            "SELECT m.name FROM unison_member m LEFT JOIN sqlar s ON s.name = m.name WHERE s.name IS NULL"
        };
        for r in &self.c().query(without_data, &[])? {
            problems.push(format!("{}: unison_member row without data", r[0].as_str()?));
        }
        // Each clone: its member row, no data of its own, and an original that
        // holds the same bytes.
        for (name, original, sha) in self.clones()? {
            if !self.c().query("SELECT 1 FROM sqlar WHERE name = ?", &[Value::text(&name)])?.is_empty() {
                problems.push(format!("{name}: both a clone and data"));
            }
            let rows = self.c().query("SELECT sha256, fp FROM unison_member WHERE name = ?", &[Value::text(&name)])?;
            let [r] = rows.as_slice() else {
                problems.push(format!("{name}: no unison_member row"));
                continue;
            };
            if r[0].as_str()? != sha {
                problems.push(format!("{name}: clone SHA-256 differs from its member row"));
            }
            match self.read(&name) {
                Ok(d) => {
                    if n::md5_bytes(&d) != r[1].as_bytes()? {
                        problems.push(format!("{name}: fingerprint mismatch"));
                    }
                }
                Err(Error(m)) if m == format!("{name}: no such member") => {
                    problems.push(format!("{name}: clone of {original}, which is missing"));
                }
                Err(Error(m)) => problems.push(m),
            }
        }
        Ok(problems)
    }

    /// Bring D.sqlar up to date with [dir]. Returns (stored, retained, bytes).
    pub fn update(&self, a: Apply<'_>, skip: &[String], dir: &str) -> Result<(usize, usize, usize)> {
        self.update_deferred(a, skip, &[], dir)
    }

    /// [update], leaving out the new or changed files named in `defer`
    /// (clones, stored after the originals).
    pub fn update_deferred(&self, a: Apply<'_>, skip: &[String], defer: &[String], dir: &str) -> Result<(usize, usize, usize)> {
        let (puts, retain) = plan_deferred(skip, defer, &self.members()?, dir)?;
        let bytes = if puts.is_empty() && retain.is_empty() { 0 } else { self.apply(a, &puts, &retain)? };
        Ok((puts.len(), retain.len(), bytes))
    }

    /// Writes every member that holds data to [out] with its mode and
    /// nanosecond mtime, SHA-256 checked; retained deletions too unless
    /// `skip_retained`. Returns what was written, as (container, member) ->
    /// file, and the clones still to restore.
    pub fn restore_data(&self, skip_retained: bool, out: &str) -> Result<(Restored, Vec<CloneJob>)> {
        fs::create_dir_all(out).map_err(|e| Error(format!("{out}: {e}")))?;
        let modes = self.modes()?;
        let (mut written, mut jobs) = (Restored::new(), Vec::new());
        for m in self.members()? {
            if skip_retained && m.retained {
                continue;
            }
            let target = n::concat(out, &m.name);
            if n::exists(&target) {
                return Err(Error(format!("{target} exists")));
            }
            let mode = modes.get(&m.name).copied().unwrap_or(0o100644) & 0o7777;
            match self.clone_of(&m.name)? {
                Some((original, _)) => {
                    let (path, member) = locate(&self.dir, &original);
                    jobs.push(CloneJob { target, source: (absolute(&path), member), sha256: m.sha256, mode, mtime_ns: m.mtime_ns });
                }
                None => {
                    let d = self.read_data(&m.name)?;
                    if n::sha_bytes(&d) != m.sha256 {
                        return Err(Error(format!("{}: SHA-256 mismatch", m.name)));
                    }
                    fs::write(&target, &d).map_err(|e| Error(format!("{target}: {e}")))?;
                    n::chmod(&target, mode)?;
                    n::utimens(&target, m.mtime_ns, m.mtime_ns)?;
                    written.insert((absolute(&self.path), m.name), target);
                }
            }
        }
        Ok((written, jobs))
    }

    /// One container to [out]: data members, then its clones; retained
    /// deletions too unless `skip_retained`.
    pub fn restore(&self, skip_retained: bool, out: &str) -> Result<()> {
        self.restore_counted(skip_retained, out).map(|_| ())
    }

    /// [restore], returning (cloned, copied).
    pub fn restore_counted(&self, skip_retained: bool, out: &str) -> Result<(usize, usize)> {
        let (written, jobs) = self.restore_data(skip_retained, out)?;
        restore_clones(&written, &jobs)
    }

    /// Name -> mode of every member, from the sqlar table (for ls -l and stat).
    pub fn modes(&self) -> Result<HashMap<String, u32>> {
        let mut t = HashMap::new();
        let sql = if self.has_clones() {
            "SELECT name, mode FROM sqlar UNION ALL SELECT name, mode FROM packdir_clone"
        } else {
            "SELECT name, mode FROM sqlar"
        };
        for r in self.c().query(sql, &[])? {
            if let (Value::Text(_), Value::Integer(m)) = (&r[0], &r[1]) {
                t.insert(r[0].as_str()?, *m as u32);
            }
        }
        Ok(t)
    }

    /// One transaction that changes the member set, bumping the epoch so that
    /// Unison sees the container changed outside it.
    fn change(&self, mut f: impl FnMut() -> t::Result<()>) -> Result<()> {
        let c = self.c();
        c.transaction(|| {
            f()?;
            c.run(BUMP, &[])
        })?;
        Ok(())
    }

    /// Delete members, bytes and all (rm). Originals first give their clones
    /// bytes of their own.
    pub fn remove(&self, names: &[String]) -> Result<()> {
        self.materialize(None, names)?;
        let c = self.c();
        let clones = self.has_clones();
        self.change(|| {
            for name in names {
                c.run("DELETE FROM unison_member WHERE name = ?", &[Value::text(name)])?;
                if clones {
                    c.run("DELETE FROM packdir_clone WHERE name = ?", &[Value::text(name)])?;
                }
                c.run("DELETE FROM sqlar WHERE name = ?", &[Value::text(name)])?;
            }
            Ok(())
        })
    }

    /// Rename a member within its container (mv); refuses an existing target.
    /// A renamed clone moves its back-reference; a renamed original moves its
    /// clones' references, each in its clone's container after this one.
    pub fn rename(&self, src: &str, dst: &str) -> Result<()> {
        if dst.contains('/') {
            return Err(Error(format!("member name with a slash: {dst}")));
        }
        let clone = self.clone_of(src)?;
        let clones = self.has_clones();
        let refs: Vec<String> = if clones {
            self.c()
                .query("SELECT clone FROM packdir_cloned_by WHERE name = ?", &[Value::text(src)])?
                .iter()
                .map(|r| r[0].as_str())
                .collect::<t::Result<_>>()?
        } else {
            Vec::new()
        };
        let c = self.c();
        self.change(|| {
            let exists = self.members().map_err(|e| t::Error::new(t::ErrorKind::Other, e.0))?.iter().any(|m| m.name == dst);
            if exists {
                return Err(t::Error::new(t::ErrorKind::Other, format!("{dst} exists")));
            }
            c.run("UPDATE sqlar SET name = ? WHERE name = ?", &[Value::text(dst), Value::text(src)])?;
            c.run("UPDATE unison_member SET name = ? WHERE name = ?", &[Value::text(dst), Value::text(src)])?;
            if clones {
                c.run("UPDATE packdir_clone SET name = ? WHERE name = ?", &[Value::text(dst), Value::text(src)])?;
                c.run("UPDATE packdir_cloned_by SET name = ? WHERE name = ?", &[Value::text(dst), Value::text(src)])?;
            }
            Ok(())
        })?;
        if let Some((original, _)) = clone {
            let (path, oname) = locate(&self.dir, &original);
            if n::exists(&path) {
                with_at(self, &path, false, |k| {
                    let back = |name: &str| relative(&k.dir, &n::concat(&self.dir, name));
                    let w = k.c();
                    w.transaction(|| {
                        w.run("DELETE FROM packdir_cloned_by WHERE name = ? AND clone = ?", &[Value::text(&oname), Value::text(&back(src))])?;
                        w.run("INSERT OR IGNORE INTO packdir_cloned_by(name, clone) VALUES (?, ?)", &[Value::text(&oname), Value::text(&back(dst))])
                    })?;
                    Ok(())
                })?;
            }
        }
        for r in &refs {
            let (path, member) = locate(&self.dir, r);
            if !n::exists(&path) {
                continue;
            }
            with_at(self, &path, false, |k| {
                let rows = k.c().query("SELECT original FROM packdir_clone WHERE name = ?", &[Value::text(&member)])?;
                let [row] = rows.as_slice() else { return Ok(()) };
                let (p, name) = locate(&k.dir, &row[0].as_str()?);
                if same_file(&p, &self.path) && name == src {
                    let w = k.c();
                    w.transaction(|| {
                        w.run(
                            "UPDATE packdir_clone SET original = ? WHERE name = ?",
                            &[Value::text(&relative(&k.dir, &n::concat(&self.dir, dst))), Value::text(&member)],
                        )?;
                        w.run(BUMP, &[])
                    })?;
                }
                Ok(())
            })?;
        }
        Ok(())
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        self.conn = None;
    }
}

/// Restore clones: with clonefile from the original's restored file when
/// `restored` has it, else, or when clonefile declines (another volume, no
/// APFS, not macOS), with the original's bytes from its container. Returns
/// (cloned, copied).
pub fn restore_clones(restored: &Restored, jobs: &[CloneJob]) -> Result<(usize, usize)> {
    let (mut cloned, mut copied) = (0, 0);
    for j in jobs {
        let finish = || -> Result<()> {
            n::chmod(&j.target, j.mode)?;
            n::utimens(&j.target, j.mtime_ns, j.mtime_ns)
        };
        let by_clone = match restored.get(&j.source) {
            Some(file) if n::exists(file) => n::clonefile(file, &j.target).is_ok(),
            _ => false,
        };
        if by_clone {
            finish()?;
            cloned += 1;
        } else {
            let (path, member) = &j.source;
            if !n::exists(path) {
                return Err(Error(format!("{}: its original's container {path} is missing", j.target)));
            }
            let k = Container::open(path.as_ref(), &OpenOptions { create: false, read_only: true, ..OpenOptions::default() })?;
            let d = k.read_data(member);
            k.close()?;
            let d = d?;
            if n::sha_bytes(&d) != j.sha256 {
                return Err(Error(format!("{}: its original {member} has changed", j.target)));
            }
            fs::write(&j.target, &d).map_err(|e| Error(format!("{}: {e}", j.target)))?;
            finish()?;
            copied += 1;
        }
    }
    Ok((cloned, copied))
}

/// What bringing a container up to date with the plain directory [dir] takes,
/// given its `known` members: new and changed files (by size and nanosecond
/// mtime) to store, files gone from [dir] to mark retained. Names in `skip`
/// (plain files already beside the container) are left out. Reads only the
/// plain side.
pub fn plan(skip: &[String], known: &[Member], dir: &str) -> Result<(Vec<Put>, Vec<String>)> {
    plan_deferred(skip, &[], known, dir)
}

/// [plan], with new or changed files named in `defer` (clones, stored after
/// the originals) left out of the puts.
pub fn plan_deferred(skip: &[String], defer: &[String], known: &[Member], dir: &str) -> Result<(Vec<Put>, Vec<String>)> {
    let have: HashMap<&str, &Member> = known.iter().map(|m| (m.name.as_str(), m)).collect();
    let mut names: Vec<String> = n::readdir(dir)?
        .into_iter()
        .filter(|name| !skip.contains(name) && n::lstat_opt(&n::concat(dir, name)).is_some_and(|s| s.kind == Kind::Reg))
        .collect();
    names.sort();
    let mut puts = Vec::new();
    for name in &names {
        let source = n::concat(dir, name);
        let st = n::stat(&source, true)?;
        let same = have.get(name.as_str()).is_some_and(|m| m.size == st.size && m.mtime_ns == st.mtime_ns && !m.retained);
        if !same && !defer.contains(name) {
            puts.push(Put { member: name.clone(), source });
        }
    }
    let present: HashSet<&String> = names.iter().collect();
    let mut retain: Vec<String> = known.iter().filter(|m| !present.contains(&m.name) && !m.retained).map(|m| m.name.clone()).collect();
    retain.sort();
    Ok((puts, retain))
}

/// Rebuild [path] into a fresh container: the same members (retained ones too
/// unless `drop_retained`, whose clones first get bytes of their own), clone
/// rows and back-references, no free pages, the epoch carried on. The new
/// file is verified and flushed to the platter before it replaces the old
/// one, so a crash leaves one or the other whole. Returns (members kept,
/// bytes before, bytes after).
pub fn repack(path: &str, drop_retained: bool, journal: Journal, engine: Option<Engine>) -> Result<(usize, i64, i64)> {
    let fresh = format!("{path}.repack");
    if n::exists(&fresh) {
        return Err(Error(format!("{fresh} exists: an earlier repack was interrupted; check and remove it")));
    }
    let before = n::stat(path, true)?.size;
    if drop_retained {
        // Dropped originals first give their clones bytes of their own.
        let c = Container::open(path.as_ref(), &OpenOptions { create: false, engine, ..OpenOptions::default() })?;
        let r = c.members().and_then(|ms| {
            let retained: Vec<String> = ms.into_iter().filter(|m| m.retained).map(|m| m.name).collect();
            c.materialize(None, &retained)
        });
        c.close()?;
        r?;
    }
    let old = Container::open(path.as_ref(), &OpenOptions { create: false, read_only: true, engine, ..OpenOptions::default() })?;
    let problems = old.verify()?;
    if !problems.is_empty() {
        old.close()?;
        return Err(Error(format!("{path}: {}; not repacked", problems.join("; "))));
    }
    let c = Container::open_as(fresh.as_ref(), &OpenOptions { journal, engine, ..OpenOptions::default() }, Some(path))?;
    let keep: Vec<Member> = old.members()?.into_iter().filter(|m| !(drop_retained && m.retained)).collect();
    let next_epoch = old.epoch()? + 1;
    let old_clones = old.has_clones();
    let w = c.c();
    w.transaction(|| {
        for m in &keep {
            let rows = old.c().query("SELECT mode, mtime, sz, data FROM sqlar WHERE name = ?", &[Value::text(&m.name)])?;
            if let [r] = rows.as_slice() {
                w.run("INSERT INTO sqlar(name, mode, mtime, sz, data) VALUES (?, ?, ?, ?, ?)", &[Value::text(&m.name), r[0].clone(), r[1].clone(), r[2].clone(), r[3].clone()])?;
            } else {
                let rows = if old_clones {
                    old.c().query("SELECT mode, mtime, sz, original, sha256 FROM packdir_clone WHERE name = ?", &[Value::text(&m.name)])?
                } else {
                    Vec::new()
                };
                let [r] = rows.as_slice() else { return Err(t::Error::new(t::ErrorKind::Other, format!("{}: no data", m.name))) };
                w.run(
                    "INSERT INTO packdir_clone(name, mode, mtime, sz, original, sha256) VALUES (?, ?, ?, ?, ?, ?)",
                    &[Value::text(&m.name), r[0].clone(), r[1].clone(), r[2].clone(), r[3].clone(), r[4].clone()],
                )?;
            }
            w.run(
                "INSERT INTO unison_member(name, mtime_ns, fp, fpalg, sha256, retained_deleted) VALUES (?, ?, ?, ?, ?, ?)",
                &[Value::text(&m.name), Value::Integer(m.mtime_ns), Value::Blob(m.fp.clone()), Value::text(&m.fpalg), Value::Blob(m.sha256.as_bytes().to_vec()), Value::Integer(m.retained as i64)],
            )?;
        }
        if old_clones {
            for r in old.c().query("SELECT name, clone FROM packdir_cloned_by", &[])? {
                w.run("INSERT INTO packdir_cloned_by(name, clone) VALUES (?, ?)", &[r[0].clone(), r[1].clone()])?;
            }
            let format = old.c().query("SELECT value FROM unison_container WHERE key = 'format'", &[])?;
            if let [f] = format.as_slice()
                && f[0].as_bytes()? == FORMAT_CLONES.as_bytes()
            {
                mark_clones(w)?;
            }
        }
        w.run("UPDATE unison_container SET value = ? WHERE key = 'epoch'", &[Value::text(&next_epoch.to_string())])
    })?;
    let problems = c.verify()?;
    if !problems.is_empty() {
        return Err(Error(format!("{fresh}: {}", problems.join("; "))));
    }
    c.close()?;
    old.close()?;
    barrier(&fresh)?;
    fs::rename(&fresh, path).map_err(|e| Error(format!("{path}: {e}")))?;
    Ok((keep.len(), before, n::stat(path, true)?.size))
}
