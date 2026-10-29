#!/usr/bin/env python3
"""Merge one run's cumulative <name>-<pid>.txt guest profiles (full records, not top-N tables)."""
import argparse
from collections import Counter, defaultdict
from pathlib import Path


class Report:
    def __init__(self):
        self.states = Counter()
        self.images = defaultdict(lambda: [0, 0])
        self.symbols = Counter()
        self.blocks = self.ms = self.processes = 0

    def add(self, other):
        self.states.update(other.states)
        self.symbols.update(other.symbols)
        for image, counts in other.images.items():
            for i in range(2):
                self.images[image][i] += counts[i]
        self.blocks += other.blocks
        self.ms += other.ms
        self.processes += other.processes


def process_type(name, args):
    # Electron/Chromium puts roles in --type; utility roles need their subtype too.
    role = []
    for key in ('--type', '--utility-sub-type'):
        for i, arg in enumerate(args):
            if arg.startswith(key + '='):
                role.append(arg)
                break
            if arg == key and i + 1 < len(args):
                role.append(key + '=' + args[i + 1])
                break
    return name + (' ' + ' '.join(role) if role else '')


def read_profile(path):
    lines = path.read_text().splitlines()
    if not lines or lines[0] != 'Attesor guest profile v1':
        raise ValueError('not an Attesor guest profile v1')
    report = Report()
    report.processes = 1
    name = None
    for line in lines:
        fields = line.split('\t')
        kind = fields[0]
        if kind == 'process':
            name = process_type(fields[1], fields[2:])
        elif kind == 'translation':
            report.blocks, report.ms = map(int, fields[1:])
        elif kind == 'state':
            report.states[fields[1]] += int(fields[2])
        elif kind == 'image':
            report.images[fields[1]] = list(map(int, fields[2:]))
        elif kind == 'symbol':
            report.symbols[fields[1]] += int(fields[2])
    if name is None or not report.states:
        raise ValueError('missing complete merger records')
    if any(n < 0 for n in report.states.values()):
        raise ValueError('negative sample count')
    return name, report


def pct(n, total):
    return 100 * n / total if total else 0


def render(label, r):
    total = sum(r.states.values())
    executed = r.states['jit'] + r.states['interp']
    print(f'{label} ({r.processes} process files)')
    print(f'Total samples: {total}')
    print(f'Translation: {r.blocks} blocks, {r.ms} ms')
    print('\nState                 Samples   % total')
    for state, n in sorted(r.states.items(), key=lambda x: (-x[1], x[0])):
        print(f'{state:<20} {n:>8} {pct(n, total):>9.2f}')
    print('\nTop images (jit + interp)       Samples   % exec    % jit % interp')
    for image, (jit, interp) in sorted(r.images.items(), key=lambda x: (-sum(x[1]), x[0]))[:40]:
        n = jit + interp
        print(f'{image:<40} {n:>8} {pct(n, executed):>8.2f} '
              f'{pct(jit, n):>8.2f} {pct(interp, n):>8.2f}')
    print('\nTop symbols (jit + interp)      Samples   % exec')
    for symbol, n in sorted(r.symbols.items(), key=lambda x: (-x[1], x[0]))[:60]:
        print(f'{symbol:<60} {n:>8} {pct(n, executed):>8.2f}')
    print()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('directory', type=Path)
    args = parser.parse_args()
    files = sorted(args.directory.glob('*.txt'))
    if not files:
        parser.error('no per-process .txt profiles in directory')
    total = Report()
    groups = defaultdict(Report)
    for path in files:
        try:
            name, report = read_profile(path)
        except (OSError, ValueError, IndexError) as error:
            parser.error(f'{path}: {error}')
        total.add(report)
        groups[name].add(report)
    render('Whole app', total)
    for name, report in sorted(groups.items()):
        render('Process type: ' + name, report)


if __name__ == '__main__':
    main()
