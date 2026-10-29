#!/usr/bin/env python3
"""OUTSIDE SANDBOX: headless Metal/VT integration gate, never launches a window.

Retains commands, stdout/stderr, per-run uptime, timing. No installation/shared cache writes.
"""
import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--attesor', type=Path, default=ROOT / 'target/release/attesor')
    p.add_argument('--bridge', type=Path, required=True)
    p.add_argument('--output', type=Path, required=True)
    p.add_argument('--runs', type=int, default=3)
    args = p.parse_args()
    if args.runs < 3: p.error('--runs must be >=3')
    out, bridge = args.output.resolve(), args.bridge.resolve()
    for name in ('libmclmetal.dylib', 'libmclbridge.dylib'):
        if not (bridge / name).is_file(): p.error(f'missing {bridge / name}')
    out.mkdir(parents=True, exist_ok=False)
    (out / 'tmp').mkdir()
    env = {k: v for k, v in os.environ.items() if not k.startswith(('ATTESOR_', 'MCL_', 'DYLD_'))}
    env.update(TMPDIR=str(out / 'tmp'), ATTESOR_CACHE=str(out / 'cache'), ATTESOR_NO_AOT_BUILD='1')
    if os.environ.get('ATTESOR_SYSROOT'): env['ATTESOR_SYSROOT'] = os.environ['ATTESOR_SYSROOT']
    bridged = dict(env, ATTESOR_GPU_DIR=str(bridge), ATTESOR_GPU_BRIDGE=str(bridge / 'libmclbridge.dylib'))
    summary = {'status': 'incomplete', 'runs': []}
    def finish(status, code):
        summary['status'] = status
        (out / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
        print(status + ': ' + str(out))
        return code
    def run(label, cmd, runenv):
        dest = out / label; dest.mkdir()
        (dest / 'command.json').write_text(json.dumps(list(map(str, cmd)), indent=2) + '\n')
        uptime = subprocess.check_output(['uptime'], text=True).strip()
        (dest / 'uptime.txt').write_text(uptime + '\n')
        try:
            result = subprocess.run(list(map(str, cmd)), cwd=ROOT, env=runenv, capture_output=True, text=True, timeout=240)
        except subprocess.TimeoutExpired:
            raise RuntimeError(label + ': timed out')
        (dest / 'stdout.txt').write_text(result.stdout); (dest / 'stderr.txt').write_text(result.stderr)
        summary['runs'].append(dict(label=label, returncode=result.returncode, uptime=uptime))
        if result.returncode == 77: raise NotImplementedError(label + ': no Metal device')
        if result.returncode: raise RuntimeError(label + f': exit {result.returncode}')
        print(label + ': PASS', flush=True)
        return result.stdout
    try:
        frameworks = ['Foundation', 'Metal', 'MetalFX', 'VideoToolbox', 'CoreMedia', 'CoreVideo', 'IOSurface']
        for name in ('headless', 'families', 'rtprobe', 'vtcodec'):
            for arch in ('x86_64', 'arm64'):
                binary = out / (name + '-' + arch)
                source = ROOT / ('gpu/headless.m' if name == 'headless' else f'tests/attesor/{name}.m')
                cmd = ['clang', '-arch', arch, '-O2', '-fobjc-arc']
                for framework in frameworks: cmd += ['-framework', framework]
                run('build-' + name + '-' + arch, cmd + [source, '-o', binary], env)
        run('native-metal-preflight', [out / 'headless-x86_64', '1', '1'], env)
        for name in ('families', 'rtprobe'):
            native = run(name + '-native', [out / (name + '-x86_64')], env)
            guest = run(name + '-guest', [args.attesor.resolve(), '--no-aot', '--gpu', out / (name + '-arm64')], bridged)
            if guest != native: raise RuntimeError(name + ': native/guest output differs')
        # Alternate native/software-guest/host-hardware-guest, >=3; raw timings live in stdout.
        for i in range(args.runs):
            for label, cmd, runenv in [
                ('native', [out / 'vtcodec-x86_64'], dict(env, ATTESOR_GPU_VT='1')),
                ('guest-vt-off', [args.attesor.resolve(), '--no-aot', '--gpu', out / 'vtcodec-arm64'], bridged),
                ('guest-vt-on', [args.attesor.resolve(), '--no-aot', '--gpu', out / 'vtcodec-arm64'], dict(bridged, ATTESOR_GPU_VT='1')),
            ]:
                stdout = run(f'vtcodec-{i+1:02}-{label}', cmd, runenv)
                rows = re.findall(r'^(h264|hevc): encoded 60 frames \(1 bytes, hw=([01])\) decoded 60 \(hw=([01])\) bad=0  enc [0-9]+ ms dec [0-9]+ ms$', stdout, re.M)
                if {row[0] for row in rows} != {'h264', 'hevc'} or len(rows) != 2 or not stdout.endswith('ok\n'):
                    raise RuntimeError(label + ': round-trip checks missing')
                # The bridge must use hardware at least where the host does natively (on this machine HEVC
                # is hardware, H.264 encode is software even for x86-64 programs).
                flags = {codec: (e, d) for codec, e, d in rows}
                if label == 'native':
                    native_hw = flags
                elif label == 'guest-vt-on' and any(native_hw[c][k] == '1' and flags[c][k] != '1' for c in native_hw for k in (0, 1)):
                    raise RuntimeError(f'{label}: hardware use {flags} falls short of native {native_hw}')
    except NotImplementedError as error:
        summary['reason'] = str(error); return finish('unavailable', 77)
    except (RuntimeError, subprocess.SubprocessError) as error:
        summary['reason'] = str(error); return finish('failed', 1)
    return finish('passed', 0)


if __name__ == '__main__': sys.exit(main())
