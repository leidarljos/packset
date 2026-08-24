#!/usr/bin/env python3
"""Bounded markdown plus append-only atoms for one workspace.

USER.md and MEMORY.md overflow is an error. Atoms are one claim each.
Deletes are tombstones. The JSONL is the log.
"""
from __future__ import annotations

import json
import re
import uuid
from collections.abc import Iterable
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

import inside_identity
import inside_prose

SCHEMA = "inside.atom/v1"
USER_CAP = 1375
MEMORY_CAP = 2200
TEXT_SOFT_CAP = 500

KINDS = frozenset(
    {
        "voice",
        "habit",
        "cache-pointer",
        "preference",
        "lesson",
        "goal",
        "conclusion",
        "card_line",
        "summary",
        "correction",
        "belief",
    }
)
LEVELS = frozenset({"explicit", "derived"})

_INVISIBLE = re.compile(r"[\u200b-\u200f\u202a-\u202e\u2060-\u206f\ufeff]")
_SECRET = re.compile(
    r"(?i)(\bapi[_-]?key\s*[=:]|\bsecret\s*[=:]|\bpassword\s*[=:]|"
    r"\btoken\s*[=:]|bearer\s+[A-Za-z0-9._\-]{8,}|sk-[A-Za-z0-9]{8,})"
)
_CAPITALIZED_RUN = re.compile(r"\b[A-Z][A-Za-z0-9]{1,}\b")
_BACKTICK_NAME = re.compile(r"`([^`]+)`")
LINK_THRESHOLD = 0.3


class MemoryOverflow(RuntimeError):
    """USER.md or MEMORY.md would exceed its character cap."""


class AtomError(ValueError):
    """Atom failed validation or the requested update was ambiguous."""


def utcnow() -> str:
    return datetime.now(UTC).strftime("%Y-%m-%dT%H:%M:%S.%f")[:-3] + "Z"


def reject_unsafe(text: str) -> str:
    if _INVISIBLE.search(text):
        raise AtomError("invisible unicode is rejected")
    if _SECRET.search(text):
        raise AtomError("credential-shaped text is rejected")
    return text


def memory_root(home: Path | None = None) -> Path:
    if home is not None:
        return Path(home)
    env = (Path.home() / ".grokinside" / "memory")
    return env


def workspace_dir(workspace: str, home: Path | None = None) -> Path:
    return memory_root(home) / "workspaces" / inside_identity.workspace_slug(workspace)


def user_path(home: Path | None = None) -> Path:
    return memory_root(home) / "USER.md"


def memory_path(workspace: str, home: Path | None = None) -> Path:
    return workspace_dir(workspace, home) / "MEMORY.md"


def atoms_path(workspace: str, home: Path | None = None) -> Path:
    return workspace_dir(workspace, home) / "atoms.jsonl"


def read_text(path: Path) -> str:
    try:
        return path.read_text(encoding="utf-8")
    except FileNotFoundError:
        return ""


def write_capped(path: Path, text: str, cap: int) -> None:
    reject_unsafe(text)
    if len(text) > cap:
        remaining = cap - len(read_text(path))
        raise MemoryOverflow(
            f"{path.name} is {len(text)} characters; cap is {cap}; "
            f"room left on disk was {max(remaining, 0)}"
        )
    if text.strip():
        inside_prose.refuse(text, role="file")
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


def set_user(text: str, home: Path | None = None) -> None:
    write_capped(user_path(home), text, USER_CAP)


def set_memory(workspace: str, text: str, home: Path | None = None) -> None:
    write_capped(memory_path(workspace, home), text, MEMORY_CAP)


def _entries(text: str) -> list[str]:
    return [p.strip() for p in re.split(r"\n\s*\n", text) if p.strip()]


def add_entry(path: Path, entry: str, cap: int) -> None:
    entry = reject_unsafe(entry.strip())
    current = read_text(path)
    if entry and entry in current:
        return
    parts = _entries(current)
    parts.append(entry)
    write_capped(path, "\n\n".join(parts) + ("\n" if parts else ""), cap)


def replace_entry(path: Path, match: str, replacement: str, cap: int) -> None:
    replacement = reject_unsafe(replacement.strip())
    current = read_text(path)
    hits = [e for e in _entries(current) if match in e]
    if len(hits) != 1:
        raise AtomError(f"replace needs exactly one match; found {len(hits)}")
    updated = current.replace(hits[0], replacement, 1)
    write_capped(path, updated, cap)


def remove_entry(path: Path, match: str, cap: int) -> None:
    current = read_text(path)
    hits = [e for e in _entries(current) if match in e]
    if len(hits) != 1:
        raise AtomError(f"remove needs exactly one match; found {len(hits)}")
    parts = [e for e in _entries(current) if e != hits[0]]
    write_capped(path, ("\n\n".join(parts) + "\n") if parts else "", cap)


def new_id() -> str:
    return str(uuid.uuid4())


def validate_atom(atom: dict[str, Any]) -> dict[str, Any]:
    kind = atom.get("kind")
    if kind not in KINDS:
        raise AtomError(f"unknown atom kind: {kind}")
    level = atom.get("level", "explicit")
    if level not in LEVELS:
        raise AtomError(f"unknown atom level: {level}")
    text = atom.get("text")
    if not isinstance(text, str) or not text.strip():
        raise AtomError("atom text is required")
    reject_unsafe(text)
    if len(text) > TEXT_SOFT_CAP:
        raise AtomError(f"atom text exceeds soft cap {TEXT_SOFT_CAP}")
    if not atom.get("workspace"):
        raise AtomError("atom workspace is required")
    raw_set = atom.get("set")
    if raw_set:
        import inside_set

        atom["set"] = inside_set.check_set_name(str(raw_set))
    elif "set" in atom:
        atom.pop("set", None)
    try:
        atom["prose"] = inside_prose.refuse(text, role="atom")
    except inside_prose.ProseError as exc:
        raise AtomError(str(exc)) from exc
    return atom


def make_atom(
    *,
    workspace: str,
    text: str,
    kind: str,
    about_peer: str,
    by_peer: str,
    level: str = "explicit",
    source: dict[str, Any] | None = None,
    valid_from: str | None = None,
    valid_to: str | None = None,
    links: Iterable[str] | None = None,
    entities: Iterable[str] | None = None,
    trust: float = 1.0,
    atom_id: str | None = None,
    set_name: str | None = None,
) -> dict[str, Any]:
    now = utcnow()
    atom = {
        "schema": SCHEMA,
        "id": atom_id or new_id(),
        "workspace": workspace,
        "about_peer": about_peer,
        "by_peer": by_peer,
        "kind": kind,
        "level": level,
        "text": text.strip(),
        "source": source,
        "ts": now,
        "valid_from": valid_from or now,
        "valid_to": valid_to,
        "links": list(links or []),
        "embedding": None,
        "trust": float(trust),
        "tombstone": False,
    }
    if entities is not None:
        atom["entities"] = [str(item) for item in entities]
    if set_name:
        atom["set"] = set_name
    return validate_atom(atom)


def make_cache_pointer(
    workspace: str,
    text: str,
    *,
    valid_to: str,
    about_peer: str,
    by_peer: str,
    source: dict[str, Any] | None = None,
) -> dict[str, Any]:
    """Dated remote snapshot. Live only while valid_to is still open."""
    return make_atom(
        workspace=workspace,
        text=text,
        kind="cache-pointer",
        about_peer=about_peer,
        by_peer=by_peer,
        valid_to=valid_to,
        source=source,
    )


def load_atoms(workspace: str, home: Path | None = None) -> list[dict[str, Any]]:
    path = atoms_path(workspace, home)
    if not path.exists():
        return []
    out: list[dict[str, Any]] = []
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line.strip():
            continue
        rec = json.loads(line)
        if isinstance(rec, dict):
            out.append(rec)
    return out


def is_live(atom: dict[str, Any], now: str | None = None) -> bool:
    """Live set: not tombstoned and (no valid_to or valid_to > now)."""
    if atom.get("tombstone"):
        return False
    valid_to = atom.get("valid_to")
    if valid_to is None or valid_to == "":
        return True
    return str(valid_to) > (now or utcnow())


def extract_entities(atom: dict[str, Any]) -> set[str]:
    """Use atom.entities when present; otherwise capitalized runs and backtick names."""
    raw = atom.get("entities")
    if raw is not None:
        return {str(item).strip() for item in raw if str(item).strip()}
    text = atom.get("text") or ""
    names = set(_CAPITALIZED_RUN.findall(text))
    names.update(part.strip() for part in _BACKTICK_NAME.findall(text) if part.strip())
    return names


def entity_jaccard(left: Iterable[str], right: Iterable[str]) -> float:
    a = set(left)
    b = set(right)
    if not a or not b:
        return 0.0
    union = a | b
    return len(a & b) / len(union)


def link_entities(
    atom: dict[str, Any],
    others: Iterable[dict[str, Any]],
    *,
    threshold: float = LINK_THRESHOLD,
    now: str | None = None,
) -> list[str]:
    """Ids of live others whose entity sets meet the Jaccard threshold both ways."""
    mine = extract_entities(atom)
    atom_id = atom.get("id")
    clock = now or utcnow()
    linked: list[str] = []
    for other in others:
        if other.get("id") == atom_id:
            continue
        if not is_live(other, clock):
            continue
        if entity_jaccard(mine, extract_entities(other)) >= threshold:
            linked.append(other["id"])
    return linked


def apply_links(
    atom: dict[str, Any],
    live: Iterable[dict[str, Any]],
    *,
    threshold: float = LINK_THRESHOLD,
    now: str | None = None,
) -> list[dict[str, Any]]:
    """Set overlap links on atom and rewrite live peers. Returns peers that changed."""
    clock = now or utcnow()
    peers = [other for other in live if other.get("id") != atom.get("id") and is_live(other, clock)]
    peer_ids = set(link_entities(atom, peers, threshold=threshold, now=clock))
    atom["links"] = sorted(peer_ids)
    atom_id = atom["id"]
    rewritten: list[dict[str, Any]] = []
    for other in peers:
        links = set(other.get("links") or [])
        should_link = other["id"] in peer_ids
        has_link = atom_id in links
        if should_link == has_link:
            continue
        if should_link:
            links.add(atom_id)
        else:
            links.discard(atom_id)
        other["links"] = sorted(links)
        rewritten.append(other)
    return rewritten


def filter_live_links(atoms: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Drop links that point outside the supplied live set."""
    live_ids = {atom["id"] for atom in atoms}
    for atom in atoms:
        links = atom.get("links") or []
        atom["links"] = [lid for lid in links if lid in live_ids]
    return atoms


def current_atoms(workspace: str, home: Path | None = None) -> list[dict[str, Any]]:
    seen: dict[str, dict[str, Any]] = {}
    for rec in load_atoms(workspace, home):
        seen[rec["id"]] = rec
    now = utcnow()
    live = [a for a in seen.values() if is_live(a, now)]
    return filter_live_links(live)


def _append(workspace: str, atom: dict[str, Any], home: Path | None = None) -> None:
    path = atoms_path(workspace, home)
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as handle:
        handle.write(json.dumps(atom, ensure_ascii=True) + "\n")


def add_atom(atom: dict[str, Any], home: Path | None = None) -> dict[str, Any]:
    atom = validate_atom(dict(atom))
    workspace = atom["workspace"]
    live = current_atoms(workspace, home)
    for existing in live:
        if existing.get("text") == atom["text"] and existing.get("kind") == atom["kind"]:
            return existing
    if "id" not in atom or not atom["id"]:
        atom["id"] = new_id()
    if "ts" not in atom:
        atom["ts"] = utcnow()
    atom["tombstone"] = False
    rewritten: list[dict[str, Any]] = []
    if is_live(atom):
        rewritten = apply_links(atom, live)
    elif "links" not in atom:
        atom["links"] = []
    _append(workspace, atom, home)
    for peer in rewritten:
        peer["ts"] = utcnow()
        _append(workspace, peer, home)
    return atom


def update_atom(
    workspace: str,
    atom_id: str,
    fields: dict[str, Any],
    home: Path | None = None,
) -> dict[str, Any]:
    current = {a["id"]: a for a in current_atoms(workspace, home)}
    if atom_id not in current:
        raise AtomError(f"no current atom {atom_id}")
    updated = dict(current[atom_id])
    updated.update(fields)
    updated["id"] = atom_id
    updated["workspace"] = workspace
    updated["ts"] = utcnow()
    updated["tombstone"] = False
    validate_atom(updated)
    rewritten: list[dict[str, Any]] = []
    if is_live(updated):
        rewritten = apply_links(updated, list(current.values()))
    elif "links" not in updated:
        updated["links"] = []
    _append(workspace, updated, home)
    for peer in rewritten:
        peer["ts"] = utcnow()
        _append(workspace, peer, home)
    return updated


def ensure_pack_files(workspace: str, home: Path | None = None) -> tuple[Path, Path]:
    """Create empty USER.md and MEMORY.md so a view has a target."""
    user = user_path(home)
    memory = memory_path(workspace, home)
    user.parent.mkdir(parents=True, exist_ok=True)
    memory.parent.mkdir(parents=True, exist_ok=True)
    if not user.exists():
        user.write_text("", encoding="utf-8")
    if not memory.exists():
        memory.write_text("", encoding="utf-8")
    return user, memory


def _link_view(dest: Path, target: Path) -> None:
    dest.parent.mkdir(parents=True, exist_ok=True)
    target = target.resolve()
    if dest.is_symlink() or dest.exists():
        dest.unlink()
    dest.symlink_to(target)


def write_pi_agents_view(dest: Path, user_text: str, memory_text: str) -> None:
    """Pi only auto-loads AGENTS.md from agentDir. Refresh each launch."""
    dest.parent.mkdir(parents=True, exist_ok=True)
    _ = user_text, memory_text
    body = (
        "# Seat memory (view)\n\n"
        "Generated by grok-inside. Same pack as memd. Not a private store.\n"
        "USER.md and MEMORY.md in this directory are the write targets.\n"
        "The shim splices matching claims. This file does not paste them.\n"
    )
    dest.write_text(body, encoding="utf-8")


def install_home_view(
    isolated_home: Path,
    *,
    layout: str,
    workspace: str = "global",
    pack_home: Path | None = None,
) -> dict[str, Path]:
    """Project USER.md and MEMORY.md into a Hermes or Pi isolated home."""
    if layout not in {"hermes", "pi"}:
        raise AtomError(f"unknown memory view layout: {layout}")
    user_src, memory_src = ensure_pack_files(workspace, pack_home)
    isolated_home = Path(isolated_home)
    if layout == "hermes":
        dest_dir = isolated_home / "memories"
        user_view = dest_dir / "USER.md"
        memory_view = dest_dir / "MEMORY.md"
        _link_view(user_view, user_src)
        _link_view(memory_view, memory_src)
        return {"user": user_view, "memory": memory_view}
    dest_dir = isolated_home / "agent"
    user_view = dest_dir / "USER.md"
    memory_view = dest_dir / "MEMORY.md"
    agents_view = dest_dir / "AGENTS.md"
    _link_view(user_view, user_src)
    _link_view(memory_view, memory_src)
    write_pi_agents_view(agents_view, read_text(user_src), read_text(memory_src))
    return {"user": user_view, "memory": memory_view, "agents": agents_view}


def delete_atom(workspace: str, atom_id: str, home: Path | None = None) -> dict[str, Any]:
    current = {a["id"]: a for a in current_atoms(workspace, home)}
    if atom_id not in current:
        raise AtomError(f"no current atom {atom_id}")
    tomb = dict(current[atom_id])
    tomb["tombstone"] = True
    tomb["ts"] = utcnow()
    _append(workspace, tomb, home)
    return tomb
