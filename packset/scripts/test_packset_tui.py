"""GET /v1/atoms, table columns, and POST /v1/atoms/update bump."""

from __future__ import annotations

import json
import os
import sys
import tempfile
import threading
import time
import unittest
from contextlib import redirect_stdout
from http.server import ThreadingHTTPServer
from io import StringIO
from pathlib import Path

import packsetd

ROOT = Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

from packset_tui.atoms import (  # noqa: E402
    BASE_COLUMNS,
    bump_ts,
    dump_table,
    fetch_atoms,
    list_workspaces,
    packset_url,
    resolve_workspace,
    table_columns,
    table_row,
    workspace_from_identity,
    workspace_name,
)
from packset_tui.cli import main  # noqa: E402

STORM = ROOT / "packset_tui" / "tokyo_night_storm.tcss"
WIDGET_CSS = ROOT / "packset_tui" / "storm.tcss"


class PacksetTuiTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.store = packsetd.Store(Path(self.tmp.name))
        self.server = ThreadingHTTPServer(
            ("127.0.0.1", 0), packsetd.make_handler(self.store)
        )
        self.port = self.server.server_address[1]
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.base = f"http://127.0.0.1:{self.port}"
        self.ws = "git:github.com/HaoZeke/packset"

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.store.close()
        self.tmp.cleanup()

    def post_atom(self, text="Prefer opening review links after a push.", **extra):
        atom = {
            "workspace": self.ws,
            "text": text,
            "kind": "preference",
            "level": "explicit",
            "about_peer": "rgoswami",
            "by_peer": "hermes",
        }
        atom.update(extra)
        return self.store.add(atom)

    def test_packset_url_default(self):
        old = os.environ.pop("PACKSET_URL", None)
        alias = os.environ.pop("INSIDE_MEMORY_URL", None)
        try:
            self.assertEqual(packset_url(), "http://127.0.0.1:8761")
        finally:
            if old is not None:
                os.environ["PACKSET_URL"] = old
            if alias is not None:
                os.environ["INSIDE_MEMORY_URL"] = alias

    def test_packset_url_env(self):
        old = os.environ.get("PACKSET_URL")
        os.environ["PACKSET_URL"] = "http://127.0.0.1:9999/"
        try:
            self.assertEqual(packset_url(), "http://127.0.0.1:9999")
        finally:
            if old is None:
                os.environ.pop("PACKSET_URL", None)
            else:
                os.environ["PACKSET_URL"] = old

    def test_fetch_atoms_get(self):
        posted = self.post_atom()
        atoms, err = fetch_atoms(self.base, self.ws)
        self.assertEqual(err, "")
        self.assertEqual(len(atoms), 1)
        self.assertEqual(atoms[0]["id"], posted["id"])
        self.assertEqual(atoms[0]["kind"], "preference")
        self.assertEqual(atoms[0]["text"], posted["text"])
        self.assertTrue(atoms[0]["ts"])

    def test_table_columns_omit_urgency_without_field(self):
        posted = self.post_atom()
        atoms, err = fetch_atoms(self.base, self.ws)
        self.assertEqual(err, "")
        self.assertEqual(table_columns(atoms), BASE_COLUMNS)
        row = table_row(atoms[0], table_columns(atoms))
        self.assertEqual(row[0], posted["id"])
        self.assertEqual(row[1], "preference")
        self.assertEqual(len(row), 4)

    def test_table_columns_include_urgency_when_present(self):
        cols = table_columns([{"id": "a", "kind": "voice", "text": "x", "ts": "1"}])
        self.assertEqual(cols, ("id", "kind", "text", "ts"))
        cols = table_columns(
            [{"id": "a", "kind": "voice", "text": "x", "ts": "1", "urgency": 3}]
        )
        self.assertEqual(cols, ("id", "kind", "text", "ts", "urgency"))
        row = table_row(
            {"id": "a", "kind": "voice", "text": "x", "ts": "1", "urgency": 3},
            cols,
        )
        self.assertEqual(row[-1], "3")

    def test_dump_table_tsv(self):
        atoms = [
            {"id": "abc", "kind": "voice", "text": "hello", "ts": "t0"},
            {
                "id": "def",
                "kind": "preference",
                "text": "world",
                "ts": "t1",
                "urgency": 2,
            },
        ]
        text = dump_table(atoms)
        self.assertEqual(
            text,
            "id\tkind\ttext\tts\turgency\n"
            "abc\tvoice\thello\tt0\t\n"
            "def\tpreference\tworld\tt1\t2\n",
        )

    def test_bump_ts_empty_fields_updates_ts(self):
        posted = self.post_atom()
        first_ts = posted["ts"]
        time.sleep(0.01)
        updated, err = bump_ts(self.base, self.ws, posted["id"])
        self.assertEqual(err, "")
        self.assertIsNotNone(updated)
        assert updated is not None
        self.assertEqual(updated["id"], posted["id"])
        self.assertNotEqual(updated["ts"], first_ts)
        atoms, ferr = fetch_atoms(self.base, self.ws)
        self.assertEqual(ferr, "")
        self.assertEqual(atoms[0]["ts"], updated["ts"])

    def test_dump_cli_writes_get_table(self):
        posted = self.post_atom(text="Pin the review checklist after push.")
        buf = StringIO()
        with redirect_stdout(buf):
            rc = main(["--url", self.base, "--workspace", self.ws, "--dump"])
        self.assertEqual(rc, 0)
        out = buf.getvalue()
        self.assertTrue(out.startswith("id\tkind\ttext\tts\n"))
        self.assertIn(posted["id"], out)
        self.assertIn("preference", out)
        self.assertIn("Pin the review checklist after push.", out)
        self.assertIn(posted["ts"], out)
        self.assertNotIn("urgency", out.split("\n", 1)[0])

    def test_bump_cli_posts_update(self):
        posted = self.post_atom(text="Remember the loopback host is 127.0.0.1.")
        first_ts = posted["ts"]
        time.sleep(0.01)
        buf = StringIO()
        with redirect_stdout(buf):
            rc = main(
                ["--url", self.base, "--workspace", self.ws, "--bump", posted["id"]]
            )
        self.assertEqual(rc, 0)
        body = json.loads(buf.getvalue())
        self.assertEqual(body["id"], posted["id"])
        self.assertNotEqual(body["ts"], first_ts)

    def test_workspace_identity(self):
        ws = workspace_from_identity(self.base, Path(self.tmp.name))
        self.assertTrue(ws.startswith(("dir:", "git:")))

    def test_resolve_workspace_picks_global_when_identity_empty(self):
        self.store.add(
            {
                "workspace": "global",
                "text": "Always put work on the graph before starting.",
                "kind": "lesson",
                "level": "explicit",
                "about_peer": "rgoswami",
                "by_peer": "hermes",
            }
        )
        old = os.environ.get("PACKSET_WORKSPACE")
        os.environ["PACKSET_WORKSPACE"] = "dir:/empty-identity"
        try:
            self.assertEqual(resolve_workspace(self.base), "global")
            self.assertEqual(resolve_workspace(self.base, "dir:/empty-identity"), "dir:/empty-identity")
        finally:
            if old is None:
                os.environ.pop("PACKSET_WORKSPACE", None)
            else:
                os.environ["PACKSET_WORKSPACE"] = old

    def test_list_workspaces_includes_global_and_posted(self):
        self.post_atom()
        rows = {row["name"]: row["live"] for row in list_workspaces(self.base)}
        self.assertIn("global", rows)
        self.assertEqual(rows[self.ws], 1)

    def test_app_cycle_workspace_loads_global_atoms(self):
        try:
            import asyncio

            from packset_tui.app import PacksetApp
        except ImportError:
            self.skipTest("textual not installed")
        self.store.add(
            {
                "workspace": "global",
                "text": "Always put work on the graph before starting.",
                "kind": "lesson",
                "level": "explicit",
                "about_peer": "rgoswami",
                "by_peer": "hermes",
            }
        )
        app = PacksetApp(base=self.base, workspace=self.ws)

        async def run() -> tuple[str, int]:
            async with app.run_test() as pilot:
                await pilot.press("]")
                return app.workspace, len(app._ids)

        workspace, n = asyncio.run(run())
        self.assertEqual(workspace, "global")
        self.assertEqual(n, 1)

    def test_workspace_env(self):
        old = os.environ.get("PACKSET_WORKSPACE")
        os.environ["PACKSET_WORKSPACE"] = "seat-test"
        try:
            self.assertEqual(workspace_name(self.base), "seat-test")
        finally:
            if old is None:
                os.environ.pop("PACKSET_WORKSPACE", None)
            else:
                os.environ["PACKSET_WORKSPACE"] = old

    def test_storm_tokens(self):
        shared = STORM.read_text(encoding="utf-8")
        self.assertIn("#24283b", shared)
        self.assertIn("#c0caf5", shared)
        self.assertIn("#7aa2f7", shared)
        self.assertIn("layout: vertical", shared)
        widgets = WIDGET_CSS.read_text(encoding="utf-8")
        self.assertIn("DataTable", widgets)
        self.assertIn("height: 1fr", widgets)

    def test_app_datatable_and_bump_binding(self):
        try:
            import asyncio

            from packset_tui.app import PacksetApp
            from textual.widgets import DataTable
        except ImportError:
            self.skipTest("textual not installed")
        posted = self.post_atom(text="Keep the writer on 127.0.0.1 only.")
        first_ts = posted["ts"]
        time.sleep(0.01)
        app = PacksetApp(base=self.base, workspace=self.ws)

        async def run() -> tuple[list[str], str]:
            async with app.run_test() as pilot:
                table = app.query_one(DataTable)
                labels = [str(col.label) for col in table.columns.values()]
                await pilot.press("b")
                return labels, app._ids[0] if app._ids else ""

        labels, aid = asyncio.run(run())
        self.assertEqual(labels, ["id", "kind", "text", "ts"])
        self.assertEqual(aid, posted["id"])
        atoms, err = fetch_atoms(self.base, self.ws)
        self.assertEqual(err, "")
        self.assertNotEqual(atoms[0]["ts"], first_ts)


if __name__ == "__main__":
    unittest.main()
