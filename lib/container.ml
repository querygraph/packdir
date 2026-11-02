(* A packed directory: the files of one directory D stored as members of one
   SQLite archive D.sqlar (docs/packdir-design.md). The standard sqlar table is
   what stock tools read (sqlite3 D.sqlar -AxC D); unison_member keeps what a
   synchronizer needs (nanosecond mtime, MD5 fingerprint, SHA-256 as hex text,
   retained deletions); unison_container the format and an epoch bumped by
   every committed batch. Blobs are stored raw. The table names are part of
   the on-disk format, shared with Semantic Unison's packed roots. *)

module N = Native

let format = "unison-packdir/1"
(* A container holding clone references (packdir_clone, packdir_cloned_by):
   builds that predate clones refuse it rather than miss members. *)
let format_clones = "unison-packdir/2"
let fpalg = "unison-md5/1" (* Unison's archive fingerprint: MD5 of the bytes *)

module T = Tursolite

type journal = Delete | Wal | Mvcc (* Mvcc: Turso's concurrent transactions *)

(* [dir]: the directory the container stands for (its path without .sqlar),
   which clone references are relative to. [clones]: it has the clone tables. *)
type t = { db : T.database; conn : T.connection; path : string; journal : journal;
           dir : string; mutable clones : bool }

type member = {
  name : string; size : int64; mtime_ns : int64; fp : string; fpalg : string;
  sha256 : string; retained : bool;
}

let exec c sql = T.exec c sql
let query c sql params = T.query c sql params
let run c sql params = T.run c sql params
let text s = T.Text s
let blob s = T.Blob s
let int64 n = T.Integer n
let to_int64 = function T.Integer n -> n | _ -> failwith "expected an integer"
let to_string = function T.Text s | T.Blob s -> s | _ -> failwith "expected text"

(* The engine when a caller names none: sqlite, or PACKDIR_ENGINE. *)
let default_engine () = match Sys.getenv_opt "PACKDIR_ENGINE" with
  | Some e -> T.parse_engine e | None -> T.Sqlite

let schema = {|
CREATE TABLE IF NOT EXISTS sqlar(name TEXT PRIMARY KEY, mode INT, mtime INT, sz INT, data BLOB);
CREATE TABLE IF NOT EXISTS unison_member(name TEXT PRIMARY KEY REFERENCES sqlar(name),
  mtime_ns INT NOT NULL, fp BLOB NOT NULL, fpalg TEXT NOT NULL, sha256 BLOB NOT NULL,
  retained_deleted INT NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS unison_container(key TEXT PRIMARY KEY, value TEXT);
CREATE TABLE IF NOT EXISTS packdir_clone(name TEXT PRIMARY KEY, mode INT, mtime INT, sz INT,
  original TEXT NOT NULL, sha256 BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS packdir_cloned_by(name TEXT NOT NULL, clone TEXT NOT NULL,
  PRIMARY KEY (name, clone));
|}

(* Clones (the manual's chapter "Clones"). A member that was an APFS clone of another is kept
   once: a packdir_clone row with its own mode, mtime and size, the path of
   its original relative to this container's directory, and the SHA-256 both
   share, and no sqlar row, so stock sqlite3 -Ax extracts every other member
   and skips it. The original's container lists its clones in
   packdir_cloned_by, and whatever changes or removes an original first gives
   those clones bytes of their own. *)

(* Lexical normalization of a path; absolute when it starts with /. *)
let normalize path =
  let abs = String.length path > 0 && path.[0] = '/' in
  let parts = List.fold_left (fun acc p -> match p, acc with
    | ("" | "."), _ -> acc
    | "..", x :: rest when x <> ".." -> rest
    | _ -> p :: acc) [] (String.split_on_char '/' path) in
  let s = String.concat "/" (List.rev parts) in
  if abs then "/" ^ s else if s = "" then "." else s

let absolute p = normalize (if Filename.is_relative p then Filename.concat (Sys.getcwd ()) p else p)

(* The path from directory [from] to [target]. *)
let relative ~from target =
  let split p = List.filter (fun x -> x <> "") (String.split_on_char '/' (absolute p)) in
  let rec drop a b = match a, b with x :: a', y :: b' when x = y -> drop a' b' | _ -> a, b in
  let up, down = drop (split from) (split target) in
  String.concat "/" (List.map (fun _ -> "..") up @ down)

let same_file a b = absolute a = absolute b

(* The container path and member name a reference relative to [dir] names:
   member names have no slash, so the container is always the parent's. *)
let locate dir rel =
  let p = normalize (Filename.concat dir rel) in
  Filename.dirname p ^ ".sqlar", Filename.basename p

(* Open D.sqlar, creating it with its page size and vacuum mode, which must be
   set before the first table. The journal is set on every open. Turso has no
   rollback journal and cannot write auto-vacuum: what it writes has neither
   until it is sealed on close, and a container SQLite made with auto-vacuum
   stays SQLite's until repack --engine turso rebuilds it (Tursolite refuses
   anything else). [read_only] is a promise not to write: Turso then leaves
   the file's bytes alone. *)
let open_ ?(journal = Delete) ?(create = true) ?(read_only = false) ?engine ?logical path =
  let want = match engine with Some e -> e | None -> default_engine () in
  let want = if journal = Mvcc then T.Turso else want in
  let fresh = not (Sys.file_exists path) in
  if fresh && not create then failwith (path ^ ": no such container");
  let resolved = T.resolve path want ~read_only in
  (* SQLite stays read-write: a hot journal needs rolling back; so does an
     MVCC file Turso must recover. *)
  let unsealed = match T.inspect path with Some h -> T.is_mvcc h | None -> false in
  let read_only = read_only && resolved = T.Turso && not unsealed && not fresh in
  let tjournal = match journal with Delete -> T.Delete | Wal -> T.Wal | Mvcc -> T.Mvcc in
  let db = T.open_database ~engine:resolved ~journal:tjournal ~page_size:65536
      ~auto_vacuum:(fresh && resolved = T.Sqlite) ~read_only ~create path in
  let c = T.connect db in
  if not read_only then begin
    (* A rollback journal deleted at each commit: about one transaction per
       container per run, and no -journal file left beside it. Each
       transaction is synced with plain fsync (synchronous = FULL); the
       drive's own cache is flushed by [barrier] every few GiB, and a container
       counts as kept only once a barrier covers it (user, 2026-10-31).
       PACKDIR_SYNC, for benchmarks only: "fullfsync" flushes the drive
       cache on every commit, "normal" syncs less often. *)
    let full, fullfsync = match Sys.getenv_opt "PACKDIR_SYNC" with
      | Some "fullfsync" -> true, true | Some "normal" -> false, false | _ -> true, false in
    exec c (if full then "PRAGMA synchronous = FULL" else "PRAGMA synchronous = NORMAL");
    if resolved = T.Sqlite then begin
      exec c (if fullfsync then "PRAGMA fullfsync = ON" else "PRAGMA fullfsync = OFF");
      exec c (if fullfsync then "PRAGMA checkpoint_fullfsync = ON" else "PRAGMA checkpoint_fullfsync = OFF")
    end;
    exec c schema;
    match query c "SELECT value FROM unison_container WHERE key='format'" [] with
    | [] -> run c "INSERT INTO unison_container VALUES ('format', ?), ('epoch', '0')" [text format]
    | [ [| T.Text f |] ] when f = format || f = format_clones -> ()
    | _ -> T.disconnect c; T.close db; failwith (path ^ ": not a " ^ format ^ " container")
  end;
  let clones = query c "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'packdir_clone'" [] <> [] in
  let logical = Option.value logical ~default:path in
  let dir = if Filename.check_suffix logical ".sqlar" then Filename.chop_suffix logical ".sqlar" else logical in
  { db; conn = c; path; journal; dir; clones }

(* One drive-cache flush for everything written to [path]'s disk so far:
   F_FULLFSYNC empties the whole drive's write cache, not just one file's. *)
let barrier path =
  let fd = Unix.openfile path [Unix.O_RDONLY] 0 in
  Fun.protect ~finally:(fun () -> Unix.close fd) (fun () -> N.fullfsync fd)

let close c =
  if c.journal = Wal && T.engine c.db = T.Sqlite then exec c.conn "PRAGMA wal_checkpoint(TRUNCATE)";
  T.disconnect c.conn;
  T.close c.db

let epoch c = match query c.conn "SELECT value FROM unison_container WHERE key='epoch'" [] with
  | [ [| T.Text e |] ] -> int_of_string e | _ -> 0

(* The member table: indexed queries, no member data read. Clones are
   members like any other. *)
let members c =
  let rows table =
    List.map (fun r -> {
        name = to_string r.(0); size = to_int64 r.(1); mtime_ns = to_int64 r.(2); fp = to_string r.(3);
        fpalg = to_string r.(4); sha256 = to_string r.(5); retained = to_int64 r.(6) <> 0L })
      (query c.conn ("SELECT m.name, s.sz, m.mtime_ns, m.fp, m.fpalg, m.sha256, m.retained_deleted \
                      FROM unison_member m JOIN " ^ table ^ " s ON s.name = m.name ORDER BY m.name") []) in
  if c.clones then List.merge (fun a b -> compare a.name b.name) (rows "sqlar") (rows "packdir_clone")
  else rows "sqlar"

(* Clone members: (name, original relative to [c.dir], shared SHA-256). *)
let clones c =
  if not c.clones then [] else
  List.map (fun r -> to_string r.(0), to_string r.(1), to_string r.(2))
    (query c.conn "SELECT name, original, sha256 FROM packdir_clone ORDER BY name" [])

let clone_of c name =
  if not c.clones then None else
  match query c.conn "SELECT original, sha256 FROM packdir_clone WHERE name = ?" [text name] with
  | [ [| o; h |] ] -> Some (to_string o, to_string h) | _ -> None

(* Name -> mode of every member, from the sqlar table (for ls -l and stat). *)
let modes c =
  let t = Hashtbl.create 64 in
  List.iter (fun r -> match r with
    | [| T.Text n; T.Integer m |] -> Hashtbl.replace t n (Int64.to_int m)
    | _ -> ()) (query c.conn (if c.clones then "SELECT name, mode FROM sqlar UNION ALL SELECT name, mode FROM packdir_clone"
                              else "SELECT name, mode FROM sqlar") []);
  t

(* The bytes of a member that holds data. *)
let read_data c name = match query c.conn "SELECT data FROM sqlar WHERE name = ?" [text name] with
  | [ [| d |] ] -> to_string d | _ -> raise Not_found

(* [f] on the container at [path]: [c] itself when it is that file. *)
let with_at ?(read_only = false) c path f =
  if same_file path c.path then f c else
  match open_ ~create:false ~read_only path with
  | k -> Fun.protect ~finally:(fun () -> close k) (fun () -> f k)

(* A member's bytes; a clone's are its original's, SHA-256 checked. *)
let read c name =
  match read_data c name with
  | d -> d
  | exception Not_found ->
      match clone_of c name with
      | None -> raise Not_found
      | Some (original, sha) ->
          let path, member = locate c.dir original in
          if not (Sys.file_exists path) then failwith (name ^ ": clone of " ^ original ^ ", whose container is missing");
          let d = with_at ~read_only:true c path (fun k ->
            try read_data k member with Not_found -> failwith (name ^ ": clone of " ^ original ^ ", which is missing")) in
          if N.sha_string d <> sha then failwith (name ^ ": clone of " ^ original ^ ", which has changed");
          d

type put = { member : string; source : string } (* member name, plain file to store *)

let crash_after = match Sys.getenv_opt "PACKDIR_CRASH_AFTER" with
  | Some n -> int_of_string_opt n | None -> None (* tests: exit inside a transaction *)

(* A file read and hashed, ready to store. *)
type ready = { put : put; st : N.stat; data : string; md5 : string; sha : string }

let read_put p =
  if String.contains p.member '/' then failwith ("member name with a slash: " ^ p.member);
  let st = N.stat p.source in
  if st.N.kind <> `Reg then failwith (p.source ^ ": not a regular file");
  let data = In_channel.with_open_bin p.source In_channel.input_all in
  let st' = N.stat p.source in
  (* Reading moves atime; identity, size, mtime and ctime must hold. *)
  if Int64.of_int (String.length data) <> st.N.size || st'.N.ino <> st.N.ino || st'.N.size <> st.N.size
     || st'.N.mtime_ns <> st.N.mtime_ns || st'.N.ctime_ns <> st.N.ctime_ns then
    failwith (p.source ^ ": changed while read");
  { put = p; st; data; md5 = Digest.string data; sha = N.sha_string data }

let bump = "UPDATE unison_container SET value = CAST(value AS INTEGER) + 1 WHERE key = 'epoch'"

let store c r =
  run c "DELETE FROM packdir_clone WHERE name = ?" [text r.put.member];
  run c "INSERT OR REPLACE INTO sqlar(name, mode, mtime, sz, data) VALUES (?, ?, ?, ?, ?)"
    [text r.put.member; int64 (Int64.of_int (0o100000 lor r.st.N.mode));
     int64 (Int64.div r.st.N.mtime_ns 1_000_000_000L); int64 r.st.N.size; blob r.data];
  run c "INSERT OR REPLACE INTO unison_member(name, mtime_ns, fp, fpalg, sha256, retained_deleted) \
         VALUES (?, ?, ?, ?, ?, 0)"
    [text r.put.member; int64 r.st.N.mtime_ns; blob r.md5; text fpalg; blob r.sha]

let mark_clones c = run c.conn "UPDATE unison_container SET value = ? WHERE key = 'format'" [text format_clones]

(* Before [names] change or go: each clone that references one of them gets
   that original's current bytes as its own sqlar row, in its own container's
   transaction, and the back-references are dropped. A back-reference whose
   clone no longer names this original is stale and simply dropped. *)
let materialize ?(write_lock = fun f -> f ()) c names =
  if c.clones then
    List.iter (fun name ->
      match query c.conn "SELECT clone FROM packdir_cloned_by WHERE name = ?" [text name] with
      | [] -> ()
      | refs ->
          let data = lazy (read_data c name) in
          List.iter (fun r ->
            let path, member = locate c.dir (to_string r.(0)) in
            if Sys.file_exists path then
              with_at c path (fun k ->
                match query k.conn "SELECT mode, mtime, sz, original FROM packdir_clone WHERE name = ?" [text member] with
                | [ [| mode; mtime; sz; original |] ] ->
                    let opath, oname = locate k.dir (to_string original) in
                    if same_file opath c.path && oname = name then
                      write_lock (fun () -> T.transaction k.conn (fun () ->
                        run k.conn "INSERT OR REPLACE INTO sqlar(name, mode, mtime, sz, data) VALUES (?, ?, ?, ?, ?)"
                          [text member; mode; mtime; sz; blob (Lazy.force data)];
                        run k.conn "DELETE FROM packdir_clone WHERE name = ?" [text member];
                        run k.conn bump []))
                | _ -> ())) refs;
          write_lock (fun () -> T.transaction c.conn (fun () ->
            run c.conn "DELETE FROM packdir_cloned_by WHERE name = ?" [text name]))) names

(* A member to keep as a clone: the plain file [source], an APFS clone of the
   member [original] (relative to this container's directory). *)
type clone_put = { clone : string; file : string; original : string }

(* Store [clones] as references. Each file is read and hashed; its original
   must be a member holding data with the same SHA-256. The original's
   container gets its back-reference first, in a transaction of its own; then
   this container gets every clone row in one. No transaction waits for
   another container. Returns the bytes not stored. *)
let apply_clones ?(write_lock = fun f -> f ()) c clones =
  if clones = [] then 0 else begin
  let ready = List.map (fun cp ->
    let r = read_put { member = cp.clone; source = cp.file } in
    let path, oname = locate c.dir cp.original in
    if same_file path c.path && oname = cp.clone then failwith (cp.clone ^ ": a clone of itself");
    if not (Sys.file_exists path) then failwith (cp.clone ^ ": original " ^ cp.original ^ " has no container");
    with_at c path (fun k ->
      (match query k.conn "SELECT m.sha256 FROM unison_member m JOIN sqlar s ON s.name = m.name WHERE m.name = ?" [text oname] with
       | [ [| h |] ] when to_string h = r.sha -> ()
       | [ _ ] -> failwith (cp.clone ^ ": differs from its original " ^ cp.original)
       | _ -> failwith (cp.clone ^ ": original " ^ cp.original ^ " holds no data"));
      let back = relative ~from:k.dir (Filename.concat c.dir cp.clone) in
      write_lock (fun () -> T.transaction k.conn (fun () ->
        run k.conn "INSERT OR IGNORE INTO packdir_cloned_by(name, clone) VALUES (?, ?)" [text oname; text back];
        mark_clones k));
      k.clones <- true);
    r, cp) clones in
  (* A member becoming a clone may itself be an original. *)
  materialize ~write_lock c (List.map (fun cp -> cp.clone) clones);
  write_lock (fun () -> T.transaction c.conn (fun () ->
    List.iter (fun (r, cp) ->
      run c.conn "DELETE FROM sqlar WHERE name = ?" [text cp.clone];
      run c.conn "INSERT OR REPLACE INTO packdir_clone(name, mode, mtime, sz, original, sha256) VALUES (?, ?, ?, ?, ?, ?)"
        [text cp.clone; int64 (Int64.of_int (0o100000 lor r.st.N.mode));
         int64 (Int64.div r.st.N.mtime_ns 1_000_000_000L); int64 r.st.N.size; text cp.original; blob r.sha];
      run c.conn "INSERT OR REPLACE INTO unison_member(name, mtime_ns, fp, fpalg, sha256, retained_deleted) \
                  VALUES (?, ?, ?, ?, ?, 0)"
        [text cp.clone; int64 r.st.N.mtime_ns; blob r.md5; text fpalg; blob r.sha]) ready;
    mark_clones c;
    run c.conn bump []));
  c.clones <- true;
  List.fold_left (fun n (r, _) -> n + String.length r.data) 0 ready
  end

let rec batches ~limit acc size cur = function
  | [] -> List.rev (if cur = [] then acc else List.rev cur :: acc)
  | p :: rest ->
      let n = Int64.to_int (N.stat p.source).N.size in
      if cur <> [] && size + n > limit then batches ~limit (List.rev cur :: acc) n [p] rest
      else batches ~limit acc (size + n) (p :: cur) rest

(* The batches on [writers] domains, each with its own connection. Under MVCC
   the data transactions run concurrently; the epoch is one row they would all
   fight over, so each batch bumps it in a short transaction of its own, after
   its data, and a crash between the two leaves the data with the epoch behind.
   The file's stamp and the pending list, not the epoch, decide what is
   checked again. The retained marks follow the last batch. *)
let apply_concurrent ~writers c groups ~retain =
  let queue = ref groups and lock = Mutex.create () and failure = ref None and stored = ref 0 in
  let written = Atomic.make 0 in
  let next () = Mutex.protect lock (fun () ->
    match !queue, !failure with
    | g :: rest, None -> queue := rest; Some g
    | _ -> None) in
  let writer () =
    let w = T.connect c.db in
    Fun.protect ~finally:(fun () -> T.disconnect w) (fun () ->
      try
        let rec loop () = match next () with
          | None -> ()
          | Some group ->
              let ready = List.map read_put group in
              T.transaction w (fun () ->
                List.iter (fun r ->
                  store w r;
                  if crash_after = Some (Atomic.fetch_and_add written 1 + 1) then Unix._exit 3) ready);
              T.transaction w (fun () -> run w bump []);
              let n = List.fold_left (fun n r -> n + String.length r.data) 0 ready in
              Mutex.protect lock (fun () -> stored := !stored + n);
              loop () in
        loop ()
      with e -> Mutex.protect lock (fun () -> if !failure = None then failure := Some e)) in
  (* What the writers share is behind [lock] or atomic. *)
  let spawn = (Domain.spawn [@alert "-do_not_spawn_domains"] [@alert "-unsafe_multidomain"]) in
  let writers = max 1 (min writers (Domain.recommended_domain_count ())) in
  List.iter Domain.join (List.init writers (fun _ -> spawn writer));
  Option.iter raise !failure;
  if retain <> [] then
    T.transaction c.conn (fun () ->
      List.iter (fun name -> run c.conn "UPDATE unison_member SET retained_deleted = 1 WHERE name = ?" [text name]) retain;
      run c.conn bump []);
  !stored

(* One batch: every put and retained deletion in transactions of at most
   [batch_bytes] of member data, the epoch bumped in each; then the changed
   blobs are read back and their SHA-256 compared, before the caller may
   record them anywhere. Files are read and hashed outside [write_lock]; only
   the transactions run inside it, so several processes can prepare
   containers while one writes to the disk. With MVCC and [writers] above one
   the batches of this container are written concurrently, each at most an
   equal share of its bytes. Returns the bytes stored. *)
let apply ?(batch_bytes = 256 * 1048576) ?(write_lock = fun f -> f ()) ?(writers = 1) c ~puts ~retain =
  materialize ~write_lock c (List.map (fun p -> p.member) puts);
  let concurrent = writers > 1 && T.mvcc c.db && List.length puts > 1 in
  let limit =
    if not concurrent then batch_bytes
    else
      let total = List.fold_left (fun t p -> t + Int64.to_int (N.stat p.source).N.size) 0 puts in
      min batch_bytes (max 1 ((total + writers - 1) / writers)) in
  let groups = match batches ~limit [] 0 [] puts with [] -> [[]] | g -> g in
  let stored = ref 0 in
  if concurrent then stored := apply_concurrent ~writers c groups ~retain
  else begin
    let done_puts = ref 0 and last = List.length groups - 1 in
    List.iteri (fun i group ->
      let ready = List.map read_put group in
      write_lock (fun () ->
        let before = !done_puts in
        T.transaction c.conn (fun () ->
          done_puts := before;
          List.iter (fun r ->
            store c.conn r;
            incr done_puts;
            if crash_after = Some !done_puts then Unix._exit 3) ready;
          if i = last then
            List.iter (fun name -> run c.conn "UPDATE unison_member SET retained_deleted = 1 WHERE name = ?" [text name]) retain;
          run c.conn bump []));
      stored := List.fold_left (fun n r -> n + String.length r.data) !stored ready) groups
  end;
  (* Read back what this batch wrote. *)
  List.iter (fun p ->
    match query c.conn "SELECT s.data, m.sha256 FROM sqlar s JOIN unison_member m ON m.name = s.name WHERE s.name = ?"
            [text p.member] with
    | [ [| d; h |] ] when N.sha_string (to_string d) = to_string h -> ()
    | _ -> failwith (c.path ^ ": " ^ p.member ^ " did not read back")) puts;
  !stored

(* Problems found: the structure check, then every member's SHA-256 and the
   pairing of the two tables. *)
let verify c =
  let problems = ref [] in
  (match query c.conn "PRAGMA integrity_check" [] with
   | [ [| T.Text "ok" |] ] -> ()
   | rows -> List.iter (fun r -> problems := ("integrity: " ^ to_string r.(0)) :: !problems) rows);
  List.iter (fun r ->
    let name = to_string r.(0) in
    match r.(1), r.(2) with
    | T.Null, _ -> problems := (name ^ ": no unison_member row") :: !problems
    | h, d ->
        let d = to_string d in
        if N.sha_string d <> to_string h then problems := (name ^ ": SHA-256 mismatch") :: !problems
        else if Digest.string d <> to_string r.(3) then problems := (name ^ ": fingerprint mismatch") :: !problems)
    (query c.conn "SELECT s.name, m.sha256, s.data, m.fp FROM sqlar s LEFT JOIN unison_member m ON m.name = s.name" []);
  let without_data = if c.clones then
      "SELECT m.name FROM unison_member m LEFT JOIN sqlar s ON s.name = m.name \
       WHERE s.name IS NULL AND m.name NOT IN (SELECT name FROM packdir_clone)"
    else "SELECT m.name FROM unison_member m LEFT JOIN sqlar s ON s.name = m.name WHERE s.name IS NULL" in
  List.iter (fun r -> problems := (to_string r.(0) ^ ": unison_member row without data") :: !problems)
    (query c.conn without_data []);
  (* Each clone: its member row, no data of its own, and an original that
     holds the same bytes. *)
  List.iter (fun (name, original, sha) ->
    let say m = problems := (name ^ ": " ^ m) :: !problems in
    if query c.conn "SELECT 1 FROM sqlar WHERE name = ?" [text name] <> [] then say "both a clone and data";
    match query c.conn "SELECT sha256, fp FROM unison_member WHERE name = ?" [text name] with
    | [ [| h; fp |] ] ->
        if to_string h <> sha then say "clone SHA-256 differs from its member row";
        (match read c name with
         | d -> if Digest.string d <> to_string fp then say "fingerprint mismatch"
         | exception Failure m -> problems := m :: !problems
         | exception Not_found -> say ("clone of " ^ original ^ ", which is missing"))
    | _ -> say "no unison_member row") (clones c);
  List.rev !problems

(* Bring D.sqlar up to date with the plain directory [dir]: new and changed
   files (by size and nanosecond mtime) are stored, files gone from [dir] are
   kept and marked retained. Names in [skip] (plain files already beside the
   container) are left out. Returns (stored, retained, bytes). *)
(* What bringing a container up to date with the plain directory [dir] takes,
   given its [known] members: new and changed files (by size and nanosecond
   mtime) to store, files gone from [dir] to mark retained. Names in [skip]
   (plain files already beside the container) are left out. Reads only the
   plain side. *)
let plan ?(skip = []) ?(defer = []) known dir =
  let have = Hashtbl.create 256 in
  List.iter (fun m -> Hashtbl.replace have m.name m) known;
  let names = List.sort compare (List.filter (fun n ->
      match N.stat ~follow:false (Filename.concat dir n) with
      | s -> s.N.kind = `Reg && not (List.mem n skip) | exception Unix.Unix_error _ -> false)
      (Array.to_list (Sys.readdir dir))) in
  let puts = List.filter_map (fun n ->
    let st = N.stat (Filename.concat dir n) in
    match Hashtbl.find_opt have n with
    | Some m when m.size = st.N.size && m.mtime_ns = st.N.mtime_ns && not m.retained -> None
    | _ when List.mem n defer -> None (* a clone, stored after the originals *)
    | _ -> Some { member = n; source = Filename.concat dir n }) names in
  let present = Hashtbl.create 256 in
  List.iter (fun n -> Hashtbl.replace present n ()) names;
  let retain = Hashtbl.fold (fun n m acc -> if Hashtbl.mem present n || m.retained then acc else n :: acc) have [] in
  puts, retain

(* Bring D.sqlar up to date with [dir]. Returns (stored, retained, bytes). *)
let update ?batch_bytes ?write_lock ?writers ?skip ?defer c dir =
  let puts, retain = plan ?skip ?defer (members c) dir in
  let bytes = if puts = [] && retain = [] then 0 else apply ?batch_bytes ?write_lock ?writers c ~puts ~retain in
  List.length puts, List.length retain, bytes

(* A clone to restore once the originals are on disk. *)
type clone_job = {
  target : string; source : string * string; (* original's container (absolute) and member *)
  job_sha : string; job_mode : int; job_mtime_ns : int64;
}

(* Write every member that holds data to [out] with its mode and nanosecond
   mtime, SHA-256 checked; retained deletions too unless [skip_retained].
   Returns what was written, as ((container, member), file), and the clones
   still to restore. *)
let restore_data ?(skip_retained = false) c out =
  N.mkdir_p out;
  let modes = modes c and written = ref [] and jobs = ref [] in
  List.iter (fun m ->
    if not (skip_retained && m.retained) then begin
      let target = Filename.concat out m.name in
      if Sys.file_exists target then failwith (target ^ " exists");
      let mode = Option.value (Hashtbl.find_opt modes m.name) ~default:0o100644 land 0o7777 in
      match clone_of c m.name with
      | Some (original, _) ->
          let path, member = locate c.dir original in
          jobs := { target; source = (absolute path, member); job_sha = m.sha256; job_mode = mode;
                    job_mtime_ns = m.mtime_ns } :: !jobs
      | None ->
          let d = read_data c m.name in
          if N.sha_string d <> m.sha256 then failwith (m.name ^ ": SHA-256 mismatch");
          Out_channel.with_open_bin target (fun oc -> output_string oc d);
          Unix.chmod target mode;
          N.utimens target m.mtime_ns m.mtime_ns;
          written := ((absolute c.path, m.name), target) :: !written
    end) (members c);
  !written, List.rev !jobs

(* Restore clones: with clonefile from the original's restored file when
   [restored] has it, else, or when clonefile declines (another volume, no
   APFS, not macOS), with the original's bytes from its container. Returns
   (cloned, copied). *)
let restore_clones ~restored jobs =
  let cloned = ref 0 and copied = ref 0 in
  List.iter (fun j ->
    let finish () = Unix.chmod j.target j.job_mode; N.utimens j.target j.job_mtime_ns j.job_mtime_ns in
    let by_clone = match Hashtbl.find_opt restored j.source with
      | Some file when Sys.file_exists file -> (try N.clonefile file j.target; true with Unix.Unix_error _ -> false)
      | _ -> false in
    if by_clone then (finish (); incr cloned)
    else begin
      let path, member = j.source in
      if not (Sys.file_exists path) then failwith (j.target ^ ": its original's container " ^ path ^ " is missing");
      let k = open_ ~create:false ~read_only:true path in
      let d = Fun.protect ~finally:(fun () -> close k) (fun () -> read_data k member) in
      if N.sha_string d <> j.job_sha then failwith (j.target ^ ": its original " ^ member ^ " has changed");
      Out_channel.with_open_bin j.target (fun oc -> output_string oc d);
      finish (); incr copied
    end) jobs;
  !cloned, !copied

(* One container to [out]: data members, then its clones. Returns (cloned, copied). *)
let restore ?skip_retained c out =
  let written, jobs = restore_data ?skip_retained c out in
  let restored = Hashtbl.create 64 in
  List.iter (fun (k, f) -> Hashtbl.replace restored k f) written;
  restore_clones ~restored jobs

(* Rebuild [path] into a fresh container: the same members (retained ones too
   unless [drop_retained]), no free pages, the epoch carried on. The new file
   is verified and flushed to the platter before it replaces the old one, so
   a crash leaves one or the other whole. Returns (members kept, bytes before,
   bytes after). *)
let repack ?(drop_retained = false) ?(journal = Delete) ?engine path =
  let fresh = path ^ ".repack" in
  if Sys.file_exists fresh then failwith (fresh ^ " exists: an earlier repack was interrupted; check and remove it");
  let before = (N.stat path).N.size in
  if drop_retained then begin
    (* Dropped originals first give their clones bytes of their own. *)
    let c = open_ ~create:false ?engine path in
    Fun.protect ~finally:(fun () -> close c) (fun () ->
      materialize c (List.filter_map (fun m -> if m.retained then Some m.name else None) (members c)))
  end;
  let old = open_ ~journal:Delete ~create:false ~read_only:true ?engine path in
  let kept = Fun.protect ~finally:(fun () -> close old) (fun () ->
    let problems = verify old in
    if problems <> [] then failwith (path ^ ": " ^ String.concat "; " problems ^ "; not repacked");
    let c = open_ ~journal ?engine ~logical:path fresh in
    Fun.protect ~finally:(fun () -> close c) (fun () ->
      let keep = List.filter (fun m -> not (drop_retained && m.retained)) (members old) in
      T.transaction c.conn (fun () ->
        List.iter (fun m ->
          (match query old.conn "SELECT mode, mtime, sz, data FROM sqlar WHERE name = ?" [text m.name] with
           | [ [| mode; mtime; sz; data |] ] ->
               run c.conn "INSERT INTO sqlar(name, mode, mtime, sz, data) VALUES (?, ?, ?, ?, ?)" [text m.name; mode; mtime; sz; data]
           | _ when old.clones ->
               (match query old.conn "SELECT mode, mtime, sz, original, sha256 FROM packdir_clone WHERE name = ?" [text m.name] with
                | [ [| mode; mtime; sz; original; sha |] ] ->
                    run c.conn "INSERT INTO packdir_clone(name, mode, mtime, sz, original, sha256) VALUES (?, ?, ?, ?, ?, ?)"
                      [text m.name; mode; mtime; sz; original; sha]
                | _ -> failwith (m.name ^ ": no data"))
           | _ -> failwith (m.name ^ ": no data"));
          run c.conn "INSERT INTO unison_member(name, mtime_ns, fp, fpalg, sha256, retained_deleted) VALUES (?, ?, ?, ?, ?, ?)"
            [text m.name; int64 m.mtime_ns; blob m.fp; text m.fpalg; blob m.sha256; int64 (if m.retained then 1L else 0L)]) keep;
        if old.clones then begin
          List.iter (fun r -> run c.conn "INSERT INTO packdir_cloned_by(name, clone) VALUES (?, ?)" [r.(0); r.(1)])
            (query old.conn "SELECT name, clone FROM packdir_cloned_by" []);
          (match query old.conn "SELECT value FROM unison_container WHERE key = 'format'" [] with
           | [ [| f |] ] when to_string f = format_clones -> mark_clones c
           | _ -> ())
        end;
        run c.conn "UPDATE unison_container SET value = ? WHERE key = 'epoch'" [text (string_of_int (epoch old + 1))]);
      let problems = verify c in
      if problems <> [] then failwith (fresh ^ ": " ^ String.concat "; " problems);
      List.length keep)) in
  barrier fresh;
  Sys.rename fresh path;
  kept, before, (N.stat path).N.size

(* One transaction that changes the member set, bumping the epoch so that
   Unison sees the container changed outside it. *)
let change ?(write_lock = fun f -> f ()) c f =
  write_lock (fun () ->
    T.transaction c.conn (fun () ->
      f ();
      run c.conn bump []))

(* Delete members, bytes and all (rm). *)
let remove ?write_lock c names =
  materialize ?write_lock c names;
  change ?write_lock c (fun () ->
    List.iter (fun n ->
      run c.conn "DELETE FROM unison_member WHERE name = ?" [text n];
      if c.clones then run c.conn "DELETE FROM packdir_clone WHERE name = ?" [text n];
      run c.conn "DELETE FROM sqlar WHERE name = ?" [text n]) names)

(* Rename a member within its container (mv); refuses an existing target. *)
(* A renamed clone moves its back-reference; a renamed original moves its
   clones' references, each in its clone's container after this one. *)
let rename ?(write_lock = fun f -> f ()) c ~src ~dst =
  if String.contains dst '/' then failwith ("member name with a slash: " ^ dst);
  let clone = clone_of c src in
  let refs = if c.clones then List.map (fun r -> to_string r.(0))
      (query c.conn "SELECT clone FROM packdir_cloned_by WHERE name = ?" [text src]) else [] in
  change ~write_lock c (fun () ->
    if List.exists (fun m -> m.name = dst) (members c) then failwith (dst ^ " exists");
    run c.conn "UPDATE sqlar SET name = ? WHERE name = ?" [text dst; text src];
    run c.conn "UPDATE unison_member SET name = ? WHERE name = ?" [text dst; text src];
    if c.clones then begin
      run c.conn "UPDATE packdir_clone SET name = ? WHERE name = ?" [text dst; text src];
      run c.conn "UPDATE packdir_cloned_by SET name = ? WHERE name = ?" [text dst; text src]
    end);
  (match clone with
   | Some (original, _) ->
       let path, oname = locate c.dir original in
       if Sys.file_exists path then
         with_at c path (fun k ->
           let back n = relative ~from:k.dir (Filename.concat c.dir n) in
           write_lock (fun () -> T.transaction k.conn (fun () ->
             run k.conn "DELETE FROM packdir_cloned_by WHERE name = ? AND clone = ?" [text oname; text (back src)];
             run k.conn "INSERT OR IGNORE INTO packdir_cloned_by(name, clone) VALUES (?, ?)" [text oname; text (back dst)])))
   | None -> ());
  List.iter (fun r ->
    let path, member = locate c.dir r in
    if Sys.file_exists path then
      with_at c path (fun k ->
        match query k.conn "SELECT original FROM packdir_clone WHERE name = ?" [text member] with
        | [ [| o |] ] when (let p, n = locate k.dir (to_string o) in same_file p c.path && n = src) ->
            write_lock (fun () -> T.transaction k.conn (fun () ->
              run k.conn "UPDATE packdir_clone SET original = ? WHERE name = ?"
                [text (relative ~from:k.dir (Filename.concat c.dir dst)); text member];
              run k.conn bump []))
        | _ -> ())) refs
