(* The packdir command line, shared by packdir and unison-packdir.

     packdir pack-tree ROOT OUT [--depth N] [--workers N] [--wal] [--batch-mib M]
                                [--engine sqlite|turso] [--mvcc] [--writers N] [--clones]
       keep every directory N levels under ROOT (default 2, the preview leaves
       <H>/<HHHH>) as OUT/<rel>.sqlar; N workers read and hash containers in
       parallel, one writes at a time; a rerun stores only new and changed
       files and marks the vanished ones retained. --engine turso writes with
       Turso; --mvcc takes off the writer lock between containers, and --writers
       writes the batches of one container concurrently; --clones keeps each
       APFS clone of another file in the tree as a reference to it
     packdir list C.sqlar
     packdir verify C.sqlar...
     packdir restore C.sqlar OUT_DIR [--skip-retained]
     packdir unpack-tree OUT DEST [--skip-retained]
       every container under OUT back to a plain tree under DEST
     packdir repack C.sqlar... [--drop-retained] [--engine sqlite|turso]
       rebuild containers compactly, verified before they replace the old ones;
       --engine turso gives a container made by SQLite the form Turso can write *)

let prog () = Filename.basename Sys.argv.(0)

let usage () =
  prerr_endline "usage: packdir pack-tree ROOT OUT [--depth N] [--workers N] [--wal] [--batch-mib M]\n\
                \                 [--engine sqlite|turso] [--mvcc] [--writers N] [--clones]\n\
                \       packdir list C.sqlar\n\
                \       packdir verify C.sqlar...\n\
                \       packdir restore C.sqlar OUT_DIR [--skip-retained]\n\
                \       packdir unpack-tree OUT DEST [--skip-retained]\n\
                \       packdir repack C.sqlar... [--drop-retained] [--engine sqlite|turso]\n\
                \       packdir ls|cat|stat|du|find|rm|cp|mv|mkdir [GNU options] PATH...";
  exit 2

let rec dirs_at depth root rel =
  if depth = 0 then [rel] else
  let here = if rel = "" then root else Filename.concat root rel in
  List.concat_map (fun n ->
    let r = if rel = "" then n else Filename.concat rel n in
    match (Unix.lstat (Filename.concat root r)).Unix.st_kind with
    | Unix.S_DIR -> dirs_at (depth - 1) root r
    | _ -> [] | exception Unix.Unix_error _ -> [])
    (List.sort compare (Array.to_list (Sys.readdir here)))

(* One writer per output disk: the transactions of all workers take this lock;
   reading and hashing happen outside it. *)
let lock_file out =
  Native.mkdir_p out;
  Unix.openfile (Filename.concat out ".packdir-writer.lock") [Unix.O_RDWR; Unix.O_CREAT] 0o644

let with_lock fd f =
  Unix.lockf fd Unix.F_LOCK 0;
  Fun.protect ~finally:(fun () -> Unix.lockf fd Unix.F_ULOCK 0) f

(* OUT/.packdir-state: for each container, its file's (inode, size, mtime)
   after the last run and its members then. A stand-in for Unison's archive,
   in a text form the Rust build shares; losing it, or an older form, only
   costs a full re-read.
     packdir-state/1
     C <rel> <ino> <size> <mtime>
     M <name> <size> <mtime_ns> <fp hex> <fpalg> <sha256> <0|1>
   with tabs between fields and % escaping % tab newline in names. *)
let state_file out = Filename.concat out ".packdir-state"
let state_magic = "packdir-state/1"

let escape s =
  let b = Buffer.create (String.length s) in
  String.iter (function
    | '%' -> Buffer.add_string b "%25" | '\t' -> Buffer.add_string b "%09"
    | '\n' -> Buffer.add_string b "%0A" | c -> Buffer.add_char b c) s;
  Buffer.contents b

let unescape s =
  let b = Buffer.create (String.length s) and i = ref 0 and n = String.length s in
  while !i < n do
    if s.[!i] = '%' && !i + 2 < n then begin
      Buffer.add_char b (Char.chr (int_of_string ("0x" ^ String.sub s (!i + 1) 2))); i := !i + 3
    end else (Buffer.add_char b s.[!i]; incr i)
  done;
  Buffer.contents b

let load_state out : (string, (int64 * int64 * int64) * Container.member list) Hashtbl.t =
  let t = Hashtbl.create 1024 in
  (try
     match In_channel.with_open_bin (state_file out) In_channel.input_lines with
     | magic :: lines when magic = state_magic ->
         let current = ref None in
         let finish () = Option.iter (fun (rel, st, ms) -> Hashtbl.replace t rel (st, List.rev ms)) !current in
         List.iter (fun line ->
           match String.split_on_char '\t' line with
           | ["C"; rel; ino; size; mtime] ->
               finish ();
               current := Some (unescape rel, (Int64.of_string ino, Int64.of_string size, Int64.of_string mtime), [])
           | ["M"; name; size; mtime_ns; fp; fpalg; sha256; retained] ->
               let m = { Container.name = unescape name; size = Int64.of_string size; mtime_ns = Int64.of_string mtime_ns;
                         fp = Digest.from_hex fp; fpalg; sha256; retained = retained = "1" } in
               current := Option.map (fun (rel, st, ms) -> rel, st, m :: ms) !current
           | _ -> failwith "bad state line") lines;
         finish ()
     | _ -> ()
   with Sys_error _ | End_of_file | Failure _ | Invalid_argument _ -> Hashtbl.reset t);
  t

let save_state out state =
  let temp = state_file out ^ ".tmp" in
  Out_channel.with_open_bin temp (fun oc ->
    output_string oc (state_magic ^ "\n");
    List.iter (fun (rel, ((ino, size, mtime), members)) ->
      Printf.fprintf oc "C\t%s\t%Ld\t%Ld\t%Ld\n" (escape rel) ino size mtime;
      List.iter (fun (m : Container.member) ->
        Printf.fprintf oc "M\t%s\t%Ld\t%Ld\t%s\t%s\t%s\t%d\n" (escape m.name) m.size m.mtime_ns
          (Digest.to_hex m.fp) m.fpalg m.sha256 (if m.retained then 1 else 0)) members)
      (List.sort compare (Hashtbl.fold (fun k v acc -> (k, v) :: acc) state [])));
  Sys.rename temp (state_file out)

(* OUT/.packdir-pending: containers written since the last barrier. Each is
   added before its first write and the list is cleared only after the
   run's final barrier and state save, so after a crash or power loss the next
   run verifies exactly the containers that may not have reached the platter. *)
let pending_file out = Filename.concat out ".packdir-pending"
let load_pending out =
  let t = Hashtbl.create 64 in
  (try List.iter (fun l -> if l <> "" then Hashtbl.replace t l ())
         (In_channel.with_open_bin (pending_file out) In_channel.input_lines)
   with Sys_error _ -> ());
  t
let add_pending out rel =
  Out_channel.with_open_gen [Open_wronly; Open_creat; Open_append; Open_binary] 0o644 (pending_file out) (fun oc ->
    output_string oc (rel ^ "\n"); flush oc; Unix.fsync (Unix.descr_of_out_channel oc))
let clear_pending out =
  Out_channel.with_open_bin (pending_file out ^ ".tmp") (fun _ -> ());
  Sys.rename (pending_file out ^ ".tmp") (pending_file out)

(* --clones: APFS clone groups among the files under the containers' leaves,
   as (container rel, name) lists in path order, and, for each container,
   the group members that are new or changed since the state: those are
   stored after the parallel pass, the first of a group with no current
   member as data, the rest as references to its original. *)
let clone_groups root dirs state =
  let by_key = Hashtbl.create 1024 in
  Array.iter (fun rel ->
    let dir = Filename.concat root rel in
    List.iter (fun n ->
      match Native.clone_key (Filename.concat dir n) with
      | Some k -> Hashtbl.replace by_key k ((rel, n) :: Option.value (Hashtbl.find_opt by_key k) ~default:[])
      | None -> ()) (try List.sort compare (Array.to_list (Sys.readdir dir)) with Sys_error _ -> [])) dirs;
  let groups = Hashtbl.fold (fun _ g acc -> if List.length g > 1 then List.sort compare g :: acc else acc) by_key [] in
  let defer = Hashtbl.create 256 in
  List.iter (List.iter (fun (rel, n) ->
    let st = Native.stat (Filename.concat (Filename.concat root rel) n) in
    let current = match Hashtbl.find_opt state rel with
      | Some (_, ms) -> List.exists (fun (m : Container.member) ->
          m.name = n && m.size = st.Native.size && m.mtime_ns = st.Native.mtime_ns && not m.retained) ms
      | None -> false in
    if not current then Hashtbl.replace defer rel (n :: Option.value (Hashtbl.find_opt defer rel) ~default:[]))) groups;
  List.sort compare groups, defer

(* The serial pass for --clones. For each group with deferred members, the
   original is its first member held as current data, or a current clone's
   original; failing both, the first deferred member is stored as data and
   becomes the original. The other deferred members are stored as clones of
   it, a container at a time, and a container whose clones don't check out
   (a file changed since the scan) stores them as data instead. Returns the
   containers touched, and (stored, bytes, clones). *)
let store_clones root out groups defer ~journal ~engine ~write_lock ~txn_lock ~add_pending =
  let logical rel n = Filename.concat (Filename.concat out rel) n in
  let opened = Hashtbl.create 64 in
  let info rel = match Hashtbl.find_opt opened rel with
    | Some i -> i
    | None ->
        let path = Filename.concat out rel ^ ".sqlar" in
        let i = if not (Sys.file_exists path) then [], [] else begin
            let c = Container.open_ ~create:false ~read_only:true ~engine path in
            Fun.protect ~finally:(fun () -> Container.close c) (fun () -> Container.members c, Container.clones c)
          end in
        Hashtbl.replace opened rel i; i in
  let current rel n =
    let ms, cs = info rel in
    let st = Native.stat (Filename.concat (Filename.concat root rel) n) in
    match List.find_opt (fun (m : Container.member) -> m.name = n) ms with
    | Some m when m.size = st.Native.size && m.mtime_ns = st.Native.mtime_ns && not m.retained ->
        (match List.find_opt (fun (name, _, _) -> name = n) cs with
         | Some (_, original, _) -> Some (Container.normalize (Filename.concat (Filename.concat out rel) original))
         | None -> Some (logical rel n))
    | _ -> None in
  let data = Hashtbl.create 64 and clones = Hashtbl.create 64 in
  let add t rel x = Hashtbl.replace t rel (x :: Option.value (Hashtbl.find_opt t rel) ~default:[]) in
  List.iter (fun group ->
    let deferred = List.filter (fun (rel, n) ->
      List.mem n (Option.value (Hashtbl.find_opt defer rel) ~default:[]) && current rel n = None) group in
    if deferred <> [] then begin
      let original, rest = match List.find_map (fun (rel, n) -> current rel n) group with
        | Some o -> o, deferred
        | None -> let (rel, n) = List.hd deferred in add data rel n; logical rel n, List.tl deferred in
      List.iter (fun (rel, n) ->
        add clones rel { Container.clone = n; file = Filename.concat (Filename.concat root rel) n;
                         original = Container.relative ~from:(Filename.concat out rel) original }) rest
    end) groups;
  let touched = Hashtbl.create 64 and stored = ref 0 and bytes = ref 0 and kept = ref 0 in
  let with_container rel f =
    let target = Filename.concat out rel ^ ".sqlar" in
    write_lock (fun () -> add_pending rel);
    Hashtbl.replace touched rel ();
    let c = ref None in
    write_lock (fun () -> Native.mkdir_p (Filename.dirname target); c := Some (Container.open_ ~journal ~engine target));
    let c = Option.get !c in
    Fun.protect ~finally:(fun () -> Container.close c) (fun () -> f c) in
  let store_data rel names =
    with_container rel (fun c ->
      let puts = List.map (fun n -> { Container.member = n; source = Filename.concat (Filename.concat root rel) n }) names in
      bytes := !bytes + Container.apply ~write_lock:txn_lock c ~puts ~retain:[];
      stored := !stored + List.length puts) in
  Hashtbl.iter (fun rel names -> store_data rel (List.rev names)) data;
  Hashtbl.iter (fun rel cps ->
    let cps = List.rev cps in
    match with_container rel (fun c -> Container.apply_clones ~write_lock:txn_lock c cps) with
    | _ -> stored := !stored + List.length cps; kept := !kept + List.length cps
    | exception (Failure e | Tursolite.Error { message = e; _ }) ->
        Printf.printf "note: clones in %s stored as data (%s)\n%!" rel e;
        store_data rel (List.map (fun (cp : Container.clone_put) -> cp.clone) cps)) clones;
  Hashtbl.fold (fun rel () acc -> rel :: acc) touched [], (!stored, !bytes, !kept)

let pack_tree ?(clones = false) root out ~depth ~journal ~engine ~batch_bytes ~workers ~writers ~barrier_bytes =
  let t0 = Unix.gettimeofday () in
  let state = load_state out in
  let suspect = load_pending out in
  let since_barrier = ref 0 and barriers = ref 0 in
  let barrier () = Container.barrier (Filename.concat out ".packdir-writer.lock"); incr barriers; since_barrier := 0 in
  let opened = ref 0 and scan_seconds = ref 0. in
  let fd = lock_file out in
  let write_lock f = with_lock fd f in
  (* MVCC writes containers side by side; everything else takes turns. *)
  let txn_lock = if journal = Container.Mvcc then (fun f -> f ()) else write_lock in
  let dirs = Array.of_list (dirs_at depth root "") in
  let groups, defer = if clones then clone_groups root dirs state else [], Hashtbl.create 1 in
  let deferred rel = Option.value (Hashtbl.find_opt defer rel) ~default:[] in
  let one rel =
    let target = Filename.concat out rel ^ ".sqlar" in
    (* A plain directory beside the container is pre-existing content, never
       touched. A file named like one of its files is not packed while the
       bytes are the same (present-plain); once they differ, the newer bytes
       are kept updated in the container beside the plain original (user,
       2026-10-31). *)
    let plain = Filename.concat out rel in
    let same = if Sys.file_exists plain && Sys.is_directory plain then begin
        Printf.printf "note: plain %s/ beside %s.sqlar is left as it is\n" rel rel;
        List.filter (fun n ->
          let mine = Filename.concat (Filename.concat root rel) n and theirs = Filename.concat plain n in
          let module N = Native in
          Sys.file_exists mine && not (Sys.is_directory theirs) &&
          if (N.stat mine).N.size = (N.stat theirs).N.size && N.sha_file mine = N.sha_file theirs then
            (Printf.printf "present-plain: %s/%s\n" rel n; true)
          else (Printf.printf "packed-beside-plain: %s/%s (the plain original differs and stays)\n" rel n; false))
          (Array.to_list (Sys.readdir plain))
      end else [] in
    let skip = same in
    (* A container whose file is as recorded after the last run needs no
       opening: its members come from the state, the role Unison's archive
       plays. Only containers with something to store are opened. *)
    let stamp () = match Native.stat target with
      | s -> Some (s.Native.ino, s.Native.size, s.Native.mtime_ns)
      | exception Unix.Unix_error _ -> None in
    let src = Filename.concat root rel in
    (* The scan: this container's stamp, and its plan against the plain side. *)
    let t = Unix.gettimeofday () in
    (* Written since the last barrier of an earlier run, or changed outside
       the tool: verified before it is trusted. *)
    let suspicious = Sys.file_exists target &&
      (Hashtbl.mem suspect rel || match Hashtbl.find_opt state rel with
        | Some (st, _) -> Some st <> stamp () | None -> false) in
    let unchanged = not suspicious && match Hashtbl.find_opt state rel with
      | Some (st, known) -> Some st = stamp () && Container.plan ~skip ~defer:(deferred rel) known src = ([], [])
      | None -> false in
    scan_seconds := !scan_seconds +. (Unix.gettimeofday () -. t);
    if unchanged then (0, 0, 0), None
    else begin
        incr opened;
        if suspicious then begin
          let problems = let c = Container.open_ ~journal ~engine target in
            Fun.protect ~finally:(fun () -> Container.close c) (fun () -> Container.verify c) in
          if problems <> [] then begin
            (* Never deleted: set aside, and the leaf packed again from the plain side. *)
            let aside = Printf.sprintf "%s.damaged-%.0f" target (Unix.time ()) in
            write_lock (fun () -> Sys.rename target aside);
            Printf.printf "damaged: %s (%s); set aside as %s and packed again\n%!" rel (String.concat "; " problems) aside
          end
        end;
        write_lock (fun () -> add_pending out rel);
        let c = write_lock (fun () -> Native.mkdir_p (Filename.dirname target); Container.open_ ~journal ~engine target) in
        let result, members = Fun.protect ~finally:(fun () -> Container.close c) (fun () ->
          let r = Container.update ~batch_bytes ~write_lock:txn_lock ~writers ~skip ~defer:(deferred rel) c src in r, Container.members c) in
        let _, _, b = result in
        since_barrier := !since_barrier + b;
        if !since_barrier >= barrier_bytes then write_lock barrier;
        result, Option.map (fun st -> rel, (st, members)) (stamp ())
    end in
  let containers = Array.length dirs and stored = ref 0 and retained = ref 0 and bytes = ref 0 in
  let updates = ref [] in
  let add ((s, r, b), u) =
    stored := !stored + s; retained := !retained + r; bytes := !bytes + b;
    Option.iter (fun u -> updates := u :: !updates) u in
  if workers <= 1 then Array.iter (fun rel -> add (one rel)) dirs
  else begin
    (* Worker k takes containers k, k+workers, ... and sends back its totals. *)
    flush stdout;
    let children = List.init (min workers (max 1 containers)) (fun k ->
      let rd, wr = Unix.pipe ~cloexec:true () in
      match Unix.fork () with
      | 0 ->
          Unix.close rd;
          let total = ref ((0, 0, 0), []) and i = ref k in
          (try
             while !i < containers do
               let (s, r, b), u = one dirs.(!i) and ((s0, r0, b0), us) = !total in
               total := ((s0 + s, r0 + r, b0 + b), Option.to_list u @ us); i := !i + workers
             done
           with e -> prerr_endline (prog () ^ ": " ^ Printexc.to_string e); flush stdout; Unix._exit 1);
          let oc = Unix.out_channel_of_descr wr in
          Marshal.to_channel oc (!total, !opened, !scan_seconds, !barriers) []; close_out oc; flush stdout; Unix._exit 0
      | pid -> Unix.close wr; pid, Unix.in_channel_of_descr rd) in
    let failed = ref 0 in
    List.iter (fun (pid, ic) ->
      (match Marshal.from_channel ic with
       | ((t, us), o, sc, b) ->
           add (t, None); updates := us @ !updates; opened := !opened + o;
           scan_seconds := !scan_seconds +. sc; barriers := !barriers + b
       | exception End_of_file -> ());
      close_in ic;
      match snd (Unix.waitpid [] pid) with
      | Unix.WEXITED 0 -> () | Unix.WEXITED n -> failed := n | _ -> failed := 1) children;
    if !failed <> 0 then exit !failed
  end;
  let kept = ref 0 in
  if groups <> [] then begin
    let touched, (s, b, k) = store_clones root out groups defer ~journal ~engine ~write_lock ~txn_lock
        ~add_pending:(add_pending out) in
    stored := !stored + s; bytes := !bytes + b; kept := k;
    List.iter (fun rel ->
      let target = Filename.concat out rel ^ ".sqlar" in
      let c = Container.open_ ~create:false ~read_only:true ~engine target in
      let ms = Fun.protect ~finally:(fun () -> Container.close c) (fun () -> Container.members c) in
      let s = Native.stat target in
      (* After the parallel pass's entries, so these, newer, win. *)
      updates := !updates @ [rel, ((s.Native.ino, s.Native.size, s.Native.mtime_ns), ms)]) touched
  end;
  (* The final barrier, then the state; only then are the pending containers
     known to be on the platter. *)
  write_lock barrier;
  List.iter (fun (rel, e) -> Hashtbl.replace state rel e) !updates;
  save_state out state;
  write_lock barrier;
  clear_pending out;
  let dt = Unix.gettimeofday () -. t0 in
  (* scan_seconds is summed over workers: the time spent stating containers
     and planning against the plain side, apart from opening and writing. *)
  Printf.printf "{\"barriers\": %d, \"containers\": %d, \"opened\": %d, \"stored\": %d, \"retained\": %d, \"bytes\": %d, \"seconds\": %.3f, \"scan_seconds\": %.3f, \"mib_per_s\": %.1f, \"files_per_s\": %.1f%s}\n"
    !barriers containers !opened !stored !retained !bytes dt !scan_seconds (float !bytes /. 1048576. /. dt) (float !stored /. dt)
    (if clones then Printf.sprintf ", \"clones\": %d" !kept else "")

let main () =
  (* GNU-style file commands over packed trees: packdir ls -l ROOT ... *)
  (match List.tl (Array.to_list Sys.argv) with
   | cmd :: rest when List.mem_assoc cmd Fileutils.commands ->
       (try (List.assoc cmd Fileutils.commands) rest with
        | Failure e | Sys_error e | Tursolite.Error { message = e; _ } -> Fileutils.err "%s" e
        | Unix.Unix_error (e, f, a) -> Fileutils.err "%s: %s %s" f (Unix.error_message e) a);
       exit !Fileutils.status
   | _ -> ());
  let args = List.tl (Array.to_list Sys.argv) in
  let flag f = List.mem f args in
  let value f d = let rec go = function a :: v :: _ when a = f -> v | _ :: r -> go r | [] -> d in go args in
  let pos = let rec go = function
      | [] -> [] | ("--depth" | "--batch-mib" | "--workers" | "--barrier-gib" | "--engine" | "--writers") :: _ :: r -> go r
      | a :: r when String.length a > 2 && String.sub a 0 2 = "--" -> go r | a :: r -> a :: go r in go args in
  let engine = match value "--engine" "" with "" -> Container.default_engine () | e -> Tursolite.parse_engine e in
  let open_read path = Container.open_ ~create:false ~read_only:true ~engine path in
  try match pos with
    | ["pack-tree"; root; out] ->
        pack_tree ~clones:(flag "--clones") root out ~depth:(int_of_string (value "--depth" "2"))
          ~journal:(if flag "--mvcc" then Container.Mvcc else if flag "--wal" then Container.Wal else Container.Delete)
          ~engine:(if flag "--mvcc" then Tursolite.Turso else engine)
          ~writers:(int_of_string (value "--writers" "1"))
          ~batch_bytes:(int_of_string (value "--batch-mib" "256") * 1048576)
          ~workers:(int_of_string (value "--workers" "1"))
          ~barrier_bytes:(int_of_string (value "--barrier-gib" "4") * 1073741824)
    | ["list"; path] ->
        let c = open_read path in
        List.iter (fun (m : Container.member) ->
          Printf.printf "%s\t%Ld\t%Ld\t%s%s\n" m.name m.size m.mtime_ns (m.sha256)
            (if m.retained then "\tretained" else "")) (Container.members c);
        Container.close c
    | "verify" :: (_ :: _ as paths) ->
        let bad = List.fold_left (fun bad p ->
          let c = open_read p in
          let problems = Container.verify c in
          Container.close c;
          List.iter (fun e -> Printf.printf "%s: %s\n" p e) problems;
          if problems = [] then Printf.printf "%s: ok, %d members\n" p (let c = open_read p in Fun.protect ~finally:(fun () -> Container.close c) (fun () -> List.length (Container.members c)));
          bad + List.length problems) 0 paths in
        if bad > 0 then exit 1
    | ["unpack-tree"; out; dest] ->
        (* Every container under OUT back to plain files under DEST: OUT/<rel>.sqlar
           becomes DEST/<rel>/, with exact bytes, modes and nanosecond mtimes. *)
        let rec containers dir rel =
          List.concat_map (fun n ->
            let r = if rel = "" then n else Filename.concat rel n in
            let p = Filename.concat dir n in
            match (Unix.lstat p).Unix.st_kind with
            | Unix.S_DIR -> containers p r
            | Unix.S_REG when Filename.check_suffix n ".sqlar" -> [Filename.chop_suffix r ".sqlar"]
            | _ -> []) (List.sort compare (Array.to_list (Sys.readdir dir))) in
        let found = containers out "" in
        let files = ref 0 and restored = Hashtbl.create 4096 and jobs = ref [] in
        (* Every file that holds data first, then the clones, from the
           originals just restored. *)
        List.iter (fun rel ->
          let c = open_read (Filename.concat out rel ^ ".sqlar") in
          Fun.protect ~finally:(fun () -> Container.close c) (fun () ->
            let written, j = Container.restore_data ~skip_retained:(flag "--skip-retained") c (Filename.concat dest rel) in
            List.iter (fun (k, f) -> Hashtbl.replace restored k f) written;
            jobs := List.rev_append j !jobs;
            files := !files + List.length written + List.length j)) found;
        let cloned, copied = Container.restore_clones ~restored (List.rev !jobs) in
        if !jobs = [] then Printf.printf "{\"containers\": %d, \"files\": %d}\n" (List.length found) !files
        else Printf.printf "{\"containers\": %d, \"files\": %d, \"cloned\": %d, \"copied\": %d}\n"
            (List.length found) !files cloned copied
    | "repack" :: (_ :: _ as paths) ->
        List.iter (fun p ->
          let kept, before, after = Container.repack ~drop_retained:(flag "--drop-retained") ~engine p in
          Printf.printf "%s: %d members, %Ld -> %Ld bytes\n" p kept before after) paths
    | ["restore"; path; out] ->
        let c = open_read path in
        Fun.protect ~finally:(fun () -> Container.close c) (fun () ->
          ignore (Container.restore ~skip_retained:(flag "--skip-retained") c out))
    | _ -> usage ()
  with Failure e | Sys_error e | Tursolite.Error { message = e; _ } -> prerr_endline (prog () ^ ": " ^ e); exit 1
     | Unix.Unix_error (e, f, a) -> Printf.eprintf "%s: %s: %s %s\n" (prog ()) f (Unix.error_message e) a; exit 1
