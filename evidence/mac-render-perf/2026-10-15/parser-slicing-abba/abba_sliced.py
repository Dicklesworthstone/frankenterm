#!/usr/bin/python3 -I
"""ABBA of mux_fused vs mux_fused_sliced in one ingest_throughput binary.

usage: abba_sliced.py BIN OUT.jsonl ROUNDS CORPUS[,CORPUS...]
Round r runs A then B when r is even, B then A when odd. Prints, per corpus,
the median MiB/s of each lane, the median of the per-round B/A ratios and
the range of those ratios.
"""
import json
import os
import statistics
import subprocess
import sys

BIN, OUT, ROUNDS, CORPORA = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4].split(',')
A, B = 'mux_fused', 'mux_fused_sliced'


def run(lane, corpus):
    out = subprocess.run([BIN, '--lane', lane, '--corpus', corpus], capture_output=True,
                         text=True, check=True).stdout
    rec = json.loads(out.strip().splitlines()[-1])
    assert rec['sanity'] == 'ok' and rec['lane'] == lane, rec
    return rec


with open(OUT, 'a') as log:
    for corpus in CORPORA:
        rates = {A: [], B: []}
        prints = set()
        ratios = []
        for r in range(ROUNDS):
            order = (A, B) if r % 2 == 0 else (B, A)
            got = {}
            for lane in order:
                rec = run(lane, corpus)
                log.write(json.dumps(rec) + '\n')
                got[lane] = rec['mib_per_s']
                prints.add(rec['state_fingerprint'])
            rates[A].append(got[A])
            rates[B].append(got[B])
            ratios.append(got[B] / got[A])
        print(f'{corpus}: {A} median {statistics.median(rates[A]):.1f} MiB/s, '
              f'{B} median {statistics.median(rates[B]):.1f} MiB/s; '
              f'per-round B/A median {statistics.median(ratios):.4f} '
              f'(min {min(ratios):.4f}, max {max(ratios):.4f}, {ROUNDS} rounds); '
              f'final-state fingerprints {sorted(prints)}; load {os.getloadavg()[0]:.1f}',
              flush=True)
