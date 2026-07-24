import React, { useState } from 'react';
import { useChatContext } from '../state/ChatProvider';
import { postMessage } from '../vscode';
import { useT } from '../i18n';

export function ProviderSettings() {
  const { state, dispatch, setDefaultProvider, refreshSetupState } = useChatContext();
  const t = useT();
  const [providerName, setProviderName] = useState('');
  const [providerType, setProviderType] = useState('openai');
  const [model, setModel] = useState('');
  const [baseUrl, setBaseUrl] = useState('');
  const [apiKey, setApiKey] = useState('');
  const authUsable = state.auth?.logged_in === true && state.auth.expired !== true;

  function submitProvider(e: React.FormEvent) {
    e.preventDefault();
    postMessage({
      type: 'providerCreate',
      provider: {
        name: providerName,
        type: providerType,
        model,
        base_url: baseUrl || undefined,
        api_key: apiKey || undefined,
        set_default: true,
      },
    });
    // Reset all form fields
    setProviderName('');
    setProviderType('openai');
    setModel('');
    setBaseUrl('');
    setApiKey('');
  }

  return (
    <div className="settings-overlay" onClick={() => dispatch({ type: 'TOGGLE_SETTINGS' })}>
      <aside className="settings-panel" onClick={(e) => e.stopPropagation()}>
        <div className="settings-header">
          <div>
            <h3>{t('provider.settingsTitle')}</h3>
            <p>{authUsable ? t('setup.signedInAs', { name: state.auth?.user?.username || t('setup.atomgitUser') }) : t('provider.notSignedIn')}</p>
          </div>
          <button className="ghost-btn" onClick={() => dispatch({ type: 'TOGGLE_SETTINGS' })}>×</button>
        </div>

        <section className="settings-section">
          <div className="settings-section-title">{t('provider.providers')}</div>
          {state.providers.length === 0 && <div className="settings-empty">{t('provider.noneConfigured')}</div>}
          {state.providers.map((p) => (
            <div className="provider-row" key={p.name}>
              <div className="provider-row-main">
                <span>{p.model}</span>
                <small>{p.name} · {p.type}{p.has_api_key ? ` · ${t('provider.keySet')}` : ''}</small>
              </div>
              {p.is_default ? (
                <span className="model-default-badge">{t('model.defaultBadge')}</span>
              ) : (
                <button className="setup-secondary" onClick={() => setDefaultProvider(p.name)}>{t('provider.use')}</button>
              )}
              <button
                className="setup-secondary"
                onClick={() => postMessage({
                  type: 'providerPatchThinking',
                  name: p.name,
                  thinking: {
                    enabled: !p.thinking_enabled,
                    budget: p.thinking_budget || 10000,
                  },
                })}
              >
                {p.thinking_enabled ? t('provider.thinkOn') : t('provider.thinkOff')}
              </button>
              <button className="setup-secondary" onClick={() => postMessage({ type: 'providerDelete', name: p.name })}>
                {t('provider.delete')}
              </button>
            </div>
          ))}
          <button className="setup-secondary setup-wide" onClick={refreshSetupState}>{t('provider.refresh')}</button>
        </section>

        <section className="settings-section">
          <div className="settings-section-title">{t('provider.addProvider')}</div>
          <form className="provider-form" onSubmit={submitProvider}>
            <input value={providerName} onChange={(e) => setProviderName(e.target.value)} placeholder={t('setup.providerName')} required />
            <input value={providerType} onChange={(e) => setProviderType(e.target.value)} placeholder={t('provider.typePlaceholder')} required />
            <input value={model} onChange={(e) => setModel(e.target.value)} placeholder={t('setup.model')} required />
            <input value={baseUrl} onChange={(e) => setBaseUrl(e.target.value)} placeholder={t('setup.baseUrl')} />
            <input value={apiKey} onChange={(e) => setApiKey(e.target.value)} placeholder={t('setup.apiKey')} type="password" />
            <button className="setup-primary" type="submit">{t('provider.saveProvider')}</button>
          </form>
        </section>

        {state.setupError && <div className="setup-error">{state.setupError}</div>}
      </aside>
    </div>
  );
}
