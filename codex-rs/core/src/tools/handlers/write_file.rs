use std::io::ErrorKind;
use std::path::PathBuf;

use async_trait::async_trait;

use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::file_change::execute_verified_action;
use crate::tools::handlers::file_change::make_write_action;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;
use serde::Deserialize;

pub struct WriteFileHandler;

#[derive(Deserialize)]
struct WriteFileArgs {
    file_path: String,
    content: String,
}

#[async_trait]
impl ToolHandler for WriteFileHandler {
    type Output = FunctionToolOutput;

    fn kind(&self) -> ToolKind {
        ToolKind::Function
    }

    async fn is_mutating(&self, _invocation: &ToolInvocation) -> bool {
        true
    }

    async fn handle(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Self::Output, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            tracker,
            call_id,
            tool_name,
            payload,
            ..
        } = invocation;

        let arguments = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "write_file handler received unsupported payload".to_string(),
                ));
            }
        };

        let args: WriteFileArgs = parse_arguments(&arguments)?;
        let file_path = PathBuf::from(&args.file_path);
        if !file_path.is_absolute() {
            return Err(FunctionCallError::RespondToModel(
                "file_path must be an absolute path".to_string(),
            ));
        }

        let metadata = tokio::fs::metadata(&file_path).await;
        let old_content = match metadata {
            Ok(metadata) => {
                if metadata.is_dir() {
                    return Err(FunctionCallError::RespondToModel(
                        "file_path must point to a file".to_string(),
                    ));
                }
                Some(tokio::fs::read_to_string(&file_path).await.map_err(|err| {
                    FunctionCallError::RespondToModel(format!("failed to read file: {err}"))
                })?)
            }
            Err(err) if err.kind() == ErrorKind::NotFound => None,
            Err(err) => {
                return Err(FunctionCallError::RespondToModel(format!(
                    "failed to inspect file: {err}"
                )));
            }
        };

        if old_content.as_deref() == Some(args.content.as_str()) {
            return Err(FunctionCallError::RespondToModel(
                "write_file would not change the file".to_string(),
            ));
        }

        let action = make_write_action(
            turn.cwd.clone(),
            file_path,
            old_content.as_deref(),
            args.content,
        )?;

        execute_verified_action(
            session,
            turn,
            Some(&tracker),
            &call_id,
            tool_name.as_str(),
            action,
            None,
        )
        .await
    }
}
