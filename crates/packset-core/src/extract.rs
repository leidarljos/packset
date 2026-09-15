//! Explicit keep-lines and hard filters. Deterministic; no model on write.

/// A write the MemoryAgentBench seat protocol will ingest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeatWrite {
    Lesson(String),
    Preference(String),
    Accept(String),
}

/// Remember / Prefer / Accept only. Raw context, habits, and dumps refuse.
#[must_use]
pub fn admit_seat_write(text: &str) -> Option<SeatWrite> {
    let last = text.trim().lines().last()?.trim();
    let lower = last.to_ascii_lowercase();
    if let Some(rest) = strip_prefix_ci(&lower, last, "accept:") {
        let id = rest.trim().trim_matches(|c: char| c == ':' || c.is_whitespace());
        if id.len() >= 4 {
            return Some(SeatWrite::Accept(id.to_string()));
        }
        return None;
    }
    match claim_from_user(text)? {
        ("lesson", claim) => Some(SeatWrite::Lesson(claim)),
        ("preference", claim) => Some(SeatWrite::Preference(claim)),
        _ => None,
    }
}

/// Return (kind, claim) for an explicit keep directive.
pub fn claim_from_user(text: &str) -> Option<(&'static str, String)> {
    let last = text.trim().lines().last()?.trim();
    if last.ends_with('?') {
        return None;
    }
    let lower = last.to_ascii_lowercase();
    // A dispatch over three prefixes, not an early return: `?` would collapse
    // the first arm and leave the other two unreachable.
    #[allow(clippy::question_mark)]
    let (kind, rest) = if let Some(r) = strip_prefix_ci(&lower, last, "remember:") {
        ("lesson", r)
    } else if let Some(r) = strip_prefix_ci(&lower, last, "from now on:") {
        ("habit", r)
    } else if let Some(r) = strip_prefix_ci(&lower, last, "prefer:") {
        ("preference", r)
    } else {
        return None;
    };
    let claim = rest
        .trim_start_matches([':', ' ', ','])
        .trim()
        .trim_end_matches(['.', ',', ';', ':']);
    if claim.len() < 8 {
        return None;
    }
    Some((kind, claim.to_string()))
}

fn strip_prefix_ci<'a>(lower: &str, orig: &'a str, prefix: &str) -> Option<&'a str> {
    if lower.starts_with(prefix) {
        Some(&orig[prefix.len()..])
    } else {
        None
    }
}

/// Tool stdout, listings, and fetched bodies are not atoms.
pub fn is_tool_dump(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() {
        return true;
    }
    let lower = t.to_ascii_lowercase();
    if lower.contains("```") && (lower.contains("stdout") || lower.contains("stderr")) {
        return true;
    }
    if lower.starts_with("<!doctype") || lower.starts_with("<html") {
        return true;
    }
    let lines: Vec<&str> = t.lines().collect();
    if lines.len() >= 8
        && lines
            .iter()
            .filter(|l| l.starts_with('-') || l.starts_with("drwx") || l.starts_with("total "))
            .count()
            >= 6
    {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remember_line() {
        let (k, c) = claim_from_user("Remember: always pin the review set").unwrap();
        assert_eq!(k, "lesson");
        assert!(c.contains("pin the review set"));
    }

    #[test]
    fn note_that_and_remember_that_are_not_claims() {
        assert!(claim_from_user("Note that the test failed on line 12").is_none());
        assert!(claim_from_user("Remember that: pin the review set").is_none());
        assert!(claim_from_user("Prefer conventional commits").is_none());
        assert!(claim_from_user("From now on, file a ticket first").is_none());
    }

    #[test]
    fn seat_write_is_remember_prefer_or_accept() {
        assert!(matches!(
            admit_seat_write("Remember: pin the review set"),
            Some(SeatWrite::Lesson(_))
        ));
        assert!(matches!(
            admit_seat_write("Prefer: conventional commits always"),
            Some(SeatWrite::Preference(_))
        ));
        assert!(matches!(
            admit_seat_write("Accept: ab12cd"),
            Some(SeatWrite::Accept(id)) if id == "ab12cd"
        ));
        assert!(admit_seat_write("From now on: file a ticket first").is_none());
        assert!(admit_seat_write("The user lives in Berlin and likes tea.").is_none());
    }

    #[test]
    fn listing_is_dump() {
        let blob = (0..8)
            .map(|i| format!("- file{i}.rs"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(is_tool_dump(&blob));
        assert!(!is_tool_dump("Remember: keep the habit."));
    }
}
