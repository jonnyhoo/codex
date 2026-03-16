use codex_protocol::config_types::ModeKind;
use rmcp::model::ToolAnnotations;
use serde::Deserialize;

use crate::config::Config;
use crate::config::types::AppToolApproval;
use crate::config::types::AppsConfigToml;
use crate::tools::spec::ToolsConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AppToolPolicy {
    pub enabled: bool,
    pub approval: AppToolApproval,
}

impl Default for AppToolPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            approval: AppToolApproval::Auto,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CodexAppToolPolicyInput<'a> {
    pub connector_id: Option<&'a str>,
    pub tool_name: &'a str,
    pub tool_title: Option<&'a str>,
    pub annotations: Option<&'a ToolAnnotations>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RuntimeCollaborationToolPolicyContext {
    pub allows_repo_mutation: bool,
    pub update_plan_available: bool,
    pub request_user_input_available: bool,
    pub requires_proposed_plan_block: bool,
    pub streams_proposed_plan: bool,
}

impl RuntimeCollaborationToolPolicyContext {
    pub(crate) fn from_mode_and_tools(mode_kind: ModeKind, tools_config: &ToolsConfig) -> Self {
        Self {
            allows_repo_mutation: mode_kind.allows_repo_mutation(),
            update_plan_available: mode_kind.update_plan_available(),
            request_user_input_available: tools_config.request_user_input
                && mode_kind
                    .request_user_input_available(tools_config.default_mode_request_user_input),
            requires_proposed_plan_block: mode_kind.requires_proposed_plan_block(),
            streams_proposed_plan: mode_kind.streams_proposed_plan(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RuntimeCodexAppsPolicyContext {
    pub apps_configured: bool,
    pub default_app_enabled: bool,
    pub default_destructive_enabled: bool,
    pub default_open_world_enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RuntimeToolPolicyContext {
    pub collaboration: RuntimeCollaborationToolPolicyContext,
    pub codex_apps: RuntimeCodexAppsPolicyContext,
}

pub(crate) fn runtime_tool_policy(
    config: &Config,
    mode_kind: ModeKind,
    tools_config: &ToolsConfig,
) -> RuntimeToolPolicyContext {
    RuntimeToolPolicyContext {
        collaboration: RuntimeCollaborationToolPolicyContext::from_mode_and_tools(
            mode_kind,
            tools_config,
        ),
        codex_apps: runtime_codex_apps_policy(config),
    }
}

pub(crate) fn runtime_codex_apps_policy(config: &Config) -> RuntimeCodexAppsPolicyContext {
    let apps_config = read_apps_config(config);
    runtime_codex_apps_policy_from_apps_config(apps_config.as_ref())
}

fn runtime_codex_apps_policy_from_apps_config(
    apps_config: Option<&AppsConfigToml>,
) -> RuntimeCodexAppsPolicyContext {
    let defaults = apps_config.and_then(|config| config.default.as_ref());
    RuntimeCodexAppsPolicyContext {
        apps_configured: apps_config.is_some(),
        default_app_enabled: defaults.map(|defaults| defaults.enabled).unwrap_or(true),
        default_destructive_enabled: defaults
            .map(|defaults| defaults.destructive_enabled)
            .unwrap_or(true),
        default_open_world_enabled: defaults
            .map(|defaults| defaults.open_world_enabled)
            .unwrap_or(true),
    }
}

pub(crate) fn app_tool_policy(
    config: &Config,
    input: CodexAppToolPolicyInput<'_>,
) -> AppToolPolicy {
    let apps_config = read_apps_config(config);
    app_tool_policy_from_apps_config(apps_config.as_ref(), input)
}

pub(crate) fn read_apps_config(config: &Config) -> Option<AppsConfigToml> {
    let effective_config = config.config_layer_stack.effective_config();
    let apps_config = effective_config.as_table()?.get("apps")?.clone();
    AppsConfigToml::deserialize(apps_config).ok()
}

pub(crate) fn app_is_enabled(apps_config: &AppsConfigToml, connector_id: Option<&str>) -> bool {
    let default_enabled = apps_config
        .default
        .as_ref()
        .map(|defaults| defaults.enabled)
        .unwrap_or(true);

    connector_id
        .and_then(|connector_id| apps_config.apps.get(connector_id))
        .map(|app| app.enabled)
        .unwrap_or(default_enabled)
}

pub(crate) fn app_tool_policy_from_apps_config(
    apps_config: Option<&AppsConfigToml>,
    input: CodexAppToolPolicyInput<'_>,
) -> AppToolPolicy {
    let Some(apps_config) = apps_config else {
        return AppToolPolicy::default();
    };

    let CodexAppToolPolicyInput {
        connector_id,
        tool_name,
        tool_title,
        annotations,
    } = input;

    let app = connector_id.and_then(|connector_id| apps_config.apps.get(connector_id));
    let tools = app.and_then(|app| app.tools.as_ref());
    let tool_config = tools.and_then(|tools| {
        tools
            .tools
            .get(tool_name)
            .or_else(|| tool_title.and_then(|title| tools.tools.get(title)))
    });
    let approval = tool_config
        .and_then(|tool| tool.approval_mode)
        .or_else(|| app.and_then(|app| app.default_tools_approval_mode))
        .unwrap_or(AppToolApproval::Auto);

    if !app_is_enabled(apps_config, connector_id) {
        return AppToolPolicy {
            enabled: false,
            approval,
        };
    }

    if let Some(enabled) = tool_config.and_then(|tool| tool.enabled) {
        return AppToolPolicy { enabled, approval };
    }

    if let Some(enabled) = app.and_then(|app| app.default_tools_enabled) {
        return AppToolPolicy { enabled, approval };
    }

    let app_defaults = apps_config.default.as_ref();
    let destructive_enabled = app
        .and_then(|app| app.destructive_enabled)
        .unwrap_or_else(|| {
            app_defaults
                .map(|defaults| defaults.destructive_enabled)
                .unwrap_or(true)
        });
    let open_world_enabled = app
        .and_then(|app| app.open_world_enabled)
        .unwrap_or_else(|| {
            app_defaults
                .map(|defaults| defaults.open_world_enabled)
                .unwrap_or(true)
        });
    let destructive_hint = annotations
        .and_then(|annotations| annotations.destructive_hint)
        .unwrap_or(false);
    let open_world_hint = annotations
        .and_then(|annotations| annotations.open_world_hint)
        .unwrap_or(false);
    let enabled =
        (destructive_enabled || !destructive_hint) && (open_world_enabled || !open_world_hint);

    AppToolPolicy { enabled, approval }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::AppConfig;
    use crate::config::types::AppsDefaultConfig;
    use pretty_assertions::assert_eq;
    use std::collections::HashMap;

    fn input<'a>(
        connector_id: Option<&'a str>,
        tool_name: &'a str,
        tool_title: Option<&'a str>,
    ) -> CodexAppToolPolicyInput<'a> {
        CodexAppToolPolicyInput {
            connector_id,
            tool_name,
            tool_title,
            annotations: None,
        }
    }

    #[test]
    fn runtime_codex_apps_policy_defaults_when_unconfigured() {
        assert_eq!(
            runtime_codex_apps_policy_from_apps_config(None),
            RuntimeCodexAppsPolicyContext {
                apps_configured: false,
                default_app_enabled: true,
                default_destructive_enabled: true,
                default_open_world_enabled: true,
            }
        );
    }

    #[test]
    fn runtime_codex_apps_policy_uses_configured_defaults() {
        let apps_config = AppsConfigToml {
            default: Some(AppsDefaultConfig {
                enabled: false,
                destructive_enabled: false,
                open_world_enabled: true,
            }),
            apps: HashMap::new(),
        };

        assert_eq!(
            runtime_codex_apps_policy_from_apps_config(Some(&apps_config)),
            RuntimeCodexAppsPolicyContext {
                apps_configured: true,
                default_app_enabled: false,
                default_destructive_enabled: false,
                default_open_world_enabled: true,
            }
        );
    }

    #[test]
    fn app_tool_policy_honors_disabled_app_and_approval_override() {
        let apps_config = AppsConfigToml {
            default: None,
            apps: HashMap::from([(
                "calendar".to_string(),
                AppConfig {
                    enabled: false,
                    default_tools_approval_mode: Some(AppToolApproval::Prompt),
                    ..Default::default()
                },
            )]),
        };

        assert_eq!(
            app_tool_policy_from_apps_config(
                Some(&apps_config),
                input(Some("calendar"), "create_event", None),
            ),
            AppToolPolicy {
                enabled: false,
                approval: AppToolApproval::Prompt,
            }
        );
    }
}
