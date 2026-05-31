// Individual settings dialogs: theme, language, and (read-only) model config.
// Each is opened on its own from the sidebar settings menu.

import { ComponentChildren } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import { getConfig, ConfigInfo, createProvider, deleteProvider } from '../api';
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

  // Add-model form state
  const [formName, setFormName] = useState('');
  const [formType, setFormType] = useState('openai');
  const [formModel, setFormModel] = useState('');
  const [formBaseUrl, setFormBaseUrl] = useState('');
  const [formApiKey, setFormApiKey] = useState('');
  const [formSetDefault, setFormSetDefault] = useState(false);
  const [adding, setAdding] = useState(false);
  const [addError, setAddError] = useState<string | null>(null);

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

  const handleAdd = async () => {
    if (!formName.trim() || !formModel.trim()) {
      setAddError(t('settings.nameModelRequired'));
      return;
    }
    setAdding(true);
    setAddError(null);
    try {
      await createProvider({
        name: formName.trim(),
        type: formType,
        model: formModel.trim(),
        base_url: formBaseUrl.trim() || undefined,
        api_key: formApiKey.trim() || undefined,
        set_default: formSetDefault || undefined,
      });
      setFormName('');
      setFormType('openai');
      setFormModel('');
      setFormBaseUrl('');
      setFormApiKey('');
      setFormSetDefault(false);
      reload();
    } catch (e: unknown) {
      setAddError((e instanceof Error ? e.message : String(e)));
    } finally {
      setAdding(false);
    }
  };

  return (
    <SettingsModal title={t('settings.menuModel')} wide onClose={onClose}>
      <div class="field-group">
        <span class="modal-label">{t('settings.modelConfig')}</span>
        {loadError && <div class="modal-error">{t('settings.loadFailed')}: {loadError}</div>}
        {!config && !loadError && <div class="modal-loading">{t('settings.loading')}</div>}
        {config && (
          <>
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
                    <div>
                      <span class="pk">{t('settings.apiKey')}: </span>
                      <span class={p.has_api_key ? 'ok' : 'nok'}>
                        {p.has_api_key ? t('settings.configured') : t('settings.notConfigured')}
                      </span>
                    </div>
                  </div>
                </div>
              ))}
            </div>

            {/* Add model form */}
            <div class="add-model-form">
              <span class="modal-label">{t('settings.addModel')}</span>
              <div class="add-model-field">
                <label class="add-model-label">{t('settings.providerName')}</label>
                <input
                  class="menu-input"
                  type="text"
                  placeholder="my-deepseek"
                  value={formName}
                  onInput={(e) => setFormName((e.target as HTMLInputElement).value)}
                />
              </div>
              <div class="add-model-field">
                <label class="add-model-label">{t('settings.model')}</label>
                <input
                  class="menu-input"
                  type="text"
                  placeholder="deepseek-chat"
                  value={formModel}
                  onInput={(e) => setFormModel((e.target as HTMLInputElement).value)}
                />
              </div>
              <div class="add-model-row">
                <div class="add-model-field add-model-field-type">
                  <label class="add-model-label">{t('settings.providerType')}</label>
                  <select
                    class="menu-input"
                    value={formType}
                    onChange={(e) => setFormType((e.target as HTMLSelectElement).value)}
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
                      checked={formSetDefault}
                      onChange={(e) => setFormSetDefault((e.target as HTMLInputElement).checked)}
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
                  value={formBaseUrl}
                  onInput={(e) => setFormBaseUrl((e.target as HTMLInputElement).value)}
                />
              </div>
              <div class="add-model-field">
                <label class="add-model-label">{t('settings.apiKeyInput')}</label>
                <input
                  class="menu-input"
                  type="password"
                  placeholder="sk-..."
                  value={formApiKey}
                  onInput={(e) => setFormApiKey((e.target as HTMLInputElement).value)}
                />
              </div>
              {addError && <div class="modal-error">{t('settings.addFailed')}: {addError}</div>}
              <div class="add-model-actions">
                <button
                  class="btn btn-primary"
                  type="button"
                  disabled={adding}
                  onClick={handleAdd}
                >
                  {adding ? t('settings.adding') : t('settings.add')}
                </button>
              </div>
            </div>
          </>
        )}
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
