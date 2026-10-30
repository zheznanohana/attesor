#!/usr/bin/env python3
"""Offline generator validation; no user policy or environment changes."""
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location('autotune', ROOT / 'tools/autotune.py')
a = importlib.util.module_from_spec(spec)
spec.loader.exec_module(a)


class Autotune(unittest.TestCase):
    def test_validation(self):
        self.assertEqual(a.allowlist(), set())
        self.assertEqual(a.validate({'com.example.App': []}), {'com.example.App': []})
        for value in [[], None, {'*': []}, {'com..x': []}, {'com.x': 'callret'},
                      {'com.x': ['loopra']}, {'com.x': [1]}, {'com.x': ['callret', 'callret']}]:
            with self.assertRaises(ValueError):
                a.validate(value)
        with tempfile.TemporaryDirectory() as d:
            p = Path(d) / 'bad.json'
            for text in ['{"com.x":[],"com.x":[]}', '{} trailing', ' ' * 65537, '{"com.x": [NaN]}']:
                p.write_text(text)
                with self.assertRaises(ValueError):
                    a.read_json(p, bounded=True)

    def report(self):
        samples = []
        for i in range(5):
            for side in (['before', 'after'] if i % 2 == 0 else ['after', 'before']):
                samples.append(dict(command=['attesor', '--no-aot', 'guest'], build=side, wall_ms=100 if side == 'before' else 80,
                                    environment={'ATTESOR_AUTOTUNE': '0', 'ATTESOR_JIT_OFF': '',
                                                 'ATTESOR_JIT_ON': 'callret' if side == 'after' else ''},
                                    stdout='checksum abc\n'))
        return dict(sha256={k: 'a' * 64 for k in ('before', 'after', 'guest')}, samples=samples, semantic_output_present=True, reference={'stdout': 'checksum abc\n'})

    def test_evidence(self):
        r = self.report()
        self.assertTrue(a.evaluate('callret', r)['eligible'])
        self.assertFalse(a.evaluate('callret', {**r, 'reference': None})['eligible'])
        self.assertFalse(a.evaluate('callret', {**r, 'samples': r['samples'][:4]})['eligible'])
        r['samples'][1]['wall_ms'] = 101
        self.assertFalse(a.evaluate('callret', r)['eligible'])
        for bad in [0, -1, float('nan'), True]:
            r = self.report(); r['samples'][1]['wall_ms'] = bad
            with self.assertRaises(ValueError):
                a.evaluate('callret', r)
        r = self.report(); r['samples'][1]['stdout'] = 'wrong'
        with self.assertRaises(ValueError):
            a.evaluate('callret', r)
        r = self.report(); r['samples'][1]['environment']['ATTESOR_JIT_OFF'] = 'loopra'
        with self.assertRaises(ValueError):
            a.evaluate('callret', r)

    def test_generator_stays_empty_even_for_win(self):
        with tempfile.TemporaryDirectory() as d:
            d = Path(d); profiles = d / 'profiles'; profiles.mkdir()
            (profiles / '1.txt').write_text('Attesor guest profile v2\nprocess\tprobe\nstate\tjit\t100\n')
            report = d / 'ab.json'; report.write_text(json.dumps(self.report()))
            output = d / 'policy.json'
            result = subprocess.run([sys.executable, str(ROOT / 'tools/autotune.py'), 'generate',
                                     '--profiles', str(profiles), '--bundle-id', 'com.example.App',
                                     '--ab', 'callret=' + str(report), '--output', str(output)], capture_output=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(json.loads(output.read_text()), {'com.example.App': []})
            (profiles / '1.txt').write_text('not a profile')
            result = subprocess.run([sys.executable, str(ROOT / 'tools/autotune.py'), 'generate',
                                     '--profiles', str(profiles), '--bundle-id', 'com.example.App',
                                     '--output', str(output)], capture_output=True)
            self.assertNotEqual(result.returncode, 0)


if __name__ == '__main__':
    unittest.main()
