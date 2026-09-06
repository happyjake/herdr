use std::path::PathBuf;

use crate::api::schema::{PlaceInfo, ResponseResult, ServerLookupPlaceParams};
use crate::app::App;
use crate::place_lookup::{self, Desk, MAX_PLACES};

use super::responses::{encode_error, encode_success};

/// Read a lookup's query and limit, or say why they are no request at all.
///
/// The limit is capped rather than refused, so a client that asks for more
/// than the server will ever answer still gets an answer.
fn read_params(params: &ServerLookupPlaceParams) -> Result<(&str, usize), &'static str> {
    let query = params.query.trim();
    if query.is_empty() {
        return Err("query must not be empty");
    }
    let limit = match params.limit {
        None => MAX_PLACES,
        Some(0) => return Err("limit must be at least 1"),
        Some(limit) => (limit as usize).min(MAX_PLACES),
    };
    Ok((query, limit))
}

impl App {
    pub(super) fn handle_server_lookup_place(
        &mut self,
        id: String,
        params: ServerLookupPlaceParams,
    ) -> String {
        let (query, limit) = match read_params(&params) {
            Ok(read) => read,
            Err(message) => return encode_error(id, "invalid_params", message),
        };

        let desk = Desk {
            workspaces: self.remembered_workspace_dirs(),
            home: crate::worktree::home_dir(),
            tmux: place_lookup::tmux_pane_dirs(),
        };

        let places = place_lookup::lookup(&desk, query, limit)
            .into_iter()
            .map(|place| PlaceInfo {
                path: place.path,
                source: place.source,
            })
            .collect();

        encode_success(id, ResponseResult::ServerLookupPlace { places })
    }

    /// Directories this server's own workspaces stand in: the live ones
    /// first, then the persisted layout, so a workspace that is open right
    /// now outranks one that was only ever saved.
    fn remembered_workspace_dirs(&self) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = self
            .state
            .workspaces
            .iter()
            .filter_map(|workspace| {
                workspace.resolved_identity_cwd_from(&self.state.terminals, &self.terminal_runtimes)
            })
            .collect();

        if let Some(snapshot) = crate::persist::load() {
            for workspace in &snapshot.workspaces {
                dirs.push(workspace.identity_cwd.clone());
                for tab in &workspace.tabs {
                    let mut panes: Vec<_> = tab.panes.iter().collect();
                    panes.sort_by_key(|(number, _)| **number);
                    dirs.extend(panes.into_iter().map(|(_, pane)| pane.cwd.clone()));
                }
            }
        }

        dirs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{ErrorResponse, SuccessResponse};
    use crate::config::Config;
    use crate::workspace::Workspace;

    #[test]
    fn a_blank_query_is_no_request_at_all() {
        for query in ["", "   ", "\t\n"] {
            let params = ServerLookupPlaceParams {
                query: query.into(),
                limit: None,
            };
            assert_eq!(read_params(&params), Err("query must not be empty"));
        }
    }

    #[test]
    fn a_query_is_read_without_its_surrounding_space() {
        let params = ServerLookupPlaceParams {
            query: "  herdr \n".into(),
            limit: None,
        };
        assert_eq!(read_params(&params), Ok(("herdr", MAX_PLACES)));
    }

    #[test]
    fn a_limit_of_zero_is_refused_and_a_large_one_is_capped() {
        let refused = ServerLookupPlaceParams {
            query: "herdr".into(),
            limit: Some(0),
        };
        assert_eq!(read_params(&refused), Err("limit must be at least 1"));

        let capped = ServerLookupPlaceParams {
            query: "herdr".into(),
            limit: Some(500),
        };
        assert_eq!(read_params(&capped), Ok(("herdr", MAX_PLACES)));

        let smaller = ServerLookupPlaceParams {
            query: "herdr".into(),
            limit: Some(3),
        };
        assert_eq!(read_params(&smaller), Ok(("herdr", 3)));
    }

    #[tokio::test]
    async fn a_lookup_answers_a_directory_one_of_this_server_s_workspaces_stands_in() {
        // A name no other memory on this machine could hold, so the answer
        // can only have come from the workspace this test opened.
        let root = std::env::temp_dir().join(format!(
            "herdr-lookup-place-api-{}/qzxvlookupplace",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("fixture directory");

        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        let mut workspace = Workspace::test_new("places");
        workspace.identity_cwd = root.clone();
        app.state.workspaces = vec![workspace];

        let response = app.handle_server_lookup_place(
            "req_lookup".into(),
            ServerLookupPlaceParams {
                query: "qzxvlookupplace".into(),
                limit: None,
            },
        );
        let response: SuccessResponse = serde_json::from_str(&response).expect("success response");
        match response.result {
            ResponseResult::ServerLookupPlace { places } => {
                assert_eq!(places.len(), 1, "places: {places:?}");
                assert_eq!(places[0].path, root.to_str().unwrap());
                assert_eq!(places[0].source, crate::api::schema::PlaceSource::Workspace);
            }
            other => panic!("expected server_lookup_place, got {other:?}"),
        }

        let refusal = app.handle_server_lookup_place(
            "req_blank".into(),
            ServerLookupPlaceParams {
                query: "   ".into(),
                limit: None,
            },
        );
        let refusal: ErrorResponse = serde_json::from_str(&refusal).expect("error response");
        assert_eq!(refusal.error.code, "invalid_params");

        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }
}
