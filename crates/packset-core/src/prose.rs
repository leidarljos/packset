//! Hemingway-style complexity for the seat pack.
//!
//! A memory a model has to parse twice wastes the 1375 / 2200 caps. The grade
//! is the Automated Readability Index, which is what Hemingway uses. Adverbs,
//! passive be-verbs and long sentences are counted alongside it. No editor
//! binary and no network: the whole check is arithmetic over the text.

/// A sentence at or past this many words reads hard.
pub const HARD_WORDS: usize = 20;
/// A sentence at or past this many words reads very hard.
pub const VERY_HARD_WORDS: usize = 30;
/// Hemingway aims at grade 9. Only "very hard" is refused.
pub const MAX_GRADE: f64 = 14.0;
/// Adverbs as a fraction of words.
pub const MAX_ADVERB_RATIO: f64 = 0.12;
/// One claim per atom, and a claim is at most this many sentences.
pub const MAX_ATOM_SENTENCES: usize = 2;

/// What the text is being written into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// One atom: one claim, so the sentence count is capped too.
    Atom,
    /// A card on disk: only the readability rules apply.
    File,
}

/// Why text is too complex for the working core.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProseError(pub String);

impl std::fmt::Display for ProseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ProseError {}

/// Readability counts for one piece of text.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    /// Word tokens.
    pub words: usize,
    /// Sentences, or zero when there are no words at all.
    pub sentences: usize,
    /// Automated Readability Index, absent under eight words.
    pub grade: Option<f64>,
    /// Flesch reading ease, absent under eight words.
    pub ease: Option<f64>,
    /// Adverb tokens.
    pub adverbs: usize,
    /// Adverbs over words.
    pub adverb_ratio: f64,
    /// Sentences carrying a be-verb near a past participle.
    pub passives: usize,
    /// Sentences at or past [`HARD_WORDS`].
    pub hard_sentences: usize,
    /// Sentences at or past [`VERY_HARD_WORDS`].
    pub very_hard_sentences: usize,
}

/// Words ending in `ly` that are not adverbs.
const FALSE_LY: &[&str] = &[
    "only", "family", "apply", "early", "daily", "weekly", "monthly", "yearly", "supply", "reply",
    "imply", "comply", "ally", "belly", "fly", "sly", "july",
];

const PLAIN_ADVERBS: &[&str] = &[
    "very",
    "really",
    "quite",
    "just",
    "actually",
    "basically",
    "literally",
    "seriously",
    "extremely",
    "incredibly",
    "totally",
    "definitely",
    "probably",
    "certainly",
];

const BE_VERBS: &[&str] = &["am", "is", "are", "was", "were", "be", "been", "being"];

/// Word tokens: a letter, then letters, apostrophes or hyphens.
fn words_of(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_alphabetic() {
            let start = i;
            i += 1;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphabetic() || bytes[i] == b'\'' || bytes[i] == b'-')
            {
                i += 1;
            }
            out.push(&text[start..i]);
        } else {
            i += 1;
        }
    }
    out
}

/// Stands in for a full stop that does not end a sentence while splitting.
const INNER_DOT: char = '\u{2024}';

/// Abbreviations whose stops never end a sentence.
const DOTTED_ABBREVIATIONS: &[&str] = &["e.g.", "i.e.", "etc.", "vs.", "cf.", "approx.", "a.k.a."];

/// `text` with every full stop that cannot end a sentence masked: one in a
/// code span (`` `foo.rs. bar` ``), one followed at once by a letter, digit
/// or `_` (`foo.rs`, `v0.29.1`, `.DS_Store`), and the stops of
/// [`DOTTED_ABBREVIATIONS`].
fn mask_inner_dots(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<char> = Vec::with_capacity(chars.len());
    let mut in_code = false;
    for (i, &c) in chars.iter().enumerate() {
        if c == '`' {
            in_code = !in_code;
        }
        let next = chars.get(i + 1).copied();
        let inner = c == '.' && (in_code || next.is_some_and(|n| n.is_alphanumeric() || n == '_'));
        out.push(if inner { INNER_DOT } else { c });
    }
    let mut masked: String = out.into_iter().collect();
    for abbr in DOTTED_ABBREVIATIONS {
        let lower = masked.to_lowercase();
        let probe = abbr.replace('.', &INNER_DOT.to_string());
        let mut from = 0;
        let mut spans = Vec::new();
        // Match after the inner stops were masked, on either spelling.
        for needle in [abbr.to_string(), {
            let mut p = probe.clone();
            p.pop();
            p.push('.');
            p
        }] {
            while let Some(pos) = lower[from..].find(&needle) {
                let at = from + pos;
                let starts_word = at == 0
                    || !lower[..at]
                        .chars()
                        .next_back()
                        .is_some_and(char::is_alphanumeric);
                if starts_word {
                    spans.push((at, at + needle.len()));
                }
                from = at + needle.len();
            }
            from = 0;
        }
        for (a, b) in spans.into_iter().rev() {
            let piece: String = masked[a..b]
                .chars()
                .map(|c| if c == '.' { INNER_DOT } else { c })
                .collect();
            masked.replace_range(a..b, &piece);
        }
    }
    masked
}

/// The text's sentences, by snapper's splitter with lowercase starts on, so
/// `Restart it. packsetd reloads.` is two claims. A full stop inside a token
/// (`0.9.3`, `127.0.0.1`, `Cargo.lock`), after an abbreviation or an
/// initial (`et al. 2021`, `J. Smith`) is not a boundary. Pieces with no
/// word are dropped.
#[must_use]
pub fn sentences(text: &str) -> Vec<String> {
    use snapper_fmt::sentence::SentenceSplitter as _;
    static SPLITTER: std::sync::OnceLock<snapper_fmt::sentence::unicode::UnicodeSentenceSplitter> =
        std::sync::OnceLock::new();
    // snapper reads `/`, `_`, `*` and `+` as Org emphasis marks. It keeps a
    // sentence open between two of them, so `see a/b. Then c/d.` would come
    // back as one sentence. Look-alike characters stand in for them while
    // splitting.
    let masked: String = mask_inner_dots(text)
        .chars()
        .map(|c| match c {
            '/' => '\u{2215}',
            '_' => '\u{2017}',
            '*' => '\u{2217}',
            '+' => '\u{FF0B}',
            other => other,
        })
        .collect();
    SPLITTER
        .get_or_init(|| {
            snapper_fmt::sentence::unicode::UnicodeSentenceSplitter::new()
                .with_lowercase_starts(true)
        })
        .split(&masked)
        .into_iter()
        .map(|piece| {
            piece
                .chars()
                .map(|c| match c {
                    '\u{2215}' => '/',
                    '\u{2017}' => '_',
                    '\u{2217}' => '*',
                    '\u{FF0B}' => '+',
                    INNER_DOT => '.',
                    other => other,
                })
                .collect::<String>()
        })
        .filter(|piece| !words_of(piece).is_empty())
        .collect()
}

fn syllables(word: &str) -> usize {
    let token: String = word
        .chars()
        .filter(|c| c.is_ascii_alphabetic())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    if token.is_empty() {
        return 1;
    }
    let mut count = 0usize;
    let mut prev_vowel = false;
    for ch in token.chars() {
        let is_vowel = matches!(ch, 'a' | 'e' | 'i' | 'o' | 'u' | 'y');
        if is_vowel && !prev_vowel {
            count += 1;
        }
        prev_vowel = is_vowel;
    }
    if token.ends_with('e') && count > 1 {
        count -= 1;
    }
    count.max(1)
}

/// Longest a name counts toward the grade, in letters.
const NAME_LETTERS: usize = 6;

/// Letters a word adds to the grade. A name is read as one unit, not
/// spelled out, so it counts at most [`NAME_LETTERS`]: an acronym
/// (`XDG`), a hyphenated or mixed-case token (`packset-embed`, `macOS`),
/// or a piece of a path, variable, version or code span (`XDG_DATA_HOME`,
/// `~/.cache`, `v0.29`, a word in backticks). Ordinary long words still
/// count in full, so plain prose is graded as before.
fn scored_len(text: &str, word: &str) -> usize {
    let len = word.len();
    if len <= NAME_LETTERS {
        return len;
    }
    let start = word.as_ptr() as usize - text.as_ptr() as usize;
    let end = start + len;
    let bytes = text.as_bytes();
    let touches =
        |b: u8| matches!(b, b'_' | b'/' | b'.' | b'`' | b'$' | b'=' | b':') || b.is_ascii_digit();
    let before = start.checked_sub(1).map(|i| bytes[i]);
    let after = bytes.get(end).copied();
    let glued = before.is_some_and(touches)
        || (after.is_some_and(touches)
            && bytes.get(end + 1).is_some_and(|b| !b.is_ascii_whitespace()));
    let upper = word.chars().filter(char::is_ascii_uppercase).count();
    let mixed = upper > 1 || (upper == 1 && !word.starts_with(|c: char| c.is_ascii_uppercase()));
    if glued || mixed || word.contains('-') {
        NAME_LETTERS
    } else {
        len
    }
}

fn is_adverb(word: &str) -> bool {
    let lower = word.to_ascii_lowercase();
    if FALSE_LY.contains(&lower.as_str()) {
        return false;
    }
    PLAIN_ADVERBS.contains(&lower.as_str()) || (lower.len() > 2 && lower.ends_with("ly"))
}

fn has_be_verb(sentence: &str) -> bool {
    words_of(sentence)
        .iter()
        .any(|w| BE_VERBS.contains(&w.to_ascii_lowercase().as_str()))
}

fn has_participle(sentence: &str) -> bool {
    words_of(sentence).iter().any(|w| {
        let lower = w.to_ascii_lowercase();
        lower.len() > 2 && (lower.ends_with("ed") || lower.ends_with("en"))
    })
}

/// Grade, ease, and the Hemingway-style counts.
#[must_use]
pub fn assess(text: &str) -> Report {
    let words = words_of(text);
    let mut pieces = sentences(text);
    if pieces.is_empty() && !words.is_empty() {
        pieces = vec![text.to_string()];
    }
    let n_words = words.len();
    let n_sent = pieces.len().max(1);
    let n_chars: usize = words.iter().map(|w| scored_len(text, w)).sum();
    let n_syl: usize = words.iter().map(|w| syllables(w)).sum();

    let adverbs = words.iter().filter(|w| is_adverb(w)).count();
    let mut passives = 0usize;
    let mut hard = 0usize;
    let mut very_hard = 0usize;
    for sentence in &pieces {
        let n = words_of(sentence).len();
        if n >= VERY_HARD_WORDS {
            very_hard += 1;
        } else if n >= HARD_WORDS {
            hard += 1;
        }
        if has_be_verb(sentence) && has_participle(sentence) {
            passives += 1;
        }
    }

    let (grade, ease) = if n_words >= 8 {
        let w = n_words as f64;
        let s = n_sent as f64;
        let g = round2(4.71 * (n_chars as f64 / w) + 0.5 * (w / s) - 21.43);
        let e = if n_syl > 0 {
            Some(round2(
                206.835 - 1.015 * (w / s) - 84.6 * (n_syl as f64 / w),
            ))
        } else {
            None
        };
        (Some(g), e)
    } else {
        (None, None)
    };

    let adverb_ratio = if n_words > 0 {
        adverbs as f64 / n_words as f64
    } else {
        0.0
    };

    Report {
        words: n_words,
        sentences: if words.is_empty() { 0 } else { pieces.len() },
        grade,
        ease,
        adverbs,
        adverb_ratio: round3(adverb_ratio),
        passives,
        hard_sentences: hard,
        very_hard_sentences: very_hard,
    }
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

/// Refuse text too hard for the working core.
///
/// # Errors
///
/// Returns [`ProseError`] when an atom carries more than one claim, when a
/// sentence reads very hard, or when the grade or the adverb ratio is past its
/// ceiling.
pub fn refuse(text: &str, role: Role) -> Result<Report, ProseError> {
    let report = assess(text);
    if role == Role::Atom {
        if report.sentences > MAX_ATOM_SENTENCES {
            return Err(ProseError(format!(
                "atom has {} sentences; one claim is at most {MAX_ATOM_SENTENCES}",
                report.sentences
            )));
        }
        if report.very_hard_sentences > 0 {
            let (index, words) = sentences(text)
                .iter()
                .map(|s| words_of(s).len())
                .enumerate()
                .find(|(_, n)| *n >= VERY_HARD_WORDS)
                .unwrap_or((0, VERY_HARD_WORDS));
            return Err(ProseError(format!(
                "atom sentence {} is very hard to read: {words} words, the limit is {}; \
                 split it at a conjunction or drop a clause",
                index + 1,
                VERY_HARD_WORDS - 1
            )));
        }
    }
    if report.words >= 12 {
        if let Some(grade) = report.grade {
            if grade > MAX_GRADE {
                return Err(ProseError(format!(
                    "readability grade {grade} exceeds {MAX_GRADE}; \
                     use shorter sentences and shorter words"
                )));
            }
            if report.adverb_ratio > MAX_ADVERB_RATIO {
                return Err(ProseError(format!(
                    "adverb ratio {} exceeds {MAX_ADVERB_RATIO}; \
                     cut the -ly words or replace them with a number",
                    report.adverb_ratio
                )));
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A number is not the end of a sentence, and neither is a version.
    #[test]
    fn a_decimal_point_does_not_end_a_sentence() {
        assert_eq!(
            sentences("BM25+ beats BM25: 0.635 vs 0.615 hit@1 on turns. It is the default.").len(),
            2
        );
        assert_eq!(
            sentences("Cargo.lock pins the client. Bump it with cargo update.").len(),
            2
        );
        assert_eq!(
            sentences("packsetd listens on 127.0.0.1 only and never on localhost.").len(),
            1
        );
        assert_eq!(sentences("Really?! Yes. No").len(), 3);
        assert_eq!(
            sentences("the tracker has 0.9.3 now, but keep at it.").len(),
            1
        );
        assert_eq!(sentences("One. Two! Three?").len(), 3);
        assert_eq!(sentences("no terminator at all").len(), 1);
        assert_eq!(sentences("...").len(), 0);
    }

    #[test]
    fn an_abbreviation_or_citation_does_not_end_a_sentence() {
        let lesson = "Raw per-atom gyration radius does not separate reactive \
                      protons in TRPMD (Hu et al. 2021, doi 10.3389/fchem.2021.624937). \
                      A spread descriptor must subtract the reactant spread.";
        assert_eq!(sentences(lesson).len(), 2);
        assert_eq!(
            sentences("Use a ring, e.g. the instanton. It converges.").len(),
            2
        );
        assert_eq!(
            sentences("See Fig. 3 and Eq. 2 for the rate. Done.").len(),
            2
        );
        assert_eq!(sentences("J. O. Richardson derived it. It holds.").len(), 2);
        assert_eq!(sentences("It stopped. 3 tests failed.").len(), 2);
    }

    /// Two citations, paths, subscripts, products or sums (x*y, a+b) in a row
    /// stay two sentences.
    #[test]
    fn an_emphasis_mark_does_not_hold_a_sentence_open() {
        assert_eq!(
            sentences("Alpha beta a/b. Gamma delta c/d. Epsilon.").len(),
            3
        );
        assert_eq!(
            sentences("Ladha (doi:10.2307/2111584) says so. Kim (doi:10.48550/x) agrees. Done.")
                .len(),
            3
        );
        assert_eq!(
            sentences("Edit crates/a/b.rs first. Then run scripts/x.sh.")[0],
            "Edit crates/a/b.rs first."
        );
        assert_eq!(
            sentences("Voter j weighs w_j. So does pi_j. Done.").len(),
            3
        );
        assert_eq!(sentences("Use x*y. Then z*w. Done.").len(), 3);
        assert_eq!(
            sentences("Alpha beta a+b. Gamma delta c+d. Epsilon.").len(),
            3
        );
    }

    #[test]
    fn a_plain_claim_passes_as_an_atom() {
        let text = "Reviews open with a reproducibility check.";
        assert!(refuse(text, Role::Atom).is_ok());
    }

    #[test]
    fn one_claim_means_at_most_two_sentences() {
        let three = "One thing. Another thing. A third thing.";
        let err = refuse(three, Role::Atom).unwrap_err();
        assert!(err.0.contains("3 sentences"), "{err}");
        // The same text is fine in a card, which holds more than one claim.
        assert!(refuse(three, Role::File).is_ok());
    }

    #[test]
    fn a_short_text_is_not_graded() {
        let report = assess("Too short to grade.");
        assert_eq!(report.grade, None);
        assert_eq!(report.ease, None);
    }

    #[test]
    fn ly_words_that_are_not_adverbs_do_not_count() {
        assert_eq!(assess("only family apply early daily").adverbs, 0);
        assert_eq!(assess("quickly").adverbs, 1);
        assert_eq!(assess("very").adverbs, 1);
    }

    #[test]
    fn a_passive_is_a_be_verb_near_a_participle() {
        assert_eq!(assess("The header was parsed by the reader.").passives, 1);
        assert_eq!(assess("The reader parses the header.").passives, 0);
    }

    #[test]
    fn sentence_length_bands_are_counted() {
        let hard = "word ".repeat(HARD_WORDS) + ".";
        assert_eq!(assess(&hard).hard_sentences, 1);
        assert_eq!(assess(&hard).very_hard_sentences, 0);
        let very = "word ".repeat(VERY_HARD_WORDS) + ".";
        assert_eq!(assess(&very).very_hard_sentences, 1);
        assert_eq!(assess(&very).hard_sentences, 0);
    }

    #[test]
    fn a_very_hard_sentence_is_refused_in_an_atom_only() {
        let very = "word ".repeat(VERY_HARD_WORDS) + ".";
        assert!(refuse(&very, Role::Atom).is_err());
        // The refusal names the sentence, its length, the limit and the fix.
        let two = format!("Short first claim here. {very}");
        let err = refuse(&two, Role::Atom).unwrap_err().to_string();
        assert!(err.contains("sentence 2"), "{err}");
        assert!(err.contains(&format!("{VERY_HARD_WORDS} words")), "{err}");
        assert!(
            err.contains(&format!("limit is {}", VERY_HARD_WORDS - 1)),
            "{err}"
        );
        assert!(err.contains("split it"), "{err}");
        // In a card the length alone does not refuse; the grade decides.
        let report = assess(&very);
        assert_eq!(report.very_hard_sentences, 1);
    }

    #[test]
    fn a_stop_inside_a_name_or_code_span_is_not_a_sentence_end() {
        for text in [
            "Delete `.DS_Store` before you commit. The hook refuses it otherwise.",
            "Edit foo.rs and bar.rs, e.g. the parser, then rerun. v0.29.1 ships the fix.",
            "Run `cargo test -p a. b. c.` first. Then push.",
            "Read src/lib.rs i.e. the root module. Then edit it.",
        ] {
            assert_eq!(sentences(text).len(), 2, "{text}: {:?}", sentences(text));
            assert!(
                refuse(text, Role::Atom)
                    .map_or_else(|e| !e.to_string().contains("sentences"), |_| true),
                "{text}"
            );
        }
        let back = sentences("Delete `.DS_Store` now. Then e.g. this.");
        assert_eq!(back[0], "Delete `.DS_Store` now.", "{back:?}");
        assert!(back[1].contains("e.g."), "{back:?}");
        assert_eq!(sentences("One. Two. Three.").len(), 3);
    }

    #[test]
    fn empty_text_has_no_sentences() {
        let report = assess("");
        assert_eq!(report.words, 0);
        assert_eq!(report.sentences, 0);
        assert_eq!(report.adverb_ratio, 0.0);
    }

    /// A plain technical sentence passes: names are read as units.
    #[test]
    fn a_plain_technical_sentence_passes() {
        for lesson in [
            "gvfsd reads XDG_DATA_HOME when it starts, so set the scratch folder before starting it.",
            "packset-embed links onnxruntime from ORT_LIB_LOCATION, so export it before cargo build.",
            "systemd-tmpfiles cleans /var/tmp/packset-scratch nightly; keep long-running indexes under ~/.local/state/packset.",
            "ljos-policyd refuses `rm -rf ~/.cache/huggingface` because the cache sits under the home directory.",
        ] {
            assert!(refuse(lesson, Role::Atom).is_ok(), "{lesson}: {:?}", refuse(lesson, Role::Atom));
        }
    }

    /// Long ordinary words still count in full.
    #[test]
    fn long_plain_words_still_count() {
        assert_eq!(
            scored_len("internationalization", "internationalization"),
            20
        );
        let t = "use XDG_DATA_HOME and packset-embed";
        let w = words_of(t);
        assert!(w.iter().all(|x| scored_len(t, x) <= NAME_LETTERS));
        let hard = "Notwithstanding institutional considerations, comprehensive \
                    interdepartmental standardization necessitates extraordinarily \
                    meticulous documentation procedures throughout organizational \
                    administrative infrastructure modernization.";
        assert!(refuse(hard, Role::Atom).is_err());
    }

    #[test]
    fn syllable_counting_drops_a_silent_e() {
        assert_eq!(syllables("make"), 1);
        assert_eq!(syllables("the"), 1);
        assert_eq!(syllables("reproducibility"), 7);
        assert_eq!(syllables(""), 1);
    }

    /// `vs.` is rewritten so the stop is U+2024, three bytes. A note with
    /// that abbreviation near char 160 then asks for byte 161, inside
    /// the new character.
    #[test]
    fn a_vs_abbreviation_near_char_160_stays_one_sentence() {
        let note = format!("{} vs. lease {}", "n".repeat(157), "m".repeat(98));
        assert_eq!(note.chars().count(), 266);
        assert_eq!(note.find("vs."), Some(158));
        let got = sentences(&note);
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].contains("vs. lease"), "{got:?}");
    }

    /// é, an emoji and a CJK ideograph on the byte the cut asks for.
    /// `İ` lowercases to a longer string, so the index of `vs.` falls
    /// inside the following character instead of on a boundary.
    #[test]
    fn a_multibyte_char_on_the_cut_stays_whole() {
        let cases = [
            (format!("{} vs. é lease", "İ".repeat(2)), 'é', true),
            (format!("{} vs. 😀 lease", "İ".repeat(4)), '😀', true),
            (format!("{} vs. 字 lease", "İ".repeat(2)), '字', false),
        ];
        for (note, mark, one_sentence) in cases {
            let got = sentences(&note);
            let flat = got.join(" ");
            assert!(flat.contains(mark), "{mark}: {got:?}");
            assert!(flat.contains("vs."), "{mark}: {got:?}");
            assert!(!flat.contains('\u{2024}'), "{mark}: {got:?}");
            if one_sentence {
                assert_eq!(got.len(), 1, "{mark}: {got:?}");
            }
        }
    }
}
