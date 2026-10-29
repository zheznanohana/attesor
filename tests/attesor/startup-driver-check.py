#!/usr/bin/env python3
"""Validate startup timing orchestration without opening any real application."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time


def main():
    repo = Path(__file__).resolve().parents[2]
    with tempfile.TemporaryDirectory(prefix='attesor startup driver ', dir='/tmp') as td:
        root = Path(td)
        fake = root / 'translator'
        fake.write_text('''#!/usr/bin/python3
import json,os,sys,time
from pathlib import Path
Path(os.environ['CAPTURE']).write_text(json.dumps({'argv':sys.argv[1:],'pid':os.getpid()}))
mode=os.environ.get('PROBE_MODE','ready')
if mode=='exit':
 print('directed failure',flush=True);sys.exit(7)
if mode=='ready':print('TEST_READY',flush=True)
time.sleep(30)
''')
        fake.chmod(0o755)
        app = root / 'program with spaces'
        app.touch()
        capture = root / 'capture.json'
        env = {**os.environ, 'ATTESOR': str(fake), 'CAPTURE': str(capture)}
        for mode, extra in [('ready', []), ('ready', ['--gpu', 'one argument']),
                            ('exit', []), ('timeout', [])]:
            start = time.monotonic()
            r = subprocess.run(['bash', str(repo / 'bench/app-startup.sh'), '--timeout', '1',
                                'TEST_READY', str(app)] + extra,
                               env={**env, 'PROBE_MODE': mode}, capture_output=True,
                               text=True, timeout=15)
            elapsed = time.monotonic() - start
            assert r.returncode == (0 if mode == 'ready' else 1), (mode, r)
            assert 'unbound variable' not in r.stderr, r.stderr
            record = json.loads(capture.read_text())
            assert str(app) in record['argv']
            if extra:
                assert record['argv'][0] == '--gpu' and 'one argument' in record['argv']
            if mode == 'ready':
                assert 'median: ready' in r.stdout and '1/1 runs' in r.stdout, r.stdout
            elif mode == 'exit':
                assert 'EXIT 7 before ready' in r.stdout and 'TIMEOUT' not in r.stdout, r.stdout
                assert elapsed < 5
            else:
                assert 'TIMEOUT' in r.stdout and 'median: ready' not in r.stdout, r.stdout
            try:
                os.kill(record['pid'], 0)
            except ProcessLookupError:
                pass
            else:
                raise AssertionError('benchmark left its private child alive')
            print('PASS startup driver', mode, extra)


if __name__ == '__main__':
    main()
