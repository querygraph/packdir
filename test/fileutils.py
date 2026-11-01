#!/usr/bin/env python3
"""Controls for packdir's GNU-style file commands (ls, cat, stat, du,
find, rm, cp, mv) over a packed tree. GNU coreutils is the oracle: each
command on the packed tree must print what the GNU command prints on the
same tree unpacked (LC_ALL=C). Synthetic data in a private temporary
directory only."""
import argparse, os, pathlib, random, shutil, subprocess, sys, tempfile

p = argparse.ArgumentParser()
p.add_argument('--packdir', required=True)
a = p.parse_args()
a.packdir = os.path.abspath(a.packdir)
GNU = {n: shutil.which('g' + n) or '/usr/local/bin/g' + n for n in ('ls', 'cat', 'stat', 'du', 'rm', 'cp')}
checks = 0

def check(ok, message):
    global checks
    if not ok:
        sys.exit('FAIL: ' + message)
    checks += 1

env = dict(os.environ, LC_ALL='C', TZ='America/Los_Angeles')

def run(cmd, cwd):
    r = subprocess.run(cmd, cwd=cwd, env=env, capture_output=True, text=True, errors='surrogateescape')
    return r.returncode, r.stdout, r.stderr

rng = random.Random(23)
with tempfile.TemporaryDirectory(prefix='packdir-fileutils-') as td:
    t = pathlib.Path(td).resolve()
    plain, packed = t / 'plain', t / 'packed'
    for leaf in ('0/0000', 'a/0a00'):
        d = plain / leaf; d.mkdir(parents=True)
        for i, size in enumerate([0, 900, 1023, 1024, 1536, 10240, 10241, 2_500_000]):
            f = d / ('%08x-%d_%d' % (rng.getrandbits(32), i, rng.choice([320, 1024])))
            f.write_bytes(rng.randbytes(size)) if size else f.write_bytes(b'')
            f.chmod(rng.choice([0o644, 0o600, 0o755]))
            ns = rng.choice([1_700_000_000_123_456_789, 1_790_000_000_987_654_321]) + i * 1_000_000_001
            os.utime(f, ns=(ns, ns))
        (d / '.hidden_320').write_text('a dot file\n'); os.utime(d / '.hidden_320', ns=(1_780_000_000_000_000_001,) * 2)
    (plain / '0/0000/lines_320').write_text('one\n\ntwo\nthree\n')
    os.utime(plain / '0/0000/lines_320', ns=(1_785_000_000_000_000_000,) * 2)
    r = subprocess.run([a.packdir, 'pack-tree', str(plain), str(packed)], capture_output=True, text=True)
    check(r.returncode == 0, 'pack-tree: ' + r.stderr)
    for f in ('.packdir-state', '.packdir-pending', '.packdir-writer.lock'):
        (packed / f).unlink(missing_ok=True)
    check(not (packed / '0/0000').exists() and (packed / '0/0000.sqlar').is_file(), 'the leaves are packed')

    def same(args, gnu, tool=None, what=None, keep=lambda l: True):
        rc1, o1, e1 = run([a.packdir, tool or gnu] + args, packed)
        rc2, o2, e2 = run([GNU[gnu]] + args, plain)
        l1 = [l for l in o1.splitlines() if keep(l)]; l2 = [l for l in o2.splitlines() if keep(l)]
        check(rc1 == rc2 and l1 == l2, '%s %s differs from GNU:\n--- ours (rc %d)\n%s%s--- GNU (rc %d)\n%s%s' %
              (what or gnu, ' '.join(args), rc1, o1, e1, rc2, o2, e2))
    not_total = lambda l: not l.startswith('total ')
    files_only = lambda l: l.startswith('-')

    # ls: names, long listings, sorts, recursion, directories.
    same(['0/0000'], 'ls')
    same(['-A', '0/0000'], 'ls')
    same(['-1', 'a/0a00', '0/0000'], 'ls')
    for opt in (['-l', '--full-time'], ['-lh', '--full-time'], ['-lS', '--full-time'], ['-lt', '--full-time'],
                ['-ltr', '--full-time'], ['-lSr', '--full-time'], ['-lA', '--full-time'], ['-l']):
        same(opt + ['0/0000'], 'ls', keep=files_only)
    same(['-R', '.'], 'ls')
    same(['-d', '0/0000', 'a'], 'ls')
    same(['0/0000/lines_320', 'a/0a00'], 'ls')
    same(['-l', '--full-time', 'nonexistent'], 'ls')

    # cat, stat, du, find.
    same(['0/0000/lines_320'], 'cat')
    same(['-n', '0/0000/lines_320'], 'cat')
    same(['-b', '0/0000/lines_320'], 'cat')
    big = sorted((plain / 'a/0a00').iterdir(), key=lambda f: -f.stat().st_size)[0].name
    same(['a/0a00/' + big], 'cat')
    fmt = '%n|%s|%a|%04a|%A|%Y|%y|%F|%U|%G|%u|%g'
    same(['-c', fmt] + ['0/0000/' + n for n in sorted(os.listdir(plain / '0/0000'))], 'stat')
    same(['--printf', '%n %s\\n', '0/0000/lines_320'], 'stat')
    # GNU du lists in directory order, ours by name: compare the file lines as sets.
    rc1, o1, _ = run([a.packdir, 'du', '-ab', '.'], packed); rc2, o2, _ = run([GNU['du'], '-ab', '.'], plain)
    files = lambda o: sorted(l for l in o.splitlines() if (plain / l.split('\t')[1]).is_file())
    check(rc1 == rc2 == 0 and files(o1) == files(o2), 'du -ab file lines:\n%s\n---\n%s' % (o1, o2))
    rc1, o1, _ = run([a.packdir, 'du', '-ah', '0/0000'], packed); rc2, o2, _ = run([GNU['du'], '-ah', '--apparent-size', '0/0000'], plain)
    check(files(o1) == files(o2), 'du -ah matches GNU --apparent-size:\n%s\n---\n%s' % (o1, o2))
    rc, o, _ = run([a.packdir, 'du', '-sb', '.'], packed)
    check(o.split('\t')[0] == str(sum(f.stat().st_size for f in plain.rglob('*') if f.is_file())), 'du -sb sums the files: ' + o)
    for fexpr in (['-type', 'f'], ['-name', '*_320'], ['-type', 'd'], ['-maxdepth', '1'], ['-name', '.h*']):
        rc1, o1, _ = run([a.packdir, 'find', '.'] + fexpr, packed)
        rc2, o2, _ = run(['find', '.'] + fexpr, plain)
        check(sorted(o1.splitlines()) == sorted(o2.splitlines()), 'find %s:\n%s\n---\n%s' % (fexpr, o1, o2))

    def verify(*cs):
        r = subprocess.run([a.packdir, 'verify', *map(str, cs)], capture_output=True, text=True)
        check(r.returncode == 0, 'containers verify: ' + r.stdout)

    # cp: a member out to a plain file (-p keeps mode and time), a plain file into a container.
    member = sorted(os.listdir(plain / 'a/0a00'))[3]
    rc, o, e = run([a.packdir, 'cp', '-pv', 'a/0a00/' + member, 'copied'], packed)
    check(rc == 0 and (packed / 'copied').read_bytes() == (plain / 'a/0a00' / member).read_bytes()
          and (packed / 'copied').stat().st_mtime_ns == (plain / 'a/0a00' / member).stat().st_mtime_ns
          and (packed / 'copied').stat().st_mode == (plain / 'a/0a00' / member).stat().st_mode,
          'cp -p out of a container: ' + o + e)
    rc2, o2, _ = run([GNU['cp'], '-pv', 'a/0a00/' + member, 'copied'], plain)
    check(o == o2, 'cp -v message matches GNU: %r / %r' % (o, o2))
    (t / 'newfile').write_bytes(b'into the container')
    rc, o, e = run([a.packdir, 'cp', str(t / 'newfile'), 'a/0a00/new_320'], packed)
    check(rc == 0 and run([a.packdir, 'cat', 'a/0a00/new_320'], packed)[1] == 'into the container', 'cp into a container: ' + e)
    verify(packed / 'a/0a00.sqlar')

    # mv within a container renames the member; mv out moves it to a plain file.
    rc, o, e = run([a.packdir, 'mv', 'a/0a00/new_320', 'a/0a00/renamed_320'], packed)
    names = run([a.packdir, 'ls', 'a/0a00'], packed)[1].split()
    check(rc == 0 and 'renamed_320' in names and 'new_320' not in names, 'mv within a container: ' + e)
    rc, o, e = run([a.packdir, 'mv', 'a/0a00/renamed_320', 'moved_out'], packed)
    check(rc == 0 and (packed / 'moved_out').read_bytes() == b'into the container'
          and 'renamed_320' not in run([a.packdir, 'ls', 'a/0a00'], packed)[1].split(), 'mv out of a container: ' + e)
    verify(packed / 'a/0a00.sqlar')

    # rm: members, refusals and messages as GNU's, then a whole packed directory.
    shutil.copytree(plain, t / 'plain2'); shutil.copytree(packed, t / 'packed2')
    for args in (['-v', '0/0000/lines_320'], ['nonexistent'], ['-f', 'nonexistent'], ['0/0000']):
        rc1, o1, e1 = run([a.packdir, 'rm'] + args, t / 'packed2')
        rc2, o2, e2 = run([GNU['rm']] + args, t / 'plain2')
        check(rc1 == rc2 and o1 == o2 and e1.split(': ', 1)[-1] == e2.split(': ', 1)[-1],
              'rm %s: ours %d %r %r, GNU %d %r %r' % (args, rc1, o1, e1, rc2, o2, e2))
    check('lines_320' not in run([a.packdir, 'ls', '0/0000'], t / 'packed2')[1].split(), 'rm removed the member')
    verify(t / 'packed2/0/0000.sqlar')
    rc, o, e = run([a.packdir, 'rm', '-r', '0/0000'], t / 'packed2')
    check(rc == 0 and not (t / 'packed2/0/0000.sqlar').exists() and '0000' not in run([a.packdir, 'ls', '0'], t / 'packed2')[1],
          'rm -r removes a packed directory: ' + e)

print('PASS%d packdir fileutils controls' % checks)
