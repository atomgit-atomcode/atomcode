import type { ModelInfo } from '../api';

export interface RuntimeModelIdentity {
  provider: string;
  /** undefined is an older server; null is a runtime with no mounted model. */
  model?: string | null;
}

export function mayFollowModelDefault(pinned: boolean, runtime: RuntimeModelIdentity | null, live: boolean): boolean {
  return !pinned && !(live && runtime);
}

/** The catalog is a set of choices, not the identity of the running agent. */
export function displayedModel(models: readonly ModelInfo[], selection: string | null, runtime?: RuntimeModelIdentity | null): ModelInfo | undefined {
  const provider = selection ?? runtime?.provider;
  const configured = models.find(model => model.provider === provider);
  const active = runtime?.provider === provider ? runtime : null;
  if (active && active.model !== undefined) {
    if (configured && configured.model === active.model) return configured;
    return unknownModel(active.provider, active.model ?? active.provider);
  }
  if (configured) return configured;
  if (provider) return unknownModel(provider, provider);
  return models.find(model => model.is_default) ?? models[0];
}

function unknownModel(provider: string, model: string): ModelInfo {
  return { provider, model, provider_type: '', is_default: false, effort_applicable: false,
    reasoning_effort: null, effort_levels: [] };
}
