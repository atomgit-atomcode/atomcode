// Model settings: providers as accounts, with the models that hang off each.
//
// The page is the configuration's own shape — an account holds the connection
// (endpoint, key), its models hold what is model-specific — so a change made
// here reads the way it lands in the file. Every write goes to the daemon's
// account endpoints, which share one writer with the terminal's `/provider`
// panel and patch the file in place, leaving a person's comments alone.

import { useEffect, useMemo, useState } from 'preact/hooks';
import {
  getConfig,
  ConfigInfo,
  ProviderInfo,
  ProviderAccountInfo,
  ProviderPresetInfo,
  DiscoveredModelInfo,
  discoverProviderModels,
  createAccount,
  editAccount,
  deleteAccount,
  addAccountModels,
  editModelProfile,
  deleteModelProfile,
  setDefaultModel,
  probeAccount,
  ProbeResult,
  ApiCallError,
} from '../api';
import { useSettings } from '../settings';
import type { MsgKey } from '../i18n';
import { ConfirmDialog } from './ConfirmDialog';
import { Select } from './Select';
import {
  ModelDraft,
  ProviderDraft,
  Blocker,
  emptyModelDraft,
  firstBlocker,
  modelsBlocker,
  modelBody,
  adoptPicked,
  parseTokens,
  formatTokens,
  apiKeyProblem,
  validBaseUrl,
} from '../lib/providerForm';

type T = (key: MsgKey, params?: Record<string, string | number>) => string;

const ERROR_CODES = new Set([
  'managed',
  'not_found',
  'id_taken',
  'needs_endpoint',
  'model_empty',
  'model_exists',
  'invalid_id',
  'id_required',
  'unknown_provider',
  'write_failed',
]);

/** A refusal in the person's language: the daemon's code when it sent one. */
function errorText(error: unknown, t: T): string {
  const detail = error instanceof Error ? error.message : String(error);
  if (error instanceof ApiCallError && error.code && ERROR_CODES.has(error.code)) {
    return t(`providers.err.${error.code}` as MsgKey, { detail });
  }
  return t('providers.err.generic', { detail });
}

function blockerText(b: Blocker, t: T): string {
  switch (b.kind) {
    case 'api_key':
      return t(`providers.block.key_${b.problem}` as MsgKey);
    case 'model_name':
    case 'model_duplicate':
    case 'model_window':
    case 'model_max_tokens':
      return t(`providers.block.${b.kind}` as MsgKey, { row: b.row + 1 });
    default:
      return t(`providers.block.${b.kind}` as MsgKey);
  }
}

/** Which inline panel is open. One at a time, so the page never shows two
 * half-filled forms about the same account. */
type Panel =
  | { kind: 'add' }
  | { kind: 'account'; id: string }
  | { kind: 'models'; id: string }
  | { kind: 'model'; id: string };

type ProbeState = ProbeResult | 'checking';

function samePanel(a: Panel | null, b: Panel): boolean {
  if (!a || a.kind !== b.kind) return false;
  return a.kind === 'add' || (a as { id: string }).id === (b as { id: string }).id;
}

/** The models of one account, in the order the catalog lists them. */
function modelsOf(config: ConfigInfo, account: ProviderAccountInfo): ProviderInfo[] {
  return config.providers.filter((p) =>
    p.account ? p.account === account.id : account.model_ids.includes(p.name),
  );
}

export function ModelConfigDialog({ onClose }: { onClose: () => void }) {
  const { t } = useSettings();
  const [config, setConfig] = useState<ConfigInfo | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [panel, setPanel] = useState<Panel | null>(null);
  const [probes, setProbes] = useState<Record<string, ProbeState>>({});
  const [confirm, setConfirm] = useState<
    { kind: 'account'; id: string; name: string; n: number } | { kind: 'model'; id: string; name: string } | null
  >(null);

  const reload = () => {
    setLoadError(null);
    return getConfig()
      .then(setConfig)
      .catch((e: unknown) => setLoadError(e instanceof Error ? e.message : String(e)));
  };
  useEffect(() => {
    void reload();
  }, []);

  /** Check an account's endpoint as saved; a model is named so its name is
   * checked too. Skipped where the daemon cannot probe. */
  async function probe(account: string, selection?: string) {
    setProbes((p) => ({ ...p, [account]: 'checking' }));
    try {
      const result = await probeAccount(account, selection);
      setProbes((p) => ({ ...p, [account]: result }));
    } catch {
      setProbes((p) => {
        const next = { ...p };
        delete next[account];
        return next;
      });
    }
  }

  async function act(run: () => Promise<unknown>) {
    setActionError(null);
    try {
      await run();
      await reload();
    } catch (error) {
      setActionError(errorText(error, t));
    }
  }

  const toggle = (next: Panel) => setPanel((cur) => (samePanel(cur, next) ? null : next));

  const accounts = useMemo(() => {
    if (!config) return [];
    const list = [...(config.provider_accounts ?? [])];
    const holdsDefault = (a: ProviderAccountInfo) => modelsOf(config, a).some((m) => m.is_default);
    return list.sort((a, b) => {
      if (holdsDefault(a) !== holdsDefault(b)) return holdsDefault(a) ? -1 : 1;
      if (a.managed !== b.managed) return a.managed ? -1 : 1;
      return (a.label ?? a.id).localeCompare(b.label ?? b.id);
    });
  }, [config]);

  return (
    <>
      <div
        class="modal-overlay"
        onClick={(e) => {
          if (e.target === e.currentTarget) onClose();
        }}
      >
        <div class="modal-card model-config-modal">
          <div class="modal-header">
            <span>⚙</span>
            <h3>{t('settings.menuModel')}</h3>
            <button class="ghost-btn modal-close" onClick={onClose} aria-label={t('settings.close')}>
              ×
            </button>
          </div>
          <div class="modal-body">
            <div class="model-config-page">
              <div class="model-config-intro">
                <div>
                  <h4>{t('settings.modelsTitle')}</h4>
                  <p>{t('providers.intro')}</p>
                </div>
                {config && (
                  <code class="model-config-path" title={config.path}>
                    {config.path}
                  </code>
                )}
              </div>
              {loadError && (
                <div class="modal-error">
                  {t('settings.loadFailed')}: {loadError}
                </div>
              )}
              {actionError && (
                <div class="modal-error" role="alert">
                  {actionError}
                </div>
              )}
              {!config && !loadError && <div class="modal-loading">{t('settings.loading')}</div>}
              {config && (
                <>
                  <div class="provider-list account-list">
                    {accounts.map((account) => (
                      <AccountCard
                        key={account.id}
                        config={config}
                        account={account}
                        panel={panel}
                        probe={probes[account.id]}
                        onToggle={toggle}
                        onClosePanel={() => setPanel(null)}
                        onSaved={async (selection) => {
                          setPanel(null);
                          await reload();
                          if (account.probeable) void probe(account.id, selection);
                        }}
                        onProbe={() => void probe(account.id, modelsOf(config, account)[0]?.name)}
                        onApplyFix={(url) =>
                          void act(async () => {
                            await editAccount(account.id, { base_url: url });
                            void probe(account.id, modelsOf(config, account)[0]?.name);
                          })
                        }
                        onDefault={(id) => void act(() => setDefaultModel(id))}
                        onDeleteAccount={() =>
                          setConfirm({
                            kind: 'account',
                            id: account.id,
                            name: account.label ?? account.id,
                            n: modelsOf(config, account).length,
                          })
                        }
                        onDeleteModel={(m) =>
                          setConfirm({ kind: 'model', id: m.name, name: m.display_name || m.model })
                        }
                      />
                    ))}
                  </div>
                  {samePanel(panel, { kind: 'add' }) ? (
                    <AddProviderCard
                      config={config}
                      onCancel={() => setPanel(null)}
                      onCreated={async (account, models, probeable) => {
                        setPanel(null);
                        await reload();
                        if (probeable) void probe(account, models[0]);
                      }}
                    />
                  ) : (
                    <button class="model-provider-add" type="button" onClick={() => setPanel({ kind: 'add' })}>
                      <span>＋</span>
                      <span>{t('providers.addProvider')}</span>
                    </button>
                  )}
                </>
              )}
            </div>
          </div>
        </div>
      </div>
      {confirm && (
        <ConfirmDialog
          title={confirm.kind === 'account' ? t('providers.deleteAccountTitle') : t('settings.deleteTitle')}
          body={
            confirm.kind === 'account'
              ? t('providers.deleteAccountConfirm', { name: confirm.name, n: confirm.n })
              : t('settings.deleteConfirm', { name: confirm.name })
          }
          confirmLabel={t('settings.delete')}
          cancelLabel={t('common.cancel')}
          onConfirm={() =>
            act(() => (confirm.kind === 'account' ? deleteAccount(confirm.id) : deleteModelProfile(confirm.id)))
          }
          onClose={() => setConfirm(null)}
        />
      )}
    </>
  );
}

function AccountCard({
  config,
  account,
  panel,
  probe,
  onToggle,
  onClosePanel,
  onSaved,
  onProbe,
  onApplyFix,
  onDefault,
  onDeleteAccount,
  onDeleteModel,
}: {
  config: ConfigInfo;
  account: ProviderAccountInfo;
  panel: Panel | null;
  probe?: ProbeState;
  onToggle: (p: Panel) => void;
  onClosePanel: () => void;
  onSaved: (selection?: string) => Promise<void>;
  onProbe: () => void;
  onApplyFix: (url: string) => void;
  onDefault: (id: string) => void;
  onDeleteAccount: () => void;
  onDeleteModel: (m: ProviderInfo) => void;
}) {
  const { t } = useSettings();
  const models = modelsOf(config, account);
  const local = account.type === 'ollama';
  const ready = account.managed || account.has_api_key || local;
  // A custom endpoint may need no key at all; its missing one is not an error.
  const optionalKey = !ready && !!account.custom;
  const health = local
    ? t('providers.local')
    : ready
      ? t('providers.keyReady')
      : optionalKey
        ? t('providers.keyOptional')
        : t('providers.keyMissing');
  const label = account.label ?? account.id;
  const editable = !account.managed;
  return (
    <div class={'provider-card account-card' + (models.some((m) => m.is_default) ? ' default' : '')}>
      <div class="provider-card-head">
        <div class="provider-identity">
          <span class="provider-name">{label}</span>
          <span
            class={'provider-health' + (ready ? ' ready' : optionalKey ? ' neutral' : '')}
            role="img"
            aria-label={health}
            title={health}
          />
        </div>
        {account.preset_name && account.preset_name !== label && (
          <span class="provider-type">{account.preset_name}</span>
        )}
        {account.custom && <span class="account-tag">{t('providers.custom')}</span>}
        {account.managed && <span class="provider-managed-badge">{t('settings.officialCodingPlan')}</span>}
        {editable && (
          <div class="provider-card-actions">
            <button class="provider-action-btn" type="button" onClick={() => onToggle({ kind: 'models', id: account.id })}>
              {t('providers.addModels')}
            </button>
            <button class="provider-action-btn" type="button" onClick={() => onToggle({ kind: 'account', id: account.id })}>
              {t('providers.editAccount')}
            </button>
            <button class="provider-action-btn danger" type="button" onClick={onDeleteAccount}>
              {t('settings.delete')}
            </button>
          </div>
        )}
      </div>
      <div class="provider-card-body">
        {account.base_url && <code title={account.base_url}>{account.base_url}</code>}
        {account.managed && <span>{t('providers.managedHint')}</span>}
        {account.legacy && !account.managed && <span>{t('providers.legacyHint')}</span>}
        <ProbeLine probe={probe} onProbe={account.probeable && editable ? onProbe : undefined} onApplyFix={onApplyFix} />
      </div>
      {samePanel(panel, { kind: 'account', id: account.id }) && (
        <AccountEditor account={account} modelCount={models.length} onCancel={onClosePanel} onSaved={() => onSaved(models[0]?.name)} />
      )}
      {samePanel(panel, { kind: 'models', id: account.id }) && (
        <AddModelsPanel account={account} existing={models.map((m) => m.model)} onCancel={onClosePanel} onSaved={onSaved} />
      )}
      <div class="account-models">
        {models.length === 0 && <div class="field-hint">{t('providers.noModels')}</div>}
        {models.map((m) => (
          <div key={m.name} class="account-model">
            <div class="account-model-row">
              <span class="account-model-name">{m.display_name || m.model}</span>
              {m.display_name && <code>{m.model}</code>}
              {m.context_window ? <span class="account-model-meta">{formatTokens(m.context_window)}</span> : null}
              {m.supports_vision && <span class="account-model-meta">{t('providers.vision')}</span>}
              {m.is_default && <span class="provider-default-badge">{t('settings.default')}</span>}
              <div class="provider-card-actions">
                {!m.is_default && (
                  <button class="provider-action-btn" type="button" onClick={() => onDefault(m.name)}>
                    {t('settings.setAsDefault')}
                  </button>
                )}
                {editable && !m.managed && (
                  <>
                    <button class="provider-action-btn" type="button" onClick={() => onToggle({ kind: 'model', id: m.name })}>
                      {t('settings.edit')}
                    </button>
                    <button class="provider-action-btn danger" type="button" onClick={() => onDeleteModel(m)}>
                      {t('settings.delete')}
                    </button>
                  </>
                )}
              </div>
            </div>
            {samePanel(panel, { kind: 'model', id: m.name }) && (
              <ModelEditor model={m} onCancel={onClosePanel} onSaved={() => onSaved(m.name)} />
            )}
          </div>
        ))}
      </div>
    </div>
  );
}

/** The last connectivity check, said once under the account. */
function ProbeLine({
  probe,
  onProbe,
  onApplyFix,
}: {
  probe?: ProbeState;
  onProbe?: () => void;
  onApplyFix: (url: string) => void;
}) {
  const { t } = useSettings();
  if (probe === 'checking') return <span class="probe-line">{t('providers.checking')}</span>;
  if (!probe || !probe.probed) {
    return onProbe ? (
      <span class="probe-line">
        <button class="link-btn" type="button" onClick={onProbe}>
          {t('providers.probeNow')}
        </button>
      </span>
    ) : null;
  }
  return (
    <span class={'probe-line ' + (probe.ok ? 'ok' : 'nok')} role="status">
      {probe.ok ? `✓ ${t('providers.probeOk')}` : probe.message}
      {probe.fix && (
        <button class="link-btn" type="button" onClick={() => onApplyFix(probe.fix as string)}>
          {t('providers.probeApplyFix', { url: probe.fix })}
        </button>
      )}
      {onProbe && (
        <button class="link-btn" type="button" onClick={onProbe}>
          {t('providers.probeAgain')}
        </button>
      )}
    </span>
  );
}

/** A key field that never shows what is stored, and says what an empty one
 * means. */
function KeyInput({
  value,
  onInput,
  placeholder,
}: {
  value: string;
  onInput: (v: string) => void;
  placeholder: string;
}) {
  return (
    <input
      class="menu-input"
      type="password"
      autocomplete="new-password"
      spellcheck={false}
      value={value}
      placeholder={placeholder}
      onInput={(e) => onInput((e.target as HTMLInputElement).value)}
    />
  );
}

function AddProviderCard({
  config,
  onCancel,
  onCreated,
}: {
  config: ConfigInfo;
  onCancel: () => void;
  onCreated: (account: string, models: string[], probeable: boolean) => Promise<void>;
}) {
  const { t } = useSettings();
  const presets = config.provider_presets ?? [];
  const protocols = config.provider_protocols ?? [];
  const hasDefault = config.providers.some((p) => p.is_default);
  // Both drafts are kept: switching modes to look must not throw a typed form away.
  const [mode, setMode] = useState<'preset' | 'custom'>(presets.length > 0 ? 'preset' : 'custom');
  const [preset, setPreset] = useState<ProviderDraft>(() => blankDraft('preset', presets[0]?.id ?? ''));
  const [custom, setCustom] = useState<ProviderDraft>(() => blankDraft('custom', protocols[0]?.id ?? ''));
  const [setDefault, setSetDefault] = useState(!hasDefault);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const draft = mode === 'preset' ? preset : custom;
  const setDraft = mode === 'preset' ? setPreset : setCustom;
  const choices = mode === 'preset' ? presets : protocols;
  const chosen: ProviderPresetInfo | undefined = choices.find((p) => p.id === draft.provider);
  const takenIds = (config.provider_accounts ?? []).map((a) => a.id);
  const blocker = firstBlocker(draft, {
    takenIds,
    hasDefaultEndpoint: !!chosen?.default_base_url,
    requiresKey: mode === 'preset' && !!chosen?.requires_api_key && chosen?.type !== 'ollama',
  });
  const endpoint = draft.baseUrl.trim() || chosen?.default_base_url || '';

  async function submit() {
    if (blocker || saving) return;
    setSaving(true);
    setError(null);
    try {
      const res = await createAccount({
        id: mode === 'custom' ? draft.id.trim() : undefined,
        provider: draft.provider,
        display_name: draft.displayName.trim() || undefined,
        base_url: draft.baseUrl.trim() || undefined,
        api_key: draft.apiKey.trim() || undefined,
        models: draft.models.map(modelBody),
        set_default: setDefault,
      });
      await onCreated(res.account, res.models, chosen?.type === 'openai');
    } catch (e) {
      setError(errorText(e, t));
    } finally {
      setSaving(false);
    }
  }

  return (
    <div class="provider-card add-provider-card">
      <div class="field-group">
        <div class="segmented" role="tablist">
          {(['preset', 'custom'] as const).map((m) => (
            <button
              key={m}
              type="button"
              role="tab"
              aria-selected={mode === m}
              class={'segmented-btn' + (mode === m ? ' active' : '')}
              disabled={saving || (m === 'preset' && presets.length === 0)}
              onClick={() => setMode(m)}
            >
              {m === 'preset' ? t('providers.modePreset') : t('providers.modeCustom')}
            </button>
          ))}
        </div>
        <span class="field-hint">{mode === 'preset' ? t('providers.modePresetHint') : t('providers.modeCustomHint')}</span>
      </div>

      <div class="add-model-field">
        <label class="add-model-label">{mode === 'preset' ? t('providers.vendor') : t('providers.protocol')}</label>
        <Select
          value={draft.provider}
          options={choices.map((p) => ({ value: p.id, label: p.display_name }))}
          onChange={(v) => setDraft({ ...draft, provider: v })}
        />
      </div>

      {mode === 'custom' && (
        <div class="add-model-field">
          <label class="add-model-label">{t('providers.accountId')}</label>
          <input
            class="menu-input"
            type="text"
            spellcheck={false}
            placeholder="my-gateway"
            value={draft.id}
            onInput={(e) => setDraft({ ...draft, id: (e.target as HTMLInputElement).value })}
          />
          <span class="field-hint">{t('providers.accountIdHint')}</span>
        </div>
      )}

      {mode === 'custom' && (
        <div class="add-model-field">
          <label class="add-model-label">{t('providers.baseUrl')}</label>
          <input
            class="menu-input"
            type="url"
            spellcheck={false}
            placeholder={chosen?.type === 'anthropic' ? 'https://gateway.example' : 'https://gateway.example/v1'}
            value={draft.baseUrl}
            onInput={(e) => setDraft({ ...draft, baseUrl: (e.target as HTMLInputElement).value })}
          />
        </div>
      )}

      <div class="add-model-field">
        <label class="add-model-label">{t('providers.apiKey')}</label>
        {chosen?.type === 'ollama' ? (
          <span class="field-hint">{t('providers.apiKeyNone')}</span>
        ) : (
          <KeyInput
            value={draft.apiKey}
            onInput={(v) => setDraft({ ...draft, apiKey: v })}
            placeholder={mode === 'custom' ? t('providers.apiKeyOptional') : 'sk-…'}
          />
        )}
      </div>

      <details class="add-provider-advanced">
        <summary>{t('providers.advanced')}</summary>
        <div class="add-model-field">
          <label class="add-model-label">
            {t('providers.displayName')} <small>{t('providers.optional')}</small>
          </label>
          <input
            class="menu-input"
            type="text"
            value={draft.displayName}
            placeholder={chosen?.display_name ?? ''}
            onInput={(e) => setDraft({ ...draft, displayName: (e.target as HTMLInputElement).value })}
          />
        </div>
        {mode === 'preset' && (
          <div class="add-model-field">
            <label class="add-model-label">{t('providers.baseUrl')}</label>
            <input
              class="menu-input"
              type="url"
              spellcheck={false}
              placeholder={chosen?.default_base_url ?? ''}
              value={draft.baseUrl}
              onInput={(e) => setDraft({ ...draft, baseUrl: (e.target as HTMLInputElement).value })}
            />
            {chosen?.default_base_url && (
              <span class="field-hint">{t('providers.baseUrlDefault', { url: chosen.default_base_url })}</span>
            )}
          </div>
        )}
      </details>

      <ModelRowsEditor
        rows={draft.models}
        setRows={(models) => setDraft({ ...draft, models })}
        discover={
          chosen?.discoverable !== false && endpoint && validBaseUrl(endpoint) && !apiKeyProblem(draft.apiKey)
            ? () =>
                discoverProviderModels({
                  type: chosen?.type ?? 'openai',
                  base_url: endpoint,
                  api_key: draft.apiKey.trim() || undefined,
                })
            : null
        }
        discoverable={chosen?.discoverable !== false}
      />

      <label class="field-row add-provider-default">
        <input type="checkbox" checked={setDefault} onChange={() => setSetDefault(!setDefault)} />
        <span>{t('providers.setFirstDefault')}</span>
      </label>

      {error && (
        <div class="modal-error" role="alert">
          {error}
        </div>
      )}
      <div class="add-model-actions">
        {blocker && <span class="form-blocker">{blockerText(blocker, t)}</span>}
        <button class="btn" type="button" onClick={onCancel} disabled={saving}>
          {t('common.cancel')}
        </button>
        <button class="btn btn-primary" type="button" disabled={!!blocker || saving} onClick={() => void submit()}>
          {saving ? t('providers.saving') : t('providers.create')}
        </button>
      </div>
    </div>
  );
}

function blankDraft(mode: 'preset' | 'custom', provider: string): ProviderDraft {
  return { mode, provider, id: '', displayName: '', baseUrl: '', apiKey: '', models: [emptyModelDraft()] };
}

/** Model rows, by hand or picked from what the endpoint lists. */
function ModelRowsEditor({
  rows,
  setRows,
  discover,
  discoverable,
  existing = [],
}: {
  rows: ModelDraft[];
  setRows: (rows: ModelDraft[]) => void;
  /** `null` while the form has not got what a listing needs yet. */
  discover: (() => Promise<DiscoveredModelInfo[]>) | null;
  discoverable: boolean;
  existing?: string[];
}) {
  const { t } = useSettings();
  const [fetching, setFetching] = useState(false);
  const [fetchError, setFetchError] = useState<string | null>(null);
  const [picking, setPicking] = useState<DiscoveredModelInfo[] | null>(null);

  async function fetchModels() {
    if (!discover) return;
    setFetching(true);
    setFetchError(null);
    try {
      setPicking(await discover());
    } catch (e) {
      setFetchError(e instanceof Error ? e.message : String(e));
    } finally {
      setFetching(false);
    }
  }

  const update = (i: number, patch: Partial<ModelDraft>) =>
    setRows(rows.map((r, j) => (j === i ? { ...r, ...patch } : r)));

  return (
    <div class="add-model-field">
      <div class="add-model-label-row">
        <label class="add-model-label">{t('providers.models')}</label>
        {discoverable ? (
          <button class="provider-action-btn" type="button" disabled={!discover || fetching} onClick={() => void fetchModels()}>
            {fetching ? t('providers.fetching') : t('providers.fetchModels')}
          </button>
        ) : (
          <span class="field-hint">{t('providers.fetchUnsupported')}</span>
        )}
      </div>
      {fetchError && <div class="modal-error">{fetchError}</div>}
      <div class="model-draft-list">
        <ModelColumns removable />
        {rows.map((row, i) => (
          <div key={i} class="model-draft-row">
            <input
              class="menu-input model-draft-id"
              type="text"
              spellcheck={false}
              placeholder={t('providers.modelId')}
              aria-label={t('providers.modelId')}
              value={row.model}
              onInput={(e) => update(i, { model: (e.target as HTMLInputElement).value })}
            />
            <input
              class="menu-input"
              type="text"
              placeholder={t('providers.displayName')}
              aria-label={t('providers.displayName')}
              value={row.displayName}
              onInput={(e) => update(i, { displayName: (e.target as HTMLInputElement).value })}
            />
            <input
              class={'menu-input model-draft-num' + (parseTokens(row.window) === null ? ' invalid' : '')}
              type="text"
              placeholder={t('providers.contextWindowPlaceholder')}
              aria-label={t('providers.contextWindow')}
              title={t('providers.contextWindow')}
              value={row.window}
              onInput={(e) => update(i, { window: (e.target as HTMLInputElement).value })}
            />
            <input
              class={'menu-input model-draft-num' + (parseTokens(row.maxTokens) === null ? ' invalid' : '')}
              type="text"
              placeholder={t('providers.maxTokensPlaceholder')}
              aria-label={t('providers.maxTokens')}
              title={t('providers.maxTokens')}
              value={row.maxTokens}
              onInput={(e) => update(i, { maxTokens: (e.target as HTMLInputElement).value })}
            />
            <Select
              value={row.vision === null ? 'auto' : row.vision ? 'on' : 'off'}
              options={[
                { value: 'auto', label: t('providers.visionAuto') },
                { value: 'on', label: t('providers.visionOn') },
                { value: 'off', label: t('providers.visionOff') },
              ]}
              onChange={(v) => update(i, { vision: v === 'auto' ? null : v === 'on' })}
            />
            <button
              class="provider-action-btn danger"
              type="button"
              disabled={rows.length === 1}
              onClick={() => setRows(rows.filter((_, j) => j !== i))}
            >
              {t('providers.removeRow')}
            </button>
          </div>
        ))}
      </div>
      <div>
        <button class="provider-action-btn" type="button" onClick={() => setRows([...rows, emptyModelDraft()])}>
          ＋ {t('providers.addModelRow')}
        </button>
      </div>
      {picking && (
        <ModelPicker
          models={picking}
          taken={[...existing, ...rows.map((r) => r.model.trim()).filter(Boolean)]}
          onCancel={() => setPicking(null)}
          onAdd={(picked) => {
            setRows(adoptPicked(rows, picked, existing));
            setPicking(null);
          }}
        />
      )}
    </div>
  );
}

/** The column names over model rows: the placeholders alone said too little
 * once a value was typed over them. */
function ModelColumns({ removable }: { removable?: boolean }) {
  const { t } = useSettings();
  return (
    <div class={'model-draft-row model-draft-head' + (removable ? '' : ' no-remove')} aria-hidden="true">
      <span>{t('providers.modelId')}</span>
      <span>{t('providers.displayName')}</span>
      <span>{t('providers.contextWindow')}</span>
      <span>{t('providers.maxTokens')}</span>
      <span>{t('providers.vision')}</span>
      {removable && <span />}
    </div>
  );
}

/** What an endpoint lists, to pick several from at once. Models already in
 * the form or the account are shown and cannot be picked twice. */
function ModelPicker({
  models,
  taken,
  onCancel,
  onAdd,
}: {
  models: DiscoveredModelInfo[];
  taken: string[];
  onCancel: () => void;
  onAdd: (picked: DiscoveredModelInfo[]) => void;
}) {
  const { t } = useSettings();
  const [search, setSearch] = useState('');
  const [selected, setSelected] = useState<string[]>([]);
  const takenSet = new Set(taken);
  const q = search.trim().toLowerCase();
  const visible = models.filter(
    (m) => !q || m.id.toLowerCase().includes(q) || (m.name ?? '').toLowerCase().includes(q),
  );
  const pickable = visible.filter((m) => !takenSet.has(m.id)).map((m) => m.id);
  return (
    <div
      class="modal-overlay"
      onClick={(e) => {
        if (e.target === e.currentTarget) onCancel();
      }}
    >
      <div class="modal-card modal-card-sm model-picker-modal" role="dialog" aria-label={t('providers.pickTitle')}>
        <div class="modal-header">
          <h3>{t('providers.pickTitle')}</h3>
          <button class="ghost-btn modal-close" onClick={onCancel} aria-label={t('settings.close')}>
            ×
          </button>
        </div>
        <div class="modal-body">
          <div class="model-discovery-picker model-discovery-picker-multi">
            <input
              class="menu-input"
              type="search"
              placeholder={t('providers.pickSearch')}
              value={search}
              onInput={(e) => setSearch((e.target as HTMLInputElement).value)}
            />
            <div class="model-picker-bulk">
              <button
                class="link-btn"
                type="button"
                onClick={() => setSelected([...new Set([...selected, ...pickable])])}
              >
                {t('providers.pickAll')}
              </button>
              <button class="link-btn" type="button" onClick={() => setSelected([])}>
                {t('providers.pickNone')}
              </button>
            </div>
            <div class="model-discovery-results">
              {visible.length === 0 && <div class="field-hint">{t('providers.pickEmpty')}</div>}
              {visible.map((m) => {
                const exists = takenSet.has(m.id);
                const checked = selected.includes(m.id);
                return (
                  <button
                    key={m.id}
                    class={'model-discovery-option' + (checked ? ' active' : '')}
                    type="button"
                    disabled={exists}
                    title={m.name ?? m.id}
                    onClick={() =>
                      setSelected((cur) => (cur.includes(m.id) ? cur.filter((id) => id !== m.id) : [...cur, m.id]))
                    }
                  >
                    <span class="model-discovery-checkbox" aria-hidden="true">
                      {checked ? '✓' : ''}
                    </span>
                    <code>{m.id}</code>
                    {m.context_window ? <small>{formatTokens(m.context_window)}</small> : null}
                    {exists && <small>{t('providers.pickAlready')}</small>}
                  </button>
                );
              })}
            </div>
          </div>
        </div>
        <div class="modal-footer">
          <button class="btn" type="button" onClick={onCancel}>
            {t('common.cancel')}
          </button>
          <button
            class="btn btn-primary"
            type="button"
            disabled={selected.length === 0}
            onClick={() => onAdd(models.filter((m) => selected.includes(m.id)))}
          >
            {t('providers.pickAdd', { n: selected.length })}
          </button>
        </div>
      </div>
    </div>
  );
}

/** The connection, which every model on the account shares — said so. */
function AccountEditor({
  account,
  modelCount,
  onCancel,
  onSaved,
}: {
  account: ProviderAccountInfo;
  modelCount: number;
  onCancel: () => void;
  onSaved: () => Promise<void>;
}) {
  const { t } = useSettings();
  const [displayName, setDisplayName] = useState(account.display_name ?? '');
  const [baseUrl, setBaseUrl] = useState(account.base_url ?? '');
  const [apiKey, setApiKey] = useState('');
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const local = account.type === 'ollama';
  const keyProblem = apiKeyProblem(apiKey);
  const urlBad = baseUrl.trim() !== '' && !validBaseUrl(baseUrl);
  const blocker = keyProblem
    ? t(`providers.block.key_${keyProblem}` as MsgKey)
    : urlBad
      ? t('providers.block.base_url')
      : null;

  async function save() {
    if (blocker || saving) return;
    setSaving(true);
    setError(null);
    try {
      await editAccount(account.id, {
        display_name: displayName.trim(),
        base_url: baseUrl.trim(),
        ...(apiKey.trim() ? { api_key: apiKey.trim() } : {}),
      });
      await onSaved();
    } catch (e) {
      setError(errorText(e, t));
    } finally {
      setSaving(false);
    }
  }

  return (
    <div class="inline-editor">
      <span class="field-hint">{t('providers.editAccountHint', { n: modelCount })}</span>
      <div class="add-model-field">
        <label class="add-model-label">{t('providers.displayName')}</label>
        <input
          class="menu-input"
          type="text"
          value={displayName}
          placeholder={account.preset_name ?? account.id}
          onInput={(e) => setDisplayName((e.target as HTMLInputElement).value)}
        />
      </div>
      <div class="add-model-field">
        <label class="add-model-label">{t('providers.baseUrl')}</label>
        <input
          class="menu-input"
          type="url"
          spellcheck={false}
          value={baseUrl}
          onInput={(e) => setBaseUrl((e.target as HTMLInputElement).value)}
        />
      </div>
      {!local && (
        <div class="add-model-field">
          <label class="add-model-label">{t('providers.apiKey')}</label>
          <KeyInput
            value={apiKey}
            onInput={setApiKey}
            placeholder={account.has_api_key ? t('providers.apiKeyKeep') : 'sk-…'}
          />
        </div>
      )}
      {error && (
        <div class="modal-error" role="alert">
          {error}
        </div>
      )}
      <div class="add-model-actions">
        {blocker && <span class="form-blocker">{blocker}</span>}
        <button class="btn" type="button" onClick={onCancel} disabled={saving}>
          {t('common.cancel')}
        </button>
        <button class="btn btn-primary" type="button" disabled={!!blocker || saving} onClick={() => void save()}>
          {saving ? t('providers.saving') : t('providers.save')}
        </button>
      </div>
    </div>
  );
}

/** More models under an account that already exists; its saved key is used to
 * list them and never comes back to the browser. */
function AddModelsPanel({
  account,
  existing,
  onCancel,
  onSaved,
}: {
  account: ProviderAccountInfo;
  existing: string[];
  onCancel: () => void;
  onSaved: (selection?: string) => Promise<void>;
}) {
  const { t } = useSettings();
  const [rows, setRows] = useState<ModelDraft[]>([emptyModelDraft()]);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const blocker = modelsBlocker(rows, existing);

  async function save() {
    if (blocker || saving) return;
    setSaving(true);
    setError(null);
    try {
      const res = await addAccountModels(account.id, rows.map(modelBody));
      await onSaved(res.created[0]);
    } catch (e) {
      setError(errorText(e, t));
    } finally {
      setSaving(false);
    }
  }

  const discoverable = account.discoverable !== false;
  return (
    <div class="inline-editor">
      <ModelRowsEditor
        rows={rows}
        setRows={setRows}
        existing={existing}
        discoverable={discoverable}
        discover={
          discoverable && account.base_url
            ? () =>
                discoverProviderModels({
                  type: account.type,
                  base_url: account.base_url as string,
                  provider_name: account.id,
                })
            : null
        }
      />
      {error && (
        <div class="modal-error" role="alert">
          {error}
        </div>
      )}
      <div class="add-model-actions">
        {blocker && <span class="form-blocker">{blockerText(blocker, t)}</span>}
        <button class="btn" type="button" onClick={onCancel} disabled={saving}>
          {t('common.cancel')}
        </button>
        <button class="btn btn-primary" type="button" disabled={!!blocker || saving} onClick={() => void save()}>
          {saving ? t('providers.saving') : t('providers.create')}
        </button>
      </div>
    </div>
  );
}

/** One model's own settings. Its account is not touched. */
function ModelEditor({
  model,
  onCancel,
  onSaved,
}: {
  model: ProviderInfo;
  onCancel: () => void;
  onSaved: () => Promise<void>;
}) {
  const { t } = useSettings();
  const [displayName, setDisplayName] = useState(model.display_name ?? '');
  const [ctxWindow, setCtxWindow] = useState(model.context_window ? formatTokens(model.context_window) : '');
  const [maxTokens, setMaxTokens] = useState(model.max_tokens ? formatTokens(model.max_tokens) : '');
  const [vision, setVision] = useState<'auto' | 'on' | 'off'>(
    model.supports_vision_override === null || model.supports_vision_override === undefined
      ? 'auto'
      : model.supports_vision_override
        ? 'on'
        : 'off',
  );
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const parsedWindow = parseTokens(ctxWindow);
  const parsedMax = parseTokens(maxTokens);
  const blocker =
    parsedWindow === null
      ? t('providers.block.model_window', { row: 1 })
      : parsedMax === null
        ? t('providers.block.model_max_tokens', { row: 1 })
        : null;

  async function save() {
    if (blocker || saving) return;
    setSaving(true);
    setError(null);
    try {
      await editModelProfile(model.name, {
        display_name: displayName.trim(),
        ...(typeof parsedWindow === 'number' ? { context_window: parsedWindow } : {}),
        ...(typeof parsedMax === 'number'
          ? { max_tokens: parsedMax }
          : model.max_tokens
            ? { clear_max_tokens: true }
            : {}),
        ...(vision === 'auto' ? { clear_supports_vision: true } : { supports_vision: vision === 'on' }),
      });
      await onSaved();
    } catch (e) {
      setError(errorText(e, t));
    } finally {
      setSaving(false);
    }
  }

  return (
    <div class="inline-editor">
      <ModelColumns />
      <div class="model-draft-row no-remove">
        <input class="menu-input model-draft-id" type="text" value={model.model} disabled aria-label={t('providers.modelId')} />
        <input
          class="menu-input"
          type="text"
          placeholder={t('providers.displayName')}
          aria-label={t('providers.displayName')}
          value={displayName}
          onInput={(e) => setDisplayName((e.target as HTMLInputElement).value)}
        />
        <input
          class={'menu-input model-draft-num' + (parsedWindow === null ? ' invalid' : '')}
          type="text"
          placeholder={t('providers.contextWindowPlaceholder')}
          aria-label={t('providers.contextWindow')}
          title={t('providers.contextWindow')}
          value={ctxWindow}
          onInput={(e) => setCtxWindow((e.target as HTMLInputElement).value)}
        />
        <input
          class={'menu-input model-draft-num' + (parsedMax === null ? ' invalid' : '')}
          type="text"
          placeholder={t('providers.maxTokensPlaceholder')}
          aria-label={t('providers.maxTokens')}
          title={t('providers.maxTokens')}
          value={maxTokens}
          onInput={(e) => setMaxTokens((e.target as HTMLInputElement).value)}
        />
        <Select
          value={vision}
          options={[
            { value: 'auto', label: t('providers.visionAuto') },
            { value: 'on', label: t('providers.visionOn') },
            { value: 'off', label: t('providers.visionOff') },
          ]}
          onChange={(v) => setVision(v)}
        />
      </div>
      {error && (
        <div class="modal-error" role="alert">
          {error}
        </div>
      )}
      <div class="add-model-actions">
        {blocker && <span class="form-blocker">{blocker}</span>}
        <button class="btn" type="button" onClick={onCancel} disabled={saving}>
          {t('common.cancel')}
        </button>
        <button class="btn btn-primary" type="button" disabled={!!blocker || saving} onClick={() => void save()}>
          {saving ? t('providers.saving') : t('providers.save')}
        </button>
      </div>
    </div>
  );
}
