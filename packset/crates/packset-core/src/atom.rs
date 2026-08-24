//! `inside.atom/v1` predicates. No I/O.

/// Schema name for a pack atom.
pub const SCHEMA: &str = "inside.atom/v1";

/// Live = not a tombstone and `valid_to` is missing or still open.
pub fn is_live(tombstone: bool, valid_to: Option<&str>, now: &str) -> bool {
    if tombstone {
        return false;
    }
    match valid_to {
        None | Some("") => true,
        Some(until) => until > now,
    }
}

/// Review clock. Missing `due_at` is not due. Independent of `valid_to`.
pub fn is_due(due_at: Option<&str>, now: &str) -> bool {
    match due_at {
        None | Some("") => false,
        Some(due) => due <= now,
    }
}

/// Jaccard on entity sets. Empty intersection is 0.
pub fn entity_jaccard<'a, I, J>(left: I, right: J) -> f64
where
    I: IntoIterator<Item = &'a str>,
    J: IntoIterator<Item = &'a str>,
{
    use std::collections::HashSet;
    let a: HashSet<&str> = left.into_iter().collect();
    let b: HashSet<&str> = right.into_iter().collect();
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(&b).count() as f64;
    let union = a.union(&b).count() as f64;
    if union == 0.0 {
        0.0
    } else {
        inter / union
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_open_valid_to() {
        assert!(is_live(false, None, "2026-08-20T00:00:00Z"));
        assert!(is_live(
            false,
            Some("2099-01-01T00:00:00Z"),
            "2026-08-20T00:00:00Z"
        ));
        assert!(!is_live(
            false,
            Some("2000-01-01T00:00:00Z"),
            "2026-08-20T00:00:00Z"
        ));
        assert!(!is_live(true, None, "2026-08-20T00:00:00Z"));
    }

    #[test]
    fn due_independent_of_live() {
        let now = "2026-08-20T00:00:00Z";
        assert!(!is_due(None, now));
        assert!(!is_due(Some(""), now));
        assert!(!is_due(Some("2026-08-21T00:00:00Z"), now));
        assert!(is_due(Some("2026-08-19T00:00:00Z"), now));
        assert!(is_due(Some(now), now));
        assert!(is_live(false, Some("2099-01-01T00:00:00Z"), now));
        assert!(is_due(Some("2026-08-19T00:00:00Z"), now));
        assert!(!is_live(false, Some("2000-01-01T00:00:00Z"), now));
        assert!(is_due(Some("2026-08-19T00:00:00Z"), now));
    }

    #[test]
    fn jaccard_overlap() {
        let v = entity_jaccard(["grok", "pack"], ["pack", "seat"]);
        assert!((v - 1.0 / 3.0).abs() < 1e-9);
    }
}
