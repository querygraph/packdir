//! A packed directory: the files of a directory D stored as members of one
//! SQLite archive D.sqlar, used like a directory. The Rust build of packdir,
//! equivalent to the OCaml one: same files, same command line.
//!
//! ```no_run
//! use packdir::container::{Container, OpenOptions};
//! let c = Container::open("leaf.sqlar".as_ref(), &OpenOptions::default()).unwrap();
//! for m in c.members().unwrap() {
//!     println!("{} {}", m.name, m.size);
//! }
//! c.close().unwrap();
//! ```

pub mod cli;
pub mod container;
pub mod fileutils;
pub mod native;

pub use firstpair_tursolite as tursolite;

/// A failure, with the words the command line prints.
#[derive(Debug, Clone, PartialEq)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl From<firstpair_tursolite::Error> for Error {
    fn from(e: firstpair_tursolite::Error) -> Error {
        Error(e.message)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Error {
        Error(e.to_string())
    }
}

impl From<String> for Error {
    fn from(e: String) -> Error {
        Error(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
