#!/usr/bin/env python3
"""Offline policy validator/generator and serial local A/B evidence collector.

Never installs a policy, edits the L2 allowlist, or promotes microbenchmark-only
wins to apps. See docs/AUTOTUNE.md for evidence gates and limitations.
"""
import argparse
import hashlib
import importlib.util
import json
import math
import os
from pathlib import Path
import re
import statistics
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
OPT_INS = {'callret', 'regioncache'}
MAX_BYTES = 65536
ID = re.compile(r'[A-Za-z0-9_-]+(?:\.[A-Za-z0-9_-]+)+', re.ASCII)


def allowlist():
    source = (ROOT / 'crates/attesor/src/policy.rs').read_text()
    match = re.search(r'pub const L2_ALLOWLIST: &\[&str\] = &\[([^\]]*)\];', source)
    if not match:
        raise ValueError('cannot read compiled-in allowlist declaration')
    tokens = re.findall(r'"([a-z0-9]+)"', match[1])
    if set(tokens) - OPT_INS:
        raise ValueError('unrecognized compiled-in opt-in')
    return set(tokens)


def unique(pairs):
    result = {}
    for k, v in pairs:
        if k in result:
            raise ValueError('duplicate JSON key: ' + k)
        result[k] = v
    return result


def read_json(path, bounded=False):
    with Path(path).open('rb') as stream:
        data = stream.read(MAX_BYTES + 1) if bounded else stream.read()
    if bounded and len(data) > MAX_BYTES:
        raise ValueError('policy exceeds 64 KiB')
    return json.loads(data.decode('utf-8'), object_pairs_hook=unique,
                      parse_constant=lambda x: (_ for _ in ()).throw(ValueError('nonfinite JSON')))


def valid_id(value):
    return isinstance(value, str) and len(value) <= 255 and ID.fullmatch(value) is not None


def validate(policy):
    if not isinstance(policy, dict):
        raise ValueError('policy must be a bundle-id -> token-list object')
    for bundle, tokens in policy.items():
        if not valid_id(bundle):
            raise ValueError('invalid bundle identifier')
        if not isinstance(tokens, list) or any(not isinstance(t, str) or t not in OPT_INS for t in tokens):
            raise ValueError('unknown opt-in token or invalid token list')
        if len(tokens) != len(set(tokens)):
            raise ValueError('duplicate opt-in token')
    if len((json.dumps(policy) + '\n').encode()) > MAX_BYTES:
        raise ValueError('policy exceeds 64 KiB')
    return policy


def profiles(directory):
    spec = importlib.util.spec_from_file_location('profile_report', ROOT / 'tools/profile-report.py')
    reader = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(reader)
    files = sorted(Path(directory).glob('*.txt'))
    if not files:
        raise ValueError('no ATTESOR_PROFILE .txt data')
    states = {}
    for path in files:
        _, report = reader.read_profile(path)
        for state, count in report.states.items():
            states[state] = states.get(state, 0) + count
    if states.get('jit', 0) + states.get('interp', 0) <= 0:
        raise ValueError('no guest execution samples')
    return dict(files=[dict(path=str(p), sha256=hashlib.sha256(p.read_bytes()).hexdigest()) for p in files], states=states)


def evaluate(token, report):
    """Require native semantic evidence, >=5 alternating pairs and consistent >=5% gain.

    This is a screening rule, NOT L2 approval, nor proof of app-level benefit.
    Raw pairs are recomputed: do not trust a supplied summary or passed flag.
    """
    if token not in OPT_INS:
        raise ValueError('not an opt-in token: ' + token)
    hashes = report.get('sha256', {})
    if (any(not re.fullmatch(r'[0-9a-f]{64}', hashes.get(k, '')) for k in ('before', 'after', 'guest'))
            or hashes['before'] != hashes['after']):
        raise ValueError('A/B must use the same hashed translator and identified guest')
    samples = report.get('samples', [])
    reference = report.get('reference')
    if not report.get('semantic_output_present') or not reference:
        return dict(eligible=False, reason='missing native semantic reference/checksum')
    def semantic(text):
        return '\n'.join(line for line in text.splitlines() if not re.fullmatch(r'\S+\s+[\d.]+ ms', line))
    expected = semantic(reference.get('stdout', ''))
    if not expected:
        return dict(eligible=False, reason='empty native semantic output')
    if len(samples) < 10 or len(samples) % 2:
        return dict(eligible=False, reason='need at least five complete alternating pairs')
    ratios = []
    command = samples[0].get('command')
    if not command or '--no-aot' not in command:
        raise ValueError('A/B requires explicit no-AOT commands')
    for i in range(0, len(samples), 2):
        pair = samples[i:i + 2]
        order = ['before', 'after'] if (i // 2) % 2 == 0 else ['after', 'before']
        if [s.get('build') for s in pair] != order:
            raise ValueError('non-alternating A/B samples')
        values = {}
        for sample in pair:
            side = sample['build']
            if sample.get('command') != command:
                raise ValueError('A/B command differs')
            env = sample.get('environment', {})
            if env != {'ATTESOR_JIT_ON': token if side == 'after' else '',
                       'ATTESOR_JIT_OFF': '', 'ATTESOR_AUTOTUNE': '0'}:
                raise ValueError('confounded A/B environment')
            if semantic(sample.get('stdout', '')) != expected:
                raise ValueError('semantic output differs from native reference')
            value = sample.get('wall_ms')
            if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value) or value <= 0:
                raise ValueError('invalid wall time')
            values[side] = value
        ratios.append(values['after'] / values['before'])
    gain = 100 * (1 - statistics.median(ratios))
    return dict(eligible=gain >= 5 and all(r < 1 for r in ratios),
                reason='screening only; still requires app-specific L2 approval',
                paired_time_reduction_percent=gain, pairs=len(ratios))


def generate(args):
    if not valid_id(args.bundle_id):
        raise ValueError('invalid bundle identifier')
    evidence = profiles(args.profiles)
    decisions = {}
    permitted = allowlist()
    for setting in args.ab:
        token, sep, filename = setting.partition('=')
        if not sep or token in decisions:
            raise ValueError('--ab requires unique TOKEN=report.json')
        decisions[token] = evaluate(token, read_json(filename))
        decisions[token].update(report=filename, sha256=hashlib.sha256(Path(filename).read_bytes()).hexdigest(),
                                l2_allowed=token in permitted)
    policy = validate({args.bundle_id: sorted(t for t, d in decisions.items() if d['eligible'] and t in permitted)})
    # Explicit output only; do not touch the user's live policy.
    Path(args.output).write_text(json.dumps(policy, indent=2) + '\n')
    result = dict(policy=policy, profile=evidence, decisions=decisions,
                  l2_allowlist=sorted(permitted), scope='Caller binds profiles/A-B to bundle ID; microbenchmarks alone do not establish app gains.')
    if args.report:
        Path(args.report).write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result, indent=2))


def measure(args):
    out = Path(args.output_dir).resolve()
    out.mkdir(parents=True, exist_ok=True)
    if any(out.iterdir()):
        raise ValueError('measurement output directory must be empty')
    binary = Path(args.attesor).resolve()
    env = {k: v for k, v in os.environ.items() if (not k.startswith(('ATTESOR_', 'DYLD_', 'MCL_')) or k in ('ATTESOR_SYSROOT', 'ATTESOR_DYLD')) and k != 'LD_PRELOAD'}
    env.update(ATTESOR_AUTOTUNE='0', ATTESOR_JIT_ON='', ATTESOR_JIT_OFF='')
    manifest = dict(binary=str(binary), sha256=hashlib.sha256(binary.read_bytes()).hexdigest(), jobs=[],
                    limitations=['Timing-only C kernels cannot establish semantic equivalence or app-specific wins.'])
    def run(label, command, extra=None):
        try:
            r = subprocess.run([str(c) for c in command], env={**env, **(extra or {})},
                               text=True, capture_output=True, timeout=1800)
            row = dict(name=label, command=[str(c) for c in command], returncode=r.returncode,
                       stdout=r.stdout, stderr=r.stderr)
        except subprocess.TimeoutExpired:
            row = dict(name=label, command=[str(c) for c in command], timeout=True, returncode=None)
        manifest['jobs'].append(row)
        (out / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
        if row['returncode'] != 0:
            raise ValueError('measurement command failed: ' + label)
    workloads = []
    for name, source in [('bench', ROOT / 'bench/bench.c'), ('kernels', ROOT / 'bench/native-ratio/kernels.c')]:
        for arch in ('arm64', 'x86_64'):
            run('build-' + name + '-' + arch, ['clang', '-arch', arch, '-O2', source, '-o', out / (name + '-' + arch)])
        workloads.append((name, out / (name + '-arm64'), out / (name + '-x86_64'), []))
    js = ROOT / 'docs/tasks/phase2/js.js'
    if js.exists() and args.js_engine and args.js_reference:
        workloads.append(('js', Path(args.js_engine).resolve(), Path(args.js_reference).resolve(), [str(js)]))
    else:
        manifest['limitations'].append('JS not measured: requires docs/tasks/phase2/js.js, --js-engine ARM Node and --js-reference native Node.')
    run('profile-kernels', [binary, '--no-aot', out / 'kernels-arm64'], {'ATTESOR_PROFILE': str(out / 'profiles')})
    for token in args.tokens:
        for name, guest, reference, guest_args in workloads:
            command = [sys.executable, ROOT / 'bench/compare-cpu.py', '--before', binary, '--after', binary,
                       '--guest', guest, '--reference', reference, '--runs', args.runs,
                       '--output', out / (token + '-' + name + '.json')]
            for side in ('before', 'after'):
                for setting in ['ATTESOR_AUTOTUNE=0', 'ATTESOR_JIT_OFF=',
                                'ATTESOR_JIT_ON=' + (token if side == 'after' else '')]:
                    command += ['--' + side + '-env', setting]
            for value in guest_args:
                command += ['--guest-arg', value]
            run(token + '-' + name, command)
    (out / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
    print('Evidence saved to ' + str(out))


def main():
    p = argparse.ArgumentParser(description=__doc__)
    sub = p.add_subparsers(dest='action', required=True)
    v = sub.add_parser('validate')
    v.add_argument('policy')
    g = sub.add_parser('generate')
    g.add_argument('--profiles', required=True)
    g.add_argument('--bundle-id', required=True)
    g.add_argument('--ab', action='append', default=[], metavar='TOKEN=REPORT')
    g.add_argument('--output', required=True)
    g.add_argument('--report')
    m = sub.add_parser('measure')
    m.add_argument('--attesor', required=True)
    m.add_argument('--output-dir', required=True)
    m.add_argument('--runs', type=int, default=5)
    m.add_argument('--tokens', nargs='+', choices=sorted(OPT_INS), default=sorted(OPT_INS))
    m.add_argument('--js-engine')
    m.add_argument('--js-reference')
    args = p.parse_args()
    try:
        if args.action == 'validate':
            policy = validate(read_json(args.policy, bounded=True))
            blocked = sorted({t for ts in policy.values() for t in ts} - allowlist())
            print(json.dumps(dict(valid=True, l2_blocked=blocked)))
        elif args.action == 'generate':
            generate(args)
        else:
            if args.runs < 5:
                raise ValueError('--runs must be at least five')
            measure(args)
    except (OSError, ValueError, IndexError, TypeError, KeyError) as error:
        p.error(str(error))


if __name__ == '__main__':
    main()
