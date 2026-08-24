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

#[cfg(test)]
mod tests {
    use super::*;

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
