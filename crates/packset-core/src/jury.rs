//! Fail-closed jury and May's sign, scored with Host Borda.
//!
//! A missing accept is not a yes and not seen. Commit is `yes >= k` and
//! `seen >= n`. Rank order is [`crate::borda::borda_merge`]: score = k - position.

use crate::borda::{borda_merge, Ballot};

/// Five product-shaped rankings. Accept is separate from rank.
pub fn fixture_rankings() -> Vec<Ballot<&'static str>> {
    vec![
        vec!["adapter", "tests", "docs"],
        vec!["adapter", "docs", "ci"],
        vec!["tests", "adapter", "docs"],
        vec!["adapter", "tests"],
        vec!["docs", "adapter", "tests"],
    ]
}

/// `(yes, seen, commit)`.
pub fn jury(accepts: &[Option<bool>], k: usize, n: usize) -> (usize, usize, bool) {
    let yes = accepts.iter().filter(|a| **a == Some(true)).count();
    let seen = accepts.iter().filter(|a| a.is_some()).count();
    (yes, seen, yes >= k && seen >= n)
}

/// `sign(yes - no)`.
pub fn majority_sign(yes: i64, no: i64) -> i32 {
    let delta = yes - no;
    if delta > 0 {
        1
    } else if delta < 0 {
        -1
    } else {
        0
    }
}

/// Smallest yes count on `n` Boolean votes with `yes - 2*no > 0`.
pub fn may_two_thirds_min_yes(n: i64) -> i64 {
    for yes in 0..=n {
        let no = n - yes;
        if yes - 2 * no > 0 {
            return yes;
        }
    }
    n
}

fn boolean_profile(yes: usize, no: usize) -> Vec<Option<bool>> {
    let mut profile = Vec::with_capacity(yes + no);
    profile.extend(std::iter::repeat(Some(true)).take(yes));
    profile.extend(std::iter::repeat(Some(false)).take(no));
    profile
}

/// May's antecedent is a profile whose sign is 0 or +1 and that does not
/// commit. One more yes must commit. A profile that already commits is skipped.
pub fn positively_responsive(k: usize, n: usize) -> bool {
    for yes in 0..=n {
        let no = n - yes;
        if !matches!(majority_sign(yes as i64, no as i64), 0 | 1) {
            continue;
        }
        let profile = boolean_profile(yes, no);
        if jury(&profile, k, n).2 {
            continue;
        }
        if no == 0 {
            return false;
        }
        let stepped = boolean_profile(yes + 1, no - 1);
        if !jury(&stepped, k, n).2 {
            return false;
        }
    }
    true
}

fn word(commit: bool) -> &'static str {
    if commit {
        "commit"
    } else {
        "hung"
    }
}

fn py_bool(value: bool) -> &'static str {
    if value {
        "True"
    } else {
        "False"
    }
}

fn two_thirds_positive(yes: i64, no: i64) -> bool {
    yes - 2 * no > 0
}

/// Lines the manuscript includes. Rank order on the fixture is Host Borda.
pub fn may_report() -> String {
    let five_seven = majority_sign(5, 7);
    let six = jury(&boolean_profile(6, 6), 7, 12).2;
    let seven_five = jury(&boolean_profile(7, 5), 7, 12).2;
    let holds = positively_responsive(7, 12);
    let nine = may_two_thirds_min_yes(12);
    let eight = jury(&boolean_profile(8, 4), 7, 12).2;
    let twelve = jury(&boolean_profile(7, 5), 7, 12).2;
    let mut eleven_votes = boolean_profile(7, 4);
    eleven_votes.push(None);
    let eleven = jury(&eleven_votes, 7, 12).2;
    let sign_gap = 7 - 2 * 5;
    let gap = if twelve && !eleven && sign_gap <= 0 {
        "gap-cause seen>=n not sign(N(1)-2N(-1))"
    } else {
        "gap-cause mismatch"
    };
    let slate = borda_merge(&fixture_rankings(), 3);
    let mut lines = vec![
        format!("majority-sign 5 7 {five_seven}"),
        format!("six-yes-six-no {}", word(six)),
        format!("seven-yes-five-no {}", word(seven_five)),
        format!(
            "positive-responsiveness-k7-n12 {}",
            if holds { "holds" } else { "fails" }
        ),
        format!("may-two-thirds-min-yes-n12 {nine}"),
        format!(
            "quota-7-commits-seven-yes {} two-thirds-positive {}",
            py_bool(seven_five),
            py_bool(two_thirds_positive(7, 5))
        ),
        format!(
            "quota-7-commits-eight-yes {} two-thirds-positive {}",
            py_bool(eight),
            py_bool(two_thirds_positive(8, 4))
        ),
        format!("seven-of-twelve {}", word(twelve)),
        format!("seven-of-eleven {}", word(eleven)),
        gap.to_string(),
        format!("host-borda {}", slate.join(" ")),
    ];
    lines.push(String::new());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn may_lines_from_shipped_functions() {
        let report = may_report();
        print!("{report}");
        let slate = borda_merge(&fixture_rankings(), 3);
        assert!(
            report.contains(&format!("host-borda {}", slate.join(" "))),
            "{report}"
        );
        for line in [
            "majority-sign 5 7 -1",
            "six-yes-six-no hung",
            "seven-yes-five-no commit",
            "positive-responsiveness-k7-n12 holds",
            "may-two-thirds-min-yes-n12 9",
            "seven-of-twelve commit",
            "seven-of-eleven hung",
            "gap-cause seen>=n not sign(N(1)-2N(-1))",
        ] {
            assert!(report.contains(line), "missing {line}\n{report}");
        }
    }
}
