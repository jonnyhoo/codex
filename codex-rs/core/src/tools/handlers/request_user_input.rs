use crate::function_tool::FunctionCallError;
use crate::request_user_input_allowed_modes_message;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;
use async_trait::async_trait;
#[cfg(test)]
use codex_protocol::config_types::ModeKind;
use codex_protocol::request_user_input::RequestUserInputArgs;

#[cfg(test)]
fn request_user_input_unavailable_message(
    mode: ModeKind,
    default_mode_request_user_input: bool,
) -> Option<String> {
    crate::tool_policy::RuntimeRequestUserInputPolicy {
        tool_enabled: true,
        available: crate::request_user_input_is_available(mode, default_mode_request_user_input),
        default_mode_enabled: default_mode_request_user_input,
        allowed_modes: Vec::new(),
    }
    .unavailable_message(mode)
}

pub(crate) fn request_user_input_tool_description(default_mode_request_user_input: bool) -> String {
    let allowed_modes = request_user_input_allowed_modes_message(default_mode_request_user_input);
    format!(
        "Request user input for one to three short questions and wait for the response. This tool is only available in {allowed_modes}."
    )
}

pub struct RequestUserInputHandler;

#[async_trait]
impl ToolHandler for RequestUserInputHandler {
    type Output = FunctionToolOutput;

    fn kind(&self) -> ToolKind {
        ToolKind::Function
    }

    async fn handle(&self, invocation: ToolInvocation) -> Result<Self::Output, FunctionCallError> {
        let tool_policy = invocation.turn.runtime_tool_policy();
        let ToolInvocation {
            session,
            turn,
            call_id,
            payload,
            ..
        } = invocation;

        let arguments = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "request_user_input handler received unsupported payload".to_string(),
                ));
            }
        };

        let mode = turn.collaboration_mode.mode;
        if let Some(message) = tool_policy.request_user_input.unavailable_message(mode) {
            return Err(FunctionCallError::RespondToModel(message));
        }

        let mut args: RequestUserInputArgs = parse_arguments(&arguments)?;
        let missing_options = args
            .questions
            .iter()
            .any(|question| question.options.as_ref().is_none_or(Vec::is_empty));
        if missing_options {
            return Err(FunctionCallError::RespondToModel(
                "request_user_input requires non-empty options for every question".to_string(),
            ));
        }
        for question in &mut args.questions {
            question.is_other = true;
        }
        let response = session
            .request_user_input(turn.as_ref(), call_id, args)
            .await
            .ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "request_user_input was cancelled before receiving a response".to_string(),
                )
            })?;

        let content = serde_json::to_string(&response).map_err(|err| {
            FunctionCallError::Fatal(format!(
                "failed to serialize request_user_input response: {err}"
            ))
        })?;

        Ok(FunctionToolOutput::from_text(content, Some(true)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn request_user_input_mode_availability_defaults_to_plan_only() {
        assert!(ModeKind::Plan.allows_request_user_input());
        assert!(!ModeKind::Default.allows_request_user_input());
        assert!(!ModeKind::Execute.allows_request_user_input());
        assert!(!ModeKind::PairProgramming.allows_request_user_input());
    }

    #[test]
    fn request_user_input_unavailable_messages_respect_default_mode_feature_flag() {
        assert_eq!(
            request_user_input_unavailable_message(ModeKind::Plan, false),
            None
        );
        assert_eq!(
            request_user_input_unavailable_message(ModeKind::Default, false),
            Some("request_user_input is unavailable in Default mode".to_string())
        );
        assert_eq!(
            request_user_input_unavailable_message(ModeKind::Default, true),
            None
        );
        assert_eq!(
            request_user_input_unavailable_message(ModeKind::Execute, false),
            Some("request_user_input is unavailable in Execute mode".to_string())
        );
        assert_eq!(
            request_user_input_unavailable_message(ModeKind::PairProgramming, false),
            Some("request_user_input is unavailable in Pair Programming mode".to_string())
        );
    }

    #[test]
    fn request_user_input_tool_description_mentions_available_modes() {
        assert_eq!(
            request_user_input_tool_description(false),
            "Request user input for one to three short questions and wait for the response. This tool is only available in Plan mode.".to_string()
        );
        assert_eq!(
            request_user_input_tool_description(true),
            "Request user input for one to three short questions and wait for the response. This tool is only available in Default or Plan mode.".to_string()
        );
    }
}
