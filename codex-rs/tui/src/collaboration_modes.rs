use codex_app_server_protocol::CollaborationModeMask as CollaborationModeMetadata;
use codex_core::models_manager::manager::ModelsManager;
use codex_protocol::config_types::CollaborationModeMask;
use codex_protocol::config_types::ModeKind;

#[derive(Clone)]
pub(crate) struct TuiCollaborationModePreset {
    pub mask: CollaborationModeMask,
    pub metadata: CollaborationModeMetadata,
}

impl TuiCollaborationModePreset {
    pub(crate) fn popup_description(&self) -> String {
        capability_summary_from_metadata(&self.metadata)
    }

    pub(crate) fn popup_selected_description(&self) -> String {
        format!("{} {}", self.metadata.description, self.popup_description())
    }

    pub(crate) fn popup_search_value(&self) -> String {
        format!(
            "{} {} {}",
            self.mask.name,
            self.metadata.description,
            self.popup_description()
        )
    }
}

pub(crate) fn metadata_for_mask(mask: &CollaborationModeMask) -> CollaborationModeMetadata {
    CollaborationModeMetadata::from(mask.clone())
}

pub(crate) fn capability_summary(mask: &CollaborationModeMask) -> String {
    capability_summary_from_metadata(&metadata_for_mask(mask))
}

fn capability_summary_from_metadata(metadata: &CollaborationModeMetadata) -> String {
    let mut capabilities = vec![if metadata.allows_repo_mutation {
        "repo edits allowed"
    } else {
        "repo edits blocked"
    }];
    capabilities.push(if metadata.update_plan_available {
        "`update_plan` available"
    } else {
        "`update_plan` unavailable"
    });
    capabilities.push(if metadata.request_user_input_available {
        "`request_user_input` available"
    } else {
        "`request_user_input` unavailable"
    });
    if metadata.streams_proposed_plan {
        capabilities.push("streams proposed plan");
    }
    capabilities.join(" · ")
}

pub(crate) fn same_preset_identity(
    left: &CollaborationModeMask,
    right: &CollaborationModeMask,
) -> bool {
    left.mode == right.mode && left.name == right.name
}

pub(crate) fn requires_proposed_plan_block(mask: &CollaborationModeMask) -> bool {
    metadata_for_mask(mask).requires_proposed_plan_block
}

pub(crate) fn streams_proposed_plan(mask: &CollaborationModeMask) -> bool {
    metadata_for_mask(mask).streams_proposed_plan
}

fn filtered_presets(models_manager: &ModelsManager) -> Vec<TuiCollaborationModePreset> {
    models_manager
        .list_collaboration_modes()
        .into_iter()
        .map(|mask| {
            let metadata = metadata_for_mask(&mask);
            TuiCollaborationModePreset { mask, metadata }
        })
        .filter(|preset| preset.metadata.tui_visible)
        .collect()
}

pub(crate) fn presets_for_tui(models_manager: &ModelsManager) -> Vec<TuiCollaborationModePreset> {
    filtered_presets(models_manager)
}

pub(crate) fn default_mask(models_manager: &ModelsManager) -> Option<CollaborationModeMask> {
    let presets = filtered_presets(models_manager);
    presets
        .iter()
        .find(|preset| preset.mask.mode == Some(ModeKind::Default))
        .map(|preset| preset.mask.clone())
        .or_else(|| presets.into_iter().next().map(|preset| preset.mask))
}

pub(crate) fn mask_for_kind(
    models_manager: &ModelsManager,
    kind: ModeKind,
) -> Option<CollaborationModeMask> {
    if !kind.is_tui_visible() {
        return None;
    }
    filtered_presets(models_manager)
        .into_iter()
        .find(|preset| preset.mask.mode == Some(kind))
        .map(|preset| preset.mask)
}

/// Cycle to the next collaboration mode preset in list order.
pub(crate) fn next_mask(
    models_manager: &ModelsManager,
    current: Option<&CollaborationModeMask>,
) -> Option<CollaborationModeMask> {
    next_mask_from_presets(&filtered_presets(models_manager), current)
}

fn next_mask_from_presets(
    presets: &[TuiCollaborationModePreset],
    current: Option<&CollaborationModeMask>,
) -> Option<CollaborationModeMask> {
    if presets.is_empty() {
        return None;
    }
    let next_index = presets
        .iter()
        .position(|preset| current.is_some_and(|mask| same_preset_identity(mask, &preset.mask)))
        .map_or(0, |idx| (idx + 1) % presets.len());
    presets.get(next_index).map(|preset| preset.mask.clone())
}

pub(crate) fn default_mode_mask(models_manager: &ModelsManager) -> Option<CollaborationModeMask> {
    mask_for_kind(models_manager, ModeKind::Default)
}

pub(crate) fn plan_mask(models_manager: &ModelsManager) -> Option<CollaborationModeMask> {
    mask_for_kind(models_manager, ModeKind::Plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::openai_models::ReasoningEffort;

    fn preset(name: &str, mode: ModeKind, model: Option<&str>) -> TuiCollaborationModePreset {
        let mask = CollaborationModeMask {
            name: name.to_string(),
            mode: Some(mode),
            model: model.map(ToString::to_string),
            reasoning_effort: Some(Some(ReasoningEffort::Medium)),
            developer_instructions: Some(Some(format!("{name} instructions"))),
        };
        let metadata = metadata_for_mask(&mask);
        TuiCollaborationModePreset { mask, metadata }
    }

    #[test]
    fn same_preset_identity_ignores_runtime_mask_overrides() {
        let mut left = preset("Design Review", ModeKind::Plan, Some("gpt-5.1-codex-mini")).mask;
        let right = preset("Design Review", ModeKind::Plan, None).mask;
        left.reasoning_effort = Some(Some(ReasoningEffort::High));

        assert!(same_preset_identity(&left, &right));
    }

    #[test]
    fn next_mask_tracks_exact_preset_identity_instead_of_mode_kind() {
        let presets = vec![
            preset("Default", ModeKind::Default, None),
            preset(
                "Default Fast",
                ModeKind::Default,
                Some("gpt-5.1-codex-mini"),
            ),
            preset("Plan", ModeKind::Plan, None),
        ];
        let current = CollaborationModeMask {
            name: "Default Fast".to_string(),
            mode: Some(ModeKind::Default),
            model: Some("gpt-5.1-codex".to_string()),
            reasoning_effort: Some(Some(ReasoningEffort::High)),
            developer_instructions: Some(Some("override".to_string())),
        };

        let next = next_mask_from_presets(&presets, Some(&current))
            .expect("expected next collaboration preset");

        assert_eq!(next.name, "Plan");
        assert_eq!(next.mode, Some(ModeKind::Plan));
    }

    #[test]
    fn proposed_plan_helpers_follow_metadata_shape() {
        let plan = preset("Plan", ModeKind::Plan, None).mask;
        let default = preset("Default", ModeKind::Default, None).mask;

        assert!(requires_proposed_plan_block(&plan));
        assert!(streams_proposed_plan(&plan));
        assert!(!requires_proposed_plan_block(&default));
        assert!(!streams_proposed_plan(&default));
    }
}
