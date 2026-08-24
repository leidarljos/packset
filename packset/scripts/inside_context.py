#!/usr/bin/env python3
"""Project rules, skills, and a git map. Not the memory pack.

Warp-shaped context: rules are always-on files, skills are on-demand
workflows (name and description first), the map is git-tracked
structure with ignore files and a status. Nothing here is written
into USER.md, MEMORY.md, or atoms.
"""

from __future__ import annotations

import fnmatch
import os
import subprocess
from pathlib import Path
from typing import Any

import inside_memory

PROJECT_RULE_NAMES = ("AGENTS.md", "WARP.md")
LINKED_RULE_NAMES = (
    "CLAUDE.md",
    "AGENT.md",
    "GEMINI.md",
    ".cursorrules",
    ".clinerules",
    ".windsurfrules",
)
LINKED_RULE_RELPATHS = (".github/copilot-instructions.md",)

SKILL_DIR_NAMES = (
    ".agents/skills",
    ".warp/skills",
    ".claude/skills",
    ".codex/skills",
    ".cursor/skills",
    ".gemini/skills",
    ".copilot/skills",
    ".factory/skills",
    ".github/skills",
    ".opencode/skills",
    ".grok/skills",
)

IGNORE_FILE_NAMES = (
    ".warpindexingignore",
    ".cursorignore",
    ".cursorindexingignore",
    ".codeiumignore",
    ".grokindexingignore",
)

MAP_TOO_LARGE = 5000
MAP_LIST_CAP = 500
ATTACH_CAP = 32 * 1024


def git_root(cwd: Path | None = None) -> Path | None:
    """Repository toplevel, or None when cwd is not in a work tree."""
    root = (cwd or Path.cwd()).resolve()
    try:
        proc = subprocess.run(
            ["git", "-C", str(root), "rev-parse", "--show-toplevel"],
            check=False,
            capture_output=True,
            text=True,
        )
    except OSError:
        return None
    if proc.returncode != 0:
        return None
    text = (proc.stdout or "").strip()
    return Path(text) if text else None


def git_dir(cwd: Path | None = None) -> Path | None:
    """Resolved git dir. Distinct for worktrees."""
    root = (cwd or Path.cwd()).resolve()
    try:
        proc = subprocess.run(
            ["git", "-C", str(root), "rev-parse", "--git-dir"],
            check=False,
            capture_output=True,
            text=True,
        )
    except OSError:
        return None
    if proc.returncode != 0:
        return None
    text = (proc.stdout or "").strip()
    if not text:
        return None
    path = Path(text)
    if not path.is_absolute():
        path = (root / path).resolve()
    return path


def _walk_to_root(cwd: Path, stop: Path) -> list[Path]:
    """cwd first, then parents, ending at stop (inclusive)."""
    cur = cwd.resolve()
    stop = stop.resolve()
    out: list[Path] = []
    while True:
        out.append(cur)
        if cur == stop or cur.parent == cur:
            break
        cur = cur.parent
    return out


def parse_frontmatter(text: str) -> tuple[dict[str, str], str]:
    """YAML-ish --- name: / description: --- block. Missing is empty."""
    raw = text or ""
    if not raw.startswith("---"):
        return {}, raw
    rest = raw[3:]
    if rest.startswith("\r\n"):
        rest = rest[2:]
    elif rest.startswith("\n"):
        rest = rest[1:]
    marker = "\n---"
    end = rest.find(marker)
    if end < 0:
        return {}, raw
    block = rest[:end]
    body = rest[end + len(marker) :]
    if body.startswith("\r\n"):
        body = body[2:]
    elif body.startswith("\n"):
        body = body[1:]
    meta: dict[str, str] = {}
    for line in block.splitlines():
        if ":" not in line:
            continue
        key, value = line.split(":", 1)
        name = key.strip().lower()
        if not name:
            continue
        meta[name] = value.strip().strip('"').strip("'")
    return meta, body


def _read_text(path: Path) -> str:
    try:
        return path.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError):
        return ""


def discover_rules(
    cwd: Path | None = None,
    *,
    home: Path | None = None,
) -> list[dict[str, Any]]:
    """Rule files, most specific first. Bodies stay on disk unless asked."""
    here = (cwd or Path.cwd()).resolve()
    root = git_root(here) or here
    found: list[dict[str, Any]] = []
    seen: set[Path] = set()

    def _add(path: Path, kind: str, scope: str) -> None:
        resolved = path.resolve()
        if resolved in seen or not path.is_file():
            return
        seen.add(resolved)
        found.append(
            {
                "path": str(resolved),
                "relpath": str(path.relative_to(root)) if _is_relative(path, root) else path.name,
                "kind": kind,
                "scope": scope,
                "name": path.name,
            }
        )

    for directory in _walk_to_root(here, root):
        scope = "cwd" if directory == here else ("root" if directory == root else "parent")
        for name in PROJECT_RULE_NAMES:
            _add(directory / name, "project", scope)
        for name in LINKED_RULE_NAMES:
            _add(directory / name, "linked", scope)
        if directory == root:
            for rel in LINKED_RULE_RELPATHS:
                _add(directory / rel, "linked", "root")

    user = inside_memory.user_path(home)
    if user.is_file() and _read_text(user).strip():
        found.append(
            {
                "path": str(user.resolve()),
                "relpath": "USER.md",
                "kind": "global",
                "scope": "global",
                "name": "USER.md",
            }
        )
    return found


def _is_relative(path: Path, root: Path) -> bool:
    try:
        path.resolve().relative_to(root.resolve())
        return True
    except ValueError:
        return False


def _skill_dirs(cwd: Path, root: Path, home: Path | None) -> list[tuple[Path, str]]:
    """(directory, scope) pairs. Project walk first, then home."""
    pairs: list[tuple[Path, str]] = []
    seen: set[Path] = set()
    for directory in _walk_to_root(cwd, root):
        for rel in SKILL_DIR_NAMES:
            candidate = directory / rel
            resolved = candidate.resolve()
            if resolved in seen or not candidate.is_dir():
                continue
            seen.add(resolved)
            scope = "project"
            pairs.append((candidate, scope))
    home_root = Path(home) if home is not None else Path.home()
    for rel in SKILL_DIR_NAMES:
        candidate = home_root / rel
        resolved = candidate.resolve()
        if resolved in seen or not candidate.is_dir():
            continue
        seen.add(resolved)
        pairs.append((candidate, "global"))
    return pairs


def _skill_from_dir(skill_dir: Path, scope: str) -> dict[str, Any] | None:
    path = skill_dir / "SKILL.md"
    if not path.is_file():
        return None
    text = _read_text(path)
    if not text.strip():
        return None
    meta, _body = parse_frontmatter(text)
    name = (meta.get("name") or skill_dir.name).strip() or skill_dir.name
    description = (meta.get("description") or "").strip()
    if not description:
        for line in text.splitlines():
            stripped = line.strip()
            if stripped.startswith("#"):
                description = stripped.lstrip("#").strip()
                if description:
                    break
    supporting = sorted(
        child.name for child in skill_dir.iterdir() if child.is_file() and child.name != "SKILL.md"
    )
    return {
        "name": name,
        "description": description,
        "path": str(path.resolve()),
        "dir": str(skill_dir.resolve()),
        "scope": scope,
        "supporting": supporting,
    }


def discover_skills(
    cwd: Path | None = None,
    *,
    home: Path | None = None,
) -> list[dict[str, Any]]:
    """Skill catalog: name, description, path. No bodies."""
    here = (cwd or Path.cwd()).resolve()
    root = git_root(here) or here
    skills: list[dict[str, Any]] = []
    seen: set[Path] = set()
    for directory, scope in _skill_dirs(here, root, home):
        try:
            children = sorted(directory.iterdir(), key=lambda p: p.name)
        except OSError:
            continue
        for child in children:
            if not child.is_dir():
                continue
            resolved = child.resolve()
            if resolved in seen:
                continue
            entry = _skill_from_dir(child, scope)
            if entry is None:
                continue
            seen.add(resolved)
            skills.append(entry)
    return skills


def read_skill(
    name: str,
    cwd: Path | None = None,
    *,
    home: Path | None = None,
) -> dict[str, Any] | None:
    """Full SKILL.md body for the first matching name in discover order."""
    wanted = (name or "").strip()
    if not wanted:
        return None
    for entry in discover_skills(cwd, home=home):
        if entry["name"] == wanted or Path(entry["dir"]).name == wanted:
            path = Path(entry["path"])
            text = _read_text(path)
            meta, body = parse_frontmatter(text)
            out = dict(entry)
            out["body"] = body
            out["frontmatter"] = meta
            out["text"] = text
            return out
    return None


def _load_ignore_patterns(root: Path) -> list[str]:
    patterns: list[str] = []
    for name in IGNORE_FILE_NAMES:
        path = root / name
        if not path.is_file():
            continue
        for line in _read_text(path).splitlines():
            stripped = line.strip()
            if not stripped or stripped.startswith("#"):
                continue
            patterns.append(stripped)
    return patterns


def _ignored(rel: str, patterns: list[str]) -> bool:
    if not patterns:
        return False
    name = Path(rel).name
    parts = Path(rel).parts
    for pat in patterns:
        raw = pat.rstrip("/")
        if fnmatch.fnmatch(rel, pat) or fnmatch.fnmatch(name, pat):
            return True
        if fnmatch.fnmatch(rel, raw) or fnmatch.fnmatch(name, raw):
            return True
        if pat.endswith("/") and (rel == raw or rel.startswith(raw + "/")):
            return True
        if raw in parts:
            return True
    return False


def _git_ls_files(root: Path) -> tuple[list[str] | None, str]:
    try:
        proc = subprocess.run(
            ["git", "-C", str(root), "ls-files", "-z"],
            check=False,
            capture_output=True,
        )
    except OSError as exc:
        return None, str(exc)
    if proc.returncode != 0:
        err = (proc.stderr or b"").decode("utf-8", errors="replace").strip()
        return None, err or "git ls-files failed"
    raw = proc.stdout or b""
    files = [part.decode("utf-8", errors="replace") for part in raw.split(b"\0") if part]
    return files, ""


def _tree_outline(files: list[str]) -> list[dict[str, Any]]:
    counts: dict[str, int] = {}
    for rel in files:
        top = rel.split("/", 1)[0] if "/" in rel else "."
        counts[top] = counts.get(top, 0) + 1
    return [
        {"path": key, "files": counts[key]}
        for key in sorted(counts, key=lambda item: (item == ".", item))
    ]


def repo_map(cwd: Path | None = None) -> dict[str, Any]:
    """Git-tracked outline. Never a pack write."""
    here = (cwd or Path.cwd()).resolve()
    root = git_root(here)
    gdir = git_dir(here)
    if root is None:
        return {
            "status": "failed",
            "reason": "not a git work tree",
            "cwd": str(here),
            "root": None,
            "git_dir": str(gdir) if gdir else None,
            "count": 0,
            "files": [],
            "tree": [],
        }
    listed, err = _git_ls_files(root)
    if listed is None:
        return {
            "status": "failed",
            "reason": err,
            "cwd": str(here),
            "root": str(root),
            "git_dir": str(gdir) if gdir else None,
            "count": 0,
            "files": [],
            "tree": [],
        }
    patterns = _load_ignore_patterns(root)
    files = [rel for rel in listed if not _ignored(rel, patterns)]
    payload: dict[str, Any] = {
        "cwd": str(here),
        "root": str(root),
        "git_dir": str(gdir) if gdir else None,
        "count": len(files),
        "ignored": len(listed) - len(files),
        "tree": _tree_outline(files),
    }
    if len(files) > MAP_TOO_LARGE:
        payload["status"] = "too-large"
        payload["files"] = []
        payload["listed"] = 0
        return payload
    payload["status"] = "synced"
    payload["files"] = files[:MAP_LIST_CAP]
    payload["listed"] = min(len(files), MAP_LIST_CAP)
    return payload


def read_attach_source(raw: str, *, cap: int = ATTACH_CAP) -> str:
    """File contents when raw is a path, else the string itself."""
    text = (raw or "").strip()
    if not text:
        return ""
    path = Path(text).expanduser()
    if path.is_file():
        body = _read_text(path)
    else:
        body = raw
    if len(body) > cap:
        return body[:cap]
    return body


def rules_payload(
    cwd: Path | None = None,
    *,
    home: Path | None = None,
    body: bool = False,
) -> dict[str, Any]:
    rules = discover_rules(cwd, home=home)
    if body:
        for rule in rules:
            rule["text"] = _read_text(Path(rule["path"]))
    return {"rules": rules, "cwd": str((cwd or Path.cwd()).resolve())}


def skills_payload(
    cwd: Path | None = None,
    *,
    home: Path | None = None,
    name: str | None = None,
) -> dict[str, Any]:
    here = cwd or Path.cwd()
    if name:
        skill = read_skill(name, here, home=home)
        if skill is None:
            return {"skills": [], "cwd": str(here.resolve()), "name": name}
        return {"skills": [skill], "cwd": str(here.resolve()), "name": name}
    return {"skills": discover_skills(here, home=home), "cwd": str(here.resolve())}


def main(argv: list[str] | None = None) -> int:
    import argparse
    import json

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("what", choices=("rules", "skills", "skill", "map"))
    parser.add_argument("name", nargs="?", default="")
    parser.add_argument("--cwd", default=os.getcwd())
    parser.add_argument("--home", default="")
    parser.add_argument("--body", action="store_true")
    args = parser.parse_args(argv)
    cwd = Path(args.cwd)
    home = Path(args.home) if args.home else None
    if args.what == "rules":
        payload = rules_payload(cwd, home=home, body=args.body)
    elif args.what == "skills":
        payload = skills_payload(cwd, home=home)
    elif args.what == "skill":
        payload = skills_payload(cwd, home=home, name=args.name)
    else:
        payload = repo_map(cwd)
    print(json.dumps(payload, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
