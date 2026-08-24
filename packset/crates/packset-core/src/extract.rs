//! Explicit keep-lines and hard filters. Deterministic; no model on write.

/// Return (kind, claim) for an explicit keep directive.
pub fn claim_from_user(text: &str) -> Option<(&'static str, String)> {
    let last = text.trim().lines().last()?.trim();
    if last.ends_with('?') {
        return None;
    }
    let lower = last.to_ascii_lowercase();
    let (kind, rest) = if let Some(r) = strip_prefix_ci(&lower, last, "remember that") {
        ("lesson", r)
    } else if let Some(r) = strip_prefix_ci(&lower, last, "remember") {
        ("lesson", r)
    } else if let Some(r) = strip_prefix_ci(&lower, last, "note that") {
        ("lesson", r)
    } else if let Some(r) = strip_prefix_ci(&lower, last, "from now on") {
        ("habit", r)
    } else if let Some(r) = strip_prefix_ci(&lower, last, "prefer") {
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
    fn listing_is_dump() {
        let blob = (0..8)
            .map(|i| format!("- file{i}.rs"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(is_tool_dump(&blob));
        assert!(!is_tool_dump("Remember: keep the habit."));
    }
}
