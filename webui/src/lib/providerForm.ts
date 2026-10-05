// Pure helpers for the model-provider settings page: parsing what a person
// types, and deciding what still blocks a form. No DOM, no fetch — so each
// rule is judged by a test rather than by clicking through the page.

/** Parse a token count as a person writes it: `128000`, `128K`, `1M`, `1.5m`.
 * `undefined` for empty input; `null` for something that is not a positive
 * whole number of tokens. */
export function parseTokens(raw: string): number | null | undefined {
  const text = raw.trim().replace(/[,_\s]/g, '');
  if (!text) return undefined;
  const m = /^(\d+(?:\.\d+)?)([kKmM]?)$/.exec(text);
  if (!m) return null;
  const scale = m[2] === '' ? 1 : m[2].toLowerCase() === 'k' ? 1_000 : 1_000_000;
  const n = Number(m[1]) * scale;
  if (!Number.isFinite(n) || n <= 0 || !Number.isInteger(n)) return null;
  return n;
}

/** A token count as a person reads it: `1000000` → `1M`, `128000` → `128K`. */
export function formatTokens(n: number): string {
  if (n >= 1_000_000 && n % 100_000 === 0) return `${n / 1_000_000}M`;
  if (n >= 1_000 && n % 1_000 === 0) return `${n / 1_000}K`;
  return String(n);
}

/** What is wrong with a pasted API key, if anything. The common mistakes are
 * pasting a whole `.env` line or a quoted value — both become a key that is
 * sent as-is and rejected with a 401 that says nothing about why. */
export type KeyProblem = 'env_line' | 'quoted' | 'whitespace' | 'non_ascii';

export function apiKeyProblem(raw: string): KeyProblem | null {
  const key = raw.trim();
  if (!key) return null;
  if (/^[A-Z][A-Z0-9_]*\s*=/.test(key)) return 'env_line';
  if (/^(['"`]).*\1$/.test(key)) return 'quoted';
  if (/\s/.test(key)) return 'whitespace';
  // eslint-disable-next-line no-control-regex
  if (!/^[\x21-\x7e]+$/.test(key)) return 'non_ascii';
  return null;
}

/** An account id a person may type: a plain key that is also a selection
 * prefix. Mirrors the daemon's rule. */
export function validAccountId(id: string): boolean {
  return /^[A-Za-z0-9][A-Za-z0-9._-]*$/.test(id);
}

/** Whether a base URL is something a request can be sent to. */
export function validBaseUrl(raw: string): boolean {
  try {
    const url = new URL(raw.trim());
    return (url.protocol === 'http:' || url.protocol === 'https:') && !url.username && !url.password;
  } catch {
    return false;
  }
}

/** A model's reasoning settings in a form. Declaring levels is what makes a
 * model offer a level picker in the chat; `effort` is the level requests carry
 * by default, `null` for the endpoint's own default. */
export interface ReasoningDraft {
  enabled: boolean;
  levels: string[];
  effort: string | null;
}

export function noReasoning(): ReasoningDraft {
  return { enabled: false, levels: [], effort: null };
}

/** What the configuration declares, as a form starts from it. */
export function reasoningFrom(levels?: string[] | null, effort?: string | null): ReasoningDraft {
  const declared = levels?.filter(Boolean) ?? [];
  if (declared.length === 0 && !effort) return noReasoning();
  return { enabled: true, levels: declared, effort: effort ?? null };
}

/** Toggle one level, in the canonical order `all` gives, dropping a default
 * that is no longer on offer. */
export function toggleLevel(r: ReasoningDraft, level: string, all: string[]): ReasoningDraft {
  const on = r.levels.includes(level);
  const levels = all.filter((l) => (l === level ? !on : r.levels.includes(l)));
  return { ...r, levels, effort: r.effort && levels.includes(r.effort) ? r.effort : null };
}

/** One model row in a form. Capacities are kept as typed, parsed on submit. */
export interface ModelDraft {
  model: string;
  displayName: string;
  window: string;
  maxTokens: string;
  /** `null` is "decide for me". */
  vision: boolean | null;
  reasoning: ReasoningDraft;
}

export function emptyModelDraft(model = ''): ModelDraft {
  return { model, displayName: '', window: '', maxTokens: '', vision: null, reasoning: noReasoning() };
}

/** The add form, in either mode. */
export interface ProviderDraft {
  mode: 'preset' | 'custom';
  /** Preset id (preset mode) or protocol id (custom mode). */
  provider: string;
  /** The account id; custom mode only. */
  id: string;
  displayName: string;
  baseUrl: string;
  apiKey: string;
  models: ModelDraft[];
}

/** Why the form cannot be submitted yet — the first reason only, so the page
 * never shows a second, misleading one. `null` when it can. */
export type Blocker =
  | { kind: 'provider' }
  | { kind: 'id' }
  | { kind: 'id_taken' }
  | { kind: 'base_url' }
  | { kind: 'api_key'; problem: KeyProblem | 'missing' }
  | { kind: 'no_models' }
  | { kind: 'model_name'; row: number }
  | { kind: 'model_duplicate'; row: number }
  | { kind: 'model_window'; row: number }
  | { kind: 'model_max_tokens'; row: number }
  | { kind: 'model_reasoning'; row: number };

export interface DraftContext {
  /** Account ids already in the configuration, any case. */
  takenIds: string[];
  /** Whether the chosen provider has an endpoint of its own. */
  hasDefaultEndpoint: boolean;
  /** Whether the chosen provider needs a key at all. */
  requiresKey: boolean;
}

export function firstBlocker(draft: ProviderDraft, ctx: DraftContext): Blocker | null {
  if (!draft.provider) return { kind: 'provider' };
  if (draft.mode === 'custom') {
    const id = draft.id.trim();
    if (!id || !validAccountId(id)) return { kind: 'id' };
    if (ctx.takenIds.some((t) => t.toLowerCase() === id.toLowerCase())) return { kind: 'id_taken' };
  }
  const base = draft.baseUrl.trim();
  if (base ? !validBaseUrl(base) : !ctx.hasDefaultEndpoint) return { kind: 'base_url' };
  const keyProblem = apiKeyProblem(draft.apiKey);
  if (keyProblem) return { kind: 'api_key', problem: keyProblem };
  if (ctx.requiresKey && !draft.apiKey.trim()) return { kind: 'api_key', problem: 'missing' };
  return modelsBlocker(draft.models);
}

/** The model half of a form, on its own: the add-models form has no account
 * fields, and its rows are judged the same way. */
export function modelsBlocker(models: ModelDraft[], existing: string[] = []): Blocker | null {
  if (models.length === 0) return { kind: 'no_models' };
  const seen = new Set(existing);
  for (let row = 0; row < models.length; row++) {
    const m = models[row];
    const name = m.model.trim();
    if (!name) return { kind: 'model_name', row };
    if (seen.has(name)) return { kind: 'model_duplicate', row };
    seen.add(name);
    if (parseTokens(m.window) === null) return { kind: 'model_window', row };
    if (parseTokens(m.maxTokens) === null) return { kind: 'model_max_tokens', row };
    if (m.reasoning.enabled && m.reasoning.levels.length === 0) return { kind: 'model_reasoning', row };
  }
  return null;
}

/** A model row as the daemon takes it. */
export function modelBody(m: ModelDraft): {
  model: string;
  display_name?: string;
  context_window?: number;
  max_tokens?: number;
  supports_vision?: boolean;
  reasoning_effort?: string;
  reasoning_effort_levels?: string[];
} {
  const window = parseTokens(m.window);
  const maxTokens = parseTokens(m.maxTokens);
  return {
    model: m.model.trim(),
    ...(m.displayName.trim() ? { display_name: m.displayName.trim() } : {}),
    ...(typeof window === 'number' ? { context_window: window } : {}),
    ...(typeof maxTokens === 'number' ? { max_tokens: maxTokens } : {}),
    ...(m.vision !== null ? { supports_vision: m.vision } : {}),
    ...(m.reasoning.enabled ? { reasoning_effort_levels: m.reasoning.levels } : {}),
    ...(m.reasoning.enabled && m.reasoning.effort ? { reasoning_effort: m.reasoning.effort } : {}),
  };
}

/** An edit's reasoning fields: set what is on, clear what was turned off. */
export function reasoningEditBody(r: ReasoningDraft): {
  reasoning_effort?: string;
  clear_reasoning_effort?: boolean;
  reasoning_effort_levels?: string[];
  clear_reasoning_effort_levels?: boolean;
} {
  if (!r.enabled) return { clear_reasoning_effort: true, clear_reasoning_effort_levels: true };
  return {
    reasoning_effort_levels: r.levels,
    ...(r.effort ? { reasoning_effort: r.effort } : { clear_reasoning_effort: true }),
  };
}

/** Rows to add from a discovery pick: the picked ids, in the order listed,
 * each carrying whatever the endpoint reported, and never one already in the
 * form or the account. A row the person already tuned is left as it is. */
export function adoptPicked(
  current: ModelDraft[],
  picked: { id: string; name?: string; context_window?: number; max_tokens?: number }[],
  existing: string[] = [],
): ModelDraft[] {
  const have = new Set([...existing, ...current.map((m) => m.model.trim()).filter(Boolean)]);
  // An untouched empty row is a placeholder, not a choice.
  const kept = current.filter((m) => m.model.trim() || m.displayName || m.window || m.maxTokens);
  const added = picked
    .filter((p) => !have.has(p.id))
    .map((p) => ({
      ...emptyModelDraft(p.id),
      displayName: p.name && p.name !== p.id ? p.name : '',
      window: p.context_window ? formatTokens(p.context_window) : '',
      maxTokens: p.max_tokens ? formatTokens(p.max_tokens) : '',
    }));
  return [...kept, ...added];
}
