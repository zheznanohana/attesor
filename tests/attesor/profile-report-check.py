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
