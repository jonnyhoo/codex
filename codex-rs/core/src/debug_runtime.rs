use std::fmt::Write as _;

use codex_protocol::protocol::AgentStatus;

use crate::InstructionAudience;
use crate::InstructionPriority;
use crate::InstructionSection;
use crate::InstructionSource;
use crate::RuntimeContext;
use crate::truncate::TruncationPolicy;

pub(crate) fn render_debug_runtime_text(runtime: &RuntimeContext) -> String {
    let mut out = String::new();
    writeln!(out, "/debug-runtime").ok();
    writeln!(out).ok();

    writeln!(out, "Session:").ok();
    write_key_value(&mut out, "session_id", runtime.session_id.to_string());
    write_key_value(&mut out, "turn_id", runtime.turn_id.as_str());
    write_optional(&mut out, "trace_id", runtime.trace_id.as_deref());
    write_key_value(
        &mut out,
        "session_source",
        runtime.session_source.to_string(),
    );
    write_key_value(&mut out, "cwd", runtime.cwd.display().to_string());
    write_optional(&mut out, "current_date", runtime.current_date.as_deref());
    write_optional(&mut out, "timezone", runtime.timezone.as_deref());
    write_optional(
        &mut out,
        "app_server_client_name",
        runtime.app_server_client_name.as_deref(),
    );

    writeln!(out).ok();
    writeln!(out, "Agent:").ok();
    write_optional_owned(
        &mut out,
        "agent_id",
        runtime.agent.agent_id.as_ref().map(ToString::to_string),
    );
    write_optional_owned(
        &mut out,
        "parent_session_id",
        runtime
            .agent
            .parent_session_id
            .as_ref()
            .map(ToString::to_string),
    );
    write_optional_owned(
        &mut out,
        "depth",
        runtime.agent.depth.map(|depth| depth.to_string()),
    );
    write_optional(
        &mut out,
        "agent_nickname",
        runtime.agent.agent_nickname.as_deref(),
    );
    write_optional(&mut out, "agent_role", runtime.agent.agent_role.as_deref());
    if runtime.agent.subagents.is_empty() {
        write_key_value(&mut out, "subagents", "0");
    } else {
        writeln!(out, "  - subagents: {}", runtime.agent.subagents.len()).ok();
        for (index, subagent) in runtime.agent.subagents.iter().enumerate() {
            writeln!(
                out,
                "    {}. thread_id={} nickname={} role={} status={}",
                index + 1,
                subagent.thread_id,
                subagent.agent_nickname.as_deref().unwrap_or("<none>"),
                subagent.agent_role.as_deref().unwrap_or("<none>"),
                format_agent_status(&subagent.status),
            )
            .ok();
        }
    }

    writeln!(out).ok();
    writeln!(out, "Model:").ok();
    write_key_value(&mut out, "slug", runtime.model.slug.as_str());
    write_key_value(
        &mut out,
        "provider_name",
        runtime.model.provider.name.as_str(),
    );
    write_optional(
        &mut out,
        "provider_base_url",
        runtime.model.provider.base_url.as_deref(),
    );
    write_key_value(
        &mut out,
        "provider_wire_api",
        format!("{:?}", runtime.model.provider.wire_api),
    );
    write_optional_owned(
        &mut out,
        "reasoning_effort",
        runtime
            .model
            .reasoning_effort
            .map(|effort| effort.to_string()),
    );
    write_key_value(
        &mut out,
        "reasoning_summary",
        runtime.model.reasoning_summary.to_string(),
    );
    write_optional_owned(
        &mut out,
        "personality",
        runtime
            .model
            .personality
            .map(|personality| personality.to_string()),
    );

    writeln!(out).ok();
    writeln!(out, "Instructions:").ok();
    write_optional(
        &mut out,
        "developer_instructions",
        runtime.instructions.developer_instructions.as_deref(),
    );
    write_optional(
        &mut out,
        "user_instructions",
        runtime.instructions.user_instructions.as_deref(),
    );
    write_optional(
        &mut out,
        "compact_prompt",
        runtime.instructions.compact_prompt.as_deref(),
    );
    write_instruction_sections(
        &mut out,
        "user_instruction_sections",
        &runtime.instructions.user_instruction_sections,
    );
    if let Some(resolved_layers) = &runtime.instructions.resolved_layers {
        writeln!(
            out,
            "  - resolved_layers: {} section(s)",
            resolved_layers.sections.len()
        )
        .ok();
        writeln!(out, "    - base_instructions:").ok();
        write_indented_text(&mut out, &resolved_layers.base_instructions, "      ");
        for (index, section) in resolved_layers.sections.iter().enumerate() {
            writeln!(
                out,
                "    {}. audience={} priority={} source={}",
                index + 1,
                format_instruction_audience(section.audience),
                format_instruction_priority(section.priority),
                format_instruction_source(section.source),
            )
            .ok();
            writeln!(out, "       text:").ok();
            write_indented_text(&mut out, &section.text, "         ");
        }
    } else {
        write_key_value(&mut out, "resolved_layers", "<none>");
    }

    writeln!(out).ok();
    writeln!(out, "Collaboration:").ok();
    write_key_value(
        &mut out,
        "mode_kind",
        format_mode_kind(runtime.collaboration.mode_kind),
    );
    write_key_value(
        &mut out,
        "collaboration_model",
        runtime.collaboration.collaboration_mode.model(),
    );
    write_optional_owned(
        &mut out,
        "collaboration_reasoning_effort",
        runtime
            .collaboration
            .collaboration_mode
            .reasoning_effort()
            .map(|effort| effort.to_string()),
    );
    write_optional(
        &mut out,
        "collaboration_developer_instructions",
        runtime
            .collaboration
            .collaboration_mode
            .settings
            .developer_instructions
            .as_deref(),
    );
    write_key_value(
        &mut out,
        "realtime_active",
        runtime.collaboration.realtime_active.to_string(),
    );
    writeln!(out, "  - tool_policy:").ok();
    write_key_value(
        &mut out,
        "    collaboration.allows_repo_mutation",
        runtime
            .tools
            .tool_policy
            .collaboration
            .allows_repo_mutation
            .to_string(),
    );
    write_key_value(
        &mut out,
        "    collaboration.update_plan_available",
        runtime
            .tools
            .tool_policy
            .collaboration
            .update_plan_available
            .to_string(),
    );
    write_key_value(
        &mut out,
        "    collaboration.request_user_input_available",
        runtime
            .tools
            .tool_policy
            .collaboration
            .request_user_input_available
            .to_string(),
    );
    write_key_value(
        &mut out,
        "    collaboration.requires_proposed_plan_block",
        runtime
            .tools
            .tool_policy
            .collaboration
            .requires_proposed_plan_block
            .to_string(),
    );
    write_key_value(
        &mut out,
        "    collaboration.streams_proposed_plan",
        runtime
            .tools
            .tool_policy
            .collaboration
            .streams_proposed_plan
            .to_string(),
    );
    write_key_value(
        &mut out,
        "    codex_apps.apps_configured",
        runtime
            .tools
            .tool_policy
            .codex_apps
            .apps_configured
            .to_string(),
    );
    write_key_value(
        &mut out,
        "    codex_apps.default_app_enabled",
        runtime
            .tools
            .tool_policy
            .codex_apps
            .default_app_enabled
            .to_string(),
    );
    write_key_value(
        &mut out,
        "    codex_apps.default_destructive_enabled",
        runtime
            .tools
            .tool_policy
            .codex_apps
            .default_destructive_enabled
            .to_string(),
    );
    write_key_value(
        &mut out,
        "    codex_apps.default_open_world_enabled",
        runtime
            .tools
            .tool_policy
            .codex_apps
            .default_open_world_enabled
            .to_string(),
    );

    writeln!(out).ok();
    writeln!(out, "Execution:").ok();
    write_key_value(
        &mut out,
        "approval_policy",
        runtime.execution.approval_policy.get().to_string(),
    );
    write_key_value(
        &mut out,
        "sandbox_policy",
        runtime.execution.sandbox_policy.get().to_string(),
    );
    write_key_value(
        &mut out,
        "shell_environment_policy",
        format_shell_environment_policy(&runtime.execution.shell_environment_policy),
    );
    write_key_value(
        &mut out,
        "windows_sandbox_level",
        format!("{:?}", runtime.execution.windows_sandbox_level),
    );
    if let Some(network) = &runtime.execution.network {
        writeln!(
            out,
            "  - network: allowed_domains={} denied_domains={}",
            format_list(&network.allowed_domains),
            format_list(&network.denied_domains),
        )
        .ok();
    } else {
        write_key_value(&mut out, "network", "<none>");
    }
    write_key_value(
        &mut out,
        "final_output_json_schema",
        runtime
            .execution
            .final_output_json_schema
            .as_ref()
            .map(|value| format!("present ({} chars)", value.to_string().len()))
            .unwrap_or_else(|| "<none>".to_string()),
    );
    write_key_value(
        &mut out,
        "truncation_policy",
        format_truncation_policy(runtime.execution.truncation_policy),
    );

    writeln!(out).ok();
    writeln!(out, "Tools:").ok();
    write_key_value(
        &mut out,
        "shell_type",
        format!("{:?}", runtime.tools.tools_config.shell_type),
    );
    write_key_value(
        &mut out,
        "unified_exec_backend",
        format!("{:?}", runtime.tools.tools_config.unified_exec_backend),
    );
    write_key_value(
        &mut out,
        "allow_login_shell",
        runtime.tools.tools_config.allow_login_shell.to_string(),
    );
    write_key_value(
        &mut out,
        "apply_patch_tool_type",
        format!("{:?}", runtime.tools.tools_config.apply_patch_tool_type),
    );
    write_key_value(
        &mut out,
        "web_search_mode",
        format!("{:?}", runtime.tools.tools_config.web_search_mode),
    );
    write_key_value(
        &mut out,
        "web_search_config_present",
        runtime
            .tools
            .tools_config
            .web_search_config
            .is_some()
            .to_string(),
    );
    write_key_value(
        &mut out,
        "web_search_tool_type",
        format!("{:?}", runtime.tools.tools_config.web_search_tool_type),
    );
    write_key_value(
        &mut out,
        "image_gen_tool",
        runtime.tools.tools_config.image_gen_tool.to_string(),
    );
    write_key_value(
        &mut out,
        "search_tool",
        runtime.tools.tools_config.search_tool.to_string(),
    );
    write_key_value(
        &mut out,
        "request_permission_enabled",
        runtime
            .tools
            .tools_config
            .request_permission_enabled
            .to_string(),
    );
    write_key_value(
        &mut out,
        "request_permissions_tool_enabled",
        runtime
            .tools
            .tools_config
            .request_permissions_tool_enabled
            .to_string(),
    );
    write_key_value(
        &mut out,
        "code_mode_enabled",
        runtime.tools.tools_config.code_mode_enabled.to_string(),
    );
    write_key_value(
        &mut out,
        "js_repl_enabled",
        runtime.tools.tools_config.js_repl_enabled.to_string(),
    );
    write_key_value(
        &mut out,
        "js_repl_tools_only",
        runtime.tools.tools_config.js_repl_tools_only.to_string(),
    );
    write_key_value(
        &mut out,
        "collab_tools",
        runtime.tools.tools_config.collab_tools.to_string(),
    );
    write_key_value(
        &mut out,
        "artifact_tools",
        runtime.tools.tools_config.artifact_tools.to_string(),
    );
    write_key_value(
        &mut out,
        "request_user_input",
        runtime.tools.tools_config.request_user_input.to_string(),
    );
    write_key_value(
        &mut out,
        "default_mode_request_user_input",
        runtime
            .tools
            .tools_config
            .default_mode_request_user_input
            .to_string(),
    );
    write_key_value(
        &mut out,
        "experimental_supported_tools",
        format_list(&runtime.tools.tools_config.experimental_supported_tools),
    );
    write_key_value(
        &mut out,
        "agent_jobs_tools",
        runtime.tools.tools_config.agent_jobs_tools.to_string(),
    );
    write_key_value(
        &mut out,
        "agent_jobs_worker_tools",
        runtime
            .tools
            .tools_config
            .agent_jobs_worker_tools
            .to_string(),
    );
    write_key_value(
        &mut out,
        "agent_roles",
        format_list(
            &runtime
                .tools
                .tools_config
                .agent_roles
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
        ),
    );

    out
}

fn write_key_value(out: &mut String, key: &str, value: impl AsRef<str>) {
    writeln!(out, "  - {key}: {}", value.as_ref()).ok();
}

fn write_optional(out: &mut String, key: &str, value: Option<&str>) {
    match value {
        Some(text) => {
            writeln!(out, "  - {key}:").ok();
            write_indented_text(out, text, "    ");
        }
        None => write_key_value(out, key, "<none>"),
    }
}

fn write_optional_owned(out: &mut String, key: &str, value: Option<String>) {
    match value {
        Some(text) => {
            writeln!(out, "  - {key}:").ok();
            write_indented_text(out, &text, "    ");
        }
        None => write_key_value(out, key, "<none>"),
    }
}

fn write_instruction_sections(out: &mut String, label: &str, sections: &[InstructionSection]) {
    writeln!(out, "  - {label}: {} section(s)", sections.len()).ok();
    for (index, section) in sections.iter().enumerate() {
        writeln!(
            out,
            "    {}. audience={} priority={} source={}",
            index + 1,
            format_instruction_audience(section.audience),
            format_instruction_priority(section.priority),
            format_instruction_source(section.source),
        )
        .ok();
        writeln!(out, "       text:").ok();
        write_indented_text(out, &section.text, "         ");
    }
}

fn write_indented_text(out: &mut String, text: &str, indent: &str) {
    if text.is_empty() {
        writeln!(out, "{indent}<empty>").ok();
        return;
    }

    for line in text.lines() {
        writeln!(out, "{indent}{line}").ok();
    }
}

fn format_instruction_audience(audience: InstructionAudience) -> &'static str {
    match audience {
        InstructionAudience::Developer => "developer",
        InstructionAudience::ContextualUser => "contextual-user",
    }
}

fn format_instruction_priority(priority: InstructionPriority) -> &'static str {
    match priority {
        InstructionPriority::System => "system",
        InstructionPriority::Developer => "developer",
        InstructionPriority::Mode => "mode",
        InstructionPriority::Repo => "repo",
        InstructionPriority::Skill => "skill",
        InstructionPriority::User => "user",
        InstructionPriority::Runtime => "runtime",
    }
}

fn format_instruction_source(source: InstructionSource) -> &'static str {
    match source {
        InstructionSource::ModelSwitch => "model-switch",
        InstructionSource::PlatformPolicy => "platform-policy",
        InstructionSource::DeveloperOverride => "developer-override",
        InstructionSource::MemoryTool => "memory-tool",
        InstructionSource::CollaborationMode => "collaboration-mode",
        InstructionSource::RealtimeContext => "realtime-context",
        InstructionSource::Personality => "personality",
        InstructionSource::Apps => "apps",
        InstructionSource::CommitMessage => "commit-message",
        InstructionSource::UserConfig => "user-config",
        InstructionSource::ProjectDoc => "project-doc",
        InstructionSource::JsRepl => "js-repl",
        InstructionSource::Plugins => "plugins",
        InstructionSource::CodeMode => "code-mode",
        InstructionSource::Skills => "skills",
        InstructionSource::ChildAgents => "child-agents",
        InstructionSource::EnvironmentContext => "environment-context",
    }
}

fn format_mode_kind(mode_kind: codex_protocol::config_types::ModeKind) -> &'static str {
    match mode_kind {
        codex_protocol::config_types::ModeKind::Default => "default",
        codex_protocol::config_types::ModeKind::Plan => "plan",
        _ => "unknown",
    }
}

fn format_agent_status(status: &AgentStatus) -> String {
    match status {
        AgentStatus::PendingInit => "pending_init".to_string(),
        AgentStatus::Running => "running".to_string(),
        AgentStatus::Completed(message) => match message {
            Some(message) => format!("completed({message})"),
            None => "completed".to_string(),
        },
        AgentStatus::Errored(error) => format!("errored({error})"),
        AgentStatus::Shutdown => "shutdown".to_string(),
        AgentStatus::NotFound => "not_found".to_string(),
    }
}

fn format_shell_environment_policy(
    policy: &crate::config::types::ShellEnvironmentPolicy,
) -> String {
    format!(
        "inherit={:?}, ignore_default_excludes={}, exclude={}, set={}, include_only={}, use_profile={}",
        policy.inherit,
        policy.ignore_default_excludes,
        policy.exclude.len(),
        policy.r#set.len(),
        policy.include_only.len(),
        policy.use_profile,
    )
}

fn format_truncation_policy(policy: TruncationPolicy) -> String {
    match policy {
        TruncationPolicy::Bytes(bytes) => format!("bytes({bytes})"),
        TruncationPolicy::Tokens(tokens) => format!("tokens({tokens})"),
    }
}

fn format_list(values: &[String]) -> String {
    if values.is_empty() {
        "<none>".to_string()
    } else {
        values.join(", ")
    }
}
