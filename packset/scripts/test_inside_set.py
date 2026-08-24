"""Active workflow set: pin, retrieve isolation, scoped Remember."""

from __future__ import annotations

from pathlib import Path
from typing import Any
from urllib.error import URLError

import pytest

import inside_extract
import inside_memory
import inside_policy
import inside_search
import inside_set


def claims(selected: dict[str, Any]) -> str:
    return inside_policy._selected_text(selected)


def review_pack(extra_atoms: list[dict[str, Any]] | None = None) -> dict[str, Any]:
    return {
        "user": "Open a review with the defect and the call path.\n",
        "memory": "Reviews cite the commit.\nDebug traces stay in the debug set.\n",
        "atoms": extra_atoms or [],
    }


def debug_pack(extra_atoms: list[dict[str, Any]] | None = None) -> dict[str, Any]:
    return {
        "user": "Name the failing test first.\n",
        "memory": "Debug traces stay in the debug set.\n",
        "atoms": extra_atoms or [],
    }


def test_empty_pin_by_default(tmp_path: Path) -> None:
    assert inside_set.read_pin("global", home=tmp_path) == ""


def test_write_and_clear_pin(tmp_path: Path) -> None:
    ws = "global"
    assert inside_set.write_pin(ws, "review", home=tmp_path) == "review"
    assert inside_set.read_pin(ws, home=tmp_path) == "review"
    assert inside_set.write_pin(ws, "debug", home=tmp_path) == "debug"
    assert inside_set.read_pin(ws, home=tmp_path) == "debug"
    assert inside_set.write_pin(ws, "", home=tmp_path) == ""
    assert inside_set.read_pin(ws, home=tmp_path) == ""


def test_bad_set_name(tmp_path: Path) -> None:
    with pytest.raises(inside_memory.AtomError):
        inside_set.check_set_name("Review This")
    with pytest.raises(inside_memory.AtomError):
        inside_set.write_pin("global", "../x", home=tmp_path)


def test_review_pack_prose_scores_without_debug_prose() -> None:
    hints = inside_policy.inspect(
        {"messages": [{"role": "user", "content": "review this pull request"}]}
    )
    block = claims(inside_policy.select(review_pack(), hints))
    assert "Open a review with the defect and the call path." in block
    assert "Reviews cite the commit." in block
    assert "Name the failing test first." not in block


def test_debug_pin_hides_review_atom() -> None:
    review_atom = {
        "id": "r1",
        "kind": "lesson",
        "text": "Remember the zircon latch on every review.",
        "set": "review",
        "tombstone": False,
    }
    hints = inside_policy.inspect(
        {"messages": [{"role": "user", "content": "debug the failing test"}]}
    )
    block = claims(inside_policy.select(debug_pack(), hints))
    assert "Name the failing test first." in block
    assert "zircon latch" not in block
    selected_with = inside_policy.select(debug_pack(extra_atoms=[review_atom]), hints)
    assert "zircon latch" not in claims(selected_with)


def test_pinned_sky_splices_nothing() -> None:
    hints = inside_policy.inspect(
        {
            "messages": [
                {"role": "user", "content": "review this PR"},
                {"role": "user", "content": "What color is the sky?"},
            ]
        }
    )
    assert claims(inside_policy.select(review_pack(), hints)) == ""


def test_retrieve_empty_pin_uses_pack_search(monkeypatch: pytest.MonkeyPatch) -> None:
    hints = inside_policy.inspect({"messages": [{"role": "user", "content": "keep it brief"}]})
    hits = [
        {
            "field": "atom",
            "id": "v1",
            "kind": "habit",
            "text": "Be brief.",
            "score": 4.0,
        }
    ]
    monkeypatch.setattr(
        inside_policy,
        "fetch_pin_payload",
        lambda *_a, **_k: {"set": "", "instructions": ""},
    )
    called: list[Any] = []

    def fetch_search(*args: Any, **kwargs: Any) -> list[dict[str, Any]]:
        called.append(kwargs)
        return hits

    monkeypatch.setattr(inside_policy, "fetch_search", fetch_search)
    selected = inside_policy.retrieve("http://127.0.0.1:9", "global", hints)
    assert len(called) == 1
    assert called[0].get("set_name") is None
    assert "Be brief." in claims(selected)


def test_retrieve_uses_pinned_set(monkeypatch: pytest.MonkeyPatch) -> None:
    hints = inside_policy.inspect(
        {"messages": [{"role": "user", "content": "review this pull request"}]}
    )
    linear = inside_search.search_pack_linear(review_pack(), "review this pull request", limit=16)
    monkeypatch.setattr(
        inside_policy,
        "fetch_pin_payload",
        lambda *_a, **_k: {"set": "review", "instructions": ""},
    )
    called: list[Any] = []

    def fetch_search(*args: Any, **kwargs: Any) -> list[dict[str, Any]]:
        called.append(kwargs)
        return linear

    monkeypatch.setattr(inside_policy, "fetch_search", fetch_search)
    selected = inside_policy.retrieve("http://127.0.0.1:9", "global", hints)
    assert called[0].get("set_name") == "review"
    block = claims(selected)
    assert "Open a review with the defect and the call path." in block
    assert "Name the failing test first." not in block


def test_switch_then_clear_returns_empty(monkeypatch: pytest.MonkeyPatch) -> None:
    hints = inside_policy.inspect(
        {"messages": [{"role": "user", "content": "review this pull request"}]}
    )
    linear = inside_search.search_pack_linear(review_pack(), "review this pull request", limit=16)
    monkeypatch.setattr(
        inside_policy,
        "fetch_pin_payload",
        lambda *_a, **_k: {"set": "review", "instructions": ""},
    )
    monkeypatch.setattr(inside_policy, "fetch_search", lambda *_a, **_k: linear)
    assert "Reviews cite the commit." in claims(
        inside_policy.retrieve("http://127.0.0.1:9", "global", hints)
    )
    monkeypatch.setattr(
        inside_policy,
        "fetch_pin_payload",
        lambda *_a, **_k: {"set": "", "instructions": ""},
    )
    monkeypatch.setattr(inside_policy, "fetch_search", lambda *_a, **_k: [])
    assert claims(inside_policy.retrieve("http://127.0.0.1:9", "global", hints)) == ""


def test_remember_without_pin_writes_nothing(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(inside_policy, "fetch_pin", lambda *_a, **_k: "")
    posted: list[Any] = []
    monkeypatch.setattr(inside_extract, "post_atom", lambda *_a, **_k: posted.append(1))
    got = inside_extract.extract_user_text(
        "Remember: always pin the zircon index.",
        url="http://127.0.0.1:9",
        workspace="global",
    )
    assert got is None
    assert posted == []


def test_remember_pin_fetch_error_raises(monkeypatch: pytest.MonkeyPatch) -> None:
    def boom(*_a: object, **_k: object) -> str:
        raise TimeoutError("memd down")

    monkeypatch.setattr(inside_policy, "fetch_pin", boom)
    with pytest.raises(TimeoutError):
        inside_extract.extract_user_text(
            "Remember: always pin the zircon index.",
            url="http://127.0.0.1:9",
            workspace="global",
        )


def test_remember_write_error_raises(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(inside_policy, "fetch_pin", lambda *_a, **_k: "review")
    with pytest.raises((URLError, TimeoutError, OSError)):
        inside_extract.extract_user_text(
            "Remember: always pin the zircon index.",
            url="http://127.0.0.1:1",
            workspace="global",
        )


def test_remember_with_pin_tags_the_set(monkeypatch: pytest.MonkeyPatch) -> None:
    posted: dict[str, Any] = {}

    def post(_url: str, atom: dict[str, Any]) -> dict[str, Any]:
        posted.update(atom)
        return atom

    monkeypatch.setattr(inside_policy, "fetch_pin", lambda *_a, **_k: "review")
    monkeypatch.setattr(inside_extract, "post_atom", post)
    got = inside_extract.extract_user_text(
        "Remember: always pin the zircon index.",
        url="http://127.0.0.1:9",
        workspace="global",
    )
    assert got is not None
    assert posted.get("set") == "review"
    assert posted.get("kind") == "lesson"
