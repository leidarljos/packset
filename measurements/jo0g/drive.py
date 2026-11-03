#!/usr/bin/env python3
"""Seed a shared workspace and time writes against a copy of it.

The writer under test is packsetd. Each probe copies one seeded home, so
every draw of a size starts from the same pack, then posts a short run of
new claims into that workspace. Hammer names a fresh workspace from its
pid, so it cannot continue a copied pack; the claims here are the shape it
posts (one lesson, unique words, one seat entity).

The default live cap is 20_000. A probe adds PROBE claims on top of a pack
that may already be that large, and those writes are what the trace times.
PACKSET_LIVE_CAP is set to 20_000 + PROBE so the timed writes do not forget
a claim: forgetting is the cap stage, and this measurement is the walk a
write does before that.
"""

import json
import os
import shutil
import signal
import subprocess
import sys
import time
import urllib.error
import urllib.request

PROBE = 48
# Default cap plus the probe, so a pack of 20_000 accepts the timed writes.
CAP = 20_000 + PROBE
WORKSPACE = "shared"
HOST = "127.0.0.1"


def pseudo(seed: int) -> str:
    consonants = "bdfgklmnprstvz"
    vowels = "aeiou"
    x = (seed * 2_654_435_761) & 0xFFFFFFFF
    x |= 1
    chars = []
    for i in range(6):
        alphabet = consonants if i % 2 == 0 else vowels
        chars.append(alphabet[x % len(alphabet)])
        x //= len(alphabet)
        x = (x * 1_103_515_245 + 12_345) & 0xFFFFFFFF
    return "".join(chars)


def claim(n: int, seat: str) -> dict:
    # Six pseudo-words, so the text shares no token with the seeded pack
    # and the candidate lists stay the probe's own. The walk of the pack
    # is then what the dedup stage is timing.
    words = " ".join(pseudo(n * 6 + i + 1) for i in range(6))
    return {
        "schema": "inside.atom/v1",
        "kind": "lesson",
        "level": "explicit",
        "text": f"Seat {seat} filed observation {n} as {words}.",
        "workspace": WORKSPACE,
        "entities": [f"item-{n:05d}"],
    }


def post(port: int, body: dict) -> None:
    data = json.dumps(body).encode()
    req = urllib.request.Request(
        f"http://{HOST}:{port}/v1/atoms",
        data=data,
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=60) as resp:
        if resp.status != 200:
            raise SystemExit(f"post failed: {resp.status} {resp.read()!r}")


def wait_health(port: int) -> None:
    deadline = time.time() + 30
    url = f"http://{HOST}:{port}/health"
    while time.time() < deadline:
        try:
            with urllib.request.urlopen(url, timeout=2) as resp:
                if resp.status == 200:
                    return
        except (urllib.error.URLError, TimeoutError):
            time.sleep(0.05)
    raise SystemExit(f"writer on {port} did not answer /health")


def start_writer(binary: str, home: str, port: int, trace: str | None) -> subprocess.Popen:
    env = os.environ.copy()
    env["PACKSET_AUTH"] = "off"
    env["PACKSET_LIVE_CAP"] = str(CAP)
    if trace:
        env["PACKSET_TRACE_WRITES"] = trace
    else:
        env.pop("PACKSET_TRACE_WRITES", None)
    return subprocess.Popen(
        [binary, "--home", home, "--no-embed", "--host", HOST, "--port", str(port)],
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )


def stop_writer(proc: subprocess.Popen) -> None:
    if proc.poll() is not None:
        return
    proc.send_signal(signal.SIGTERM)
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=5)


def seed(binary: str, home: str, n: int, port: int) -> None:
    if os.path.exists(home):
        shutil.rmtree(home)
    os.makedirs(home, exist_ok=True)
    proc = start_writer(binary, home, port, None)
    try:
        wait_health(port)
        for i in range(n):
            post(port, claim(i, "seed"))
    finally:
        stop_writer(proc)


def probe(binary: str, seed_home: str, dest: str, trace: str, port: int) -> None:
    if os.path.exists(dest):
        shutil.rmtree(dest)
    shutil.copytree(seed_home, dest, symlinks=True)
    if os.path.exists(trace):
        os.remove(trace)
    proc = start_writer(binary, dest, port, trace)
    try:
        wait_health(port)
        started = time.perf_counter()
        # Probe ids sit above any seeded claim so the texts are new.
        base = 1_000_000
        for i in range(PROBE):
            post(port, claim(base + i, "probe"))
        elapsed = time.perf_counter() - started
    finally:
        stop_writer(proc)
    rate = PROBE / elapsed if elapsed else 0.0
    print(f"probe_writes={PROBE} seconds={elapsed:.4f} write_req_s={rate:.2f}")


def main() -> None:
    cmd = sys.argv[1]
    if cmd == "seed":
        _, _, binary, home, n, port = sys.argv
        seed(binary, home, int(n), int(port))
    elif cmd == "probe":
        _, _, binary, seed_home, dest, trace, port = sys.argv
        probe(binary, seed_home, dest, trace, int(port))
    else:
        raise SystemExit(f"unknown command {cmd}")


if __name__ == "__main__":
    main()
