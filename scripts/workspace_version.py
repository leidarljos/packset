#!/usr/bin/env python3
"""The workspace version, and the dependency lines cog writes from it.

The version is written in [workspace.package] in the workspace Cargo.toml.
cog reads it here and writes the internal crates' version requirements,
which Cargo needs because those crates publish.
"""

from __future__ import annotations

import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
CARGO_TOML = ROOT / "Cargo.toml"

# Published path dependencies. Their version requirement is the workspace
# version, so a release of one crate accepts the others from that release.
MEMBERS = ("packset-client", "packset-core")


def workspace_version(path: Path = CARGO_TOML) -> str:
    with path.open("rb") as handle:
        manifest = tomllib.load(handle)
    return manifest["workspace"]["package"]["version"]


def workspace_dependency_lines(path: Path = CARGO_TOML) -> str:
    version = workspace_version(path)
    lines = [
        f'{name} = {{ path = "crates/{name}", version = "{version}" }}'
        for name in MEMBERS
    ]
    return "\n".join(lines) + "\n"


def main() -> None:
    print(workspace_version())


if __name__ == "__main__":
    main()
