use std::fmt;

use crate::api::schema::{ErrorBody, ErrorResponse, Method, Request, ServerCapabilities};

#[derive(Debug)]
pub(super) struct ProtocolMismatchReported;

impl fmt::Display for ProtocolMismatchReported {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("protocol mismatch was already reported")
    }
}

impl std::error::Error for ProtocolMismatchReported {}

pub(super) fn mismatch_response(
    request_id: &str,
    server_protocol: u32,
    restart_guidance: &str,
) -> Option<ErrorResponse> {
    let client_protocol = crate::protocol::PROTOCOL_VERSION;
    if client_protocol == server_protocol {
        return None;
    }

    let message = if client_protocol > server_protocol {
        format!(
            "client protocol {client_protocol} is newer than server protocol {server_protocol}; restart the Herdr server before using this command. {restart_guidance}"
        )
    } else {
        format!(
            "client protocol {client_protocol} is older than server protocol {server_protocol}; upgrade the Herdr client before using this command"
        )
    };

    Some(ErrorResponse {
        id: request_id.to_string(),
        error: ErrorBody {
            code: "protocol_mismatch".into(),
            message,
        },
    })
}

pub(super) fn missing_capability_response(
    request: &Request,
    capabilities: Option<&ServerCapabilities>,
    restart_guidance: &str,
) -> Option<ErrorResponse> {
    if matches!(&request.method, Method::AgentPrompt(params) if params.has_expectations())
        && !capabilities.is_some_and(|caps| caps.agent_prompt_expectations)
    {
        return Some(ErrorResponse {
            id: request.id.clone(),
            error: ErrorBody {
                code: "protocol_mismatch".into(),
                message: format!(
                    "server does not support conditional agent prompt expectations (missing capability agent_prompt_expectations); restart the Herdr server before using this command. {restart_guidance}"
                ),
            },
        });
    }

    None
}

pub(super) fn reported_error() -> std::io::Error {
    std::io::Error::other(ProtocolMismatchReported)
}

pub(super) fn was_reported(err: &std::io::Error) -> bool {
    err.get_ref()
        .and_then(|source| source.downcast_ref::<ProtocolMismatchReported>())
        .is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_protocol_has_no_error() {
        assert!(mismatch_response("req", crate::protocol::PROTOCOL_VERSION, "restart").is_none());
    }

    #[test]
    fn older_server_error_preserves_request_id_and_guidance() {
        let response = mismatch_response(
            "cli:agent:wait",
            crate::protocol::PROTOCOL_VERSION - 1,
            "Run the session stop command, then restart.",
        )
        .unwrap();

        assert_eq!(response.id, "cli:agent:wait");
        assert_eq!(response.error.code, "protocol_mismatch");
        assert!(response.error.message.contains(&format!(
            "client protocol {}",
            crate::protocol::PROTOCOL_VERSION
        )));
        assert!(response.error.message.contains(&format!(
            "server protocol {}",
            crate::protocol::PROTOCOL_VERSION - 1
        )));
        assert!(response.error.message.contains("restart"));
    }

    #[test]
    fn newer_server_error_tells_user_to_upgrade_client() {
        let response = mismatch_response(
            "cli:pane:list",
            crate::protocol::PROTOCOL_VERSION + 1,
            "unused restart guidance",
        )
        .unwrap();

        assert!(response
            .error
            .message
            .contains("older than server protocol"));
        assert!(response.error.message.contains("upgrade the Herdr client"));
        assert!(!response.error.message.contains("unused restart guidance"));
    }

    #[test]
    fn reported_error_is_recognizable_without_string_matching() {
        assert!(was_reported(&reported_error()));
        assert!(!was_reported(&std::io::Error::other(
            "protocol mismatch was already reported"
        )));
    }
}
