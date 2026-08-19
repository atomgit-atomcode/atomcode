// Individual settings dialogs: theme, language, and (read-only) model config.
// Each is opened on its own from the sidebar settings menu.

import { ComponentChildren } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import {
  getConfig,
  ConfigInfo,
  ProviderInfo,
  ProviderAccountInfo,
  ProviderPresetInfo,
  createProvider,
  createModelsForAccount,
  updateProvider,
  setDefaultProvider,
  deleteProvider,
  discoverProviderModels,
  DiscoveredModelInfo,
  getTunnelStatus,
  TunnelStatus,
} from '../api';
import { useSettings, Theme, FontScale } from '../settings';
import { Lang } from '../i18n';
import { ConfirmDialog } from './ConfirmDialog';
import { Select } from './Select';
import {
  loadPrefs,
  savePrefs,
  requestNotificationPermission,
  notificationsSupported,
  type NotificationPrefs,
} from '../lib/notifications';

// AtomGit 托管 provider 的 LLM 网关地址；其上下文窗口由平台固定，前端禁止修改。
const ATOMGIT_BASE_URL = 'https://llm-api.atomgit.com/v1';

// 上下文窗口预设（数值与配置一致，显示时按 /1000 换算为「k tokens」）。
const CONTEXT_WINDOW_PRESETS = [32000, 64000, 128000, 256000, 512000, 1000000];

/** 把 context_window 数值格式化为下拉标签：1000000 → "1M"，其余 → "<n>K"。 */
function fmtContextWindow(v: number): string {
  return v >= 1000000 ? `${v / 1000000}M` : `${Math.round(v / 1000)}K`;
}

function isManagedProvider(provider: ProviderInfo): boolean {
  return provider.requires_login === true || provider.base_url === ATOMGIT_BASE_URL;
}

/** Shared modal chrome for the settings dialogs. */
function SettingsModal({
  title,
  wide,
  cardClass,
  hideFooter,
  onClose,
  children,
}: {
  title: string;
  wide?: boolean;
  cardClass?: string;
  // 弹窗自带底部操作（如「添加模型」的 关闭/添加）时隐藏这里的页脚关闭，避免重复。
  hideFooter?: boolean;
  onClose: () => void;
  children: ComponentChildren;
}) {
  const { t } = useSettings();
  return (
    <div
      class="modal-overlay"
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div class={'modal-card' + (wide ? '' : ' modal-card-sm') + (cardClass ? ` ${cardClass}` : '')}>
        <div class="modal-header">
          <span>⚙</span>
          <h3>{title}</h3>
          <button class="ghost-btn modal-close" onClick={onClose} aria-label={t('settings.close')}>
            ×
          </button>
        </div>
        <div class="modal-body">{children}</div>
        {!hideFooter && (
          <div class="modal-footer">
            <button class="btn" onClick={onClose}>
              {t('settings.close')}
            </button>
          </div>
        )}
      </div>
    </div>
  );
}

export function ThemeDialog({ onClose }: { onClose: () => void }) {
  const { theme, setTheme, fontScale, setFontScale, t } = useSettings();
  const options: { value: Theme; label: string }[] = [
    { value: 'light', label: t('settings.theme.light') },
    { value: 'dark', label: t('settings.theme.dark') },
    { value: 'system', label: t('settings.theme.system') },
  ];
  // Grouped with the theme rather than given a menu entry of its own: both
  // answer "how should this look", and one dialog keeps the sidebar short.
  const scales: { value: FontScale; label: string }[] = [
    { value: 'small', label: t('settings.fontScale.small') },
    { value: 'normal', label: t('settings.fontScale.normal') },
    { value: 'large', label: t('settings.fontScale.large') },
    { value: 'xlarge', label: t('settings.fontScale.xlarge') },
  ];
  return (
    <SettingsModal title={t('settings.menuTheme')} onClose={onClose}>
      <div class="field-group">
        <span class="modal-label">{t('settings.theme')}</span>
        <div class="segmented">
          {options.map((o) => (
            <button
              key={o.value}
              class={'segmented-btn' + (theme === o.value ? ' active' : '')}
              onClick={() => setTheme(o.value)}
              type="button"
            >
              {o.label}
            </button>
          ))}
        </div>
      </div>
      <div class="field-group">
        <span class="modal-label">{t('settings.fontScale')}</span>
        <div class="segmented">
          {scales.map((o) => (
            <button
              key={o.value}
              class={'segmented-btn' + (fontScale === o.value ? ' active' : '')}
              onClick={() => setFontScale(o.value)}
              type="button"
            >
              {o.label}
            </button>
          ))}
        </div>
      </div>
    </SettingsModal>
  );
}

export function LanguageDialog({ onClose }: { onClose: () => void }) {
  const { lang, setLang, t } = useSettings();
  const options: { value: Lang; label: string }[] = [
    { value: 'zh', label: '中文' },
    { value: 'en', label: 'English' },
  ];
  return (
    <SettingsModal title={t('settings.menuLang')} onClose={onClose}>
      <div class="field-group">
        <span class="modal-label">{t('settings.language')}</span>
        <div class="segmented">
          {options.map((o) => (
            <button
              key={o.value}
              class={'segmented-btn' + (lang === o.value ? ' active' : '')}
              onClick={() => setLang(o.value)}
              type="button"
            >
              {o.label}
            </button>
          ))}
        </div>
      </div>
    </SettingsModal>
  );
}

export function NotificationsDialog({ onClose }: { onClose: () => void }) {
  const { t } = useSettings();
  const supported = notificationsSupported();
  const [prefs, setPrefsState] = useState<NotificationPrefs>(() => loadPrefs());
  const [permission, setPermission] = useState<NotificationPermission>(() =>
    typeof Notification !== 'undefined' ? Notification.permission : 'denied',
  );

  function setPrefs(next: NotificationPrefs) {
    setPrefsState(next);
    savePrefs(next);
  }

  async function grantPermission() {
    const granted = await requestNotificationPermission();
    const actual: NotificationPermission =
      typeof Notification !== 'undefined'
        ? Notification.permission
        : granted
          ? 'granted'
          : 'denied';
    setPermission(actual);
    return actual;
  }

  // 用户手势内请求权限：开启开关时若尚未授权，先请求再落库。
  async function toggleEnabled() {
    if (!prefs.enabled && permission !== 'granted') {
      const actual = await grantPermission();
      if (actual !== 'granted') return; // 未授予则不开启，避免“开了但不弹”的静默失效。
    }
    setPrefs({ ...prefs, enabled: !prefs.enabled });
  }

  function setMinDurationSecs(v: string) {
    const n = Number(v);
    if (!Number.isFinite(n) || n < 0) return;
    setPrefs({ ...prefs, minDurationSecs: Math.floor(n) });
  }

  return (
    <SettingsModal title={t('settings.notifications.title')} onClose={onClose}>
      <div class="field-group">
        {!supported && (
          <div class="field-hint">{t('settings.notifications.unsupported')}</div>
        )}
        <div class="field-row">
          <span class="modal-label">{t('settings.notifications.enabled')}</span>
          <input
            type="checkbox"
            checked={prefs.enabled}
            disabled={!supported}
            onChange={toggleEnabled}
          />
        </div>
        {prefs.enabled && supported && (
          <>
            <div class="field-row">
              <span class="modal-label">{t('settings.notifications.backgroundOnly')}</span>
              <input
                type="checkbox"
                checked={prefs.backgroundOnly}
                onChange={() => setPrefs({ ...prefs, backgroundOnly: !prefs.backgroundOnly })}
              />
            </div>
            <div class="field-row">
              <span class="modal-label">{t('settings.notifications.minDuration')}</span>
              <input
                type="number"
                min={0}
                step={1}
                value={prefs.minDurationSecs}
                disabled={!prefs.enabled}
                onInput={(e) => setMinDurationSecs((e.target as HTMLInputElement).value)}
              />
            </div>
          </>
        )}
        <div class="field-hint">
          {permission === 'granted' && t('settings.notifications.permissionGranted')}
          {permission === 'default' && t('settings.notifications.permissionDefault')}
          {permission === 'denied' && t('settings.notifications.permissionDenied')}
        </div>
        {supported && permission === 'default' && (
          <button class="btn" type="button" onClick={() => void grantPermission()}>
            {t('settings.notifications.grantPermission')}
          </button>
        )}
      </div>
    </SettingsModal>
  );
}

export function ModelConfigDialog({ onClose }: { onClose: () => void }) {
  const { t } = useSettings();
  const [config, setConfig] = useState<ConfigInfo | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [addMode, setAddMode] = useState<'preset' | 'custom' | null>(null);
  const [addingModels, setAddingModels] = useState(false);
  const [editTarget, setEditTarget] = useState<ProviderInfo | null>(null);
  const [deleteTarget, setDeleteTarget] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);

  const reload = () => {
    setLoadError(null);
    getConfig()
      .then(setConfig)
      .catch((e: unknown) => setLoadError(e instanceof Error ? e.message : String(e)));
  };

  useEffect(() => { reload(); }, []);

  async function makeDefault(name: string) {
    setActionError(null);
    try {
      await setDefaultProvider(name);
      reload();
    } catch (error: unknown) {
      setActionError(error instanceof Error ? error.message : String(error));
    }
  }

  return (
    <>
    <SettingsModal
      title={t('settings.menuModel')}
      wide
      cardClass="model-config-modal"
      onClose={onClose}
    >
      <div class="model-config-page">
        <div class="model-config-intro">
          <div>
            <h4>{t('settings.modelsTitle')}</h4>
            <p>{t('settings.modelsIntro')}</p>
          </div>
          {config && <code class="model-config-path" title={config.path}>{config.path}</code>}
        </div>
        {loadError && <div class="modal-error">{t('settings.loadFailed')}: {loadError}</div>}
        {actionError && <div class="modal-error">{actionError}</div>}
        {!config && !loadError && <div class="modal-loading">{t('settings.loading')}</div>}
        {config && (
          <>
            <div class="provider-list model-provider-list">
              {[...config.providers].sort((a, b) => {
                if (a.is_default !== b.is_default) return a.is_default ? -1 : 1;
                return a.name.localeCompare(b.name);
              }).map((p) => (
                <div
                  key={p.name}
                  class={'provider-card model-provider-card' + (p.is_default ? ' default' : '')}
                >
                  <div class="provider-card-head">
                    <div class="provider-identity">
                      <span class="provider-name">{p.name}</span>
                      <span class={'provider-health' + (p.has_api_key || p.type === 'ollama' ? ' ready' : '')} />
                    </div>
                    {p.is_default && (
                      <span class="provider-default-badge">{t('settings.default')}</span>
                    )}
                    {isManagedProvider(p) && (
                      <span class="provider-managed-badge">{t('settings.officialCodingPlan')}</span>
                    )}
                    <span class="provider-type">{p.type}</span>
                    <div class="provider-card-actions">
                      {!p.is_default && (
                        <button class="provider-action-btn" type="button" onClick={() => void makeDefault(p.name)}>
                          {t('settings.setAsDefault')}
                        </button>
                      )}
                      {!isManagedProvider(p) && (
                        <>
                          <button class="provider-action-btn" type="button" onClick={() => setEditTarget(p)}>
                            {t('settings.edit')}
                          </button>
                          <button class="provider-action-btn danger" type="button" onClick={() => setDeleteTarget(p.name)}>
                            {t('settings.delete')}
                          </button>
                        </>
                      )}
                    </div>
                  </div>
                  <div class="provider-card-body">
                    <span>{p.model}</span>
                    {p.context_window && <span>{fmtContextWindow(p.context_window)} tokens</span>}
                    {!isManagedProvider(p) && (
                      <span class={p.has_api_key || p.type === 'ollama' ? 'ok' : 'nok'}>
                        {p.type === 'ollama'
                          ? t('settings.localProvider')
                          : p.has_api_key
                            ? t('settings.configured')
                            : t('settings.notConfigured')}
                      </span>
                    )}
                    {p.base_url && <code title={p.base_url}>{p.base_url}</code>}
                  </div>
                </div>
              ))}
            </div>
            <div class="model-provider-add-grid">
              {(config.provider_accounts?.some((account) => !account.managed) ?? false) && (
                <button class="model-provider-add" type="button" onClick={() => setAddingModels(true)}>
                  <span>＋</span>
                  <span>{t('settings.addModel')}</span>
                </button>
              )}
              {(config.provider_presets?.length ?? 0) > 0 && (
                <button class="model-provider-add" type="button" onClick={() => setAddMode('preset')}>
                  <span>＋</span>
                  <span>{t('settings.addProvider')}</span>
                </button>
              )}
              <button class="model-provider-add" type="button" onClick={() => setAddMode('custom')}>
                <span>＋</span>
                <span>{t('settings.addCustomProvider')}</span>
              </button>
            </div>
          </>
        )}
      </div>
    </SettingsModal>
    {addMode && (
      <ProviderFormDialog
        custom={addMode === 'custom'}
        presets={config?.provider_presets ?? []}
        existingNames={config?.providers.map((p) => p.name) ?? []}
        onClose={() => setAddMode(null)}
        onSaved={() => {
          setAddMode(null);
          reload();
        }}
      />
    )}
    {addingModels && config && (
      <AddAccountModelsDialog
        accounts={(config.provider_accounts ?? []).filter((account) => !account.managed)}
        providers={config.providers}
        onClose={() => setAddingModels(false)}
        onSaved={() => {
          setAddingModels(false);
          reload();
        }}
      />
    )}
    {editTarget && (
      <ProviderFormDialog
        editing={editTarget}
        custom
        presets={config?.provider_presets ?? []}
        existingNames={config?.providers.map((p) => p.name) ?? []}
        onClose={() => setEditTarget(null)}
        onSaved={() => {
          setEditTarget(null);
          reload();
        }}
      />
    )}
    {deleteTarget && (
      <ConfirmDialog
        title={t('settings.deleteTitle')}
        body={t('settings.deleteConfirm', { name: deleteTarget })}
        confirmLabel={t('settings.delete')}
        cancelLabel={t('common.cancel')}
        onConfirm={async () => {
          await deleteProvider(deleteTarget);
          reload();
        }}
        onClose={() => setDeleteTarget(null)}
      />
    )}
    </>
  );
}

/** Add several model profiles under one existing account without ever
 * round-tripping its credential through the browser. */
function AddAccountModelsDialog({
  accounts,
  providers,
  onClose,
  onSaved,
}: {
  accounts: ProviderAccountInfo[];
  providers: ProviderInfo[];
  onClose: () => void;
  onSaved: () => void;
}) {
  const { t } = useSettings();
  const [accountId, setAccountId] = useState(accounts[0]?.id ?? '');
  const [models, setModels] = useState<DiscoveredModelInfo[] | null>(null);
  const [selected, setSelected] = useState<string[]>([]);
  const [manualModel, setManualModel] = useState('');
  const [search, setSearch] = useState('');
  const [contextWindow, setContextWindow] = useState(128000);
  const [discovering, setDiscovering] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const account = accounts.find((candidate) => candidate.id === accountId);
  const existingWireModels = new Set(
    providers
      .filter((provider) => account?.model_ids.includes(provider.name))
      .map((provider) => provider.model),
  );
  const selectedSet = new Set(selected);
  const visible = (models ?? [])
    .filter((candidate) => {
      const query = search.trim().toLowerCase();
      return !query
        || candidate.id.toLowerCase().includes(query)
        || candidate.name?.toLowerCase().includes(query);
    })
    .slice(0, 200);

  const discover = async () => {
    if (!account?.base_url) {
      setError(t('settings.fetchNeedsBaseUrl'));
      return;
    }
    setDiscovering(true);
    setError(null);
    try {
      const found = await discoverProviderModels({
        type: account.type,
        base_url: account.base_url,
        provider_name: account.id,
      });
      setModels(found);
      setSelected([]);
      if (found.length === 0) setError(t('settings.fetchEmpty'));
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setDiscovering(false);
    }
  };

  const save = async () => {
    const manual = manualModel.trim();
    const wireModels = [...selected];
    if (manual && !wireModels.includes(manual)) wireModels.push(manual);
    if (wireModels.length === 0) {
      setError(t('settings.selectAtLeastOneModel'));
      return;
    }
    setSaving(true);
    setError(null);
    try {
      await createModelsForAccount(
        accountId,
        wireModels.map((wireModel) => {
          const discovered = models?.find((candidate) => candidate.id === wireModel);
          return {
            model: wireModel,
            display_name: discovered?.name,
            context_window: discovered?.context_window ?? contextWindow,
            max_tokens: discovered?.max_tokens,
          };
        }),
      );
      onSaved();
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  };

  return (
    <SettingsModal title={t('settings.addModel')} hideFooter onClose={onClose}>
      <div class="field-group add-model-form">
        <div class="add-model-field">
          <label class="add-model-label">{t('settings.providerAccount')}</label>
          <Select
            value={accountId}
            options={accounts.map((item) => ({
              value: item.id,
              label: item.display_name ? `${item.display_name} · ${item.id}` : item.id,
            }))}
            onChange={(value) => {
              setAccountId(value);
              setModels(null);
              setSelected([]);
              setManualModel('');
              setSearch('');
              setError(null);
            }}
          />
          {account && (
            <span class="field-hint">
              {account.base_url} · {account.has_api_key
                ? t('settings.reuseSavedApiKey')
                : t('settings.noSavedApiKey')}
            </span>
          )}
        </div>
        <div class="add-model-field">
          <div class="add-model-label-row">
            <label class="add-model-label">{t('settings.models')}</label>
            <button class="provider-action-btn" type="button" disabled={discovering} onClick={() => void discover()}>
              {discovering ? t('settings.fetchingModels') : t('settings.fetchModels')}
            </button>
          </div>
          {models && models.length > 0 && (
            <div class="model-discovery-picker model-discovery-picker-multi">
              <input
                class="menu-input"
                type="search"
                placeholder={t('settings.searchModels')}
                value={search}
                onInput={(e) => setSearch((e.target as HTMLInputElement).value)}
              />
              <div class="model-discovery-summary">
                {t('settings.selectedModels', { count: selected.length })}
              </div>
              <div class="model-discovery-results">
                {visible.map((candidate) => {
                  const checked = selectedSet.has(candidate.id);
                  const selectionId = `${accountId}/${candidate.id}`;
                  const exists = (account?.model_ids.includes(selectionId) ?? false)
                    || existingWireModels.has(candidate.id);
                  return (
                    <button
                      key={candidate.id}
                      class={'model-discovery-option' + (checked ? ' active' : '')}
                      type="button"
                      disabled={exists}
                      onClick={() => setSelected((current) => (
                        current.includes(candidate.id)
                          ? current.filter((id) => id !== candidate.id)
                          : [...current, candidate.id]
                      ))}
                    >
                      <span class="model-discovery-checkbox" aria-hidden="true">
                        {checked ? '✓' : ''}
                      </span>
                      <span>{candidate.name ?? candidate.id}</span>
                      {candidate.name && <code>{candidate.id}</code>}
                      {exists && <small>{t('settings.alreadyAdded')}</small>}
                    </button>
                  );
                })}
              </div>
            </div>
          )}
        </div>
        <div class="add-model-field">
          <label class="add-model-label">{t('settings.manualModel')}</label>
          <input
            class="menu-input"
            type="text"
            placeholder="deepseek-chat"
            value={manualModel}
            onInput={(e) => setManualModel((e.target as HTMLInputElement).value)}
          />
        </div>
        <div class="add-model-field">
          <label class="add-model-label">{t('settings.defaultContextWindow')}</label>
          <Select
            value={String(contextWindow)}
            options={CONTEXT_WINDOW_PRESETS.map((value) => ({
              value: String(value),
              label: `${fmtContextWindow(value)} tokens`,
            }))}
            onChange={(value) => setContextWindow(Number(value))}
          />
          <span class="field-hint">{t('settings.discoveredContextPreferred')}</span>
        </div>
        {error && <div class="modal-error">{t('settings.addFailed')}: {error}</div>}
        <div class="add-model-actions">
          <button class="btn" type="button" onClick={onClose}>{t('settings.close')}</button>
          <button class="btn btn-primary" type="button" disabled={saving} onClick={() => void save()}>
            {saving ? t('settings.adding') : t('settings.addSelectedModels')}
          </button>
        </div>
      </div>
    </SettingsModal>
  );
}

/**
 * 「添加 / 编辑模型」弹窗。
 * - 添加模式（无 editing）：name/model/base_url/api_key 均必填。
 * - 编辑模式（有 editing）：name 可改（后端按 key 迁移并修正默认项）；api_key 留空表示
 *   保留现有，仅在填写时才覆盖；走 PATCH。两种模式共用此表单避免重复。
 */
function ProviderFormDialog({
  editing,
  custom = false,
  presets = [],
  existingNames = [],
  onClose,
  onSaved,
}: {
  editing?: ProviderInfo;
  custom?: boolean;
  presets?: ProviderPresetInfo[];
  // 已有 provider 名称列表，用于重复名校验（编辑模式会排除自身原名）。
  existingNames?: string[];
  onClose: () => void;
  onSaved: () => void;
}) {
  const { t } = useSettings();
  const isEdit = !!editing;
  const initialPreset = !custom && !editing ? presets[0] : undefined;
  const [presetId, setPresetId] = useState(initialPreset?.id ?? '');
  const [name] = useState(editing?.name ?? '');
  const [nameInput, setNameInput] = useState(editing?.name ?? initialPreset?.display_name ?? '');
  const [type, setType] = useState(editing?.type ?? initialPreset?.type ?? 'openai');
  const [model, setModel] = useState(editing?.model ?? '');
  const [baseUrl, setBaseUrl] = useState(editing?.base_url ?? initialPreset?.default_base_url ?? '');
  const [apiKey, setApiKey] = useState('');
  const [contextWindow, setContextWindow] = useState<number>(editing?.context_window ?? 128000);
  const [setDefault, setSetDefault] = useState(editing?.is_default ?? false);
  const [saving, setSaving] = useState(false);
  const [discovering, setDiscovering] = useState(false);
  const [discovered, setDiscovered] = useState<DiscoveredModelInfo[] | null>(null);
  const [modelSearch, setModelSearch] = useState('');
  const [error, setError] = useState<string | null>(null);
  const selectedPreset = presets.find((preset) => preset.id === presetId);
  const requiresApiKey = type !== 'ollama' && selectedPreset?.requires_api_key !== false;
  const canDiscover = type === 'ollama'
    || ((type === 'openai' || type === 'openai-compat' || type === 'openai_compat')
      && (custom || isEdit || selectedPreset?.model_source === 'discovery_api'));

  // AtomGit 托管 provider 上下文窗口由平台固定，禁止用户改动。
  const isAtomGit = editing?.base_url === ATOMGIT_BASE_URL;
  // 当前值若非预设（如旧配置），并入选项首位，避免静默改写。
  const cwOptions = CONTEXT_WINDOW_PRESETS.includes(contextWindow)
    ? CONTEXT_WINDOW_PRESETS
    : [contextWindow, ...CONTEXT_WINDOW_PRESETS];

  const handleSave = async () => {
    const newName = nameInput.trim();
    if (isEdit) {
      // 编辑：name 必填且可改；api_key 留空=保留现有，故不计入必填。
      if (!newName || !model.trim() || !baseUrl.trim()) {
        setError(t('settings.allRequired'));
        return;
      }
    } else if (
      !newName ||
      !model.trim() ||
      !baseUrl.trim() ||
      (requiresApiKey && !apiKey.trim())
    ) {
      setError(t('settings.allRequired'));
      return;
    }
    // name 为主键，重复会静默覆盖/冲突，故提前拦截。编辑模式排除自身原名。
    if (
      existingNames.some(
        (n) => n.toLowerCase() !== name.toLowerCase() && n.toLowerCase() === newName.toLowerCase(),
      )
    ) {
      setError(t('settings.nameExists'));
      return;
    }
    setSaving(true);
    setError(null);
    try {
      if (isEdit) {
        await updateProvider(name, {
          // 仅在改名时才传 name，避免无谓的 key 迁移。
          ...(newName !== name ? { name: newName } : {}),
          type,
          model: model.trim(),
          base_url: baseUrl.trim(),
          // 仅在用户填写了新 key 时才覆盖；留空保留现有。
          ...(apiKey.trim() ? { api_key: apiKey.trim() } : {}),
          // AtomGit 上下文窗口由平台锁定，不下发该字段。
          ...(isAtomGit ? {} : { context_window: contextWindow }),
        });
        // PATCH 不处理默认项：若勾选且原本非默认，单独设默认（用新名，改名后旧 key 已不存在）。
        if (setDefault && !editing?.is_default) {
          await setDefaultProvider(newName);
        }
      } else {
        await createProvider({
          name: newName,
          type,
          model: model.trim(),
          base_url: baseUrl.trim(),
          ...(apiKey.trim() ? { api_key: apiKey.trim() } : {}),
          context_window: contextWindow,
          set_default: setDefault || undefined,
        });
      }
      onSaved();
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  };

  const handleDiscover = async () => {
    if (!baseUrl.trim()) {
      setError(t('settings.fetchNeedsBaseUrl'));
      return;
    }
    setDiscovering(true);
    setError(null);
    try {
      const found = await discoverProviderModels({
        type,
        base_url: baseUrl.trim(),
        ...(apiKey.trim() ? { api_key: apiKey.trim() } : {}),
        ...(isEdit ? { provider_name: name } : {}),
      });
      setDiscovered(found);
      setModelSearch('');
      if (found.length === 0) setError(t('settings.fetchEmpty'));
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setDiscovering(false);
    }
  };

  const visibleModels = (discovered ?? [])
    .filter((candidate) => {
      const query = modelSearch.trim().toLowerCase();
      return !query
        || candidate.id.toLowerCase().includes(query)
        || candidate.name?.toLowerCase().includes(query);
    })
    .slice(0, 100);

  return (
    <SettingsModal
      title={isEdit
        ? t('settings.editModel')
        : custom
          ? t('settings.addCustomProvider')
          : t('settings.addProvider')}
      hideFooter
      onClose={onClose}
    >
      <div class="field-group add-model-form">
        {!isEdit && !custom && (
          <div class="add-model-field">
            <label class="add-model-label">{t('settings.provider')}</label>
            <Select
              value={presetId}
              options={presets.map((preset) => ({ value: preset.id, label: preset.display_name }))}
              onChange={(value) => {
                const preset = presets.find((item) => item.id === value);
                setPresetId(value);
                if (!preset) return;
                setNameInput(preset.display_name);
                setType(preset.type);
                setBaseUrl(preset.default_base_url ?? '');
                setApiKey('');
                setDiscovered(null);
              }}
            />
          </div>
        )}
        <div class="add-model-field">
          <label class="add-model-label">{t('settings.providerName')}</label>
          <input
            class="menu-input"
            type="text"
            placeholder="my-deepseek"
            value={nameInput}
            onInput={(e) => setNameInput((e.target as HTMLInputElement).value)}
          />
        </div>
        <div class="add-model-field">
          <div class="add-model-label-row">
            <label class="add-model-label">{t('settings.model')}</label>
            {canDiscover && (
              <button
                class="provider-action-btn"
                type="button"
                disabled={discovering}
                onClick={() => void handleDiscover()}
              >
                {discovering ? t('settings.fetchingModels') : t('settings.fetchModels')}
              </button>
            )}
          </div>
          <input
            class="menu-input"
            type="text"
            placeholder="deepseek-chat"
            value={model}
            onInput={(e) => setModel((e.target as HTMLInputElement).value)}
          />
          {discovered && discovered.length > 0 && (
            <div class="model-discovery-picker">
              <input
                class="menu-input"
                type="search"
                placeholder={t('settings.searchModels')}
                value={modelSearch}
                onInput={(e) => setModelSearch((e.target as HTMLInputElement).value)}
              />
              <div class="model-discovery-results">
                {visibleModels.map((candidate) => (
                  <button
                    key={candidate.id}
                    class={'model-discovery-option' + (candidate.id === model ? ' active' : '')}
                    type="button"
                    onClick={() => {
                      setModel(candidate.id);
                      if (candidate.context_window) setContextWindow(candidate.context_window);
                      setDiscovered(null);
                    }}
                  >
                    <span>{candidate.name ?? candidate.id}</span>
                    {candidate.name && <code>{candidate.id}</code>}
                  </button>
                ))}
                {visibleModels.length === 0 && (
                  <span class="field-hint">{t('settings.noMatchingModels')}</span>
                )}
              </div>
            </div>
          )}
        </div>
        <div class="add-model-row">
          <div class="add-model-field add-model-field-type">
            <label class="add-model-label">{t('settings.providerType')}</label>
            <Select
              value={type}
              disabled={!custom && !isEdit}
              options={[
                { value: 'openai', label: 'openai' },
                { value: 'anthropic', label: 'anthropic' },
                { value: 'ollama', label: 'ollama' },
              ]}
              onChange={(v) => {
                setType(v);
                setDiscovered(null);
              }}
            />
          </div>
          <div class="add-model-field add-model-field-default">
            <label class="add-model-checkbox-label">
              <input
                type="checkbox"
                checked={setDefault}
                disabled={editing?.is_default}
                onChange={(e) => setSetDefault((e.target as HTMLInputElement).checked)}
              />
              {t('settings.setAsDefault')}
            </label>
          </div>
        </div>
        <div class="add-model-field">
          <label class="add-model-label">{t('settings.contextWindow')}</label>
          <Select
            value={String(contextWindow)}
            disabled={isAtomGit}
            options={cwOptions.map((v) => ({
              value: String(v),
              label: `${fmtContextWindow(v)} tokens`,
            }))}
            onChange={(v) => setContextWindow(Number(v))}
          />
          {isAtomGit && (
            <span class="field-hint">{t('settings.contextWindowLocked')}</span>
          )}
        </div>
        <div class="add-model-field">
          <label class="add-model-label">{t('settings.baseUrl')}</label>
          <input
            class="menu-input"
            type="text"
            placeholder="https://api.example.com/v1"
            value={baseUrl}
            onInput={(e) => {
              setBaseUrl((e.target as HTMLInputElement).value);
              setDiscovered(null);
            }}
          />
        </div>
        {(requiresApiKey || (isEdit && type !== 'ollama')) && (
          <div class="add-model-field">
            <label class="add-model-label">{t('settings.apiKeyInput')}</label>
            <input
              class="menu-input"
              type="password"
              placeholder={isEdit ? t('settings.apiKeyKeep') : 'sk-...'}
              value={apiKey}
              onInput={(e) => {
                setApiKey((e.target as HTMLInputElement).value);
                setDiscovered(null);
              }}
            />
          </div>
        )}
        {error && (
          <div class="modal-error">
            {(isEdit ? t('settings.updateFailed') : t('settings.addFailed'))}: {error}
          </div>
        )}
        <div class="add-model-actions">
          <button class="btn" type="button" onClick={onClose}>
            {t('settings.close')}
          </button>
          <button class="btn btn-primary" type="button" disabled={saving} onClick={handleSave}>
            {isEdit
              ? (saving ? t('settings.saving') : t('settings.save'))
              : (saving ? t('settings.adding') : t('settings.add'))}
          </button>
        </div>
      </div>
    </SettingsModal>
  );
}

/** 远程访问（蒲公英 / Oray PGY）：检测状态，给出可扫码的私网 URL。 */
export function RemoteAccessDialog({ onClose }: { onClose: () => void }) {
  const { t, lang } = useSettings();
  const [status, setStatus] = useState<TunnelStatus | null>(null);
  const [loading, setLoading] = useState(true);
  const [copied, setCopied] = useState(false);

  const reload = () => {
    setLoading(true);
    getTunnelStatus()
      .then(setStatus)
      .catch(() => setStatus(null))
      .finally(() => setLoading(false));
  };
  useEffect(() => { reload(); }, []);

  const pgy = status?.pgy;
  // 服务端未给 remote_url（绑回环）时，展示一个「示意」地址。注意：token 现在只存在
  // 于 HttpOnly Cookie 中，前端 JS 读不到（防插件拦截，CWE-598），所以这里无法拼出
  // 可直接登录的链接——要可分享的真实链接需把 webui 绑到局域网，由服务端下发
  // remote_url（带 token）。回环示意地址因此不带 token。
  const fallbackUrl =
    pgy?.ipv4 && status
      ? `http://${pgy.ipv4}:${status.port}/?sync=1`
      : null;

  function copy() {
    const url = status?.remote_url ?? fallbackUrl;
    if (!url) return;
    navigator.clipboard?.writeText(url).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    });
  }

  return (
    <SettingsModal title={t('remote.title')} onClose={onClose}>
      <div class="field-group remote-access">
        <p class="field-hint">{t('remote.intro')}</p>

        {loading && <div class="modal-loading">{t('remote.loading')}</div>}

        {!loading && status && (
          <>
            {/* 1) 未装 / 未连蒲公英 */}
            {(!pgy?.installed || !pgy?.ipv4) && (
              <div class="remote-state">
                <p>{pgy?.installed ? t('remote.notConnected') : t('remote.notInstalled')}</p>
                <a
                  class="btn btn-primary"
                  href="https://pgy.oray.com"
                  target="_blank"
                  rel="noreferrer"
                >
                  {t('remote.installLink')}
                </a>
              </div>
            )}

            {/* 2) 已装+有 IP，但 server 仅绑回环 → 提示改绑 */}
            {pgy?.installed && pgy?.ipv4 && !status.remote_url && (
              <div class="remote-state">
                <p>{t('remote.notReachable', { ip: pgy.ipv4 })}</p>
                {fallbackUrl && <code class="remote-url">{fallbackUrl}</code>}
              </div>
            )}

            {/* 3) 就绪：二维码 + URL */}
            {status.remote_url && (
              <div class="remote-state remote-ready">
                <p>{t('remote.ready')}</p>
                {status.qr_svg && (
                  <div
                    class="remote-qr"
                    // eslint-disable-next-line react/no-danger
                    dangerouslySetInnerHTML={{ __html: status.qr_svg }}
                  />
                )}
                <code class="remote-url">{status.remote_url}</code>
                <div class="remote-actions">
                  <button class="btn" onClick={copy}>
                    {copied ? t('remote.copied') : t('remote.copy')}
                  </button>
                </div>
                <p class="field-hint remote-warn">⚠️ {t('remote.warnToken')}</p>
              </div>
            )}
          </>
        )}

        <div class="remote-actions">
          <button class="btn" onClick={reload} disabled={loading}>
            {t('remote.refresh')}
          </button>
          {/* 使用引导：跳官网对应语言的说明页，新标签打开。 */}
          <a
            class="btn"
            href={`https://atomcode.atomgit.com/docs/${lang}/webui-remote-access.html`}
            target="_blank"
            rel="noreferrer"
          >
            {t('remote.guide')}
          </a>
        </div>
      </div>
    </SettingsModal>
  );
}
