#!/usr/bin/env python3
"""Soak packsetd the way ljos does, and print resident memory.

A turn is what the seat sends around one prompt: health, status, a
search, a recall, the listing without vectors, the pack, and sometimes
a remember, a preference, a note, an activate, a grade or an attach.
Workspaces rotate through repositories, which is a seat moving between
checkouts, then a steady stretch stays on the hot ones.

Samples are kilobytes from /proc/<pid>/status. RssAnon is the heap.
RssFile is the mapped database. VmSwap is what left RAM.
"""

import argparse
import http.client
import json
import os
import signal
import subprocess
import tempfile
import time
import urllib.parse


def pseudo(seed: int) -> str:
    cons = "bdfgklmnprstvz"
    vows = "aeiou"
    x = (seed * 2654435761) & 0xFFFFFFFF
    out = []
    for i in range(4):
        alphabet = cons if i % 2 == 0 else vows
        out.append(alphabet[x % len(alphabet)])
        x = (x * 1103515245 + 12345) & 0xFFFFFFFF
    return "".join(out)


def sample(pid: int) -> dict:
    wanted = ("VmRSS", "RssAnon", "RssFile", "VmData", "VmSwap", "VmSize")
    found = {name: 0 for name in wanted}
    with open(f"/proc/{pid}/status", encoding="utf-8") as handle:
        for line in handle:
            key, _, rest = line.partition(":")
            if key in found:
                found[key] = int(rest.strip().split()[0])
    return found


class Client:
    def __init__(self, port: int):
        self.port = port
        self.errors = 0
        self.sent = 0
        self.ids = []

    def call(self, method: str, path: str, body=None):
        self.sent += 1
        conn = http.client.HTTPConnection("127.0.0.1", self.port, timeout=60)
        data = None
        headers = {}
        if body is not None:
            data = json.dumps(body)
            headers["Content-Type"] = "application/json"
        try:
            conn.request(method, path, body=data, headers=headers)
            resp = conn.getresponse()
            raw = resp.read()
            code = resp.status
        except OSError:
            self.errors += 1
            return 0, {}
        finally:
            conn.close()
        if code >= 400:
            self.errors += 1
        parsed = {}
        if raw and raw[:1] in (b"{", b"["):
            try:
                parsed = json.loads(raw)
            except json.JSONDecodeError:
                parsed = {}
        return code, parsed

    def turn(self, workspace: str, n: int, write: bool):
        q = urllib.parse.quote(workspace)
        cue = f"{pseudo(n)} {pseudo(n + 3)}"
        self.call("GET", "/health")
        self.call("GET", f"/v1/status?workspace={q}")
        self.call("GET", f"/v1/search?workspace={q}&q={urllib.parse.quote(cue)}&limit=8")
        self.call("GET", f"/v1/recall?workspace={q}&q={urllib.parse.quote(cue)}")
        self.call("GET", f"/v1/atoms?workspace={q}&embedding=omit")
        if n % 5 == 0:
            self.call("GET", f"/v1/pack?workspace={q}")
            self.call("GET", "/v1/rules?cwd=.")
            self.call("GET", f"/v1/accessions?workspace={q}")
        if n % 7 == 0:
            self.call(
                "GET",
                f"/v1/activate?workspace={q}&q={urllib.parse.quote(cue)}&fire=1&limit=8",
            )
        if n % 11 == 0:
            self.call("GET", f"/v1/hubs?workspace={q}&limit=5")
            self.call("POST", "/v1/attach", {"workspace": workspace, "text": cue, "label": "log"})
            self.call("GET", f"/v1/attach?workspace={q}")
        if not write:
            # Grade the newest claim in this workspace. An older id may
            # have been superseded, and the seat grades what it just used.
            if n % 9 == 0:
                for atom_id, atom_ws in reversed(self.ids):
                    if atom_ws == workspace:
                        self.call(
                            "POST",
                            "/v1/grade",
                            {"workspace": atom_ws, "id": atom_id, "recalled": True},
                        )
                        break
            return
        kind_roll = n % 5
        if kind_roll == 0:
            body = {
                "workspace": workspace,
                "kind": "preference",
                "origin": "user-declared",
                "text": f"Prefer {pseudo(n)} when naming {pseudo(n + 1)}.",
                "entities": ["seat:grok"],
            }
        elif kind_roll == 1:
            body = {
                "workspace": workspace,
                "kind": "note",
                "origin": "user-declared",
                "issue": f"ljos-{pseudo(n)}",
                "text": (
                    f"The note on {pseudo(n)} records what the seat decided. "
                    f"{pseudo(n + 2).capitalize()} was the part that mattered."
                ),
                "entities": ["seat:grok", "deed-patch-overlay"],
            }
        else:
            body = {
                "workspace": workspace,
                "kind": "lesson",
                "origin": "user-declared",
                "level": "explicit",
                "text": (
                    f"The seat learned {pseudo(n)} {pseudo(n + 1)} today. "
                    f"{pseudo(n + 2).capitalize()} stays with that lesson."
                ),
                "entities": ["seat:grok"],
            }
        code, parsed = self.call("POST", "/v1/atoms", body)
        atom_id = parsed.get("id")
        if code < 400 and isinstance(atom_id, str):
            self.ids.append((atom_id, workspace))


def start_daemon(
    binary: str, home: str, port: int, idle_trim: int, env_extra: dict
) -> subprocess.Popen:
    env = os.environ.copy()
    env["PACKSET_AUTH"] = "off"
    env["PACKSET_EMBED"] = ""
    env["INSIDE_EMBED"] = "off"
    env["PACKSET_IDLE_TRIM"] = "off" if idle_trim == 0 else str(idle_trim)
    env.pop("PACKSET_MILLI", None)
    env.update(env_extra)
    proc = subprocess.Popen(
        [binary, "--home", home, "--port", str(port), "--host", "127.0.0.1"],
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
    )
    deadline = time.time() + 30
    while time.time() < deadline:
        if proc.poll() is not None:
            err = proc.stderr.read().decode() if proc.stderr else ""
            raise SystemExit(f"packsetd exited: {err}")
        try:
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=1)
            conn.request("GET", "/health")
            resp = conn.getresponse()
            resp.read()
            conn.close()
            if resp.status == 200:
                return proc
        except OSError:
            time.sleep(0.05)
    proc.send_signal(signal.SIGTERM)
    raise SystemExit("packsetd did not answer")


def report(label: str, pid: int, sent: int, errors: int):
    row = sample(pid)
    print(
        f"{label:16} sent={sent:5} errors={errors:4} "
        f"rss={row['VmRSS']:7} anon={row['RssAnon']:7} "
        f"file={row['RssFile']:7} swap={row['VmSwap']:7} data={row['VmData']:7}",
        flush=True,
    )
    return row


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin", required=True)
    parser.add_argument("--port", type=int, default=8761)
    parser.add_argument("--workspaces", type=int, default=24)
    parser.add_argument("--fill-atoms", type=int, default=40)
    parser.add_argument("--steady", type=int, default=3)
    parser.add_argument("--steady-turns", type=int, default=200)
    parser.add_argument("--idle", type=float, default=3.0)
    parser.add_argument(
        "--idle-trim",
        type=int,
        default=1,
        help="PACKSET_IDLE_TRIM seconds; 0 keeps the pages",
    )
    args = parser.parse_args()

    home = tempfile.mkdtemp(prefix="packset-soak-")
    proc = start_daemon(args.bin, home, args.port, args.idle_trim, {})
    client = Client(args.port)
    print(f"pid={proc.pid} home={home}", flush=True)
    try:
        report("start", proc.pid, 0, 0)
        for n in range(80):
            client.turn("seat", n, write=True)
        report("warm-seat", proc.pid, client.sent, client.errors)

        for w in range(args.workspaces):
            workspace = f"git:github.com/example/repo-{w}"
            for n in range(args.fill_atoms):
                client.turn(workspace, w * 1000 + n, write=True)
            if (w + 1) % 4 == 0:
                report(f"fill-{w + 1}", proc.pid, client.sent, client.errors)

        hot = ["seat", "git:github.com/example/repo-0", "git:github.com/example/repo-1"]
        for window in range(args.steady):
            for n in range(args.steady_turns):
                client.turn(hot[n % len(hot)], 50_000 + window * 10_000 + n, write=(n % 4 == 0))
            report(f"steady-{window + 1}", proc.pid, client.sent, client.errors)

        time.sleep(args.idle)
        report("after-idle", proc.pid, client.sent, client.errors)
    finally:
        proc.send_signal(signal.SIGTERM)
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=5)
    if client.errors:
        raise SystemExit(f"{client.errors} requests failed")


if __name__ == "__main__":
    main()
