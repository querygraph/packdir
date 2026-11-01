(* GNU fileutils-style commands over a tree with packed directories: a path
   ROOT/A/B/name reads through ROOT/A/B.sqlar when the plain directory
   ROOT/A/B/ holds no file of that name, so a packed tree is listed, read,
   copied, moved and removed as if it were plain (see the README).
   Options and output formats follow GNU coreutils 9; sorting is by bytes, as
   GNU does with LC_ALL=C. Commands that change a container (rm, mv, cp into
   one) bump its epoch, so Unison sees the change; don't run them while Unison
   writes the same tree. *)

module N = Native

(* --- The packed view -------------------------------------------------- *)

let companion n =
  Filename.check_suffix n ".sqlar" || Filename.check_suffix n ".sqlar-journal" ||
  Filename.check_suffix n ".sqlar.repack" || n = ".unison-packed" ||
  (let k = ".sqlar.damaged-" in
   let rec has i = i + String.length k <= String.length n && (String.sub n i (String.length k) = k || has (i + 1)) in
   has 0)

let lstat_opt p = try Some (N.stat ~follow:false p) with Unix.Unix_error _ -> None
let container_of dir = let c = dir ^ ".sqlar" in match lstat_opt c with Some s when s.N.kind = `Reg -> Some c | _ -> None

type entry = {
  path : string; kind : [ `Dir | `File | `Link ]; size : int64; mode : int; mtime_ns : int64;
  uid : int; gid : int; nlink : int;
  where : [ `Plain | `Member of string * string ]; (* container, member name *)
}

let with_container c f = let k = Container.open_ c in Fun.protect ~finally:(fun () -> Container.close k) (fun () -> f k)

let show_retained = ref false

(* Members of the container standing for [dir], with their modes. *)
let members dir =
  match container_of dir with
  | None -> []
  | Some c -> with_container c (fun k ->
      let modes = Container.modes k in
      List.filter_map (fun (m : Container.member) ->
        if m.retained && not !show_retained then None
        else Some (c, m, Option.value (Hashtbl.find_opt modes m.name) ~default:0o100644)) (Container.members k))

let plain_entry p (s : N.stat) =
  { path = p; kind = (match s.N.kind with `Dir -> `Dir | `Link -> `Link | _ -> `File); size = s.N.size;
    mode = s.N.mode; mtime_ns = s.N.mtime_ns; uid = s.N.uid; gid = s.N.gid; nlink = 1; where = `Plain }

let member_entry p c (m : Container.member) mode =
  let cs = N.stat c in
  { path = p; kind = `File; size = m.size; mode = mode land 0o7777; mtime_ns = m.mtime_ns;
    uid = cs.N.uid; gid = cs.N.gid; nlink = 1; where = `Member (c, m.name) }

(* What [p] is in the packed view: a member wins over a plain file of the
   same name beside its container, as in Unison. *)
let lookup p =
  let dir = Filename.dirname p and name = Filename.basename p in
  let member = List.find_opt (fun (_, (m : Container.member), _) -> m.name = name) (members dir) in
  match member, lstat_opt p with
  | Some (c, m, mode), _ -> Some (member_entry p c m mode)
  | None, Some s -> Some (plain_entry p s)
  | None, None ->
      (match container_of p with
       | Some c -> let s = N.stat c in Some { (plain_entry p s) with kind = `Dir; mode = 0o755 }
       | None -> None)

let is_dir p = match lookup p with Some e -> e.kind = `Dir | None -> false

(* Names under a directory: plain entries (a container X.sqlar as X, its
   companions hidden) and the members of the directory's own container. *)
let children dir =
  let plain = try Array.to_list (Sys.readdir dir) with Sys_error _ -> [] in
  let names = List.filter_map (fun n ->
    if Filename.check_suffix n ".sqlar" then Some (Filename.chop_suffix n ".sqlar")
    else if companion n then None else Some n) plain in
  List.sort_uniq compare (names @ List.map (fun (_, (m : Container.member), _) -> m.name) (members dir))

let read e = match e.where with
  | `Plain -> In_channel.with_open_bin e.path In_channel.input_all
  | `Member (c, name) -> with_container c (fun k -> Container.read k name)

(* --- Options ---------------------------------------------------------- *)

let prog = ref "packdir"
let status = ref 0
let err fmt = Printf.ksprintf (fun m -> Printf.eprintf "%s: %s\n%!" !prog m; status := 1) fmt
let quote s = "'" ^ s ^ "'"

(* Long options that take a value, as --name=VALUE or --name VALUE. *)
let long_value = ["format"; "printf"; "sort"; "time-style"; "max-depth"; "target-directory"]

(* GNU-style arguments: -abc bundles, --long and --long=VALUE, -- ends options. *)
let parse ~short_value args =
  let opts = ref [] and pos = ref [] in
  let rec go = function
    | [] -> ()
    | "--" :: rest -> pos := List.rev_append rest !pos
    | a :: rest when String.length a > 2 && String.sub a 0 2 = "--" ->
        (match String.index_opt a '=', rest with
         | Some i, _ ->
             opts := (String.sub a 2 (i - 2), Some (String.sub a (i + 1) (String.length a - i - 1))) :: !opts; go rest
         | None, v :: rest' when List.mem (String.sub a 2 (String.length a - 2)) long_value ->
             opts := (String.sub a 2 (String.length a - 2), Some v) :: !opts; go rest'
         | None, _ -> opts := (String.sub a 2 (String.length a - 2), None) :: !opts; go rest)
    | a :: rest when String.length a > 1 && a.[0] = '-' ->
        let rec flags i rest =
          if i >= String.length a then go rest else
          let c = String.make 1 a.[i] in
          if List.mem c short_value then
            if i + 1 < String.length a then (opts := (c, Some (String.sub a (i + 1) (String.length a - i - 1))) :: !opts; go rest)
            else (match rest with v :: rest -> opts := (c, Some v) :: !opts; go rest | [] -> err "option requires an argument -- '%s'" c)
          else (opts := (c, None) :: !opts; flags (i + 1) rest) in
        flags 1 rest
    | a :: rest -> pos := a :: !pos; go rest in
  go args;
  List.rev !opts, List.rev !pos

let has opts names = List.exists (fun (k, _) -> List.mem k names) opts
let value opts names = List.fold_left (fun acc (k, v) -> if List.mem k names then v else acc) None opts

(* --- Formatting ------------------------------------------------------- *)

let mode_string kind mode =
  let b = Bytes.make 10 '-' in
  Bytes.set b 0 (match kind with `Dir -> 'd' | `Link -> 'l' | `File -> '-');
  let bit i c = if mode land (1 lsl (8 - i)) <> 0 then Bytes.set b (i + 1) c in
  List.iteri (fun i c -> bit i c) ['r'; 'w'; 'x'; 'r'; 'w'; 'x'; 'r'; 'w'; 'x'];
  let special mask pos lo up = if mode land mask <> 0 then
      Bytes.set b pos (if Bytes.get b pos = 'x' then lo else up) in
  special 0o4000 3 's' 'S'; special 0o2000 6 's' 'S'; special 0o1000 9 't' 'T';
  Bytes.to_string b

let user uid = try (Unix.getpwuid uid).Unix.pw_name with Not_found -> string_of_int uid
let group gid = try (Unix.getgrgid gid).Unix.gr_name with Not_found -> string_of_int gid

(* The local offset, east of UTC, in seconds, at time [t]. *)
let utc_offset t =
  let lt = Unix.localtime t and gt = Unix.gmtime t in
  let days = (lt.Unix.tm_year - gt.Unix.tm_year) * 366 + (lt.Unix.tm_yday - gt.Unix.tm_yday) in
  let days = if days > 1 then -1 else if days < -1 then 1 else days in
  ((days * 24 + lt.Unix.tm_hour - gt.Unix.tm_hour) * 60 + lt.Unix.tm_min - gt.Unix.tm_min) * 60

let months = [| "Jan"; "Feb"; "Mar"; "Apr"; "May"; "Jun"; "Jul"; "Aug"; "Sep"; "Oct"; "Nov"; "Dec" |]

(* GNU's full-iso: 2026-10-31 13:52:01.123456789 -0700 *)
let full_iso ns =
  let s = Int64.to_float (Int64.div ns 1_000_000_000L) and frac = Int64.rem ns 1_000_000_000L in
  let s, frac = if frac < 0L then s -. 1., Int64.add frac 1_000_000_000L else s, frac in
  let tm = Unix.localtime s and off = utc_offset s in
  Printf.sprintf "%04d-%02d-%02d %02d:%02d:%02d.%09Ld %c%02d%02d" (tm.Unix.tm_year + 1900) (tm.Unix.tm_mon + 1)
    tm.Unix.tm_mday tm.Unix.tm_hour tm.Unix.tm_min tm.Unix.tm_sec frac (if off < 0 then '-' else '+')
    (abs off / 3600) (abs off mod 3600 / 60)

(* GNU's default: "Oct 31 13:52" within six months, else "Oct 31  2025". *)
let ls_date ns =
  let s = Int64.to_float ns /. 1e9 in
  let tm = Unix.localtime s and now = Unix.gettimeofday () in
  if s > now -. 15778476. && s <= now +. 60. then
    Printf.sprintf "%s %2d %02d:%02d" months.(tm.Unix.tm_mon) tm.Unix.tm_mday tm.Unix.tm_hour tm.Unix.tm_min
  else Printf.sprintf "%s %2d  %d" months.(tm.Unix.tm_mon) tm.Unix.tm_mday (tm.Unix.tm_year + 1900)

(* GNU's -h: powers of 1024, rounded up; one decimal below 10. *)
let human n =
  let n = Int64.to_float n in
  let units = [| ""; "K"; "M"; "G"; "T"; "P"; "E" |] in
  let rec pick v i = if v >= 1024. && i < Array.length units - 1 then pick (v /. 1024.) (i + 1) else v, i in
  let v, i = pick n 0 in
  if i = 0 then Printf.sprintf "%.0f" v
  else if v < 10. then
    let d = Float.ceil (v *. 10.) /. 10. in
    if d >= 10. then Printf.sprintf "%.0f%s" (Float.ceil v) units.(i) else Printf.sprintf "%.1f%s" d units.(i)
  else
    let c = Float.ceil v in
    if c >= 1024. && i < Array.length units - 1 then Printf.sprintf "1.0%s" units.(i + 1)
    else Printf.sprintf "%.0f%s" c units.(i)

(* --- ls --------------------------------------------------------------- *)

let ls args =
  prog := "ls";
  let opts, paths = parse ~short_value:[] args in
  let o k = has opts k in
  let long = o ["l"; "full-time"] || o ["g"; "n"] and all = o ["a"; "all"] and almost = o ["A"; "almost-all"] in
  let full = o ["full-time"] || value opts ["time-style"] = Some "full-iso" in
  let hum = o ["h"; "human-readable"] and recursive = o ["R"; "recursive"] and rev = o ["r"; "reverse"] in
  let by = match value opts ["sort"] with
    | Some "time" -> `Time | Some "size" -> `Size | Some "none" -> `None | Some "name" | None ->
        if o ["t"] then `Time else if o ["S"] then `Size else if o ["U"; "f"] then `None else `Name
    | Some w -> err "invalid argument '%s' for '--sort'" w; `Name in
  let directory = o ["d"; "directory"] and one = o ["1"] in
  show_retained := o ["retained"];
  let sort es =
    let cmp = match by with
      | `Name -> (fun a b -> compare (Filename.basename a.path) (Filename.basename b.path))
      | `Time -> (fun a b -> match compare b.mtime_ns a.mtime_ns with 0 -> compare a.path b.path | c -> c)
      | `Size -> (fun a b -> match compare b.size a.size with 0 -> compare a.path b.path | c -> c)
      | `None -> (fun _ _ -> 0) in
    let es = List.stable_sort cmp es in if rev then List.rev es else es in
  let print_long es name_of =
    let w f = List.fold_left (fun m e -> max m (String.length (f e))) 0 es in
    let size e = if hum then human e.size else Int64.to_string e.size in
    let wl = w (fun e -> string_of_int e.nlink) and wu = w (fun e -> user e.uid) and wg = w (fun e -> group e.gid)
    and ws = w size in
    List.iter (fun e ->
      let parts = [mode_string e.kind e.mode; Printf.sprintf "%*d" wl e.nlink] @
        (if o ["g"] then [] else [Printf.sprintf "%-*s" wu (if o ["n"] then string_of_int e.uid else user e.uid)]) @
        [Printf.sprintf "%-*s" wg (if o ["n"] then string_of_int e.gid else group e.gid); Printf.sprintf "%*s" ws (size e);
         (if full then full_iso e.mtime_ns else ls_date e.mtime_ns); name_of e] in
      print_endline (String.concat " " parts)) es in
  let print_entries es name_of =
    if long then print_long es name_of
    else if one || not (Unix.isatty Unix.stdout) then List.iter (fun e -> print_endline (name_of e)) es
    else begin
      let names = List.map name_of es in
      let width = (try int_of_string (Sys.getenv "COLUMNS") with _ -> 80) in
      let cw = 2 + List.fold_left (fun m n -> max m (String.length n)) 0 names in
      let cols = max 1 (width / cw) in
      let n = List.length names in let rows = (n + cols - 1) / cols in
      let a = Array.of_list names in
      for r = 0 to rows - 1 do
        let line = Buffer.create 80 in
        for c = 0 to cols - 1 do
          let i = c * rows + r in
          if i < n then Buffer.add_string line (if (c + 1) * rows + r < n then Printf.sprintf "%-*s" cw a.(i) else a.(i))
        done;
        print_endline (Buffer.contents line)
      done
    end in
  let total es = if long then
      let k = List.fold_left (fun s e -> Int64.add s (Int64.div (Int64.add e.size 4095L) 4096L)) 0L es in
      print_endline ("total " ^ (if hum then human (Int64.mul k 4096L) else Int64.to_string (Int64.mul k 4L))) in
  let dir_entries d =
    let names = children d in
    let names = List.filter (fun n -> all || almost || n.[0] <> '.') names in
    let es = List.filter_map (fun n -> lookup (Filename.concat d n)) names in
    let dots = if all then List.filter_map (fun n -> Option.map (fun e -> { e with path = Filename.concat d n })
                                               (lookup (if n = "." then d else Filename.dirname d))) ["."; ".."] else [] in
    sort (dots @ es) in
  let paths = if paths = [] then ["."] else paths in
  let entries = List.filter_map (fun p -> match lookup p with
    | Some e -> Some { e with path = p }
    | None -> err "cannot access %s: No such file or directory" (quote p); status := 2; None) paths in
  let files, dirs = List.partition (fun e -> directory || e.kind <> `Dir) entries in
  let files = sort files and dirs = sort dirs in
  if files <> [] then print_entries files (fun e -> e.path);
  let header = recursive || List.length entries > 1 in
  let first = ref (files = []) in
  let rec show d =
    if not !first then print_newline (); first := false;
    if header then print_endline (d ^ ":");
    let es = dir_entries d in
    total es; print_entries es (fun e -> Filename.basename e.path);
    if recursive then
      List.iter (fun e -> let n = Filename.basename e.path in
        if e.kind = `Dir && n <> "." && n <> ".." then show e.path) es in
  List.iter (fun e -> show e.path) dirs

(* --- cat -------------------------------------------------------------- *)

let cat args =
  prog := "cat";
  let opts, paths = parse ~short_value:[] args in
  let number = has opts ["n"; "number"] and nonblank = has opts ["b"; "number-nonblank"] in
  let line = ref 0 in
  List.iter (fun p -> match (if p = "-" then None else lookup p) with
    | None when p <> "-" -> err "%s: No such file or directory" p
    | Some e when e.kind = `Dir -> err "%s: Is a directory" p
    | e ->
        let d = match e with Some e -> read e | None -> In_channel.input_all stdin in
        if not (number || nonblank) then print_string d
        else begin
          let ls = String.split_on_char '\n' d in
          let n = List.length ls in
          List.iteri (fun i l ->
            if i = n - 1 && l = "" then () else begin
              if nonblank && l = "" then print_string "\n"
              else (incr line; Printf.printf "%6d\t%s%s" !line l (if i < n - 1 then "\n" else ""))
            end) ls
        end) (if paths = [] then ["-"] else paths)

(* --- stat ------------------------------------------------------------- *)

let stat args =
  prog := "stat";
  let opts, paths = parse ~short_value:["c"] args in
  let format, newline = match value opts ["c"; "format"], value opts ["printf"] with
    | Some f, _ -> f, true | None, Some f -> f, false
    | None, None -> "  File: %n\n  Size: %s\tType: %F\nAccess: (%04a/%A)  Uid: (%u/%U)   Gid: (%g/%G)\nModify: %y", true in
  let escape s = (* --printf backslash escapes *)
    let b = Buffer.create (String.length s) and i = ref 0 in
    while !i < String.length s do
      (if s.[!i] = '\\' && !i + 1 < String.length s then begin
         incr i; Buffer.add_char b (match s.[!i] with 'n' -> '\n' | 't' -> '\t' | '\\' -> '\\' | c -> c)
       end else Buffer.add_char b s.[!i]); incr i
    done; Buffer.contents b in
  let format = if newline then format else escape format in
  List.iter (fun p -> match lookup p with
    | None -> err "cannot stat %s: No such file or directory" (quote p)
    | Some e ->
        let b = Buffer.create 128 and i = ref 0 in
        while !i < String.length format do
          (if format.[!i] = '%' && !i + 1 < String.length format then begin
             (* %04a style: an optional width with zero fill *)
             let j = ref (!i + 1) in
             while !j < String.length format && format.[!j] >= '0' && format.[!j] <= '9' do incr j done;
             let width = String.sub format (!i + 1) (!j - !i - 1) in
             let pad s = if width = "" then s else
                 let w = int_of_string width in
                 if String.length s >= w then s else String.make (w - String.length s) (if width.[0] = '0' then '0' else ' ') ^ s in
             let c = if !j < String.length format then format.[!j] else '%' in
             Buffer.add_string b (pad (match c with
               | 'n' -> p | 's' -> Int64.to_string e.size | 'a' -> Printf.sprintf "%o" e.mode
               | 'A' -> mode_string e.kind e.mode | 'y' -> full_iso e.mtime_ns
               | 'Y' -> Int64.to_string (Int64.div e.mtime_ns 1_000_000_000L)
               | 'F' -> (match e.kind with `Dir -> "directory" | `Link -> "symbolic link"
                                          | `File -> if e.size = 0L then "regular empty file" else "regular file")
               | 'U' -> user e.uid | 'G' -> group e.gid | 'u' -> string_of_int e.uid | 'g' -> string_of_int e.gid
               | 'h' -> string_of_int e.nlink | '%' -> "%"
               | c -> "?" ^ String.make 1 c));
             i := !j
           end else Buffer.add_char b format.[!i]); incr i
        done;
        print_string (Buffer.contents b); if newline then print_newline ()) paths

(* --- du --------------------------------------------------------------- *)

(* Sizes are apparent sizes (members have no blocks of their own): -b is
   bytes, otherwise 1024-byte units rounded up per file, as GNU du
   --apparent-size reports them. Directories themselves count 0. *)
let du args =
  prog := "du";
  let opts, paths = parse ~short_value:["d"] args in
  let o k = has opts k in
  let bytes = o ["b"; "bytes"] and hum = o ["h"; "human-readable"] and summ = o ["s"; "summarize"] in
  let all = o ["a"; "all"] and tot = o ["c"; "total"] in
  let max_depth = match value opts ["d"; "max-depth"] with Some d -> int_of_string d | None -> max_int in
  let fmt n = if hum then human n else if bytes then Int64.to_string n else Int64.to_string (Int64.div (Int64.add n 1023L) 1024L) in
  let grand = ref 0L in
  let rec walk p depth =
    match lookup p with
    | None -> err "cannot access %s: No such file or directory" (quote p); 0L
    | Some e when e.kind <> `Dir ->
        if (all || depth = 0) && depth <= max_depth && not (summ && depth > 0) then Printf.printf "%s\t%s\n" (fmt e.size) p;
        if bytes || hum then e.size else Int64.mul (Int64.div (Int64.add e.size 1023L) 1024L) 1024L
    | Some _ ->
        let s = List.fold_left (fun s n -> Int64.add s (walk (Filename.concat p n) (depth + 1))) 0L (children p) in
        if depth <= max_depth && not (summ && depth > 0) then Printf.printf "%s\t%s\n" (fmt s) p;
        s in
  List.iter (fun p -> grand := Int64.add !grand (walk p 0)) (if paths = [] then ["."] else paths);
  if tot then Printf.printf "%s\ttotal\n" (fmt !grand)

(* --- find ------------------------------------------------------------- *)

(* fnmatch-style patterns: * ? and [...] *)
let rec glob p i s j =
  if i = String.length p then j = String.length s else
  match p.[i] with
  | '*' -> glob p (i + 1) s j || (j < String.length s && glob p i s (j + 1))
  | '?' -> j < String.length s && glob p (i + 1) s (j + 1)
  | '[' when String.index_from_opt p i ']' <> None ->
      let close = String.index_from p (i + 1) ']' in
      let set = String.sub p (i + 1) (close - i - 1) in
      let neg = set <> "" && (set.[0] = '!' || set.[0] = '^') in
      let set = if neg then String.sub set 1 (String.length set - 1) else set in
      let rec mem k = k < String.length set &&
        ((k + 2 < String.length set && set.[k + 1] = '-' && s.[j] >= set.[k] && s.[j] <= set.[k + 2]) || set.[k] = s.[j] || mem (k + 1)) in
      j < String.length s && (mem 0 <> neg) && glob p (close + 1) s (j + 1)
  | c -> j < String.length s && s.[j] = c && glob p (i + 1) s (j + 1)

let find args =
  prog := "find";
  let rec split acc = function
    | a :: rest when a = "" || a.[0] <> '-' -> split (a :: acc) rest
    | rest -> List.rev acc, rest in
  let paths, expr = split [] args in
  let name = ref None and typ = ref None and maxd = ref max_int and mind = ref 0 in
  let rec go = function
    | "-name" :: p :: r -> name := Some p; go r
    | "-type" :: t :: r -> typ := Some t; go r
    | "-maxdepth" :: n :: r -> maxd := int_of_string n; go r
    | "-mindepth" :: n :: r -> mind := int_of_string n; go r
    | "-print" :: r -> go r
    | [] -> ()
    | a :: _ -> err "unknown predicate %s" (quote a); exit 1 in
  go expr;
  let rec walk p depth =
    match lookup p with
    | None -> err "%s: No such file or directory" (quote p)
    | Some e ->
        let ok = depth >= !mind &&
          (match !name with Some pat -> glob pat 0 (Filename.basename p) 0 | None -> true) &&
          (match !typ with Some "f" -> e.kind = `File | Some "d" -> e.kind = `Dir | Some "l" -> e.kind = `Link
                         | Some t -> err "unknown argument to -type: %s" t; exit 1 | None -> true) in
        if ok then print_endline p;
        if e.kind = `Dir && depth < !maxd then List.iter (fun n -> walk (Filename.concat p n) (depth + 1)) (children p) in
  List.iter (fun p -> walk p 0) (if paths = [] then ["."] else paths)

(* --- rm --------------------------------------------------------------- *)

let rm args =
  prog := "rm";
  let opts, paths = parse ~short_value:[] args in
  let force = has opts ["f"; "force"] and recursive = has opts ["r"; "R"; "recursive"] in
  let dirs = has opts ["d"; "dir"] and verbose = has opts ["v"; "verbose"] in
  let say fmt = Printf.ksprintf (fun m -> if verbose then print_endline m) fmt in
  let rec remove p =
    match lookup p with
    | None -> if not force then err "cannot remove %s: No such file or directory" (quote p)
    | Some { where = `Member (c, name); _ } ->
        with_container c (fun k -> Container.remove k [name]); say "removed %s" (quote p)
    | Some { kind = `Dir; _ } when not recursive && not dirs -> err "cannot remove %s: Is a directory" (quote p)
    | Some { kind = `Dir; _ } ->
        if recursive then List.iter (fun n -> remove (Filename.concat p n)) (children p);
        (* An emptied container goes with its directory. *)
        (match container_of p with
         | Some c when (with_container c (fun k -> Container.members k)) = [] || recursive ->
             Sys.remove c; List.iter (fun x -> if Sys.file_exists (c ^ x) then Sys.remove (c ^ x)) ["-journal"]
         | Some _ -> err "cannot remove %s: Directory not empty" (quote p)
         | None -> ());
        if Sys.file_exists p then
          (try Unix.rmdir p with Unix.Unix_error (Unix.ENOTEMPTY, _, _) -> err "cannot remove %s: Directory not empty" (quote p));
        if not (Sys.file_exists p) then say "removed directory %s" (quote p)
    | Some _ -> Sys.remove p; say "removed %s" (quote p) in
  if paths = [] then (err "missing operand"; exit 1);
  List.iter remove paths

(* --- cp and mv -------------------------------------------------------- *)

(* Write [e]'s bytes to [dst] in the packed view: a member when dst's
   directory is packed, else a plain file. *)
let put ?(preserve = false) e dst =
  let dir = Filename.dirname dst in
  match container_of dir with
  | Some c ->
      (* Into a packed directory: a member, which wins over a plain file of
         the same name beside the container. Container.apply stores a plain
         file, so members are staged through one. *)
      let src = match e.where with
        | `Plain -> e.path
        | `Member _ ->
            let t = Filename.temp_file "packdir-cp" "" in
            Out_channel.with_open_bin t (fun oc -> output_string oc (read e));
            Unix.chmod t e.mode; N.utimens t e.mtime_ns e.mtime_ns; t in
      if not preserve && e.where = `Plain then begin
        (* Without -p a copy gets the time of copying, like cp. *)
        let t = Filename.temp_file "packdir-cp" "" in
        Out_channel.with_open_bin t (fun oc -> output_string oc (read e));
        ignore (with_container c (fun k -> Container.apply k ~puts:[{ Container.member = Filename.basename dst; source = t }] ~retain:[]));
        Sys.remove t
      end else
        ignore (with_container c (fun k -> Container.apply k ~puts:[{ Container.member = Filename.basename dst; source = src }] ~retain:[]));
      if src <> e.path then Sys.remove src
  | _ ->
      Out_channel.with_open_bin dst (fun oc -> output_string oc (read e));
      if preserve then (Unix.chmod dst e.mode; N.utimens dst e.mtime_ns e.mtime_ns)

let cp_mv ~move args =
  prog := (if move then "mv" else "cp");
  let opts, paths = parse ~short_value:["t"] args in
  let o k = has opts k in
  let preserve = move || o ["p"; "a"; "archive"; "preserve"] and recursive = o ["r"; "R"; "a"; "recursive"; "archive"] in
  let noclobber = o ["n"; "no-clobber"] and verbose = o ["v"; "verbose"] in
  let srcs, dest = match value opts ["t"; "target-directory"], List.rev paths with
    | Some t, _ -> paths, t
    | None, d :: (_ :: _ as rs) -> List.rev rs, d
    | _ -> err "missing destination file operand"; exit 1 in
  let into = is_dir dest in
  if List.length srcs > 1 && not into then (err "target %s is not a directory" (quote dest); exit 1);
  let rec copy src dst =
    match lookup src with
    | None -> err "cannot stat %s: No such file or directory" (quote src)
    | Some e when e.kind = `Dir ->
        if move && (match e.where with `Plain -> container_of src = None | _ -> false) then
          (Unix.rename src dst; if verbose then Printf.printf "renamed %s -> %s\n" (quote src) (quote dst))
        else if not recursive && not move then err "-r not specified; omitting directory %s" (quote src)
        else begin
          if not (is_dir dst) then Unix.mkdir dst 0o755;
          List.iter (fun n -> copy (Filename.concat src n) (Filename.concat dst n)) (children src);
          if move then rm ["-r"; "-f"; src]
        end
    | Some e ->
        if noclobber && lookup dst <> None then () else begin
          (match e.where, (if move then container_of (Filename.dirname dst) else None) with
           | `Member (c, name), Some c' when c = c' ->
               with_container c (fun k -> Container.rename k ~src:name ~dst:(Filename.basename dst))
           | _ ->
               put ~preserve e dst;
               if move then rm ["-f"; src]);
          if verbose then Printf.printf "%s%s -> %s\n" (if move then "renamed " else "") (quote src) (quote dst)
        end in
  List.iter (fun s -> copy s (if into then Filename.concat dest (Filename.basename s) else dest)) srcs

let commands = ["ls", ls; "cat", cat; "stat", stat; "du", du; "find", find; "rm", rm;
                "cp", cp_mv ~move:false; "mv", cp_mv ~move:true]
