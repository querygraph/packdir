(* A packed directory: the files of one directory D stored as members of one
   SQLite archive D.sqlar (docs/packdir-design.md). The standard sqlar table is
   what stock tools read (sqlite3 D.sqlar -AxC D); unison_member keeps what a
   synchronizer needs (nanosecond mtime, MD5 fingerprint, SHA-256 as hex text,
   retained deletions); unison_container the format and an epoch bumped by
   every committed batch. Blobs are stored raw. The table names are part of
   the on-disk format, shared with Semantic Unison's packed roots. *)

module N = Native

let format = "unison-packdir/1"
let fpalg = "unison-md5/1" (* Unison's archive fingerprint: MD5 of the bytes *)

type journal = Delete | Wal

type t = { db : Sqlite3.db; path : string; journal : journal }

type member = {
  name : string; size : int64; mtime_ns : int64; fp : string; fpalg : string;
  sha256 : string; retained : bool;
}

let check db what rc =
  if not (Sqlite3.Rc.is_success rc) then
    failwith (Printf.sprintf "%s: %s (%s)" what (Sqlite3.Rc.to_string rc) (Sqlite3.errmsg db))

let exec db sql = check db sql (Sqlite3.exec db sql)

let query db sql params =
  let st = Sqlite3.prepare db sql in
  Fun.protect ~finally:(fun () -> ignore (Sqlite3.finalize st)) (fun () ->
    List.iteri (fun i p -> check db "bind" (Sqlite3.bind st (i + 1) p)) params;
    let rec rows acc = match Sqlite3.step st with
      | Sqlite3.Rc.ROW -> rows (Sqlite3.row_data st :: acc)
      | Sqlite3.Rc.DONE -> List.rev acc
      | rc -> check db sql rc; List.rev acc in
    rows [])

let run db sql params = ignore (query db sql params)
let text s = Sqlite3.Data.TEXT s
let blob s = Sqlite3.Data.BLOB s
let int64 n = Sqlite3.Data.INT n
let to_int64 = function Sqlite3.Data.INT n -> n | _ -> failwith "expected an integer"
let to_string = function Sqlite3.Data.TEXT s | Sqlite3.Data.BLOB s -> s | _ -> failwith "expected text"

let schema = {|
CREATE TABLE IF NOT EXISTS sqlar(name TEXT PRIMARY KEY, mode INT, mtime INT, sz INT, data BLOB);
CREATE TABLE IF NOT EXISTS unison_member(name TEXT PRIMARY KEY REFERENCES sqlar(name),
  mtime_ns INT NOT NULL, fp BLOB NOT NULL, fpalg TEXT NOT NULL, sha256 BLOB NOT NULL,
  retained_deleted INT NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS unison_container(key TEXT PRIMARY KEY, value TEXT);
|}

(* Open D.sqlar, creating it with its page size and vacuum mode, which must be
   set before the first table. The journal is set on every open. *)
let open_ ?(journal = Delete) ?(create = true) path =
  let fresh = not (Sys.file_exists path) in
  if fresh && not create then failwith (path ^ ": no such container");
  let db = Sqlite3.db_open path in
  Sqlite3.busy_timeout db 60000;
  if fresh then (exec db "PRAGMA page_size = 65536"; exec db "PRAGMA auto_vacuum = INCREMENTAL");
  (* A rollback journal deleted at each commit: about one transaction per
     container per run, and no -journal file left beside it. *)
  exec db (match journal with Delete -> "PRAGMA journal_mode = DELETE" | Wal -> "PRAGMA journal_mode = WAL");
  (* Each transaction is synced with plain fsync (synchronous = FULL); the
     drive's own cache is flushed by [barrier] every few GiB, and a container
     counts as kept only once a barrier covers it (user, 2026-10-31).
     PACKDIR_SYNC, for benchmarks only: "fullfsync" flushes the drive
     cache on every commit, "normal" syncs less often. *)
  let full, fullfsync = match Sys.getenv_opt "PACKDIR_SYNC" with
    | Some "fullfsync" -> true, true | Some "normal" -> false, false | _ -> true, false in
  exec db (if full then "PRAGMA synchronous = FULL" else "PRAGMA synchronous = NORMAL");
  exec db (if fullfsync then "PRAGMA fullfsync = ON" else "PRAGMA fullfsync = OFF");
  exec db (if fullfsync then "PRAGMA checkpoint_fullfsync = ON" else "PRAGMA checkpoint_fullfsync = OFF");
  exec db schema;
  (match query db "SELECT value FROM unison_container WHERE key='format'" [] with
   | [] -> run db "INSERT INTO unison_container VALUES ('format', ?), ('epoch', '0')" [text format]
   | [ [| Sqlite3.Data.TEXT f |] ] when f = format -> ()
   | _ -> ignore (Sqlite3.db_close db); failwith (path ^ ": not a " ^ format ^ " container"));
  { db; path; journal }

(* One drive-cache flush for everything written to [path]'s disk so far:
   F_FULLFSYNC empties the whole drive's write cache, not just one file's. *)
let barrier path =
  let fd = Unix.openfile path [Unix.O_RDONLY] 0 in
  Fun.protect ~finally:(fun () -> Unix.close fd) (fun () -> N.fullfsync fd)

let close c =
  if c.journal = Wal then exec c.db "PRAGMA wal_checkpoint(TRUNCATE)";
  ignore (Sqlite3.db_close c.db)

let epoch c = match query c.db "SELECT value FROM unison_container WHERE key='epoch'" [] with
  | [ [| Sqlite3.Data.TEXT e |] ] -> int_of_string e | _ -> 0

(* The member table: one indexed query, no member data read. *)
let members c =
  List.map (fun r -> {
      name = to_string r.(0); size = to_int64 r.(1); mtime_ns = to_int64 r.(2); fp = to_string r.(3);
      fpalg = to_string r.(4); sha256 = to_string r.(5); retained = to_int64 r.(6) <> 0L })
    (query c.db "SELECT m.name, s.sz, m.mtime_ns, m.fp, m.fpalg, m.sha256, m.retained_deleted \
                 FROM unison_member m JOIN sqlar s ON s.name = m.name ORDER BY m.name" [])

let read c name = match query c.db "SELECT data FROM sqlar WHERE name = ?" [text name] with
  | [ [| d |] ] -> to_string d | _ -> raise Not_found

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

(* One batch: every put and retained deletion in transactions of at most
   [batch_bytes] of member data, the epoch bumped in each; then the changed
   blobs are read back and their SHA-256 compared, before the caller may
   record them anywhere. Files are read and hashed outside [write_lock]; only
   the transactions run inside it, so several processes can prepare
   containers while one writes to the disk. Returns the bytes stored. *)
let apply ?(batch_bytes = 256 * 1048576) ?(write_lock = fun f -> f ()) c ~puts ~retain =
  let stored = ref 0 and done_puts = ref 0 in
  let rec batches acc size cur = function
    | [] -> List.rev (if cur = [] then acc else List.rev cur :: acc)
    | p :: rest ->
        let n = Int64.to_int (N.stat p.source).N.size in
        if cur <> [] && size + n > batch_bytes then batches (List.rev cur :: acc) n [p] rest
        else batches acc (size + n) (p :: cur) rest in
  let groups = match batches [] 0 [] puts with [] -> [[]] | g -> g in
  let last = List.length groups - 1 in
  List.iteri (fun i group ->
    let ready = List.map read_put group in
    write_lock (fun () ->
      exec c.db "BEGIN IMMEDIATE";
      (try
         List.iter (fun r ->
           run c.db "INSERT OR REPLACE INTO sqlar(name, mode, mtime, sz, data) VALUES (?, ?, ?, ?, ?)"
             [text r.put.member; int64 (Int64.of_int (0o100000 lor r.st.N.mode));
              int64 (Int64.div r.st.N.mtime_ns 1_000_000_000L); int64 r.st.N.size; blob r.data];
           run c.db "INSERT OR REPLACE INTO unison_member(name, mtime_ns, fp, fpalg, sha256, retained_deleted) \
                     VALUES (?, ?, ?, ?, ?, 0)"
             [text r.put.member; int64 r.st.N.mtime_ns; blob r.md5; text fpalg; blob r.sha];
           stored := !stored + String.length r.data;
           incr done_puts;
           if crash_after = Some !done_puts then Unix._exit 3) ready;
         if i = last then
           List.iter (fun name -> run c.db "UPDATE unison_member SET retained_deleted = 1 WHERE name = ?" [text name]) retain;
         run c.db "UPDATE unison_container SET value = CAST(value AS INTEGER) + 1 WHERE key = 'epoch'" [];
         exec c.db "COMMIT"
       with e -> ignore (Sqlite3.exec c.db "ROLLBACK"); raise e))) groups;
  (* Read back what this batch wrote. *)
  List.iter (fun p ->
    match query c.db "SELECT s.data, m.sha256 FROM sqlar s JOIN unison_member m ON m.name = s.name WHERE s.name = ?"
            [text p.member] with
    | [ [| d; h |] ] when N.sha_string (to_string d) = to_string h -> ()
    | _ -> failwith (c.path ^ ": " ^ p.member ^ " did not read back")) puts;
  !stored

(* Problems found: the structure check, then every member's SHA-256 and the
   pairing of the two tables. *)
let verify c =
  let problems = ref [] in
  (match query c.db "PRAGMA integrity_check" [] with
   | [ [| Sqlite3.Data.TEXT "ok" |] ] -> ()
   | rows -> List.iter (fun r -> problems := ("integrity: " ^ to_string r.(0)) :: !problems) rows);
  List.iter (fun r ->
    let name = to_string r.(0) in
    match r.(1), r.(2) with
    | Sqlite3.Data.NULL, _ -> problems := (name ^ ": no unison_member row") :: !problems
    | h, d ->
        let d = to_string d in
        if N.sha_string d <> to_string h then problems := (name ^ ": SHA-256 mismatch") :: !problems
        else if Digest.string d <> to_string r.(3) then problems := (name ^ ": fingerprint mismatch") :: !problems)
    (query c.db "SELECT s.name, m.sha256, s.data, m.fp FROM sqlar s LEFT JOIN unison_member m ON m.name = s.name" []);
  List.iter (fun r -> problems := (to_string r.(0) ^ ": unison_member row without data") :: !problems)
    (query c.db "SELECT m.name FROM unison_member m LEFT JOIN sqlar s ON s.name = m.name WHERE s.name IS NULL" []);
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
let plan ?(skip = []) known dir =
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
    | _ -> Some { member = n; source = Filename.concat dir n }) names in
  let present = Hashtbl.create 256 in
  List.iter (fun n -> Hashtbl.replace present n ()) names;
  let retain = Hashtbl.fold (fun n m acc -> if Hashtbl.mem present n || m.retained then acc else n :: acc) have [] in
  puts, retain

(* Bring D.sqlar up to date with [dir]. Returns (stored, retained, bytes). *)
let update ?batch_bytes ?write_lock ?skip c dir =
  let puts, retain = plan ?skip (members c) dir in
  let bytes = if puts = [] && retain = [] then 0 else apply ?batch_bytes ?write_lock c ~puts ~retain in
  List.length puts, List.length retain, bytes

(* Write every member to [out] with its mode and nanosecond mtime, SHA-256
   checked; retained deletions too unless [skip_retained]. *)
let restore ?(skip_retained = false) c out =
  N.mkdir_p out;
  List.iter (fun m ->
    if not (skip_retained && m.retained) then begin
      let d = read c m.name in
      if N.sha_string d <> m.sha256 then failwith (m.name ^ ": SHA-256 mismatch");
      let target = Filename.concat out m.name in
      if Sys.file_exists target then failwith (target ^ " exists");
      Out_channel.with_open_bin target (fun oc -> output_string oc d);
      (match query c.db "SELECT mode FROM sqlar WHERE name = ?" [text m.name] with
       | [ [| Sqlite3.Data.INT mode |] ] -> Unix.chmod target (Int64.to_int mode land 0o7777)
       | _ -> ());
      N.utimens target m.mtime_ns m.mtime_ns
    end) (members c)

(* Rebuild [path] into a fresh container: the same members (retained ones too
   unless [drop_retained]), no free pages, the epoch carried on. The new file
   is verified and flushed to the platter before it replaces the old one, so
   a crash leaves one or the other whole. Returns (members kept, bytes before,
   bytes after). *)
let repack ?(drop_retained = false) ?(journal = Delete) path =
  let fresh = path ^ ".repack" in
  if Sys.file_exists fresh then failwith (fresh ^ " exists: an earlier repack was interrupted; check and remove it");
  let before = (N.stat path).N.size in
  let old = open_ ~journal ~create:false path in
  let kept = Fun.protect ~finally:(fun () -> close old) (fun () ->
    let problems = verify old in
    if problems <> [] then failwith (path ^ ": " ^ String.concat "; " problems ^ "; not repacked");
    let c = open_ ~journal fresh in
    Fun.protect ~finally:(fun () -> close c) (fun () ->
      let keep = List.filter (fun m -> not (drop_retained && m.retained)) (members old) in
      exec c.db "BEGIN IMMEDIATE";
      (try
         List.iter (fun m ->
           (match query old.db "SELECT mode, mtime, sz, data FROM sqlar WHERE name = ?" [text m.name] with
            | [ [| mode; mtime; sz; data |] ] ->
                run c.db "INSERT INTO sqlar(name, mode, mtime, sz, data) VALUES (?, ?, ?, ?, ?)" [text m.name; mode; mtime; sz; data]
            | _ -> failwith (m.name ^ ": no data"));
           run c.db "INSERT INTO unison_member(name, mtime_ns, fp, fpalg, sha256, retained_deleted) VALUES (?, ?, ?, ?, ?, ?)"
             [text m.name; int64 m.mtime_ns; blob m.fp; text m.fpalg; blob m.sha256; int64 (if m.retained then 1L else 0L)]) keep;
         run c.db "UPDATE unison_container SET value = ? WHERE key = 'epoch'" [text (string_of_int (epoch old + 1))];
         exec c.db "COMMIT"
       with e -> ignore (Sqlite3.exec c.db "ROLLBACK"); raise e);
      let problems = verify c in
      if problems <> [] then failwith (fresh ^ ": " ^ String.concat "; " problems);
      List.length keep)) in
  barrier fresh;
  Sys.rename fresh path;
  kept, before, (N.stat path).N.size


(* Name -> mode of every member, from the sqlar table (for ls -l and stat). *)
let modes c =
  let t = Hashtbl.create 64 in
  List.iter (fun r -> match r with
    | [| Sqlite3.Data.TEXT n; Sqlite3.Data.INT m |] -> Hashtbl.replace t n (Int64.to_int m)
    | _ -> ()) (query c.db "SELECT name, mode FROM sqlar" []);
  t

(* One transaction that changes the member set, bumping the epoch so that
   Unison sees the container changed outside it. *)
let change ?(write_lock = fun f -> f ()) c f =
  write_lock (fun () ->
    exec c.db "BEGIN IMMEDIATE";
    (try
       f ();
       run c.db "UPDATE unison_container SET value = CAST(value AS INTEGER) + 1 WHERE key = 'epoch'" [];
       exec c.db "COMMIT"
     with e -> ignore (Sqlite3.exec c.db "ROLLBACK"); raise e))

(* Delete members, bytes and all (rm). *)
let remove ?write_lock c names =
  change ?write_lock c (fun () ->
    List.iter (fun n ->
      run c.db "DELETE FROM unison_member WHERE name = ?" [text n];
      run c.db "DELETE FROM sqlar WHERE name = ?" [text n]) names)

(* Rename a member within its container (mv); refuses an existing target. *)
let rename ?write_lock c ~src ~dst =
  if String.contains dst '/' then failwith ("member name with a slash: " ^ dst);
  change ?write_lock c (fun () ->
    if query c.db "SELECT 1 FROM sqlar WHERE name = ?" [text dst] <> [] then failwith (dst ^ " exists");
    run c.db "UPDATE sqlar SET name = ? WHERE name = ?" [text dst; text src];
    run c.db "UPDATE unison_member SET name = ? WHERE name = ?" [text dst; text src])
