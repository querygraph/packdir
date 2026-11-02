#!/usr/bin/env python3
"""Controls across the builds of packdir: every build packs a tree in every
mode, and every other build must read it, list it and restore it exactly, run
the same file commands to the same output, carry on from its state and recover
its crash; on macOS, the same for a tree of APFS clones packed with --clones.
Synthetic data only. Pass each build as --build NAME=PATH."""
import argparse, ctypes, ctypes.util, hashlib, json, os, pathlib, random, shutil, sqlite3, struct, subprocess, sys, tempfile

p = argparse.ArgumentParser()
p.add_argument('--build', action='append', required=True, help='NAME=PATH')
a = p.parse_args()
builds = {n: os.path.abspath(path) for n, path in (b.split('=', 1) for b in a.build)}
MODES = {'sqlite': [], 'turso': ['--engine', 'turso'], 'mvcc': ['--mvcc'], 'mvcc-writers': ['--mvcc', '--writers', '3', '--workers', '2']}
checks = 0

def check(ok, message):
    global checks
    if not ok:
        sys.exit('FAIL: ' + message)
    checks += 1

env = dict(os.environ, LC_ALL='C', TZ='America/Los_Angeles')

def tool(build, *args, rc=0, cwd=None, extra=None):
    r = subprocess.run([builds[build], *map(str, args)], capture_output=True, text=True, cwd=cwd, env=dict(env, **(extra or {})))
    check(r.returncode == rc, '%s %s exited %d, want %d:\n%s%s' % (build, ' '.join(map(str, args)), r.returncode, rc, r.stdout, r.stderr))
    return r.stdout

def tree(root):
    return {str(f.relative_to(root)): (f.read_bytes(), f.stat().st_mtime_ns, f.stat().st_mode & 0o777)
            for f in sorted(root.rglob('*')) if f.is_file()}

def tables(path):
    """Everything the format stores, in a form that does not depend on the engine."""
    db = sqlite3.connect('file:%s?mode=ro' % path, uri=True)
    out = (db.execute('SELECT name, mode, mtime, sz, data FROM sqlar ORDER BY name').fetchall(),
           db.execute('SELECT name, mtime_ns, fp, fpalg, sha256, retained_deleted FROM unison_member ORDER BY name').fetchall(),
           db.execute("SELECT value FROM unison_container WHERE key = 'format'").fetchall())
    header = (db.execute('PRAGMA page_size').fetchone()[0], db.execute('PRAGMA journal_mode').fetchone()[0],
              db.execute('PRAGMA integrity_check').fetchone()[0])
    db.close()
    return out, header

rng = random.Random(41)
with tempfile.TemporaryDirectory(prefix='packdir-cross-') as td:
    t = pathlib.Path(td).resolve()
    src = t / 'src'
    for leaf in ('0/0000', '0/0a11', 'a/0a22', 'a/0a33'):
        d = src / leaf; d.mkdir(parents=True)
        for i in range(rng.randint(4, 9)):
            f = d / ('%08x_%d' % (rng.getrandbits(32), rng.choice([320, 1024])))
            f.write_bytes(rng.randbytes(rng.choice([0, 100, 70000, 300000])))
            ns = 1_700_000_000_123_456_789 + rng.randint(0, 10**15)
            os.utime(f, ns=(ns, ns)); f.chmod(rng.choice([0o644, 0o600]))
    plain = tree(src)
    reference = {}
    for writer in builds:
        for mode, flags in MODES.items():
            out = t / ('%s-%s' % (writer, mode))
            r = json.loads(tool(writer, 'pack-tree', src, out, *flags).splitlines()[-1])
            where = '%s %s' % (writer, mode)
            check(r['stored'] == len(plain) and r['containers'] == 4, where + ': %r' % r)
            containers = sorted(out.glob('*/*.sqlar'))
            left = [f.name for f in out.rglob('*') if f.is_file() and not f.name.endswith('.sqlar') and not f.name.startswith('.packdir')]
            check(not left, where + ': nothing beside the containers: %r' % left)
            # What is stored is one thing, whoever wrote it, in whichever mode.
            for c in containers:
                rel = str(c.relative_to(out))
                stored, header = tables(c)
                check(header == (65536, 'delete', 'ok'), where + ': standard file: %r' % (header,))
                check(reference.setdefault(rel, stored) == stored, where + ': %s differs from the first build' % rel)
            stock = t / ('stock-' + out.name) / '0/0000'; stock.mkdir(parents=True)
            subprocess.run(['sqlite3', str(out / '0/0000.sqlar'), '-AxC', str(stock)], check=True)
            check(all(b == plain['0/0000/' + n][0] for n, (b, _, _) in tree(stock).items()), where + ': stock sqlite3 extracts it')
            for reader in builds:
                seen = '%s read by %s' % (where, reader)
                check('ok' in tool(reader, 'verify', *containers), seen)
                target = t / ('restore-%s-%s' % (out.name, reader))
                tool(reader, 'unpack-tree', out, target)
                check(tree(target) == plain, seen + ': restores exactly')
                # Its state is everyone's: a rerun with nothing new opens nothing.
                r = json.loads(tool(reader, 'pack-tree', src, out).splitlines()[-1])
                check(r['opened'] == 0 and r['stored'] == 0, seen + ': carries on from the state: %r' % r)
    # The file commands print the same, whoever runs them.
    first = next(iter(builds))
    out = t / (first + '-sqlite')
    for args in (['ls', '-lR', '--full-time', '.'], ['ls', '-A', '0/0000'], ['du', '-ab', '.'], ['find', '.', '-type', 'f'],
                 ['stat', '-c', '%n %s %a %y', '0/0000'], ['cat', '0/0000/nothing'], ['ls', 'nothing'], ['du', '-sh', '.']):
        outputs = {}
        for b in builds:
            r = subprocess.run([builds[b], *args], capture_output=True, text=True, cwd=out, env=env)
            outputs[b] = (r.returncode, r.stdout, r.stderr)
        check(len(set(outputs.values())) == 1, 'file command %s differs: %r' % (args, outputs))
    # A crash in one build, recovered by each other, incrementally.
    names = list(builds)
    for i, writer in enumerate(names):
        for mode in ('sqlite', 'mvcc'):
            reader = names[(i + 1) % len(names)]
            base = t / ('%s-%s' % (writer, mode))
            work = t / ('crash-%s-%s' % (writer, mode))
            shutil.copytree(base, work)
            csrc = t / ('crash-src-%s-%s' % (writer, mode)); shutil.copytree(src, csrc)
            (csrc / '0/0000/crash_a').write_bytes(b'a' * 90000); (csrc / '0/0000/crash_b').write_bytes(b'b' * 90000)
            old = sorted((csrc / 'a/0a22').iterdir())[0]; old.unlink()
            tool(writer, 'pack-tree', csrc, work, *MODES[mode], rc=3, extra={'PACKDIR_CRASH_AFTER': '1'})
            r = json.loads(tool(reader, 'pack-tree', csrc, work, *MODES[mode]).splitlines()[-1])
            check(r['stored'] == 2 and r['retained'] == 1, '%s %s crash, %s carries on: %r' % (writer, mode, reader, r))
            check('ok' in tool(writer, 'verify', *sorted(work.glob('*/*.sqlar'))), 'and it verifies')
            check(any(l.endswith('retained') for l in tool(writer, 'list', work / 'a/0a22.sqlar').splitlines()), 'the deletion is retained')
    # A container SQLite made is Turso's only once repack has rebuilt it.
    for b in builds:
        out = t / ('upgrade-' + b)
        tool(b, 'pack-tree', src, out)
        c = out / '0/0000.sqlar'
        image = c.read_bytes()
        leaf = t / ('upgrade-src-' + b); shutil.copytree(src, leaf)
        (leaf / '0/0000/new_320').write_bytes(b'new')
        r = subprocess.run([builds[b], 'pack-tree', str(leaf), str(out), '--engine', 'turso'], capture_output=True, text=True, env=env)
        check(r.returncode != 0 and 'auto-vacuum' in r.stdout + r.stderr, b + ': Turso refuses a container with auto-vacuum: %r' % (r,))
        check(c.read_bytes() == image, b + ': and leaves it as it was')
        check('ok' in tool(b, 'verify', c, '--engine', 'turso'), b + ': Turso may still read it')
        check(c.read_bytes() == image, b + ': without changing it')
        before = tool(b, 'list', c)
        tool(b, 'repack', *sorted(out.glob('*/*.sqlar')), '--engine', 'turso')
        check(tool(b, 'list', c) == before, b + ': repack keeps the members')
        check(not sqlite3.connect(c).execute('PRAGMA auto_vacuum').fetchone()[0], b + ': and drops auto-vacuum')
        r = json.loads(tool(b, 'pack-tree', leaf, out, '--mvcc').splitlines()[-1])
        check(r['stored'] == 1, b + ': then Turso writes it: %r' % r)
        check(tables(c)[1] == (65536, 'delete', 'ok'), b + ': sealed again')
    # APFS clones (docs/clones.md): every build packs a tree with a clone group
    # inside one container and one across containers with --clones, in every
    # mode; the clone tables are one thing whoever wrote them, and every build
    # verifies, reads, restores (clone ids checked) and carries on from the
    # result, through a rerun and a changed original.
    if sys.platform == 'darwin':
        def clone_src(root):
            """The same clone tree, bytes and mtimes, each time it is built."""
            r = random.Random(43)
            for d in ('lr/0000', 'lr/0001', 'pm/0000'):
                (root / d).mkdir(parents=True)
            x = root / 'lr/0000/x'; x.write_bytes(r.randbytes(300000))
            q = root / 'lr/0001/q'; q.write_bytes(r.randbytes(200000))
            for s, d in ((x, 'lr/0000/y'), (x, 'pm/0000/x-pm'), (q, 'pm/0000/q-pm')):
                subprocess.run(['cp', '-c', str(s), str(root / d)], check=True)  # clonefile
            shutil.copyfile(x, root / 'lr/0000/z')  # identical, not a clone
            (root / 'pm/0000/plain').write_bytes(b'plain member\n')
            os.chmod(root / 'lr/0000/y', 0o600)
            for i, f in enumerate(sorted(p for p in root.rglob('*') if p.is_file())):
                ns = 1_700_000_000_123_456_789 + i * 1_000_003
                os.utime(f, ns=(ns, ns))
            return root

        libc = ctypes.CDLL(ctypes.util.find_library('c'), use_errno=True)
        class attrlist(ctypes.Structure):
            _fields_ = [('bitmapcount', ctypes.c_ushort), ('reserved', ctypes.c_ushort), ('commonattr', ctypes.c_uint),
                        ('volattr', ctypes.c_uint), ('dirattr', ctypes.c_uint), ('fileattr', ctypes.c_uint),
                        ('forkattr', ctypes.c_uint)]
        def clone_id(path):
            """(clone id, refcount): getattrlist(ATTR_CMNEXT_CLONEID | ATTR_CMNEXT_CLONE_REFCNT)."""
            al = attrlist(5, 0, 0x80000000, 0, 0, 0, 0x100 | 0x1000)  # RETURNED_ATTRS; CLONEID | CLONE_REFCNT
            buf = ctypes.create_string_buffer(64)
            if libc.getattrlist(str(path).encode(), ctypes.byref(al), buf, 64, 0x20 | 0x1) != 0:  # CMN_EXTENDED | NOFOLLOW
                raise OSError(ctypes.get_errno(), str(path))
            return struct.unpack_from('<QI', buf.raw, 4 + 20)

        def clone_tables(path):
            db = sqlite3.connect('file:%s?mode=ro' % path, uri=True)
            out = (db.execute('SELECT name, mode, mtime, sz, original, sha256 FROM packdir_clone ORDER BY name').fetchall(),
                   db.execute('SELECT name, clone FROM packdir_cloned_by ORDER BY name, clone').fetchall())
            db.close()
            return out

        def ids_check(root, seen):
            gx = clone_id(root / 'lr/0000/x')
            check(gx[1] == 3 and clone_id(root / 'lr/0000/y')[0] == gx[0] and clone_id(root / 'pm/0000/x-pm')[0] == gx[0],
                  seen + ': x, y and x-pm are one clone group again')
            check(clone_id(root / 'pm/0000/q-pm')[0] == clone_id(root / 'lr/0001/q')[0], seen + ': q-pm is a clone of q again')
            check(clone_id(root / 'lr/0000/z')[1] == 1, seen + ': z stays independent')

        csrc = clone_src(t / 'clone-src')
        cplain = tree(csrc)
        x_bytes = (csrc / 'lr/0000/x').read_bytes()
        creference, after_reference = {}, {}
        for writer in builds:
            for mode, flags in MODES.items():
                out = t / ('clones-%s-%s' % (writer, mode))
                where = 'clones %s %s' % (writer, mode)
                r = json.loads(tool(writer, 'pack-tree', csrc, out, '--clones', *flags).splitlines()[-1])
                check(r['stored'] == 7 and r['clones'] == 3 and r['containers'] == 3, where + ': %r' % r)
                containers = sorted(out.glob('*/*.sqlar'))
                # The stored tables, clone tables included, are one thing.
                for c in containers:
                    rel = str(c.relative_to(out))
                    stored, header = tables(c)
                    check(header == (65536, 'delete', 'ok'), where + ': standard file: %r' % (header,))
                    check(stored[2] == [('unison-packdir/2',)], where + ': %s is format /2' % rel)
                    both = (stored, clone_tables(c))
                    check(creference.setdefault(rel, both) == both, where + ': %s differs from the first build' % rel)
                check([(n, o) for (n, _, _, _, o, _) in clone_tables(out / 'lr/0000.sqlar')[0]] == [('y', 'x')], where + ': y is a clone of x')
                stock = t / ('clone-stock-%s-%s' % (writer, mode)); stock.mkdir()
                subprocess.run(['sqlite3', str(out / 'lr/0000.sqlar'), '-Ax'], cwd=stock, check=True, capture_output=True)
                check(sorted(os.listdir(stock)) == ['x', 'z'], where + ': stock extraction skips the clone')
                for reader in builds:
                    seen = '%s read by %s' % (where, reader)
                    check(all(l.endswith('members') for l in tool(reader, 'verify', *containers).splitlines()), seen + ': verifies')
                    for clone in ('lr/0000/y', 'pm/0000/x-pm'):
                        got = subprocess.run([builds[reader], 'cat', out / clone], capture_output=True, env=env).stdout
                        check(got == x_bytes, seen + ': cat ' + clone)
                    target = t / ('clone-restore-%s-%s-%s' % (writer, mode, reader))
                    u = json.loads(tool(reader, 'unpack-tree', out, target).splitlines()[-1])
                    check(u == {'containers': 3, 'files': 7, 'cloned': 3, 'copied': 0}, seen + ': unpack-tree %r' % u)
                    check(tree(target) == cplain, seen + ': restores exactly')
                    ids_check(target, seen)
                    r = json.loads(tool(reader, 'pack-tree', csrc, out, '--clones', *flags).splitlines()[-1])
                    check(r['opened'] == 0 and r['stored'] == 0 and r['clones'] == 0, seen + ': a rerun opens nothing: %r' % r)
                    # The original changes: its clones get its old bytes first.
                    work = t / ('clone-work-%s-%s-%s' % (writer, mode, reader))
                    shutil.copytree(out, work)
                    wsrc = clone_src(t / ('clone-src-%s-%s-%s' % (writer, mode, reader)))
                    with open(wsrc / 'lr/0000/x', 'r+b') as f:
                        f.write(b'changed')
                    os.utime(wsrc / 'lr/0000/x', ns=(1_800_000_000_000_000_000, 1_800_000_000_000_000_000))
                    tool(reader, 'pack-tree', wsrc, work, '--clones', *flags)
                    lr0, pm0 = work / 'lr/0000.sqlar', work / 'pm/0000.sqlar'
                    check(clone_tables(lr0) == ([], []), seen + ': y materialized and x\'s back-references gone')
                    check([n for (n, *_) in clone_tables(pm0)[0]] == ['q-pm'], seen + ': x-pm materialized, q-pm still a clone')
                    got = subprocess.run([builds[reader], 'cat', work / 'lr/0000/y'], capture_output=True, env=env).stdout
                    check(got == x_bytes, seen + ': y keeps the old bytes')
                    wcontainers = sorted(work.glob('*/*.sqlar'))
                    for other in builds:
                        check(all(l.endswith('members') for l in tool(other, 'verify', *wcontainers).splitlines()),
                              seen + ': verified by %s after the change' % other)
                    for c in wcontainers:
                        rel = str(c.relative_to(work))
                        both = (tables(c)[0], clone_tables(c))
                        check(after_reference.setdefault(rel, both) == both, seen + ': %s after the change differs' % rel)
                    back = t / ('clone-back-%s-%s-%s' % (writer, mode, reader))
                    u = json.loads(tool(reader, 'unpack-tree', work, back).splitlines()[-1])
                    check(u == {'containers': 3, 'files': 7, 'cloned': 1, 'copied': 0}, seen + ': unpack after the change %r' % u)
                    check(tree(back) == tree(wsrc), seen + ': restores the changed tree')

print('PASS%d packdir cross controls (%s)' % (checks, ', '.join(builds)))
