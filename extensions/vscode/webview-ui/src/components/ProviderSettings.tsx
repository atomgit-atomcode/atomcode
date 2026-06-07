import React, { useState } from 'react';
import { useChatContext } from '../state/ChatProvider';
import { postMessage } from '../vscode';

export function ProviderSettings() {
  const { state, dispatch, setDefaultProvider, refreshSetupState } = useChatContext();
  const [providerName, setProviderName] = useState('');
  const [providerType, setProviderType] = useState('openai');
  const [model, setModel] = useState('');
  const [baseUrl, setBaseUrl] = useState('');
  const [apiKey, setApiKey] = useState('');

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
            <h3>AtomCode Settings</h3>
            <p>{state.auth?.logged_in ? `Signed in as ${state.auth.user?.username}` : 'Not signed in'}</p>
          </div>
          <button className="ghost-btn" onClick={() => dispatch({ type: 'TOGGLE_SETTINGS' })}>×</button>
        </div>

        <section className="settings-section">
          <div className="settings-section-title">Providers</div>
          {state.providers.length === 0 && <div className="settings-empty">No providers configured.</div>}
          {state.providers.map((p) => (
            <div className="provider-row" key={p.name}>
              <div className="provider-row-main">
                <span>{p.model}</span>
                <small>{p.name} · {p.type}{p.has_api_key ? ' · key set' : ''}</small>
              </div>
              {p.is_default ? (
                <span className="model-default-badge">default</span>
              ) : (
                <button className="setup-secondary" onClick={() => setDefaultProvider(p.name)}>Use</button>
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
                {p.thinking_enabled ? 'Think on' : 'Think off'}
              </button>
              <button className="setup-secondary" onClick={() => postMessage({ type: 'providerDelete', name: p.name })}>
                Delete
              </button>
            </div>
          ))}
          <button className="setup-secondary setup-wide" onClick={refreshSetupState}>Refresh</button>
        </section>

        <section className="settings-section">
          <div className="settings-section-title">Add Provider</div>
          <form className="provider-form" onSubmit={submitProvider}>
            <input value={providerName} onChange={(e) => setProviderName(e.target.value)} placeholder="Provider name" required />
            <input value={providerType} onChange={(e) => setProviderType(e.target.value)} placeholder="Type, e.g. openai" required />
            <input value={model} onChange={(e) => setModel(e.target.value)} placeholder="Model" required />
            <input value={baseUrl} onChange={(e) => setBaseUrl(e.target.value)} placeholder="Base URL" />
            <input value={apiKey} onChange={(e) => setApiKey(e.target.value)} placeholder="API key" type="password" />
            <button className="setup-primary" type="submit">Save provider</button>
          </form>
        </section>

        {state.setupError && <div className="setup-error">{state.setupError}</div>}
      </aside>
    </div>
  );
}
