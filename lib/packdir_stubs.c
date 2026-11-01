/* Native calls for packdir: SHA-256, nanosecond stat, utimensat and the
   drive-cache flush. */
#include <errno.h>
#include <fcntl.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>
#include <caml/alloc.h>
#include <caml/fail.h>
#include <caml/memory.h>
#include <caml/mlvalues.h>
#include <caml/signals.h>
#include <caml/unixsupport.h>

#ifdef __APPLE__
#include <CommonCrypto/CommonDigest.h>
typedef CC_SHA256_CTX sha_ctx;
#define SHA_INIT CC_SHA256_Init
#define SHA_UPDATE(c, d, n) CC_SHA256_Update(c, d, (CC_LONG)(n))
#define SHA_FINAL(o, c) CC_SHA256_Final(o, c)
#else
#include <openssl/sha.h>
typedef SHA256_CTX sha_ctx;
#define SHA_INIT SHA256_Init
#define SHA_UPDATE SHA256_Update
#define SHA_FINAL SHA256_Final
#endif

CAMLprim value packdir_sha_init(value unit) {
  CAMLparam1(unit);
  CAMLlocal1(v);
  v = caml_alloc_string(sizeof(sha_ctx));
  SHA_INIT((sha_ctx *)Bytes_val(v));
  CAMLreturn(v);
}

CAMLprim value packdir_sha_update(value ctx, value data, value off, value len) {
  SHA_UPDATE((sha_ctx *)Bytes_val(ctx), Bytes_val(data) + Long_val(off), Long_val(len));
  return Val_unit;
}

/* Lower-case hex text: the form every container stores in unison_member. */
CAMLprim value packdir_sha_final(value ctx) {
  CAMLparam1(ctx);
  CAMLlocal1(v);
  static const char hex[] = "0123456789abcdef";
  unsigned char out[32];
  SHA_FINAL(out, (sha_ctx *)Bytes_val(ctx));
  v = caml_alloc_string(64);
  for (int i = 0; i < 32; i++) {
    Bytes_val(v)[2 * i] = hex[out[i] >> 4];
    Bytes_val(v)[2 * i + 1] = hex[out[i] & 15];
  }
  CAMLreturn(v);
}

static int64_t ns(struct timespec t) { return (int64_t)t.tv_sec * 1000000000 + t.tv_nsec; }

/* [| ino; size; mtime_ns; ctime_ns; atime_ns; mode; uid; gid; kind |]
   kind: 0 regular, 1 directory, 2 symlink, 3 other. */
CAMLprim value packdir_stat(value path, value follow) {
  CAMLparam2(path, follow);
  CAMLlocal1(v);
  struct stat st;
  char *p = caml_stat_strdup(String_val(path));
  caml_enter_blocking_section();
  int rc = Bool_val(follow) ? stat(p, &st) : lstat(p, &st);
  caml_leave_blocking_section();
  caml_stat_free(p);
  if (rc < 0) uerror("stat", path);
#ifdef __APPLE__
  struct timespec m = st.st_mtimespec, c = st.st_ctimespec, a = st.st_atimespec;
#else
  struct timespec m = st.st_mtim, c = st.st_ctim, a = st.st_atim;
#endif
  int64_t kind = S_ISREG(st.st_mode) ? 0 : S_ISDIR(st.st_mode) ? 1 : S_ISLNK(st.st_mode) ? 2 : 3;
  int64_t f[9] = {(int64_t)st.st_ino, (int64_t)st.st_size, ns(m), ns(c), ns(a),
                  st.st_mode & 07777, st.st_uid, st.st_gid, kind};
  v = caml_alloc_tuple(9);
  for (int i = 0; i < 9; i++) Store_field(v, i, caml_copy_int64(f[i]));
  CAMLreturn(v);
}

CAMLprim value packdir_utimens(value path, value atime, value mtime) {
  CAMLparam3(path, atime, mtime);
  struct timespec t[2];
  int64_t a = Int64_val(atime), m = Int64_val(mtime);
  t[0].tv_sec = a / 1000000000; t[0].tv_nsec = a % 1000000000;
  t[1].tv_sec = m / 1000000000; t[1].tv_nsec = m % 1000000000;
  if (utimensat(AT_FDCWD, String_val(path), t, 0) < 0) uerror("utimensat", path);
  CAMLreturn(Val_unit);
}

/* F_FULLFSYNC: flush the drive's own write cache (macOS); fsync elsewhere. */
CAMLprim value packdir_fullfsync(value fd) {
#ifdef F_FULLFSYNC
  if (fcntl(Int_val(fd), F_FULLFSYNC) == 0) return Val_unit;
#endif
  if (fsync(Int_val(fd)) < 0) uerror("fullfsync", Nothing);
  return Val_unit;
}
