#!/usr/bin/env python3
"""Explicit serial performance acceptance. Run alone after correctness gates.

Builds fixtures and invokes compare-cpu.py / prewarm-startup.py sequentially.
Does not change translator defaults, install anything, or use the user AOT cache.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--attesor', required=True, type=Path)
    p.add_argument('--output-dir', required=True, type=Path,
                   help='New or empty directory; artifacts and raw JSON reports are retained')
    p.add_argument('--runs', type=int, default=7)
    p.add_argument('--pressure-count', type=int, default=200000)
    p.add_argument('--branch-count', type=int, default=2000000)
    p.add_argument('--tbi-count', type=int, default=10000000)
    p.add_argument('--region-count', type=int, default=200000)
    a = p.parse_args()
    if a.runs < 3 or min(a.pressure_count, a.branch_count, a.tbi_count, a.region_count) < 1:
        p.error('--runs must be >=3 and workload counts must be positive')
    translator = a.attesor.resolve(strict=True)
    if not translator.is_file() or not os.access(translator, os.X_OK):
        p.error('--attesor must be an executable file')
    out = a.output_dir.resolve()
    if out.exists() and (not out.is_dir() or any(out.iterdir())):
        p.error('--output-dir must be new or empty')
    out.mkdir(parents=True, exist_ok=True)
    root = Path(__file__).resolve().parents[1]
    fixtures = out / 'fixtures'; fixtures.mkdir()
    # Preserve explicit guest loader locations only. Inherited toggles, tracing,
    # native/GPU bridges, prewarm sentinels and cache settings would bias results.
    env = {k: v for k, v in os.environ.items()
           if not k.startswith('ATTESOR_') and k not in
           {'PREWARM_SENTINEL', 'DYLD_INSERT_LIBRARIES', 'DYLD_LIBRARY_PATH', 'DYLD_FALLBACK_LIBRARY_PATH'}}
    for key in ['ATTESOR_SYSROOT', 'ATTESOR_DYLD']:
        if key in os.environ:
            env[key] = os.environ[key]
    metadata = dict(translator=str(translator), translator_sha256=digest(translator),
                    runs=a.runs, status='running', fixtures={}, stages=[],
                    driver_sha256={name: digest(root / 'bench' / name) for name in
                                   ['serial-acceptance.py', 'compare-cpu.py', 'prewarm-startup.py']},
                    clang_version=subprocess.run(['clang', '--version'], text=True, capture_output=True,
                                                 check=True).stdout,
                    scope='Serial same-binary toggle comparisons; run without concurrent CPU tests. '
                          'Short process wall times include dyld/libc startup. '
                          'No result is a general application performance claim.',
                    inherited_loader_environment={k: v for k, v in env.items() if k.startswith('ATTESOR_')})
    manifest = out / 'manifest.json'

    def save():
        manifest.write_text(json.dumps(metadata, indent=2) + '\n')

    def run(label, command):
        command = [str(x) for x in command]
        row = dict(label=label, command=command, status='running')
        metadata['stages'].append(row); save()
        print(label, flush=True)
        with (out / (label + '.log')).open('w') as log:
            subprocess.run(command, cwd=root, env=env, stdout=log,
                           stderr=subprocess.STDOUT, check=True)
        row['status'] = 'passed'; save()

    try:
        with tempfile.TemporaryDirectory(prefix='attesor-serial-private-') as private:
            # compare-cpu uses --no-aot; this additionally prevents any incidental
            # cache access from reaching the user's default directory. Prewarm's
            # driver allocates its own independent temporary cache directories.
            env.update(ATTESOR_CACHE=str(Path(private) / 'cache'),
                       ATTESOR_NO_AOT_BUILD='1', ATTESOR_AOT_CHECKPOINT='0',
                       ATTESOR_JIT_OFF='', ATTESOR_JIT_ON='')
            specs = {
                'pressure': ('tests/attesor/nextuse-pressure.c', True),
                'short': ('tests/attesor/aot-prewarm-probe.c', True),
                'mixed': ('bench/native-ratio/kernels.c', False),
                'directbranch': ('tests/attesor/direct-branch.c', True),
                'tbimask': ('tests/attesor/tbi-chain.c', True),
                'regioncache': ('tests/attesor/regioncache.c', True),
            }
            for name, (source, semantic) in specs.items():
                binaries = {}
                for arch in ['arm64', 'x86_64']:
                    exe = fixtures / (name + '-' + arch)
                    run('build-' + name + '-' + arch,
                        ['clang', '-arch', arch, '-O2', '-ffp-contract=off', root / source, '-o', exe])
                    binaries[arch] = dict(path=str(exe), sha256=digest(exe))
                metadata['fixtures'][name] = dict(source=source, source_sha256=digest(root / source),
                    binaries=binaries, semantic_output_expected=semantic,
                    semantic_limitation=None if semantic else
                    'Ordinary kernels print timing rows only, without checksum. Native reference '
                    'checks process success and the empty semantic output only; it does not prove '
                    'numerical equivalence. Separate correctness gates are required.')
                save()

            def compare(name, workload, args, off, on):
                fixture = metadata['fixtures'][workload]
                run(name, [sys.executable, root / 'bench/compare-cpu.py',
                    '--before', translator, '--after', translator,
                    '--guest', fixture['binaries']['arm64']['path'],
                    '--reference', fixture['binaries']['x86_64']['path'],
                    '--runs', a.runs, '--output', out / (name + '.json'),
                    '--before-env=ATTESOR_JIT_OFF=' + off,
                    '--before-env=ATTESOR_JIT_ON=',
                    '--after-env=ATTESOR_JIT_OFF=',
                    '--after-env=ATTESOR_JIT_ON=' + on,
                    *['--guest-arg=' + str(v) for v in args]])
                report = json.loads((out / (name + '.json')).read_text())
                if fixture['semantic_output_expected'] and not report['semantic_output_present']:
                    raise RuntimeError(name + ': expected semantic output is missing')
                if not report.get('reference'):
                    raise RuntimeError(name + ': native reference record is missing')

            compare('nextuse-pressure', 'pressure', [a.pressure_count], 'nextuse', '')
            compare('nextuse-short', 'short', [], 'nextuse', '')
            compare('nextuse-mixed', 'mixed', [], 'nextuse', '')
            compare('directbranch', 'directbranch', ['--bench', a.branch_count], 'directbranch', '')
            compare('tbimask', 'tbimask', [a.tbi_count], 'tbimask', '')
            compare('regioncache', 'regioncache', [a.region_count], '', 'regioncache')
            run('short-prewarm', [sys.executable, root / 'bench/prewarm-startup.py',
                '--attesor', translator, '--runs', a.runs, '--output', out / 'short-prewarm.json'])
            metadata['status'] = 'passed'; save()
    except BaseException as exc:
        metadata.update(status='failed', error=str(exc)); save()
        raise


if __name__ == '__main__':
    main()
