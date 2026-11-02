#!/usr/bin/env python3
"""Controls for APFS clones in packdir (the manual's chapter "Clones"), synthetic data only.

Needs macOS and an APFS scratch directory (the system temporary directory is
one). A tree with clone groups inside one container and across containers,
and identical files that are not clones, is packed with --clones, then
checked: what is stored, verify, stock sqlite3 -Ax, cat, unpack-tree (clone ids
read back with getattrlist), a single-container restore, and then every way an
original or a clone can change: a rerun, a new clone of a packed original, a
changed original, a changed clone, a deleted original kept as retained,
repack --drop-retained, rm and mv of an original, mv of a clone, a stale
back-reference."""
import argparse, ctypes, ctypes.util, hashlib, json, os, pathlib, random, shutil, sqlite3, struct, subprocess, sys, tempfile

p = argparse.ArgumentParser()
p.add_argument('--packdir', required=True, help='packdir binary')
p.add_argument('--pack-args', default='', help='extra pack-tree options, e.g. "--engine turso"')
a = p.parse_args()
if sys.platform != 'darwin':
    sys.exit('clones.py needs macOS (APFS clone ids)')

checks = 0
def check(ok, message):
    global checks
    if not ok:
        sys.exit('FAIL: ' + message)
    checks += 1

def tool(*args, rc=0):
    extra = a.pack_args.split()
    if args[0] == 'repack':  # repack takes only the engine
        extra = [x for i, x in enumerate(extra) if x == '--engine' or (i > 0 and extra[i - 1] == '--engine')]
    args = [*args, *(extra if args[0] in ('pack-tree', 'repack') else [])]
    r = subprocess.run([a.packdir, *map(str, args)], capture_output=True, text=True)
    check(r.returncode == rc, '%s exited %d (want %d):\n%s%s' % (args[0], r.returncode, rc, r.stdout, r.stderr))
    return r.stdout

def last_json(text):
    return json.loads([l for l in text.splitlines() if l.startswith('{')][-1])

# getattrlist(ATTR_CMNEXT_CLONEID | ATTR_CMNEXT_CLONE_REFCNT)
libc = ctypes.CDLL(ctypes.util.find_library('c'), use_errno=True)
class attrlist(ctypes.Structure):
    _fields_ = [('bitmapcount', ctypes.c_ushort), ('reserved', ctypes.c_ushort), ('commonattr', ctypes.c_uint),
                ('volattr', ctypes.c_uint), ('dirattr', ctypes.c_uint), ('fileattr', ctypes.c_uint),
                ('forkattr', ctypes.c_uint)]
def clone_id(path):
    al = attrlist(5, 0, 0x80000000, 0, 0, 0, 0x100 | 0x1000)  # RETURNED_ATTRS; CLONEID | CLONE_REFCNT
    buf = ctypes.create_string_buffer(64)
    if libc.getattrlist(str(path).encode(), ctypes.byref(al), buf, 64, 0x20 | 0x1) != 0:  # CMN_EXTENDED | NOFOLLOW
        raise OSError(ctypes.get_errno(), str(path))
    cid, refcnt = struct.unpack_from('<QI', buf.raw, 4 + 20)
    return cid, refcnt

def cp_clone(src, dst):
    subprocess.run(['cp', '-c', str(src), str(dst)], check=True)

def sha(b):
    return hashlib.sha256(b).hexdigest()

def tree(root):
    return {str(f.relative_to(root)): (f.read_bytes(), f.stat().st_mtime_ns, f.stat().st_mode & 0o777)
            for f in sorted(root.rglob('*')) if f.is_file()}

def db(path, sql, *args):
    c = sqlite3.connect(path)
    try:
        return c.execute(sql, args).fetchall()
    finally:
        c.close()

def verify(out):
    for c in sorted(out.rglob('*.sqlar')):
        o = tool('verify', c)
        check(o.strip().endswith('members'), 'verify %s:\n%s' % (c, o))

rng = random.Random(7)
with tempfile.TemporaryDirectory(prefix='packdir-clones-') as td:
    t = pathlib.Path(td).resolve()
    src, out = t / 'src', t / 'out'
    for d in ('lr/0000', 'lr/0001', 'pm/0000'):
        (src / d).mkdir(parents=True)
    x = src / 'lr/0000/x'; x.write_bytes(rng.randbytes(300000))
    cp_clone(x, src / 'lr/0000/y')                  # clone, same container
    cp_clone(x, src / 'pm/0000/x-pm')               # clone, other container
    shutil.copy2(x, src / 'lr/0000/z')               # identical, not a clone
    q = src / 'lr/0001/q'; q.write_bytes(rng.randbytes(200000))
    cp_clone(q, src / 'pm/0000/q-pm')
    (src / 'pm/0000/plain').write_bytes(b'plain member\n')
    os.chmod(src / 'lr/0000/y', 0o600)

    # Pack: one copy of each group's bytes, references for the rest.
    r = last_json(tool('pack-tree', src, out, '--clones'))
    check(r['stored'] == 7 and r['clones'] == 3, 'pack-tree --clones: %s' % r)
    lr0, lr1, pm0 = out / 'lr/0000.sqlar', out / 'lr/0001.sqlar', out / 'pm/0000.sqlar'
    check(sorted(n for (n,) in db(lr0, 'SELECT name FROM sqlar')) == ['x', 'z'], 'lr/0000 holds x and z as data')
    check(db(lr0, 'SELECT name, original FROM packdir_clone') == [('y', 'x')], 'y is a clone of x')
    check(sorted(db(pm0, 'SELECT name, original FROM packdir_clone')) == [('q-pm', '../../lr/0001/q'), ('x-pm', '../../lr/0000/x')],
          'pm/0000 clones reference lr: %s' % db(pm0, 'SELECT name, original FROM packdir_clone'))
    check(sorted(db(lr0, 'SELECT name, clone FROM packdir_cloned_by')) == [('x', '../../pm/0000/x-pm'), ('x', 'y')],
          'x lists its clones')
    check(db(lr0, "SELECT value FROM unison_container WHERE key = 'format'") == [('unison-packdir/2',)], 'format /2 with clones')
    check(db(lr1, "SELECT value FROM unison_container WHERE key = 'format'") == [('unison-packdir/2',)], 'an original\'s container is /2 too')
    verify(out)

    # The packed view: ls lists clones, cat reads them through their originals.
    check(sorted(tool('ls', out / 'lr/0000').split()) == ['x', 'y', 'z'], 'ls lists the clone')
    check(subprocess.run([a.packdir, 'cat', out / 'pm/0000/x-pm'], capture_output=True).stdout == x.read_bytes(), 'cat a clone')
    check('-rw-------' in tool('ls', '-l', out / 'lr/0000/y'), 'a clone keeps its own mode')

    # Stock sqlite3 -Ax: every data member, and no clone.
    stock = t / 'stock'; stock.mkdir()
    subprocess.run(['sqlite3', str(lr0), '-Ax'], cwd=stock, check=True, capture_output=True)
    check(sorted(os.listdir(stock)) == ['x', 'z'], 'stock extraction skips clones: %s' % os.listdir(stock))

    # unpack-tree: the same tree, with the clone groups recreated.
    back = t / 'back'
    u = last_json(tool('unpack-tree', out, back))
    check(u == {'containers': 3, 'files': 7, 'cloned': 3, 'copied': 0}, 'unpack-tree: %s' % u)
    check(tree(back) == tree(src), 'unpack-tree gives back the source')
    gx = clone_id(back / 'lr/0000/x')
    check(clone_id(back / 'lr/0000/y')[0] == gx[0] and clone_id(back / 'pm/0000/x-pm')[0] == gx[0] and gx[1] == 3,
          'x, y and x-pm are one clone group again')
    check(clone_id(back / 'lr/0000/z')[1] == 1, 'z stays independent')
    check(clone_id(back / 'pm/0000/q-pm')[0] == clone_id(back / 'lr/0001/q')[0], 'q-pm is a clone of q again')

    # A single container: clones of its own members are cloned, others copied.
    one = t / 'one'
    tool('restore', lr0, one)
    check(clone_id(one / 'y')[0] == clone_id(one / 'x')[0], 'restore clones within a container')
    alone = t / 'alone'
    tool('restore', pm0, alone)
    check((alone / 'x-pm').read_bytes() == x.read_bytes() and clone_id(alone / 'x-pm')[1] == 1,
          'restore copies a clone whose original is elsewhere')

    # A rerun opens nothing.
    r = last_json(tool('pack-tree', src, out, '--clones'))
    check(r['opened'] == 0 and r['stored'] == 0, 'rerun: %s' % r)

    # A new clone of a packed original, in a later run.
    cp_clone(x, src / 'pm/0000/x-pm2')
    r = last_json(tool('pack-tree', src, out, '--clones'))
    check(r['clones'] == 1 and ('x-pm2', '../../lr/0000/x') in db(pm0, 'SELECT name, original FROM packdir_clone'),
          'a later clone references the packed original: %s' % r)
    verify(out)

    # The original changes: its clones get its old bytes first.
    old = x.read_bytes()
    with open(x, 'r+b') as f:
        f.write(b'changed')
    tool('pack-tree', src, out, '--clones')
    check(sorted(n for (n,) in db(pm0, 'SELECT name FROM packdir_clone')) == ['q-pm'], 'x\'s clones were materialized')
    check(db(lr0, 'SELECT count(*) FROM packdir_cloned_by') == [(0,)], 'x\'s back-references are gone')
    check(subprocess.run([a.packdir, 'cat', out / 'lr/0000/y'], capture_output=True).stdout == old, 'y keeps the old bytes')
    verify(out)
    out_back = t / 'back2'
    tool('unpack-tree', out, out_back)
    check(tree(out_back) == tree(src), 'unpack after a changed original')

    # A changed clone is an ordinary change; the group keeps its other members.
    with open(src / 'pm/0000/q-pm', 'ab') as f:
        f.write(b'+')
    tool('pack-tree', src, out, '--clones')
    check(db(pm0, 'SELECT name FROM packdir_clone') == [], 'a changed clone holds data')
    verify(out)

    # A deleted original is retained, and its clones still resolve; dropping it
    # materializes them.
    shutil.copy2(src / 'lr/0001/q', t / 'q.keep')
    cp_clone(src / 'lr/0001/q', src / 'pm/0000/q-pm3')
    tool('pack-tree', src, out, '--clones')
    check(('q-pm3', '../../lr/0001/q') in db(pm0, 'SELECT name, original FROM packdir_clone'), 'q-pm3 is a clone of q')
    os.remove(src / 'lr/0001/q')
    tool('pack-tree', src, out, '--clones')
    check(subprocess.run([a.packdir, 'cat', out / 'pm/0000/q-pm3'], capture_output=True).stdout == (t / 'q.keep').read_bytes(),
          'a clone of a retained original reads')
    tool('repack', lr1, '--drop-retained')
    check(db(pm0, "SELECT count(*) FROM packdir_clone WHERE name = 'q-pm3'") == [(0,)], 'drop-retained materializes')
    check(subprocess.run([a.packdir, 'cat', out / 'pm/0000/q-pm3'], capture_output=True).stdout == (t / 'q.keep').read_bytes(),
          'q-pm3 keeps its bytes')
    verify(out)

    # rm and mv of an original, mv of a clone, through the packed view.
    w = src / 'lr/0001/w'; w.write_bytes(rng.randbytes(100000))
    cp_clone(w, src / 'pm/0000/w1'); cp_clone(w, src / 'pm/0000/w2'); cp_clone(w, src / 'lr/0001/w3')
    tool('pack-tree', src, out, '--clones')
    check(len(db(lr1, "SELECT * FROM packdir_cloned_by WHERE name = 'w'")) == 3, 'w has three clones')
    tool('mv', out / 'lr/0001/w', out / 'lr/0001/w-renamed')
    check(('w1', '../../lr/0001/w-renamed') in db(pm0, 'SELECT name, original FROM packdir_clone'), 'mv of an original moves the references')
    check(('w3', 'w-renamed') in db(lr1, 'SELECT name, original FROM packdir_clone'), 'and within its container')
    tool('mv', out / 'pm/0000/w2', out / 'pm/0000/w2-renamed')
    check(('w-renamed', '../../pm/0000/w2-renamed') in db(lr1, 'SELECT name, clone FROM packdir_cloned_by'), 'mv of a clone moves its back-reference')
    verify(out)
    tool('rm', out / 'lr/0001/w-renamed')
    check(db(pm0, "SELECT count(*) FROM packdir_clone WHERE name IN ('w1', 'w2-renamed')") == [(0,)], 'rm of an original materializes its clones')
    check(subprocess.run([a.packdir, 'cat', out / 'pm/0000/w2-renamed'], capture_output=True).stdout == w.read_bytes(),
          'the renamed clone kept its bytes')
    verify(out)

    # A stale back-reference (a crash between it and the clone row) is harmless.
    db_c = sqlite3.connect(lr0)
    db_c.execute("INSERT INTO packdir_cloned_by(name, clone) VALUES ('z', '../../pm/0000/nothing')"); db_c.commit(); db_c.close()
    verify(out)
    with open(src / 'lr/0000/z', 'ab') as f:
        f.write(b'+')
    tool('pack-tree', src, out, '--clones')
    verify(out)

    # Without --clones nothing is a reference.
    plain_out = t / 'plain-out'
    tool('pack-tree', src, plain_out)
    check(all(db(c, 'SELECT count(*) FROM packdir_clone') == [(0,)] for c in plain_out.rglob('*.sqlar')), 'no clones without --clones')
    check(all(db(c, "SELECT value FROM unison_container WHERE key = 'format'") == [('unison-packdir/1',)]
              for c in plain_out.rglob('*.sqlar')), 'format stays /1 without clones')

print('ok %d clone checks' % checks)
