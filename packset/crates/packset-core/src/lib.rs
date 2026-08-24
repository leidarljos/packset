//! Store-agnostic packset algorithms.
//!
//! Host merge is a named fuse then diversify then decay panel.
//! Default fuse is Borda (`k - position`). Reciprocal Rank Fusion,
//! CombSUM / CombMNZ, Dowdall, Kemeny-Young, Schulze, Copeland,
//! and Tideman are named fuse voters. Default diversify is MMR.
//! A Determinantal Point Process is a named diversify voter.
//! Default decay is off. The host reads `PACKSET_FUSE`,
//! `PACKSET_DIVERSIFY`, and `PACKSET_DECAY`. Clients do not
//! choose this.

pub mod atom;
pub mod borda;
pub mod comb;
pub mod copeland;
pub mod decay;
pub mod dowdall;
pub mod dpp;
pub mod extract;
pub mod kemeny;
pub mod mmr;
pub mod panel;
pub mod rrf;
pub mod schulze;
pub mod tideman;

pub use atom::{entity_jaccard, is_due, is_live, SCHEMA as ATOM_SCHEMA};
pub use borda::{borda_merge, Ballot};
pub use comb::{combmnz_merge, combsum_merge, ScoredBallot};
pub use copeland::copeland_merge;
pub use decay::temporal_decay;
pub use dowdall::dowdall_merge;
pub use dpp::dpp_rerank;
pub use extract::{claim_from_user, is_tool_dump};
pub use kemeny::kemeny_merge;
pub use mmr::mmr_rerank;
pub use panel::{Decay, Diversify, Fuse, Panel, UnknownVoter};
pub use rrf::rrf_merge;
pub use schulze::schulze_merge;
pub use tideman::ranked_pairs_merge;
