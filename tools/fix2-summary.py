#!/usr/bin/env python3
"""Print all-kernel medians, raw samples and ON/OFF ratios; never hide regressions."""
import argparse,collections,json,re,statistics
p=argparse.ArgumentParser(description=__doc__)
p.add_argument('results')
a=p.parse_args()
rows=json.load(open(a.results))
d=collections.defaultdict(list)
for row in rows:
    if row['rc']: raise SystemExit(f"failed command: {row}")
    for name,ms in re.findall(r'^(.+?)\s+(\d+(?:\.\d+)?) ms\s*$', row['stdout'], re.M):
        d[row['guest'],name.strip(),row['version'],row['token']].append(float(ms))
cols=[('before','off'),('before','rel32chain'),('before','sharedtrans'),('after','off'),('after','rel32chain'),('after','sharedtrans')]
print('| guest / kernel | before OFF | before rel32 | before shared | after OFF | after rel32 | after shared | rel32/OFF | shared/OFF |')
print('|---|---:|---:|---:|---:|---:|---:|---:|---:|')
for guest,name in dict.fromkeys((g,n) for g,n,v,t in d):
    samples=[d[guest,name,v,t] for v,t in cols]
    if any(len(x)<3 for x in samples):raise SystemExit(f'need >=3 samples: {guest} / {name}')
    m=[statistics.median(x) for x in samples]
    print('| '+guest+' / '+name+' | '+' | '.join(f'{v:.1f}' for v in m)+f' | {m[4]/m[3]:.3f} | {m[5]/m[3]:.3f} |')
print('\n## Raw milliseconds (all samples, no removal)')
for key,samples in d.items():print(' / '.join(key)+': '+', '.join(map(str,samples)))
