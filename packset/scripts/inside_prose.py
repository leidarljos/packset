#!/usr/bin/env python3
"""Hemingway-style complexity for the seat pack.

Memories that a model has to parse twice waste the 1375 / 2200
caps. Grade is Automated Readability Index (what Hemingway
uses). We also count adverbs, passive be-verbs, and long
sentences. No editor binary. No network.
"""
from __future__ import annotations

import re
from typing import Any

_SENTENCE = re.compile(r"[.!?]+")
_WORD = re.compile(r"[A-Za-z][A-Za-z'-]*")
_ADVERB = re.compile(
    r"(?i)\b(very|really|quite|just|actually|basically|literally|seriously|"
    r"extremely|incredibly|totally|definitely|probably|certainly|"
    r"[A-Za-z]+ly)\b"
)
_BE = re.compile(r"(?i)\b(am|is|are|was|were|be|been|being)\b")
_PARTICIPLE = re.compile(r"(?i)\b\w+(ed|en)\b")

HARD_WORDS = 20
VERY_HARD_WORDS = 30
# Hemingway aims at grade 9. We refuse only "very hard" (15+).
MAX_GRADE = 14.0
MAX_ADVERB_RATIO = 0.12
MAX_ATOM_SENTENCES = 2


class ProseError(ValueError):
    """Text is too complex to live in the working core."""


def _syllables(word: str) -> int:
    token = re.sub(r"[^a-z]", "", word.lower())
    if not token:
        return 1
    vowels = set("aeiouy")
    count = 0
    prev = False
    for ch in token:
        is_v = ch in vowels
        if is_v and not prev:
            count += 1
        prev = is_v
    if token.endswith("e") and count > 1:
        count -= 1
    return max(count, 1)


def assess(text: str) -> dict[str, Any]:
    """Return ARI grade, Flesch ease, and Hemingway-style counts."""
    raw = text or ""
    words = _WORD.findall(raw)
    sentences = [s for s in _SENTENCE.split(raw) if _WORD.search(s)]
    if not sentences and words:
        sentences = [raw]
    n_words = len(words)
    n_sent = max(len(sentences), 1)
    n_chars = sum(len(w) for w in words)
    n_syl = sum(_syllables(w) for w in words)
    _FALSE_LY = {
        "only",
        "family",
        "apply",
        "early",
        "daily",
        "weekly",
        "monthly",
        "yearly",
        "supply",
        "reply",
        "imply",
        "comply",
        "ally",
        "belly",
        "fly",
        "sly",
        "july",
    }
    adverbs = [w for w in _ADVERB.findall(raw) if w.lower() not in _FALSE_LY]
    # Passive: a be-verb near a past participle in the same sentence.
    passives = 0
    hard = 0
    very_hard = 0
    for sent in sentences:
        sent_words = _WORD.findall(sent)
        n = len(sent_words)
        if n >= VERY_HARD_WORDS:
            very_hard += 1
        elif n >= HARD_WORDS:
            hard += 1
        if _BE.search(sent) and _PARTICIPLE.search(sent):
            passives += 1
    grade = None
    ease = None
    if n_words >= 8:
        grade = round(4.71 * (n_chars / n_words) + 0.5 * (n_words / n_sent) - 21.43, 2)
        if n_syl:
            ease = round(
                206.835 - 1.015 * (n_words / n_sent) - 84.6 * (n_syl / n_words), 2
            )
    adverb_ratio = (len(adverbs) / n_words) if n_words else 0.0
    return {
        "words": n_words,
        "sentences": len(sentences) if words else 0,
        "grade": grade,
        "ease": ease,
        "adverbs": len(adverbs),
        "adverb_ratio": round(adverb_ratio, 3),
        "passives": passives,
        "hard_sentences": hard,
        "very_hard_sentences": very_hard,
    }


def refuse(text: str, *, role: str = "file") -> dict[str, Any]:
    """Raise ProseError if the text is too hard for the working core."""
    report = assess(text)
    if role == "atom":
        if report["sentences"] > MAX_ATOM_SENTENCES:
            raise ProseError(
                f"atom has {report['sentences']} sentences; "
                f"one claim is at most {MAX_ATOM_SENTENCES}"
            )
        if report["very_hard_sentences"]:
            raise ProseError("atom sentence is very hard to read")
    if report["words"] >= 12 and report["grade"] is not None:
        if report["grade"] > MAX_GRADE:
            raise ProseError(
                f"readability grade {report['grade']} exceeds {MAX_GRADE}"
            )
        if report["adverb_ratio"] > MAX_ADVERB_RATIO:
            raise ProseError(
                f"adverb ratio {report['adverb_ratio']} exceeds {MAX_ADVERB_RATIO}"
            )
    return report
