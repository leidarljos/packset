#!/usr/bin/env python3
"""Turn an explicit remember/prefer line into one seat atom.

Every harness POSTs through the shim. This is the write that
session-end extract was supposed to be: one claim, when the
user says to keep it. Dedup is memd's job.
"""

from __future__ import annotations

import json
import re
import urllib.request
from typing import Any

import inside_memory

_PATTERNS = (
    (re.compile(r"(?i)^\s*remember(?:\s+that)?[:\s]+(.+)$"), "lesson"),
    (re.compile(r"(?i)^\s*note that[:\s]+(.+)$"), "lesson"),
    (re.compile(r"(?i)^\s*from now on[,:]?\s+(.+)$"), "habit"),
    (re.compile(r"(?i)^\s*prefer[:\s]+(.+)$"), "preference"),
)
_FIRST_SENTENCE = re.compile(r"\s*[.!?]+\s*")
_QUESTION = re.compile(
    r"(?i)^\s*(what|which|who|whom|whose|where|when|why|how|"
    r"do|does|did|is|are|can|could|would|should)\b"
)


def claim_from_user(text: str) -> tuple[str, str] | None:
    """Return (kind, claim) when a line is an explicit keep directive."""
    blob = (text or "").strip()
    if not blob:
        return None
    # Stock Grok Build wraps the turn; keep directives are inside it.
    try:
        from inside_policy import _unwrap_user_query

        blob = _unwrap_user_query(blob)
    except Exception:
        pass
    last = blob.splitlines()[-1].strip()
    if last.endswith("?") or _QUESTION.match(last):
        return None
    for pat, kind in _PATTERNS:
        match = pat.match(last)
        if not match:
            continue
        claim = match.group(1).strip()
        claim = _FIRST_SENTENCE.split(claim, 1)[0].strip().rstrip(".,;:")
        if len(claim) < 8:
            return None
        return kind, claim
    return None


def atom_from_user(
    text: str,
    *,
    workspace: str,
    about_peer: str = "user",
    by_peer: str = "user",
    set_name: str | None = None,
) -> dict[str, Any] | None:
    parsed = claim_from_user(text)
    if parsed is None:
        return None
    kind, claim = parsed
    return inside_memory.make_atom(
        workspace=workspace,
        text=claim,
        kind=kind,
        about_peer=about_peer,
        by_peer=by_peer,
        level="explicit",
        set_name=set_name,
    )


def post_atom(url: str, atom: dict[str, Any]) -> dict[str, Any]:
    target = (url or "").rstrip("/") + "/v1/atoms"
    payload = json.dumps(atom).encode("utf-8")
    req = urllib.request.Request(
        target,
        data=payload,
        method="POST",
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=5) as resp:
        body = json.loads(resp.read().decode("utf-8"))
    if not isinstance(body, dict):
        raise ValueError("atom response is not an object")
    return body


def extract_user_text(
    text: str,
    *,
    url: str,
    workspace: str,
    about_peer: str = "user",
    by_peer: str = "user",
) -> dict[str, Any] | None:
    import inside_policy

    pin = inside_policy.fetch_pin(url, workspace)
    if not pin:
        return None
    atom = atom_from_user(
        text,
        workspace=workspace,
        about_peer=about_peer,
        by_peer=by_peer,
        set_name=pin,
    )
    if atom is None:
        return None
    return post_atom(url, atom)


def is_tool_dump(text: str) -> bool:
    """Tool stdout, listings, and fetched bodies are attach, not atoms."""
    t = (text or "").strip()
    if not t:
        return True
    lower = t.lower()
    if "```" in lower and ("stdout" in lower or "stderr" in lower):
        return True
    if lower.startswith("<!doctype") or lower.startswith("<html"):
        return True
    lines = t.splitlines()
    if len(lines) >= 8 and sum(
        1
        for line in lines
        if line.startswith("-") or line.startswith("drwx") or line.startswith("total ")
    ) >= 6:
        return True
    return False

