#!/usr/bin/env python3
"""Lossless merge and process-role grouping, including entries outside top-N tables."""
import contextlib
import importlib.util
import io
from pathlib import Path
import tempfile

root = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location('profile_report', root / 'tools/profile-report.py')
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
with tempfile.TemporaryDirectory(prefix='profile-report-') as td:
    path = Path(td) / 'worker-1.txt'
    path.write_text('Attesor guest profile v1\n'
                    'process\tworker\tworker\t--type=utility\t--utility-sub-type=network\n'
                    'translation\t17\t25\nstate\tjit\t60\nstate\tinterp\t40\n'
                    'state\tidle\t100\nimage\tlib\t60\t40\n'
                    'symbol\tlib`hidden-from-top-60\t100\n')
    name, report = module.read_profile(path)
    assert name == 'worker --type=utility --utility-sub-type=network'
    merged = module.Report()
    merged.add(report)
    merged.add(report)
    assert sum(merged.states.values()) == 400
    assert merged.images['lib'] == [120, 80]
    assert merged.symbols['lib`hidden-from-top-60'] == 200
    assert (merged.blocks, merged.ms, merged.processes) == (34, 50, 2)
    out = io.StringIO()
    with contextlib.redirect_stdout(out):
        module.render('Merged', merged)
        module.render('Empty', module.Report())
    assert '60.00    40.00' in out.getvalue()
    assert 'hidden-from-top-60' in out.getvalue()
    assert module.process_type('worker', ['--type', 'renderer']) == 'worker --type=renderer'
print('profile-report: PASS lossless merge, weighted shares, zero samples, process roles')

with tempfile.TemporaryDirectory(prefix='profile-report-v2-') as td:
    path = Path(td) / 'worker-1.txt'
    header = 'Attesor guest profile v2\nprocess\tworker\t--type=gpu-process\nstate\tidle\t5\nstate\tgpu\t10\n'
    records = ('call\tbsd\t363\tkevent\t0\t0\t200\t200\t900\t600\n'
               'call\tmach\t3\tmach_absolute_time\t1\t0\t200\t0\t700\t0\n'
               'call\tgpu-op\t4\tbatch\t10\t0\t3\t0\t5000\t0\n'
               'call\tgpu-selector\t7\tcommandBuffer\t8\t0\t20\t0\t4000\t0\n'
               'bridge-selectors\ttrue\n'
               'mmap-fast\t100\t75\n')
    path.write_text(header + records)
    name, a = module.read_profile(path)
    path.write_text(header + records.replace('gpu-selector\t7', 'gpu-selector\t91'))
    _, b = module.read_profile(path)
    a.add(b)
    assert a.calls['gpu-selector', 0, 'commandBuffer'] == [16, 0, 40, 0, 8000, 0]
    assert a.calls['bsd', 363, 'kevent'] == [0, 0, 400, 400, 1800, 1200]
    assert a.bridge_selectors['true'] == 2
    assert (a.mmap_attempts, a.mmap_hits) == (200, 150)
    out = io.StringIO()
    with contextlib.redirect_stdout(out):
        module.render(name, a)
    assert 'kevent' in out.getvalue() and 'Classified idle' in out.getvalue()
    assert 'commandBuffer' in out.getvalue() and 'n/a' in out.getvalue()
    assert 'mmap first-try aligned: 150/200 (75.00%' in out.getvalue()
    for bad in ('-1\t0', '1\t2', '1\t-1'):
        path.write_text(header + 'mmap-fast\t' + bad + '\n')
        try:
            module.read_profile(path)
        except ValueError:
            pass
        else:
            raise AssertionError('accepted invalid mmap counters: ' + bad)
    for bad in ('-1\t0\t1\t0\t9\t6', '0\t0\t1\t0\t9\t10', '0\t0\t1\t2\t9\t0'):
        path.write_text(header + 'call\tbsd\t20\tgetpid\t' + bad + '\n')
        try:
            module.read_profile(path)
        except ValueError:
            pass
        else:
            raise AssertionError('accepted invalid counters: ' + bad)
print('profile-report: PASS v1/v2, syscall time splits, zero-sample idle calls, selector names across processes, invalid counters')

with tempfile.TemporaryDirectory(prefix='profile-jit-cost-') as td:
    path = Path(td) / 'jit-1.txt'
    path.write_text('Attesor guest profile v2\nprocess\tnode\nstate\ttranslate-runtime\t7\n'
                    'state\tinvalidate\t3\nstate\tindirect\t2\nstate\tcall-return\t1\n'
                    'jit-metric\ttranslate.runtime.ns\t12345678\n'
                    'jit-metric\tinvalidate.ic-ivau.calls\t45\n')
    _, a = module.read_profile(path)
    _, b = module.read_profile(path)
    a.add(b)
    assert a.jit_metrics['translate.runtime.ns'] == 24691356
    assert a.jit_metrics['invalidate.ic-ivau.calls'] == 90
    assert a.states['translate-runtime'] == 14
    path.write_text('Attesor guest profile v2\nprocess\tnode\nstate\tjit\t1\njit-metric\tbad\t-1\n')
    try: module.read_profile(path)
    except ValueError: pass
    else: raise AssertionError('negative JIT metric accepted')
print('profile-report: PASS JIT phases, nanoseconds, counters, merge and validation')
