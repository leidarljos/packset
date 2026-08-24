"""Hemingway-style gate. No editor binary."""

from __future__ import annotations

from pathlib import Path

import pytest

import inside_memory
import inside_prose


def test_simple_claim_is_clean() -> None:
    report = inside_prose.refuse("Reviews open with a reproducibility check.", role="atom")
    assert report["sentences"] <= 2
    assert report["very_hard_sentences"] == 0


def test_adverb_slop_is_refused() -> None:
    text = (
        "We really actually basically just quite literally definitely "
        "probably certainly want this done."
    )
    with pytest.raises(inside_prose.ProseError):
        inside_prose.refuse(text, role="file")


def test_three_sentence_atom_is_refused() -> None:
    with pytest.raises(inside_prose.ProseError):
        inside_prose.refuse("One claim. Two claim. Three claim.", role="atom")


def test_validate_atom_stores_prose() -> None:
    atom = inside_memory.make_atom(
        workspace="git:ex/p",
        text="Reviews open with a reproducibility check.",
        kind="voice",
        about_peer="rgoswami",
        by_peer="hermes",
    )
    assert "prose" in atom
    assert atom["prose"]["words"] >= 4


def test_user_slop_raises(tmp_path: Path) -> None:
    with pytest.raises((inside_prose.ProseError, inside_memory.AtomError)):
        inside_memory.set_user(
            "We really actually basically just quite literally "
            "definitely probably certainly want this done now.",
            home=tmp_path,
        )
