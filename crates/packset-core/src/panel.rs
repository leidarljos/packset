//! Named fuse then diversify then decay. Host picks the sequence.
//!
//! Default fuse is CombMNZ. Default diversify is MMR. Default decay
//! is off. Names come from `PACKSET_FUSE`, `PACKSET_DIVERSIFY`,
//! and `PACKSET_DECAY`, or from packsetd flags. Clients do not
//! choose this. Unknown names fail closed. Later voters add a
//! variant and a match arm; they do not change the default.

use std::env;
use std::hash::Hash;

use crate::borda::{borda_merge, Ballot};
use crate::comb::{combmnz_merge, combsum_merge, ScoredBallot};
use crate::copeland::copeland_merge;
use crate::decay::temporal_decay;
use crate::dowdall::dowdall_merge;
use crate::dpp::dpp_rerank;
use crate::kemeny::kemeny_merge;
use crate::mmr::{mmr_rerank, Ranked};
use crate::rrf::rrf_merge;
use crate::schulze::schulze_merge;
use crate::tideman::ranked_pairs_merge;

/// Half-life used when decay is on. Matches the host recency scale.
const DECAY_HALF_LIFE_DAYS: f64 = 14.0;

/// Fuse slot. Only implemented names parse, and each name runs the voter it
/// names.
///
/// The default is CombMNZ, measured rather than assumed. Over the ballots this
/// crate actually fuses, against 1536 questions with labelled evidence, it
/// leads Borda by 1.8 points of session hit@1 and 0.010 nDCG@5, and leads at
/// turn granularity too. Reciprocal rank fusion led on one pair of two ballots
/// and trailed Borda on the three the seat ships, which is why the default
/// comes from sweeping the shipped ballots rather than the strongest pair
/// available.
///
/// The two score fusions read the scores rather than the positions, so a
/// caller fusing lists whose scores are not comparable wants a rank voter.
/// Min-max normalisation per list is what makes them comparable enough.
///
/// Ranked pairs says little about two ballots. It decides a pair by which
/// majority prefers it, and with two voters a disagreement is one against one,
/// so few victories lock and the order falls back to the first ballot. It is
/// here for a panel of three or more, which is what it was designed for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum Fuse {
    Borda,
    Rrf,
    CombSum,
    #[default]
    CombMnz,
    Dowdall,
    Kemeny,
    Schulze,
    Copeland,
    Tideman,
}

/// Diversify slot. `None` keeps fuse order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum Diversify {
    #[default]
    Mmr,
    Dpp,
    None,
}

/// Decay slot. Off leaves fuse scores unchanged; On is a half-life on age;
/// Fsrs is the review model's retrievability, from stability and the time
/// since the last review.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum Decay {
    Off,
    On,
    /// The default: retrievability from the review clock, floored, cards
    /// exempt. Measured on a longitudinal corpus at 0.947 against 0.230 for
    /// lexical order; on a corpus with no review history it changes nothing.
    #[default]
    Fsrs,
}

/// Host sequence. Clients do not choose this.
///
/// Derived from each slot's own default rather than written out. A second copy
/// of the defaults is a second place to change them, and the two disagreeing
/// means `Panel::default()` and an unset environment name different panels
/// while both call themselves the default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Panel {
    pub fuse: Fuse,
    pub diversify: Diversify,
    pub decay: Decay,
}

/// A name that is not an implemented voter.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UnknownVoter {
    #[error("unknown fuse `{0}`")]
    Fuse(String),
    #[error("unknown diversify `{0}`")]
    Diversify(String),
    #[error("unknown decay `{0}`")]
    Decay(String),
    #[error("not implemented fuse `{0}`")]
    FuseNotImplemented(String),
    #[error("not implemented diversify `{0}`")]
    DiversifyNotImplemented(String),
}

fn env_or<'a>(
    raw: Option<&'a str>,
    default: &'a str,
    empty: fn(String) -> UnknownVoter,
) -> Result<&'a str, UnknownVoter> {
    match raw {
        None => Ok(default),
        Some("") => Err(empty(String::new())),
        Some(name) => Ok(name),
    }
}

impl Fuse {
    pub fn parse(name: &str) -> Result<Self, UnknownVoter> {
        match name {
            "borda" => Ok(Self::Borda),
            "rrf" => Ok(Self::Rrf),
            "combsum" => Ok(Self::CombSum),
            "combmnz" => Ok(Self::CombMnz),
            "dowdall" => Ok(Self::Dowdall),
            "kemeny" => Ok(Self::Kemeny),
            "schulze" => Ok(Self::Schulze),
            "copeland" => Ok(Self::Copeland),
            "tideman" => Ok(Self::Tideman),
            other => Err(UnknownVoter::Fuse(other.to_string())),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Borda => "borda",
            Self::Rrf => "rrf",
            Self::CombSum => "combsum",
            Self::CombMnz => "combmnz",
            Self::Dowdall => "dowdall",
            Self::Kemeny => "kemeny",
            Self::Schulze => "schulze",
            Self::Copeland => "copeland",
            Self::Tideman => "tideman",
        }
    }
}

impl Diversify {
    pub fn parse(name: &str) -> Result<Self, UnknownVoter> {
        match name {
            "mmr" => Ok(Self::Mmr),
            "dpp" => Ok(Self::Dpp),
            "none" => Ok(Self::None),
            other => Err(UnknownVoter::Diversify(other.to_string())),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mmr => "mmr",
            Self::Dpp => "dpp",
            Self::None => "none",
        }
    }
}

impl Decay {
    pub fn parse(name: &str) -> Result<Self, UnknownVoter> {
        match name {
            "off" => Ok(Self::Off),
            "on" => Ok(Self::On),
            "fsrs" | "retrievability" => Ok(Self::Fsrs),
            other => Err(UnknownVoter::Decay(other.to_string())),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::On => "on",
            Self::Fsrs => "fsrs",
        }
    }
}

impl Panel {
    /// A fuse and a diversifier with the shipped decay slot.
    pub fn parse(fuse: &str, diversify: &str) -> Result<Self, UnknownVoter> {
        Self::named(fuse, diversify, Decay::default().as_str())
    }

    pub fn named(fuse: &str, diversify: &str, decay: &str) -> Result<Self, UnknownVoter> {
        Ok(Self {
            fuse: Fuse::parse(fuse)?,
            diversify: Diversify::parse(diversify)?,
            decay: Decay::parse(decay)?,
        })
    }

    /// Read `PACKSET_FUSE`, `PACKSET_DIVERSIFY`, `PACKSET_DECAY`.
    /// Unset keys take the default. Empty values fail closed.
    pub fn from_env() -> Result<Self, UnknownVoter> {
        let fuse = env::var("PACKSET_FUSE").ok();
        let diversify = env::var("PACKSET_DIVERSIFY").ok();
        let decay = env::var("PACKSET_DECAY").ok();
        Self::from_env_vars(fuse.as_deref(), diversify.as_deref(), decay.as_deref())
    }

    pub fn from_env_vars(
        fuse: Option<&str>,
        diversify: Option<&str>,
        decay: Option<&str>,
    ) -> Result<Self, UnknownVoter> {
        Self::named(
            env_or(fuse, "combmnz", UnknownVoter::Fuse)?,
            env_or(diversify, "mmr", UnknownVoter::Diversify)?,
            env_or(decay, "fsrs", UnknownVoter::Decay)?,
        )
    }

    /// The factor a fused score is scaled by. `age_days` is the time since
    /// the last review (or the write); `stability_days` is the review model's,
    /// read only by `Fsrs`. Evergreen sources stay at one.
    pub fn decay_weight(&self, source: &str, age_days: f64, stability_days: f64) -> f64 {
        match self.decay {
            Decay::Off => 1.0,
            Decay::On => temporal_decay(source, age_days, Some(DECAY_HALF_LIFE_DAYS)),
            Decay::Fsrs => {
                if matches!(source, "global" | "workspace" | "user" | "evergreen") {
                    1.0
                } else {
                    crate::decay::retrievability(age_days, stability_days)
                        .max(crate::decay::RETRIEVABILITY_FLOOR)
                }
            }
        }
    }

    pub fn fuse_merge<T>(&self, ballots: &[Ballot<T>], k: usize) -> Vec<T>
    where
        T: Clone + Eq + Hash,
    {
        match self.fuse {
            Fuse::Borda => borda_merge(ballots, k),
            Fuse::Rrf => {
                let mut out = rrf_merge(ballots, 60);
                out.truncate(k);
                out
            }
            Fuse::CombSum | Fuse::CombMnz => self.fuse_scored(&ranks_as_scored(ballots), k),
            Fuse::Dowdall => dowdall_merge(ballots, k),
            Fuse::Kemeny => kemeny_merge(ballots, k),
            Fuse::Schulze => schulze_merge(ballots, k),
            Fuse::Copeland => copeland_merge(ballots, k),
            Fuse::Tideman => ranked_pairs_merge(ballots, k),
        }
    }

    /// Fuse scored lists. CombSUM / CombMNZ use the raw scores;
    /// rank voters keep list order and ignore the numbers.
    pub fn fuse_scored<T>(&self, ballots: &[ScoredBallot<T>], k: usize) -> Vec<T>
    where
        T: Clone + Eq + Hash,
    {
        match self.fuse {
            Fuse::CombSum => {
                let mut out = combsum_merge(ballots);
                out.truncate(k);
                out
            }
            Fuse::CombMnz => {
                let mut out = combmnz_merge(ballots);
                out.truncate(k);
                out
            }
            Fuse::Borda
            | Fuse::Rrf
            | Fuse::Dowdall
            | Fuse::Kemeny
            | Fuse::Schulze
            | Fuse::Copeland
            | Fuse::Tideman => {
                let ranks: Vec<Ballot<T>> = ballots
                    .iter()
                    .map(|b| b.iter().map(|(id, _)| id.clone()).collect())
                    .collect();
                self.fuse_merge(&ranks, k)
            }
        }
    }

    pub fn rerank(&self, items: &[Ranked], lambda: f64) -> Vec<String> {
        match self.diversify {
            Diversify::Mmr => mmr_rerank(items, lambda),
            Diversify::Dpp => dpp_rerank(items, items.len()),
            Diversify::None => items.iter().map(|i| i.id.clone()).collect(),
        }
    }
}

fn ranks_as_scored<T: Clone>(ballots: &[Ballot<T>]) -> Vec<ScoredBallot<T>> {
    ballots
        .iter()
        .map(|ballot| {
            let n = ballot.len() as f64;
            ballot
                .iter()
                .enumerate()
                .map(|(pos, id)| (id.clone(), n - pos as f64))
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    /// Every voter, over ballots that disagree about order and about which
    /// candidates exist at all.
    ///
    /// The sort in the standard library panics on a comparator that is not a
    /// total order rather than returning a wrong answer, so a voter whose
    /// ordering can cycle takes the process down. Schulze's did, and only on
    /// three ballots or more: with two, every contested pair ties and the
    /// tie-break alone decides, which is total. A panel is host configuration,
    /// so the input that reaches a voter is not one a client chose and not one
    /// a fixed example is likely to contain.
    #[test]
    fn no_voter_asks_the_sort_for_an_impossible_order() {
        // Deterministic, so a failure here is reproducible and this needs no
        // dependency to generate it.
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let names: Vec<String> = (0..9).map(|n| format!("id{n}")).collect();
        let voters = [
            "borda", "rrf", "combsum", "combmnz", "dowdall", "kemeny", "schulze", "copeland",
            "tideman",
        ];
        for round in 0..200 {
            let ballots: Vec<Vec<String>> = (0..3 + (round % 3))
                .map(|_| {
                    let mut pool = names.clone();
                    for at in (1..pool.len()).rev() {
                        pool.swap(at, (next() % (at as u64 + 1)) as usize);
                    }
                    pool.truncate(4 + (next() % 5) as usize);
                    pool
                })
                .collect();
            for voter in voters {
                let panel = Panel::named(voter, "none", "off").expect("voter");
                let ranked = panel.fuse_merge(&ballots, names.len());
                let seen: std::collections::HashSet<&String> = ranked.iter().collect();
                assert_eq!(seen.len(), ranked.len(), "{voter} repeated a key");
            }
        }
    }

    use super::*;

    fn keep_dup_other() -> Vec<Ranked> {
        vec![
            Ranked {
                id: "keep".into(),
                rel: 1.0,
                tokens: ["review", "open", "repro"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            },
            Ranked {
                id: "dup".into(),
                rel: 0.55,
                tokens: ["review", "open", "repro"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            },
            Ranked {
                id: "other".into(),
                rel: 0.5,
                tokens: ["pin", "zircon", "index"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            },
        ]
    }

    #[test]
    fn default_is_combmnz_then_mmr() {
        let panel = Panel::default();
        assert_eq!(panel.fuse, Fuse::CombMnz);
        assert_eq!(panel.diversify, Diversify::Mmr);
        assert_eq!(panel.decay, Decay::Fsrs);
        assert_eq!(panel.fuse.as_str(), "combmnz");
        assert_eq!(panel.diversify.as_str(), "mmr");
        assert_eq!(panel.decay.as_str(), "fsrs");
        assert_eq!(Panel::parse("combmnz", "mmr").unwrap(), panel);
        assert_eq!(Panel::named("combmnz", "mmr", "fsrs").unwrap(), panel);
        // Nothing set is the default, and an empty value still fails closed.
        assert_eq!(Panel::from_env_vars(None, None, None).unwrap(), panel);
        assert!(Panel::from_env_vars(Some(""), None, None).is_err());
        // The two ways of asking for the default have to name the same panel.
        // They are separate constants, and separate constants drift.
        assert_eq!(
            Panel::named(
                Fuse::default().as_str(),
                Diversify::default().as_str(),
                Decay::default().as_str()
            )
            .unwrap(),
            panel
        );
    }

    /// The default reads scores where Borda read positions, so a list whose
    /// scores say one thing and whose order says another comes out differently.
    #[test]
    fn the_default_counts_score_and_support_rather_than_position() {
        // `b` leads one list narrowly and trails the other badly; `a` leads
        // one outright and is absent from the other. Support and score mass
        // favour b; position alone does not.
        let first = vec![("a", 10.0), ("b", 9.5), ("c", 0.0)];
        let second = vec![("b", 10.0), ("c", 9.0), ("d", 0.0)];
        // Min-max per list: a=1.0, b=0.95, c=0 and b=1.0, c=0.9, d=0. Summed,
        // then multiplied by how many lists retrieved each: b 1.95*2, c 0.9*2,
        // a 1.0*1, d 0.
        let out = Panel::default().fuse_scored(&[first, second], 4);
        assert_eq!(out, vec!["b", "c", "a", "d"], "{out:?}");
    }

    #[test]
    fn default_rerank_matches_mmr_fixture() {
        let items = keep_dup_other();
        let reranked = Panel::default().rerank(&items, 0.7);
        assert_eq!(reranked, mmr_rerank(&items, 0.7));
        assert_eq!(reranked, vec!["keep", "other", "dup"]);
    }

    #[test]
    fn diversify_none_keeps_fuse_order() {
        let panel = Panel {
            fuse: Fuse::Borda,
            diversify: Diversify::None,
            decay: Decay::Off,
        };
        let items = keep_dup_other();
        assert_eq!(panel.rerank(&items, 0.7), vec!["keep", "dup", "other"]);
    }

    #[test]
    fn parse_rrf_calls_rrf_merge() {
        assert_eq!(Fuse::parse("rrf").unwrap(), Fuse::Rrf);
        assert_eq!(Fuse::Rrf.as_str(), "rrf");
        let panel = Panel::parse("rrf", "mmr").unwrap();
        assert_eq!(panel.fuse, Fuse::Rrf);
        assert_eq!(panel.diversify, Diversify::Mmr);
        let a = vec!["x", "y", "z"];
        let b = vec!["y", "x", "z"];
        let c = vec!["z"];
        let out = Panel {
            fuse: Fuse::Rrf,
            diversify: Diversify::None,
            decay: Decay::Off,
        }
        .fuse_merge(&[a, b, c], 3);
        assert_eq!(out[0], "z");
    }

    #[test]
    fn parse_comb_calls_scored_merge() {
        assert_eq!(Fuse::parse("combsum").unwrap(), Fuse::CombSum);
        assert_eq!(Fuse::parse("combmnz").unwrap(), Fuse::CombMnz);
        assert_eq!(Fuse::CombSum.as_str(), "combsum");
        assert_eq!(Fuse::CombMnz.as_str(), "combmnz");
        let a = vec![("c", 1.0), ("d", 0.4), ("low", 0.0)];
        let b = vec![("hi", 1.0), ("d", 0.25), ("lo", 0.0)];
        let sum = Panel {
            fuse: Fuse::CombSum,
            diversify: Diversify::None,
            decay: Decay::Off,
        }
        .fuse_scored(&[a.clone(), b.clone()], 3);
        assert_eq!(sum[0], "c");
        let mnz = Panel {
            fuse: Fuse::CombMnz,
            diversify: Diversify::None,
            decay: Decay::Off,
        }
        .fuse_scored(&[a, b], 3);
        assert_eq!(mnz[0], "d");
    }

    #[test]
    fn parse_dowdall_calls_dowdall_merge() {
        assert_eq!(Fuse::parse("dowdall").unwrap(), Fuse::Dowdall);
        assert_eq!(Fuse::Dowdall.as_str(), "dowdall");
        let panel = Panel::parse("dowdall", "mmr").unwrap();
        assert_eq!(panel.fuse, Fuse::Dowdall);
        assert_eq!(panel.diversify, Diversify::Mmr);
        let a = vec!["a", "c", "d", "e", "z"];
        let b = vec!["b", "c", "d", "e", "z"];
        let c = vec!["a", "b", "c", "d", "z"];
        let out = Panel {
            fuse: Fuse::Dowdall,
            diversify: Diversify::None,
            decay: Decay::Off,
        }
        .fuse_merge(&[a, b, c], 5);
        assert_eq!(out[0], "a");
    }

    #[test]
    fn parse_kemeny_calls_kemeny_merge() {
        assert_eq!(Fuse::parse("kemeny").unwrap(), Fuse::Kemeny);
        assert_eq!(Fuse::Kemeny.as_str(), "kemeny");
        let panel = Panel::parse("kemeny", "mmr").unwrap();
        assert_eq!(panel.fuse, Fuse::Kemeny);
        assert_eq!(panel.diversify, Diversify::Mmr);
        let a = vec!["a", "b", "c"];
        let b = vec!["b", "a", "c"];
        let out = Panel {
            fuse: Fuse::Kemeny,
            diversify: Diversify::None,
            decay: Decay::Off,
        }
        .fuse_merge(&[a, b], 3);
        assert_eq!(out, vec!["a", "b", "c"]);
    }

    #[test]
    fn parse_schulze_calls_schulze_merge() {
        assert_eq!(Fuse::parse("schulze").unwrap(), Fuse::Schulze);
        assert_eq!(Fuse::Schulze.as_str(), "schulze");
        let panel = Panel::parse("schulze", "mmr").unwrap();
        assert_eq!(panel.fuse, Fuse::Schulze);
        assert_eq!(panel.diversify, Diversify::Mmr);
        let a = vec!["a", "c", "d", "e", "f"];
        let b = vec!["a", "c", "d", "e", "f"];
        let c = vec!["a", "c", "d", "e", "f"];
        let d = vec!["c", "d", "e", "f", "a"];
        let e = vec!["d", "e", "f", "c", "a"];
        let out = Panel {
            fuse: Fuse::Schulze,
            diversify: Diversify::None,
            decay: Decay::Off,
        }
        .fuse_merge(&[a, b, c, d, e], 5);
        assert_eq!(out[0], "a");
    }

    #[test]
    fn parse_copeland_calls_copeland_merge() {
        assert_eq!(Fuse::parse("copeland").unwrap(), Fuse::Copeland);
        assert_eq!(Fuse::Copeland.as_str(), "copeland");
        let panel = Panel::parse("copeland", "mmr").unwrap();
        assert_eq!(panel.fuse, Fuse::Copeland);
        assert_eq!(panel.diversify, Diversify::Mmr);
        let ballots = [
            vec!["a", "c", "d", "e", "f"],
            vec!["a", "c", "d", "e", "f"],
            vec!["a", "c", "d", "e", "f"],
            vec!["c", "d", "e", "f", "a"],
            vec!["d", "e", "f", "c", "a"],
        ];
        let out = Panel {
            fuse: Fuse::Copeland,
            diversify: Diversify::None,
            decay: Decay::Off,
        }
        .fuse_merge(&ballots, 5);
        assert_eq!(out[0], "a");
    }

    #[test]
    fn parse_tideman_calls_ranked_pairs_merge() {
        assert_eq!(Fuse::parse("tideman").unwrap(), Fuse::Tideman);
        assert_eq!(Fuse::Tideman.as_str(), "tideman");
        let panel = Panel::parse("tideman", "mmr").unwrap();
        assert_eq!(panel.fuse, Fuse::Tideman);
        assert_eq!(panel.diversify, Diversify::Mmr);
        let a = vec!["a", "c", "d", "e", "f"];
        let b = vec!["a", "c", "d", "e", "f"];
        let c = vec!["a", "c", "d", "e", "f"];
        let d = vec!["c", "d", "e", "f", "a"];
        let e = vec!["d", "e", "f", "c", "a"];
        let out = Panel {
            fuse: Fuse::Tideman,
            diversify: Diversify::None,
            decay: Decay::Off,
        }
        .fuse_merge(&[a, b, c, d, e], 5);
        assert_eq!(out[0], "a");
    }

    #[test]
    fn parse_dpp_calls_dpp_rerank() {
        assert_eq!(Diversify::parse("dpp").unwrap(), Diversify::Dpp);
        assert_eq!(Diversify::Dpp.as_str(), "dpp");
        let panel = Panel::parse("borda", "dpp").unwrap();
        assert_eq!(panel.fuse, Fuse::Borda);
        assert_eq!(panel.diversify, Diversify::Dpp);
        let items = keep_dup_other();
        let reranked = panel.rerank(&items, 0.7);
        assert_eq!(reranked, dpp_rerank(&items, items.len()));
        assert_eq!(reranked, vec!["keep", "other", "dup"]);
        assert_eq!(Panel::default().diversify, Diversify::Mmr);
    }

    #[test]
    fn nothing_named_is_the_shipped_default() {
        let panel = Panel::from_env_vars(None, None, None).unwrap();
        assert_eq!(panel, Panel::default());
        assert_eq!(panel.decay, Decay::Fsrs);
    }

    #[test]
    fn from_env_vars_reads_named_sequence() {
        let panel = Panel::from_env_vars(Some("rrf"), Some("none"), Some("on")).unwrap();
        assert_eq!(panel.fuse, Fuse::Rrf);
        assert_eq!(panel.diversify, Diversify::None);
        assert_eq!(panel.decay, Decay::On);
        let unset = Panel::from_env_vars(None, None, None).unwrap();
        assert_eq!(unset, Panel::default());
        assert!(Panel::from_env_vars(Some(""), None, None).is_err());
        let rrf = Panel::from_env_vars(Some("rrf"), None, None).unwrap();
        let a = vec!["x", "y", "z"];
        let b = vec!["y", "x", "z"];
        let c = vec!["z"];
        let out = rrf.fuse_merge(&[a, b, c], 3);
        assert_eq!(out[0], "z");
        let kemeny = Panel::from_env_vars(Some("kemeny"), None, None).unwrap();
        assert_eq!(kemeny.fuse, Fuse::Kemeny);
        let dpp = Panel::from_env_vars(None, Some("dpp"), None).unwrap();
        assert_eq!(dpp.diversify, Diversify::Dpp);
    }

    #[test]
    fn decay_off_is_one_on_uses_temporal() {
        let off = Panel::named("combmnz", "mmr", "off").unwrap();
        assert_eq!(off.decay_weight("session", 14.0, 1.0), 1.0);
        let on = Panel::named("borda", "mmr", "on").unwrap();
        let w = on.decay_weight("session", 14.0, 1.0);
        assert!((w - 0.5).abs() < 1e-9);
        assert_eq!(on.decay_weight("global", 400.0, 1.0), 1.0);
        let fsrs = Panel::named("combmnz", "mmr", "fsrs").unwrap();
        assert_eq!(fsrs.decay, Decay::Fsrs);
        assert_eq!(Decay::parse("retrievability").unwrap(), Decay::Fsrs);
        assert_eq!(Decay::Fsrs.as_str(), "fsrs");
        assert!((fsrs.decay_weight("atoms", 3.0, 3.0) - 0.9).abs() < 1e-9);
        assert_eq!(
            fsrs.decay_weight("atoms", 1e6, 1.0),
            crate::decay::RETRIEVABILITY_FLOOR
        );
        assert_eq!(fsrs.decay_weight("user", 1e6, 1.0), 1.0);
    }

    #[test]
    fn unknown_name_is_error() {
        assert!(matches!(
            Fuse::parse("not-a-voter"),
            Err(UnknownVoter::Fuse(name)) if name == "not-a-voter"
        ));
        assert!(matches!(
            Diversify::parse("not-a-voter"),
            Err(UnknownVoter::Diversify(name)) if name == "not-a-voter"
        ));
        assert!(matches!(
            Decay::parse("maybe"),
            Err(UnknownVoter::Decay(name)) if name == "maybe"
        ));
        assert!(Panel::parse("borda", "not-a-voter").is_err());
        assert!(Panel::named("borda", "mmr", "maybe").is_err());
        assert!(Fuse::parse("").is_err());
        assert!(Fuse::parse("Borda").is_err());
        assert!(Fuse::parse("CombMNZ").is_err());
        assert!(Fuse::parse("Dowdall").is_err());
        assert!(Fuse::parse("Kemeny").is_err());
        assert!(Fuse::parse("Schulze").is_err());
        assert!(Fuse::parse("Copeland").is_err());
        assert!(Fuse::parse("Tideman").is_err());
        assert!(Fuse::parse("ranked-pairs").is_err());
        assert!(Diversify::parse("").is_err());
        assert!(Diversify::parse("Dpp").is_err());
        assert!(Decay::parse("On").is_err());
    }
}
