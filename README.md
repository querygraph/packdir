# packdir

packdir keeps the files of a directory `D` as members of one SQLite archive, `D.sqlar`. A tree of
millions of small files becomes a few thousand files on disk. You can still list, read, copy and
remove what's inside as if it were plain, and `sqlite3 D.sqlar -AxC D` restores it with no
packdir at all.

There are two builds that behave the same, OCaml (stock OCaml 5 and OxCaml) in `lib/` and Rust in
`rust/`, each a command and a library. Each stores containers with SQLite or with Turso, through
[firstpair-tursolite](https://github.com/querygraph/tursolite), and any of them reads what any other wrote.

It was built for Semantic Unison, to mirror a 3.6-million-file preview cache onto a spinning HFS+
disk, and works on its own. The manual is `docs/packdir-manual.md`.

## Building

OCaml or OxCaml, in the switch you use. Without the SDK it builds on SQLite alone:

```sh
git clone https://github.com/querygraph/tursolite ../tursolite
make -C ../tursolite sdk
opam pin add firstpair-tursolite-sdk ../tursolite
opam pin add firstpair-tursolite ../tursolite
opam pin add packdir .
```

Rust, with both engines by default (features `sqlite`, `turso`):

```sh
cd rust && cargo build --release
```

## Commands

```sh
packdir pack-tree ROOT OUT [--depth 2] [--workers N] [--wal] [--batch-mib 256] [--barrier-gib 4]
                           [--engine sqlite|turso] [--mvcc] [--writers N]
packdir unpack-tree OUT DEST [--skip-retained]
packdir repack C.sqlar... [--drop-retained] [--engine sqlite|turso]
packdir verify C.sqlar...
packdir list C.sqlar
packdir restore C.sqlar OUT_DIR [--skip-retained]
```

- **`pack-tree`** keeps every directory `--depth` levels under `ROOT` as `OUT/<rel>.sqlar`. A
  rerun stores only new and changed files, by size and nanosecond mtime, and marks files gone from
  `ROOT` as retained. Containers whose file hasn't changed aren't opened. Workers read and hash in
  parallel, and only the transactions take turns, so the disk sees one writer. `--mvcc` writes with
  Turso's concurrent transactions and no lock between containers; `--writers` overlaps them inside
  one container.
- **`unpack-tree`** turns every container back into a plain tree, with exact bytes, modes and
  nanosecond mtimes. It never overwrites a file.
- **`repack`** rebuilds containers compactly, optionally without retained members. The new file is
  verified and flushed to the platter before it replaces the old one. `--engine turso` rebuilds a
  container made by SQLite into the form Turso can write.
- **`verify`** runs `PRAGMA integrity_check`, then checks every member's SHA-256 and fingerprint.
  It never creates a file; a missing container is an error, as for `list`, `restore` and `repack`.

`PACKDIR_ENGINE=turso` sets the engine for every command, the file commands included.

## File commands

These see a packed tree as plain files. A path `ROOT/A/B/name` reads through `ROOT/A/B.sqlar`,
and a member wins over a plain file of the same name beside its container.

| Command | Options |
| --- | --- |
| `ls` | `-l -1 -a -A -h -R -r -t -S -U -d -g -n`, `--full-time`, `--sort=`, `--time-style=full-iso`, and `--retained` to show retained deletions |
| `cat` | `-n -b`, `-` for standard input |
| `stat` | `-c FORMAT`, `--printf FORMAT`: `%n %s %a %A %y %Y %F %U %G %u %g %h`, with widths such as `%04a` |
| `du` | `-a -b -s -h -c -d N`; sizes are apparent sizes, as with GNU's `--apparent-size` |
| `find` | `-name GLOB -type f\|d\|l -maxdepth N -mindepth N` |
| `rm` | `-f -r -R -d -v` |
| `cp` | `-p -a -r -R -n -v -t DIR` |
| `mv` | `-n -v -t DIR`; renames inside a container, otherwise copies then removes |
| `mkdir` | `-p -v`, and `--packed` to start an empty container |

Output, exit codes and messages match GNU coreutils 9 under `LC_ALL=C`; `test/fileutils.py` runs
each command on a packed tree and GNU's on the same tree unpacked. A few things can't match:
`ls -l`'s `total` line and directory lines, `du`'s directory sizes, and the order of `du` and
`find` output, which is sorted here.

`rm`, `mv` and `cp` into a container change it in one transaction that bumps its epoch.

## The container format

```sql
CREATE TABLE sqlar(name TEXT PRIMARY KEY, mode INT, mtime INT, sz INT, data BLOB);  -- the standard table
CREATE TABLE unison_member(name TEXT PRIMARY KEY REFERENCES sqlar(name),
  mtime_ns INT NOT NULL, fp BLOB NOT NULL, fpalg TEXT NOT NULL, sha256 BLOB NOT NULL,
  retained_deleted INT NOT NULL DEFAULT 0);
CREATE TABLE unison_container(key TEXT PRIMARY KEY, value TEXT);  -- format, epoch
```

- `page_size = 65536`, `journal_mode = DELETE`, `synchronous = FULL`, and `auto_vacuum = INCREMENTAL`
  on containers SQLite creates. Turso cannot write auto-vacuum, so its containers have none.
- Blobs are stored raw, so `sz = length(data)`. `sha256` holds the lower-case hex text, and `fp`
  the raw MD5, with `fpalg = 'unison-md5/1'`. The `format` value is `unison-packdir/1`, or `unison-packdir/2` in a container holding clones (see the manual's chapter "Clones").
- The table names are part of the format: Semantic Unison's packed roots read and write the same
  files.

**Durability.** Transactions are synced with `fsync`. One `F_FULLFSYNC` every `--barrier-gib`, and
at the end, empties the drive's own cache. Containers written since the last barrier are listed in
`OUT/.packdir-pending`; the next run verifies those, and any container whose file no longer matches
`OUT/.packdir-state`. A damaged container is set aside as `D.sqlar.damaged-<time>`, never deleted,
and packed again.

## SQLite and Turso

The engines share the format, with rules that `firstpair-tursolite` enforces when a file is opened:
Turso never writes a container with auto-vacuum (`repack --engine turso` makes one it can), and
reads of those fall to SQLite. Turso has no rollback journal and SQLite cannot open a file in MVCC
mode, so everything Turso writes is sealed on close, back to a standard file. A crash leaves a
container unsealed until Turso next opens it and recovers it. SQLite stays the default: on the one
benchmark so far it beat MVCC, and writers inside one container only helped alongside workers. See the
manual's chapters on engines and concurrency.

## As a library

Rust:

```rust
use packdir::container::{Apply, Container, OpenOptions};

let c = Container::open("leaf.sqlar".as_ref(), &OpenOptions::default())?;
let (stored, retained, bytes) = c.update(Apply::default(), &[], "plain/leaf")?;
c.close()?;
```

OCaml and OxCaml:

```ocaml
let c = Packdir.Container.open_ "D.sqlar" in
let bytes = Packdir.Container.apply c
    ~puts:[{ Packdir.Container.member = "a.jpg"; source = "/tmp/a.jpg" }] ~retain:[] in
Packdir.Container.close c
```

`Packdir.Container` is the format, `Packdir.Fileutils` the file commands, `Packdir.Cli` the whole
command line, and `Packdir.Native` the SHA-256, nanosecond `stat`, `utimensat` and `F_FULLFSYNC`
calls.

## Building

It builds with dune on stock OCaml 5 and OxCaml, with the opam `sqlite3` binding.

```sh
dune build
python3 test/controls.py --packdir _build/default/bin/main.exe    # 92 controls
python3 test/fileutils.py --packdir _build/default/bin/main.exe   # 45 controls, against GNU coreutils
```

`PACKDIR_SYNC` (`fullfsync`, `normal`) and `PACKDIR_CRASH_AFTER` exist for benchmarks and tests.
`test/bench.py` compares methods on a target disk.

## License

Apache License 2.0; see `LICENSE`.

## Tests

```sh
python3 test/controls.py --packdir BIN [--pack-args "--engine turso --mvcc --writers 4"]
python3 test/fileutils.py --packdir BIN
python3 test/cross.py --build stock=BIN --build oxcaml=BIN --build rust=BIN
```

`cross.py` packs with every build in every mode, and has every other build read, restore, resume
and recover the result.
