#!/usr/bin/env python3
"""packsetd: loopback pack writer. One process, every client.

Listen on 127.0.0.1. USER.md and MEMORY.md stay files. Atoms live in
LMDB (heed / redb family: mmap B+tree, not SQL). Isolated harness
homes do not get a private store; they talk to this URL.
"""
from __future__ import annotations

import argparse
import json
import os
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlparse

import lmdb

import inside_context
import inside_embed
import inside_extract
import inside_identity
import inside_memory
import inside_prose
import inside_recall
import inside_search
import inside_set

DEFAULT_PORT = 8761
MAP_SIZE = 256 * 1024 * 1024


def _atom_key(workspace: str, atom_id: str) -> bytes:
    return f"{workspace}\0{atom_id}".encode()


def _ws_prefix(workspace: str) -> bytes:
    return f"{workspace}\0".encode()


class Store:
    def __init__(self, home: Path):
        self.home = Path(home)
        self.home.mkdir(parents=True, exist_ok=True)
        self.db_path = self.home / "memory.lmdb"
        self.milli_dir = self.home / "memory.milli"
        self.lock = threading.RLock()
        self.env = lmdb.open(
            str(self.db_path),
            map_size=MAP_SIZE,
            subdir=True,
            max_dbs=1,
            writemap=True,
        )
        self._attach: dict[str, dict] = {}

    def close(self) -> None:
        self.env.close()

    def _scan(self, workspace: str | None = None) -> list[dict]:
        if workspace is not None and not workspace:
            return []
        prefix = _ws_prefix(workspace) if workspace is not None else b""
        out: list[dict] = []
        with self.env.begin() as txn:
            cur = txn.cursor()
            if prefix:
                if not cur.set_range(prefix):
                    return out
            elif not cur.first():
                return out
            for key, raw in cur:
                if prefix and not key.startswith(prefix):
                    break
                rec = json.loads(raw)
                if isinstance(rec, dict):
                    out.append(rec)
        return out

    def get(self, workspace: str, atom_id: str) -> dict | None:
        if not workspace or not atom_id:
            return None
        key = _atom_key(workspace, atom_id)
        with self.lock:
            with self.env.begin() as txn:
                raw = txn.get(key)
        if raw is None:
            return None
        rec = json.loads(raw)
        if not isinstance(rec, dict):
            return None
        return rec

    def current(self, workspace: str, set_name: str | None = None) -> list[dict]:
        with self.lock:
            now = inside_memory.utcnow()
            live = [
                atom
                for atom in self._scan(workspace)
                if inside_memory.is_live(atom, now)
            ]
            live = inside_memory.filter_live_links(live)
            if set_name:
                named = inside_set.check_set_name(set_name)
                live = [atom for atom in live if atom.get("set") == named]
            return live

    def add(self, atom: dict) -> dict:
        text = atom.get("text") if isinstance(atom.get("text"), str) else ""
        if inside_extract.is_tool_dump(text):
            raise inside_memory.AtomError("tool dump is attach, not an atom")
        atom = inside_memory.validate_atom(dict(atom))
        if not atom.get("id"):
            atom["id"] = inside_memory.new_id()
        if not atom.get("ts"):
            atom["ts"] = inside_memory.utcnow()
        atom["tombstone"] = False
        if atom.get("embedding") is None:
            atom["embedding"] = inside_embed.encode_one(atom["text"], home=self.home)
        with self.lock:
            named = atom.get("set")
            if named:
                live = self.current(atom["workspace"], set_name=named)
            else:
                live = [
                    peer
                    for peer in self.current(atom["workspace"])
                    if not peer.get("set")
                ]
            for existing in live:
                if (
                    existing.get("text") == atom["text"]
                    and existing.get("kind") == atom["kind"]
                    and existing.get("set") == atom.get("set")
                ):
                    return existing
            rewritten = []
            if inside_memory.is_live(atom):
                rewritten = inside_memory.apply_links(atom, live)
            elif "links" not in atom:
                atom["links"] = []
            self._upsert(atom)
            for peer in rewritten:
                peer["ts"] = inside_memory.utcnow()
                self._upsert(peer)
            self._milli_upsert([atom, *rewritten])
        return atom

    def update(self, workspace: str, atom_id: str, fields: dict) -> dict:
        with self.lock:
            current = {a["id"]: a for a in self.current(workspace)}
            if atom_id not in current:
                raise inside_memory.AtomError(f"no current atom {atom_id}")
            updated = dict(current[atom_id])
            updated.update(fields)
            updated["id"] = atom_id
            updated["workspace"] = workspace
            updated["ts"] = inside_memory.utcnow()
            updated["tombstone"] = False
            inside_memory.validate_atom(updated)
            if "text" in fields:
                updated["embedding"] = inside_embed.encode_one(
                    updated["text"], home=self.home
                )
            rewritten = []
            if inside_memory.is_live(updated):
                rewritten = inside_memory.apply_links(updated, list(current.values()))
            elif "links" not in updated:
                updated["links"] = []
            self._upsert(updated)
            for peer in rewritten:
                peer["ts"] = inside_memory.utcnow()
                self._upsert(peer)
            self._milli_upsert([updated, *rewritten])
        return updated

    def delete(self, workspace: str, atom_id: str) -> dict:
        with self.lock:
            current = {a["id"]: a for a in self.current(workspace)}
            if atom_id not in current:
                raise inside_memory.AtomError(f"no current atom {atom_id}")
            tomb = dict(current[atom_id])
            tomb["tombstone"] = True
            tomb["ts"] = inside_memory.utcnow()
            self._upsert(tomb)
            inside_search.delete_documents([atom_id], self.milli_dir)
        return tomb

    def _upsert(self, atom: dict) -> None:
        payload = dict(atom)
        if "links" not in payload or payload["links"] is None:
            payload["links"] = []
        key = _atom_key(payload["workspace"], payload["id"])
        blob = json.dumps(payload, ensure_ascii=True).encode()
        with self.env.begin(write=True) as txn:
            txn.put(key, blob)

    def _milli_files(self, workspace: str | None = None) -> None:
        docs = []
        user = inside_memory.read_text(inside_memory.user_path(self.home))
        if user:
            docs.append(
                {
                    "id": inside_search.document_id("user", workspace=workspace or ""),
                    "field": "user",
                    "kind": "user",
                    "text": user,
                    "entities": "",
                    "workspace": workspace or "",
                    "trust": 1.5,
                }
            )
        if workspace:
            memory = inside_memory.read_text(
                inside_memory.memory_path(workspace, self.home)
            )
            if memory:
                docs.append(
                    {
                        "id": inside_search.document_id(
                            "memory", workspace=workspace
                        ),
                        "field": "memory",
                        "kind": "memory",
                        "text": memory,
                        "entities": "",
                        "workspace": workspace,
                        "trust": 1.25,
                    }
                )
        if docs:
            inside_search.upsert_documents(docs, self.milli_dir)

    def _milli_upsert(self, atoms: list[dict]) -> None:
        now = inside_memory.utcnow()
        live = [
            inside_search.atom_document(atom)
            for atom in atoms
            if inside_memory.is_live(atom, now) and atom.get("id")
        ]
        dead = [
            str(atom.get("id"))
            for atom in atoms
            if atom.get("id") and not inside_memory.is_live(atom, now)
        ]
        if live:
            inside_search.upsert_documents(live, self.milli_dir)
        if dead:
            inside_search.delete_documents(dead, self.milli_dir)

    def pack(self, workspace: str, set_name: str | None = None) -> dict:
        """Workspace pack, or the same shape scoped to one set.

        Always ``user`` / ``memory`` / ``atoms`` so search and splice stay one
        path. When ``set_name`` is set, include ``set`` and load that set's
        prose and atoms only.
        """
        if set_name:
            cards = inside_set.read_set(workspace, set_name, home=self.home)
            return {
                "workspace": workspace,
                "set": cards["set"],
                "user": cards["user"],
                "memory": cards["memory"],
                "instructions": cards.get("instructions") or "",
                "atoms": self.current(workspace, set_name=cards["set"]),
            }
        return {
            "workspace": workspace,
            "user": inside_memory.read_text(inside_memory.user_path(self.home)),
            "memory": inside_memory.read_text(
                inside_memory.memory_path(workspace, self.home)
            ),
            "atoms": self.current(workspace),
        }

    def pin(self, workspace: str) -> str:
        return inside_set.read_pin(workspace, home=self.home)

    def set_pin(self, workspace: str, name: str) -> str:
        return inside_set.write_pin(workspace, name, home=self.home)

    def pin_payload(self, workspace: str) -> dict:
        named = self.pin(workspace)
        instructions = ""
        if named:
            cards = inside_set.read_set(workspace, named, home=self.home)
            instructions = cards.get("instructions") or ""
        return {"workspace": workspace, "set": named, "instructions": instructions}

    def workspaces(self) -> list[dict]:
        """Distinct workspace ids with live counts. Always includes global."""
        now = inside_memory.utcnow()
        counts: dict[str, int] = {}
        with self.lock:
            for rec in self._scan(None):
                name = rec.get("workspace")
                if not isinstance(name, str) or not name:
                    continue
                if inside_memory.is_live(rec, now):
                    counts[name] = counts.get(name, 0) + 1
                else:
                    counts.setdefault(name, 0)
        counts.setdefault("global", 0)
        return [{"name": name, "live": counts[name]} for name in sorted(counts)]

    def status(self, workspace: str | None = None) -> dict:
        """Seat home, atom counts, pin, milli, and embedder. Optional workspace scope."""
        live_by_kind: dict[str, int] = {}
        tombstone_by_kind: dict[str, int] = {}
        expired_by_kind: dict[str, int] = {}
        last_write_ts = ""
        now = inside_memory.utcnow()
        with self.lock:
            for rec in self._scan(workspace):
                kind = str(rec.get("kind") or "unknown")
                ts = str(rec.get("ts") or "")
                if ts and ts > last_write_ts:
                    last_write_ts = ts
                if rec.get("tombstone"):
                    tombstone_by_kind[kind] = tombstone_by_kind.get(kind, 0) + 1
                elif inside_memory.is_live(rec, now):
                    live_by_kind[kind] = live_by_kind.get(kind, 0) + 1
                else:
                    expired_by_kind[kind] = expired_by_kind.get(kind, 0) + 1
            pin_name = self.pin(workspace) if workspace else ""
        milli_path = inside_search.milli_bin()
        index_ready = (self.milli_dir / "data.mdb").is_file()
        return {
            "home": str(self.home),
            "workspace": workspace or "",
            "set": pin_name,
            "live": sum(live_by_kind.values()),
            "tombstone": sum(tombstone_by_kind.values()),
            "expired": sum(expired_by_kind.values()),
            "live_by_kind": dict(sorted(live_by_kind.items())),
            "tombstone_by_kind": dict(sorted(tombstone_by_kind.items())),
            "expired_by_kind": dict(sorted(expired_by_kind.items())),
            "last_write_ts": last_write_ts or None,
            "milli": {
                "binary": str(milli_path) if milli_path else None,
                "index_dir": str(self.milli_dir),
                "index_ready": index_ready,
            },
            "embedder": {
                "enabled": inside_embed.enabled(),
                "available": inside_embed.available(home=self.home),
            },
        }

    def put_attach(self, workspace: str, text: str, label: str = "") -> dict:
        body = text if isinstance(text, str) else str(text or "")
        if len(body) > inside_context.ATTACH_CAP:
            body = body[: inside_context.ATTACH_CAP]
        slot = {"text": body, "label": (label or "").strip()}
        with self.lock:
            self._attach[workspace] = slot
        return {"workspace": workspace, **slot}

    def take_attach(self, workspace: str) -> dict | None:
        with self.lock:
            return self._attach.pop(workspace, None)

    def peek_attach(self, workspace: str) -> dict | None:
        with self.lock:
            slot = self._attach.get(workspace)
            return dict(slot) if slot else None


def _json_body(handler: BaseHTTPRequestHandler) -> dict:
    length = int(handler.headers.get("Content-Length") or "0")
    raw = handler.rfile.read(length) if length else b"{}"
    payload = json.loads(raw.decode("utf-8") or "{}")
    if not isinstance(payload, dict):
        raise ValueError("JSON object required")
    return payload


def make_handler(store: Store):
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, fmt: str, *args) -> None:
            sys.stderr.write("packsetd: " + (fmt % args) + "\n")

        def _send(self, code: int, payload) -> None:
            body = json.dumps(payload).encode("utf-8")
            self.send_response(code)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def _err(self, code: int, message: str) -> None:
            self._send(code, {"error": message})

        def do_GET(self) -> None:
            parsed = urlparse(self.path)
            qs = parse_qs(parsed.query)
            if parsed.path == "/health":
                self.send_response(200)
                self.send_header("Content-Type", "text/plain")
                self.end_headers()
                self.wfile.write(b"packsetd ok")
                return
            if parsed.path == "/__inside_memd/health":
                return self._err(404, "not found")
            if parsed.path == "/v1/status":
                workspace = (qs.get("workspace") or [""])[0] or None
                self._send(200, store.status(workspace))
                return
            if parsed.path == "/v1/workspaces":
                self._send(200, {"workspaces": store.workspaces()})
                return
            if parsed.path == "/v1/identity":
                cwd = (qs.get("cwd") or [os.getcwd()])[0]
                harness = (qs.get("harness") or ["any"])[0]
                ident = inside_identity.identity(harness=harness, cwd=Path(cwd))
                self._send(200, ident)
                return
            if parsed.path == "/v1/pin":
                workspace = (qs.get("workspace") or [""])[0]
                if not workspace:
                    return self._err(400, "workspace required")
                self._send(200, store.pin_payload(workspace))
                return
            if parsed.path == "/v1/rules":
                cwd = Path((qs.get("cwd") or [os.getcwd()])[0])
                body = (qs.get("body") or [""])[0] in {"1", "true", "yes"}
                self._send(
                    200, inside_context.rules_payload(cwd, home=store.home, body=body)
                )
                return
            if parsed.path == "/v1/skills":
                cwd = Path((qs.get("cwd") or [os.getcwd()])[0])
                name = (qs.get("name") or [""])[0] or None
                # Global skills live under $HOME, not the pack home.
                self._send(200, inside_context.skills_payload(cwd, name=name))
                return
            if parsed.path == "/v1/map":
                cwd = Path((qs.get("cwd") or [os.getcwd()])[0])
                self._send(200, inside_context.repo_map(cwd))
                return
            if parsed.path == "/v1/attach":
                workspace = (qs.get("workspace") or [""])[0]
                if not workspace:
                    return self._err(400, "workspace required")
                peek = (qs.get("peek") or [""])[0] in {"1", "true", "yes"}
                slot = store.peek_attach(workspace) if peek else store.take_attach(workspace)
                if slot is None:
                    self._send(200, {"workspace": workspace, "text": "", "label": ""})
                    return
                self._send(200, {"workspace": workspace, **slot})
                return
            if parsed.path == "/v1/set":
                workspace = (qs.get("workspace") or [""])[0]
                name = (qs.get("name") or [""])[0]
                if not workspace:
                    return self._err(400, "workspace required")
                if not name:
                    return self._err(400, "name required")
                try:
                    self._send(200, store.pack(workspace, set_name=name))
                except inside_memory.AtomError as exc:
                    return self._err(400, str(exc))
                return
            if parsed.path == "/v1/pack":
                workspace = (qs.get("workspace") or [""])[0]
                if not workspace:
                    return self._err(400, "workspace required")
                set_name = (qs.get("set") or [""])[0] or None
                try:
                    self._send(200, store.pack(workspace, set_name=set_name))
                except inside_memory.AtomError as exc:
                    return self._err(400, str(exc))
                return
            if parsed.path == "/v1/atoms":
                workspace = (qs.get("workspace") or [""])[0]
                if not workspace:
                    return self._err(400, "workspace required")
                self._send(200, {"atoms": store.current(workspace)})
                return
            if parsed.path.startswith("/v1/atoms/"):
                atom_id = parsed.path[len("/v1/atoms/") :]
                if not atom_id or "/" in atom_id:
                    return self._err(404, "not found")
                workspace = (qs.get("workspace") or [""])[0]
                if not workspace:
                    return self._err(400, "workspace required")
                atom = store.get(workspace, atom_id)
                if atom is None or not inside_memory.is_live(atom):
                    return self._err(404, "no atom")
                self._send(200, atom)
                return
            if parsed.path == "/v1/search":
                workspace = (qs.get("workspace") or [""])[0]
                if not workspace:
                    return self._err(400, "workspace required")
                query = (qs.get("q") or [""])[0]
                limit_raw = (qs.get("limit") or [""])[0]
                set_name = (qs.get("set") or [""])[0] or None
                try:
                    limit = 16 if not limit_raw else int(limit_raw)
                except ValueError:
                    return self._err(400, "limit must be an integer")
                try:
                    # Full live atoms always; set filters in the search layer.
                    # When a set is named, pack prose is that set's cards.
                    if set_name:
                        cards = inside_set.read_set(
                            workspace, set_name, home=store.home
                        )
                        pack = {
                            "workspace": workspace,
                            "set": cards["set"],
                            "user": cards["user"],
                            "memory": cards["memory"],
                            "atoms": store.current(workspace),
                        }
                        set_name = cards["set"]
                    else:
                        pack = store.pack(workspace)
                except inside_memory.AtomError as exc:
                    return self._err(400, str(exc))
                hits, engine = inside_search.search_pack_with_engine(
                    pack,
                    query,
                    limit=limit,
                    index_dir=store.milli_dir,
                    set_name=set_name,
                )
                self._send(200, {"hits": hits, "engine": engine})
                return
            if parsed.path == "/v1/recall":
                workspace = (qs.get("workspace") or [""])[0]
                if not workspace:
                    return self._err(400, "workspace required")
                limit_raw = (qs.get("limit") or [""])[0]
                try:
                    limit = (
                        inside_recall.DEFAULT_LIMIT
                        if not limit_raw
                        else int(limit_raw)
                    )
                except ValueError:
                    return self._err(400, "limit must be an integer")
                seed_raw = (qs.get("seed") or [""])[0]
                seeds = [part for part in seed_raw.split(",") if part] or None
                hints = (qs.get("q") or [""])[0] or None
                self._send(
                    200,
                    {
                        "atoms": inside_recall.recall(
                            workspace,
                            seeds=seeds,
                            hints=hints,
                            limit=limit,
                            atoms=store.current(workspace),
                        )
                    },
                )
                return
            self._err(404, "not found")

        def do_PUT(self) -> None:
            parsed = urlparse(self.path)
            try:
                body = _json_body(self)
            except ValueError as exc:
                return self._err(400, str(exc))
            try:
                if parsed.path == "/v1/pin":
                    workspace = body.get("workspace") or ""
                    if not workspace:
                        return self._err(400, "workspace required")
                    pinned = store.set_pin(workspace, body.get("set") or "")
                    return self._send(200, {"workspace": workspace, "set": pinned})
                if parsed.path == "/v1/set":
                    workspace = body.get("workspace") or ""
                    name = body.get("name") or body.get("set") or ""
                    if not workspace:
                        return self._err(400, "workspace required")
                    stored = inside_set.check_set_name(name)
                    if "user" in body:
                        inside_set.write_set_user(
                            workspace, stored, body.get("user") or "", home=store.home
                        )
                    if "memory" in body:
                        inside_set.write_set_memory(
                            workspace,
                            stored,
                            body.get("memory") or "",
                            home=store.home,
                        )
                    if "instructions" in body:
                        inside_set.write_set_instructions(
                            workspace,
                            stored,
                            body.get("instructions") or "",
                            home=store.home,
                        )
                    return self._send(200, store.pack(workspace, set_name=stored))
                if parsed.path == "/v1/user":
                    inside_memory.set_user(body.get("text") or "", home=store.home)
                    store._milli_files()
                    return self._send(200, {"ok": True})
                if parsed.path == "/v1/memory":
                    workspace = body.get("workspace") or ""
                    inside_memory.set_memory(
                        workspace, body.get("text") or "", home=store.home
                    )
                    store._milli_files(workspace)
                    return self._send(200, {"ok": True})
            except inside_memory.MemoryOverflow as exc:
                return self._err(413, str(exc))
            except (inside_memory.AtomError, inside_prose.ProseError) as exc:
                return self._err(400, str(exc))
            self._err(404, "not found")

        def do_POST(self) -> None:
            parsed = urlparse(self.path)
            try:
                body = _json_body(self)
            except ValueError as exc:
                return self._err(400, str(exc))
            try:
                if parsed.path == "/v1/atoms":
                    return self._send(200, store.add(body))
                if parsed.path == "/v1/atoms/update":
                    return self._send(
                        200,
                        store.update(
                            body["workspace"], body["id"], body.get("fields") or {}
                        ),
                    )
                if parsed.path == "/v1/atoms/delete":
                    return self._send(
                        200, store.delete(body["workspace"], body["id"])
                    )
                if parsed.path == "/v1/attach":
                    workspace = body.get("workspace") or ""
                    if not workspace:
                        return self._err(400, "workspace required")
                    text = body.get("text")
                    if text is None:
                        source = body.get("path") or ""
                        text = inside_context.read_attach_source(source)
                    elif not isinstance(text, str):
                        text = str(text)
                    return self._send(
                        200,
                        store.put_attach(
                            workspace, text, label=body.get("label") or ""
                        ),
                    )
            except (inside_memory.AtomError, KeyError) as exc:
                return self._err(400, str(exc))
            self._err(404, "not found")

    return Handler


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=int(os.environ.get("GROK_MEM_PORT") or DEFAULT_PORT))
    parser.add_argument(
        "--home",
        default=os.environ.get("GROK_INSIDE_MEMORY_HOME") or str(inside_memory.memory_root()),
    )
    parser.add_argument(
        "--fuse",
        default=None,
        help="host fuse voter (PACKSET_FUSE); default borda",
    )
    parser.add_argument(
        "--diversify",
        default=None,
        help="host diversify voter (PACKSET_DIVERSIFY); default mmr",
    )
    parser.add_argument(
        "--decay",
        default=None,
        help="host decay voter (PACKSET_DECAY); default off",
    )
    args = parser.parse_args(argv)
    if args.host != "127.0.0.1":
        raise SystemExit("packsetd listens on 127.0.0.1 only")
    try:
        inside_search.bind_host_panel(args.fuse, args.diversify, args.decay)
    except inside_search.UnknownVoter as exc:
        raise SystemExit(f"packsetd: {exc}") from exc
    store = Store(Path(args.home))
    server = ThreadingHTTPServer((args.host, args.port), make_handler(store))
    sys.stderr.write(f"packsetd: listening on http://{args.host}:{args.port}\n")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
        store.close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
