use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use codex_protocol::models::FileSystemPermissions;
use codex_protocol::models::PermissionProfile;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::collections::BTreeSet;

use crate::apply_patch;
use crate::apply_patch::InternalApplyPatchInvocation;
use crate::apply_patch::convert_apply_patch_to_protocol;
use crate::codex::Session;
use crate::codex::TurnContext;
use crate::function_tool::FunctionCallError;
use crate::lsp::LspWatchedFileChange;
use crate::lsp::LspWatchedFileChangeKind;
use crate::sandboxing::SandboxPermissions;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::events::ToolEmitter;
use crate::tools::events::ToolEventCtx;
use crate::tools::handlers::apply_granted_turn_permissions;
use crate::tools::orchestrator::ToolOrchestrator;
use crate::tools::runtimes::apply_patch::ApplyPatchRequest;
use crate::tools::runtimes::apply_patch::ApplyPatchRuntime;
use crate::tools::sandboxing::ToolCtx;
use codex_apply_patch::ApplyPatchAction;
use codex_apply_patch::ApplyPatchFileChange;

pub(crate) fn file_paths_for_action(action: &ApplyPatchAction) -> Vec<AbsolutePathBuf> {
    let mut keys = Vec::new();
    let cwd = action.cwd.as_path();

    for (path, change) in action.changes() {
        if let Some(key) = to_abs_path(cwd, path) {
            keys.push(key);
        }

        if let ApplyPatchFileChange::Update { move_path, .. } = change
            && let Some(dest) = move_path
            && let Some(key) = to_abs_path(cwd, dest)
        {
            keys.push(key);
        }
    }

    keys
}

pub(crate) async fn execute_verified_action(
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    tracker: Option<&SharedTurnDiffTracker>,
    call_id: &str,
    tool_name: &str,
    action: ApplyPatchAction,
    timeout_ms: Option<u64>,
) -> Result<FunctionToolOutput, FunctionCallError> {
    let watched_file_changes = watched_file_changes_for_action(&action);
    match apply_patch::apply_patch(turn.as_ref(), action).await {
        InternalApplyPatchInvocation::Output(item) => {
            let content = item?;
            session
                .services
                .lsp_session_manager
                .note_external_file_changes(&watched_file_changes);
            Ok(FunctionToolOutput::from_text(content, Some(true)))
        }
        InternalApplyPatchInvocation::DelegateToExec(apply) => {
            let changes = convert_apply_patch_to_protocol(&apply.action);
            let file_paths = file_paths_for_action(&apply.action);
            let effective_additional_permissions = apply_granted_turn_permissions(
                session.as_ref(),
                SandboxPermissions::UseDefault,
                write_permissions_for_paths(&file_paths),
            )
            .await;
            let emitter = ToolEmitter::apply_patch(changes.clone(), apply.auto_approved);
            let event_ctx = ToolEventCtx::new(session.as_ref(), turn.as_ref(), call_id, tracker);
            emitter.begin(event_ctx).await;

            let req = ApplyPatchRequest {
                action: apply.action,
                file_paths,
                changes,
                exec_approval_requirement: apply.exec_approval_requirement,
                sandbox_permissions: effective_additional_permissions.sandbox_permissions,
                additional_permissions: effective_additional_permissions.additional_permissions,
                permissions_preapproved: effective_additional_permissions.permissions_preapproved,
                timeout_ms,
                codex_exe: turn.codex_linux_sandbox_exe.clone(),
            };

            let mut orchestrator = ToolOrchestrator::new();
            let mut runtime = ApplyPatchRuntime::new();
            let tool_ctx = ToolCtx {
                session: session.clone(),
                turn: turn.clone(),
                call_id: call_id.to_string(),
                tool_name: tool_name.to_string(),
            };
            let out = orchestrator
                .run(
                    &mut runtime,
                    &req,
                    &tool_ctx,
                    turn.as_ref(),
                    turn.approval_policy.value(),
                )
                .await
                .map(|result| result.output);
            let event_ctx = ToolEventCtx::new(session.as_ref(), turn.as_ref(), call_id, tracker);
            let content = emitter.finish(event_ctx, out).await?;
            session
                .services
                .lsp_session_manager
                .note_external_file_changes(&watched_file_changes);
            Ok(FunctionToolOutput::from_text(content, Some(true)))
        }
    }
}

fn write_permissions_for_paths(file_paths: &[AbsolutePathBuf]) -> Option<PermissionProfile> {
    let write_paths = file_paths
        .iter()
        .map(|path| {
            path.parent()
                .unwrap_or_else(|| path.clone())
                .into_path_buf()
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(AbsolutePathBuf::from_absolute_path)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;

    let permissions = (!write_paths.is_empty()).then_some(PermissionProfile {
        file_system: Some(FileSystemPermissions {
            read: Some(vec![]),
            write: Some(write_paths),
        }),
        ..Default::default()
    })?;

    crate::sandboxing::normalize_additional_permissions(permissions).ok()
}

pub(crate) fn watched_file_changes_for_action(
    action: &ApplyPatchAction,
) -> Vec<LspWatchedFileChange> {
    let cwd = action.cwd.as_path();
    let mut changes = Vec::new();

    for (path, change) in action.changes() {
        let Some(resolved_path) = to_abs_path(cwd, path).map(AbsolutePathBuf::into_path_buf) else {
            continue;
        };

        match change {
            ApplyPatchFileChange::Add { .. } => changes.push(LspWatchedFileChange {
                path: resolved_path,
                kind: LspWatchedFileChangeKind::Created,
            }),
            ApplyPatchFileChange::Delete { .. } => changes.push(LspWatchedFileChange {
                path: resolved_path,
                kind: LspWatchedFileChangeKind::Deleted,
            }),
            ApplyPatchFileChange::Update { move_path, .. } => {
                if let Some(move_path) = move_path
                    && let Some(destination_path) =
                        to_abs_path(cwd, move_path).map(AbsolutePathBuf::into_path_buf)
                {
                    changes.push(LspWatchedFileChange {
                        path: resolved_path,
                        kind: LspWatchedFileChangeKind::Deleted,
                    });
                    changes.push(LspWatchedFileChange {
                        path: destination_path,
                        kind: LspWatchedFileChangeKind::Created,
                    });
                } else {
                    changes.push(LspWatchedFileChange {
                        path: resolved_path,
                        kind: LspWatchedFileChangeKind::Changed,
                    });
                }
            }
        }
    }

    changes
}

pub(crate) fn render_rewrite_patch(
    file_path: &Path,
    old_content: Option<&str>,
    new_content: &str,
) -> String {
    let mut patch = String::from("*** Begin Patch\n");
    match old_content {
        Some(old_content) => {
            patch.push_str(&format!("*** Update File: {}\n", file_path.display()));
            patch.push_str("@@\n");
            for line in patch_lines(old_content) {
                patch.push_str(&format!("-{line}\n"));
            }
            for line in patch_lines(new_content) {
                patch.push_str(&format!("+{line}\n"));
            }
        }
        None => {
            patch.push_str(&format!("*** Add File: {}\n", file_path.display()));
            for line in patch_lines(new_content) {
                patch.push_str(&format!("+{line}\n"));
            }
        }
    }
    patch.push_str("*** End Patch");
    patch
}

pub(crate) fn make_write_action(
    cwd: PathBuf,
    file_path: PathBuf,
    old_content: Option<&str>,
    new_content: String,
) -> Result<ApplyPatchAction, FunctionCallError> {
    let patch = render_rewrite_patch(&file_path, old_content, &new_content);
    let command = vec!["apply_patch".to_string(), patch];
    match codex_apply_patch::maybe_parse_apply_patch_verified(&command, &cwd) {
        codex_apply_patch::MaybeApplyPatchVerified::Body(action) => Ok(action),
        codex_apply_patch::MaybeApplyPatchVerified::CorrectnessError(err) => {
            Err(FunctionCallError::RespondToModel(err.to_string()))
        }
        codex_apply_patch::MaybeApplyPatchVerified::ShellParseError(err) => {
            Err(FunctionCallError::RespondToModel(format!(
                "failed to parse generated apply_patch payload: {err:?}"
            )))
        }
        codex_apply_patch::MaybeApplyPatchVerified::NotApplyPatch => {
            Err(FunctionCallError::RespondToModel(
                "generated payload did not parse as apply_patch".to_string(),
            ))
        }
    }
}

fn to_abs_path(cwd: &Path, path: &Path) -> Option<AbsolutePathBuf> {
    AbsolutePathBuf::resolve_path_against_base(path, cwd).ok()
}

fn patch_lines(content: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = content.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    lines
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;

    use super::*;
    use codex_apply_patch::MaybeApplyPatchVerified;

    #[test]
    fn approval_keys_include_move_destination() {
        let tmp = TempDir::new().expect("tmp");
        let cwd = tmp.path();
        std::fs::create_dir_all(cwd.join("old")).expect("create old dir");
        std::fs::create_dir_all(cwd.join("renamed/dir")).expect("create dest dir");
        std::fs::write(cwd.join("old/name.txt"), "old content\n").expect("write old file");
        let patch = r#"*** Begin Patch
*** Update File: old/name.txt
*** Move to: renamed/dir/name.txt
@@
-old content
+new content
*** End Patch"#;
        let argv = vec!["apply_patch".to_string(), patch.to_string()];
        let action = match codex_apply_patch::maybe_parse_apply_patch_verified(&argv, cwd) {
            MaybeApplyPatchVerified::Body(action) => action,
            other => panic!("expected patch body, got: {other:?}"),
        };

        let keys = file_paths_for_action(&action);
        assert_eq!(keys.len(), 2);
    }

    #[test]
    fn watched_file_changes_map_move_to_delete_and_create() {
        let tmp = TempDir::new().expect("tmp");
        let cwd = tmp.path();
        std::fs::create_dir_all(cwd.join("old")).expect("create old dir");
        std::fs::create_dir_all(cwd.join("renamed/dir")).expect("create dest dir");
        std::fs::write(cwd.join("old/name.txt"), "old content\n").expect("write old file");
        let patch = r#"*** Begin Patch
*** Update File: old/name.txt
*** Move to: renamed/dir/name.txt
@@
-old content
+new content
*** End Patch"#;
        let argv = vec!["apply_patch".to_string(), patch.to_string()];
        let action = match codex_apply_patch::maybe_parse_apply_patch_verified(&argv, cwd) {
            MaybeApplyPatchVerified::Body(action) => action,
            other => panic!("expected patch body, got: {other:?}"),
        };

        let watched_changes = watched_file_changes_for_action(&action);
        assert_eq!(watched_changes.len(), 2);
        assert_eq!(watched_changes[0].kind, LspWatchedFileChangeKind::Deleted);
        assert_eq!(watched_changes[1].kind, LspWatchedFileChangeKind::Created);
    }
}
