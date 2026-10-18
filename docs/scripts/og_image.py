#!/usr/bin/env python3
"""Build the Open Graph card from the packset mark.

Reads ``docs/source/_static/mark.svg`` (the logo the site already uses) and
writes ``docs/source/_static/og.png``, 1200 by 630, the mark centered on the
paper background. Requires ``rsvg-convert`` from librsvg.
"""

from __future__ import annotations

import struct
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
MARK = ROOT / "docs" / "source" / "_static" / "mark.svg"
OUT = ROOT / "docs" / "source" / "_static" / "og.png"

WIDTH = 1200
HEIGHT = 630
MARK_PX = 480
PAPER = "#F6F2E8"


def card_svg(mark: str) -> str:
    start = mark.find("<svg")
    if start < 0:
        raise SystemExit(f"{MARK}: not an svg")
    open_end = mark.find(">", start)
    close = mark.rfind("</svg>")
    if open_end < 0 or close < 0:
        raise SystemExit(f"{MARK}: svg element did not close")
    inner = mark[open_end + 1 : close]
    x = (WIDTH - MARK_PX) // 2
    y = (HEIGHT - MARK_PX) // 2
    return (
        '<?xml version="1.0" encoding="UTF-8"?>\n'
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{WIDTH}" height="{HEIGHT}" '
        f'viewBox="0 0 {WIDTH} {HEIGHT}">\n'
        f'  <rect width="{WIDTH}" height="{HEIGHT}" fill="{PAPER}"/>\n'
        f'  <svg x="{x}" y="{y}" width="{MARK_PX}" height="{MARK_PX}" viewBox="0 0 200 200">\n'
        f"{inner}\n"
        "  </svg>\n"
        "</svg>\n"
    )


def png_size(path: Path) -> tuple[int, int]:
    data = path.read_bytes()
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        raise SystemExit(f"{path}: not a png")
    return struct.unpack(">II", data[16:24])


def main() -> int:
    if not MARK.is_file():
        print(f"missing {MARK}", file=sys.stderr)
        return 1
    svg = card_svg(MARK.read_text(encoding="utf-8"))
    with tempfile.NamedTemporaryFile(suffix=".svg") as handle:
        handle.write(svg.encode("utf-8"))
        handle.flush()
        try:
            subprocess.run(
                ["rsvg-convert", "-w", str(WIDTH), "-h", str(HEIGHT), "-o", str(OUT), handle.name],
                check=True,
            )
        except FileNotFoundError:
            print("og_image: rsvg-convert is not installed (librsvg)", file=sys.stderr)
            return 1
    width, height = png_size(OUT)
    if (width, height) != (WIDTH, HEIGHT):
        print(f"og_image: {OUT} is {width}x{height}, wanted {WIDTH}x{HEIGHT}", file=sys.stderr)
        return 1
    print(f"wrote {OUT.relative_to(ROOT)} ({WIDTH}x{HEIGHT})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
