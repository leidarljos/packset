"""HTTP checks for packsetd. No live harness required."""

from __future__ import annotations

import json
import threading
from collections.abc import Iterator
from dataclasses import dataclass
from http.server import ThreadingHTTPServer
from pathlib import Path
from typing import Any
from urllib.error import HTTPError
from urllib.request import Request, urlopen

import pytest

import inside_extract
import inside_memory
import inside_policy
import packsetd

WS = "git:github.com/HaoZeke/joss-reviews"


@dataclass
class Packset:
    url: str
    store: packsetd.Store
    home: Path
    workspace: str

    def get(self, path: str) -> tuple[int, bytes]:
        with urlopen(self.url + path) as resp:
            return resp.status, resp.read()

    def json(self, method: str, path: str, payload: dict[str, Any]) -> tuple[int, dict[str, Any]]:
        req = Request(
            self.url + path,
            data=json.dumps(payload).encode(),
            method=method,
            headers={"Content-Type": "application/json"},
        )
        with urlopen(req) as resp:
            return resp.status, json.loads(resp.read().decode())


@pytest.fixture
def packset(tmp_path: Path) -> Iterator[Packset]:
    store = packsetd.Store(tmp_path)
    server = ThreadingHTTPServer(("127.0.0.1", 0), packsetd.make_handler(store))
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    port = server.server_address[1]
    try:
        yield Packset(
            url=f"http://127.0.0.1:{port}",
            store=store,
            home=tmp_path,
            workspace=WS,
        )
    finally:
        server.shutdown()
        server.server_close()
        store.close()


def voice(**fields: Any) -> dict[str, Any]:
    body: dict[str, Any] = {
        "workspace": WS,
        "text": "Reviews open with a reproducibility check.",
        "kind": "voice",
        "level": "explicit",
        "about_peer": "rgoswami",
        "by_peer": "hermes",
    }
    body.update(fields)
    return body


def test_health(packset: Packset) -> None:
    status, body = packset.get("/health")
    assert status == 200
    assert body == b"packsetd ok"


def test_old_health_path_returns_404(packset: Packset) -> None:
    with pytest.raises(HTTPError) as ctx:
        packset.get("/__inside_memd/health")
    assert ctx.value.code == 404


def test_get_atom_by_id(packset: Packset) -> None:
    _, posted = packset.json("POST", "/v1/atoms", voice())
    status, raw = packset.get(f"/v1/atoms/{posted['id']}?workspace={WS}")
    assert status == 200
    got = json.loads(raw.decode())
    assert got["id"] == posted["id"]
    assert got["text"] == posted["text"]
    with pytest.raises(HTTPError) as ctx:
        packset.get(f"/v1/atoms/missing-id?workspace={WS}")
    assert ctx.value.code == 404
    packset.store.delete(WS, posted["id"])
    with pytest.raises(HTTPError) as gone:
        packset.get(f"/v1/atoms/{posted['id']}?workspace={WS}")
    assert gone.value.code == 404


def test_workspaces_lists_global_and_live(packset: Packset) -> None:
    status, raw = packset.get("/v1/workspaces")
    assert status == 200
    empty = json.loads(raw.decode())["workspaces"]
    assert {"name": "global", "live": 0} in empty
    packset.json("POST", "/v1/atoms", voice())
    packset.json(
        "POST",
        "/v1/atoms",
        voice(workspace="global", text="A global lesson stays on the global store."),
    )
    _, listed = packset.get("/v1/workspaces")
    rows = {row["name"]: row["live"] for row in json.loads(listed.decode())["workspaces"]}
    assert rows[WS] == 1
    assert rows["global"] == 1


def test_get_empty_workspace_is_none(packset: Packset) -> None:
    packset.json("POST", "/v1/atoms", voice())
    assert packset.store.get("", "any") is None


def test_empty_workspace_status_counts_zero(packset: Packset) -> None:
    packset.json("POST", "/v1/atoms", voice(text="A live voice atom under a real workspace."))
    empty = packset.store.status("")
    assert empty["live"] == 0
    unscoped = packset.store.status(None)
    assert unscoped["live"] == 1


def test_scan_empty_workspace_is_empty(packset: Packset) -> None:
    packset.json("POST", "/v1/atoms", voice())
    assert packset.store._scan("") == []
    assert packset.store.get("", "any") is None


def test_status_empty_and_scoped(packset: Packset) -> None:
    status, raw = packset.get("/v1/status")
    assert status == 200
    body = json.loads(raw.decode())
    assert body["home"] == str(packset.home)
    assert body["live"] == 0
    assert body["tombstone"] == 0
    assert body["expired"] == 0
    assert "milli" in body
    assert "embedder" in body
    assert "binary" in body["milli"]
    assert "index_ready" in body["milli"]
    assert "available" in body["embedder"]

    _, created = packset.json(
        "POST", "/v1/atoms", voice(text="Status counts one live voice atom here.")
    )
    packset.json("POST", "/v1/atoms/delete", {"workspace": WS, "id": created["id"]})
    packset.json(
        "POST",
        "/v1/atoms",
        voice(
            text="A second live preference stays after delete.",
            kind="preference",
        ),
    )
    packset.json(
        "POST",
        "/v1/atoms",
        voice(
            text="An expired cache pointer no longer counts as live.",
            kind="cache-pointer",
            valid_to="2000-01-01T00:00:00+00:00",
        ),
    )
    other_ws = "git:github.com/HaoZeke/other"
    packset.json(
        "POST",
        "/v1/atoms",
        voice(
            workspace=other_ws,
            text="A live atom in a second workspace.",
            kind="preference",
        ),
    )

    status, raw = packset.get("/v1/status")
    assert status == 200
    body = json.loads(raw.decode())
    assert body["workspace"] == ""
    assert body["live"] == 2
    assert body["tombstone"] == 1
    assert body["expired"] == 1
    assert body["live_by_kind"].get("preference") == 2
    assert body["tombstone_by_kind"].get("voice") == 1
    assert body["expired_by_kind"].get("cache-pointer") == 1
    assert body["last_write_ts"]

    status, raw = packset.get(f"/v1/status?workspace={WS}")
    assert status == 200
    body = json.loads(raw.decode())
    assert body["workspace"] == WS
    assert body["live"] == 1
    assert body["tombstone"] == 1
    assert body["expired"] == 1
    assert body["live_by_kind"].get("preference") == 1
    assert body["tombstone_by_kind"].get("voice") == 1
    assert body["expired_by_kind"].get("cache-pointer") == 1
    assert body["last_write_ts"]


def test_two_clients_same_pack(packset: Packset) -> None:
    atom = voice()
    _, first = packset.json("POST", "/v1/atoms", atom)
    _, second = packset.json("POST", "/v1/atoms", atom)
    assert first["id"] == second["id"]
    status, raw = packset.get(f"/v1/pack?workspace={WS}")
    assert status == 200
    pack = json.loads(raw.decode())
    assert len(pack["atoms"]) == 1
    assert pack["atoms"][0]["text"] == atom["text"]


def test_user_and_memory_shared(packset: Packset) -> None:
    packset.json("PUT", "/v1/user", {"text": "No thanks.\n"})
    packset.json("PUT", "/v1/memory", {"workspace": WS, "text": "Read paper.pdf first.\n"})
    _, raw = packset.get(f"/v1/pack?workspace={WS}")
    pack = json.loads(raw.decode())
    assert "No thanks" in pack["user"]
    assert "paper.pdf" in pack["memory"]


def test_expired_atom_absent_from_pack(packset: Packset) -> None:
    _, stored = packset.json(
        "POST",
        "/v1/atoms",
        voice(
            text="Stale remote snapshot.",
            kind="cache-pointer",
            valid_to="2000-01-01T00:00:00.000Z",
        ),
    )
    status, raw = packset.get(f"/v1/pack?workspace={WS}")
    assert status == 200
    pack = json.loads(raw.decode())
    ids = [a["id"] for a in pack["atoms"]]
    assert stored["id"] not in ids
    assert pack["atoms"] == []


def test_open_and_missing_valid_to_in_pack(packset: Packset) -> None:
    _, open_atom = packset.json(
        "POST",
        "/v1/atoms",
        voice(text="Open validity claim.", valid_to=None),
    )
    _, future = packset.json(
        "POST",
        "/v1/atoms",
        voice(
            text="Still inside the validity window.",
            kind="habit",
            valid_to="2099-01-01T00:00:00.000Z",
        ),
    )
    _, bare = packset.json(
        "POST",
        "/v1/atoms",
        voice(text="No validity field at all.", kind="preference", by_peer="pi"),
    )
    _, raw = packset.get(f"/v1/pack?workspace={WS}")
    pack = json.loads(raw.decode())
    ids = {a["id"] for a in pack["atoms"]}
    assert ids == {open_atom["id"], future["id"], bare["id"]}


def test_cache_pointer_pack_only_while_open(packset: Packset) -> None:
    _, stale = packset.json(
        "POST",
        "/v1/atoms",
        voice(
            text="open issues @ 2000-01-01T00:00:00.000Z: 4 labeled needs-review",
            kind="cache-pointer",
            valid_to="2000-01-01T00:00:00.000Z",
        ),
    )
    _, open_ptr = packset.json(
        "POST",
        "/v1/atoms",
        voice(
            text="open issues @ 2026-08-11T12:00:00.000Z: 2 labeled needs-review",
            kind="cache-pointer",
            valid_to="2099-01-01T00:00:00.000Z",
        ),
    )
    _, raw = packset.get(f"/v1/pack?workspace={WS}")
    pack = json.loads(raw.decode())
    ids = [a["id"] for a in pack["atoms"]]
    assert stale["id"] not in ids
    assert open_ptr["id"] in ids
    assert pack["atoms"][0]["kind"] == "cache-pointer"


def test_add_links_entity_overlap(packset: Packset) -> None:
    _, first = packset.json(
        "POST",
        "/v1/atoms",
        voice(
            text="JOSS reviews open with a reproducibility check.",
            entities=["JOSS"],
        ),
    )
    _, second = packset.json(
        "POST",
        "/v1/atoms",
        voice(
            text="JOSS labels need a dated snapshot.",
            kind="habit",
            entities=["JOSS"],
        ),
    )
    _, third = packset.json(
        "POST",
        "/v1/atoms",
        voice(
            text="unrelated lowercase only.",
            kind="preference",
            by_peer="pi",
            entities=["Unrelated"],
        ),
    )
    _, raw = packset.get(f"/v1/atoms?workspace={WS}")
    live = {a["id"]: a for a in json.loads(raw.decode())["atoms"]}
    assert second["id"] in live[first["id"]]["links"]
    assert first["id"] in live[second["id"]]["links"]
    assert live[third["id"]]["links"] == []
    assert third["id"] not in live[first["id"]]["links"]


def test_recall_returns_live_atoms(packset: Packset) -> None:
    _, live = packset.json(
        "POST",
        "/v1/atoms",
        voice(
            text="Reviews open with a reproducibility check.",
            entities=["JOSS"],
        ),
    )
    _, neighbor = packset.json(
        "POST",
        "/v1/atoms",
        voice(
            text="JOSS labels need a dated snapshot.",
            kind="habit",
            entities=["JOSS"],
        ),
    )
    _, stale = packset.json(
        "POST",
        "/v1/atoms",
        voice(
            text="Stale remote snapshot.",
            kind="cache-pointer",
            valid_to="2000-01-01T00:00:00.000Z",
        ),
    )
    status, raw = packset.get(f"/v1/recall?workspace={WS}&limit=64&seed={live['id']}")
    assert status == 200
    payload = json.loads(raw.decode())
    assert "atoms" in payload
    ids = [atom["id"] for atom in payload["atoms"]]
    assert live["id"] in ids
    assert neighbor["id"] in ids
    assert stale["id"] not in ids


def test_search_ranks_live_atoms(packset: Packset) -> None:
    packset.json("PUT", "/v1/memory", {"workspace": WS, "text": "Read paper.pdf first.\n"})
    _, stored = packset.json(
        "POST",
        "/v1/atoms",
        voice(
            text="JOSS reviews open with a reproducibility check.",
            entities=["JOSS"],
        ),
    )
    packset.json(
        "POST",
        "/v1/atoms",
        voice(
            text="Stale JOSS snapshot.",
            kind="cache-pointer",
            valid_to="2000-01-01T00:00:00.000Z",
        ),
    )
    status, raw = packset.get(f"/v1/search?workspace={WS}&q=joss")
    assert status == 200
    payload = json.loads(raw.decode())
    assert payload.get("engine") in {
        "linear",
        "milli",
        "linear+dense",
        "milli+dense",
    }
    ids = [h.get("id") for h in payload["hits"] if h.get("field") == "atom"]
    assert stored["id"] in ids
    assert len(ids) == 1
    status, raw = packset.get(f"/v1/search?workspace={WS}&q=paper")
    payload = json.loads(raw.decode())
    assert [h["field"] for h in payload["hits"]] == ["memory"]
    client_hits = inside_policy.fetch_search(packset.url, WS, "joss")
    assert [h.get("id") for h in client_hits if h.get("field") == "atom"] == [stored["id"]]


def test_tombstones_still_disappear_from_pack(packset: Packset) -> None:
    _, stored = packset.json("POST", "/v1/atoms", voice())
    packset.json("POST", "/v1/atoms/delete", {"workspace": WS, "id": stored["id"]})
    _, raw = packset.get(f"/v1/pack?workspace={WS}")
    pack = json.loads(raw.decode())
    assert pack["atoms"] == []


def test_pin_switch_and_clear(packset: Packset) -> None:
    status, raw = packset.get(f"/v1/pin?workspace={WS}")
    assert status == 200
    assert json.loads(raw.decode())["set"] == ""
    _, body = packset.json("PUT", "/v1/pin", {"workspace": WS, "set": "review"})
    assert body["set"] == "review"
    _, raw = packset.get(f"/v1/pin?workspace={WS}")
    assert json.loads(raw.decode())["set"] == "review"
    _, body = packset.json("PUT", "/v1/pin", {"workspace": WS, "set": ""})
    assert body["set"] == ""


def test_set_pack_matches_workspace_pack_shape(packset: Packset) -> None:
    packset.json(
        "PUT",
        "/v1/set",
        {
            "workspace": WS,
            "set": "review",
            "user": "Open a review with the defect.\n",
            "memory": "Reviews cite the commit.\n",
        },
    )
    packset.json(
        "POST",
        "/v1/atoms",
        voice(
            text="Remember the zircon latch on reviews.",
            kind="lesson",
            about_peer="user",
            by_peer="user",
            set="review",
        ),
    )
    packset.json(
        "POST",
        "/v1/atoms",
        voice(
            text="Print the failing test name first.",
            kind="lesson",
            about_peer="user",
            by_peer="user",
            set="debug",
        ),
    )
    status, raw = packset.get(f"/v1/set?workspace={WS}&name=review")
    assert status == 200
    pack = json.loads(raw.decode())
    assert pack["set"] == "review"
    assert "Open a review" in pack["user"]
    assert "cite the commit" in pack["memory"]
    assert "voice" not in pack
    assert "lessons" not in pack
    kinds = [a["set"] for a in pack["atoms"]]
    assert kinds == ["review"]
    assert [a["text"] for a in pack["atoms"]] == ["Remember the zircon latch on reviews."]


def test_same_claim_under_two_sets_is_two_atoms(packset: Packset) -> None:
    claim = {
        "workspace": WS,
        "text": "Always pin the zircon index on reviews.",
        "kind": "lesson",
        "level": "explicit",
        "about_peer": "user",
        "by_peer": "user",
        "entities": ["zircon"],
    }
    _, review = packset.json("POST", "/v1/atoms", {**claim, "set": "review"})
    _, debug = packset.json("POST", "/v1/atoms", {**claim, "set": "debug"})
    assert review["id"] != debug["id"]
    _, again = packset.json("POST", "/v1/atoms", {**claim, "set": "review"})
    assert again["id"] == review["id"]

    status, raw = packset.get(f"/v1/search?workspace={WS}&q=zircon&set=review")
    assert status == 200
    hits = json.loads(raw.decode())["hits"]
    atom_ids = [h.get("id") for h in hits if h.get("field") == "atom"]
    assert atom_ids == [review["id"]]

    client_hits = inside_policy.fetch_search(packset.url, WS, "zircon", set_name="review")
    assert [h.get("id") for h in client_hits if h.get("field") == "atom"] == [review["id"]]

    _, neighbor = packset.json(
        "POST",
        "/v1/atoms",
        voice(
            text="Zircon latch belongs in the debug notebook.",
            kind="habit",
            about_peer="user",
            by_peer="user",
            entities=["zircon"],
            set="debug",
        ),
    )
    _, raw = packset.get(f"/v1/atoms?workspace={WS}")
    live = {a["id"]: a for a in json.loads(raw.decode())["atoms"]}
    assert neighbor["id"] not in (live[review["id"]].get("links") or [])
    assert review["id"] not in (live[neighbor["id"]].get("links") or [])
    assert debug["id"] in live[neighbor["id"]]["links"]
    assert neighbor["id"] in live[debug["id"]]["links"]


def test_remember_refused_write_returns_400(
    packset: Packset, monkeypatch: pytest.MonkeyPatch
) -> None:
    packset.json("PUT", "/v1/pin", {"workspace": WS, "set": "review"})
    quiet = inside_extract.extract_user_text(
        "What color is the sky?",
        url=packset.url,
        workspace=WS,
    )
    assert quiet is None

    def refuse(_atom: dict[str, Any]) -> dict[str, Any]:
        raise inside_memory.AtomError("store refused")

    monkeypatch.setattr(packset.store, "add", refuse)
    with pytest.raises(HTTPError) as ctx:
        inside_extract.extract_user_text(
            "Remember: always pin the zircon index.",
            url=packset.url,
            workspace=WS,
        )
    assert ctx.value.code == 400


@pytest.mark.parametrize(
    ("args", "match"),
    [
        (["--fuse", "not-a-voter"], "unknown fuse"),
        (["--fuse", "schulze"], "not implemented"),
    ],
    ids=["unknown", "unimplemented"],
)
def test_fuse_is_startup_error(tmp_path: Path, args: list[str], match: str) -> None:
    with pytest.raises(SystemExit, match=match):
        packsetd.main(["--home", str(tmp_path), *args])

