#!/usr/bin/env python3
"""Summarize bounded loop CFG metadata and 1-ms exact-block-PC samples.
Run with ATTESOR_JIT_ON=loopcensus, ATTESOR_PROFILE=<private directory>,
--no-aot, and stderr redirected to <log>. '?' means non-Mach-O code, not a
proof of V8 ownership. Nested samples are assigned to the smallest loop.
"""
import argparse
from collections import Counter
from pathlib import Path
p = argparse.ArgumentParser(description=__doc__)
p.add_argument('profiles', type=Path)
p.add_argument('log', type=Path)
a = p.parse_args()
loops = {}
for line in a.log.read_text().splitlines():
    if not line.startswith('loop-census\t'): continue
    _, header, image, blocks, edges, words = line.split('\t')
    # Latest translation at an address wins (V8 may rewrite its code).
    loops[int(header, 16)] = dict(image=image, blocks=int(blocks), edges=edges,
                                 words={int(w, 16) for w in words.split(',')})
pcs = Counter()
for f in a.profiles.glob('*.txt'):
    for line in f.read_text().splitlines():
        row = line.split('\t')
        if row[0] == 'pc': pcs[int(row[1], 16)] += int(row[2])
counts = Counter()
for pc, n in pcs.items():
    candidates = [h for h, l in loops.items() if pc in l['words']]
    if candidates: counts[min(candidates, key=lambda h: len(loops[h]['words']))] += n
print('exec_pc_samples', sum(pcs.values()), 'loop_samples', sum(counts.values()))
for label, headers in [('all', list(loops)), ('non-Mach-O', [h for h,l in loops.items() if l['image'] == '?'])]:
    hot = [h for h in headers if counts[h]]
    multi = [h for h in hot if loops[h]['blocks'] > 1]
    print(label, 'discovered', len(headers), 'hot(>=1 sample)', len(hot), 'multi', len(multi),
          'block_hist', dict(sorted(Counter(loops[h]['blocks'] for h in multi).items())),
          'loop_samples', sum(counts[h] for h in hot), 'multi_samples', sum(counts[h] for h in multi),
          'backedge_shapes', dict(Counter(e.split(':')[1] for h in multi for e in loops[h]['edges'].split(','))))
print('samples\theader\timage\tblocks\tbackedges')
for h,n in counts.most_common():
    l = loops[h]
    print(f"{n}\t{h:x}\t{l['image']}\t{l['blocks']}\t{l['edges']}")
