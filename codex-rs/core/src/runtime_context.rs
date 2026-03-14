use std::path::PathBuf;

use serde_json::Value;

use crate::config::Constrained;
use crate::config::types::ShellEnvironmentPolicy;
use crate::model_provider_info::ModelProviderInfo;
use crate::protocol::AskForApproval;
use crate::protocol::SandboxPolicy;
use crate::tools::spec::ToolsConfig;
use crate::truncate::TruncationPolicy;
use codex_protocol::ThreadId;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Personality;
use codex_protocol::config_types::ReasoningSummary as ReasoningSummaryConfig;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::TurnContextNetworkItem;

/// Snapshot of runtime-scoped session/turn state needed by tools, agents, and prompt assembly.
///
/// This intentionally mirrors already-existing fields that are currently scattered across
/// `Session`, `TurnContext`, and tool-specific config structs. It does not introduce new behavior;
/// it gives the runtime a single read model to evolve against.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RuntimeContext {
    pub session_id: ThreadId,
    pub turn_id: String,
    pub trace_id: Option<String>,
    pub session_source: SessionSource,
    pub cwd: PathBuf,
    pub current_date: Option<String>,
    pub timezone: Option<String>,
    pub app_server_client_name: Option<String>,
    pub model: RuntimeModelContext,
    pub instructions: RuntimeInstructionContext,
    pub collaboration: RuntimeCollaborationContext,
    pub execution: RuntimeExecutionContext,
    pub tools: RuntimeToolsContext,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RuntimeModelContext {
    pub slug: String,
    pub provider: ModelProviderInfo,
    pub reasoning_effort: Option<ReasoningEffortConfig>,
    pub reasoning_summary: ReasoningSummaryConfig,
    pub personality: Option<Personality>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RuntimeInstructionContext {
    pub developer_instructions: Option<String>,
    pub user_instructions: Option<String>,
    pub compact_prompt: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RuntimeCollaborationContext {
    pub mode_kind: ModeKind,
    pub collaboration_mode: CollaborationMode,
    pub realtime_active: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RuntimeExecutionContext {
    pub approval_policy: Constrained<AskForApproval>,
    pub sandbox_policy: Constrained<SandboxPolicy>,
    pub shell_environment_policy: ShellEnvironmentPolicy,
    pub windows_sandbox_level: WindowsSandboxLevel,
    pub network: Option<TurnContextNetworkItem>,
    pub final_output_json_schema: Option<Value>,
    pub truncation_policy: TruncationPolicy,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RuntimeToolsContext {
    pub tools_config: ToolsConfig,
}
