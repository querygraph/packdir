(* Native calls (packdir_stubs.c) and the file helpers built on them. *)

external sha_init : unit -> bytes = "packdir_sha_init"
external sha_update : bytes -> bytes -> int -> int -> unit = "packdir_sha_update" [@@noalloc]
external sha_final : bytes -> string = "packdir_sha_final"
external stat_raw : string -> bool -> int64 array = "packdir_stat"
external utimens : string -> int64 -> int64 -> unit = "packdir_utimens"
external fullfsync : Unix.file_descr -> unit = "packdir_fullfsync"
external clone_info_raw : string -> int64 array = "packdir_clone_info"
external clonefile : string -> string -> unit = "packdir_clonefile"

(* (device, APFS clone id) of a regular file that has whole-file clones, or
   None: not APFS, not macOS, or no other file shares its clone id. *)
let clone_key path = match clone_info_raw path with
  | [| dev; id; refcount |] when refcount > 1L -> Some (dev, id)
  | _ | exception _ -> None

let block = 1048576

type stat = {
  ino : int64; size : int64; mtime_ns : int64; ctime_ns : int64; atime_ns : int64;
  mode : int; uid : int; gid : int; kind : [ `Reg | `Dir | `Link | `Other ];
}

let stat ?(follow = true) path =
  let a = stat_raw path follow in
  { ino = a.(0); size = a.(1); mtime_ns = a.(2); ctime_ns = a.(3); atime_ns = a.(4);
    mode = Int64.to_int a.(5); uid = Int64.to_int a.(6); gid = Int64.to_int a.(7);
    kind = (match a.(8) with 0L -> `Reg | 1L -> `Dir | 2L -> `Link | _ -> `Other) }

let sha_string s =
  let c = sha_init () in
  sha_update c (Bytes.unsafe_of_string s) 0 (String.length s);
  sha_final c

let sha_file path =
  In_channel.with_open_bin path (fun ic ->
    let c = sha_init () and buf = Bytes.create block in
    let rec loop () =
      let n = input ic buf 0 block in
      if n > 0 then (sha_update c buf 0 n; loop ()) in
    loop ();
    sha_final c)

let rec mkdir_p path =
  if not (Sys.file_exists path) then begin
    mkdir_p (Filename.dirname path);
    try Unix.mkdir path 0o777 with Unix.Unix_error (Unix.EEXIST, _, _) -> ()
  end
