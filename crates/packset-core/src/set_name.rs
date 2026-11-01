//! Set names: a scope on the seat store.
//!
//! A set is the same shape as a workspace pack, so its name has to be safe as a
//! directory component and as a value compared against `atom.set`. One grammar
//! decides both.

/// A legal set id, lowercased.
///
/// # Errors
///
/// Returns a message when the name is not a lowercase letter followed by up to
/// thirty-one lowercase letters, digits or hyphens.
pub fn check(name: &str) -> Result<String, String> {
    let raw = name.trim().to_ascii_lowercase();
    let mut chars = raw.chars();
    let legal = match chars.next() {
        Some(first) if first.is_ascii_lowercase() => {
            raw.len() <= 32
                && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        }
        _ => false,
    };
    if legal {
        Ok(raw)
    } else {
        Err(format!("bad set name: {}", crate::record::quoted(name)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_name_is_lowercased() {
        assert_eq!(check("Review").unwrap(), "review");
        assert_eq!(check("  joss-reviews  ").unwrap(), "joss-reviews");
        assert_eq!(check("a").unwrap(), "a");
    }

    #[test]
    fn it_must_open_with_a_letter() {
        assert!(check("1review").is_err());
        assert!(check("-review").is_err());
        assert!(check("").is_err());
    }

    #[test]
    fn a_separator_that_would_escape_the_directory_is_refused() {
        assert!(check("../etc").is_err());
        assert!(check("a/b").is_err());
        assert!(check("a b").is_err());
        assert!(check("a_b").is_err());
    }

    #[test]
    fn thirty_two_characters_fit_and_thirty_three_do_not() {
        assert!(check(&"a".repeat(32)).is_ok());
        assert!(check(&"a".repeat(33)).is_err());
    }
}
