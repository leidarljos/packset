#!/usr/bin/env python3
"""Local ONNX embeddings for live atoms.

Keyword search is milli. Dense rank is optional and local:
BAAI/bge-small-en-v1.5 via fastembed (ONNX, 384-d, ~67MB, MIT).
No torch. No hosted embedder. No company wrapper. The store
is still atoms in LMDB; the vector is a field on the atom.

Default is on when the model is already cached, or when
INSIDE_EMBED_DOWNLOAD is set. INSIDE_EMBED=off disables.
"""
from __future__ import annotations

import os
from collections.abc import Iterable
from pathlib import Path

import inside_memory

DEFAULT_MODEL = "BAAI/bge-small-en-v1.5"
DIM = 384

_MODEL = None
_MODEL_FAILED = False


def enabled() -> bool:
    raw = (os.environ.get("INSIDE_EMBED") or "on").strip().lower()
    return raw not in {"0", "off", "none", "false", "no"}


def cache_dir(home: Path | None = None) -> Path:
    env = os.environ.get("INSIDE_EMBED_CACHE")
    if env:
        return Path(env)
    return inside_memory.memory_root(home) / "embed-cache"


def _download_ok() -> bool:
    raw = (os.environ.get("INSIDE_EMBED_DOWNLOAD") or "").strip().lower()
    return raw in {"1", "on", "true", "yes"}


def _cache_has_model(directory: Path) -> bool:
    if not directory.is_dir():
        return False
    return any(directory.rglob("*.onnx"))


def available(home: Path | None = None) -> bool:
    if not enabled() or _MODEL_FAILED:
        return False
    try:
        import fastembed  # noqa: F401
    except ImportError:
        return False
    return _download_ok() or _cache_has_model(cache_dir(home))


def _load(home: Path | None = None):
    global _MODEL, _MODEL_FAILED
    if _MODEL is not None or _MODEL_FAILED:
        return _MODEL
    if not available(home):
        return None
    try:
        from fastembed import TextEmbedding
    except ImportError:
        _MODEL_FAILED = True
        return None
    directory = cache_dir(home)
    directory.mkdir(parents=True, exist_ok=True)
    try:
        _MODEL = TextEmbedding(
            model_name=os.environ.get("INSIDE_EMBED_MODEL") or DEFAULT_MODEL,
            cache_dir=str(directory),
            threads=1,
        )
    except Exception:
        _MODEL_FAILED = True
        return None
    return _MODEL


def encode(texts: Iterable[str], home: Path | None = None) -> list[list[float]] | None:
    """Return one 384-d vector per text, or None if the embedder is off."""
    blobs = [str(t or "") for t in texts]
    if not blobs:
        return []
    model = _load(home)
    if model is None:
        return None
    out = []
    for vec in model.embed(blobs):
        out.append([float(x) for x in vec])
    return out


def encode_one(text: str, home: Path | None = None) -> list[float] | None:
    batch = encode([text], home=home)
    if not batch:
        return None
    return batch[0]


def cosine(a: list[float], b: list[float]) -> float:
    if not a or not b or len(a) != len(b):
        return 0.0
    dot = sum(x * y for x, y in zip(a, b, strict=True))
    na = sum(x * x for x in a) ** 0.5
    nb = sum(y * y for y in b) ** 0.5
    if na == 0.0 or nb == 0.0:
        return 0.0
    return dot / (na * nb)
