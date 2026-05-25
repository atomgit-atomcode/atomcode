import React, { useState } from 'react';
import { postMessage } from '../vscode';
import { useChatContext } from '../state/ChatProvider';

interface QuickAction {
  id: string;
  label: string;
  icon: string;
}

const quickActions: QuickAction[] = [
  { id: 'explain', label: 'Explain Code', icon: '💡' },
  { id: 'fix', label: 'Fix Issues', icon: '🔧' },
  { id: 'test', label: 'Write Tests', icon: '🧪' },
  { id: 'refactor', label: 'Refactor', icon: '♻️' },
  { id: 'docs', label: 'Add Docs', icon: '📝' },
  { id: 'review', label: 'Code Review', icon: '🔍' },
];

export function WelcomeScreen() {
  const { state } = useChatContext();
  const [manualOpen, setManualOpen] = useState(false);
  const [providerName, setProviderName] = useState('openai');
  const [providerType, setProviderType] = useState('openai');
  const [model, setModel] = useState('gpt-4o');
  const [baseUrl, setBaseUrl] = useState('https://api.openai.com/v1');
  const [apiKey, setApiKey] = useState('');

  function handleAction(action: string) {
    postMessage({ type: 'quickAction', action });
  }

  function startLogin() {
    postMessage({ type: 'authLoginStart' });
  }

  function cancelLogin() {
    postMessage({ type: 'authLoginCancel' });
  }

  function setupCodingPlan() {
    postMessage({ type: 'codingPlanSetup' });
  }

  function refreshSetupState() {
    postMessage({ type: 'refreshSetupState' });
  }

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
    setProviderName('openai');
    setProviderType('openai');
    setModel('gpt-4o');
    setBaseUrl('https://api.openai.com/v1');
    setApiKey('');
    setManualOpen(false);
  }

  const needsSetup = state.setupRequired || state.providers.length === 0;

  return (
    <div className="welcome-screen">
      <div className="welcome-content">
        <h1 className="welcome-title">AtomCode</h1>
        <p className="welcome-subtitle">
          {needsSetup ? 'Set up AtomCode to start chatting in VS Code' : 'AI-powered coding assistant'}
        </p>

        {needsSetup && (
          <section className="setup-card">
            <div className="setup-step">
              <div className="setup-copy">
                <div className="setup-title">Account</div>
                <div className="setup-subtitle">
                  {state.auth?.logged_in
                    ? `Signed in as ${state.auth.user?.name || state.auth.user?.username || 'AtomGit user'}`
                    : 'Sign in to use AtomGit CodingPlan models.'}
                </div>
              </div>
              <div className="setup-actions">
                {state.auth?.logged_in ? (
                  <button type="button" className="setup-secondary" onClick={refreshSetupState}>Refresh account</button>
                ) : (
                  <button type="button" className="setup-primary" onClick={startLogin}>Sign in with AtomGit</button>
                )}
              </div>
            </div>

            {state.loginUrl && (
              <div className="setup-url">
                <span>{state.loginUrl}</span>
                <button type="button" onClick={() => navigator.clipboard.writeText(state.loginUrl || '')}>Copy</button>
                <button type="button" onClick={cancelLogin}>Cancel</button>
              </div>
            )}

            <div className="setup-step">
              <div className="setup-copy">
                <div className="setup-title">Models</div>
                <div className="setup-subtitle">
                  {state.providers.length > 0
                    ? `${state.providers.length} provider${state.providers.length === 1 ? '' : 's'} configured`
                    : 'Sync CodingPlan models or add a provider manually.'}
                </div>
              </div>
              <div className="setup-actions">
                {state.auth?.logged_in && (
                  <button type="button" className="setup-primary" onClick={setupCodingPlan}>Sync CodingPlan models</button>
                )}
              </div>
            </div>

            <button type="button" className="setup-secondary setup-wide" onClick={() => setManualOpen(!manualOpen)}>
              Add provider manually
            </button>

            {manualOpen && (
              <form className="provider-form" onSubmit={submitProvider}>
                <input value={providerName} onChange={(e) => setProviderName(e.target.value)} placeholder="Provider name" />
                <input value={providerType} onChange={(e) => setProviderType(e.target.value)} placeholder="Type, e.g. openai" />
                <input value={model} onChange={(e) => setModel(e.target.value)} placeholder="Model" />
                <input value={baseUrl} onChange={(e) => setBaseUrl(e.target.value)} placeholder="Base URL" />
                <input value={apiKey} onChange={(e) => setApiKey(e.target.value)} placeholder="API key" type="password" />
                <button className="setup-primary setup-wide" type="submit">Save provider</button>
              </form>
            )}

            {state.setupStatus && <div className="setup-status">{state.setupStatus}</div>}
            {state.setupError && <div className="setup-error">{state.setupError}</div>}
          </section>
        )}

        <div className="quick-actions">
          {quickActions.map((a) => (
            <button
              key={a.id}
              className="quick-action-card"
              onClick={() => handleAction(a.id)}
            >
              <span className="quick-action-icon">{a.icon}</span>
              <span className="quick-action-label">{a.label}</span>
            </button>
          ))}
        </div>
      </div>
    </div>
  );
}
