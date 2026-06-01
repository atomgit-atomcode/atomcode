// Individual settings dialogs: theme, language, and (read-only) model config.
// Each is opened on its own from the sidebar settings menu.

import { ComponentChildren } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import {
  getConfig,
  ConfigInfo,
  createProvider,
  deleteProvider,
  getTunnelStatus,
  TunnelStatus,
  getToken,
} from '../api';
import { useSettings, Theme } from '../settings';
import { Lang } from '../i18n';

/** Shared modal chrome for the settings dialogs. */
function SettingsModal({
  title,
  wide,
  onClose,
  children,
}: {
  title: string;
  wide?: boolean;
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
        <div class="modal-footer">
          <button class="btn" onClick={onClose}>
            {t('settings.close')}
          </button>
        </div>
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

export function ModelConfigDialog({ onClose }: { onClose: () => void }) {
  const { t } = useSettings();
  const [config, setConfig] = useState<ConfigInfo | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);

  // 添加模型改为独立弹窗
  const [showAdd, setShowAdd] = useState(false);

  const reload = () =>
    getConfig()
      .then(setConfig)
      .catch((e: unknown) => setLoadError(e instanceof Error ? e.message : String(e)));

  useEffect(() => { reload(); }, []);

  const handleDelete = async (name: string) => {
    if (!window.confirm(t('settings.deleteConfirm').replace('{name}', name))) return;
    try {
      await deleteProvider(name);
      reload();
    } catch (e: unknown) {
      alert(e instanceof Error ? e.message : String(e));
    }
  };

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
              {config.providers.map((p) => (
                <div key={p.name} class={'provider-card' + (p.is_default ? ' default' : '')}>
                  <div class="provider-card-head">
                    <span class="provider-name">{p.name}</span>
                    {p.is_default && (
                      <span class="provider-default-badge">{t('settings.default')}</span>
                    )}
                    <span class="provider-type">{p.type}</span>
                    <button
                      class="provider-delete-btn"
                      type="button"
                      onClick={() => handleDelete(p.name)}
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
                    {p.base_url !== 'https://llm-api.atomgit.com/v1' && (
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
      <AddModelDialog
        onClose={() => setShowAdd(false)}
        onAdded={() => {
          setShowAdd(false);
          reload();
        }}
      />
    )}
    </>
  );
}

/** 独立「添加模型」弹窗。base_url 与 api key 为必填。 */
function AddModelDialog({
  onClose,
  onAdded,
}: {
  onClose: () => void;
  onAdded: () => void;
}) {
  const { t } = useSettings();
  const [name, setName] = useState('');
  const [type, setType] = useState('openai');
  const [model, setModel] = useState('');
  const [baseUrl, setBaseUrl] = useState('');
  const [apiKey, setApiKey] = useState('');
  const [setDefault, setSetDefault] = useState(false);
  const [adding, setAdding] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const handleAdd = async () => {
    if (!name.trim() || !model.trim() || !baseUrl.trim() || !apiKey.trim()) {
      setError(t('settings.allRequired'));
      return;
    }
    setAdding(true);
    setError(null);
    try {
      await createProvider({
        name: name.trim(),
        type,
        model: model.trim(),
        base_url: baseUrl.trim(),
        api_key: apiKey.trim(),
        set_default: setDefault || undefined,
      });
      onAdded();
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setAdding(false);
    }
  };

  return (
    <SettingsModal title={t('settings.addModel')} onClose={onClose}>
      <div class="field-group add-model-form">
        <div class="add-model-field">
          <label class="add-model-label">{t('settings.providerName')}</label>
          <input
            class="menu-input"
            type="text"
            placeholder="my-deepseek"
            value={name}
            onInput={(e) => setName((e.target as HTMLInputElement).value)}
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
            <select
              class="menu-input"
              value={type}
              onChange={(e) => setType((e.target as HTMLSelectElement).value)}
            >
              <option value="openai">openai</option>
              <option value="claude">claude</option>
              <option value="ollama">ollama</option>
            </select>
          </div>
          <div class="add-model-field add-model-field-default">
            <label class="add-model-checkbox-label">
              <input
                type="checkbox"
                checked={setDefault}
                onChange={(e) => setSetDefault((e.target as HTMLInputElement).checked)}
              />
              {t('settings.setAsDefault')}
            </label>
          </div>
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
            placeholder="sk-..."
            value={apiKey}
            onInput={(e) => setApiKey((e.target as HTMLInputElement).value)}
          />
        </div>
        {error && <div class="modal-error">{t('settings.addFailed')}: {error}</div>}
        <div class="add-model-actions">
          <button class="btn" type="button" onClick={onClose}>
            {t('settings.close')}
          </button>
          <button class="btn btn-primary" type="button" disabled={adding} onClick={handleAdd}>
            {adding ? t('settings.adding') : t('settings.add')}
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
  const { t } = useSettings();
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
  // 服务端未给 remote_url（绑回环）时，前端用本地 token 拼一个「示意」地址。
  const fallbackUrl =
    pgy?.ipv4 && status
      ? `http://${pgy.ipv4}:${status.port}/?token=${getToken()}&sync=1`
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
        </div>
      </div>
    </SettingsModal>
  );
}
