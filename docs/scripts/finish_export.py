#!/usr/bin/env python3
"""Finish the org to RST export: restore the toctree, refuse a corrupting shape.

Two failures here are quiet rather than loud, which is why they are checked
instead of trusted:

- Pandoc drops a trailing ``rst`` export block often enough that a build comes
  out with every guide orphaned and no way to reach it from the index.
- A line that begins with ``[12]`` is a footnote *definition* in RST. When the
  citation markers in this project's prose land at column zero, docutils eats
  the surrounding sentence into a footnote nobody references. The build still
  succeeds; the page loses text.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

# The pages a site has are the org files it has, index aside, in a fixed order.
_ORG = Path(__file__).resolve().parents[1] / "orgmode"
_ORDER = ["getting-started", "howto", "reference", "explanation", "architecture"]
PAGES = [p for p in _ORDER if (_ORG / f"{p}.org").is_file()] + sorted(
    q.stem for q in _ORG.glob("*.org") if q.stem not in _ORDER and q.stem != "index"
)

TOCTREE = """

.. toctree::
   :maxdepth: 1
   :caption: Guides
   :hidden:

""" + "".join(f"   {page}\n" for page in PAGES)

_FOOTNOTE_DEF = re.compile(r"^\[\d+\]")
_ORG_TITLE = re.compile(r"^#\+title:\s*(.*)$", re.IGNORECASE)
_ORG_DESC = re.compile(r"^#\+description:\s*(.*)$", re.IGNORECASE)


def org_fields(path: Path) -> tuple[str, str]:
    title = ""
    description = ""
    for line in path.read_text(encoding="utf-8").splitlines():
        if match := _ORG_TITLE.match(line):
            title = match.group(1).strip()
        elif match := _ORG_DESC.match(line):
            description = match.group(1).strip()
    return title, description


def stamp_page(rst: Path, org: Path) -> None:
    """Put the org title and description where Sphinx reads them.

    Pandoc keeps ``#+title`` and ``#+description`` as metadata and does not
    write them into the body, so the first section becomes the page title.
    """
    title, description = org_fields(org)
    missing = []
    if not title:
        missing.append("#+title")
    if not description:
        missing.append("#+description")
    if missing:
        print(f"{org}: missing {', '.join(missing)}", file=sys.stderr)
        raise SystemExit(1)
    text = rst.read_text(encoding="utf-8")
    if text.startswith(".. title::"):
        print(f"{rst.name}: title already stamped")
        return
    # A field list here becomes docinfo. Sphinx lifts it into page
    # metadata and does not render it.
    block = f".. title:: {title}\n\n:description: {description}\n\n"
    rst.write_text(block + text, encoding="utf-8")
    print(f"{rst.name}: title and description stamped")


def restore_toctree(index: Path) -> None:
    text = index.read_text(encoding="utf-8")
    if ".. toctree::" in text:
        print("index.rst: toctree present")
        return
    index.write_text(text.rstrip("\n") + TOCTREE, encoding="utf-8")
    print("index.rst: toctree restored")


def bracket_leading_lines(path: Path) -> list[tuple[int, str]]:
    return [
        (number, line)
        for number, line in enumerate(path.read_text(encoding="utf-8").split("\n"), 1)
        if _FOOTNOTE_DEF.match(line)
    ]


def main() -> int:
    source = Path("docs/source")
    index = source / "index.rst"
    if not index.is_file():
        print("missing docs/source/index.rst after export", file=sys.stderr)
        return 1
    restore_toctree(index)

    missing = [page for page in PAGES if not (source / f"{page}.rst").is_file()]
    if missing:
        print(f"missing exported pages: {', '.join(missing)}", file=sys.stderr)
        return 1
    stamp_page(index, _ORG / "index.org")
    for page in PAGES:
        stamp_page(source / f"{page}.rst", _ORG / f"{page}.org")

    failed = False
    for org in sorted(Path("docs/orgmode").glob("*.org")):
        for number, line in bracket_leading_lines(org):
            print(
                f"{org}:{number}: a line starting with a citation marker becomes an "
                f"RST footnote and swallows the sentence; rewrap it\n    {line}",
                file=sys.stderr,
            )
            failed = True
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
