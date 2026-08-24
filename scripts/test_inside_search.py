"""Ranked pack search. No Meilisearch process."""

from __future__ import annotations

import json
from collections.abc import Callable
from pathlib import Path
from typing import Any

import pytest

import inside_memory
import inside_search

WS = "git:ex/p"


@pytest.fixture(autouse=True)
def _isolated_panel(monkeypatch: pytest.MonkeyPatch) -> None:
    for key in (
        inside_search.ENV_FUSE,
        inside_search.ENV_DIVERSIFY,
        inside_search.ENV_DECAY,
    ):
        monkeypatch.delenv(key, raising=False)


@pytest.fixture
def scored_pair() -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
    primary = [
        {"field": "atom", "id": "a", "text": "alpha one", "score": 1.0},
        {"field": "atom", "id": "b", "text": "beta two", "score": 0.5},
        {"field": "atom", "id": "c", "text": "gamma three", "score": 0.1},
    ]
    secondary = [
        {"field": "atom", "id": "b", "text": "beta two", "score": 0.9},
        {"field": "atom", "id": "c", "text": "gamma three", "score": 0.4},
        {"field": "atom", "id": "a", "text": "alpha one", "score": 0.2},
    ]
    return primary, secondary


@pytest.fixture
def review_debug() -> tuple[dict[str, Any], dict[str, Any]]:
    review = inside_memory.make_atom(
        workspace=WS,
        text="Remember the zircon latch on every review.",
        kind="lesson",
        about_peer="user",
        by_peer="user",
        entities=["zircon"],
        set_name="review",
    )
    debug = inside_memory.make_atom(
        workspace=WS,
        text="Zircon latch belongs in the debug notebook.",
        kind="habit",
        about_peer="user",
        by_peer="user",
        entities=["zircon"],
        set_name="debug",
    )
    return review, debug


@pytest.fixture
def milli_index(tmp_path: Path) -> Path:
    if inside_search.milli_bin() is None:
        pytest.skip("inside-milli binary not available")
    return tmp_path / "memory.milli"


def voice(**fields: Any) -> dict[str, Any]:
    atom = inside_memory.make_atom(
        workspace=WS,
        text="JOSS reviews open with a reproducibility check.",
        kind="voice",
        about_peer="rgoswami",
        by_peer="hermes",
        entities=["JOSS"],
    )
    atom.update(fields)
    return atom


def test_empty_query() -> None:
    assert inside_search.search_pack({"user": "hi", "atoms": []}, "") == []


def test_user_and_memory_hit() -> None:
    pack = {
        "user": "No thanks. Be brief.\n",
        "memory": "Read paper.pdf first.\n",
        "atoms": [],
    }
    hits = inside_search.search_pack(pack, "brief")
    assert [(h["field"], h.get("text")) for h in hits] == [("user", "No thanks. Be brief.")]
    hits = inside_search.search_pack(pack, "paper")
    assert [(h["field"], h.get("text")) for h in hits] == [("memory", "Read paper.pdf first.")]


def test_live_atom_prefix_and_typo() -> None:
    atom = voice()
    pack = {"user": "", "memory": "", "atoms": [atom]}
    exact = inside_search.search_pack(pack, "joss")
    assert len(exact) == 1
    assert exact[0]["id"] == atom["id"]
    typo = inside_search.search_pack(pack, "reproducibility")
    assert typo[0]["id"] == atom["id"]
    typo = inside_search.search_pack(pack, "reproducability")
    assert typo[0]["id"] == atom["id"]


def test_due_atom_hits_without_query_overlap() -> None:
    due = voice(
        text="Review this lesson on the due clock.",
        due_at="2000-01-01T00:00:00.000Z",
    )
    quiet = voice(text="Unrelated high trust habit.", trust=9.0)
    pack = {"user": "", "memory": "", "atoms": [due, quiet]}
    hits = inside_search.search_pack_linear(pack, "zircon")
    ids = [h["id"] for h in hits]
    assert due["id"] in ids
    assert quiet["id"] not in ids


def test_engine_search_leads_with_due_atom() -> None:
    due = voice(
        text="Review this lesson on the due clock.",
        due_at="2000-01-01T00:00:00.000Z",
    )
    quiet = voice(text="Unrelated high trust habit.", trust=9.0)
    pack = {"user": "", "memory": "", "atoms": [due, quiet]}
    hits, _engine = inside_search.search_pack_with_engine(pack, "zircon")
    ids = [h["id"] for h in hits]
    assert ids and ids[0] == due["id"]
    assert quiet["id"] not in ids


def test_expired_atom_absent() -> None:
    stale = inside_memory.make_atom(
        workspace=WS,
        text="JOSS stale snapshot.",
        kind="cache-pointer",
        about_peer="rgoswami",
        by_peer="hermes",
        valid_to="2000-01-01T00:00:00.000Z",
    )
    assert inside_search.search_pack({"user": "", "memory": "", "atoms": [stale]}, "joss") == []


def test_pack_documents_skips_dead_atoms() -> None:
    live = inside_memory.make_atom(
        workspace=WS,
        text="JOSS reviews.",
        kind="voice",
        about_peer="rgoswami",
        by_peer="hermes",
    )
    stale = inside_memory.make_atom(
        workspace=WS,
        text="JOSS stale snapshot.",
        kind="cache-pointer",
        about_peer="rgoswami",
        by_peer="hermes",
        valid_to="2000-01-01T00:00:00.000Z",
    )
    docs = inside_search.pack_documents(
        {
            "workspace": WS,
            "user": "Be brief.",
            "memory": "Read paper.pdf first.",
            "atoms": [live, stale],
        }
    )
    ids = {doc["id"] for doc in docs}
    assert "user" in ids
    mem_id = inside_search.document_id("memory", workspace=WS)
    assert mem_id in ids
    assert mem_id.replace("_", "").isalnum()
    assert live["id"] in ids
    assert stale["id"] not in ids


def test_borda_two_lists_of_three() -> None:
    left = [("f", "x"), ("f", "y"), ("f", "z")]
    right = [("f", "y"), ("f", "x"), ("f", "z")]
    assert inside_search.borda_merge([left, right], 3) == left
    left = [("f", "a"), ("f", "b"), ("f", "c")]
    right = [("f", "b"), ("f", "c"), ("f", "a")]
    assert inside_search.borda_merge([left, right], 3) == [
        ("f", "b"),
        ("f", "a"),
        ("f", "c"),
    ]


def test_rrf_two_lists_of_three() -> None:
    left = [("f", "x"), ("f", "y"), ("f", "z")]
    right = [("f", "y"), ("f", "x"), ("f", "z")]
    assert inside_search.rrf_merge([left, right], 60) == left


def test_rrf_omitted_rank_lifts_z_above_borda_last() -> None:
    left = [("f", "x"), ("f", "y"), ("f", "z")]
    right = [("f", "y"), ("f", "x"), ("f", "z")]
    only_z = [("f", "z")]
    assert inside_search.borda_merge([left, right, only_z], 3) == left
    assert inside_search.rrf_merge([left, right, only_z], 60)[0] == ("f", "z")


def test_dowdall_two_lists_of_three() -> None:
    left = [("f", "a"), ("f", "b"), ("f", "c")]
    right = [("f", "b"), ("f", "c"), ("f", "a")]
    assert inside_search.borda_merge([left, right], 3) == [
        ("f", "b"),
        ("f", "a"),
        ("f", "c"),
    ]
    ranked, scores = inside_search.dowdall_scores([left, right], 3)
    assert ranked == [("f", "b"), ("f", "a"), ("f", "c")]
    assert scores[("f", "a")] == pytest.approx(1.0 + 1.0 / 3.0)
    assert scores[("f", "b")] == pytest.approx(0.5 + 1.0)
    assert scores[("f", "c")] == pytest.approx(1.0 / 3.0 + 0.5)


def test_dowdall_last_place_pile_keeps_first_place() -> None:
    first = [("f", "a"), ("f", "c"), ("f", "d"), ("f", "e"), ("f", "z")]
    second = [("f", "b"), ("f", "c"), ("f", "d"), ("f", "e"), ("f", "z")]
    third = [("f", "a"), ("f", "b"), ("f", "c"), ("f", "d"), ("f", "z")]
    assert inside_search.borda_merge([first, second, third], 5)[0] == ("f", "c")
    assert inside_search.dowdall_merge([first, second, third], 5) == [
        ("f", "a"),
        ("f", "b"),
        ("f", "c"),
        ("f", "d"),
        ("f", "z"),
        ("f", "e"),
    ]


def test_kemeny_two_lists_first_seen() -> None:
    left = [("f", "a"), ("f", "b"), ("f", "c")]
    right = [("f", "b"), ("f", "a"), ("f", "c")]
    assert inside_search.kemeny_merge([left, right], 3) == left
    left = [("f", "b"), ("f", "a"), ("f", "c")]
    right = [("f", "a"), ("f", "b"), ("f", "c")]
    assert inside_search.kemeny_merge([left, right], 3) == left


def test_kemeny_cycle_of_three_pins_first_seen() -> None:
    first = [("f", "a"), ("f", "b"), ("f", "c")]
    second = [("f", "b"), ("f", "c"), ("f", "a")]
    third = [("f", "c"), ("f", "a"), ("f", "b")]
    assert inside_search.kemeny_merge([first, second, third], 3) == first


def test_merge_is_borda_then_mmr(
    scored_pair: tuple[list[dict[str, Any]], list[dict[str, Any]]],
) -> None:
    primary, secondary = scored_pair
    ids = [h["id"] for h in inside_search._merge_hits(primary, secondary, 3)]
    assert ids[0] == "b"
    assert set(ids) == {"a", "b", "c"}


def test_mmr_after_borda_splits_near_duplicates() -> None:
    items = [
        (("f", "keep"), 1.0, {"review", "open", "repro"}),
        (("f", "dup"), 0.55, {"review", "open", "repro"}),
        (("f", "other"), 0.5, {"pin", "zircon", "index"}),
    ]
    order = inside_search.mmr_rerank(items, 0.7)
    assert order[0] == ("f", "keep")
    assert order[1] == ("f", "other")
    assert order[2] == ("f", "dup")


def test_default_panel_matches_borda_then_mmr(
    scored_pair: tuple[list[dict[str, Any]], list[dict[str, Any]]],
) -> None:
    primary, secondary = scored_pair
    assert inside_search.resolve_panel() == ("borda", "mmr", "off")
    implicit = [h["id"] for h in inside_search._merge_hits(primary, secondary, 3)]
    named = [
        h["id"]
        for h in inside_search._merge_hits(primary, secondary, 3, fuse="borda", diversify="mmr")
    ]
    assert implicit == named
    assert implicit[0] == "b"
    assert set(implicit) == {"a", "b", "c"}


def test_diversify_none_keeps_borda_order(
    scored_pair: tuple[list[dict[str, Any]], list[dict[str, Any]]],
) -> None:
    primary, secondary = scored_pair
    ids = [h["id"] for h in inside_search._merge_hits(primary, secondary, 3, diversify="none")]
    assert ids == ["b", "a", "c"]


def test_parse_rrf_is_a_fuse() -> None:
    assert inside_search.parse_fuse("rrf") == "rrf"
    assert inside_search.resolve_panel("rrf", "mmr") == ("rrf", "mmr", "off")
    left = [
        {"field": "atom", "id": "x", "text": "x", "score": 1.0},
        {"field": "atom", "id": "y", "text": "y", "score": 0.5},
        {"field": "atom", "id": "z", "text": "z", "score": 0.1},
    ]
    right = [
        {"field": "atom", "id": "y", "text": "y", "score": 1.0},
        {"field": "atom", "id": "x", "text": "x", "score": 0.5},
        {"field": "atom", "id": "z", "text": "z", "score": 0.1},
    ]
    ids = [h["id"] for h in inside_search._merge_hits(left, right, 3, fuse="rrf", diversify="none")]
    assert ids == ["x", "y", "z"]


def test_parse_dowdall_is_a_fuse() -> None:
    assert inside_search.parse_fuse("dowdall") == "dowdall"
    assert inside_search.resolve_panel("dowdall", "none") == ("dowdall", "none", "off")
    first = [
        {"field": "atom", "id": "a", "text": "a", "score": 1.0},
        {"field": "atom", "id": "c", "text": "c", "score": 0.4},
        {"field": "atom", "id": "d", "text": "d", "score": 0.3},
        {"field": "atom", "id": "e", "text": "e", "score": 0.2},
        {"field": "atom", "id": "z", "text": "z", "score": 0.1},
    ]
    second = [
        {"field": "atom", "id": "b", "text": "b", "score": 1.0},
        {"field": "atom", "id": "c", "text": "c", "score": 0.4},
        {"field": "atom", "id": "d", "text": "d", "score": 0.3},
        {"field": "atom", "id": "e", "text": "e", "score": 0.2},
        {"field": "atom", "id": "z", "text": "z", "score": 0.1},
    ]
    ids = [
        h["id"]
        for h in inside_search._merge_hits(first, second, 5, fuse="dowdall", diversify="none")
    ]
    assert ids[0] == "a"


def test_parse_kemeny_is_a_fuse() -> None:
    assert inside_search.parse_fuse("kemeny") == "kemeny"
    assert inside_search.resolve_panel("kemeny", "mmr") == ("kemeny", "mmr", "off")
    left = [
        {"field": "atom", "id": "a", "text": "a", "score": 1.0},
        {"field": "atom", "id": "b", "text": "b", "score": 0.5},
        {"field": "atom", "id": "c", "text": "c", "score": 0.1},
    ]
    right = [
        {"field": "atom", "id": "b", "text": "b", "score": 1.0},
        {"field": "atom", "id": "a", "text": "a", "score": 0.5},
        {"field": "atom", "id": "c", "text": "c", "score": 0.1},
    ]
    ids = [
        h["id"] for h in inside_search._merge_hits(left, right, 3, fuse="kemeny", diversify="none")
    ]
    assert ids == ["a", "b", "c"]


@pytest.mark.parametrize(
    "call",
    [
        lambda: inside_search.parse_fuse("not-a-voter"),
        lambda: inside_search.parse_diversify("not-a-voter"),
        lambda: inside_search.parse_decay("maybe"),
        lambda: inside_search._merge_hits([], [], 1, fuse="not-a-voter"),
    ],
    ids=["fuse", "diversify", "decay", "merge"],
)
def test_unknown_voter_is_error(call: Callable[[], object]) -> None:
    with pytest.raises(inside_search.UnknownVoter):
        call()


@pytest.mark.parametrize("name", ["combmnz", "schulze", "copeland", "tideman"])
def test_unimplemented_fuse_is_error(name: str) -> None:
    with pytest.raises(inside_search.UnknownVoter, match="not implemented"):
        inside_search.parse_fuse(name)
    with pytest.raises(inside_search.UnknownVoter, match="not implemented"):
        inside_search._merge_hits([], [], 1, fuse=name)


def test_unimplemented_diversify_is_error() -> None:
    with pytest.raises(inside_search.UnknownVoter, match="not implemented"):
        inside_search.parse_diversify("dpp")


def test_env_borda_mmr_matches_merge_fixtures(
    monkeypatch: pytest.MonkeyPatch,
    scored_pair: tuple[list[dict[str, Any]], list[dict[str, Any]]],
) -> None:
    monkeypatch.setenv("PACKSET_FUSE", "borda")
    monkeypatch.setenv("PACKSET_DIVERSIFY", "mmr")
    monkeypatch.setenv("PACKSET_DECAY", "off")
    primary, secondary = scored_pair
    ids = [h["id"] for h in inside_search._merge_hits(primary, secondary, 3)]
    assert ids[0] == "b"
    assert set(ids) == {"a", "b", "c"}


def test_env_rrf_changes_omitted_rank_order(monkeypatch: pytest.MonkeyPatch) -> None:
    left = [
        {"field": "atom", "id": "x", "text": "x", "score": 1.0},
        {"field": "atom", "id": "y", "text": "y", "score": 0.5},
        {"field": "atom", "id": "z", "text": "z", "score": 0.1},
    ]
    right = [
        {"field": "atom", "id": "y", "text": "y", "score": 1.0},
        {"field": "atom", "id": "x", "text": "x", "score": 0.5},
        {"field": "atom", "id": "z", "text": "z", "score": 0.1},
    ]
    only_z = [{"field": "atom", "id": "z", "text": "z", "score": 1.0}]
    ballots = [left, right, only_z]
    monkeypatch.setenv("PACKSET_FUSE", "borda")
    monkeypatch.setenv("PACKSET_DIVERSIFY", "none")
    borda_ids = [h["id"] for h in inside_search._merge_ballots(ballots, 3)]
    monkeypatch.setenv("PACKSET_FUSE", "rrf")
    rrf_ids = [h["id"] for h in inside_search._merge_ballots(ballots, 3)]
    assert borda_ids == ["x", "y", "z"]
    assert rrf_ids[0] == "z"
    assert borda_ids != rrf_ids


def test_env_unknown_fuse_is_error(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("PACKSET_FUSE", "not-a-voter")
    with pytest.raises(inside_search.UnknownVoter):
        inside_search.resolve_panel()
    with pytest.raises(inside_search.UnknownVoter):
        inside_search._merge_hits([], [], 1)
    monkeypatch.setenv("PACKSET_FUSE", "")
    with pytest.raises(inside_search.UnknownVoter):
        inside_search.resolve_panel()


def test_linear_when_milli_absent() -> None:
    if inside_search.milli_bin() is not None:
        pytest.skip("inside-milli binary is present")
    pack = {"workspace": WS, "user": "Be brief.", "memory": "", "atoms": []}
    hits, engine = inside_search.search_pack_with_engine(pack, "brief")
    assert engine == "linear"
    assert [(h["field"], h.get("text")) for h in hits] == [("user", "Be brief.")]


def test_short_query_is_exact() -> None:
    pack = {
        "user": "Prefers Conventional Commits.\n",
        "memory": "",
        "atoms": [],
    }
    assert inside_search.search_pack(pack, "pr") == []
    assert inside_search.search_pack(pack, "prefers")[0]["field"] == "user"


def test_stopwords_do_not_hit() -> None:
    pack = {
        "user": "The review queue is the bottleneck.\n",
        "memory": "",
        "atoms": [],
    }
    assert inside_search.search_pack(pack, "What color is the sky?") == []
    assert inside_search.search_pack(pack, "review")[0]["field"] == "user"


def test_tombstone_absent() -> None:
    atom = inside_memory.make_atom(
        workspace=WS,
        text="JOSS reviews.",
        kind="voice",
        about_peer="rgoswami",
        by_peer="hermes",
    )
    atom["tombstone"] = True
    assert inside_search.search_pack({"user": "", "memory": "", "atoms": [atom]}, "joss") == []


def test_set_filter_on_full_atom_list(
    review_debug: tuple[dict[str, Any], dict[str, Any]],
) -> None:
    review, debug = review_debug
    pack = {
        "workspace": WS,
        "user": "Open a review with the defect.\n",
        "memory": "",
        "atoms": [review, debug],
    }
    unscoped = inside_search.search_pack_linear(pack, "zircon")
    unscoped_ids = {h["id"] for h in unscoped if h.get("field") == "atom"}
    assert unscoped_ids == {review["id"], debug["id"]}

    review_hits = inside_search.search_pack(pack, "zircon", set_name="review")
    review_ids = [h["id"] for h in review_hits if h.get("field") == "atom"]
    assert review_ids == [review["id"]]
    assert debug["id"] not in review_ids

    prose = inside_search.search_pack(pack, "defect", set_name="review")
    assert [(h["field"], h.get("text")) for h in prose] == [
        ("user", "Open a review with the defect."),
    ]

    assert inside_search.atom_document(review)["set"] == "review"
    assert inside_search.atom_document(debug)["set"] == "debug"


def test_reindex_atoms_skips_user_memory_docs(
    review_debug: tuple[dict[str, Any], dict[str, Any]],
) -> None:
    review, _debug = review_debug
    pack = {
        "workspace": WS,
        "set": "review",
        "user": "SETCARDONLYPHRASE for the pin.\n",
        "memory": "SETMEMORYPHRASE for the pin.\n",
        "atoms": [review],
    }
    docs = [
        inside_search.atom_document(atom)
        for atom in inside_search._live_atoms(pack)
        if atom.get("id")
    ]
    assert len(docs) == 1
    assert docs[0]["field"] == "atom"
    assert "user" not in {d["field"] for d in docs}
    full = inside_search.pack_documents(pack)
    assert "user" in {d["field"] for d in full}


def test_set_miss_does_not_poison_workspace_prose_in_index(
    milli_index: Path,
    review_debug: tuple[dict[str, Any], dict[str, Any]],
) -> None:
    review, debug = review_debug
    workspace_pack = {
        "workspace": WS,
        "user": "Workspace prose mentions UNIQUEWORKSPACEPHRASE only.\n",
        "memory": "",
        "atoms": [review, debug],
    }
    set_pack = {
        "workspace": WS,
        "set": "review",
        "user": "Set card mentions UNIQUESETPHRASE only.\n",
        "memory": "",
        "atoms": [review, debug],
    }
    assert inside_search.replace_index(workspace_pack, milli_index)
    set_hits = inside_search.search_pack(
        set_pack,
        "UNIQUESETPHRASE",
        index_dir=milli_index,
        set_name="review",
    )
    set_user = [h.get("text") or "" for h in set_hits if h.get("field") == "user"]
    assert len(set_user) == 1
    assert "UNIQUESETPHRASE" in set_user[0]
    inside_search.search_pack(
        set_pack,
        "nomatchtokenxyz",
        index_dir=milli_index,
        set_name="review",
    )
    unscoped_set = inside_search.search_pack(
        workspace_pack, "UNIQUESETPHRASE", index_dir=milli_index
    )
    assert [h.get("text") for h in unscoped_set if "UNIQUESETPHRASE" in (h.get("text") or "")] == []
    unscoped = inside_search.search_pack(
        workspace_pack,
        "UNIQUEWORKSPACEPHRASE",
        index_dir=milli_index,
    )
    workspace_user = [h.get("text") or "" for h in unscoped if h.get("field") == "user"]
    assert len(workspace_user) == 1
    assert "UNIQUEWORKSPACEPHRASE" in workspace_user[0]


def test_stale_index_without_set_field_still_scopes(
    milli_index: Path,
    review_debug: tuple[dict[str, Any], dict[str, Any]],
) -> None:
    review, debug = review_debug
    pack = {
        "workspace": WS,
        "user": "",
        "memory": "",
        "atoms": [review, debug],
    }
    stale_docs = []
    for atom in (review, debug):
        doc = inside_search.atom_document(atom)
        doc.pop("set", None)
        stale_docs.append(doc)
    payload = inside_search._run_milli(
        ["index", "--index", str(milli_index), "--replace"],
        stdin="\n".join(json.dumps(d) for d in stale_docs) + "\n",
        timeout=60.0,
    )
    assert payload is not None
    hits = inside_search.search_pack(pack, "zircon", index_dir=milli_index, set_name="review")
    atom_ids = [h["id"] for h in hits if h.get("field") == "atom"]
    assert review["id"] in atom_ids
    assert debug["id"] not in atom_ids
