use async_trait::async_trait;
use codex_protocol::models::FunctionCallOutputBody;

use crate::function_tool::FunctionCallError;
use crate::lsp::LspToolRequest;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;

pub struct LspHandler;

#[async_trait]
impl ToolHandler for LspHandler {
    fn kind(&self) -> ToolKind {
        ToolKind::Function
    }

    async fn handle(&self, invocation: ToolInvocation) -> Result<ToolOutput, FunctionCallError> {
        let ToolInvocation { payload, turn, .. } = invocation;

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
        let rendered =
            tokio::task::spawn_blocking(move || crate::lsp::invoke(request, cwd, codex_home))
                .await
                .map_err(|err| {
                    FunctionCallError::RespondToModel(format!("failed to join lsp task: {err}"))
                })?
                .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;

        Ok(ToolOutput::Function {
            body: FunctionCallOutputBody::Text(rendered),
            success: Some(true),
        })
    }
}
