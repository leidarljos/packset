"""packset-tui entry: dump/bump without Textual; app is optional."""

from __future__ import annotations

import argparse
import json
import sys

from packset_tui.atoms import (
    bump_ts,
    dump_table,
    fetch_atoms,
    packset_url,
    resolve_workspace,
    workspace_name,
)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="packset-tui")
    parser.add_argument(
        "--url",
        default=None,
        help="packsetd base URL (else PACKSET_URL, default http://127.0.0.1:8761)",
    )
    parser.add_argument(
        "--workspace",
        default=None,
        help="Atom workspace (else PACKSET_WORKSPACE or GET /v1/identity)",
    )
    parser.add_argument(
        "--dump",
        action="store_true",
        help="Print the atom table as TSV and exit",
    )
    parser.add_argument(
        "--bump",
        metavar="ID",
        default=None,
        help="POST /v1/atoms/update with empty fields to bump ts, then exit",
    )
    args = parser.parse_args(argv)
    base = (args.url or packset_url()).rstrip("/")
    workspace = args.workspace or workspace_name(base)
    if args.bump:
        updated, err = bump_ts(base, workspace, args.bump)
        if err:
            sys.stderr.write(err + "\n")
            return 1
        assert updated is not None
        sys.stdout.write(json.dumps({"id": updated.get("id"), "ts": updated.get("ts")}))
        sys.stdout.write("\n")
        return 0
    if args.dump:
        atoms, err = fetch_atoms(base, workspace)
        if err:
            sys.stderr.write(err + "\n")
            return 1
        sys.stdout.write(dump_table(atoms))
        return 0
    from packset_tui.app import PacksetApp

    PacksetApp(base=base, workspace=resolve_workspace(base, args.workspace)).run()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
