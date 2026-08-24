#!/usr/bin/env python3
"""Pinned sets: a scope name on the shared seat store.

A set is the same shape as a workspace pack: optional user prose, optional
memory prose, and atoms tagged with that name. The pin is one file per
workspace so every launcher on the seat sees the same active set. Empty pin
means pack search over the whole workspace and no Remember write.
"""

from __future__ import annotations

import re
from pathlib import Path
from typing import Any

import inside_identity
import inside_memory

_SET_NAME = re.compile(r"^[a-z][a-z0-9-]{0,31}$")


def check_set_name(name: str) -> str:
    """Return a legal set id or raise AtomError."""
    raw = (name or "").strip().lower()
    if not _SET_NAME.fullmatch(raw):
        raise inside_memory.AtomError(f"bad set name: {name!r}")
    return raw


def pin_path(workspace: str, home: Path | None = None) -> Path:
    return inside_memory.workspace_dir(workspace, home) / "pin"


def set_dir(workspace: str, name: str, home: Path | None = None) -> Path:
    slug = inside_identity.workspace_slug(workspace)
    return inside_memory.memory_root(home) / "workspaces" / slug / "sets" / name


def set_user_path(workspace: str, name: str, home: Path | None = None) -> Path:
    return set_dir(workspace, name, home) / "USER.md"


def set_memory_path(workspace: str, name: str, home: Path | None = None) -> Path:
    return set_dir(workspace, name, home) / "MEMORY.md"


def set_instructions_path(workspace: str, name: str, home: Path | None = None) -> Path:
    return set_dir(workspace, name, home) / "INSTRUCTIONS.md"


def read_pin(workspace: str, home: Path | None = None) -> str:
    """Active set name, or empty when nothing is pinned."""
    raw = inside_memory.read_text(pin_path(workspace, home)).strip()
    if not raw:
        return ""
    try:
        return check_set_name(raw.splitlines()[0])
    except inside_memory.AtomError:
        return ""


def write_pin(workspace: str, name: str, home: Path | None = None) -> str:
    """Pin `name`, or clear when name is empty. Returns the stored pin."""
    path = pin_path(workspace, home)
    path.parent.mkdir(parents=True, exist_ok=True)
    if not (name or "").strip():
        path.write_text("", encoding="utf-8")
        return ""
    stored = check_set_name(name)
    path.write_text(stored + "\n", encoding="utf-8")
    return stored


def read_set(workspace: str, name: str, home: Path | None = None) -> dict[str, Any]:
    """Pack-shaped prose for one set. Missing files are empty strings."""
    stored = check_set_name(name)
    return {
        "workspace": workspace,
        "set": stored,
        "user": inside_memory.read_text(set_user_path(workspace, stored, home)),
        "memory": inside_memory.read_text(set_memory_path(workspace, stored, home)),
        "instructions": inside_memory.read_text(set_instructions_path(workspace, stored, home)),
    }


def write_set_user(workspace: str, name: str, text: str, home: Path | None = None) -> None:
    stored = check_set_name(name)
    inside_memory.write_capped(set_user_path(workspace, stored, home), text, inside_memory.USER_CAP)


def write_set_memory(workspace: str, name: str, text: str, home: Path | None = None) -> None:
    stored = check_set_name(name)
    inside_memory.write_capped(
        set_memory_path(workspace, stored, home), text, inside_memory.MEMORY_CAP
    )


def write_set_instructions(workspace: str, name: str, text: str, home: Path | None = None) -> None:
    stored = check_set_name(name)
    inside_memory.write_capped(
        set_instructions_path(workspace, stored, home),
        text,
        inside_memory.USER_CAP,
    )
