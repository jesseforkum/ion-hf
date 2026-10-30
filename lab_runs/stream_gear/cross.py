import re, sys
def load(files, arm):
    d = {}
    for f in files:
        for l in open(f, encoding='utf-8'):
            m = re.match(r'stream=(\d) (\w+) ([+-]\d+) dB seed \d+ (\d+) s: .*arrivals \[(.*)\]', l)
            if m and m.group(1) == arm:
                pts = [(float(a.split('s:')[0]), int(a.split('s:')[1])) for a in m.group(5).split()]
                d[(m.group(2), int(m.group(3)))] = (int(m.group(4)), pts)
    return d
def at(pts, t):
    v = 0
    for (s, b) in pts:
        if s <= t: v = b
    return v
on = load(sys.argv[1].split(','), '1'); off = load(sys.argv[2].split(','), '0')
for k in sorted(on, key=lambda k: (k[1], k[0])):
    if k not in off: continue
    T = min(on[k][0], off[k][0]); a, b = on[k][1], off[k][1]
    ts = sorted(set([p[0] for p in a] + [p[0] for p in b]))
    last_behind = 0.0
    for t in ts:
        if t > T: break
        if at(a, t) < at(b, t): last_behind = t
    cross = last_behind
    print(f"{k[0]} {k[1]:+d}: crossover {cross:.0f} s, off holds {at(b, cross)} B then ({at(b,cross)*60/max(cross,1):.0f} B/min), at {T}s on {at(a,T)} off {at(b,T)}")
