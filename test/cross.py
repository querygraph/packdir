#!/usr/bin/env python3
"""Controls across the builds of packdir: every build packs a tree in every
mode, and every other build must read it, list it and restore it exactly, run
the same file commands to the same output, carry on from its state and recover
its crash. Synthetic data only. Pass each build as --build NAME=PATH."""
import argparse, hashlib, json, os, pathlib, random, shutil, sqlite3, subprocess, sys, tempfile

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

print('PASS%d packdir cross controls (%s)' % (checks, ', '.join(builds)))
