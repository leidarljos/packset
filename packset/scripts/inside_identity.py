#!/usr/bin/env python3
"""Workspace and peer identity for the shared memory pack.

Same human, same repository, every client. Default workspace is the
normalized git remote. Strategies match the interchange spec.
"""
from __future__ import annotations

import getpass
import json
import os
import re
import subprocess
from pathlib import Path
from typing import Any
from urllib.parse import urlparse

SCHEMA = "inside.identity/v1"
STRATEGIES = ("per-repo", "per-directory", "per-session", "global")

_SSH = re.compile(r"^(?:ssh://)?(?:git@)?([^/:]+)[:/](.+?)(?:\.git)?$")


def normalize_remote(url: str) -> str:
    raw = (url or "").strip()
    if not raw:
        raise ValueError("empty git remote")
    parsed = urlparse(raw)
    if parsed.scheme in {"http", "https", "ssh"} and parsed.netloc:
        host = parsed.hostname or parsed.netloc
        path = parsed.path.lstrip("/").removesuffix(".git")
        return f"git:{host}/{path}"
    match = _SSH.match(raw)
    if match:
        host, path = match.group(1), match.group(2).removesuffix(".git")
        return f"git:{host}/{path}"
    raise ValueError(f"unrecognized git remote: {raw}")


def git_remote(cwd: Path | None = None) -> str | None:
    root = cwd or Path.cwd()
    try:
        proc = subprocess.run(
            ["git", "-C", str(root), "remote", "get-url", "origin"],
            check=False,
            capture_output=True,
            text=True,
        )
    except OSError:
        return None
    if proc.returncode != 0:
        return None
    url = (proc.stdout or "").strip()
    return url or None


def resolve_workspace(
    cwd: Path | None = None,
    strategy: str = "per-repo",
    session_id: str | None = None,
    remote: str | None = None,
) -> str:
    if strategy not in STRATEGIES:
        raise ValueError(f"unknown workspace_strategy: {strategy}")
    root = (cwd or Path.cwd()).resolve()
    if strategy == "global":
        return "global"
    if strategy == "per-session":
        if not session_id:
            raise ValueError("per-session workspace needs a session_id")
        return f"session:{session_id}"
    if strategy == "per-directory":
        return f"dir:{root}"
    url = remote if remote is not None else git_remote(root)
    if url:
        return normalize_remote(url)
    return f"dir:{root}"


def user_peer(explicit: str | None = None) -> str:
    if explicit and explicit.strip():
        return explicit.strip()
    env = (os.environ.get("GROK_INSIDE_USER_PEER") or "").strip()
    if env:
        return env
    return getpass.getuser()


def agent_peer(harness: str, profile: str | None = None) -> str:
    name = (harness or "").strip()
    if not name:
        raise ValueError("harness is required")
    prof = (profile or "").strip()
    if prof and prof != "default":
        return f"{name}.{prof}"
    return name


def identity(
    *,
    cwd: Path | None = None,
    strategy: str = "per-repo",
    harness: str,
    profile: str | None = None,
    session_id: str | None = None,
    turn: int = 0,
    user: str | None = None,
    remote: str | None = None,
) -> dict[str, Any]:
    root = (cwd or Path.cwd()).resolve()
    workspace = resolve_workspace(
        cwd=root, strategy=strategy, session_id=session_id, remote=remote
    )
    return {
        "schema": SCHEMA,
        "workspace": workspace,
        "workspace_strategy": strategy,
        "user_peer": user_peer(user),
        "agent_peer": agent_peer(harness, profile),
        "harness": harness,
        "profile": (profile or "") or "",
        "session_id": session_id,
        "cwd": str(root),
        "turn": int(turn),
    }


def load_defaults(path: Path) -> dict[str, Any]:
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError:
        return {}
    except (OSError, json.JSONDecodeError) as exc:
        raise RuntimeError(f"cannot read {path}: {exc}") from exc
    return payload if isinstance(payload, dict) else {}


def workspace_slug(workspace: str) -> str:
    slug = re.sub(r"[^A-Za-z0-9._-]+", "_", workspace).strip("_")
    return slug or "workspace"
