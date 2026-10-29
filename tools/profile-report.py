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
        # samples, idle samples, calls, idle calls, total ns, host kernel ns
        self.calls = defaultdict(lambda: [0] * 6)
        self.bridge_selectors = Counter()
        self.blocks = self.ms = self.processes = 0

    def add(self, other):
        self.states.update(other.states)
        self.symbols.update(other.symbols)
        self.bridge_selectors.update(other.bridge_selectors)
        for key, counts in other.calls.items():
            for i, n in enumerate(counts):
                self.calls[key][i] += n
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
    if not lines or lines[0] not in ('Attesor guest profile v1', 'Attesor guest profile v2'):
        raise ValueError('not an Attesor guest profile v1/v2')
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
        elif kind == 'call':
            if len(fields) != 10:
                raise ValueError('invalid call record')
            category, number, name = fields[1:4]
            number = int(number)
            counts = list(map(int, fields[4:]))
            if any(n < 0 for n in counts) or counts[1] > counts[0] or counts[3] > counts[2] or counts[5] > counts[4]:
                raise ValueError('invalid call counters or host/emulation split')
            # Selector ids are process-local; names identify the merged rows.
            key = category, (0 if category == 'gpu-selector' else number), name
            for i, n in enumerate(counts):
                report.calls[key][i] += n
        elif kind == 'bridge-selectors':
            report.bridge_selectors[fields[1]] += 1
        elif kind == 'symbol':
            report.symbols[fields[1]] += int(fields[2])
    if name is None or not report.states:
        raise ValueError('missing complete merger records')
    if any(n < 0 for n in report.states.values()):
        raise ValueError('negative sample count')
    return name, report


def pct(n, total):
    return 100 * n / total if total else 0


def render_calls(r):
    for title, classes in (
            ('Syscalls (including idle)', {'bsd', 'mach', 'machdep'}),
            ('GPU bridge operations (caller wall time)', {'gpu-op'}),
            ('GPU selectors (bridge execution, nested in operations)', {'gpu-selector'})):
        rows = [(key, values) for key, values in r.calls.items() if key[0] in classes]
        print('\n' + title)
        print('Class/number Name                                      Samples % samples     Calls    Total ms     Host ms Emulation ms')
        total = sum(v[0] for _, v in rows)
        for (category, number, name), v in sorted(rows, key=lambda row: (-row[1][4], row[0])):
            identity = category if category == 'gpu-selector' else f'{category}/{number}'
            host = 'n/a' if category in ('gpu-op', 'gpu-selector') else f'{v[5] / 1e6:.3f}'
            emulation = 'n/a' if category in ('gpu-op', 'gpu-selector') else f'{(v[4] - v[5]) / 1e6:.3f}'
            print(f'{identity:<13} {name:<40} {v[0]:>7} {pct(v[0], total):>9.2f} '
                  f'{v[2]:>9} {v[4] / 1e6:>11.3f} {host:>11} {emulation:>12}')
    print('\nClassified idle (including zero-sample polls):')
    print('Class/number Name                                      Idle calls Idle samples')
    for (category, number, name), v in sorted(r.calls.items()):
        if v[1] or v[3]:
            print(f'{category + "/" + str(number):<13} {name:<40} {v[3]:>10} {v[1]:>12}')
    if r.bridge_selectors:
        print('GPU selector hook: ' + ', '.join(f'{key}={n} process files' for key, n in sorted(r.bridge_selectors.items())))
    print('Times are elapsed thread wall time, including waits. Host covers raw host syscalls and explicitly scoped libc kernel wrappers.')
    print('GPU kernel/emulation splits are unavailable. Selector time measures bridge execution and overlaps caller operation time.')


def render(label, r):
    total = sum(r.states.values())
    executed = r.states['jit'] + r.states['interp']
    print(f'{label} ({r.processes} process files)')
    print(f'Total samples: {total}')
    print(f'Translation: {r.blocks} blocks, {r.ms} ms')
    print('\nState                 Samples   % total')
    for state, n in sorted(r.states.items(), key=lambda x: (-x[1], x[0])):
        print(f'{state:<20} {n:>8} {pct(n, total):>9.2f}')
    render_calls(r)
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
