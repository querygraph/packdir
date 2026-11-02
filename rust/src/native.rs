//! Native calls and the file helpers built on them.
use crate::{Error, Result};
use md5::{Digest as _, Md5};
#[cfg(not(target_os = "macos"))]
use sha2::Sha256;
use std::ffi::CString;
use std::fs;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;

pub const BLOCK: usize = 1048576;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Reg,
    Dir,
    Link,
    Other,
}

#[derive(Clone, Copy, Debug)]
pub struct Stat {
    pub ino: i64,
    pub size: i64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub atime_ns: i64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub kind: Kind,
}

fn at(path: &str, e: std::io::Error) -> Error {
    Error(format!("{path}: {e}"))
}

pub fn stat(path: &str, follow: bool) -> Result<Stat> {
    let m = if follow { fs::metadata(path) } else { fs::symlink_metadata(path) }.map_err(|e| at(path, e))?;
    let t = m.file_type();
    Ok(Stat {
        ino: m.ino() as i64,
        size: m.size() as i64,
        mtime_ns: m.mtime() * 1_000_000_000 + m.mtime_nsec(),
        ctime_ns: m.ctime() * 1_000_000_000 + m.ctime_nsec(),
        atime_ns: m.atime() * 1_000_000_000 + m.atime_nsec(),
        mode: m.mode() & 0o7777,
        uid: m.uid(),
        gid: m.gid(),
        kind: if t.is_file() {
            Kind::Reg
        } else if t.is_dir() {
            Kind::Dir
        } else if t.is_symlink() {
            Kind::Link
        } else {
            Kind::Other
        },
    })
}

pub fn lstat_opt(path: &str) -> Option<Stat> {
    stat(path, false).ok()
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

pub fn to_hex(bytes: &[u8]) -> String {
    hex(bytes)
}

pub fn from_hex(s: &str) -> Result<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return Err(Error("bad hex".into()));
    }
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).map_err(|_| Error("bad hex".into())))
        .collect()
}

// macOS's own SHA-256, the one the OCaml build's stubs reach; well ahead of the
// sha2 crate on CPUs without SHA instructions.
#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn CC_SHA256_Init(ctx: *mut u8) -> i32;
    fn CC_SHA256_Update(ctx: *mut u8, data: *const u8, len: u32) -> i32;
    fn CC_SHA256_Final(md: *mut u8, ctx: *mut u8) -> i32;
}

#[cfg(target_os = "macos")]
struct Sha([u64; 16]); // CC_SHA256_CTX is 104 bytes

#[cfg(target_os = "macos")]
impl Sha {
    fn new() -> Sha {
        let mut s = Sha([0; 16]);
        // SAFETY: the buffer is larger than CC_SHA256_CTX and aligned for it.
        unsafe { CC_SHA256_Init(s.0.as_mut_ptr().cast()) };
        s
    }

    fn update(&mut self, data: &[u8]) {
        for chunk in data.chunks(1 << 30) {
            // SAFETY: the context is initialized and the slice valid for its length.
            unsafe { CC_SHA256_Update(self.0.as_mut_ptr().cast(), chunk.as_ptr(), chunk.len() as u32) };
        }
    }

    fn finish(mut self) -> String {
        let mut md = [0u8; 32];
        // SAFETY: md holds the 32 bytes of a SHA-256.
        unsafe { CC_SHA256_Final(md.as_mut_ptr(), self.0.as_mut_ptr().cast()) };
        hex(&md)
    }
}

#[cfg(not(target_os = "macos"))]
struct Sha(Sha256);

#[cfg(not(target_os = "macos"))]
impl Sha {
    fn new() -> Sha {
        Sha(Sha256::new())
    }

    fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    fn finish(self) -> String {
        hex(&self.0.finalize())
    }
}

/// SHA-256 as lowercase hex, the form the format stores.
pub fn sha_bytes(data: &[u8]) -> String {
    let mut h = Sha::new();
    h.update(data);
    h.finish()
}

pub fn sha_file(path: &str) -> Result<String> {
    let mut f = fs::File::open(path).map_err(|e| at(path, e))?;
    let mut h = Sha::new();
    let mut buf = vec![0u8; BLOCK];
    loop {
        let n = f.read(&mut buf).map_err(|e| at(path, e))?;
        if n == 0 {
            return Ok(h.finish());
        }
        h.update(&buf[..n]);
    }
}

/// Unison's archive fingerprint: the raw MD5 of the bytes.
pub fn md5_bytes(data: &[u8]) -> Vec<u8> {
    Md5::digest(data).to_vec()
}

fn c_path(path: &str) -> Result<CString> {
    CString::new(path).map_err(|_| Error(format!("{path}: a NUL in the name")))
}

pub fn utimens(path: &str, atime_ns: i64, mtime_ns: i64) -> Result<()> {
    let spec = |ns: i64| libc::timespec { tv_sec: (ns / 1_000_000_000) as _, tv_nsec: (ns % 1_000_000_000) as _ };
    let t = [spec(atime_ns), spec(mtime_ns)];
    let p = c_path(path)?;
    // SAFETY: p is a NUL-terminated path and t two timespecs, as utimensat wants.
    if unsafe { libc::utimensat(libc::AT_FDCWD, p.as_ptr(), t.as_ptr(), 0) } < 0 {
        return Err(at(path, std::io::Error::last_os_error()));
    }
    Ok(())
}

/// F_FULLFSYNC: flush the drive's own write cache (macOS); fsync elsewhere.
pub fn fullfsync(file: &fs::File) -> Result<()> {
    #[cfg(target_os = "macos")]
    // SAFETY: the descriptor stays open for the length of the call.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) } == 0 {
        return Ok(());
    }
    // SAFETY: as above.
    if unsafe { libc::fsync(file.as_raw_fd()) } < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

// APFS clones: getattrlist's ATTR_CMNEXT_CLONEID and ATTR_CMNEXT_CLONE_REFCNT,
// and clonefile(2). The libc crate has the calls but not every constant.
#[cfg(target_os = "macos")]
const ATTR_CMNEXT_CLONE_REFCNT: u32 = 0x1000;
#[cfg(target_os = "macos")]
const CLONE_NOFOLLOW: u32 = 0x1;
#[cfg(target_os = "macos")]
const CLONE_NOOWNERCOPY: u32 = 0x2;

/// getattrlist's reply: its length, the attributes returned, then the clone
/// id and refcount, packed.
#[cfg(target_os = "macos")]
#[repr(C, packed)]
#[derive(Default)]
struct CloneAttrs {
    length: u32,
    returned: [u32; 5],
    clone_id: u64,
    refcount: u32,
}

/// (device, APFS clone id, refcount) of a regular file on a volume that
/// reports clone ids (macOS APFS), or None. Files with the same device and
/// clone id and a refcount above 1 are whole-file clones of one another;
/// writing to one gives it a new clone id.
pub fn clone_info(path: &str) -> Option<(u64, u64, u64)> {
    let m = fs::symlink_metadata(path).ok()?;
    if !m.file_type().is_file() {
        return None;
    }
    #[cfg(target_os = "macos")]
    {
        let p = c_path(path).ok()?;
        // SAFETY: attrlist is plain data; zero is its empty value.
        let mut al: libc::attrlist = unsafe { std::mem::zeroed() };
        al.bitmapcount = libc::ATTR_BIT_MAP_COUNT;
        al.commonattr = libc::ATTR_CMN_RETURNED_ATTRS;
        al.forkattr = libc::ATTR_CMNEXT_CLONEID | ATTR_CMNEXT_CLONE_REFCNT;
        let mut out = CloneAttrs::default();
        // SAFETY: p is a NUL-terminated path, al an attrlist, and out a buffer
        // of the size passed.
        let rc = unsafe {
            libc::getattrlist(
                p.as_ptr(),
                (&raw mut al).cast(),
                (&raw mut out).cast(),
                std::mem::size_of::<CloneAttrs>(),
                libc::FSOPT_ATTR_CMN_EXTENDED | libc::FSOPT_NOFOLLOW,
            )
        };
        let returned = out.returned;
        let want = libc::ATTR_CMNEXT_CLONEID | ATTR_CMNEXT_CLONE_REFCNT;
        if rc == 0 && returned[4] & want == want {
            return Some((m.dev(), out.clone_id, out.refcount as u64));
        }
    }
    None
}

/// (device, APFS clone id) of a regular file that has whole-file clones, or
/// None: not APFS, not macOS, or no other file shares its clone id.
pub fn clone_key(path: &str) -> Option<(u64, u64)> {
    match clone_info(path) {
        Some((dev, id, refcount)) if refcount > 1 => Some((dev, id)),
        _ => None,
    }
}

/// clonefile(src, dst): a new file `dst` sharing `src`'s blocks. An error where
/// there is no clonefile (another volume, no APFS, not macOS).
pub fn clonefile(src: &str, dst: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let (s, d) = (c_path(src)?, c_path(dst)?);
        // SAFETY: both are NUL-terminated paths.
        if unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), CLONE_NOFOLLOW | CLONE_NOOWNERCOPY) } < 0 {
            return Err(at(dst, std::io::Error::last_os_error()));
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = src;
        Err(at(dst, std::io::Error::from_raw_os_error(libc::EOPNOTSUPP)))
    }
}

pub fn chmod(path: &str, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|e| at(path, e))
}

/// Names in a directory. A name that is not UTF-8 is an error rather than a
/// file silently left out; on macOS the file system does not allow them.
pub fn readdir(dir: &str) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for e in fs::read_dir(dir).map_err(|e| at(dir, e))? {
        let e = e.map_err(|e| at(dir, e))?;
        names.push(e.file_name().into_string().map_err(|n| Error(format!("{dir}: {n:?} is not UTF-8")))?);
    }
    Ok(names)
}

pub fn exists(path: &str) -> bool {
    Path::new(path).exists()
}

pub fn is_dir(path: &str) -> bool {
    Path::new(path).is_dir()
}

/// Filename.concat
pub fn concat(dir: &str, name: &str) -> String {
    if dir.is_empty() || dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// Filename.basename
pub fn basename(path: &str) -> String {
    let t = path.trim_end_matches('/');
    if path.is_empty() {
        ".".into()
    } else if t.is_empty() {
        "/".into()
    } else {
        t.rsplit('/').next().unwrap_or(t).to_string()
    }
}

/// Filename.dirname
pub fn dirname(path: &str) -> String {
    let t = path.trim_end_matches('/');
    if path.is_empty() {
        return ".".into();
    }
    if t.is_empty() {
        return "/".into();
    }
    match t.rfind('/') {
        None => ".".into(),
        Some(i) => {
            let d = t[..i].trim_end_matches('/');
            if d.is_empty() { "/".into() } else { d.to_string() }
        }
    }
}
