//! `inside.atom/v1` predicates. No I/O.

/// Schema name for a pack atom.
pub const SCHEMA: &str = "inside.atom/v1";

/// Live at `at`: not a tombstone, `valid_from` missing or already started,
/// and `valid_to` missing or still open. The window is half-open `[from, to)`.
pub fn is_live_at(
    tombstone: bool,
    valid_from: Option<&str>,
    valid_to: Option<&str>,
    at: &str,
) -> bool {
    if tombstone {
        return false;
    }
    if let Some(from) = valid_from {
        if !from.is_empty() && from > at {
            return false;
        }
    }
    match valid_to {
        None | Some("") => true,
        Some(until) => until > at,
    }
}

/// Live now: the `valid_to` half of [`is_live_at`], with no start bound.
pub fn is_live(tombstone: bool, valid_to: Option<&str>, now: &str) -> bool {
    is_live_at(tombstone, None, valid_to, now)
}

/// Review clock. Missing `due_at` is not due. Independent of `valid_to`.
pub fn is_due(due_at: Option<&str>, now: &str) -> bool {
    match due_at {
        None | Some("") => false,
        Some(due) => due <= now,
    }
}

/// Prefixes a deed accession can open with.
///
/// deedar mints `deed-<kind>-<slug>` and answers `get` for a `sha256:` of the
/// canonical deed or of one product path. Those two forms are the whole
/// vocabulary, and the tracker refuses anything else where a citation is
/// written, so a pack that accepts a typo leaves a citation nothing resolves.
pub const ACCESSION_PREFIXES: &[&str] = &["deed-", "sha256:"];

/// Why an entity that opens like a deed accession is not one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntityRefusal {
    /// The entity is empty or only whitespace.
    Empty,
    /// A bare `deed-` or `sha256:` that names nothing.
    BarePrefix,
    /// A separator that would split the entity into two.
    Separator(char),
}

/// Check one entity, returning it trimmed.
///
/// An entity is otherwise a free-form name, so only the accession shape is
/// checked: the pack cannot ask whether a deed exists, and does not try to.
/// What it can catch is the citation nothing will ever resolve, at the moment
/// somebody writes it rather than later from `deedar evidence -`.
///
/// # Errors
///
/// Returns the reason when the entity opens with an accession prefix and is
/// not a well-formed accession.
pub fn check_entity(value: &str) -> Result<&str, EntityRefusal> {
    let text = value.trim();
    if text.is_empty() {
        return Err(EntityRefusal::Empty);
    }
    let Some(prefix) = ACCESSION_PREFIXES.iter().find(|p| text.starts_with(**p)) else {
        return Ok(text);
    };
    let rest = &text[prefix.len()..];
    if rest.is_empty() {
        return Err(EntityRefusal::BarePrefix);
    }
    // The search projection joins entities with a space and the documented
    // sweep splits them on a comma, so a separator inside one silently becomes
    // two entities, neither of which resolves.
    if let Some(bad) = rest.chars().find(|c| c.is_whitespace() || *c == ',') {
        return Err(EntityRefusal::Separator(bad));
    }
    Ok(text)
}

/// Whether an entity names a deed rather than a free-form thing.
///
/// The reader's half of [`check_entity`]: a sweep over a workspace keeps the
/// entities that are citations and ignores the rest.
#[must_use]
pub fn is_accession(value: &str) -> bool {
    let text = value.trim();
    ACCESSION_PREFIXES
        .iter()
        .any(|p| text.strip_prefix(*p).is_some_and(|rest| !rest.is_empty()))
        && !text.contains(|c: char| c.is_whitespace() || c == ',')
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
    fn live_at_reads_both_ends_of_the_window() {
        let at = "2026-08-20T00:00:00Z";
        assert!(is_live_at(false, None, None, at));
        assert!(is_live_at(
            false,
            Some("2026-01-01T00:00:00Z"),
            Some("2099-01-01T00:00:00Z"),
            at
        ));
        assert!(!is_live_at(false, Some("2026-09-01T00:00:00Z"), None, at));
        assert!(!is_live_at(
            false,
            Some("2026-01-01T00:00:00Z"),
            Some("2026-08-20T00:00:00Z"),
            at
        ));
        assert!(!is_live_at(true, Some("2026-01-01T00:00:00Z"), None, at));
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
    fn a_free_form_entity_passes_untouched() {
        assert_eq!(check_entity("  JOSS  "), Ok("JOSS"));
        assert_eq!(check_entity("a whole phrase"), Ok("a whole phrase"));
        assert!(!is_accession("JOSS"));
    }

    #[test]
    fn a_well_formed_accession_is_kept() {
        assert_eq!(check_entity("deed-patch-overlay"), Ok("deed-patch-overlay"));
        assert!(is_accession("deed-patch-overlay"));
        assert!(is_accession("sha256:aabbccdd"));
    }

    #[test]
    fn a_bare_prefix_names_no_deed() {
        assert_eq!(check_entity("deed-"), Err(EntityRefusal::BarePrefix));
        assert_eq!(check_entity("sha256:"), Err(EntityRefusal::BarePrefix));
        assert!(!is_accession("deed-"));
    }

    #[test]
    fn a_separator_inside_an_accession_would_split_it() {
        assert_eq!(
            check_entity("deed-patch overlay"),
            Err(EntityRefusal::Separator(' '))
        );
        assert_eq!(check_entity("deed-a,b"), Err(EntityRefusal::Separator(',')));
        assert!(!is_accession("deed-a,b"));
    }

    #[test]
    fn the_shape_is_checked_and_the_store_is_not() {
        // The pack cannot ask whether a deed exists: a plausible accession for
        // a deed nobody minted is accepted here and caught later by
        // `deedar evidence -`.
        assert_eq!(
            check_entity("deed-patch-nobody-minted-this"),
            Ok("deed-patch-nobody-minted-this")
        );
    }

    #[test]
    fn an_empty_entity_is_refused() {
        assert_eq!(check_entity("   "), Err(EntityRefusal::Empty));
    }

    #[test]
    fn jaccard_overlap() {
        let v = entity_jaccard(["grok", "pack"], ["pack", "seat"]);
        assert!((v - 1.0 / 3.0).abs() < 1e-9);
    }
}
