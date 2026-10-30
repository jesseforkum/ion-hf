"""The mixed-size table from stream_sizes output: python sizes_table.py FILE..."""
import re, sys
t = {}
for f in sys.argv[1:]:
    for l in open(f, encoding='utf-8'):
        m = re.match(r'SIZE (\w+) ([+-]\d+) (\d+) (on|off) (~?)(\S+)', l)
        if m:
            v = float('inf') if m.group(6) == 'inf' else float(m.group(6))
            t[(m.group(1), int(m.group(2)), int(m.group(3)), m.group(4))] = (v, m.group(5))
sizes = sorted({k[2] for k in t})
print('| S:N | channel | ' + ' | '.join(f'{s // 1000} kB on / off (ratio)' for s in sizes) + ' |')
print('|---:|---|' + '---|' * len(sizes))
worst = 9
for snr in sorted({k[1] for k in t}, reverse=True):
    for mode in ['WGN', 'MPG', 'MPP']:
        cells = []
        for s in sizes:
            a, b = t.get((mode, snr, s, 'on')), t.get((mode, snr, s, 'off'))
            if not a or not b:
                cells.append('—'); continue
            r = b[0] / a[0] if a[0] else 0
            worst = min(worst, r)
            bpm = lambda x: f"{x[1]}{s * 60 / x[0]:.0f}" if x[0] != float('inf') else '0'
            cells.append(f"{bpm(a)} / {bpm(b)} ({r:.2f})")
        if any(c != '—' for c in cells):
            print(f'| {snr:+d} | {mode} | ' + ' | '.join(cells) + ' |')
print(f'\nworst ratio {worst:.2f}')
