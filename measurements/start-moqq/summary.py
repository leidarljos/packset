"""Median and interquartile range of the start logs in this directory.

Usage: python3 measurements/start-moqq/summary.py
Quartiles are statistics.quantiles(n=4, method="inclusive").
"""

import re
import statistics
from collections import defaultdict
from pathlib import Path

HERE = Path(__file__).parent


def spread(values):
    values = sorted(values)
    if len(values) < 2:
        return f"{values[0]}"
    q1, q2, q3 = statistics.quantiles(values, n=4, method="inclusive")
    return f"{q2:.10g} ({q1:.10g} to {q3:.10g})"


def tests():
    runs = defaultdict(lambda: defaultdict(list))
    for log in sorted(HERE.glob("*.log")):
        if log.name.startswith("sweep-"):
            continue
        label = re.sub(r"-\d+\.log$", "", log.name)
        text = log.read_text()
        bound = re.search(r"over a (\d+) KiB pack", text)
        for start in ("stamping", "current"):
            head = re.search(
                rf"^{start} start: a fault mapped (\d+) KiB, bound (\d+) KiB, peak (\d+) KiB, "
                rf"headroom (\d+) KiB",
                text,
                re.M,
            )
            rest = re.search(
                rf"^{start} start: (-?\d+) KiB over base after the pass by VmRSS, (-?\d+) by smaps: "
                rf"RssAnon ([+-]\d+), RssFile ([+-]\d+) and RssShmem ([+-]\d+) \(test binary ([+-]\d+), "
                rf"data file ([+-]\d+)",
                text,
                re.M,
            )
            if not head or not rest:
                continue
            got = runs[label][start]
            got.append(
                dict(
                    unit=int(head[1]),
                    bound=int(head[2]),
                    peak=int(head[3]),
                    headroom=int(head[4]),
                    pack=int(bound[1]),
                    vmrss=int(rest[1]),
                    smaps=int(rest[2]),
                    anon=int(rest[3]),
                    exe=int(rest[6]),
                    mdb=int(rest[7]),
                    failed="FAILED" in text,
                )
            )
    print(
        "| run | start | n | fault unit KiB | bound KiB | pack KiB | peak over base KiB, median (IQR) "
        "| headroom KiB, median (IQR) | after the pass, VmRSS over base | smaps over base | RssAnon "
        "| test binary | data file | test failed |"
    )
    print("|---|---|---|---|---|---|---|---|---|---|---|---|---|")
    for label, starts in runs.items():
        for start, got in starts.items():
            col = lambda key: [run[key] for run in got]
            print(
                f"| {label} | {start} | {len(got)} | {spread(col('unit'))} | {spread(col('bound'))} "
                f"| {spread(col('pack'))} | {spread(col('peak'))} | {spread(col('headroom'))} "
                f"| {spread(col('vmrss'))} | {spread(col('smaps'))} | {spread(col('anon'))} "
                f"| {spread(col('exe'))} | {spread(col('mdb'))} | {sum(col('failed'))} of {len(got)} |"
            )


def sweeps():
    for label in sorted({re.sub(r"-\d+\.log$", "", p.name) for p in HERE.glob("sweep-*.log")}):
        rows = defaultdict(list)
        for log in sorted(HERE.glob(f"{label}-*.log")):
            for line in log.read_text().splitlines():
                cells = [cell.strip() for cell in line.strip("|").split("|")]
                if len(cells) == 11 and cells[0].isdigit():
                    rows[(int(cells[0]), cells[2])].append(cells)
        print(f"\n{label}\n")
        print(
            "| atoms | data.mdb MiB | start | n | open ms | pass ms, median (IQR) | base MiB "
            "| VmRSS MiB | RssAnon MiB | RssFile MiB | VmHWM MiB, median (IQR) | peak over base KiB, median (IQR) |"
        )
        print("|---|---|---|---|---|---|---|---|---|---|---|---|")
        for (atoms, start), got in sorted(rows.items()):
            col = lambda i: [float(m[0]) for run in got if (m := re.match(r"[\d.]+", run[i]))] or ["-"]
            stopped = sum("stopped" in run[4] for run in got)
            note = f", stopped {stopped} of {len(got)}" if stopped else ""
            print(
                f"| {atoms} | {spread(col(1))} | {start} | {len(got)} | {spread(col(3))} "
                f"| {spread(col(4))}{note} | {spread(col(5))} | {spread(col(6))} | {spread(col(7))} "
                f"| {spread(col(8))} | {spread(col(9))} | {spread(col(10))} |"
            )


tests()
sweeps()
