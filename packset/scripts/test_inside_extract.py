"""Explicit remember/prefer lines become seat atoms."""

from __future__ import annotations

import inside_extract


def test_remember_line() -> None:
    got = inside_extract.claim_from_user("Remember: always pin the zircon index.")
    assert got == ("lesson", "always pin the zircon index")


def test_prefer_line() -> None:
    kind, text = inside_extract.claim_from_user("Prefer conventional commits")
    assert kind == "preference"
    assert "conventional" in text


def test_quote_prompt_returns_none() -> None:
    assert (
        inside_extract.claim_from_user(
            "In one short sentence, quote the preference and the papers."
        )
        is None
    )


def test_question_with_prefer_returns_none() -> None:
    assert (
        inside_extract.claim_from_user("What latch do I prefer on reviews? One short sentence.")
        is None
    )


def test_atom_kind_and_workspace() -> None:
    atom = inside_extract.atom_from_user(
        "Note that reviews cite the commit SHA.",
        workspace="global",
    )
    assert atom is not None
    assert atom["kind"] == "lesson"
    assert atom["workspace"] == "global"
    assert "commit SHA" in atom["text"]


def test_listing_is_dump() -> None:
    listing = "\n".join(f"- file{i}.rs" for i in range(8))
    assert inside_extract.is_tool_dump(listing)
    assert not inside_extract.is_tool_dump("Remember: keep the habit.")
