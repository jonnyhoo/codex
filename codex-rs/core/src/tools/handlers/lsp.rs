use async_trait::async_trait;

use crate::function_tool::FunctionCallError;
use crate::lsp::LspToolRequest;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;

pub struct LspHandler;

#[async_trait]
impl ToolHandler for LspHandler {
    type Output = FunctionToolOutput;

    fn kind(&self) -> ToolKind {
        ToolKind::Function
    }

    async fn handle(&self, invocation: ToolInvocation) -> Result<Self::Output, FunctionCallError> {
        let ToolInvocation {
            payload,
            session,
            turn,
            ..
        } = invocation;

        let arguments = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "lsp handler received unsupported payload".to_string(),
                ));
            }
        };

        let request: LspToolRequest = parse_arguments(&arguments)?;
        let cwd = turn.cwd.clone();
        let codex_home = turn.config.codex_home.clone();
        let lsp_session_manager = session.services.lsp_session_manager.clone();
        let file_watcher = session.services.file_watcher.clone();
        let rendered = tokio::task::spawn_blocking(move || {
            crate::lsp::invoke_with_session_manager(
                request,
                cwd,
                codex_home,
                Some(lsp_session_manager),
                Some(file_watcher),
            )
        })
        .await
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!("failed to join lsp task: {err}"))
        })?
        .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;

        Ok(FunctionToolOutput::from_text(rendered, Some(true)))
    }
}
