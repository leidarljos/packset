#!/usr/bin/env python3
"""HTTP checks for packsetd. No live harness required."""
import json
import tempfile
import threading
import unittest
import urllib.error
import urllib.request
from http.server import ThreadingHTTPServer
from pathlib import Path

import inside_memd
import inside_memory
import inside_policy


class MemdTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.store = inside_memd.Store(Path(self.tmp.name))
        self.server = ThreadingHTTPServer(
            ("127.0.0.1", 0), inside_memd.make_handler(self.store)
        )
        self.port = self.server.server_address[1]
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.base = f"http://127.0.0.1:{self.port}"
        self.ws = "git:github.com/HaoZeke/joss-reviews"

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.store.close()
        self.tmp.cleanup()

    def get(self, path):
        with urllib.request.urlopen(self.base + path) as resp:
            return resp.status, resp.read()

    def json_req(self, method, path, payload):
        data = json.dumps(payload).encode("utf-8")
        req = urllib.request.Request(
            self.base + path,
            data=data,
            method=method,
            headers={"Content-Type": "application/json"},
        )
        with urllib.request.urlopen(req) as resp:
            return resp.status, json.loads(resp.read().decode())

    def test_health(self):
        status, body = self.get("/health")
        self.assertEqual(status, 200)
        self.assertEqual(body, b"packsetd ok")

    def test_two_clients_same_pack(self):
        atom = {
            "workspace": self.ws,
            "text": "Reviews open with a reproducibility check.",
            "kind": "voice",
            "level": "explicit",
            "about_peer": "rgoswami",
            "by_peer": "hermes",
        }
        _, first = self.json_req("POST", "/v1/atoms", atom)
        _, second = self.json_req("POST", "/v1/atoms", atom)
        self.assertEqual(first["id"], second["id"])
        status, raw = self.get(f"/v1/pack?workspace={self.ws}")
        self.assertEqual(status, 200)
        pack = json.loads(raw.decode())
        self.assertEqual(len(pack["atoms"]), 1)
        self.assertEqual(pack["atoms"][0]["text"], atom["text"])

    def test_user_and_memory_shared(self):
        self.json_req("PUT", "/v1/user", {"text": "No thanks.\n"})
        self.json_req(
            "PUT",
            "/v1/memory",
            {"workspace": self.ws, "text": "Read paper.pdf first.\n"},
        )
        _, raw = self.get(f"/v1/pack?workspace={self.ws}")
        pack = json.loads(raw.decode())
        self.assertIn("No thanks", pack["user"])
        self.assertIn("paper.pdf", pack["memory"])

    def test_expired_atom_absent_from_pack(self):
        _, stored = self.json_req(
            "POST",
            "/v1/atoms",
            {
                "workspace": self.ws,
                "text": "Stale remote snapshot.",
                "kind": "cache-pointer",
                "level": "explicit",
                "about_peer": "rgoswami",
                "by_peer": "hermes",
                "valid_to": "2000-01-01T00:00:00.000Z",
            },
        )
        status, raw = self.get(f"/v1/pack?workspace={self.ws}")
        self.assertEqual(status, 200)
        pack = json.loads(raw.decode())
        ids = [a["id"] for a in pack["atoms"]]
        self.assertNotIn(stored["id"], ids)
        self.assertEqual(pack["atoms"], [])

    def test_open_and_missing_valid_to_in_pack(self):
        _, open_atom = self.json_req(
            "POST",
            "/v1/atoms",
            {
                "workspace": self.ws,
                "text": "Open validity claim.",
                "kind": "voice",
                "level": "explicit",
                "about_peer": "rgoswami",
                "by_peer": "hermes",
                "valid_to": None,
            },
        )
        _, future = self.json_req(
            "POST",
            "/v1/atoms",
            {
                "workspace": self.ws,
                "text": "Still inside the validity window.",
                "kind": "habit",
                "level": "explicit",
                "about_peer": "rgoswami",
                "by_peer": "hermes",
                "valid_to": "2099-01-01T00:00:00.000Z",
            },
        )
        _, bare = self.json_req(
            "POST",
            "/v1/atoms",
            {
                "workspace": self.ws,
                "text": "No validity field at all.",
                "kind": "preference",
                "level": "explicit",
                "about_peer": "rgoswami",
                "by_peer": "pi",
            },
        )
        _, raw = self.get(f"/v1/pack?workspace={self.ws}")
        pack = json.loads(raw.decode())
        ids = {a["id"] for a in pack["atoms"]}
        self.assertEqual(ids, {open_atom["id"], future["id"], bare["id"]})

    def test_cache_pointer_pack_only_while_open(self):
        _, stale = self.json_req(
            "POST",
            "/v1/atoms",
            {
                "workspace": self.ws,
                "text": "open issues @ 2000-01-01T00:00:00.000Z: 4 labeled needs-review",
                "kind": "cache-pointer",
                "level": "explicit",
                "about_peer": "rgoswami",
                "by_peer": "hermes",
                "valid_to": "2000-01-01T00:00:00.000Z",
            },
        )
        _, open_ptr = self.json_req(
            "POST",
            "/v1/atoms",
            {
                "workspace": self.ws,
                "text": "open issues @ 2026-08-11T12:00:00.000Z: 2 labeled needs-review",
                "kind": "cache-pointer",
                "level": "explicit",
                "about_peer": "rgoswami",
                "by_peer": "hermes",
                "valid_to": "2099-01-01T00:00:00.000Z",
            },
        )
        _, raw = self.get(f"/v1/pack?workspace={self.ws}")
        pack = json.loads(raw.decode())
        ids = [a["id"] for a in pack["atoms"]]
        self.assertNotIn(stale["id"], ids)
        self.assertIn(open_ptr["id"], ids)
        self.assertEqual(pack["atoms"][0]["kind"], "cache-pointer")

    def test_add_links_entity_overlap(self):
        _, first = self.json_req(
            "POST",
            "/v1/atoms",
            {
                "workspace": self.ws,
                "text": "JOSS reviews open with a reproducibility check.",
                "kind": "voice",
                "level": "explicit",
                "about_peer": "rgoswami",
                "by_peer": "hermes",
                "entities": ["JOSS"],
            },
        )
        _, second = self.json_req(
            "POST",
            "/v1/atoms",
            {
                "workspace": self.ws,
                "text": "JOSS labels need a dated snapshot.",
                "kind": "habit",
                "level": "explicit",
                "about_peer": "rgoswami",
                "by_peer": "hermes",
                "entities": ["JOSS"],
            },
        )
        _, third = self.json_req(
            "POST",
            "/v1/atoms",
            {
                "workspace": self.ws,
                "text": "unrelated lowercase only.",
                "kind": "preference",
                "level": "explicit",
                "about_peer": "rgoswami",
                "by_peer": "pi",
                "entities": ["Unrelated"],
            },
        )
        _, raw = self.get(f"/v1/atoms?workspace={self.ws}")
        payload = json.loads(raw.decode())
        live = {a["id"]: a for a in payload["atoms"]}
        self.assertIn(second["id"], live[first["id"]]["links"])
        self.assertIn(first["id"], live[second["id"]]["links"])
        self.assertEqual(live[third["id"]]["links"], [])
        self.assertNotIn(third["id"], live[first["id"]]["links"])

    def test_recall_returns_live_atoms(self):
        _, live = self.json_req(
            "POST",
            "/v1/atoms",
            {
                "workspace": self.ws,
                "text": "Reviews open with a reproducibility check.",
                "kind": "voice",
                "level": "explicit",
                "about_peer": "rgoswami",
                "by_peer": "hermes",
                "entities": ["JOSS"],
            },
        )
        _, neighbor = self.json_req(
            "POST",
            "/v1/atoms",
            {
                "workspace": self.ws,
                "text": "JOSS labels need a dated snapshot.",
                "kind": "habit",
                "level": "explicit",
                "about_peer": "rgoswami",
                "by_peer": "hermes",
                "entities": ["JOSS"],
            },
        )
        _, stale = self.json_req(
            "POST",
            "/v1/atoms",
            {
                "workspace": self.ws,
                "text": "Stale remote snapshot.",
                "kind": "cache-pointer",
                "level": "explicit",
                "about_peer": "rgoswami",
                "by_peer": "hermes",
                "valid_to": "2000-01-01T00:00:00.000Z",
            },
        )
        status, raw = self.get(
            f"/v1/recall?workspace={self.ws}&limit=64&seed={live['id']}"
        )
        self.assertEqual(status, 200)
        payload = json.loads(raw.decode())
        self.assertIn("atoms", payload)
        ids = [atom["id"] for atom in payload["atoms"]]
        self.assertIn(live["id"], ids)
        self.assertIn(neighbor["id"], ids)
        self.assertNotIn(stale["id"], ids)

    def test_search_ranks_live_atoms(self):
        self.json_req(
            "PUT",
            "/v1/memory",
            {"workspace": self.ws, "text": "Read paper.pdf first.\n"},
        )
        _, stored = self.json_req(
            "POST",
            "/v1/atoms",
            {
                "workspace": self.ws,
                "text": "JOSS reviews open with a reproducibility check.",
                "kind": "voice",
                "level": "explicit",
                "about_peer": "rgoswami",
                "by_peer": "hermes",
                "entities": ["JOSS"],
            },
        )
        self.json_req(
            "POST",
            "/v1/atoms",
            {
                "workspace": self.ws,
                "text": "Stale JOSS snapshot.",
                "kind": "cache-pointer",
                "level": "explicit",
                "about_peer": "rgoswami",
                "by_peer": "hermes",
                "valid_to": "2000-01-01T00:00:00.000Z",
            },
        )
        status, raw = self.get(f"/v1/search?workspace={self.ws}&q=joss")
        self.assertEqual(status, 200)
        payload = json.loads(raw.decode())
        self.assertIn(
            payload.get("engine"),
            {"linear", "milli", "linear+dense", "milli+dense"},
        )
        ids = [h.get("id") for h in payload["hits"] if h.get("field") == "atom"]
        self.assertIn(stored["id"], ids)
        self.assertEqual(len(ids), 1)
        status, raw = self.get(f"/v1/search?workspace={self.ws}&q=paper")
        payload = json.loads(raw.decode())
        self.assertTrue(any(h["field"] == "memory" for h in payload["hits"]))
        client_hits = inside_policy.fetch_search(self.base, self.ws, "joss")
        self.assertIn(stored["id"], [h.get("id") for h in client_hits])

    def test_tombstones_still_disappear_from_pack(self):
        _, stored = self.json_req(
            "POST",
            "/v1/atoms",
            {
                "workspace": self.ws,
                "text": "Reviews open with a reproducibility check.",
                "kind": "voice",
                "level": "explicit",
                "about_peer": "rgoswami",
                "by_peer": "hermes",
            },
        )
        self.json_req(
            "POST",
            "/v1/atoms/delete",
            {"workspace": self.ws, "id": stored["id"]},
        )
        _, raw = self.get(f"/v1/pack?workspace={self.ws}")
        pack = json.loads(raw.decode())
        self.assertEqual(pack["atoms"], [])

    def test_pin_switch_and_clear(self):
        status, raw = self.get(f"/v1/pin?workspace={self.ws}")
        self.assertEqual(status, 200)
        self.assertEqual(json.loads(raw.decode())["set"], "")
        _, body = self.json_req(
            "PUT", "/v1/pin", {"workspace": self.ws, "set": "review"}
        )
        self.assertEqual(body["set"], "review")
        _, raw = self.get(f"/v1/pin?workspace={self.ws}")
        self.assertEqual(json.loads(raw.decode())["set"], "review")
        _, body = self.json_req("PUT", "/v1/pin", {"workspace": self.ws, "set": ""})
        self.assertEqual(body["set"], "")

    def test_set_pack_matches_workspace_pack_shape(self):
        self.json_req(
            "PUT",
            "/v1/set",
            {
                "workspace": self.ws,
                "set": "review",
                "user": "Open a review with the defect.\n",
                "memory": "Reviews cite the commit.\n",
            },
        )
        self.json_req(
            "POST",
            "/v1/atoms",
            {
                "workspace": self.ws,
                "text": "Remember the zircon latch on reviews.",
                "kind": "lesson",
                "level": "explicit",
                "about_peer": "user",
                "by_peer": "user",
                "set": "review",
            },
        )
        self.json_req(
            "POST",
            "/v1/atoms",
            {
                "workspace": self.ws,
                "text": "Print the failing test name first.",
                "kind": "lesson",
                "level": "explicit",
                "about_peer": "user",
                "by_peer": "user",
                "set": "debug",
            },
        )
        status, raw = self.get(f"/v1/set?workspace={self.ws}&name=review")
        self.assertEqual(status, 200)
        pack = json.loads(raw.decode())
        self.assertEqual(pack["set"], "review")
        self.assertIn("Open a review", pack["user"])
        self.assertIn("cite the commit", pack["memory"])
        self.assertNotIn("voice", pack)
        self.assertNotIn("lessons", pack)
        kinds = [a["set"] for a in pack["atoms"]]
        self.assertEqual(kinds, ["review"])
        self.assertTrue(all("zircon" in a["text"] for a in pack["atoms"]))

    def test_same_claim_under_two_sets_is_two_atoms(self):
        claim = {
            "workspace": self.ws,
            "text": "Always pin the zircon index on reviews.",
            "kind": "lesson",
            "level": "explicit",
            "about_peer": "user",
            "by_peer": "user",
            "entities": ["zircon"],
        }
        _, review = self.json_req("POST", "/v1/atoms", {**claim, "set": "review"})
        _, debug = self.json_req("POST", "/v1/atoms", {**claim, "set": "debug"})
        self.assertNotEqual(review["id"], debug["id"])
        _, again = self.json_req("POST", "/v1/atoms", {**claim, "set": "review"})
        self.assertEqual(again["id"], review["id"])

        status, raw = self.get(f"/v1/search?workspace={self.ws}&q=zircon&set=review")
        self.assertEqual(status, 200)
        hits = json.loads(raw.decode())["hits"]
        atom_ids = [h.get("id") for h in hits if h.get("field") == "atom"]
        self.assertIn(review["id"], atom_ids)
        self.assertNotIn(debug["id"], atom_ids)

        client_hits = inside_policy.fetch_search(
            self.base, self.ws, "zircon", set_name="review"
        )
        self.assertIn(review["id"], [h.get("id") for h in client_hits])
        self.assertNotIn(debug["id"], [h.get("id") for h in client_hits])

        _, neighbor = self.json_req(
            "POST",
            "/v1/atoms",
            {
                "workspace": self.ws,
                "text": "Zircon latch belongs in the debug notebook.",
                "kind": "habit",
                "level": "explicit",
                "about_peer": "user",
                "by_peer": "user",
                "entities": ["zircon"],
                "set": "debug",
            },
        )
        _, raw = self.get(f"/v1/atoms?workspace={self.ws}")
        live = {a["id"]: a for a in json.loads(raw.decode())["atoms"]}
        self.assertNotIn(neighbor["id"], live[review["id"]].get("links") or [])
        self.assertNotIn(review["id"], live[neighbor["id"]].get("links") or [])
        self.assertIn(debug["id"], live[neighbor["id"]]["links"])
        self.assertIn(neighbor["id"], live[debug["id"]]["links"])

    def test_remember_refused_write_is_not_no_remember(self):
        import inside_extract

        self.json_req("PUT", "/v1/pin", {"workspace": self.ws, "set": "review"})
        quiet = inside_extract.extract_user_text(
            "What color is the sky?",
            url=self.base,
            workspace=self.ws,
        )
        self.assertIsNone(quiet)

        def refuse(_atom):
            raise inside_memory.AtomError("store refused")

        self.store.add = refuse
        with self.assertRaises(urllib.error.HTTPError) as ctx:
            inside_extract.extract_user_text(
                "Remember: always pin the zircon index.",
                url=self.base,
                workspace=self.ws,
            )
        self.assertEqual(ctx.exception.code, 400)

    def test_get_atom_by_id(self):
        _, posted = self.json_req(
            "POST",
            "/v1/atoms",
            {
                "workspace": self.ws,
                "text": "Reviews open with a reproducibility check.",
                "kind": "voice",
                "level": "explicit",
                "about_peer": "rgoswami",
                "by_peer": "hermes",
            },
        )
        status, raw = self.get(f"/v1/atoms/{posted['id']}?workspace={self.ws}")
        self.assertEqual(status, 200)
        got = json.loads(raw.decode())
        self.assertEqual(got["id"], posted["id"])
        self.assertEqual(got["text"], posted["text"])
        with self.assertRaises(urllib.error.HTTPError) as ctx:
            self.get(f"/v1/atoms/missing-id?workspace={self.ws}")
        self.assertEqual(ctx.exception.code, 404)
        self.store.delete(self.ws, posted["id"])
        with self.assertRaises(urllib.error.HTTPError) as gone:
            self.get(f"/v1/atoms/{posted['id']}?workspace={self.ws}")
        self.assertEqual(gone.exception.code, 404)

    def test_scan_empty_workspace_is_empty(self):
        self.json_req(
            "POST",
            "/v1/atoms",
            {
                "workspace": self.ws,
                "text": "Reviews open with a reproducibility check.",
                "kind": "voice",
                "level": "explicit",
                "about_peer": "rgoswami",
                "by_peer": "hermes",
            },
        )
        self.assertEqual(self.store._scan(""), [])
        self.assertEqual(self.store.get("", "any"), None)


if __name__ == "__main__":
    unittest.main()
