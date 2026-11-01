#!/usr/bin/env python3
"""Benchmark harness for packed directories (docs/packdir-design.md).

Copies one leaf set (<H>/<HHHH>/<files>) to a target disk by each method and
records wall time and the target disk's bytes written and I/O operations
(iostat), as JSON lines:

  plain      Unison, file per file (clone off), one way and additive like the mirror profile
  sqlar      packdir pack-tree, DELETE journal, a drive-cache barrier every 4 GiB
  sqlar-wal  packdir pack-tree --wal
  -w4, -fullfsync, -nosync    4 workers; F_FULLFSYNC per container instead; no flush at all
  cp         cp -R, then sync (first run only)
  tar        tar | tar, then sync (first run only)
  bundle     Unison into an HFS+ sparse bundle on the target

  bench.py first SRC OUT --unison U --packdir P [--methods ...]
  bench.py incremental SRC OUT --unison U --packdir P [--methods ...]
  bench.py sample DIR [--gib G]      small synthetic set for a dry run

OUT must be new for 'first', must contain 'packdir-bench' in its path, and is
the only place written (Unison's archives go in OUT's sibling .unison-home on
the same run's work directory). Nothing is ever deleted."""
import argparse, json, os, pathlib, random, re, subprocess, sys, time

FIRST = ['plain', 'sqlar', 'sqlar-wal', 'cp', 'tar', 'bundle']
INCREMENTAL = ['plain', 'sqlar', 'sqlar-wal', 'bundle']


def disk_of(path):
    dev = subprocess.run(['df', path], capture_output=True, text=True, check=True).stdout.splitlines()[1].split()[0]
    info = subprocess.run(['diskutil', 'info', dev], capture_output=True, text=True).stdout
    m = re.search(r'Part of Whole:\s+(\S+)', info)
    whole = m.group(1) if m else os.path.basename(dev)
    # An APFS volume's whole disk is a synthesized container: iostat counts its physical store.
    store = re.search(r'APFS Physical Store:\s+(disk\d+)', subprocess.run(['diskutil', 'info', whole], capture_output=True, text=True).stdout)
    return store.group(1) if store else whole


def io_totals(disk):
    """(transfers, MiB) since boot for one disk, from iostat -I."""
    lines = subprocess.run(['iostat', '-Id', disk], capture_output=True, text=True, check=True).stdout.splitlines()
    kbt, xfrs, mb = lines[-1].split()[:3]
    return float(xfrs), float(mb)


def leaf_stats(src):
    files = [f for f in pathlib.Path(src).glob('*/*/*') if f.is_file()]
    return len(files), sum(f.stat().st_size for f in files)


def unison(args, src, dst, home):
    os.makedirs(dst, exist_ok=True)
    env = dict(os.environ, UNISON=str(home))
    # The mirror profile's flow: one way and additive, nothing deleted on the target.
    flow = ['-force', str(src), '-noupdate', str(src), '-nocreation', str(src), '-nodeletion', str(src),
            '-nodeletion', str(dst)]
    with (home / 'unison.out').open('a') as log:
        r = subprocess.run([args.unison, str(src), str(dst), '-batch', '-times', '-perms', '0', '-clone', 'off',
                        '-confirmbigdel=false', *flow, '-logfile', str(home / 'unison.log')], env=env,
                       stdout=log, stderr=log)
    # Exit 1 means items were skipped: the deletions that nodeletion keeps.
    if r.returncode not in (0, 1):
        sys.exit('unison exited %d; see %s' % (r.returncode, home / 'unison.out'))


def propagation_seconds(home):
    """Unison's own time for its last propagation, from its log."""
    try:
        lines = (home / 'unison.log').read_text(errors='replace').splitlines()
    except OSError:
        return None
    for l in reversed(lines):
        m = re.search(r'finished propagating changes at .*, ([0-9.]+) s', l)
        if m:
            return float(m.group(1))
    return None


def walk(path):
    """Stat every entry under path: the scan cost of the target side alone."""
    t0, n = time.monotonic(), 0
    for root, dirs, files in os.walk(path):
        for f in dirs + files:
            os.lstat(os.path.join(root, f)); n += 1
    return round(time.monotonic() - t0, 3), n


def method(args, name, phase, src, out, home):
    dst = out / name
    if name == 'plain':
        unison(args, src, dst, home)
    elif name.startswith('sqlar'):
        # sqlar[-wal][-w4][-fullfsync|-nosync]: journal, 4 workers, and the
        # sync: default a drive-cache barrier every 4 GiB, -fullfsync one per
        # container, -nosync none.
        extra = (['--wal'] if '-wal' in name else []) + (['--workers', '4'] if '-w4' in name else [])
        sync = 'fullfsync' if '-fullfsync' in name else 'normal' if '-nosync' in name else None
        env = dict(os.environ, PACKDIR_SYNC=sync) if sync else None
        r = subprocess.run([args.packdir, 'pack-tree', str(src), str(dst), *extra], capture_output=True, text=True,
                           check=True, env=env)
        return json.loads(r.stdout.splitlines()[-1])
    elif name == 'cp':
        subprocess.run(['cp', '-Rp', str(src), str(dst)], check=True)
        subprocess.run(['sync'], check=True)
    elif name == 'tar':
        dst.mkdir()
        subprocess.run('tar -cf - -C %s . | tar -xpf - -C %s' % (sh(src), sh(dst)), shell=True, check=True)
        subprocess.run(['sync'], check=True)
    elif name == 'bundle':
        image, mount = out / 'bundle.sparsebundle', out / 'bundle-mount'
        if phase == 'first':
            subprocess.run(['hdiutil', 'create', '-quiet', '-type', 'SPARSEBUNDLE', '-fs', 'HFS+J', '-volname', 'packdir-bench',
                            '-size', '%dg' % args.bundle_gib, str(image)], check=True)
        mount.mkdir(exist_ok=True)
        subprocess.run(['hdiutil', 'attach', '-quiet', '-nobrowse', '-mountpoint', str(mount), str(image)], check=True)
        try:
            unison(args, src, mount / 'cache', home)
        finally:
            subprocess.run(['hdiutil', 'detach', '-quiet', str(mount)], check=True)
    return None


def sh(p):
    return "'" + str(p).replace("'", "'\\''") + "'"


def run(args, phase):
    src, out = pathlib.Path(args.src).resolve(), pathlib.Path(args.out).absolute()
    if 'packdir-bench' not in str(out):
        sys.exit('OUT must be inside a folder named packdir-bench')
    if phase == 'first':
        if out.exists():
            sys.exit('OUT must not exist for a first run: ' + str(out))
        out.mkdir(parents=True)
    elif not out.is_dir():
        sys.exit('no first run in ' + str(out))
    work = pathlib.Path(args.work).resolve() if args.work else out.parent / (out.name + '.work')
    work.mkdir(exist_ok=True)
    disk = disk_of(str(out))
    files, nbytes = leaf_stats(src)
    results = out / 'RESULTS.jsonl'
    # Read the source once, untimed, so every method reads it from memory alike
    # (the set fits in RAM) and the comparison is the target disk's cost.
    for f in pathlib.Path(src).rglob('*'):
        if f.is_file():
            with f.open('rb') as h:
                while h.read(1 << 20):
                    pass
    for name in args.methods or (FIRST if phase == 'first' else INCREMENTAL):
        home = work / ('unison-home-' + name)
        home.mkdir(exist_ok=True)
        x0, m0 = io_totals(disk)
        t0 = time.monotonic()
        detail = method(args, name, phase, src, out, home)
        subprocess.run(['sync'], check=True)  # what is still in memory counts too
        dt = time.monotonic() - t0
        x1, m1 = io_totals(disk)
        # Scan apart from transfer: Unison's propagation time from its log; the
        # prototype's own scan time; and a stat walk of the target copy alone.
        target = out / ('bundle.sparsebundle' if name == 'bundle' else name)
        if name in ('plain', 'bundle'):
            prop = propagation_seconds(home)
            detail = dict(propagation_seconds=prop, scan_seconds=round(dt - prop, 3) if prop is not None else None)
        walk_seconds, entries = walk(target)
        detail = dict(detail or {}, target_walk_seconds=walk_seconds, target_entries=entries)
        row = dict(phase=phase, method=name, utc=time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()), disk=disk,
                   source_files=files, source_bytes=nbytes, seconds=round(dt, 3),
                   disk_mib=round(m1 - m0, 1), disk_ios=int(x1 - x0), iops=round((x1 - x0) / dt, 1) if dt else None,
                   source_mib_per_s=round(nbytes / 1048576 / dt, 1) if dt else None, detail=detail)
        print(json.dumps(row), flush=True)
        with results.open('a') as f:
            f.write(json.dumps(row) + '\n')


def sample(args):
    """A small leaf set in the cache's layout, random (incompressible) bytes."""
    root = pathlib.Path(args.dir)
    if root.exists():
        sys.exit(str(root) + ' exists')
    rng, total, target = random.Random(1), 0, args.gib * 1024 ** 3
    n = 0
    while total < target:
        h = '%x' % (n % 16); leaf = root / h / ('%s%03x' % (h, n // 16)); leaf.mkdir(parents=True)
        for _ in range(rng.randint(31, 89)):
            size = min(int(rng.lognormvariate(13.4, 1.0)), 8 << 20)
            (leaf / ('%032x-%016x_%d' % (rng.getrandbits(128), rng.getrandbits(64), rng.choice([320, 1024, 2048, 2880])))).write_bytes(os.urandom(size))
            total += size
        n += 1
    print(json.dumps(dict(leaves=n, bytes=total)))


p = argparse.ArgumentParser()
sub = p.add_subparsers(dest='cmd', required=True)
for c in ('first', 'incremental'):
    q = sub.add_parser(c); q.add_argument('src'); q.add_argument('out')
    q.add_argument('--unison', required=True); q.add_argument('--packdir', required=True)
    q.add_argument('--methods', nargs='*'); q.add_argument('--work'); q.add_argument('--bundle-gib', type=int, default=200)
q = sub.add_parser('sample'); q.add_argument('dir'); q.add_argument('--gib', type=float, default=1)
a = p.parse_args()
if a.cmd == 'sample':
    sample(a)
else:
    run(a, a.cmd)
