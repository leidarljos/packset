#!/usr/bin/env python3
"""Median and interquartile range of the jo0g write traces.

Quartiles use statistics.quantiles(n=4, method="inclusive"), the same cut
as measurements/start-moqq/summary.py. A stage time is the milliseconds
the writer recorded for that write. Wall rate is the probe's write_req_s.
"""

import statistics
import sys
from pathlib import Path

STAGES = [
    "total",
    "prepare",
    "snapshot",
    "dedup",
    "replace",
    "links",
    "upsert",
    "project",
    "cap",
]


def parse_log(path: Path):
    rate = None
    rows = []
    for line in path.read_text().splitlines():
        if line.startswith("probe_writes="):
            for field in line.split():
                key, value = field.split("=", 1)
                if key == "write_req_s":
                    rate = float(value)
        if " live=" not in line or line.startswith("#"):
            continue
        rec = {}
        for field in line.split()[1:]:
            key, value = field.split("=", 1)
            if key == "live":
                rec[key] = int(value)
            else:
                rec[key] = float(value[:-2] if value.endswith("ms") else value)
        rows.append(rec)
    return rate, rows


def iqr(values):
    xs = sorted(values)
    if len(xs) < 2:
        return xs[0], xs[0], xs[0]
    q = statistics.quantiles(xs, n=4, method="inclusive")
    return xs[len(xs) // 2], q[0], q[2]


def main() -> None:
    root = Path(sys.argv[1])
    groups = {}
    for path in sorted(root.glob("*.log")):
        # <size>-<side>-<fs>-<draw>.log
        name = path.name[: -len(".log")]
        prefix, _draw = name.rsplit("-", 1)
        rate, rows = parse_log(path)
        groups.setdefault(prefix, []).append((rate, rows))
    for prefix in sorted(groups):
        draws = groups[prefix]
        print(f"\n{prefix}  draws={len(draws)}")
        rates = [rate for rate, _ in draws if rate is not None]
        if rates:
            med, lo, hi = iqr(rates)
            print(f"  write_req_s  median {med:.1f}  IQR {lo:.1f}..{hi:.1f}")
        pooled = [row for _, rows in draws for row in rows]
        print(f"  writes {len(pooled)}")
        if not pooled:
            continue
        for stage in STAGES:
            xs = [row[stage] for row in pooled if stage in row]
            if not xs:
                continue
            med, lo, hi = iqr(xs)
            print(f"  {stage:10} median {med:7.2f}  IQR {lo:.2f}..{hi:.2f}")


if __name__ == "__main__":
    main()
