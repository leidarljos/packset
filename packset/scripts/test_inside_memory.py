"""Shared identity and the memory pack."""

from __future__ import annotations

import subprocess
from pathlib import Path

import pytest

import inside_identity
import inside_memory

WS = "git:github.com/HaoZeke/joss-reviews"


def test_normalize_https_and_ssh() -> None:
    https = inside_identity.normalize_remote("https://github.com/HaoZeke/grok-inside.git")
    ssh = inside_identity.normalize_remote("git@github.com:HaoZeke/grok-inside.git")
    assert https == "git:github.com/HaoZeke/grok-inside"
    assert ssh == https


def test_same_workspace_for_two_clients() -> None:
    remote = "https://github.com/HaoZeke/joss-reviews.git"
    hermes = inside_identity.identity(
        harness="hermes", remote=remote, cwd=Path("/tmp/joss-reviews")
    )
    codex = inside_identity.identity(harness="codex", remote=remote, cwd=Path("/tmp/joss-reviews"))
    assert hermes["workspace"] == codex["workspace"]
    assert hermes["workspace"] == "git:github.com/HaoZeke/joss-reviews"
    assert hermes["agent_peer"] == "hermes"
    assert codex["agent_peer"] == "codex"


def test_per_directory_and_global() -> None:
    root = Path("/tmp/some-tree").resolve()
    assert inside_identity.resolve_workspace(cwd=root, strategy="per-directory") == (f"dir:{root}")
    assert inside_identity.resolve_workspace(cwd=root, strategy="global") == "global"


def test_git_remote_from_repo(tmp_path: Path) -> None:
    subprocess.run(["git", "init"], cwd=tmp_path, check=True, capture_output=True)
    subprocess.run(
        ["git", "remote", "add", "origin", "git@gitlab.com:me/proj.git"],
        cwd=tmp_path,
        check=True,
        capture_output=True,
    )
    ident = inside_identity.identity(harness="pi", cwd=tmp_path)
    assert ident["workspace"] == "git:gitlab.com/me/proj"


def test_user_overflow_is_an_error(tmp_path: Path) -> None:
    with pytest.raises(inside_memory.MemoryOverflow):
        inside_memory.set_user("x" * (inside_memory.USER_CAP + 1), home=tmp_path)


def test_memory_overflow_is_an_error(tmp_path: Path) -> None:
    with pytest.raises(inside_memory.MemoryOverflow):
        inside_memory.set_memory(WS, "y" * (inside_memory.MEMORY_CAP + 1), home=tmp_path)


def test_user_under_cap_round_trips(tmp_path: Path) -> None:
    inside_memory.set_user("Prefers Conventional Commits.\n", home=tmp_path)
    assert "Conventional Commits" in inside_memory.read_text(inside_memory.user_path(tmp_path))


def test_duplicate_add_is_noop(tmp_path: Path) -> None:
    path = inside_memory.user_path(tmp_path)
    inside_memory.add_entry(path, "No thanks.", inside_memory.USER_CAP)
    inside_memory.add_entry(path, "No thanks.", inside_memory.USER_CAP)
    assert inside_memory.read_text(path).count("No thanks.") == 1


def test_replace_needs_one_match(tmp_path: Path) -> None:
    path = inside_memory.user_path(tmp_path)
    inside_memory.add_entry(path, "Alpha note", inside_memory.USER_CAP)
    inside_memory.add_entry(path, "Beta note", inside_memory.USER_CAP)
    with pytest.raises(inside_memory.AtomError):
        inside_memory.replace_entry(path, "note", "Gamma", inside_memory.USER_CAP)
    inside_memory.replace_entry(path, "Alpha", "Gamma note", inside_memory.USER_CAP)
    text = inside_memory.read_text(path)
    assert "Gamma note" in text
    assert "Alpha note" not in text


def test_atom_add_update_delete(tmp_path: Path) -> None:
    atom = inside_memory.make_atom(
        workspace=WS,
        text="Reviews open with a reproducibility check.",
        kind="voice",
        about_peer="rgoswami",
        by_peer="hermes",
    )
    stored = inside_memory.add_atom(atom, home=tmp_path)
    again = inside_memory.add_atom(atom, home=tmp_path)
    assert stored["id"] == again["id"]
    assert len(inside_memory.current_atoms(WS, tmp_path)) == 1

    updated = inside_memory.update_atom(
        WS,
        stored["id"],
        {"text": "Reviews open with scope and a reproducibility check."},
        home=tmp_path,
    )
    assert updated["text"] != stored["text"]
    assert len(inside_memory.current_atoms(WS, tmp_path)) == 1

    inside_memory.delete_atom(WS, stored["id"], home=tmp_path)
    assert inside_memory.current_atoms(WS, tmp_path) == []
    log = inside_memory.load_atoms(WS, tmp_path)
    assert log[-1]["tombstone"]


def test_unknown_kind_rejected() -> None:
    with pytest.raises(inside_memory.AtomError):
        inside_memory.make_atom(
            workspace=WS,
            text="nope",
            kind="vibes",
            about_peer="rgoswami",
            by_peer="pi",
        )


def test_secret_text_rejected(tmp_path: Path) -> None:
    with pytest.raises(inside_memory.AtomError):
        inside_memory.set_user("api_key=sk-not-a-real-key", home=tmp_path)


def test_english_secrets_sentence_writes(tmp_path: Path) -> None:
    inside_memory.set_user("Never commit secrets to git.\n", home=tmp_path)
    assert "secrets" in inside_memory.read_text(inside_memory.user_path(tmp_path))


def test_expired_atom_absent_from_current(tmp_path: Path) -> None:
    past = "2000-01-01T00:00:00.000Z"
    atom = inside_memory.make_atom(
        workspace=WS,
        text="Stale remote snapshot.",
        kind="cache-pointer",
        about_peer="rgoswami",
        by_peer="hermes",
        valid_to=past,
    )
    stored = inside_memory.add_atom(atom, home=tmp_path)
    assert inside_memory.current_atoms(WS, tmp_path) == []
    log = inside_memory.load_atoms(WS, tmp_path)
    assert log[-1]["id"] == stored["id"]
    assert log[-1]["valid_to"] == past
    assert not log[-1].get("tombstone")


def test_open_and_missing_valid_to_still_current(tmp_path: Path) -> None:
    open_atom = inside_memory.make_atom(
        workspace=WS,
        text="Open validity claim.",
        kind="voice",
        about_peer="rgoswami",
        by_peer="hermes",
        valid_to=None,
    )
    future = inside_memory.make_atom(
        workspace=WS,
        text="Still inside the validity window.",
        kind="habit",
        about_peer="rgoswami",
        by_peer="hermes",
        valid_to="2099-01-01T00:00:00.000Z",
    )
    bare = {
        "workspace": WS,
        "text": "No validity field at all.",
        "kind": "preference",
        "about_peer": "rgoswami",
        "by_peer": "pi",
    }
    inside_memory.add_atom(open_atom, home=tmp_path)
    inside_memory.add_atom(future, home=tmp_path)
    inside_memory.add_atom(bare, home=tmp_path)
    texts = {a["text"] for a in inside_memory.current_atoms(WS, tmp_path)}
    assert texts == {
        "Open validity claim.",
        "Still inside the validity window.",
        "No validity field at all.",
    }


def test_extract_entities_from_text() -> None:
    from_text = inside_memory.extract_entities({"text": "See `AlphaRepo` and JOSS Reviews."})
    assert "AlphaRepo" in from_text
    assert "JOSS" in from_text
    assert "Reviews" in from_text
    explicit = inside_memory.extract_entities(
        {"text": "ignored text", "entities": ["JOSS", "Reviews"]}
    )
    assert explicit == {"JOSS", "Reviews"}


def test_entity_overlap_links_both_ways(tmp_path: Path) -> None:
    first = inside_memory.make_atom(
        workspace=WS,
        text="JOSS reviews open with a reproducibility check.",
        kind="voice",
        about_peer="rgoswami",
        by_peer="hermes",
        entities=["JOSS"],
    )
    second = inside_memory.make_atom(
        workspace=WS,
        text="JOSS labels need a dated snapshot.",
        kind="habit",
        about_peer="rgoswami",
        by_peer="hermes",
        entities=["JOSS"],
    )
    third = inside_memory.make_atom(
        workspace=WS,
        text="unrelated lowercase only.",
        kind="preference",
        about_peer="rgoswami",
        by_peer="pi",
        entities=["Unrelated"],
    )
    stored_a = inside_memory.add_atom(first, home=tmp_path)
    stored_b = inside_memory.add_atom(second, home=tmp_path)
    stored_c = inside_memory.add_atom(third, home=tmp_path)
    live = {a["id"]: a for a in inside_memory.current_atoms(WS, tmp_path)}
    assert stored_b["id"] in live[stored_a["id"]]["links"]
    assert stored_a["id"] in live[stored_b["id"]]["links"]
    assert live[stored_c["id"]]["links"] == []
    assert stored_c["id"] not in live[stored_a["id"]]["links"]
    assert stored_c["id"] not in live[stored_b["id"]]["links"]


def test_new_claim_rewrites_old_live_links(tmp_path: Path) -> None:
    first = inside_memory.make_atom(
        workspace=WS,
        text="AlphaRepo uses Conventional Commits.",
        kind="habit",
        about_peer="rgoswami",
        by_peer="hermes",
    )
    stored_a = inside_memory.add_atom(first, home=tmp_path)
    assert (stored_a.get("links") or []) == []
    second = inside_memory.make_atom(
        workspace=WS,
        text="AlphaRepo reviews open with a check.",
        kind="voice",
        about_peer="rgoswami",
        by_peer="hermes",
    )
    stored_b = inside_memory.add_atom(second, home=tmp_path)
    live = {a["id"]: a for a in inside_memory.current_atoms(WS, tmp_path)}
    assert stored_b["id"] in live[stored_a["id"]]["links"]
    assert stored_a["id"] in live[stored_b["id"]]["links"]
    log = inside_memory.load_atoms(WS, tmp_path)
    a_versions = [rec for rec in log if rec["id"] == stored_a["id"]]
    assert len(a_versions) >= 2
    assert stored_b["id"] in a_versions[-1]["links"]


def test_tombstoned_atoms_leave_live_neighbourhoods(tmp_path: Path) -> None:
    first = inside_memory.make_atom(
        workspace=WS,
        text="JOSS reviews open with a reproducibility check.",
        kind="voice",
        about_peer="rgoswami",
        by_peer="hermes",
        entities=["JOSS"],
    )
    second = inside_memory.make_atom(
        workspace=WS,
        text="JOSS labels need a dated snapshot.",
        kind="habit",
        about_peer="rgoswami",
        by_peer="hermes",
        entities=["JOSS"],
    )
    stored_a = inside_memory.add_atom(first, home=tmp_path)
    stored_b = inside_memory.add_atom(second, home=tmp_path)
    inside_memory.delete_atom(WS, stored_a["id"], home=tmp_path)
    live = inside_memory.current_atoms(WS, tmp_path)
    assert len(live) == 1
    assert live[0]["id"] == stored_b["id"]
    assert stored_a["id"] not in (live[0].get("links") or [])


def test_expired_atoms_are_not_in_live_graph(tmp_path: Path) -> None:
    stale = inside_memory.make_atom(
        workspace=WS,
        text="Expired JOSS snapshot.",
        kind="cache-pointer",
        about_peer="rgoswami",
        by_peer="hermes",
        valid_to="2000-01-01T00:00:00.000Z",
        entities=["JOSS"],
    )
    live_atom = inside_memory.make_atom(
        workspace=WS,
        text="Live JOSS claim.",
        kind="voice",
        about_peer="rgoswami",
        by_peer="hermes",
        entities=["JOSS"],
    )
    inside_memory.add_atom(stale, home=tmp_path)
    stored_live = inside_memory.add_atom(live_atom, home=tmp_path)
    live = inside_memory.current_atoms(WS, tmp_path)
    assert [a["id"] for a in live] == [stored_live["id"]]
    assert (live[0].get("links") or []) == []


def test_cache_pointer_stores_timestamped_snapshot(tmp_path: Path) -> None:
    stamp = "2026-08-11T12:00:00.000Z"
    ptr = inside_memory.make_cache_pointer(
        WS,
        f"open issues @ {stamp}: 3 labeled needs-review",
        valid_to="2099-01-01T00:00:00.000Z",
        about_peer="rgoswami",
        by_peer="hermes",
    )
    assert ptr["kind"] == "cache-pointer"
    assert stamp in ptr["text"]
    stored = inside_memory.add_atom(ptr, home=tmp_path)
    live = inside_memory.current_atoms(WS, tmp_path)
    assert len(live) == 1
    assert live[0]["id"] == stored["id"]
    assert live[0]["kind"] == "cache-pointer"


def test_cache_pointer_live_only_while_valid_to_open(tmp_path: Path) -> None:
    stale = inside_memory.make_cache_pointer(
        WS,
        "open issues @ 2000-01-01T00:00:00.000Z: 4 labeled needs-review",
        valid_to="2000-01-01T00:00:00.000Z",
        about_peer="rgoswami",
        by_peer="hermes",
    )
    open_ptr = inside_memory.make_cache_pointer(
        WS,
        "open issues @ 2026-08-11T12:00:00.000Z: 2 labeled needs-review",
        valid_to="2099-01-01T00:00:00.000Z",
        about_peer="rgoswami",
        by_peer="hermes",
    )
    inside_memory.add_atom(stale, home=tmp_path)
    stored = inside_memory.add_atom(open_ptr, home=tmp_path)
    live = inside_memory.current_atoms(WS, tmp_path)
    assert [a["id"] for a in live] == [stored["id"]]
    assert len(inside_memory.load_atoms(WS, tmp_path)) == 2


def test_cache_pointer_refresh_updates_text_and_validity(tmp_path: Path) -> None:
    ptr = inside_memory.make_cache_pointer(
        WS,
        "open issues @ 2026-08-01T00:00:00.000Z: 4 labeled needs-review",
        valid_to="2099-01-01T00:00:00.000Z",
        about_peer="rgoswami",
        by_peer="hermes",
    )
    stored = inside_memory.add_atom(ptr, home=tmp_path)
    new_from = inside_memory.utcnow()
    new_to = "2099-06-01T00:00:00.000Z"
    new_text = "open issues @ 2026-08-11T12:00:00.000Z: 2 labeled needs-review"
    refreshed = inside_memory.update_atom(
        WS,
        stored["id"],
        {
            "text": new_text,
            "valid_from": new_from,
            "valid_to": new_to,
        },
        home=tmp_path,
    )
    assert refreshed["id"] == stored["id"]
    assert refreshed["kind"] == "cache-pointer"
    assert refreshed["text"] == new_text
    assert refreshed["valid_from"] == new_from
    assert refreshed["valid_to"] == new_to
    live = inside_memory.current_atoms(WS, tmp_path)
    assert len(live) == 1
    assert live[0]["text"] == new_text


def test_tombstones_still_disappear_from_current(tmp_path: Path) -> None:
    atom = inside_memory.make_atom(
        workspace=WS,
        text="Reviews open with a reproducibility check.",
        kind="voice",
        about_peer="rgoswami",
        by_peer="hermes",
    )
    stored = inside_memory.add_atom(atom, home=tmp_path)
    inside_memory.delete_atom(WS, stored["id"], home=tmp_path)
    assert inside_memory.current_atoms(WS, tmp_path) == []
    log = inside_memory.load_atoms(WS, tmp_path)
    assert log[-1]["tombstone"]


def test_hermes_home_view_is_write_through(tmp_path: Path) -> None:
    inside_memory.set_user("No thanks.\n", home=tmp_path)
    inside_memory.set_memory(WS, "Read paper.pdf first.\n", home=tmp_path)
    isolated = tmp_path / "hermes-inside"
    views = inside_memory.install_home_view(
        isolated,
        layout="hermes",
        workspace=WS,
        pack_home=tmp_path,
    )
    assert views["user"].is_symlink()
    assert views["memory"].is_symlink()
    assert views["user"].read_text(encoding="utf-8") == "No thanks.\n"
    assert views["memory"].read_text(encoding="utf-8") == "Read paper.pdf first.\n"
    views["memory"].write_text("Updated via Hermes.\n", encoding="utf-8")
    assert (
        inside_memory.read_text(inside_memory.memory_path(WS, tmp_path)) == "Updated via Hermes.\n"
    )
    assert not (isolated / "memory.sqlite").exists()
    assert not (isolated / "memory.lmdb").exists()
    assert not (isolated / "memories" / "atoms.jsonl").exists()


def test_pi_home_view_writes_agents_snapshot(tmp_path: Path) -> None:
    inside_memory.set_user("Be brief.\n", home=tmp_path)
    inside_memory.set_memory(WS, "Open with a check.\n", home=tmp_path)
    isolated = tmp_path / "pi-inside"
    views = inside_memory.install_home_view(
        isolated,
        layout="pi",
        workspace=WS,
        pack_home=tmp_path,
    )
    assert views["user"].is_symlink()
    assert views["memory"].is_symlink()
    agents = views["agents"].read_text(encoding="utf-8")
    assert "Seat memory (view)" in agents
    assert "Be brief." not in agents
    assert "Open with a check." not in agents
    assert "USER.md" in agents
    assert not (isolated / "memory.sqlite").exists()
