//! Apply Patch runtime: executes verified patches under the orchestrator.
//!
//! Assumes `apply_patch` verification/approval happened upstream. Reuses that
//! decision to avoid re-prompting, builds the self-invocation command for
//! `codex --codex-run-as-apply-patch`, and runs under the current
//! `SandboxAttempt` with a minimal environment.
use crate::exec::ExecToolCallOutput;
use crate::exec::SandboxType;
use crate::exec::StreamOutput;
use crate::sandboxing::CommandSpec;
use crate::sandboxing::SandboxPermissions;
use crate::sandboxing::execute_env;
use crate::tools::sandboxing::Approvable;
use crate::tools::sandboxing::ApprovalCtx;
use crate::tools::sandboxing::ExecApprovalRequirement;
use crate::tools::sandboxing::SandboxAttempt;
use crate::tools::sandboxing::Sandboxable;
use crate::tools::sandboxing::SandboxablePreference;
use crate::tools::sandboxing::ToolCtx;
use crate::tools::sandboxing::ToolError;
use crate::tools::sandboxing::ToolRuntime;
use crate::tools::sandboxing::with_cached_approval;
use codex_apply_patch::ApplyPatchAction;
use codex_apply_patch::CODEX_CORE_APPLY_PATCH_FILE_ARG1;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::FileChange;
use codex_protocol::protocol::ReviewDecision;
use codex_utils_absolute_path::AbsolutePathBuf;
use futures::future::BoxFuture;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

#[derive(Debug)]
pub struct ApplyPatchRequest {
    pub action: ApplyPatchAction,
    pub file_paths: Vec<AbsolutePathBuf>,
    pub changes: std::collections::HashMap<PathBuf, FileChange>,
    pub exec_approval_requirement: ExecApprovalRequirement,
    pub timeout_ms: Option<u64>,
    pub codex_exe: Option<PathBuf>,
}

#[derive(Default)]
pub struct ApplyPatchRuntime;

impl ApplyPatchRuntime {
    pub fn new() -> Self {
        Self
    }

    fn build_command_spec(
        req: &ApplyPatchRequest,
        manifest_path: PathBuf,
    ) -> Result<CommandSpec, ToolError> {
        use std::env;
        let exe = if let Some(path) = &req.codex_exe {
            path.clone()
        } else {
            env::current_exe()
                .map_err(|e| ToolError::Rejected(format!("failed to determine codex exe: {e}")))?
        };
        let program = exe.to_string_lossy().to_string();
        Ok(CommandSpec {
            program,
            args: vec![
                CODEX_CORE_APPLY_PATCH_FILE_ARG1.to_string(),
                manifest_path.display().to_string(),
            ],
            cwd: req.action.cwd.clone(),
            expiration: req.timeout_ms.into(),
            // Run apply_patch with a minimal environment for determinism and to avoid leaks.
            env: HashMap::new(),
            sandbox_permissions: SandboxPermissions::UseDefault,
            additional_permissions: None,
            justification: None,
        })
    }

    fn stdout_stream(ctx: &ToolCtx) -> Option<crate::exec::StdoutStream> {
        Some(crate::exec::StdoutStream {
            sub_id: ctx.turn.sub_id.clone(),
            call_id: ctx.call_id.clone(),
            tx_event: ctx.session.get_tx_event(),
        })
    }

    fn apply_in_process(action: &ApplyPatchAction) -> ExecToolCallOutput {
        let started_at = Instant::now();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        match codex_apply_patch::apply_action(action, &mut stdout, &mut stderr) {
            Ok(()) => {
                let stdout = String::from_utf8_lossy(&stdout).into_owned();
                ExecToolCallOutput {
                    exit_code: 0,
                    stdout: StreamOutput::new(stdout.clone()),
                    stderr: StreamOutput::new(String::new()),
                    aggregated_output: StreamOutput::new(stdout),
                    duration: started_at.elapsed(),
                    timed_out: false,
                }
            }
            Err(err) => {
                let stderr = if stderr.is_empty() {
                    err.to_string()
                } else {
                    String::from_utf8_lossy(&stderr).into_owned()
                };
                Self::failed_output(started_at.elapsed(), stderr)
            }
        }
    }

    fn failed_output(duration: Duration, message: String) -> ExecToolCallOutput {
        let stderr = if message.ends_with('\n') {
            message
        } else {
            format!("{message}\n")
        };
        ExecToolCallOutput {
            exit_code: 1,
            stdout: StreamOutput::new(String::new()),
            stderr: StreamOutput::new(stderr.clone()),
            aggregated_output: StreamOutput::new(stderr),
            duration,
            timed_out: false,
        }
    }
}

impl Sandboxable for ApplyPatchRuntime {
    fn sandbox_preference(&self) -> SandboxablePreference {
        SandboxablePreference::Auto
    }
    fn escalate_on_failure(&self) -> bool {
        true
    }
}

impl Approvable<ApplyPatchRequest> for ApplyPatchRuntime {
    type ApprovalKey = AbsolutePathBuf;

    fn approval_keys(&self, req: &ApplyPatchRequest) -> Vec<Self::ApprovalKey> {
        req.file_paths.clone()
    }

    fn start_approval_async<'a>(
        &'a mut self,
        req: &'a ApplyPatchRequest,
        ctx: ApprovalCtx<'a>,
    ) -> BoxFuture<'a, ReviewDecision> {
        let session = ctx.session;
        let turn = ctx.turn;
        let call_id = ctx.call_id.to_string();
        let retry_reason = ctx.retry_reason.clone();
        let approval_keys = self.approval_keys(req);
        let changes = req.changes.clone();
        Box::pin(async move {
            if let Some(reason) = retry_reason {
                let rx_approve = session
                    .request_patch_approval(turn, call_id, changes.clone(), Some(reason), None)
                    .await;
                return rx_approve.await.unwrap_or_default();
            }

            with_cached_approval(
                &session.services,
                "apply_patch",
                approval_keys,
                || async move {
                    let rx_approve = session
                        .request_patch_approval(turn, call_id, changes, None, None)
                        .await;
                    rx_approve.await.unwrap_or_default()
                },
            )
            .await
        })
    }

    fn wants_no_sandbox_approval(&self, policy: AskForApproval) -> bool {
        match policy {
            AskForApproval::Never => false,
            AskForApproval::Reject(reject_config) => !reject_config.rejects_sandbox_approval(),
            AskForApproval::OnFailure => true,
            AskForApproval::OnRequest => true,
            AskForApproval::UnlessTrusted => true,
        }
    }

    // apply_patch approvals are decided upstream by assess_patch_safety.
    //
    // This override ensures the orchestrator runs the patch approval flow when required instead
    // of falling back to the global exec approval policy.
    fn exec_approval_requirement(
        &self,
        req: &ApplyPatchRequest,
    ) -> Option<ExecApprovalRequirement> {
        Some(req.exec_approval_requirement.clone())
    }
}

impl ToolRuntime<ApplyPatchRequest, ExecToolCallOutput> for ApplyPatchRuntime {
    async fn run(
        &mut self,
        req: &ApplyPatchRequest,
        attempt: &SandboxAttempt<'_>,
        ctx: &ToolCtx,
    ) -> Result<ExecToolCallOutput, ToolError> {
        if attempt.sandbox == SandboxType::None {
            // Avoid command-line length failures by applying verified absolute-path changes
            // directly when no sandbox execution is required.
            return Ok(Self::apply_in_process(&req.action));
        }

        let manifest_dir = tempfile::Builder::new()
            .prefix("codex-apply-patch-")
            .tempdir_in(&req.action.cwd)
            .map_err(|err| {
                ToolError::Rejected(format!("failed to create patch tempdir in cwd: {err}"))
            })?;
        let manifest_path = manifest_dir.path().join("payload.patch");
        std::fs::write(&manifest_path, &req.action.patch).map_err(|err| {
            ToolError::Rejected(format!(
                "failed to write patch payload {}: {err}",
                manifest_path.display()
            ))
        })?;

        let spec = Self::build_command_spec(req, manifest_path)?;
        let env = attempt
            .env_for(spec, None)
            .map_err(|err| ToolError::Codex(err.into()))?;
        let out = execute_env(env, Self::stdout_stream(ctx))
            .await
            .map_err(ToolError::Codex)?;
        drop(manifest_dir);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_apply_patch::MaybeApplyPatchVerified;
    use codex_protocol::protocol::RejectConfig;
    use tempfile::tempdir;

    #[test]
    fn wants_no_sandbox_approval_reject_respects_sandbox_flag() {
        let runtime = ApplyPatchRuntime::new();
        assert!(runtime.wants_no_sandbox_approval(AskForApproval::OnRequest));
        assert!(
            !runtime.wants_no_sandbox_approval(AskForApproval::Reject(RejectConfig {
                sandbox_approval: true,
                rules: false,
                mcp_elicitations: false,
            }))
        );
        assert!(
            runtime.wants_no_sandbox_approval(AskForApproval::Reject(RejectConfig {
                sandbox_approval: false,
                rules: false,
                mcp_elicitations: false,
            }))
        );
    }

    #[test]
    fn apply_in_process_handles_large_verified_patch() {
        let dir = tempdir().expect("tmpdir");
        let content = "x".repeat(40_000);
        let patch =
            format!("*** Begin Patch\n*** Add File: nested/large.txt\n+{content}\n*** End Patch");
        let argv = vec!["apply_patch".to_string(), patch];
        let action = match codex_apply_patch::maybe_parse_apply_patch_verified(&argv, dir.path()) {
            MaybeApplyPatchVerified::Body(action) => action,
            other => panic!("expected verified patch action, got {other:?}"),
        };

        let output = ApplyPatchRuntime::apply_in_process(&action);
        assert_eq!(output.exit_code, 0);

        let written = std::fs::read_to_string(dir.path().join("nested").join("large.txt"))
            .expect("read patched file");
        assert_eq!(written, format!("{content}\n"));
        assert!(output.stderr.text.is_empty());
        assert!(output.stdout.text.contains("large.txt"));
    }
}
