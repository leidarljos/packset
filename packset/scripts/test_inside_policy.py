"""Inspect / select / splice. No daemon required."""

from __future__ import annotations

import json
from typing import Any

import pytest

import inside_memory
import inside_policy


def pack() -> dict[str, Any]:
    return {
        "workspace": "git:example.com/proj",
        "user": "Prefers Conventional Commits.\n",
        "memory": "Read paper.pdf first.\n",
        "atoms": [
            {
                "id": "v1",
                "kind": "voice",
                "text": "Speaks in short sentences.",
                "tombstone": False,
            },
            {
                "id": "h1",
                "kind": "habit",
                "text": "Reviews open with a reproducibility check.",
                "tombstone": False,
            },
            {
                "id": "c1",
                "kind": "cache-pointer",
                "text": "Open issues snapshot 2026-08-01.",
                "tombstone": False,
            },
            {
                "id": "g1",
                "kind": "goal",
                "text": "Ship the memory splice.",
                "tombstone": False,
            },
            {
                "id": "b1",
                "kind": "belief",
                "text": "The review queue is the bottleneck.",
                "tombstone": False,
            },
        ],
    }


def claims(selected: dict[str, Any]) -> str:
    return inside_policy._selected_text(selected)


def test_review_turn_is_not_remote() -> None:
    hints = inside_policy.inspect({"messages": [{"role": "user", "content": "review this PR"}]})
    assert hints["user_text"] == "review this PR"
    assert hints["tool_names"] == []
    assert not hints["wants_remote"]


def test_github_or_host_sets_wants_remote() -> None:
    github = inside_policy.inspect(
        {"messages": [{"role": "user", "content": "list github issues"}]}
    )
    assert github["wants_remote"]
    host = inside_policy.inspect(
        {"messages": [{"role": "user", "content": "see docs.example.com/labels"}]}
    )
    assert host["wants_remote"]
    tools = inside_policy.inspect(
        {
            "messages": [{"role": "user", "content": "status"}],
            "tools": [{"type": "function", "function": {"name": "gh_issue_list"}}],
        }
    )
    assert tools["tool_names"] == ["gh_issue_list"]
    assert tools["wants_remote"]


def test_input_user_text() -> None:
    hints = inside_policy.inspect(
        {"input": [{"type": "message", "role": "user", "content": "hello"}]}
    )
    assert hints["user_text"] == "hello"


def test_type_user_without_role() -> None:
    hints = inside_policy.inspect(
        {
            "messages": [
                {
                    "type": "user",
                    "content": [
                        {
                            "type": "text",
                            "text": "What is the seat fix token SEATOK1?",
                        }
                    ],
                }
            ]
        }
    )
    assert hints["user_text"] == "What is the seat fix token SEATOK1?"


def test_user_query_wrapper_unwrapped() -> None:
    hints = inside_policy.inspect(
        {
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": ("<user_query>\nWhat is the seat fix token SEATOK1?\n</user_query>"),
                }
            ]
        }
    )
    assert hints["user_text"] == "What is the seat fix token SEATOK1?"


def test_inspect_uses_latest_user_turn() -> None:
    hints = inside_policy.inspect(
        {
            "messages": [
                {"role": "user", "content": "review this PR"},
                {"role": "assistant", "content": "Looking."},
                {"role": "user", "content": "What color is the sky?"},
            ]
        }
    )
    assert hints["user_text"] == "What color is the sky?"
    assert not hints["wants_remote"]


def test_inspect_skips_tool_result_user_messages() -> None:
    hints = inside_policy.inspect(
        {
            "messages": [
                {"role": "user", "content": "review this PR"},
                {
                    "role": "assistant",
                    "content": [
                        {
                            "type": "tool_use",
                            "id": "t1",
                            "name": "bash",
                            "input": {},
                        }
                    ],
                },
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "t1",
                            "content": "ok",
                        }
                    ],
                },
            ]
        }
    )
    assert hints["user_text"] == "review this PR"


def test_review_turn_gets_voice_and_habit_not_cache() -> None:
    hints = inside_policy.inspect({"messages": [{"role": "user", "content": "review this PR"}]})
    selected = inside_policy.select(pack(), hints)
    head_kinds = [atom["kind"] for atom in selected["head_atoms"]]
    tail_kinds = [atom["kind"] for atom in selected["tail_atoms"]]
    assert head_kinds == []
    assert "habit" in tail_kinds
    assert "goal" not in tail_kinds
    assert "cache-pointer" not in head_kinds + tail_kinds
    assert "belief" not in tail_kinds
    source = pack()
    assert selected["user"] == source["user"]
    assert selected["memory"] == source["memory"]
    block = claims(selected)
    assert "Reviews open with a reproducibility check." in block
    assert "Prefers Conventional Commits" not in block
    assert "Speaks in short sentences" not in block
    assert "Read paper.pdf first" not in block


def test_no_github_omits_cache_pointer() -> None:
    hints = inside_policy.inspect(
        {"messages": [{"role": "user", "content": "summarize the module"}]}
    )
    selected = inside_policy.select(pack(), hints)
    assert not hints["wants_remote"]
    kinds = [atom["kind"] for atom in selected["tail_atoms"]]
    assert "cache-pointer" not in kinds


def test_remote_turn_keeps_cache_pointer() -> None:
    hints = inside_policy.inspect(
        {"messages": [{"role": "user", "content": "refresh github issues"}]}
    )
    selected = inside_policy.select(pack(), hints)
    assert hints["wants_remote"]
    assert [atom["kind"] for atom in selected["tail_atoms"]] == ["cache-pointer"]


def test_head_prefix_is_byte_stable() -> None:
    body = pack()
    hints = inside_policy.inspect({"messages": [{"role": "user", "content": "review this PR"}]})
    first = inside_policy.select(body, hints)
    second = inside_policy.select(body, hints)
    assert first["head_prefix"] == second["head_prefix"]
    assert first["head_prefix"].encode("utf-8") == second["head_prefix"].encode("utf-8")
    assert "Reviews open with a reproducibility check." in first["tail_atoms"][0]["text"]
    assert "Speaks in short sentences." not in first["head_prefix"]


def test_unrelated_turn_selects_nothing() -> None:
    hints = inside_policy.inspect(
        {"messages": [{"role": "user", "content": "What color is the sky?"}]}
    )
    selected = inside_policy.select(pack(), hints)
    assert selected["tail_atoms"] == []
    assert claims(selected) == ""


def test_prior_review_then_sky_selects_nothing() -> None:
    hints = inside_policy.inspect(
        {
            "messages": [
                {"role": "user", "content": "review this PR"},
                {"role": "assistant", "content": "Looking."},
                {"role": "user", "content": "What color is the sky?"},
            ]
        }
    )
    selected = inside_policy.select(pack(), hints)
    assert selected["tail_atoms"] == []
    assert claims(selected) == ""


def test_short_token_does_not_prefix_match_prefers() -> None:
    hints = inside_policy.inspect({"messages": [{"role": "user", "content": "pr"}]})
    selected = inside_policy.select(pack(), hints)
    block = claims(selected)
    assert "Prefers Conventional Commits" not in block
    assert selected["tail_atoms"] == []
    assert block == ""


def test_duplicate_file_and_atom_text_is_kept_once() -> None:
    body = pack()
    body["user"] = "Prefer zircon-latch-42 on every review.\n"
    body["atoms"] = [
        {
            "id": "p1",
            "kind": "preference",
            "text": "Prefer zircon-latch-42 on every review.",
            "tombstone": False,
        }
    ]
    hints = inside_policy.inspect({"messages": [{"role": "user", "content": "review this PR"}]})
    selected = inside_policy.select(body, hints)
    block = claims(selected)
    assert block.count("Prefer zircon-latch-42 on every review.") == 1
    assert selected["tail_atoms"] == []


def test_judge_items_round_trip_drops_unkept() -> None:
    hints = inside_policy.inspect({"messages": [{"role": "user", "content": "review this PR"}]})
    selected = inside_policy.select(pack(), hints)
    items = inside_policy.items_from_selected(selected)
    assert len(items) >= 1
    kept = inside_policy.selected_from_items(selected, items[:1])
    assert len(inside_policy.items_from_selected(kept)) == 1
    assert inside_policy.items_from_selected(kept)[0]["text"] in items[0]["text"]


def test_overflow_on_select_does_not_trim() -> None:
    body = pack()
    body["user"] = "x" * (inside_memory.USER_CAP + 1)
    hints = inside_policy.inspect({"messages": [{"role": "user", "content": "review this PR"}]})
    with pytest.raises(inside_memory.MemoryOverflow):
        inside_policy.select(body, hints)


def selected_for(text: str = "review this PR") -> dict[str, Any]:
    hints = inside_policy.inspect({"messages": [{"role": "user", "content": text}]})
    return inside_policy.select(pack(), hints)


def test_overflow_raises_and_does_not_trim() -> None:
    selected = {
        "user": "x" * (inside_memory.USER_CAP + 1),
        "memory": "ok",
        "head_prefix": "x",
        "head_atoms": [],
        "tail_atoms": [],
    }
    body = {"messages": [{"role": "user", "content": "hi"}]}
    with pytest.raises(inside_memory.MemoryOverflow):
        inside_policy.splice(body, selected)
    selected = {
        "user": "ok",
        "memory": "y" * (inside_memory.MEMORY_CAP + 1),
        "head_prefix": "y",
        "head_atoms": [],
        "tail_atoms": [],
    }
    with pytest.raises(inside_memory.MemoryOverflow):
        inside_policy.splice(body, selected)


def test_chat_body_gets_system_then_user() -> None:
    selected = selected_for()
    raw = inside_policy.splice(
        {"messages": [{"role": "user", "content": "review this PR"}]},
        selected,
    )
    out = json.loads(raw)
    roles = [message["role"] for message in out["messages"]]
    assert roles[0] in ("system", "developer")
    assert roles[1:] == ["user"]
    assert out["messages"][0]["content"].startswith(selected["head_prefix"])
    assert "Reviews open with a reproducibility check." in out["messages"][0]["content"]
    assert "End seat memory." in out["messages"][0]["content"]


def test_anthropic_system_list_leaves_message_content_shapes() -> None:
    selected = selected_for()
    body = {
        "model": "v9",
        "system": [{"type": "text", "text": "You are Claude Code."}],
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "hi"}]},
            {
                "role": "assistant",
                "content": [{"type": "tool_use", "id": "t1", "name": "bash", "input": {}}],
            },
            {
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}],
            },
        ],
    }
    shapes = [type(m["content"]).__name__ for m in body["messages"]]
    out = json.loads(inside_policy.splice(body, selected))
    assert [type(m["content"]).__name__ for m in out["messages"]] == shapes
    assert out["messages"][2]["content"][0]["type"] == "tool_result"
    assert out["system"][0]["text"].startswith("Seat memory:")


def test_splice_twice_is_idempotent() -> None:
    selected = selected_for()
    body = {"messages": [{"role": "user", "content": "review this PR"}]}
    once = inside_policy.splice(body, selected)
    twice = inside_policy.splice(once, selected)
    first = json.loads(once)
    second = json.loads(twice)
    assert first["messages"] == second["messages"]
    heads = [
        message for message in second["messages"] if message.get("role") in ("system", "developer")
    ]
    assert len(heads) == 1


def test_input_list_round_trips() -> None:
    selected = selected_for()
    raw = inside_policy.splice(
        {"input": [{"type": "message", "role": "user", "content": "review this PR"}]},
        selected,
    )
    out = json.loads(raw)
    assert out["input"][0]["role"] == "system"
    assert out["input"][1]["role"] == "user"
    again = json.loads(inside_policy.splice(raw, selected))
    assert out["input"] == again["input"]
    assert sum(1 for item in again["input"] if item.get("role") == "system") == 1
