//! Temporal decay. Evergreen sources are exempt.
//!
//! Decay is a voter, not a second store.

pub fn temporal_decay(source: &str, age_days: f64, half_life_days: Option<f64>) -> f64 {
    if matches!(source, "global" | "workspace" | "user" | "evergreen") {
        return 1.0;
    }
    let Some(half) = half_life_days else {
        return 1.0;
    };
    if half <= 0.0 {
        return 1.0;
    }
    let lambda = std::f64::consts::LN_2 / half;
    (-lambda * age_days.max(0.0)).exp()
}

/// FSRS-4.5 forgetting curve: the probability a claim reviewed `elapsed_days`
/// ago with stability `stability_days` is still recalled,
/// `(1 + 19/81 * t/S)^(-1/2)`, so `R(S) = 0.9`
/// (doi:10.1145/3534678.3539081). Power-law forgetting after Wixted and
/// Ebbesen (doi:10.1111/j.1467-9280.1991.tb00175.x).
pub fn retrievability(elapsed_days: f64, stability_days: f64) -> f64 {
    let stability = if stability_days > 0.0 {
        stability_days
    } else {
        1.0
    };
    (1.0 + 19.0 / 81.0 * elapsed_days.max(0.0) / stability).powf(-0.5)
}

/// The least a retrievability weight scales a score by. A forgotten claim is
/// still found when nothing else answers.
pub const RETRIEVABILITY_FLOOR: f64 = 0.25;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retrievability_is_nine_tenths_at_one_stability() {
        assert!((retrievability(0.0, 3.0) - 1.0).abs() < 1e-12);
        assert!((retrievability(3.0, 3.0) - 0.9).abs() < 1e-9);
        let far = retrievability(30.0, 3.0);
        assert!((far - 0.5467).abs() < 1e-3, "{far}");
        assert!(retrievability(1.0, 1.0) > retrievability(2.0, 1.0));
        assert!((retrievability(1.0, 0.0) - 0.9).abs() < 1e-9);
    }

    #[test]
    fn evergreen_is_one() {
        assert_eq!(temporal_decay("global", 400.0, Some(7.0)), 1.0);
    }

    #[test]
    fn half_life_halves() {
        let d = temporal_decay("session", 7.0, Some(7.0));
        assert!((d - 0.5).abs() < 1e-9);
    }
}
