"""Include-first recall. No daemon required."""

from __future__ import annotations

from pathlib import Path
from typing import Any

import inside_memory
import inside_recall

WS = "git:example.com/proj"


def atom(ident: str, **fields: Any) -> dict[str, Any]:
    rec: dict[str, Any] = {
        "id": ident,
        "workspace": WS,
        "kind": "habit",
        "text": f"Claim {ident}.",
        "trust": 1.0,
        "ts": "2026-08-11T12:00:00.000Z",
        "tombstone": False,
        "links": [],
        "valid_to": None,
    }
    rec.update(fields)
    return rec


def crowd(n: int = 70) -> list[dict[str, Any]]:
    return [atom(f"x{i:03d}", text=f"Unrelated claim {i}.", trust=0.2) for i in range(n)]


def test_small_pack_returns_every_live_atom() -> None:
    atoms = [
        atom("a", kind="voice", text="Speaks in short sentences."),
        atom("b", kind="habit", text="Reviews open with a check."),
        atom("c", kind="preference", text="Prefers Conventional Commits."),
    ]
    out = inside_recall.recall(WS, atoms=atoms)
    assert {row["id"] for row in out} == {"a", "b", "c"}


def test_small_pack_needs_no_seeds() -> None:
    out = inside_recall.recall(WS, atoms=[atom("a"), atom("b")], seeds=None, hints=None)
    assert len(out) == 2


def test_large_pack_walks_one_hop() -> None:
    atoms = crowd(70)
    seed = atom(
        "seed",
        text="Action seed about JOSS.",
        links=["nbr", "x000"],
        trust=0.5,
    )
    neighbor = atom("nbr", text="JOSS neighbour claim.", trust=0.4, links=["seed"])
    far = atom("far", text="Far unlinked claim.", trust=0.99)
    atoms.extend([seed, neighbor, far])
    out = inside_recall.recall(WS, seeds=["seed"], atoms=atoms)
    ids = [row["id"] for row in out]
    assert "seed" in ids
    assert "nbr" in ids
    assert "x000" in ids
    assert "far" not in ids
    assert len(out) <= 64
    assert "x001" not in ids


def test_hints_select_seeds_by_text_and_entities() -> None:
    atoms = crowd(70)
    seed = atom(
        "seed",
        text="Reviews open on JOSS.",
        entities=["JOSS"],
        links=["nbr"],
    )
    neighbor = atom("nbr", text="Linked review habit.", links=["seed"])
    other = atom("other", text="Cache of github issues.", kind="cache-pointer")
    atoms.extend([seed, neighbor, other])
    by_text = inside_recall.recall(WS, hints={"user_text": "review this PR"}, atoms=atoms)
    assert {row["id"] for row in by_text} == {"seed", "nbr"}
    by_ent = inside_recall.recall(WS, hints={"entities": ["JOSS"]}, atoms=atoms)
    assert {row["id"] for row in by_ent} == {"seed", "nbr"}


def test_tombstoned_and_expired_are_not_walked() -> None:
    atoms = crowd(70)
    seed = atom("seed", text="Live seed.", links=["dead", "stale", "nbr"])
    dead = atom("dead", text="Tombstoned neighbour.", tombstone=True, links=["seed"])
    stale = atom(
        "stale",
        text="Expired neighbour.",
        kind="cache-pointer",
        valid_to="2000-01-01T00:00:00.000Z",
        links=["seed"],
    )
    neighbor = atom("nbr", text="Live neighbour.", links=["seed"])
    atoms.extend([seed, dead, stale, neighbor])
    out = inside_recall.recall(WS, seeds=["seed"], atoms=atoms)
    assert {row["id"] for row in out} == {"seed", "nbr"}


def test_tombstoned_seed_is_skipped() -> None:
    atoms = crowd(70)
    dead = atom("dead", text="Dead seed.", tombstone=True, links=["nbr"])
    neighbor = atom("nbr", text="Would be a neighbour.", links=["dead"])
    atoms.extend([dead, neighbor])
    assert inside_recall.recall(WS, seeds=["dead"], atoms=atoms) == []


def test_default_limit_is_64() -> None:
    atoms = crowd(80)
    seed_links = [f"x{i:03d}" for i in range(80)]
    atoms.append(atom("seed", text="Seed with many links.", links=seed_links))
    out = inside_recall.recall(WS, seeds=["seed"], atoms=atoms)
    assert len(out) == 64
    huge = inside_recall.recall(WS, seeds=["seed"], atoms=atoms, limit=1000)
    assert len(huge) == 64


def test_prefers_neighbours_then_trust_ts_id() -> None:
    atoms = crowd(70)
    seed = atom(
        "seed",
        text="Seed.",
        trust=0.1,
        ts="2026-08-01T00:00:00.000Z",
        links=["n1", "n2"],
    )
    n1 = atom("n1", text="Neighbour one.", trust=0.5, ts="2026-08-10T00:00:00.000Z")
    n2 = atom("n2", text="Neighbour two.", trust=0.9, ts="2026-08-09T00:00:00.000Z")
    atoms.extend([seed, n1, n2])
    ids = [row["id"] for row in inside_recall.recall(WS, seeds=["seed"], atoms=atoms)]
    assert ids[:2] == ["n2", "n1"]
    assert ids[2] == "seed"


def test_text_budget_caps_returned_text() -> None:
    out = inside_recall.recall(
        WS,
        atoms=[
            atom("a", text="A" * 20000, trust=1.0),
            atom("b", text="B" * 20000, trust=0.9),
            atom("c", text="C" * 20000, trust=0.8),
        ],
    )
    total = sum(len(row.get("text") or "") for row in out)
    assert total <= inside_recall.TEXT_BUDGET
    assert len(out) >= 1
    assert len(out) < 3


def test_sort_is_trust_then_ts_then_id() -> None:
    atoms = [
        atom("c", trust=0.5, ts="2026-08-11T00:00:00.000Z"),
        atom("a", trust=0.9, ts="2026-08-01T00:00:00.000Z"),
        atom("b", trust=0.9, ts="2026-08-10T00:00:00.000Z"),
        atom("d", trust=0.9, ts="2026-08-10T00:00:00.000Z"),
    ]
    assert [row["id"] for row in inside_recall.recall(WS, atoms=atoms)] == [
        "b",
        "d",
        "a",
        "c",
    ]


def test_home_load_skips_expired_and_tombstones(tmp_path: Path) -> None:
    live = inside_memory.make_atom(
        workspace=WS,
        text="Live voice claim.",
        kind="voice",
        about_peer="rgoswami",
        by_peer="hermes",
    )
    stale = inside_memory.make_atom(
        workspace=WS,
        text="Expired snapshot.",
        kind="cache-pointer",
        about_peer="rgoswami",
        by_peer="hermes",
        valid_to="2000-01-01T00:00:00.000Z",
    )
    stored = inside_memory.add_atom(live, home=tmp_path)
    inside_memory.add_atom(stale, home=tmp_path)
    dead = inside_memory.make_atom(
        workspace=WS,
        text="Habit that will be dropped.",
        kind="habit",
        about_peer="rgoswami",
        by_peer="hermes",
    )
    dropped = inside_memory.add_atom(dead, home=tmp_path)
    inside_memory.delete_atom(WS, dropped["id"], home=tmp_path)
    ids = [row["id"] for row in inside_recall.recall(WS, home=tmp_path)]
    assert ids == [stored["id"]]


def test_large_pack_without_seeds_returns_empty() -> None:
    assert inside_recall.recall(WS, atoms=crowd(70)) == []
