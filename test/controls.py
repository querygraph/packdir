#!/usr/bin/env python3
"""Controls for packdir (lib), synthetic data only.

A generated tree in the preview cache's layout (<H>/<HHHH>/<files>) is packed into
one .sqlar per leaf and checked against what stock tools and the tool itself
restore; then an incremental run (a change, an addition, a deletion kept as
retained), corruption, a crash inside a transaction, a plain directory beside
its container, and the WAL variant."""
import argparse, hashlib, json, os, pathlib, random, shutil, sqlite3, subprocess, sys, tempfile

p = argparse.ArgumentParser()
p.add_argument('--packdir', required=True, help='packdir binary (lib main.exe)')
a = p.parse_args()

checks = 0
def check(ok, message):
    global checks
    if not ok:
        sys.exit('FAIL: ' + message)
    checks += 1

def tool(*args, rc=0, env=None):
    r = subprocess.run([a.packdir, *map(str, args)], capture_output=True, text=True, env=env)
    check(r.returncode == rc, '%s exited %d (want %d):\n%s%s' % (args[0], r.returncode, rc, r.stdout, r.stderr))
    return r.stdout

def tree(root):
    return {str(f.relative_to(root)): (f.read_bytes(), f.stat().st_mtime_ns, f.stat().st_mode & 0o777)
            for f in sorted(root.rglob('*')) if f.is_file()}

rng = random.Random(1)
with tempfile.TemporaryDirectory(prefix='packdir-') as td:
    t = pathlib.Path(td).resolve()
    src, out = t / 'quasar/cache', t / 'ten/cache'
    for h in '0a':
        for leaf in ('000%s' % h, '00%s1' % h, '0%s22' % h):
            d = src / h / leaf; d.mkdir(parents=True)
            for i in range(rng.randint(3, 8)):
                f = d / ('%08x-%s_%d' % (rng.getrandbits(32), 'ab' * 8, rng.choice([320, 1024, 2048])))
                f.write_bytes(rng.randbytes(rng.randint(0, 900_000)))
                ns = 1_700_000_000_123_456_789 + rng.randint(0, 10**15)
                os.utime(f, ns=(ns, ns)); f.chmod(rng.choice([0o644, 0o600]))
    plain = tree(src)

    # First run: one container per leaf, every member intact.
    r = json.loads(tool('pack-tree', src, out).splitlines()[-1])
    containers = sorted(out.glob('*/*.sqlar'))
    check(r['barriers'] >= 1 and (out / '.packdir-pending').read_text() == '', 'a barrier, then the pending list cleared: %r' % r)
    check(r['containers'] == 6 and len(containers) == 6, 'one container per leaf: %r' % r)
    check(r['stored'] == len(plain), 'every file stored')
    check(not [p for p in out.rglob('*') if p.name.endswith(('-journal', '-wal', '-shm'))],
          'no -wal, -shm or -journal left beside the containers (DELETE journal)')
    check('ok' in tool('verify', *containers), 'verify')

    # Stock sqlite3 restores bytes and mode, mtime to the second; the tool restores exact mtimes.
    stock, exact = t / 'stock', t / 'exact'
    for c in containers:
        rel = c.relative_to(out).with_suffix('')
        (stock / rel).mkdir(parents=True)
        subprocess.run(['sqlite3', str(c), '-AxC', str(stock / rel)], check=True)
        tool('restore', c, exact / rel)
    s, e = tree(stock), tree(exact)
    check(sorted(s) == sorted(plain) and all(s[k][0] == plain[k][0] and s[k][2] == plain[k][2] for k in plain),
          'stock sqlite3 -Ax restores bytes and mode')
    check(all(s[k][1] // 10**9 == plain[k][1] // 10**9 for k in plain), 'stock restore keeps mtime to the second')
    check(e == plain, 'packdir restore is exact, nanosecond mtimes included')
    db = sqlite3.connect(containers[0])
    check(db.execute('SELECT count(*) FROM sqlar').fetchone()[0] > 0, 'read by Python stdlib sqlite3')
    check(db.execute('PRAGMA page_size').fetchone()[0] == 65536, 'page size 65536')
    check(all(sz == len(d) for sz, d in db.execute('SELECT sz, data FROM sqlar')), 'blobs stored raw')
    db.close()

    # Incremental: one change, one addition, one deletion in one leaf; others untouched.
    leaf = src / '0/0000'
    names = sorted(leaf.iterdir())
    names[0].write_bytes(b'changed level'); os.utime(names[0], ns=(1_800_000_000_000_000_001,) * 2)
    (leaf / 'added_1024').write_bytes(rng.randbytes(5000))
    gone = names[1].name; names[1].unlink()
    before = {c: (c.stat().st_mtime_ns, c.read_bytes()) for c in containers}
    r = json.loads(tool('pack-tree', src, out).splitlines()[-1])
    check(r['stored'] == 2 and r['retained'] == 1, 'incremental stores 2 and retains 1: %r' % r)
    changed = [c for c in containers if (c.stat().st_mtime_ns, c.read_bytes()) != before[c]]
    check(changed == [out / '0/0000.sqlar'], 'only the changed leaf container was written: %r' % changed)
    listing = tool('list', out / '0/0000.sqlar')
    check(any(l.startswith(gone + '\t') and l.endswith('retained') for l in listing.splitlines()), 'deletion kept as retained')
    tool('restore', out / '0/0000.sqlar', t / 'skip', '--skip-retained')
    check(not (t / 'skip' / gone).exists() and (t / 'skip/added_1024').is_file(), 'restore --skip-retained')
    r = json.loads(tool('pack-tree', src, out).splitlines()[-1])
    check(r['stored'] == 0 and r['retained'] == 0, 'a rerun with nothing new writes nothing: %r' % r)
    # Unchanged containers are not even opened: an unreadable one doesn't stop the rerun.
    locked = out / 'a/0a22.sqlar'; locked.chmod(0)
    r = json.loads(tool('pack-tree', src, out).splitlines()[-1])
    check(r['stored'] == 0, 'unchanged container left unopened: %r' % r)
    locked.chmod(0o644)

    # A crash inside a transaction leaves the container as it was; the rerun converges.
    (leaf / 'crash_a').write_bytes(b'a' * 1000); (leaf / 'crash_b').write_bytes(b'b' * 1000)
    image = (out / '0/0000.sqlar').read_bytes()
    tool('pack-tree', src, out, rc=3, env=dict(os.environ, PACKDIR_CRASH_AFTER='1'))
    check('0/0000' in (out / '.packdir-pending').read_text().split(), 'the interrupted container is pending')
    check('ok' in tool('verify', out / '0/0000.sqlar'), 'container verifies after the crash')
    check('crash_a' not in tool('list', out / '0/0000.sqlar'), 'the interrupted batch was rolled back')
    o = tool('pack-tree', src, out); r = json.loads(o.splitlines()[-1])
    check(r['stored'] == 2 and 'damaged' not in o, 'the rerun verifies the pending container and stores the batch: %r' % r)
    check((out / '.packdir-pending').read_text() == '', 'pending list cleared after the rerun')

    # Corruption: a flipped byte inside a blob is caught by its SHA-256.
    victim = out / 'a/0a22.sqlar'
    member = next(iter(sorted((src / 'a/0a22').iterdir(), key=lambda f: -f.stat().st_size)))
    data = bytearray(victim.read_bytes()); i = data.find(member.read_bytes()[1000:1064])
    check(i > 0, 'found the member bytes in the container')
    data[i] ^= 1; victim.write_bytes(bytes(data))
    out_v = tool('verify', victim, rc=1)
    check('SHA-256 mismatch' in out_v, 'flipped blob byte detected: ' + out_v)
    o = tool('pack-tree', src, out)
    aside = list(victim.parent.glob('0a22.sqlar.damaged-*'))
    check('damaged: a/0a22' in o and len(aside) == 1, 'the next run finds the damaged container and sets it aside: ' + o)
    check('ok' in tool('verify', victim), 'and packs the leaf again')
    tool('restore', victim, t / 'repacked')
    check(tree(t / 'repacked') == {k.split('/', 2)[2]: v for k, v in tree(src).items() if k.startswith('a/0a22/')},
          'the repacked container restores exactly')

    # A plain directory beside a container stays as it is; identical names are present-plain,
    # differing ones are kept updated in the container.
    keep = out / 'a/00a1'; keep.mkdir()
    first = sorted((src / 'a/00a1').iterdir())[0].name
    (keep / first).write_bytes(b'original preserved preview'); (keep / 'face.lrfprev').write_bytes(b'face')
    (src / 'a/00a1' / first).write_bytes(b'new bytes for a colliding name')
    second = sorted((src / 'a/00a1').iterdir())[1]
    shutil.copy2(second, keep / second.name)  # the migration's byte-identical copy
    o = tool('pack-tree', src, out)
    check('packed-beside-plain: a/00a1/' + first in o, 'differing same-name file reported: ' + o)
    check(any(l.startswith(first + '\t') for l in tool('list', out / 'a/00a1.sqlar').splitlines()), 'and kept in the container')
    tool('restore', out / 'a/00a1.sqlar', t / 'beside')
    check((t / 'beside' / first).read_bytes() == b'new bytes for a colliding name', 'the container holds the newer bytes')
    (src / 'a/00a1' / first).write_bytes(b'newer still'); tool('pack-tree', src, out)
    tool('restore', out / 'a/00a1.sqlar', t / 'beside2')
    check((t / 'beside2' / first).read_bytes() == b'newer still', 'and keeps updating them')
    check('present-plain: a/00a1/' + second.name in o and 'packed-beside-plain: a/00a1/' + second.name not in o,
          'identical same-name plain file is present-plain: ' + o)
    check((keep / first).read_bytes() == b'original preserved preview' and (keep / 'face.lrfprev').is_file(),
          'plain directory beside the container untouched')

    # Four workers: the same members, each container written under the one writer lock.
    par = t / 'par'
    r = json.loads(tool('pack-tree', src, par, '--workers', 4).splitlines()[-1])
    check(r['containers'] == 6 and 'ok' in tool('verify', *sorted(par.glob('*/*.sqlar'))), 'four workers: %r' % r)
    for c in sorted(par.glob('*/*.sqlar')):
        rel = c.relative_to(par).with_suffix('')
        tool('restore', c, t / 'par-restored' / rel)
    check(tree(t / 'par-restored') == {k: v for k, v in tree(src).items() if k.count('/') == 2},
          'four workers restore to the plain tree exactly')

    # Read-only commands never create a container.
    for cmd in (['verify'], ['list'], ['repack']):
        tool(*cmd, t / 'no-such.sqlar', rc=1)
    tool('restore', t / 'no-such.sqlar', t / 'x', rc=1)
    check(not (t / 'no-such.sqlar').exists(), 'a missing container is reported, never created')

    # Unpack: every container back to the plain tree it came from.
    r = json.loads(tool('unpack-tree', par, t / 'unpacked').splitlines()[-1])
    check(tree(t / 'unpacked') == {k: v for k, v in tree(src).items() if k.count('/') == 2},
          'unpack-tree restores the whole tree exactly: %r' % r)

    # Repack: same members and bytes, no growth, retained deletions dropped on request.
    c0 = out / '0/0000.sqlar'
    tool('restore', c0, t / 'before-repack')
    size0 = c0.stat().st_size
    o = tool('repack', c0)
    check('ok' in tool('verify', c0) and c0.stat().st_size <= size0, 'repack verifies and does not grow: ' + o)
    tool('restore', c0, t / 'after-repack')
    check(tree(t / 'after-repack') == tree(t / 'before-repack'), 'repack keeps every member exactly')
    retained = [l.split('\t')[0] for l in tool('list', c0).splitlines() if l.endswith('retained')]
    check(retained, 'container has retained members to drop')
    tool('repack', c0, '--drop-retained')
    check(not any(l.endswith('retained') for l in tool('list', c0).splitlines()), '--drop-retained drops them')
    (out / '0/0000.sqlar.repack').write_bytes(b'left by an interrupted repack')
    tool('repack', c0, rc=1)
    check((out / '0/0000.sqlar.repack').read_bytes() == b'left by an interrupted repack', 'a leftover .repack is refused, not overwritten')

    # WAL variant, for the benchmark: same contents, restored the same way.
    wal = t / 'wal'
    tool('pack-tree', src, wal, '--wal')
    check('ok' in tool('verify', *sorted(wal.glob('*/*.sqlar'))), 'WAL containers verify')

print('PASS%d packdir controls' % checks)
