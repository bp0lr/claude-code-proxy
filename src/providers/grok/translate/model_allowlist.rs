pub fn resolve_model(model: &str) -> String {
    model.to_string()
}

/// The registry's advertised list is the single source of truth, so adding a
/// model there is enough to make it selectable everywhere.
pub fn assert_allowed_model(model: &str) -> anyhow::Result<()> {
    if crate::registry::GROK_MODELS.contains(&model) {
        Ok(())
    } else {
        anyhow::bail!("unsupported Grok model")
    }
}
