"""Packset pane: Textual DataTable over packsetd HTTP.

Not WorkGraph. Not todos. Not the vault.
"""

from __future__ import annotations

from typing import ClassVar

from textual.app import App, ComposeResult
from textual.binding import Binding
from textual.containers import Vertical
from textual.screen import ModalScreen
from textual.widgets import DataTable, Footer, Header, Input, OptionList, Static
from textual.widgets.option_list import Option

from packset_tui.atoms import (
    BASE_COLUMNS,
    bump_ts,
    fetch_atoms,
    list_workspaces,
    packset_url,
    table_columns,
    table_row,
    workspace_name,
)
from packset_tui.theme import CSS_FILES


class WorkspaceScreen(ModalScreen[str | None]):
    """Pick a workspace id the writer already has, or type one."""

    BINDINGS: ClassVar[list[Binding]] = [
        Binding("escape", "dismiss", "Close", show=False),
    ]

    def __init__(self, rows: list[dict], current: str) -> None:
        super().__init__()
        self._rows = rows
        self._current = current

    def compose(self) -> ComposeResult:
        options = [
            Option(f"{row['name']}  ({row['live']})", id=f"ws-{idx}")
            for idx, row in enumerate(self._rows)
        ]
        with Vertical(id="workspace-dialog"):
            yield Static("workspace  enter name or pick", id="workspace-hint")
            yield Input(value=self._current, placeholder="workspace", id="workspace-input")
            yield OptionList(*options, id="workspace-list")

    def on_mount(self) -> None:
        self.query_one("#workspace-input", Input).focus()
        names = [str(row["name"]) for row in self._rows]
        if self._current in names:
            self.query_one("#workspace-list", OptionList).highlighted = names.index(
                self._current
            )

    def on_input_submitted(self, event: Input.Submitted) -> None:
        name = event.value.strip()
        self.dismiss(name or None)

    def on_option_list_option_selected(self, event: OptionList.OptionSelected) -> None:
        ident = str(event.option.id or "")
        if ident.startswith("ws-"):
            idx = int(ident.removeprefix("ws-"))
            if 0 <= idx < len(self._rows):
                self.dismiss(str(self._rows[idx]["name"]))
                return
        self.dismiss(None)

    def action_dismiss(self) -> None:
        self.dismiss(None)


class PacksetApp(App[None]):
    CSS_PATH = CSS_FILES
    TITLE = "packset"
    BINDINGS: ClassVar[list[Binding]] = [
        Binding("b", "bump", "Bump ts"),
        Binding("r", "refresh", "Refresh"),
        Binding("w", "workspace", "Workspace"),
        Binding("[", "workspace_prev", "Prev ws"),
        Binding("]", "workspace_next", "Next ws"),
        Binding("q", "quit", "Quit"),
    ]

    def __init__(
        self,
        base: str | None = None,
        workspace: str | None = None,
    ) -> None:
        super().__init__()
        self.base = (base or packset_url()).rstrip("/")
        self.workspace = workspace or workspace_name(self.base)
        self._ids: list[str] = []
        self._columns: tuple[str, ...] = BASE_COLUMNS

    def compose(self) -> ComposeResult:
        self.sub_title = f"{self.base}  {self.workspace}"
        yield Header()
        yield DataTable()
        yield Static(id="status")
        yield Footer()

    def on_mount(self) -> None:
        table = self.query_one(DataTable)
        table.cursor_type = "row"
        table.zebra_stripes = True
        self.refresh_table()

    def _set_status(self, text: str) -> None:
        self.query_one("#status", Static).update(text)

    def _catalog(self) -> list[dict]:
        return list_workspaces(self.base, extra=[self.workspace, workspace_name(self.base)])

    def set_workspace(self, name: str) -> None:
        cleaned = name.strip()
        if not cleaned or cleaned == self.workspace:
            return
        self.workspace = cleaned
        self.sub_title = f"{self.base}  {self.workspace}"
        self.refresh_table()

    def refresh_table(self) -> None:
        table = self.query_one(DataTable)
        atoms, err = fetch_atoms(self.base, self.workspace)
        columns = table_columns(atoms)
        if columns != self._columns or not table.columns:
            table.clear(columns=True)
            table.add_columns(*columns)
            self._columns = columns
        else:
            table.clear()
        self._ids = []
        if err:
            self._set_status(f"offline  {err}")
            return
        for atom in atoms:
            aid = str(atom.get("id") or "")
            self._ids.append(aid)
            table.add_row(*table_row(atom, columns), key=aid)
        hint = ""
        if not atoms:
            others = [row for row in self._catalog() if row["name"] != self.workspace and row["live"]]
            if others:
                top = others[0]
                hint = f"  w switch ({top['name']} has {top['live']})"
        self._set_status(f"{len(atoms)} atoms  {self.workspace}{hint}")

    def action_refresh(self) -> None:
        self.refresh_table()

    def action_workspace(self) -> None:
        def picked(name: str | None) -> None:
            if name:
                self.set_workspace(name)

        self.push_screen(WorkspaceScreen(self._catalog(), self.workspace), picked)

    def action_workspace_prev(self) -> None:
        self._cycle_workspace(-1)

    def action_workspace_next(self) -> None:
        self._cycle_workspace(1)

    def _cycle_workspace(self, step: int) -> None:
        names = [str(row["name"]) for row in self._catalog()]
        if not names:
            return
        if self.workspace in names:
            idx = names.index(self.workspace)
        else:
            idx = 0
        self.set_workspace(names[(idx + step) % len(names)])

    def action_bump(self) -> None:
        table = self.query_one(DataTable)
        if not self._ids:
            self.notify("no atom")
            return
        coord = table.cursor_coordinate
        if coord.row < 0 or coord.row >= len(self._ids):
            self.notify("select a row")
            return
        updated, err = bump_ts(self.base, self.workspace, self._ids[coord.row])
        self.notify(err or "ts bumped")
        if updated is not None:
            self.refresh_table()
