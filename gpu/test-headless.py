#!/usr/bin/env python3
"""CPU-only oracle/measurement guard tests. Does not open a Metal device."""
import importlib.util
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('measure', ROOT / 'gpu/measure-headless.py')
measure = importlib.util.module_from_spec(spec)
spec.loader.exec_module(measure)
LENGTHS = (1, 7, 8, 4095, 4096, 4097, 16384, 65536)
MASK = (1 << 64) - 1


def reference(frames, seed):
    # Independently implemented integer oracle; no Foundation/Metal or guest code.
    value, checked = 14695981039346656037, 0
    def consume(byte):
        nonlocal value, checked
        value = ((value ^ byte) * 1099511628211) & MASK
        checked += 1
    def pattern(frame, item, offset):
        x = seed ^ (frame << 32) ^ (item << 24) ^ offset
        x = ((x ^ (x >> 30)) * 0xbf58476d1ce4e5b9) & MASK
        x = ((x ^ (x >> 27)) * 0x94d049bb133111eb) & MASK
        return (x ^ (x >> 31)) & 255
    for frame in range(frames):
        for item, length in enumerate(LENGTHS):
            for offset in range(length): consume(pattern(frame, item, offset))
        for offset in range(64 * 32 * 4): consume(pattern(frame, 8, offset))
        color = [255 if frame & bit else 0 for bit in (1, 2, 4)] + [255]
        for _ in range(64 * 32):
            for byte in color: consume(byte)
    return dict(frames=frames, seed=seed, checksum=f'{value:016x}', checked_bytes=checked)


class Guards(unittest.TestCase):
    def test_missing_or_ambiguous_result(self):
        line = 'RESULT mode=gpu frames=1 seed=1 checksum=0123456789abcdef checked_bytes=110608 mismatch=0\n'
        for value in ('', line.replace('mismatch=0', 'mismatch=1'), line * 2, line.replace('mode=gpu', 'mode=skip')):
            with self.assertRaises(ValueError): measure.parse_result(value)
        self.assertEqual(measure.parse_result(line)['mode'], 'gpu')

    def test_invalid_measurement(self):
        line = 'MEASURE seconds=1.000 frames_per_second=120.0 command_buffers=1080 submitted_upload_bytes=1234\n'
        self.assertEqual(measure.parse_measure(line)['command_buffers'], 1080)
        for bad in ('', line * 2, line.replace('seconds=1.000', 'seconds=0.000'), line.replace('120.0', '0.0')):
            with self.assertRaises(ValueError): measure.parse_measure(bad)

    def test_upload_counters(self):
        self.assertIsNone(measure.parse_upload_counters('baseline has no counters'))
        self.assertEqual(measure.parse_upload_counters(
            '[mclupload] snapshot_bytes=7 borrowed_bytes=8 inline_bytes=9\n'
            '[mclupload] snapshot_bytes=1 borrowed_bytes=2 inline_bytes=3\n'),
            dict(snapshot_bytes=8, borrowed_bytes=10, inline_bytes=12))

    def test_differential_sensitivity(self):
        good = reference(2, 1)
        self.assertTrue(measure.equivalent(good, good))
        for field in good:
            bad = dict(good)
            bad[field] = 'bad' if field == 'checksum' else good[field] + 1
            self.assertFalse(measure.equivalent(bad, good), field)

    def test_private_environment(self):
        with patch.dict(os.environ, {'ATTESOR_CACHE': '/never-use', 'ATTESOR_GPU_VT': '1',
                                     'ATTESOR_JIT_ON': 'unaccepted', 'MCL_NOFAST': '1',
                                     'DYLD_INSERT_LIBRARIES': '/never-load'}, clear=True):
            env = measure.clean_env(Path('/private/tmp/test'), None)
        self.assertEqual(env['ATTESOR_CACHE'], '/private/tmp/test/cache')
        for key in ('ATTESOR_GPU_VT', 'ATTESOR_JIT_ON', 'MCL_NOFAST', 'DYLD_INSERT_LIBRARIES'):
            self.assertNotIn(key, env)


class NativeOracle(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temp = tempfile.TemporaryDirectory(prefix='attesor-g-oracle-', dir='/private/tmp')
        cls.binary = Path(cls.temp.name) / 'headless'
        cls.env = dict(os.environ, TMPDIR=cls.temp.name)
        subprocess.run(['clang', '-arch', 'x86_64', '-O2', '-Wall', '-Wextra', '-Werror', '-fobjc-arc',
                        '-framework', 'Foundation', '-framework', 'Metal', str(ROOT / 'gpu/headless.m'),
                        '-o', str(cls.binary)], env=cls.env, check=True, timeout=60)

    @classmethod
    def tearDownClass(cls): cls.temp.cleanup()

    def run_oracle(self, *args):
        return subprocess.run([str(self.binary), *map(str, args)], env=self.env,
                              text=True, capture_output=True, timeout=60)

    def test_three_seeds(self):
        for seed in (1, 0x100f5e1f7e57cafe, MASK):
            p = self.run_oracle('--oracle', 3, seed)
            self.assertEqual(p.returncode, 0, p.stderr)
            result = measure.parse_result(p.stdout)
            self.assertEqual(result['mode'], 'oracle')
            self.assertTrue(measure.equivalent(result, reference(3, seed)))
            self.assertNotIn('MEASURE', p.stdout)

    def test_intentional_corruption(self):
        p = self.run_oracle('--check-oracle')
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertIn('24 intentional corruptions detected', p.stdout)

    def test_invalid_inputs(self):
        for args in (('--oracle', 0), ('--oracle', -1), ('--oracle', 100001),
                     ('--oracle', '1x'), ('--oracle', 1, -1), ('--oracle', 1, 2**64),
                     ('--oracle', 1, 1, 'extra')):
            p = self.run_oracle(*args)
            self.assertEqual(p.returncode, 2, (args, p.stdout, p.stderr))
            self.assertNotIn('RESULT', p.stdout)


if __name__ == '__main__': unittest.main(verbosity=2)
