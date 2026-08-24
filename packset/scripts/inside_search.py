#!/usr/bin/env python3
"""Ranked search over the seat pack.

Meilisearch is milli on heed/LMDB: a search layer, not the store.
When packset-milli is on PATH (terra-built), /v1/search uses that
inverted index so years of atoms stay query-cost, not corpus-cost.
Without the binary this is a linear prefix + one-edit scorer.
No second daemon. No SQL FTS.
"""
from __future__ import annotations

import hashlib
import json
import math
import os
import re
import shutil
import subprocess
from collections.abc import Iterable
from datetime import UTC, datetime
from itertools import permutations
from pathlib import Path
from typing import Any

import inside_memory

_TOKEN = re.compile(r"[a-z0-9][a-z0-9_-]*")
_PARA = re.compile(r"\n\s*\n")
_SHORT_EXACT = 4
_RECENCY_HALF_LIFE_DAYS = 14.0
_STOP = frozenset(
    {
        "a",
        "an",
        "and",
        "are",
        "as",
        "at",
        "be",
        "been",
        "being",
        "but",
        "by",
        "can",
        "could",
        "do",
        "does",
        "for",
        "from",
        "if",
        "in",
        "is",
        "it",
        "its",
        "me",
        "my",
        "no",
        "not",
        "of",
        "on",
        "or",
        "our",
        "please",
        "should",
        "than",
        "that",
        "the",
        "then",
        "these",
        "this",
        "those",
        "to",
        "was",
        "we",
        "were",
        "what",
        "when",
        "where",
        "which",
        "who",
        "will",
        "with",
        "would",
        "yes",
        "you",
        "your",
    }
)


def _tokens(text: str) -> list[str]:
    return [tok for tok in _TOKEN.findall((text or "").lower()) if tok not in _STOP]


def paragraphs(text: str) -> list[str]:
    """Split a file into paragraphs, then lines if the file is one block."""
    blob = text or ""
    parts = [part.strip() for part in _PARA.split(blob) if part.strip()]
    if len(parts) <= 1:
        parts = [part.strip() for part in blob.splitlines() if part.strip()]
    return parts


def score_text(query: str, text: str) -> float:
    """Lexical score of text against query. Stopwords dropped; short tokens exact."""
    return _text_score(_tokens(query), text)


def _edits(a: str, b: str) -> int:
    if a == b:
        return 0
    if abs(len(a) - len(b)) > 1:
        return 2
    if len(a) > len(b):
        a, b = b, a
    # a is shorter or equal
    if len(a) == len(b):
        return sum(x != y for x, y in zip(a, b, strict=True))
    # insertion into a to make b
    i = j = diffs = 0
    while i < len(a) and j < len(b):
        if a[i] == b[j]:
            i += 1
            j += 1
            continue
        diffs += 1
        j += 1
        if diffs > 1:
            return diffs
    return diffs + (len(b) - j)


def _token_score(query: str, hay: str) -> float:
    if not query or not hay:
        return 0.0
    if hay == query:
        return 4.0
    # "pr" is not a prefix of "prefers". Short queries are exact only.
    if len(query) < _SHORT_EXACT:
        return 0.0
    if hay.startswith(query):
        return 3.0
    if query in hay:
        return 2.0
    if _edits(query, hay) <= 1:
        return 1.5
    return 0.0


def _recency(ts: Any) -> float:
    """Half-life decay. Missing timestamps count as current."""
    if not ts:
        return 1.0
    try:
        when = datetime.fromisoformat(str(ts).replace("Z", "+00:00"))
    except ValueError:
        return 1.0
    if when.tzinfo is None:
        when = when.replace(tzinfo=UTC)
    days = max(0.0, (datetime.now(UTC) - when).total_seconds() / 86400.0)
    return 0.5 ** (days / _RECENCY_HALF_LIFE_DAYS)


def _text_score(query_tokens: Iterable[str], text: str) -> float:
    hay = _tokens(text)
    if not hay:
        return 0.0
    total = 0.0
    for q in query_tokens:
        best = 0.0
        for h in hay:
            best = max(best, _token_score(q, h))
        total += best
    return total


def milli_bin() -> Path | None:
    env = (
        os.environ.get("PACKSET_MILLI")
        or os.environ.get("INSIDE_MILLI")
        or os.environ.get("GROK_INSIDE_MILLI")
    )
    if env:
        path = Path(env)
        if path.is_file() and os.access(path, os.X_OK):
            return path
    here = Path(__file__).resolve().parent.parent
    for candidate in (
        here / "bin" / "packset-milli",
        here / "crates" / "packset-milli" / "target" / "release" / "packset-milli",
        here / "bin" / "inside-milli",
    ):
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return candidate
    found = shutil.which("packset-milli") or shutil.which("inside-milli")
    return Path(found) if found else None


def milli_index_dir(home: Path | None = None) -> Path:
    env = os.environ.get("INSIDE_MILLI_INDEX")
    if env:
        return Path(env)
    root = Path(home) if home is not None else inside_memory.memory_root()
    return root / "memory.milli"


def document_id(field: str, *, workspace: str = "", atom_id: str = "") -> str:
    """milli primary keys are [A-Za-z0-9_-] only."""
    if field == "user":
        return "user"
    if field == "memory":
        digest = hashlib.sha256(workspace.encode("utf-8")).hexdigest()[:16]
        return f"memory_{digest}"
    return str(atom_id)


def pack_documents(pack: dict[str, Any]) -> list[dict[str, Any]]:
    """JSON documents milli indexes. Live atoms only."""
    workspace = str(pack.get("workspace") or "")
    docs: list[dict[str, Any]] = []
    user = pack.get("user") or ""
    if user:
        docs.append(
            {
                "id": document_id("user", workspace=workspace),
                "field": "user",
                "kind": "user",
                "text": user,
                "entities": "",
                "workspace": workspace,
                "trust": 1.5,
            }
        )
    memory = pack.get("memory") or ""
    if memory:
        docs.append(
            {
                "id": document_id("memory", workspace=workspace),
                "field": "memory",
                "kind": "memory",
                "text": memory,
                "entities": "",
                "workspace": workspace,
                "trust": 1.25,
            }
        )
    now = inside_memory.utcnow()
    for atom in pack.get("atoms") or []:
        if not isinstance(atom, dict) or not inside_memory.is_live(atom, now):
            continue
        docs.append(atom_document(atom))
    return docs


def atom_document(atom: dict[str, Any]) -> dict[str, Any]:
    return {
        "id": document_id("atom", atom_id=str(atom.get("id") or "")),
        "field": "atom",
        "kind": str(atom.get("kind") or "atom"),
        "text": str(atom.get("text") or ""),
        "entities": " ".join(str(x) for x in (atom.get("entities") or [])),
        "workspace": str(atom.get("workspace") or ""),
        "set": str(atom.get("set") or ""),
        "trust": float(atom.get("trust") if atom.get("trust") is not None else 1.0),
    }


def _atom_in_set(atom: dict[str, Any], set_name: str | None) -> bool:
    """True when the atom belongs in this search scope."""
    if not set_name:
        return True
    return str(atom.get("set") or "") == set_name


def _live_atoms(
    pack: dict[str, Any], *, set_name: str | None = None
) -> list[dict[str, Any]]:
    now = inside_memory.utcnow()
    out: list[dict[str, Any]] = []
    for atom in pack.get("atoms") or []:
        if not isinstance(atom, dict) or not inside_memory.is_live(atom, now):
            continue
        if not _atom_in_set(atom, set_name):
            continue
        out.append(atom)
    return out


def _run_milli(argv: list[str], *, stdin: str | None = None, timeout: float = 30.0) -> dict[str, Any] | None:
    binary = milli_bin()
    if binary is None:
        return None
    try:
        proc = subprocess.run(
            [str(binary), *argv],
            input=stdin,
            text=True,
            capture_output=True,
            timeout=timeout,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    if proc.returncode != 0:
        return None
    try:
        payload = json.loads(proc.stdout or "{}")
    except json.JSONDecodeError:
        return None
    return payload if isinstance(payload, dict) else None


def replace_index(pack: dict[str, Any], index_dir: Path) -> bool:
    """Replace the projection from a workspace corpus pack.

    Callers must not pass set-scoped cards as ``user``/``memory`` — that
    would poison the shared workspace prose documents.
    """
    docs = pack_documents(pack)
    payload = _run_milli(
        ["index", "--index", str(index_dir), "--replace"],
        stdin="\n".join(json.dumps(doc, ensure_ascii=True) for doc in docs) + ("\n" if docs else ""),
        timeout=120.0,
    )
    return payload is not None


def upsert_documents(docs: list[dict[str, Any]], index_dir: Path) -> bool:
    if not docs:
        return True
    payload = _run_milli(
        ["index", "--index", str(index_dir)],
        stdin="\n".join(json.dumps(doc, ensure_ascii=True) for doc in docs) + "\n",
        timeout=60.0,
    )
    return payload is not None


def delete_documents(ids: list[str], index_dir: Path) -> bool:
    if not ids:
        return True
    payload = _run_milli(
        ["delete", "--index", str(index_dir)],
        stdin=json.dumps(ids),
        timeout=30.0,
    )
    return payload is not None


def reindex_atoms(pack: dict[str, Any], index_dir: Path) -> bool:
    """Upsert live atom documents only. Never touches workspace user/memory docs."""
    docs = [atom_document(atom) for atom in _live_atoms(pack) if atom.get("id")]
    return upsert_documents(docs, index_dir)


def _index_ready(index_dir: Path) -> bool:
    return (index_dir / "data.mdb").is_file() or (index_dir / "data.mdb").exists()


def _pack_is_set_scoped(pack: dict[str, Any], set_name: str | None) -> bool:
    return bool(set_name) or bool(pack.get("set"))


def _filter_atom_hits(
    hits: list[dict[str, Any]],
    pack: dict[str, Any],
    *,
    set_name: str | None = None,
) -> list[dict[str, Any]]:
    """Keep atom hits that are live in the pack and match the set scope.

    Pack atoms are the source of truth for ``set`` membership so a stale
    milli projection without the field still scopes correctly after filter.
    Workspace user/memory hits from the index are dropped: prose always
    comes from the pack via :func:`_prose_hits`.
    """
    live = {atom.get("id"): atom for atom in _live_atoms(pack)}
    out: list[dict[str, Any]] = []
    for hit in hits:
        if hit.get("field") in {"user", "memory"}:
            continue
        atom = live.get(hit.get("id"))
        if atom is None:
            continue
        if not _atom_in_set(atom, set_name):
            continue
        out.append(hit)
    return out


def _prose_hits(
    pack: dict[str, Any], query: str, *, limit: int
) -> list[dict[str, Any]]:
    """Score pack user/memory only. Caller puts workspace or set cards in pack."""
    return search_pack_linear(
        {
            "user": pack.get("user") or "",
            "memory": pack.get("memory") or "",
            "atoms": [],
        },
        query,
        limit=limit,
    )


def _ensure_milli_atoms(
    pack: dict[str, Any],
    directory: Path,
    *,
    set_name: str | None,
) -> bool:
    """Make the atom projection usable for this query without poisoning prose.

    Set-scoped packs never full-replace the index (set cards must not become
    workspace user/memory docs). When a set is named, backfill that set's
    atoms so ``--set`` sees the field even on older projections.
    """
    if not _index_ready(directory):
        if _pack_is_set_scoped(pack, set_name):
            return reindex_atoms(pack, directory)
        return replace_index(pack, directory)
    if set_name:
        docs = [
            atom_document(atom)
            for atom in _live_atoms(pack, set_name=set_name)
            if atom.get("id")
        ]
        return upsert_documents(docs, directory)
    return True


def search_milli(
    pack: dict[str, Any],
    query: str,
    *,
    limit: int = 16,
    index_dir: Path | None = None,
    set_name: str | None = None,
) -> list[dict[str, Any]] | None:
    """Atom ranking via milli, prose via the pack. One merge path.

    Never rebuilds the index from set-card prose. Pin scope uses live pack
    atoms for membership and ``--set`` only after those atoms are upserted.
    """
    if milli_bin() is None:
        return None
    directory = index_dir
    if directory is None:
        env = os.environ.get("INSIDE_MILLI_INDEX")
        directory = Path(env) if env else None
    if directory is None:
        return None
    if not _ensure_milli_atoms(pack, directory, set_name=set_name):
        return None

    def _run_search() -> list[dict[str, Any]] | None:
        argv = [
            "search",
            "--index",
            str(directory),
            "--q",
            query,
            "--limit",
            str(limit),
        ]
        workspace = str(pack.get("workspace") or "")
        if workspace:
            argv.extend(["--workspace", workspace])
        if set_name:
            argv.extend(["--set", set_name])
        payload = _run_milli(argv, timeout=10.0)
        if payload is None:
            return None
        raw = payload.get("hits")
        if not isinstance(raw, list):
            return None
        hits = [hit for hit in raw if isinstance(hit, dict)]
        return _filter_atom_hits(hits, pack, set_name=set_name)

    atom_hits = _run_search()
    if atom_hits is None:
        return None
    if not atom_hits and query.strip():
        # Retry once after atom-only reindex (never set-card full replace).
        # A failed upsert means the projection is not authoritative — fall
        # back to the linear scorer rather than returning prose-only misses.
        if not reindex_atoms(pack, directory):
            return None
        atom_hits = _run_search()
        if atom_hits is None:
            return None
    return _merge_hits(
        _prose_hits(pack, query, limit=limit), atom_hits, max(0, int(limit))
    )


def _file_hits(field: str, text: str, qtoks: Iterable[str], bias: float) -> list[dict[str, Any]]:
    hits: list[dict[str, Any]] = []
    for para in paragraphs(text):
        score = _text_score(qtoks, para)
        if not score:
            continue
        hits.append(
            {
                "field": field,
                "id": None,
                "kind": field,
                "text": para,
                "score": score + bias,
            }
        )
    return hits


def search_pack_linear(
    pack: dict[str, Any],
    query: str,
    *,
    limit: int = 16,
    set_name: str | None = None,
) -> list[dict[str, Any]]:
    """Prefix + one-edit scan. Fine for tens of atoms; not for years."""
    qtoks = _tokens(query)
    if not qtoks:
        return []
    hits: list[dict[str, Any]] = []
    hits.extend(_file_hits("user", pack.get("user") or "", qtoks, 0.5))
    hits.extend(_file_hits("memory", pack.get("memory") or "", qtoks, 0.25))
    now = inside_memory.utcnow()
    for atom in pack.get("atoms") or []:
        if not isinstance(atom, dict) or not inside_memory.is_live(atom, now):
            continue
        if not _atom_in_set(atom, set_name):
            continue
        blob = " ".join(
            [
                str(atom.get("text") or ""),
                " ".join(str(x) for x in (atom.get("entities") or [])),
            ]
        )
        relevance = _text_score(qtoks, blob)
        if not relevance:
            continue
        try:
            trust = float(atom.get("trust") if atom.get("trust") is not None else 1.0)
        except (TypeError, ValueError):
            trust = 1.0
        hits.append(
            {
                "field": "atom",
                "id": atom.get("id"),
                "kind": atom.get("kind"),
                "text": atom.get("text") or "",
                "score": relevance + 0.1 * trust + _recency(atom.get("ts")),
            }
        )
    hits.sort(key=lambda h: (-float(h["score"]), str(h.get("field") or ""), str(h.get("id") or "")))
    return hits[: max(0, int(limit))]


def dense_hits(
    pack: dict[str, Any],
    query: str,
    *,
    limit: int = 16,
    set_name: str | None = None,
) -> list[dict[str, Any]]:
    """Cosine over stored atom vectors. Off when the embedder is absent."""
    import inside_embed

    qv = inside_embed.encode_one(query)
    if not qv:
        return []
    now = inside_memory.utcnow()
    hits: list[dict[str, Any]] = []
    for atom in pack.get("atoms") or []:
        if not isinstance(atom, dict) or not inside_memory.is_live(atom, now):
            continue
        if not _atom_in_set(atom, set_name):
            continue
        emb = atom.get("embedding")
        if not isinstance(emb, list) or not emb:
            continue
        score = inside_embed.cosine(qv, [float(x) for x in emb])
        if score < 0.35:
            continue
        hits.append(
            {
                "field": "atom",
                "id": atom.get("id"),
                "kind": atom.get("kind"),
                "text": atom.get("text") or "",
                "score": score,
            }
        )
    hits.sort(key=lambda h: (-float(h["score"]), str(h.get("id") or "")))
    return hits[: max(0, int(limit))]


def _hit_key(hit: dict[str, Any]) -> tuple[str, str]:
    return (str(hit.get("field") or ""), str(hit.get("id") or ""))


def _ballot_keys(hits: list[dict[str, Any]]) -> list[tuple[str, str]]:
    seen: set[tuple[str, str]] = set()
    out: list[tuple[str, str]] = []
    for hit in hits:
        key = _hit_key(hit)
        if key in seen:
            continue
        seen.add(key)
        out.append(key)
    return out


def borda_scores(
    ballots: list[list[tuple[str, str]]], k: int
) -> tuple[list[tuple[str, str]], dict[tuple[str, str], int]]:
    """k-position Borda. Score is k - position. Ties keep first-seen order."""
    if not ballots or k <= 0:
        return [], {}
    scores: dict[tuple[str, str], int] = {}
    first_seen: list[tuple[str, str]] = []
    for ballot in ballots:
        for pos, key in enumerate(ballot[:k]):
            if key not in scores:
                first_seen.append(key)
            scores[key] = scores.get(key, 0) + (k - pos)
    ranked = list(first_seen)
    ranked.sort(key=lambda key: (-scores.get(key, 0), first_seen.index(key)))
    return ranked, scores


def borda_merge(ballots: list[list[tuple[str, str]]], k: int) -> list[tuple[str, str]]:
    ranked, _scores = borda_scores(ballots, k)
    return ranked


def rrf_scores(
    ballots: list[list[tuple[str, str]]], k0: int
) -> tuple[list[tuple[str, str]], dict[tuple[str, str], float]]:
    """Reciprocal Rank Fusion. Score is 1 / (k0 + rank). Ties keep first-seen order."""
    if not ballots:
        return [], {}
    scores: dict[tuple[str, str], float] = {}
    first_seen: list[tuple[str, str]] = []
    for ballot in ballots:
        for pos, key in enumerate(ballot):
            if key not in scores:
                first_seen.append(key)
            scores[key] = scores.get(key, 0.0) + 1.0 / (k0 + pos + 1)
    ranked = list(first_seen)
    ranked.sort(key=lambda key: (-scores.get(key, 0.0), first_seen.index(key)))
    return ranked, scores


def rrf_merge(ballots: list[list[tuple[str, str]]], k0: int) -> list[tuple[str, str]]:
    ranked, _scores = rrf_scores(ballots, k0)
    return ranked


def dowdall_scores(
    ballots: list[list[tuple[str, str]]], k: int
) -> tuple[list[tuple[str, str]], dict[tuple[str, str], float]]:
    """Dowdall (Nauru) Borda. Score is 1/(position+1). Ties keep first-seen order."""
    if not ballots or k <= 0:
        return [], {}
    scores: dict[tuple[str, str], float] = {}
    first_seen: list[tuple[str, str]] = []
    for ballot in ballots:
        for pos, key in enumerate(ballot[:k]):
            if key not in scores:
                first_seen.append(key)
            scores[key] = scores.get(key, 0.0) + 1.0 / (pos + 1)
    ranked = list(first_seen)
    ranked.sort(key=lambda key: (-scores.get(key, 0.0), first_seen.index(key)))
    return ranked, scores


def dowdall_merge(ballots: list[list[tuple[str, str]]], k: int) -> list[tuple[str, str]]:
    ranked, _scores = dowdall_scores(ballots, k)
    return ranked


KEMENY_EXACT_MAX = 8


def kemeny_merge(
    ballots: list[list[tuple[str, str]]], k: int
) -> list[tuple[str, str]]:
    """Kemeny-Young. Exact for n<=8; wider falls back to Borda.

    Ties keep first-seen order.
    """
    if not ballots or k <= 0:
        return []
    first_seen: list[tuple[str, str]] = []
    index: dict[tuple[str, str], int] = {}
    for ballot in ballots:
        for key in ballot[:k]:
            if key not in index:
                index[key] = len(first_seen)
                first_seen.append(key)
    n = len(first_seen)
    if n == 0:
        return []
    if n > KEMENY_EXACT_MAX:
        return borda_merge(ballots, k)
    pairwise = [[0] * n for _ in range(n)]
    for ballot in ballots:
        pos: list[int | None] = [None] * n
        for p, key in enumerate(ballot[:k]):
            i = index.get(key)
            if i is not None and pos[i] is None:
                pos[i] = p
        for i in range(n):
            for j in range(n):
                if i == j:
                    continue
                pi, pj = pos[i], pos[j]
                if pi is not None and pj is not None and pi < pj:
                    pairwise[i][j] += 1
                elif pi is not None and pj is None:
                    pairwise[i][j] += 1
    best: tuple[int, ...] | None = None
    best_dist: int | None = None
    for perm in permutations(range(n)):
        dist = 0
        for a in range(n):
            for b in range(a + 1, n):
                dist += pairwise[perm[b]][perm[a]]
        if best_dist is None or dist < best_dist:
            best_dist = dist
            best = perm
    assert best is not None
    return [first_seen[i] for i in best]


def _jaccard(a: set[str], b: set[str]) -> float:
    if not a and not b:
        return 0.0
    uni = len(a | b)
    if uni == 0:
        return 0.0
    return len(a & b) / uni


def mmr_rerank(
    items: list[tuple[tuple[str, str], float, set[str]]],
    lambda_rel: float = 0.7,
) -> list[tuple[str, str]]:
    """MMR after fuse. lambda in [0, 1); otherwise keep fuse order."""
    if len(items) < 2 or not (0.0 <= lambda_rel < 1.0):
        return [key for key, _rel, _toks in items]
    rels = [rel for _key, rel, _toks in items]
    lo = min(rels)
    hi = max(rels)
    span = max(hi - lo, 1e-9)

    def rel_p(rel: float) -> float:
        return (rel - lo) / span

    selected: list[int] = []
    rest = list(range(len(items)))
    while rest:
        best = rest[0]
        best_s = float("-inf")
        for i in rest:
            novelty = 0.0
            if selected:
                novelty = max(
                    _jaccard(items[i][2], items[j][2]) for j in selected
                )
            score = lambda_rel * rel_p(items[i][1]) - (1.0 - lambda_rel) * novelty
            if score > best_s:
                best_s = score
                best = i
        selected.append(best)
        rest.remove(best)
    return [items[i][0] for i in selected]


DEFAULT_FUSE = "borda"
DEFAULT_DIVERSIFY = "mmr"
DEFAULT_DECAY = "off"
ENV_FUSE = "PACKSET_FUSE"
ENV_DIVERSIFY = "PACKSET_DIVERSIFY"
ENV_DECAY = "PACKSET_DECAY"
_IMPLEMENTED_FUSE = frozenset({"borda", "rrf", "dowdall", "kemeny"})
_RESERVED_FUSE = frozenset({"combmnz", "schulze", "copeland", "tideman"})
_IMPLEMENTED_DIVERSIFY = frozenset({"mmr", "none"})
_RESERVED_DIVERSIFY = frozenset({"dpp"})


class UnknownVoter(ValueError):
    """A fuse, diversify, or decay name that is not an implemented voter."""


def parse_fuse(name: str) -> str:
    if name in _IMPLEMENTED_FUSE:
        return name
    if name in _RESERVED_FUSE:
        raise UnknownVoter(f"not implemented fuse {name}")
    raise UnknownVoter(f"unknown fuse {name}")


def parse_diversify(name: str) -> str:
    if name in _IMPLEMENTED_DIVERSIFY:
        return name
    if name in _RESERVED_DIVERSIFY:
        raise UnknownVoter(f"not implemented diversify {name}")
    raise UnknownVoter(f"unknown diversify {name}")


def parse_decay(name: str) -> str:
    if name in {"on", "off"}:
        return name
    raise UnknownVoter(f"unknown decay {name}")


def _env_or(key: str, default: str) -> str:
    if key not in os.environ:
        return default
    return os.environ[key]


def resolve_panel(
    fuse: str | None = None,
    diversify: str | None = None,
    decay: str | None = None,
) -> tuple[str, str, str]:
    """Host sequence from names or PACKSET_* env. Clients do not choose this."""
    if fuse is None:
        fuse = _env_or(ENV_FUSE, DEFAULT_FUSE)
    if diversify is None:
        diversify = _env_or(ENV_DIVERSIFY, DEFAULT_DIVERSIFY)
    if decay is None:
        decay = _env_or(ENV_DECAY, DEFAULT_DECAY)
    return parse_fuse(fuse), parse_diversify(diversify), parse_decay(decay)


def bind_host_panel(
    fuse: str | None = None,
    diversify: str | None = None,
    decay: str | None = None,
) -> tuple[str, str, str]:
    """Resolve and publish the host panel into PACKSET_* env."""
    fuse, diversify, decay = resolve_panel(fuse, diversify, decay)
    os.environ[ENV_FUSE] = fuse
    os.environ[ENV_DIVERSIFY] = diversify
    os.environ[ENV_DECAY] = decay
    return fuse, diversify, decay


def temporal_decay(
    source: str, age_days: float, half_life_days: float | None
) -> float:
    """Half-life weight. Evergreen sources stay 1.0."""
    if source in {"global", "workspace", "user", "evergreen"}:
        return 1.0
    if half_life_days is None or half_life_days <= 0:
        return 1.0
    return math.exp(-math.log(2.0) / half_life_days * max(age_days, 0.0))


def _hit_decay_weight(hit: dict[str, Any]) -> float:
    source = str(hit.get("kind") or hit.get("field") or "")
    ts = hit.get("ts")
    if not ts:
        return 1.0
    try:
        when = datetime.fromisoformat(str(ts).replace("Z", "+00:00"))
    except ValueError:
        return 1.0
    if when.tzinfo is None:
        when = when.replace(tzinfo=UTC)
    days = max(0.0, (datetime.now(UTC) - when).total_seconds() / 86400.0)
    return temporal_decay(source, days, _RECENCY_HALF_LIFE_DAYS)


def _merge_ballots(
    ballots: list[list[dict[str, Any]]],
    limit: int,
    *,
    fuse: str | None = None,
    diversify: str | None = None,
    decay: str | None = None,
) -> list[dict[str, Any]]:
    """Named fuse then diversify then decay. Default is Borda then MMR, decay off."""
    if limit <= 0:
        return []
    fuse, diversify, decay = resolve_panel(fuse, diversify, decay)
    by_key: dict[tuple[str, str], dict[str, Any]] = {}
    for hits in reversed(ballots):
        for hit in hits:
            by_key[_hit_key(hit)] = hit
    keys = [_ballot_keys(hits) for hits in ballots]
    if fuse == "borda":
        ranked, scores = borda_scores(keys, limit)
    elif fuse == "rrf":
        ranked, scores = rrf_scores(keys, 60)
    elif fuse == "dowdall":
        ranked, scores = dowdall_scores(keys, limit)
    elif fuse == "kemeny":
        ranked = kemeny_merge(keys, limit)
        scores = {key: float(len(ranked) - i) for i, key in enumerate(ranked)}
    else:
        raise UnknownVoter(f"unknown fuse {fuse}")
    weights = {key: float(scores.get(key, 0)) for key in ranked}
    if decay == "on":
        for key in ranked:
            weights[key] *= _hit_decay_weight(by_key[key])
        order_index = {key: i for i, key in enumerate(ranked)}
        ranked = sorted(ranked, key=lambda key: (-weights[key], order_index[key]))
    items: list[tuple[tuple[str, str], float, set[str]]] = []
    for key in ranked:
        hit = by_key[key]
        toks = set(_TOKEN.findall(str(hit.get("text") or "").lower()))
        items.append((key, weights[key], toks))
    if diversify == "mmr":
        order = mmr_rerank(items)
    elif diversify == "none":
        order = ranked
    else:
        raise UnknownVoter(f"unknown diversify {diversify}")
    return [by_key[key] for key in order][:limit]


def _merge_hits(
    primary: list[dict[str, Any]],
    secondary: list[dict[str, Any]],
    limit: int,
    *,
    fuse: str | None = None,
    diversify: str | None = None,
    decay: str | None = None,
) -> list[dict[str, Any]]:
    """Named fuse then diversify. Default is Borda then MMR. Not de-dupe."""
    return _merge_ballots(
        [primary, secondary],
        limit,
        fuse=fuse,
        diversify=diversify,
        decay=decay,
    )


def _merge_dense(
    keyword: list[dict[str, Any]], dense: list[dict[str, Any]], limit: int
) -> list[dict[str, Any]]:
    return _merge_hits(keyword, dense, limit)


def lexical_search(
    pack: dict[str, Any],
    query: str,
    *,
    limit: int = 16,
    index_dir: Path | None = None,
    set_name: str | None = None,
) -> tuple[list[dict[str, Any]], str]:
    """BM25 (milli) or the linear scorer. No dense-only extras."""
    if not _tokens(query):
        return [], "linear"
    ranked = search_milli(
        pack, query, limit=limit, index_dir=index_dir, set_name=set_name
    )
    if ranked is not None:
        return ranked, "milli"
    return search_pack_linear(pack, query, limit=limit, set_name=set_name), "linear"


def search_pack_with_engine(
    pack: dict[str, Any],
    query: str,
    *,
    limit: int = 16,
    index_dir: Path | None = None,
    set_name: str | None = None,
) -> tuple[list[dict[str, Any]], str]:
    """Ranked hits plus which engine produced them."""
    if not _tokens(query):
        return [], "linear"
    dense = dense_hits(pack, query, limit=limit, set_name=set_name)
    ranked, engine = lexical_search(
        pack, query, limit=limit, index_dir=index_dir, set_name=set_name
    )
    if dense:
        merged = _merge_dense(ranked, dense, max(0, int(limit)))
        return merged, f"{engine}+dense"
    return ranked, engine


def search_pack(
    pack: dict[str, Any],
    query: str,
    *,
    limit: int = 16,
    index_dir: Path | None = None,
    set_name: str | None = None,
) -> list[dict[str, Any]]:
    """Ranked hits over USER.md, MEMORY.md, and live atoms."""
    hits, _engine = search_pack_with_engine(
        pack, query, limit=limit, index_dir=index_dir, set_name=set_name
    )
    return hits
