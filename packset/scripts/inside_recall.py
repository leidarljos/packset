#!/usr/bin/env python3
"""Include-first recall over live atoms.

Small packs return every live atom. Large packs walk one hop on
atom.links from the action seeds. Tombstones and expired atoms stay
out. This module reads the atom pack; it does not index a git tree.
"""
from __future__ import annotations

import re
from collections.abc import Iterable
from pathlib import Path
from typing import Any

import inside_memory

DEFAULT_LIMIT = 64
TEXT_BUDGET = 32000

_TOKEN = re.compile(r"[a-z0-9][a-z0-9_-]*")


def _trust(atom: dict[str, Any]) -> float:
    try:
        return float(atom.get("trust") if atom.get("trust") is not None else 0.0)
    except (TypeError, ValueError):
        return 0.0


def _sort_atoms(atoms: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """trust desc, ts desc, id asc. Stable."""
    out = list(atoms)
    out.sort(key=lambda atom: str(atom.get("id") or ""))
    out.sort(key=lambda atom: str(atom.get("ts") or ""), reverse=True)
    out.sort(key=lambda atom: _trust(atom), reverse=True)
    return out


def _apply_budget(
    atoms: list[dict[str, Any]], budget: int = TEXT_BUDGET
) -> list[dict[str, Any]]:
    out: list[dict[str, Any]] = []
    used = 0
    for atom in atoms:
        n = len(atom.get("text") or "")
        if out and used + n > budget:
            break
        out.append(atom)
        used += n
    return out


def _cap_limit(limit: int | None) -> int:
    if limit is None:
        return DEFAULT_LIMIT
    try:
        value = int(limit)
    except (TypeError, ValueError):
        return DEFAULT_LIMIT
    if value < 0:
        return 0
    if value > DEFAULT_LIMIT:
        return DEFAULT_LIMIT
    return value


def _live_set(
    workspace: str,
    home: Path | None,
    atoms: list[dict[str, Any]] | None,
) -> list[dict[str, Any]]:
    if atoms is None:
        return inside_memory.current_atoms(workspace, home)
    now = inside_memory.utcnow()
    live: list[dict[str, Any]] = []
    for atom in atoms:
        if not isinstance(atom, dict):
            continue
        if not inside_memory.is_live(atom, now):
            continue
        rec = dict(atom)
        rec["links"] = list(atom.get("links") or [])
        live.append(rec)
    return inside_memory.filter_live_links(live)


def _tokens(text: str) -> set[str]:
    return {tok for tok in _TOKEN.findall(text.lower()) if len(tok) >= 2}


def _hint_parts(hints: Any) -> tuple[str, set[str]]:
    if hints is None:
        return "", set()
    if isinstance(hints, str):
        return hints, set()
    if isinstance(hints, dict):
        bits = [
            str(hints.get("user_text") or ""),
            str(hints.get("text") or ""),
            str(hints.get("q") or ""),
        ]
        for name in hints.get("tool_names") or []:
            bits.append(str(name))
        entities = {
            str(item).strip()
            for item in (hints.get("entities") or [])
            if str(item).strip()
        }
        return " ".join(bits), entities
    return str(hints), set()


def _matches_hints(atom: dict[str, Any], hints: Any) -> bool:
    text, want_entities = _hint_parts(hints)
    tokens = _tokens(text)
    hay = (atom.get("text") or "").lower()
    if tokens and any(tok in hay for tok in tokens):
        return True
    atom_entities = inside_memory.extract_entities(atom)
    if want_entities and want_entities & atom_entities:
        return True
    lowered = {item.lower() for item in atom_entities}
    if tokens and any(tok in lowered for tok in tokens):
        return True
    return False


def _resolve_seeds(
    live: list[dict[str, Any]],
    seeds: Iterable[str] | None,
    hints: Any,
) -> list[str]:
    live_ids = {atom["id"] for atom in live if atom.get("id")}
    if seeds:
        out: list[str] = []
        seen: set[str] = set()
        for seed in seeds:
            sid = str(seed)
            if sid in live_ids and sid not in seen:
                out.append(sid)
                seen.add(sid)
        return out
    if hints is None or hints == "":
        return []
    return [atom["id"] for atom in live if atom.get("id") and _matches_hints(atom, hints)]


def _one_hop(
    live: list[dict[str, Any]], seed_ids: list[str]
) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
    by_id = {atom["id"]: atom for atom in live if atom.get("id")}
    seed_set = {sid for sid in seed_ids if sid in by_id}
    neighbor_ids: set[str] = set()
    for sid in seed_set:
        for lid in by_id[sid].get("links") or []:
            if lid in by_id and lid not in seed_set:
                neighbor_ids.add(lid)
    seeds = [by_id[sid] for sid in seed_ids if sid in by_id]
    neighbors = [by_id[nid] for nid in neighbor_ids]
    return seeds, neighbors


def _finish(atoms: list[dict[str, Any]], limit: int) -> list[dict[str, Any]]:
    return _apply_budget(_sort_atoms(atoms)[:limit])


def recall(
    workspace: str,
    *,
    seeds: Iterable[str] | None = None,
    hints: Any = None,
    limit: int = DEFAULT_LIMIT,
    home: Path | None = None,
    atoms: list[dict[str, Any]] | None = None,
) -> list[dict[str, Any]]:
    """Live atoms for this action. Include-first while the pack is small."""
    cap = _cap_limit(limit)
    live = _live_set(workspace, home, atoms)
    if cap == 0:
        return []
    if len(live) <= cap:
        return _finish(live, cap)
    seed_ids = _resolve_seeds(live, seeds, hints)
    if not seed_ids:
        return []
    seed_atoms, neighbors = _one_hop(live, seed_ids)
    ranked = _sort_atoms(neighbors) + _sort_atoms(seed_atoms)
    seen: set[str] = set()
    picked: list[dict[str, Any]] = []
    for atom in ranked:
        aid = atom.get("id")
        if not aid or aid in seen:
            continue
        seen.add(aid)
        picked.append(atom)
        if len(picked) >= cap:
            break
    return _apply_budget(picked)
