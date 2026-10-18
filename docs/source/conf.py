"""Sphinx configuration for the packset project site (Shibuya theme)."""

from __future__ import annotations

from pathlib import Path

from docutils import nodes
from sphinx.writers.html5 import HTML5Translator

_DOCS = Path(__file__).resolve().parent
_ROOT = _DOCS.parent.parent

project = "packset"
copyright = "2026, Rohit Goswami"
author = "Rohit Goswami"
release = "0.2.0"
version = "0.2"

extensions = [
    "sphinx.ext.mathjax",
    "sphinx_copybutton",
]

templates_path = ["_templates"]
exclude_patterns: list[str] = []

html_theme = "shibuya"
html_static_path = ["_static"]
html_favicon = "_static/favicon.svg"
html_logo = "_static/mark.svg"
html_title = "packset"
html_css_files = ["custom.css"]
# The copy-page menu fetches this absolute URL. Sphinx writes the page
# source under _sources/, so the link is a file the built site serves.
html_baseurl = "https://leidarljos.github.io/packset/"
html_copy_source = True
html_show_sourcelink = True

# 1200x630 PNG from docs/scripts/render-og-png.sh. Social cards need a PNG.
_OG_IMAGE = "https://leidarljos.github.io/packset/_static/og.png"
_PAGE_DESCRIPTION = {
    "search": "Search the packset documentation.",
    "genindex": "Index of the packset documentation.",
}

html_context = {
    "source_type": "github",
    "source_user": "leidarljos",
    "source_repo": "packset",
    "source_version": "main",
    "source_docs_path": "/docs/source/",
}

html_theme_options = {
    "accent_color": "gold",
    "color_mode": "auto",
    "dark_code": True,
    "open_in_chatgpt": False,
    "open_in_claude": False,
    "github_url": "https://github.com/leidarljos/packset",
    "nav_links": [
        {"title": "Get started", "url": "getting-started"},
        {"title": "How-to", "url": "howto"},
        {"title": "Reference", "url": "reference"},
        {"title": "Explanation", "url": "explanation"},
        {"title": "Search", "url": "search"},
    ],
}

# Offline builds must not reach for an inventory.
intersphinx_mapping: dict = {}

copybutton_prompt_text = r"\$ "
copybutton_prompt_is_regexp = True


class _HTMLTranslator(HTML5Translator):
    """Sections start at h2. The template supplies the one page title h1."""

    def __init__(self, document: nodes.document, builder: object) -> None:
        super().__init__(document, builder)
        self.initial_header_level = 2


def _use_document_title(app: object, doctree: nodes.document) -> None:
    """Prev/next labels follow ``.. title::``, not the first section."""
    title = doctree.get("title")
    if not title:
        return
    node = nodes.title()
    node += nodes.Text(title)
    app.env.titles[app.env.docname] = node  # type: ignore[attr-defined]


def _page_social(app: object, pagename: str, templatename: str, context: dict, doctree: object) -> None:
    meta = context.get("meta")
    if not isinstance(meta, dict):
        meta = {}
        context["meta"] = meta
    meta["cover"] = _OG_IMAGE
    if not meta.get("description"):
        meta["description"] = _PAGE_DESCRIPTION.get(
            pagename, "Documentation for packset."
        )


def setup(app: object) -> None:
    app.set_translator("html", _HTMLTranslator)  # type: ignore[attr-defined]
    # After the builtin title collector (priority 500).
    app.connect("doctree-read", _use_document_title, priority=800)  # type: ignore[attr-defined]
    app.connect("html-page-context", _page_social)  # type: ignore[attr-defined]
