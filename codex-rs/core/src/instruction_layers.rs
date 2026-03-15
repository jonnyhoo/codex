/// Audience for a resolved instruction section.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InstructionAudience {
    Developer,
    ContextualUser,
}

/// Effective precedence bucket for an instruction section.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InstructionPriority {
    System,
    Developer,
    Mode,
    Repo,
    Skill,
    User,
    Runtime,
}

/// Concrete source of a resolved instruction section.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InstructionSource {
    ModelSwitch,
    PlatformPolicy,
    DeveloperOverride,
    MemoryTool,
    CollaborationMode,
    RealtimeContext,
    Personality,
    Apps,
    CommitMessage,
    UserConfig,
    ProjectDoc,
    JsRepl,
    Plugins,
    CodeMode,
    Skills,
    ChildAgents,
    EnvironmentContext,
}

/// One resolved instruction section before it is rendered into a prompt/input item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InstructionSection {
    pub audience: InstructionAudience,
    pub priority: InstructionPriority,
    pub source: InstructionSource,
    pub text: String,
}

impl InstructionSection {
    pub(crate) fn new(
        audience: InstructionAudience,
        priority: InstructionPriority,
        source: InstructionSource,
        text: impl Into<String>,
    ) -> Self {
        Self {
            audience,
            priority,
            source,
            text: text.into(),
        }
    }
}

/// Resolved instruction layers for a turn before they are rendered into prompt/input items.
///
/// This records the already-resolved base instructions plus the ordered developer/contextual-user
/// sections that will be rendered into the model input.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ResolvedInstructionLayers {
    pub base_instructions: String,
    pub sections: Vec<InstructionSection>,
}

impl ResolvedInstructionLayers {
    pub(crate) fn developer_sections(&self) -> Vec<String> {
        self.sections_for(InstructionAudience::Developer)
    }

    fn sections_for(&self, audience: InstructionAudience) -> Vec<String> {
        self.sections
            .iter()
            .filter(|section| section.audience == audience)
            .map(|section| section.text.clone())
            .collect()
    }
}
