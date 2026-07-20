use atomcode_capabilities::cc_hooks::HookConfig;

pub trait PluginHookSource: Send + Sync {
    fn load(&self) -> Result<Vec<HookConfig>, String>;
}

#[derive(Clone, Default)]
pub struct StaticPluginHookSource {
    hooks: Vec<HookConfig>,
}

impl StaticPluginHookSource {
    pub fn new(hooks: Vec<HookConfig>) -> Self {
        Self { hooks }
    }
}

impl PluginHookSource for StaticPluginHookSource {
    fn load(&self) -> Result<Vec<HookConfig>, String> {
        Ok(self.hooks.clone())
    }
}
