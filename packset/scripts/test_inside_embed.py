"""Local ONNX embedder. Works without the model present."""

from __future__ import annotations

import pytest

import inside_embed


def test_off_returns_none(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("INSIDE_EMBED", "off")
    assert not inside_embed.enabled()
    assert inside_embed.encode_one("Reviews open first.") is None


def test_cosine_identical_and_orthogonal() -> None:
    assert inside_embed.cosine([1.0, 0.0], [1.0, 0.0]) == pytest.approx(1.0)
    assert inside_embed.cosine([1.0, 0.0], [0.0, 1.0]) == pytest.approx(0.0)
    assert inside_embed.cosine([], [1.0]) == 0.0


def test_available_false_without_cache_or_download(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("INSIDE_EMBED_DOWNLOAD", "0")
    monkeypatch.setenv("INSIDE_EMBED_CACHE", "/tmp/inside-embed-missing")
    assert not inside_embed.available()
