/// Resolved instruction layers for a turn before they are rendered into prompt/input items.
///
/// This is a runtime-facing normalization step: it records the already-resolved base instructions
/// and the ordered developer/contextual-user sections that will be rendered into the model input.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ResolvedInstructionLayers {
    pub base_instructions: String,
    pub developer_sections: Vec<String>,
    pub contextual_user_sections: Vec<String>,
}
