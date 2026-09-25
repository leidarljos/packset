//! What each tool takes.
//!
//! The workspace is optional everywhere: a seat has one it means, named by
//! `PACKSET_WORKSPACE`, and a caller that has to repeat it on every call will
//! eventually pass the wrong one.

use schemars::JsonSchema;
use serde::Deserialize;

/// A question for the pack.
#[derive(Deserialize, JsonSchema)]
pub struct SearchArgs {
    /// What to ask.
    pub query: String,
    /// Which workspace. The seat's own when omitted.
    #[serde(default)]
    pub workspace: Option<String>,
    /// How many hits. Ten when omitted.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Retrieve the atoms that were live at this timestamp instead of now.
    #[serde(default)]
    pub as_of: Option<String>,
    /// Run the measured cross-encoder second stage. Off unless asked.
    #[serde(default)]
    pub rerank: Option<bool>,
}

/// A workspace, optionally dated.
#[derive(Deserialize, JsonSchema)]
pub struct AtomsArgs {
    /// Which workspace. The seat's own when omitted.
    #[serde(default)]
    pub workspace: Option<String>,
    /// Retrieve the atoms that were live at this timestamp instead of now.
    #[serde(default)]
    pub as_of: Option<String>,
}

/// A workspace, or the seat's own.
#[derive(Deserialize, JsonSchema)]
pub struct WorkspaceArgs {
    /// Which workspace. The seat's own when omitted.
    #[serde(default)]
    pub workspace: Option<String>,
}

/// One accession, and where to look for what cites it.
#[derive(Deserialize, JsonSchema)]
pub struct CitersArgs {
    /// `deed-<kind>-<slug>`, or a `sha256:` accession.
    pub accession: String,
    /// Which workspace. The seat's own when omitted.
    #[serde(default)]
    pub workspace: Option<String>,
}
