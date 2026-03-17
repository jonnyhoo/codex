use crate::history_cell::PlainHistoryCell;
use crate::parse_latest_turn_context_from_rollout_text;
use codex_app_server_protocol::ConfigLayerSource;
use codex_core::config::Config;
use codex_core::config_loader::ConfigLayerEntry;
use codex_core::config_loader::ConfigLayerStack;
use codex_core::config_loader::ConfigLayerStackOrdering;
use codex_core::config_loader::NetworkConstraints;
use codex_core::config_loader::RequirementSource;
use codex_core::config_loader::ResidencyRequirement;
use codex_core::config_loader::SandboxModeRequirement;
use codex_core::config_loader::WebSearchModeRequirement;
use codex_protocol::protocol::InstructionSection;
use codex_protocol::protocol::ResolvedInstructionLayers;
use codex_protocol::protocol::SessionNetworkProxyRuntime;
use codex_protocol::protocol::TurnContextItem;
use codex_protocol::protocol::TurnContextToolPolicy;
use ratatui::style::Stylize;
use ratatui::text::Line;
use std::path::Path;
use toml::Value as TomlValue;

pub(crate) fn new_debug_config_output(
    config: &Config,
    session_network_proxy: Option<&SessionNetworkProxyRuntime>,
    rollout_path: Option<&Path>,
) -> PlainHistoryCell {
    let mut lines = render_debug_config_lines(&config.config_layer_stack);
    lines.extend(render_turn_context_snapshot_lines(rollout_path));

    if let Some(proxy) = session_network_proxy {
        lines.push("".into());
        lines.push("Session runtime:".bold().into());
        lines.push("  - network_proxy".into());
        let SessionNetworkProxyRuntime {
            http_addr,
            socks_addr,
        } = proxy;
        let all_proxy = session_all_proxy_url(
            http_addr,
            socks_addr,
            config
                .permissions
                .network
                .as_ref()
                .is_some_and(codex_core::config::NetworkProxySpec::socks_enabled),
        );
        lines.push(format!("    - HTTP_PROXY  = http://{http_addr}").into());
        lines.push(format!("    - ALL_PROXY   = {all_proxy}").into());
    }

    PlainHistoryCell::new(lines)
}

pub(crate) fn new_debug_runtime_output(text: &str) -> PlainHistoryCell {
    PlainHistoryCell::new(text.lines().map(|line| line.to_string().into()).collect())
}

fn render_turn_context_snapshot_lines(rollout_path: Option<&Path>) -> Vec<Line<'static>> {
    let Some(rollout_path) = rollout_path else {
        return Vec::new();
    };

    let mut lines = vec!["".into(), "Latest persisted turn context:".bold().into()];
    lines.push(format!("  - rollout_path: {}", rollout_path.display()).into());

    let snapshot = std::fs::read_to_string(rollout_path)
        .ok()
        .and_then(|text| parse_latest_turn_context_from_rollout_text(&text));
    let Some(snapshot) = snapshot else {
        lines.push("  - snapshot: <unavailable>".dim().into());
        return lines;
    };

    lines.extend(render_turn_context_summary_lines(&snapshot));
    lines
}

fn render_turn_context_summary_lines(snapshot: &TurnContextItem) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    let turn_id = snapshot.turn_id.as_deref().unwrap_or("<none>");
    lines.push(format!("  - turn_id: {turn_id}").into());
    lines.push(format!("  - model: {}", snapshot.model).into());
    if let Some(collaboration_mode) = snapshot.collaboration_mode.as_ref() {
        lines.push(
            format!(
                "  - collaboration_mode: {}",
                collaboration_mode.mode.display_name()
            )
            .into(),
        );
    }
    if let Some(realtime_active) = snapshot.realtime_active {
        lines.push(format!("  - realtime_active: {realtime_active}").into());
    }
    if let Some(tool_policy) = snapshot.tool_policy.as_ref() {
        lines.extend(render_turn_context_tool_policy(tool_policy));
    }

    lines.push(
        format!(
            "  - user_instruction_sections: {}",
            snapshot.user_instruction_sections.len()
        )
        .into(),
    );
    if !snapshot.user_instruction_sections.is_empty() {
        lines.extend(render_instruction_sections(
            "    ",
            snapshot.user_instruction_sections.as_slice(),
        ));
    }

    match snapshot.resolved_instruction_layers.as_ref() {
        Some(resolved_layers) => {
            lines.extend(render_resolved_instruction_layers(resolved_layers));
        }
        None => {
            lines.push(
                "  - resolved_instruction_layers: <unavailable>"
                    .dim()
                    .into(),
            );
        }
    }

    lines
}

fn render_turn_context_tool_policy(tool_policy: &TurnContextToolPolicy) -> Vec<Line<'static>> {
    let request_user_input = &tool_policy.request_user_input;
    let allowed_modes = join_or_empty(
        request_user_input
            .allowed_modes
            .iter()
            .map(|mode_kind| mode_kind.display_name().to_string())
            .collect(),
    );

    vec![
        "  - tool_policy.request_user_input:".into(),
        format!("    - tool_enabled: {}", request_user_input.tool_enabled).into(),
        format!("    - available: {}", request_user_input.available).into(),
        format!(
            "    - default_mode_enabled: {}",
            request_user_input.default_mode_enabled
        )
        .into(),
        format!("    - allowed_modes: {allowed_modes}").into(),
    ]
}

fn render_resolved_instruction_layers(
    resolved_layers: &ResolvedInstructionLayers,
) -> Vec<Line<'static>> {
    let mut lines = vec!["  - resolved_instruction_layers:".into()];
    lines.push("    - base_instructions:".into());
    lines.extend(render_text_block(
        "      ",
        resolved_layers.base_instructions.as_str(),
    ));
    lines.push(format!("    - sections: {}", resolved_layers.sections.len()).into());
    if resolved_layers.sections.is_empty() {
        lines.push("      <none>".dim().into());
    } else {
        lines.extend(render_instruction_sections(
            "      ",
            resolved_layers.sections.as_slice(),
        ));
    }
    lines
}

fn render_instruction_sections(
    indent: &str,
    sections: &[InstructionSection],
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (index, section) in sections.iter().enumerate() {
        lines.push(
            format!(
                "{indent}{}. audience={} priority={} source={}",
                index + 1,
                section.audience,
                section.priority,
                section.source
            )
            .into(),
        );
        lines.extend(render_text_block(
            &format!("{indent}   "),
            section.text.as_str(),
        ));
    }
    lines
}

fn render_text_block(indent: &str, text: &str) -> Vec<Line<'static>> {
    if text.is_empty() {
        return vec![format!("{indent}<empty>").dim().into()];
    }

    text.lines()
        .map(|line| format!("{indent}{line}").into())
        .collect()
}

fn session_all_proxy_url(http_addr: &str, socks_addr: &str, socks_enabled: bool) -> String {
    if socks_enabled {
        format!("socks5h://{socks_addr}")
    } else {
        format!("http://{http_addr}")
    }
}

fn render_debug_config_lines(stack: &ConfigLayerStack) -> Vec<Line<'static>> {
    let mut lines = vec!["/debug-config".magenta().into(), "".into()];

    lines.push(
        "Config layer stack (lowest precedence first):"
            .bold()
            .into(),
    );
    let layers = stack.get_layers(ConfigLayerStackOrdering::LowestPrecedenceFirst, true);
    if layers.is_empty() {
        lines.push("  <none>".dim().into());
    } else {
        for (index, layer) in layers.iter().enumerate() {
            let source = format_config_layer_source(&layer.name);
            let status = if layer.is_disabled() {
                "disabled"
            } else {
                "enabled"
            };
            lines.push(format!("  {}. {source} ({status})", index + 1).into());
            lines.extend(render_non_file_layer_details(layer));
            if let Some(reason) = &layer.disabled_reason {
                lines.push(format!("     reason: {reason}").dim().into());
            }
        }
    }

    let requirements = stack.requirements();
    let requirements_toml = stack.requirements_toml();

    lines.push("".into());
    lines.push("Requirements:".bold().into());
    let mut requirement_lines = Vec::new();

    if let Some(policies) = requirements_toml.allowed_approval_policies.as_ref() {
        let value = join_or_empty(policies.iter().map(ToString::to_string).collect::<Vec<_>>());
        requirement_lines.push(requirement_line(
            "allowed_approval_policies",
            value,
            requirements.approval_policy.source.as_ref(),
        ));
    }

    if let Some(modes) = requirements_toml.allowed_sandbox_modes.as_ref() {
        let value = join_or_empty(
            modes
                .iter()
                .copied()
                .map(format_sandbox_mode_requirement)
                .collect::<Vec<_>>(),
        );
        requirement_lines.push(requirement_line(
            "allowed_sandbox_modes",
            value,
            requirements.sandbox_policy.source.as_ref(),
        ));
    }

    if let Some(modes) = requirements_toml.allowed_web_search_modes.as_ref() {
        let normalized = normalize_allowed_web_search_modes(modes);
        let value = join_or_empty(
            normalized
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
        );
        requirement_lines.push(requirement_line(
            "allowed_web_search_modes",
            value,
            requirements.web_search_mode.source.as_ref(),
        ));
    }

    if let Some(servers) = requirements_toml.mcp_servers.as_ref() {
        let value = join_or_empty(servers.keys().cloned().collect::<Vec<_>>());
        requirement_lines.push(requirement_line(
            "mcp_servers",
            value,
            requirements
                .mcp_servers
                .as_ref()
                .map(|sourced| &sourced.source),
        ));
    }

    // TODO(gt): Expand this debug output with detailed skills and rules display.
    if requirements_toml.rules.is_some() {
        requirement_lines.push(requirement_line(
            "rules",
            "configured".to_string(),
            requirements.exec_policy_source(),
        ));
    }

    if let Some(residency) = requirements_toml.enforce_residency {
        requirement_lines.push(requirement_line(
            "enforce_residency",
            format_residency_requirement(residency),
            requirements.enforce_residency.source.as_ref(),
        ));
    }

    if let Some(network) = requirements.network.as_ref() {
        requirement_lines.push(requirement_line(
            "experimental_network",
            format_network_constraints(&network.value),
            Some(&network.source),
        ));
    }

    if requirement_lines.is_empty() {
        lines.push("  <none>".dim().into());
    } else {
        lines.extend(requirement_lines);
    }

    lines
}

fn render_non_file_layer_details(layer: &ConfigLayerEntry) -> Vec<Line<'static>> {
    match &layer.name {
        ConfigLayerSource::SessionFlags => render_session_flag_details(&layer.config),
        ConfigLayerSource::Mdm { .. } | ConfigLayerSource::LegacyManagedConfigTomlFromMdm => {
            render_mdm_layer_details(layer)
        }
        ConfigLayerSource::System { .. }
        | ConfigLayerSource::User { .. }
        | ConfigLayerSource::Project { .. }
        | ConfigLayerSource::LegacyManagedConfigTomlFromFile { .. } => Vec::new(),
    }
}

fn render_session_flag_details(config: &TomlValue) -> Vec<Line<'static>> {
    let mut pairs = Vec::new();
    flatten_toml_key_values(config, None, &mut pairs);

    if pairs.is_empty() {
        return vec!["     - <none>".dim().into()];
    }

    pairs
        .into_iter()
        .map(|(key, value)| format!("     - {key} = {value}").into())
        .collect()
}

fn render_mdm_layer_details(layer: &ConfigLayerEntry) -> Vec<Line<'static>> {
    let value = layer
        .raw_toml()
        .map(ToString::to_string)
        .unwrap_or_else(|| format_toml_value(&layer.config));
    if value.is_empty() {
        return vec!["     MDM value: <empty>".dim().into()];
    }

    if value.contains('\n') {
        let mut lines = vec!["     MDM value:".into()];
        lines.extend(value.lines().map(|line| format!("       {line}").into()));
        lines
    } else {
        vec![format!("     MDM value: {value}").into()]
    }
}

fn flatten_toml_key_values(
    value: &TomlValue,
    prefix: Option<&str>,
    out: &mut Vec<(String, String)>,
) {
    match value {
        TomlValue::Table(table) => {
            let mut entries = table.iter().collect::<Vec<_>>();
            entries.sort_by_key(|(key, _)| key.as_str());
            for (key, child) in entries {
                let next_prefix = if let Some(prefix) = prefix {
                    format!("{prefix}.{key}")
                } else {
                    key.to_string()
                };
                flatten_toml_key_values(child, Some(&next_prefix), out);
            }
        }
        _ => {
            let key = prefix.unwrap_or("<value>").to_string();
            out.push((key, format_toml_value(value)));
        }
    }
}

fn format_toml_value(value: &TomlValue) -> String {
    value.to_string()
}

fn requirement_line(
    name: &str,
    value: String,
    source: Option<&RequirementSource>,
) -> Line<'static> {
    let source = source
        .map(ToString::to_string)
        .unwrap_or_else(|| "<unspecified>".to_string());
    format!("  - {name}: {value} (source: {source})").into()
}

fn join_or_empty(values: Vec<String>) -> String {
    if values.is_empty() {
        "<empty>".to_string()
    } else {
        values.join(", ")
    }
}

fn normalize_allowed_web_search_modes(
    modes: &[WebSearchModeRequirement],
) -> Vec<WebSearchModeRequirement> {
    if modes.is_empty() {
        return vec![WebSearchModeRequirement::Disabled];
    }

    let mut normalized = modes.to_vec();
    if !normalized.contains(&WebSearchModeRequirement::Disabled) {
        normalized.push(WebSearchModeRequirement::Disabled);
    }
    normalized
}

fn format_config_layer_source(source: &ConfigLayerSource) -> String {
    match source {
        ConfigLayerSource::Mdm { domain, key } => {
            format!("MDM ({domain}:{key})")
        }
        ConfigLayerSource::System { file } => {
            format!("system ({})", file.as_path().display())
        }
        ConfigLayerSource::User { file } => {
            format!("user ({})", file.as_path().display())
        }
        ConfigLayerSource::Project { dot_codex_folder } => {
            format!(
                "project ({}/config.toml)",
                dot_codex_folder.as_path().display()
            )
        }
        ConfigLayerSource::SessionFlags => "session-flags".to_string(),
        ConfigLayerSource::LegacyManagedConfigTomlFromFile { file } => {
            format!("legacy managed_config.toml ({})", file.as_path().display())
        }
        ConfigLayerSource::LegacyManagedConfigTomlFromMdm => {
            "legacy managed_config.toml (MDM)".to_string()
        }
    }
}

fn format_sandbox_mode_requirement(mode: SandboxModeRequirement) -> String {
    match mode {
        SandboxModeRequirement::ReadOnly => "read-only".to_string(),
        SandboxModeRequirement::WorkspaceWrite => "workspace-write".to_string(),
        SandboxModeRequirement::DangerFullAccess => "danger-full-access".to_string(),
        SandboxModeRequirement::ExternalSandbox => "external-sandbox".to_string(),
    }
}

fn format_residency_requirement(requirement: ResidencyRequirement) -> String {
    match requirement {
        ResidencyRequirement::Us => "us".to_string(),
    }
}

fn format_network_constraints(network: &NetworkConstraints) -> String {
    let mut parts = Vec::new();

    let NetworkConstraints {
        enabled,
        http_port,
        socks_port,
        allow_upstream_proxy,
        dangerously_allow_non_loopback_proxy,
        dangerously_allow_all_unix_sockets,
        allowed_domains,
        managed_allowed_domains_only,
        denied_domains,
        allow_unix_sockets,
        allow_local_binding,
    } = network;

    if let Some(enabled) = enabled {
        parts.push(format!("enabled={enabled}"));
    }
    if let Some(http_port) = http_port {
        parts.push(format!("http_port={http_port}"));
    }
    if let Some(socks_port) = socks_port {
        parts.push(format!("socks_port={socks_port}"));
    }
    if let Some(allow_upstream_proxy) = allow_upstream_proxy {
        parts.push(format!("allow_upstream_proxy={allow_upstream_proxy}"));
    }
    if let Some(dangerously_allow_non_loopback_proxy) = dangerously_allow_non_loopback_proxy {
        parts.push(format!(
            "dangerously_allow_non_loopback_proxy={dangerously_allow_non_loopback_proxy}"
        ));
    }
    if let Some(dangerously_allow_all_unix_sockets) = dangerously_allow_all_unix_sockets {
        parts.push(format!(
            "dangerously_allow_all_unix_sockets={dangerously_allow_all_unix_sockets}"
        ));
    }
    if let Some(allowed_domains) = allowed_domains {
        parts.push(format!("allowed_domains=[{}]", allowed_domains.join(", ")));
    }
    if let Some(managed_allowed_domains_only) = managed_allowed_domains_only {
        parts.push(format!(
            "managed_allowed_domains_only={managed_allowed_domains_only}"
        ));
    }
    if let Some(denied_domains) = denied_domains {
        parts.push(format!("denied_domains=[{}]", denied_domains.join(", ")));
    }
    if let Some(allow_unix_sockets) = allow_unix_sockets {
        parts.push(format!(
            "allow_unix_sockets=[{}]",
            allow_unix_sockets.join(", ")
        ));
    }
    if let Some(allow_local_binding) = allow_local_binding {
        parts.push(format!("allow_local_binding={allow_local_binding}"));
    }

    join_or_empty(parts)
}

#[cfg(test)]
mod tests {
    use super::render_debug_config_lines;
    use super::render_turn_context_snapshot_lines;
    use super::session_all_proxy_url;
    use codex_app_server_protocol::ConfigLayerSource;
    use codex_core::config::Constrained;
    use codex_core::config_loader::ConfigLayerEntry;
    use codex_core::config_loader::ConfigLayerStack;
    use codex_core::config_loader::ConfigRequirements;
    use codex_core::config_loader::ConfigRequirementsToml;
    use codex_core::config_loader::ConstrainedWithSource;
    use codex_core::config_loader::McpServerIdentity;
    use codex_core::config_loader::McpServerRequirement;
    use codex_core::config_loader::NetworkConstraints;
    use codex_core::config_loader::RequirementSource;
    use codex_core::config_loader::ResidencyRequirement;
    use codex_core::config_loader::SandboxModeRequirement;
    use codex_core::config_loader::Sourced;
    use codex_core::config_loader::WebSearchModeRequirement;
    use codex_protocol::config_types::ReasoningSummary as ReasoningSummaryConfig;
    use codex_protocol::config_types::WebSearchMode;
    use codex_protocol::protocol::AskForApproval;
    use codex_protocol::protocol::InstructionAudience;
    use codex_protocol::protocol::InstructionPriority;
    use codex_protocol::protocol::InstructionSection;
    use codex_protocol::protocol::InstructionSource;
    use codex_protocol::protocol::ResolvedInstructionLayers;
    use codex_protocol::protocol::RolloutItem;
    use codex_protocol::protocol::RolloutLine;
    use codex_protocol::protocol::SandboxPolicy;
    use codex_protocol::protocol::TurnContextItem;
    use codex_protocol::protocol::TurnContextRequestUserInputPolicy;
    use codex_protocol::protocol::TurnContextToolPolicy;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use ratatui::text::Line;
    use std::collections::BTreeMap;
    use tempfile::NamedTempFile;
    use toml::Value as TomlValue;

    fn empty_toml_table() -> TomlValue {
        TomlValue::Table(toml::map::Map::new())
    }

    fn absolute_path(path: &str) -> AbsolutePathBuf {
        AbsolutePathBuf::from_absolute_path(path).expect("absolute path")
    }

    fn render_to_text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn debug_config_output_lists_all_layers_including_disabled() {
        let system_file = if cfg!(windows) {
            absolute_path("C:\\etc\\codex\\config.toml")
        } else {
            absolute_path("/etc/codex/config.toml")
        };
        let project_folder = if cfg!(windows) {
            absolute_path("C:\\repo\\.codex")
        } else {
            absolute_path("/repo/.codex")
        };

        let layers = vec![
            ConfigLayerEntry::new(
                ConfigLayerSource::System { file: system_file },
                empty_toml_table(),
            ),
            ConfigLayerEntry::new_disabled(
                ConfigLayerSource::Project {
                    dot_codex_folder: project_folder,
                },
                empty_toml_table(),
                "project is untrusted",
            ),
        ];
        let stack = ConfigLayerStack::new(
            layers,
            ConfigRequirements::default(),
            ConfigRequirementsToml::default(),
        )
        .expect("config layer stack");

        let rendered = render_to_text(&render_debug_config_lines(&stack));
        assert!(rendered.contains("(enabled)"));
        assert!(rendered.contains("(disabled)"));
        assert!(rendered.contains("reason: project is untrusted"));
        assert!(rendered.contains("Requirements:"));
        assert!(rendered.contains("  <none>"));
    }

    #[test]
    fn debug_config_output_lists_requirement_sources() {
        let requirements_file = if cfg!(windows) {
            absolute_path("C:\\ProgramData\\OpenAI\\Codex\\requirements.toml")
        } else {
            absolute_path("/etc/codex/requirements.toml")
        };

        let requirements = ConfigRequirements {
            approval_policy: ConstrainedWithSource::new(
                Constrained::allow_any(AskForApproval::OnRequest),
                Some(RequirementSource::CloudRequirements),
            ),
            sandbox_policy: ConstrainedWithSource::new(
                Constrained::allow_any(SandboxPolicy::new_read_only_policy()),
                Some(RequirementSource::SystemRequirementsToml {
                    file: requirements_file.clone(),
                }),
            ),
            mcp_servers: Some(Sourced::new(
                BTreeMap::from([(
                    "docs".to_string(),
                    McpServerRequirement {
                        identity: McpServerIdentity::Command {
                            command: "codex-mcp".to_string(),
                        },
                    },
                )]),
                RequirementSource::LegacyManagedConfigTomlFromMdm,
            )),
            enforce_residency: ConstrainedWithSource::new(
                Constrained::allow_any(Some(ResidencyRequirement::Us)),
                Some(RequirementSource::CloudRequirements),
            ),
            web_search_mode: ConstrainedWithSource::new(
                Constrained::allow_any(WebSearchMode::Cached),
                Some(RequirementSource::CloudRequirements),
            ),
            network: Some(Sourced::new(
                NetworkConstraints {
                    enabled: Some(true),
                    allowed_domains: Some(vec!["example.com".to_string()]),
                    ..Default::default()
                },
                RequirementSource::CloudRequirements,
            )),
            ..ConfigRequirements::default()
        };

        let requirements_toml = ConfigRequirementsToml {
            allowed_approval_policies: Some(vec![AskForApproval::OnRequest]),
            allowed_sandbox_modes: Some(vec![SandboxModeRequirement::ReadOnly]),
            allowed_web_search_modes: Some(vec![WebSearchModeRequirement::Cached]),
            feature_requirements: None,
            mcp_servers: Some(BTreeMap::from([(
                "docs".to_string(),
                McpServerRequirement {
                    identity: McpServerIdentity::Command {
                        command: "codex-mcp".to_string(),
                    },
                },
            )])),
            rules: None,
            enforce_residency: Some(ResidencyRequirement::Us),
            network: None,
        };

        let user_file = if cfg!(windows) {
            absolute_path("C:\\users\\alice\\.codex\\config.toml")
        } else {
            absolute_path("/home/alice/.codex/config.toml")
        };
        let stack = ConfigLayerStack::new(
            vec![ConfigLayerEntry::new(
                ConfigLayerSource::User { file: user_file },
                empty_toml_table(),
            )],
            requirements,
            requirements_toml,
        )
        .expect("config layer stack");

        let rendered = render_to_text(&render_debug_config_lines(&stack));
        assert!(
            rendered.contains("allowed_approval_policies: on-request (source: cloud requirements)")
        );
        assert!(
            rendered.contains(
                format!(
                    "allowed_sandbox_modes: read-only (source: {})",
                    requirements_file.as_path().display()
                )
                .as_str(),
            )
        );
        assert!(
            rendered.contains(
                "allowed_web_search_modes: cached, disabled (source: cloud requirements)"
            )
        );
        assert!(rendered.contains("mcp_servers: docs (source: MDM managed_config.toml (legacy))"));
        assert!(rendered.contains("enforce_residency: us (source: cloud requirements)"));
        assert!(rendered.contains(
            "experimental_network: enabled=true, allowed_domains=[example.com] (source: cloud requirements)"
        ));
        assert!(!rendered.contains("  - rules:"));
    }
    #[test]
    fn debug_config_output_lists_session_flag_key_value_pairs() {
        let session_flags = toml::from_str::<TomlValue>(
            r#"
model = "gpt-5"
[sandbox_workspace_write]
network_access = true
writable_roots = ["/tmp"]
"#,
        )
        .expect("session flags");

        let stack = ConfigLayerStack::new(
            vec![ConfigLayerEntry::new(
                ConfigLayerSource::SessionFlags,
                session_flags,
            )],
            ConfigRequirements::default(),
            ConfigRequirementsToml::default(),
        )
        .expect("config layer stack");

        let rendered = render_to_text(&render_debug_config_lines(&stack));
        assert!(rendered.contains("session-flags (enabled)"));
        assert!(rendered.contains("     - model = \"gpt-5\""));
        assert!(rendered.contains("     - sandbox_workspace_write.network_access = true"));
        assert!(rendered.contains("sandbox_workspace_write.writable_roots"));
        assert!(rendered.contains("/tmp"));
    }

    #[test]
    fn debug_config_output_shows_legacy_mdm_layer_value() {
        let raw_mdm_toml = r#"
# managed by MDM
model = "managed_model"
approval_policy = "never"
"#;
        let mdm_value = toml::from_str::<TomlValue>(raw_mdm_toml).expect("MDM value");

        let stack = ConfigLayerStack::new(
            vec![ConfigLayerEntry::new_with_raw_toml(
                ConfigLayerSource::LegacyManagedConfigTomlFromMdm,
                mdm_value,
                raw_mdm_toml.to_string(),
            )],
            ConfigRequirements::default(),
            ConfigRequirementsToml::default(),
        )
        .expect("config layer stack");

        let rendered = render_to_text(&render_debug_config_lines(&stack));
        assert!(rendered.contains("legacy managed_config.toml (MDM) (enabled)"));
        assert!(rendered.contains("MDM value:"));
        assert!(rendered.contains("# managed by MDM"));
        assert!(rendered.contains("model = \"managed_model\""));
        assert!(rendered.contains("approval_policy = \"never\""));
    }

    #[test]
    fn debug_config_output_normalizes_empty_web_search_mode_list() {
        let requirements = ConfigRequirements {
            web_search_mode: ConstrainedWithSource::new(
                Constrained::allow_any(WebSearchMode::Disabled),
                Some(RequirementSource::CloudRequirements),
            ),
            ..ConfigRequirements::default()
        };

        let requirements_toml = ConfigRequirementsToml {
            allowed_approval_policies: None,
            allowed_sandbox_modes: None,
            allowed_web_search_modes: Some(Vec::new()),
            feature_requirements: None,
            mcp_servers: None,
            rules: None,
            enforce_residency: None,
            network: None,
        };

        let stack = ConfigLayerStack::new(Vec::new(), requirements, requirements_toml)
            .expect("config layer stack");

        let rendered = render_to_text(&render_debug_config_lines(&stack));
        assert!(
            rendered.contains("allowed_web_search_modes: disabled (source: cloud requirements)")
        );
    }

    #[test]
    fn session_all_proxy_url_uses_socks_when_enabled() {
        assert_eq!(
            session_all_proxy_url("127.0.0.1:3128", "127.0.0.1:8081", true),
            "socks5h://127.0.0.1:8081".to_string()
        );
    }

    #[test]
    fn session_all_proxy_url_uses_http_when_socks_disabled() {
        assert_eq!(
            session_all_proxy_url("127.0.0.1:3128", "127.0.0.1:8081", false),
            "http://127.0.0.1:3128".to_string()
        );
    }

    #[test]
    fn debug_config_output_reads_latest_turn_context_instruction_snapshot() {
        let rollout_file = NamedTempFile::new().expect("temp rollout");
        let rollout_line = RolloutLine {
            timestamp: "t0".to_string(),
            item: RolloutItem::TurnContext(TurnContextItem {
                turn_id: Some("turn-1".to_string()),
                trace_id: None,
                cwd: std::env::temp_dir(),
                current_date: None,
                timezone: None,
                approval_policy: AskForApproval::Never,
                sandbox_policy: SandboxPolicy::DangerFullAccess,
                network: None,
                model: "gpt-5".to_string(),
                personality: None,
                collaboration_mode: None,
                realtime_active: Some(false),
                effort: None,
                summary: ReasoningSummaryConfig::Auto,
                user_instructions: Some("repo instructions".to_string()),
                user_instruction_sections: vec![InstructionSection {
                    audience: InstructionAudience::ContextualUser,
                    priority: InstructionPriority::Repo,
                    source: InstructionSource::ProjectDoc,
                    text: "repo instructions".to_string(),
                }],
                developer_instructions: Some("developer override".to_string()),
                resolved_instruction_layers: Some(ResolvedInstructionLayers {
                    base_instructions: "base instructions".to_string(),
                    sections: vec![InstructionSection {
                        audience: InstructionAudience::Developer,
                        priority: InstructionPriority::Developer,
                        source: InstructionSource::DeveloperOverride,
                        text: "developer override".to_string(),
                    }],
                }),
                tool_policy: Some(TurnContextToolPolicy {
                    request_user_input: TurnContextRequestUserInputPolicy {
                        tool_enabled: true,
                        available: false,
                        default_mode_enabled: false,
                        allowed_modes: vec![codex_protocol::config_types::ModeKind::Plan],
                    },
                }),
                final_output_json_schema: None,
                truncation_policy: None,
            }),
        };
        std::fs::write(
            rollout_file.path(),
            format!(
                "{}\n",
                serde_json::to_string(&rollout_line).expect("serialize rollout")
            ),
        )
        .expect("write rollout");

        let rendered = render_to_text(&render_turn_context_snapshot_lines(Some(
            rollout_file.path(),
        )));
        assert!(rendered.contains("Latest persisted turn context:"));
        assert!(rendered.contains("turn_id: turn-1"));
        assert!(rendered.contains("model: gpt-5"));
        assert!(rendered.contains("tool_policy.request_user_input:"));
        assert!(rendered.contains("tool_enabled: true"));
        assert!(rendered.contains("available: false"));
        assert!(rendered.contains("allowed_modes: Plan"));
        assert!(rendered.contains("user_instruction_sections: 1"));
        assert!(rendered.contains("audience=contextual_user priority=repo source=project_doc"));
        assert!(rendered.contains("repo instructions"));
        assert!(rendered.contains("base_instructions:"));
        assert!(rendered.contains("base instructions"));
        assert!(rendered.contains("developer override"));
    }
}
