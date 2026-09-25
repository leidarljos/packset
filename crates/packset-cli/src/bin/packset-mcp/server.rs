//! The pack over MCP: a reader over the daemon, not a second store.
//! A writer that is down is reported as down, never as an empty pack.

use packset_client::PacksetClient;
use rmcp::{
    handler::server::wrapper::Json, handler::server::wrapper::Parameters,
    handler::server::ServerHandler, model::*, prompt_handler, tool, tool_handler, tool_router,
    ErrorData as McpError,
};
use serde::Serialize;

use crate::args::*;

#[derive(Clone)]
pub struct PacksetServer {
    port: u16,
    workspace: String,
}

/// One remembered claim.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct AtomRow {
    /// The atom's id, when it has one.
    pub id: Option<String>,
    /// What the seat remembers.
    pub text: String,
    /// `conclusion`, `preference`, and the rest.
    pub kind: String,
    /// How well it matched, on the panel's scale.
    pub score: f64,
}

/// What a pack is answering with, or why it is not.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct PackState {
    /// Whether the writer answered at all.
    pub up: bool,
    /// The workspace these answers are about.
    pub workspace: String,
    /// What the writer said about itself, when it is up.
    pub detail: Option<serde_json::Value>,
}

fn client(port: u16) -> PacksetClient {
    PacksetClient::new(format!("http://127.0.0.1:{port}"))
}

/// The writer did not answer. The message names the verb that starts one.
fn down(e: impl std::fmt::Display) -> McpError {
    McpError::internal_error(
        format!(
            "the pack writer did not answer: {e}. This is not an empty pack. \
             `packset ensure` starts one; until then nothing here can say what \
             the seat knows."
        ),
        None,
    )
}

#[tool_router]
impl PacksetServer {
    /// Read the port and workspace the seat uses, the same as `ljos doctor`.
    /// Loads `~/.config/ljos/env` when those keys are unset. An absent
    /// `PACKSET_WORKSPACE` is `seat`, not `default`.
    #[must_use]
    pub fn from_env() -> Self {
        let port = packset_client::default_port();
        let workspace = packset_client::resolved_workspace();
        Self { port, workspace }
    }

    /// Open on a named port and workspace, for a test.
    #[cfg(test)]
    #[must_use]
    pub fn at(port: u16, workspace: impl Into<String>) -> Self {
        Self {
            port,
            workspace: workspace.into(),
        }
    }

    fn workspace_for(&self, given: Option<&str>) -> String {
        given
            .filter(|w| !w.is_empty())
            .unwrap_or(&self.workspace)
            .to_string()
    }

    #[tool(
        description = "What the seat remembers about a question, ranked by the panel the host configured. An empty list means the pack holds nothing matching; a failure means the writer is not running, which is a different thing.",
        annotations(
            title = "Search the pack",
            read_only_hint = true,
            open_world_hint = false
        )
    )]
    async fn packset_search(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<Json<Vec<AtomRow>>, McpError> {
        let workspace = self.workspace_for(args.workspace.as_deref());
        let hits = client(self.port)
            .search_opts(
                &workspace,
                &args.query,
                args.limit.unwrap_or(10),
                args.as_of.as_deref(),
                args.rerank.unwrap_or(false),
            )
            .map_err(down)?;
        Ok(Json(
            hits.into_iter()
                .map(|hit| AtomRow {
                    id: hit.id,
                    text: hit.text,
                    kind: hit.kind,
                    score: hit.score,
                })
                .collect(),
        ))
    }

    #[tool(
        description = "The atoms in a workspace. Live-now when as_of is omitted; the ones that were live at that timestamp when it is set. Search cannot answer a dated question without this.",
        annotations(
            title = "Retrieve atoms, optionally as-of a date",
            read_only_hint = true,
            open_world_hint = false
        )
    )]
    async fn packset_atoms(
        &self,
        Parameters(args): Parameters<AtomsArgs>,
    ) -> Result<Json<Vec<AtomRow>>, McpError> {
        let workspace = self.workspace_for(args.workspace.as_deref());
        let atoms = client(self.port)
            .atoms_as_of(&workspace, args.as_of.as_deref())
            .map_err(down)?;
        Ok(Json(
            atoms
                .iter()
                .map(|atom| AtomRow {
                    id: atom
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
                    text: atom
                        .get("text")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    kind: atom
                        .get("kind")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    score: 0.0,
                })
                .collect(),
        ))
    }

    #[tool(
        description = "Every deed accession the live atoms in a workspace cite. This is the pack's half of the join: hand these to a deed store to find out what the remembered claims stand on.",
        annotations(
            title = "Accessions the pack cites",
            read_only_hint = true,
            open_world_hint = false
        )
    )]
    async fn packset_accessions(
        &self,
        Parameters(args): Parameters<WorkspaceArgs>,
    ) -> Result<Json<Vec<String>>, McpError> {
        let workspace = self.workspace_for(args.workspace.as_deref());
        Ok(Json(
            client(self.port).accessions(&workspace).map_err(down)?,
        ))
    }

    #[tool(
        description = "The remembered claims citing one deed accession, which is the join walked the other way: from a product back to what the seat concluded from it.",
        annotations(
            title = "What cites a deed",
            read_only_hint = true,
            open_world_hint = false
        )
    )]
    async fn packset_citers(
        &self,
        Parameters(args): Parameters<CitersArgs>,
    ) -> Result<Json<Vec<AtomRow>>, McpError> {
        let workspace = self.workspace_for(args.workspace.as_deref());
        let atoms = client(self.port)
            .citers(&workspace, &args.accession)
            .map_err(down)?;
        Ok(Json(
            atoms
                .iter()
                .map(|atom| AtomRow {
                    id: atom
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
                    text: atom
                        .get("text")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    kind: atom
                        .get("kind")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    score: 0.0,
                })
                .collect(),
        ))
    }

    #[tool(
        description = "Whether the pack writer is running, and what it says about itself: counts by kind, the pinned set, and whether the search and dense projections are available. Ask this when another verb reports the writer down.",
        annotations(
            title = "Is the pack up",
            read_only_hint = true,
            open_world_hint = false
        )
    )]
    async fn packset_state(
        &self,
        Parameters(args): Parameters<WorkspaceArgs>,
    ) -> Result<Json<PackState>, McpError> {
        let workspace = self.workspace_for(args.workspace.as_deref());
        // The one verb that answers when the writer is down.
        match client(self.port).status(Some(&workspace)) {
            Ok(detail) => Ok(Json(PackState {
                up: true,
                workspace,
                detail: Some(detail),
            })),
            Err(_) => Ok(Json(PackState {
                up: false,
                workspace,
                detail: None,
            })),
        }
    }
}

#[tool_handler]
#[prompt_handler(router = Self::prompt_router())]
impl ServerHandler for PacksetServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_prompts()
                .build(),
        )
        .with_server_info(Implementation::new("packset", env!("CARGO_PKG_VERSION")))
        .with_instructions(
            "This reads a pack a local writer owns; it is not a store of its own. A \
                 failure here usually means the writer is not running, which is not an \
                 empty pack: ask whether the pack is up before concluding the seat knows \
                 nothing. The accession verbs are the join. What the seat remembers is in \
                 the atoms, what the work produced is in the deed store, and the accession \
                 is the only identifier that crosses.",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every tool reads; the daemon is the only writer.
    #[test]
    fn the_surface_only_reads() {
        let tools = PacksetServer::tool_router().list_all();
        assert!(tools.len() >= 4, "{} tools", tools.len());
        for tool in &tools {
            let hints = tool
                .annotations
                .as_ref()
                .unwrap_or_else(|| panic!("{} carries no annotations", tool.name));
            assert_eq!(
                hints.read_only_hint,
                Some(true),
                "{} writes, and this surface has no business writing a pack",
                tool.name
            );
            assert_eq!(hints.open_world_hint, Some(false), "{}", tool.name);
        }
    }

    /// A dead writer fails search and is reported by `state`.
    #[tokio::test]
    async fn a_writer_that_is_not_running_is_not_an_empty_pack() {
        // A port nothing listens on.
        let server = PacksetServer::at(1, "sample");

        let Err(err) = server
            .packset_search(Parameters(SearchArgs {
                query: "anything".into(),
                workspace: None,
                limit: None,
                as_of: None,
                rerank: None,
            }))
            .await
        else {
            panic!("a dead writer answered a search");
        };
        let said = format!("{err:?}");
        assert!(said.contains("not an empty pack"), "{said}");
        assert!(said.contains("packset ensure"), "{said}");

        let state = server
            .packset_state(Parameters(WorkspaceArgs { workspace: None }))
            .await
            .expect("state answers even when the writer does not");
        assert!(!state.0.up);
        assert_eq!(state.0.workspace, "sample");
        assert!(state.0.detail.is_none());
    }

    /// An absent or empty workspace is the seat's own.
    #[test]
    fn a_missing_workspace_is_the_seats_own() {
        let server = PacksetServer::at(1, "sample");
        assert_eq!(server.workspace_for(None), "sample");
        assert_eq!(server.workspace_for(Some("")), "sample");
        assert_eq!(server.workspace_for(Some("other")), "other");
    }

    #[test]
    fn from_env_uses_ljos_env_workspace_not_default() {
        let dir = std::env::temp_dir().join(format!("packset-mcp-env-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".config/ljos")).unwrap();
        std::fs::write(
            dir.join(".config/ljos/env"),
            "PACKSET_WORKSPACE=git:example.com/seat/notes\n",
        )
        .unwrap();
        let old_home = std::env::var("HOME").ok();
        let old_ws = std::env::var("PACKSET_WORKSPACE").ok();
        unsafe {
            std::env::remove_var("PACKSET_WORKSPACE");
            std::env::set_var("HOME", &dir);
        }
        let server = PacksetServer::from_env();
        unsafe {
            match old_home {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
            match old_ws {
                Some(w) => std::env::set_var("PACKSET_WORKSPACE", w),
                None => std::env::remove_var("PACKSET_WORKSPACE"),
            }
        }
        assert_eq!(server.workspace_for(None), "git:example.com/seat/notes");
        assert_ne!(server.workspace_for(None), "default");
    }
}
