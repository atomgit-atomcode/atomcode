// Individual settings dialogs: theme, language, and (read-only) model config.
// Each is opened on its own from the sidebar settings menu.

import { ComponentChildren } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import { getConfig, ConfigInfo } from '../api';
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
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    getConfig()
      .then(setConfig)
      .catch((e: unknown) => setError(e instanceof Error ? e.message : String(e)));
  }, []);

  return (
    <SettingsModal title={t('settings.menuModel')} wide onClose={onClose}>
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
                <div key={p.name} class={'provider-card' + (p.is_default ? ' default' : '')}>
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
                        {p.has_api_key ? t('settings.configured') : t('settings.notConfigured')}
                      </span>
                    </div>
                  </div>
                </div>
              ))}
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
