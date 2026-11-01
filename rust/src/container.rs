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
use std::sync::atomic::{AtomicUsize, Ordering};

pub const FORMAT: &str = "unison-packdir/1";
/// Unison's archive fingerprint: MD5 of the bytes.
pub const FPALG: &str = "unison-md5/1";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sqlar(name TEXT PRIMARY KEY, mode INT, mtime INT, sz INT, data BLOB);
CREATE TABLE IF NOT EXISTS unison_member(name TEXT PRIMARY KEY REFERENCES sqlar(name),
  mtime_ns INT NOT NULL, fp BLOB NOT NULL, fpalg TEXT NOT NULL, sha256 BLOB NOT NULL,
  retained_deleted INT NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS unison_container(key TEXT PRIMARY KEY, value TEXT);
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
}

/// A member to store: its name, and the plain file holding its bytes.
#[derive(Clone, Debug, PartialEq)]
pub struct Put {
    pub member: String,
    pub source: String,
}

/// Limits on [Container::apply].
#[derive(Clone, Copy)]
pub struct Apply<'a> {
    pub batch_bytes: usize,
    /// Takes turns on one disk; holds only the transactions.
    pub write_lock: Option<&'a (dyn Fn(&mut dyn FnMut() -> Result<()>) -> Result<()> + Sync)>,
    pub writers: usize,
}

impl Default for Apply<'_> {
    fn default() -> Self {
        Apply { batch_bytes: 256 * 1048576, write_lock: None, writers: 1 }
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

impl Container {
    /// Opens D.sqlar, creating it with its page size and vacuum mode. Turso has
    /// no rollback journal and cannot write auto-vacuum: what it writes has
    /// neither until it is sealed on close, and a container SQLite made with
    /// auto-vacuum stays SQLite's until [repack] with Turso rebuilds it.
    pub fn open(path: &Path, o: &OpenOptions) -> Result<Container> {
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
        let db = Database::open(path, &t::Options {
            engine: resolved,
            journal: o.journal,
            page_size: Some(65536),
            auto_vacuum: fresh && resolved == Engine::Sqlite,
            read_only,
            create: o.create,
            ..t::Options::default()
        })?;
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
                [r] if r.len() == 1 && r[0] == Value::text(FORMAT) => {}
                _ => {
                    drop(c);
                    db.close()?;
                    return Err(Error(format!("{name}: not a {FORMAT} container")));
                }
            }
        }
        Ok(Container { db: Some(db), conn: Some(c), path: name, journal: o.journal })
    }

    fn c(&self) -> &Connection {
        self.conn.as_ref().expect("open container")
    }

    fn database(&self) -> &Database {
        self.db.as_ref().expect("open container")
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

    /// The member table: one indexed query, no member data read.
    pub fn members(&self) -> Result<Vec<Member>> {
        let rows = self.c().query(
            "SELECT m.name, s.sz, m.mtime_ns, m.fp, m.fpalg, m.sha256, m.retained_deleted \
             FROM unison_member m JOIN sqlar s ON s.name = m.name ORDER BY m.name",
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
    }

    pub fn read(&self, name: &str) -> Result<Vec<u8>> {
        let rows = self.c().query("SELECT data FROM sqlar WHERE name = ?", &[Value::text(name)])?;
        match rows.first() {
            Some(r) if rows.len() == 1 => Ok(r[0].as_bytes()?.to_vec()),
            _ => Err(Error(format!("{name}: no such member"))),
        }
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
    /// bytes. Returns the bytes stored.
    pub fn apply(&self, a: Apply<'_>, puts: &[Put], retain: &[String]) -> Result<usize> {
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
                match a.write_lock {
                    Some(lock) => lock(&mut commit)?,
                    None => commit()?,
                }
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

    /// Problems found: the structure check, then every member's SHA-256 and
    /// the pairing of the two tables.
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
        for r in &self.c().query("SELECT m.name FROM unison_member m LEFT JOIN sqlar s ON s.name = m.name WHERE s.name IS NULL", &[])? {
            problems.push(format!("{}: unison_member row without data", r[0].as_str()?));
        }
        Ok(problems)
    }

    /// Bring D.sqlar up to date with [dir]. Returns (stored, retained, bytes).
    pub fn update(&self, a: Apply<'_>, skip: &[String], dir: &str) -> Result<(usize, usize, usize)> {
        let (puts, retain) = plan(skip, &self.members()?, dir)?;
        let bytes = if puts.is_empty() && retain.is_empty() { 0 } else { self.apply(a, &puts, &retain)? };
        Ok((puts.len(), retain.len(), bytes))
    }

    /// Writes every member to [out] with its mode and nanosecond mtime, SHA-256
    /// checked; retained deletions too unless `skip_retained`.
    pub fn restore(&self, skip_retained: bool, out: &str) -> Result<()> {
        fs::create_dir_all(out).map_err(|e| Error(format!("{out}: {e}")))?;
        for m in self.members()? {
            if skip_retained && m.retained {
                continue;
            }
            let d = self.read(&m.name)?;
            if n::sha_bytes(&d) != m.sha256 {
                return Err(Error(format!("{}: SHA-256 mismatch", m.name)));
            }
            let target = n::concat(out, &m.name);
            if n::exists(&target) {
                return Err(Error(format!("{target} exists")));
            }
            fs::write(&target, &d).map_err(|e| Error(format!("{target}: {e}")))?;
            let rows = self.c().query("SELECT mode FROM sqlar WHERE name = ?", &[Value::text(&m.name)])?;
            if let Some(Value::Integer(mode)) = rows.first().map(|r| &r[0]) {
                n::chmod(&target, *mode as u32 & 0o7777)?;
            }
            n::utimens(&target, m.mtime_ns, m.mtime_ns)?;
        }
        Ok(())
    }

    /// Name -> mode of every member, from the sqlar table (for ls -l and stat).
    pub fn modes(&self) -> Result<HashMap<String, u32>> {
        let mut t = HashMap::new();
        for r in self.c().query("SELECT name, mode FROM sqlar", &[])? {
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

    /// Delete members, bytes and all (rm).
    pub fn remove(&self, names: &[String]) -> Result<()> {
        let c = self.c();
        self.change(|| {
            for name in names {
                c.run("DELETE FROM unison_member WHERE name = ?", &[Value::text(name)])?;
                c.run("DELETE FROM sqlar WHERE name = ?", &[Value::text(name)])?;
            }
            Ok(())
        })
    }

    /// Rename a member within its container (mv); refuses an existing target.
    pub fn rename(&self, src: &str, dst: &str) -> Result<()> {
        if dst.contains('/') {
            return Err(Error(format!("member name with a slash: {dst}")));
        }
        let c = self.c();
        self.change(|| {
            if !c.query("SELECT 1 FROM sqlar WHERE name = ?", &[Value::text(dst)])?.is_empty() {
                return Err(t::Error::new(format!("{dst} exists")));
            }
            c.run("UPDATE sqlar SET name = ? WHERE name = ?", &[Value::text(dst), Value::text(src)])?;
            c.run("UPDATE unison_member SET name = ? WHERE name = ?", &[Value::text(dst), Value::text(src)])
        })
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        self.conn = None;
    }
}

/// What bringing a container up to date with the plain directory [dir] takes,
/// given its `known` members: new and changed files (by size and nanosecond
/// mtime) to store, files gone from [dir] to mark retained. Names in `skip`
/// (plain files already beside the container) are left out. Reads only the
/// plain side.
pub fn plan(skip: &[String], known: &[Member], dir: &str) -> Result<(Vec<Put>, Vec<String>)> {
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
        if !same {
            puts.push(Put { member: name.clone(), source });
        }
    }
    let present: HashSet<&String> = names.iter().collect();
    let mut retain: Vec<String> = known.iter().filter(|m| !present.contains(&m.name) && !m.retained).map(|m| m.name.clone()).collect();
    retain.sort();
    Ok((puts, retain))
}

/// Rebuild [path] into a fresh container: the same members (retained ones too
/// unless `drop_retained`), no free pages, the epoch carried on. The new file
/// is verified and flushed to the platter before it replaces the old one, so
/// a crash leaves one or the other whole. Returns (members kept, bytes before,
/// bytes after).
pub fn repack(path: &str, drop_retained: bool, journal: Journal, engine: Option<Engine>) -> Result<(usize, i64, i64)> {
    let fresh = format!("{path}.repack");
    if n::exists(&fresh) {
        return Err(Error(format!("{fresh} exists: an earlier repack was interrupted; check and remove it")));
    }
    let before = n::stat(path, true)?.size;
    let old = Container::open(path.as_ref(), &OpenOptions { create: false, read_only: true, engine, ..OpenOptions::default() })?;
    let problems = old.verify()?;
    if !problems.is_empty() {
        old.close()?;
        return Err(Error(format!("{path}: {}; not repacked", problems.join("; "))));
    }
    let c = Container::open(fresh.as_ref(), &OpenOptions { journal, engine, ..OpenOptions::default() })?;
    let keep: Vec<Member> = old.members()?.into_iter().filter(|m| !(drop_retained && m.retained)).collect();
    let next_epoch = old.epoch()? + 1;
    let w = c.c();
    w.transaction(|| {
        for m in &keep {
            let rows = old.c().query("SELECT mode, mtime, sz, data FROM sqlar WHERE name = ?", &[Value::text(&m.name)])?;
            let [r] = rows.as_slice() else { return Err(t::Error::new(format!("{}: no data", m.name))) };
            w.run("INSERT INTO sqlar(name, mode, mtime, sz, data) VALUES (?, ?, ?, ?, ?)", &[Value::text(&m.name), r[0].clone(), r[1].clone(), r[2].clone(), r[3].clone()])?;
            w.run(
                "INSERT INTO unison_member(name, mtime_ns, fp, fpalg, sha256, retained_deleted) VALUES (?, ?, ?, ?, ?, ?)",
                &[Value::text(&m.name), Value::Integer(m.mtime_ns), Value::Blob(m.fp.clone()), Value::text(&m.fpalg), Value::Blob(m.sha256.as_bytes().to_vec()), Value::Integer(m.retained as i64)],
            )?;
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
