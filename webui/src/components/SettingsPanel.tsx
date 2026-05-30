// Settings modal: theme (light/dark/system), language (zh/en), and the
// (read-only) model/provider configuration.

import { useEffect, useState } from 'preact/hooks';
import { getConfig, ConfigInfo } from '../api';
import { useSettings, Theme } from '../settings';
import { Lang } from '../i18n';

interface SettingsPanelProps {
  onClose: () => void;
}

export function SettingsPanel({ onClose }: SettingsPanelProps) {
  const { theme, setTheme, lang, setLang, t } = useSettings();
  const [config, setConfig] = useState<ConfigInfo | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    getConfig()
      .then(setConfig)
      .catch((e: unknown) =>
        setError(e instanceof Error ? e.message : String(e)),
      );
  }, []);

  const themeOptions: { value: Theme; label: string }[] = [
    { value: 'light', label: t('settings.theme.light') },
    { value: 'dark', label: t('settings.theme.dark') },
    { value: 'system', label: t('settings.theme.system') },
  ];

  const langOptions: { value: Lang; label: string }[] = [
    { value: 'zh', label: '中文' },
    { value: 'en', label: 'English' },
  ];

  return (
    <div
      class="modal-overlay"
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div class="modal-card">
        <div class="modal-header">
          <span>⚙</span>
          <h3>{t('settings.title')}</h3>
          <button class="ghost-btn modal-close" onClick={onClose} aria-label={t('settings.close')}>
            ×
          </button>
        </div>

        <div class="modal-body">
          {/* Theme */}
          <div class="field-group">
            <span class="modal-label">{t('settings.theme')}</span>
            <div class="segmented">
              {themeOptions.map((o) => (
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

          {/* Language */}
          <div class="field-group">
            <span class="modal-label">{t('settings.language')}</span>
            <div class="segmented">
              {langOptions.map((o) => (
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

          {/* Model configuration (read-only) */}
          <div class="field-group">
            <span class="modal-label">
              {t('settings.modelConfig')} <span class="modal-sub">{t('common.readonly')}</span>
            </span>
            {error && <div class="modal-error">{t('settings.loadFailed')}: {error}</div>}
            {!config && !error && <div class="modal-loading">{t('settings.loading')}</div>}
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
                    <div
                      key={p.name}
                      class={'provider-card' + (p.is_default ? ' default' : '')}
                    >
                      <div class="provider-card-head">
                        <span class="provider-name">{p.name}</span>
                        {p.is_default && (
                          <span class="provider-default-badge">{t('settings.default')}</span>
                        )}
                        <span class="provider-type">{p.type}</span>
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
                            {p.has_api_key
                              ? t('settings.configured')
                              : t('settings.notConfigured')}
                          </span>
                        </div>
                      </div>
                    </div>
                  ))}
                </div>
              </>
            )}
          </div>
        </div>

        <div class="modal-footer">
          <button class="btn" onClick={onClose}>
            {t('settings.close')}
          </button>
        </div>
      </div>
    </div>
  );
}

function Row({
  label,
  value,
  mono,
}: {
  label: string;
  value: string;
  mono?: boolean;
}) {
  return (
    <div class="config-row">
      <span class="config-key">{label}</span>
      <span class={'config-val' + (mono ? ' mono' : '')}>{value}</span>
    </div>
  );
}
