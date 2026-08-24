"""Packset HTTP and table model. No Textual, no WorkGraph."""

from __future__ import annotations

import json
import os
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Any

DEFAULT_URL = "http://127.0.0.1:8761"
BASE_COLUMNS = ("id", "kind", "text", "ts")


def packset_url() -> str:
    for key in ("PACKSET_URL", "INSIDE_MEMORY_URL"):
        raw = (os.environ.get(key) or "").strip()
        if raw and raw.lower() != "off":
            return raw.rstrip("/")
    return DEFAULT_URL


def resolve_workspace(base: str, explicit: str | None = None) -> str:
    """Interactive default: identity, then a store that actually has atoms."""
    if explicit and explicit.strip():
        return explicit.strip()
    chosen = workspace_name(base)
    atoms, err = fetch_atoms(base, chosen)
    if err or atoms:
        return chosen
    names = [
        str(row["name"])
        for row in list_workspaces(base, extra=[chosen, "global"])
        if row.get("name") and row["name"] != chosen and int(row.get("live") or 0) > 0
    ]
    if "global" in names:
        return "global"
    return names[0] if names else chosen


def workspace_name(base: str | None = None) -> str:
    raw = (os.environ.get("PACKSET_WORKSPACE") or "").strip()
    if raw and raw.lower() != "off":
        return raw
    root = (os.environ.get("GROKOS_WORKSPACE") or "").strip()
    cwd = Path(root) if root else Path.cwd()
    return workspace_from_identity(base or packset_url(), cwd)


def workspace_from_identity(base: str, cwd: Path) -> str:
    abs_cwd = cwd.resolve()
    q = urllib.parse.urlencode({"cwd": str(abs_cwd)})
    url = f"{base.rstrip('/')}/v1/identity?{q}"
    try:
        with urllib.request.urlopen(url, timeout=2) as resp:
            body = json.loads(resp.read().decode())
    except (urllib.error.URLError, TimeoutError, json.JSONDecodeError, OSError):
        return f"dir:{abs_cwd}"
    if isinstance(body, dict):
        ws = body.get("workspace")
        if isinstance(ws, str) and ws.strip():
            return ws.strip()
    return f"dir:{abs_cwd}"


def list_workspaces(
    base: str,
    extra: list[str] | None = None,
) -> list[dict[str, Any]]:
    """Workspace ids the writer knows, plus extras (global, current, identity)."""
    by_name: dict[str, int] = {}
    url = f"{base.rstrip('/')}/v1/workspaces"
    try:
        with urllib.request.urlopen(url, timeout=2) as resp:
            body = json.loads(resp.read().decode())
    except (urllib.error.URLError, TimeoutError, json.JSONDecodeError, OSError):
        body = {}
    rows = body.get("workspaces") if isinstance(body, dict) else None
    if isinstance(rows, list):
        for row in rows:
            if isinstance(row, dict):
                name = str(row.get("name") or "").strip()
                if name:
                    by_name[name] = int(row.get("live") or 0)
            elif isinstance(row, str) and row.strip():
                by_name[row.strip()] = by_name.get(row.strip(), 0)
    for name in extra or []:
        cleaned = name.strip()
        if cleaned:
            by_name.setdefault(cleaned, 0)
    by_name.setdefault("global", 0)
    return [{"name": name, "live": by_name[name]} for name in sorted(by_name)]


def fetch_atoms(base: str, workspace: str) -> tuple[list[dict[str, Any]], str]:
    q = urllib.parse.urlencode({"workspace": workspace})
    url = f"{base.rstrip('/')}/v1/atoms?{q}"
    try:
        with urllib.request.urlopen(url, timeout=2) as resp:
            body = json.loads(resp.read().decode())
    except (urllib.error.URLError, TimeoutError, json.JSONDecodeError, OSError) as exc:
        return [], str(exc)
    atoms = body.get("atoms") if isinstance(body, dict) else None
    if not isinstance(atoms, list):
        return [], "bad atoms payload"
    return [a for a in atoms if isinstance(a, dict)], ""


def bump_ts(base: str, workspace: str, atom_id: str) -> tuple[dict[str, Any] | None, str]:
    url = f"{base.rstrip('/')}/v1/atoms/update"
    payload = json.dumps(
        {"workspace": workspace, "id": atom_id, "fields": {}}
    ).encode()
    req = urllib.request.Request(
        url, data=payload, method="POST", headers={"Content-Type": "application/json"}
    )
    try:
        with urllib.request.urlopen(req, timeout=2) as resp:
            body = json.loads(resp.read().decode())
    except (urllib.error.URLError, TimeoutError, json.JSONDecodeError, OSError) as exc:
        return None, str(exc)
    if not isinstance(body, dict):
        return None, "bad update payload"
    return body, ""


def table_columns(atoms: list[dict[str, Any]]) -> tuple[str, ...]:
    cols = list(BASE_COLUMNS)
    if any("urgency" in atom for atom in atoms):
        cols.append("urgency")
    return tuple(cols)


def table_cell(atom: dict[str, Any], column: str) -> str:
    if column == "id":
        return str(atom.get("id") or "")
    if column == "kind":
        return str(atom.get("kind") or "")
    if column == "text":
        return str(atom.get("text") or "").replace("\n", " ")
    if column == "ts":
        return str(atom.get("ts") or "")
    if column == "urgency":
        urgency = atom.get("urgency")
        return "" if urgency is None else str(urgency)
    return ""


def table_row(atom: dict[str, Any], columns: tuple[str, ...]) -> tuple[str, ...]:
    return tuple(table_cell(atom, column) for column in columns)


def dump_table(atoms: list[dict[str, Any]]) -> str:
    columns = table_columns(atoms)
    lines = ["\t".join(columns)]
    for atom in atoms:
        lines.append("\t".join(table_row(atom, columns)))
    return "\n".join(lines) + "\n"
