// Individual settings dialogs: theme, language, and (read-only) model config.
// Each is opened on its own from the sidebar settings menu.

import { ComponentChildren } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import {
  getConfig,
  ConfigInfo,
  ProviderInfo,
  createProvider,
  updateProvider,
  setDefaultProvider,
  deleteProvider,
  getTunnelStatus,
  TunnelStatus,
} from '../api';
import { useSettings, Theme } from '../settings';
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

/** Shared modal chrome for the settings dialogs. */
function SettingsModal({
  title,
  wide,
  hideFooter,
  onClose,
  children,
}: {
  title: string;
  wide?: boolean;
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
      <div class={'modal-card' + (wide ? '' : ' modal-card-sm')}>
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
  const { theme, setTheme, t } = useSettings();
  const options: { value: Theme; label: string }[] = [
    { value: 'light', label: t('settings.theme.light') },
    { value: 'dark', label: t('settings.theme.dark') },
    { value: 'system', label: t('settings.theme.system') },
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

  // 用户手势内请求权限：开启开关时若尚未授权，先请求再落库。
  async function toggleEnabled() {
    if (!prefs.enabled && permission !== 'granted') {
      // requestPermission 返回 'granted' | 'denied' | 'default'（忽略弹窗）。
      // 不能用布尔近似：忽略弹窗时是 'default'，误标 denied 会误导用户。
      const result = await requestNotificationPermission();
      const actual: NotificationPermission =
        typeof Notification !== 'undefined' ? Notification.permission : result ? 'granted' : 'denied';
      setPermission(actual);
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
      </div>
    </SettingsModal>
  );
}

export function ModelConfigDialog({ onClose }: { onClose: () => void }) {
  const { t } = useSettings();
  const [config, setConfig] = useState<ConfigInfo | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);

  // 添加模型改为独立弹窗
  const [showAdd, setShowAdd] = useState(false);
  // 编辑已有 provider：选中的条目（null=未编辑）。
  const [editTarget, setEditTarget] = useState<ProviderInfo | null>(null);
  // 删除确认改用 webui 弹窗（ConfirmDialog），不再用系统 confirm/alert。
  const [deleteTarget, setDeleteTarget] = useState<string | null>(null);

  const reload = () =>
    getConfig()
      .then(setConfig)
      .catch((e: unknown) => setLoadError(e instanceof Error ? e.message : String(e)));

  useEffect(() => { reload(); }, []);

  return (
    <>
    <SettingsModal title={t('settings.menuModel')} wide onClose={onClose}>
      <div class="field-group">
        <span class="modal-label">{t('settings.modelConfig')}</span>
        {loadError && <div class="modal-error">{t('settings.loadFailed')}: {loadError}</div>}
        {!config && !loadError && <div class="modal-loading">{t('settings.loading')}</div>}
        {config && (
          <>
            <div class="add-model-top">
              <button
                class="btn btn-primary"
                type="button"
                onClick={() => setShowAdd(true)}
              >
                ＋ {t('settings.addModel')}
              </button>
            </div>
            <Row label={t('settings.defaultProvider')} value={config.default_provider} />
            {config.default_workdir && (
              <Row label={t('settings.defaultWorkdir')} value={config.default_workdir} mono />
            )}
            <Row label={t('settings.configFile')} value={config.path} mono />

            <div class="provider-list">
              <span class="modal-label">
                {t('settings.providers')} ({config.providers.length})
              </span>
              {[...config.providers].sort((a, b) => {
                if (a.is_default !== b.is_default) return a.is_default ? -1 : 1;
                return a.name.localeCompare(b.name);
              }).map((p) => (
                <div key={p.name} class={'provider-card' + (p.is_default ? ' default' : '')}>
                  <div class="provider-card-head">
                    <span class="provider-name">{p.name}</span>
                    {p.is_default && (
                      <span class="provider-default-badge">{t('settings.default')}</span>
                    )}
                    <span class="provider-type">{p.type}</span>
                    {/* AtomGit 托管 provider 由平台固定，禁止编辑（仅保留删除）。 */}
                    {p.base_url !== ATOMGIT_BASE_URL && (
                      <button
                        class="provider-edit-btn"
                        type="button"
                        onClick={() => setEditTarget(p)}
                        title={t('settings.edit')}
                      >
                        {t('settings.edit')}
                      </button>
                    )}
                    <button
                      class="provider-delete-btn"
                      type="button"
                      onClick={() => setDeleteTarget(p.name)}
                      title={t('settings.delete')}
                    >
                      {t('settings.delete')}
                    </button>
                  </div>
                  <div class="provider-card-body">
                    <div>
                      <span class="pk">{t('settings.model')}: </span>
                      <span class="pv">{p.model}</span>
                    </div>
                    {p.base_url && (
                      <div>
                        <span class="pk">base_url: </span>
                        <span class="pv">{p.base_url}</span>
                      </div>
                    )}
                    {p.context_window && (
                      <div>
                        <span class="pk">{t('settings.contextWindow')}: </span>
                        <span>{(p.context_window / 1000).toFixed(0)}k tokens</span>
                      </div>
                    )}
                    {p.base_url !== ATOMGIT_BASE_URL && (
                      <div>
                        <span class="pk">{t('settings.apiKey')}: </span>
                        <span class={p.has_api_key ? 'ok' : 'nok'}>
                          {p.has_api_key ? t('settings.configured') : t('settings.notConfigured')}
                        </span>
                      </div>
                    )}
                  </div>
                </div>
              ))}
            </div>
          </>
        )}
      </div>
    </SettingsModal>
    {showAdd && (
      <ProviderFormDialog
        existingNames={config?.providers.map((p) => p.name) ?? []}
        onClose={() => setShowAdd(false)}
        onSaved={() => {
          setShowAdd(false);
          reload();
        }}
      />
    )}
    {editTarget && (
      <ProviderFormDialog
        editing={editTarget}
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

/**
 * 「添加 / 编辑模型」弹窗。
 * - 添加模式（无 editing）：name/model/base_url/api_key 均必填。
 * - 编辑模式（有 editing）：name 可改（后端按 key 迁移并修正默认项）；api_key 留空表示
 *   保留现有，仅在填写时才覆盖；走 PATCH。两种模式共用此表单避免重复。
 */
function ProviderFormDialog({
  editing,
  existingNames = [],
  onClose,
  onSaved,
}: {
  editing?: ProviderInfo;
  // 已有 provider 名称列表，用于重复名校验（编辑模式会排除自身原名）。
  existingNames?: string[];
  onClose: () => void;
  onSaved: () => void;
}) {
  const { t } = useSettings();
  const isEdit = !!editing;
  const [name] = useState(editing?.name ?? '');
  const [nameInput, setNameInput] = useState(editing?.name ?? '');
  const [type, setType] = useState(editing?.type ?? 'openai');
  const [model, setModel] = useState(editing?.model ?? '');
  const [baseUrl, setBaseUrl] = useState(editing?.base_url ?? '');
  const [apiKey, setApiKey] = useState('');
  const [contextWindow, setContextWindow] = useState<number>(editing?.context_window ?? 128000);
  const [setDefault, setSetDefault] = useState(editing?.is_default ?? false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

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
    } else if (!newName || !model.trim() || !baseUrl.trim() || !apiKey.trim()) {
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
          api_key: apiKey.trim(),
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

  return (
    <SettingsModal
      title={isEdit ? t('settings.editModel') : t('settings.addModel')}
      hideFooter
      onClose={onClose}
    >
      <div class="field-group add-model-form">
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
          <label class="add-model-label">{t('settings.model')}</label>
          <input
            class="menu-input"
            type="text"
            placeholder="deepseek-chat"
            value={model}
            onInput={(e) => setModel((e.target as HTMLInputElement).value)}
          />
        </div>
        <div class="add-model-row">
          <div class="add-model-field add-model-field-type">
            <label class="add-model-label">{t('settings.providerType')}</label>
            <Select
              value={type}
              options={[
                { value: 'openai', label: 'openai' },
                { value: 'claude', label: 'claude' },
                { value: 'ollama', label: 'ollama' },
              ]}
              onChange={(v) => setType(v)}
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
            onInput={(e) => setBaseUrl((e.target as HTMLInputElement).value)}
          />
        </div>
        <div class="add-model-field">
          <label class="add-model-label">{t('settings.apiKeyInput')}</label>
          <input
            class="menu-input"
            type="password"
            placeholder={isEdit ? t('settings.apiKeyKeep') : 'sk-...'}
            value={apiKey}
            onInput={(e) => setApiKey((e.target as HTMLInputElement).value)}
          />
        </div>
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

function Row({ label, value, mono }: { label: string; value: string; mono?: boolean }) {
  return (
    <div class="config-row">
      <span class="config-key">{label}</span>
      <span class={'config-val' + (mono ? ' mono' : '')}>{value}</span>
    </div>
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
