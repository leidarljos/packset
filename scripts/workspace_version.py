#!/usr/bin/env python3
"""The workspace version, and the dependency lines cog writes from it.

The version is written in [workspace.package] in the workspace Cargo.toml.
cog reads it here and writes the internal crates' version requirements,
which Cargo needs because those crates publish.

Ubuntu 22.04's python3 is 3.10 and has no tomllib. The release job reads
the manifest on that release, so this is a small reader for the one string
the workflows and cog need.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
CARGO_TOML = ROOT / "Cargo.toml"

# Published path dependencies. Their version requirement is the workspace
# version, so a release of one crate accepts the others from that release.
MEMBERS = ("packset-client", "packset-core")


def _table_body(text: str, name: str) -> str | None:
    found = re.search(
        r"(?ms)^\[%s\]\s*(.*?)(?=^\[|\Z)" % re.escape(name),
        text,
    )
    if not found:
        return None
    return found.group(1)


def workspace_version(path: Path = CARGO_TOML) -> str:
    text = path.read_text(encoding="utf-8")
    for name in ("workspace.package", "package"):
        body = _table_body(text, name)
        if body is None:
            continue
        version = re.search(r'(?m)^version\s*=\s*"([^"]+)"', body)
        if version:
            return version.group(1)
    raise SystemExit(f"{path} has no version")


def workspace_dependency_lines(path: Path = CARGO_TOML) -> str:
    version = workspace_version(path)
    lines = [
        f'{name} = {{ path = "crates/{name}", version = "{version}" }}'
        for name in MEMBERS
    ]
    return "\n".join(lines) + "\n"


def main() -> None:
    path = Path(sys.argv[1]) if len(sys.argv) > 1 else CARGO_TOML
    print(workspace_version(path))


if __name__ == "__main__":
    main()
