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

/// Sentences, split at a `.`, `!` or `?` that ends a run of terminators and
/// is followed by whitespace or the end of the text. A full stop inside a
/// token (`0.9.3`, `127.0.0.1`, `Cargo.lock`) is not a boundary.
fn sentences_of(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let terminator = |b: u8| matches!(b, b'.' | b'!' | b'?');
    let mut out = Vec::new();
    let mut start = 0usize;
    for (at, &b) in bytes.iter().enumerate() {
        if !terminator(b) {
            continue;
        }
        let ends = match bytes.get(at + 1) {
            None => true,
            Some(&next) => next.is_ascii_whitespace(),
        };
        if !ends {
            continue;
        }
        out.push(&text[start..at]);
        start = at + 1;
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out.into_iter()
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
    let mut sentences: Vec<&str> = sentences_of(text);
    if sentences.is_empty() && !words.is_empty() {
        sentences = vec![text];
    }
    let n_words = words.len();
    let n_sent = sentences.len().max(1);
    let n_chars: usize = words.iter().map(|w| w.len()).sum();
    let n_syl: usize = words.iter().map(|w| syllables(w)).sum();

    let adverbs = words.iter().filter(|w| is_adverb(w)).count();
    let mut passives = 0usize;
    let mut hard = 0usize;
    let mut very_hard = 0usize;
    for sentence in &sentences {
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
        sentences: if words.is_empty() { 0 } else { sentences.len() },
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
            let (index, words) = sentences_of(text)
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
            sentences_of("BM25+ beats BM25: 0.635 vs 0.615 hit@1 on turns. It is the default.")
                .len(),
            2
        );
        assert_eq!(
            sentences_of("Cargo.lock pins the client. Bump it with cargo update.").len(),
            2
        );
        assert_eq!(
            sentences_of("packsetd listens on 127.0.0.1 only and never on localhost.").len(),
            1
        );
        assert_eq!(sentences_of("Really?! Yes. No").len(), 3);
        assert_eq!(
            sentences_of("the tracker has 0.9.3 now, but keep at it.").len(),
            1
        );
        assert_eq!(sentences_of("One. Two! Three?").len(), 3);
        assert_eq!(sentences_of("no terminator at all").len(), 1);
        assert_eq!(sentences_of("...").len(), 0);
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
    fn empty_text_has_no_sentences() {
        let report = assess("");
        assert_eq!(report.words, 0);
        assert_eq!(report.sentences, 0);
        assert_eq!(report.adverb_ratio, 0.0);
    }

    #[test]
    fn syllable_counting_drops_a_silent_e() {
        assert_eq!(syllables("make"), 1);
        assert_eq!(syllables("the"), 1);
        assert_eq!(syllables("reproducibility"), 7);
        assert_eq!(syllables(""), 1);
    }
}
