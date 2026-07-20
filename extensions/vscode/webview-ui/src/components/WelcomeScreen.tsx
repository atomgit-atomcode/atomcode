import React, { useState } from 'react';
import { postMessage } from '../vscode';
import { useChatContext } from '../state/ChatProvider';
import { MsgKey, useT } from '../i18n';

interface QuickAction {
  id: string;
  labelKey: MsgKey;
  icon: string;
}

const quickActions: QuickAction[] = [
  { id: 'intro', labelKey: 'welcome.quick.intro', icon: '💡' },
  { id: 'projectOverview', labelKey: 'welcome.quick.projectOverview', icon: '🗂️' },
  { id: 'improvements', labelKey: 'welcome.quick.improvements', icon: '🔎' },
  { id: 'devPlan', labelKey: 'welcome.quick.devPlan', icon: '🧭' },
  { id: 'configuration', labelKey: 'welcome.quick.configuration', icon: '⚙️' },
  { id: 'tips', labelKey: 'welcome.quick.tips', icon: '✨' },
];

export function WelcomeScreen() {
  const { state } = useChatContext();
  const t = useT();
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
  const authUsable = state.auth?.logged_in === true && state.auth.expired !== true;
  const signedInName = state.auth?.user?.name || state.auth?.user?.username || t('setup.atomgitUser');

  return (
    <div className="welcome-screen">
      <div className="welcome-content">
        <h1 className="welcome-title">AtomCode</h1>
        <p className="welcome-subtitle">
          {needsSetup ? t('welcome.subtitle.setup') : t('welcome.subtitle.ready')}
        </p>

        {needsSetup && (
          <section className="setup-card">
            <div className="setup-step">
              <div className="setup-copy">
                <div className="setup-title">{t('setup.account')}</div>
                <div className="setup-subtitle">
                  {authUsable
                    ? t('setup.signedInAs', { name: signedInName })
                    : t('setup.signInHint')}
                </div>
              </div>
              <div className="setup-actions">
                {authUsable ? (
                  <button type="button" className="setup-secondary" onClick={refreshSetupState}>{t('setup.refreshAccount')}</button>
                ) : (
                  <button type="button" className="setup-primary" onClick={startLogin}>{t('setup.signInWithAtomGit')}</button>
                )}
              </div>
            </div>

            {state.loginUrl && (
              <div className="setup-url">
                <span>{state.loginUrl}</span>
                <button type="button" onClick={() => navigator.clipboard.writeText(state.loginUrl || '')}>{t('setup.copy')}</button>
                <button type="button" onClick={cancelLogin}>{t('setup.cancel')}</button>
              </div>
            )}

            <div className="setup-step">
              <div className="setup-copy">
                <div className="setup-title">{t('setup.models')}</div>
                <div className="setup-subtitle">
                  {state.providers.length > 0
                    ? t('setup.providersConfigured', { count: state.providers.length })
                    : t('setup.syncOrAddProvider')}
                </div>
              </div>
              <div className="setup-actions">
                {authUsable && (
                  <button type="button" className="setup-primary" onClick={setupCodingPlan}>{t('setup.syncCodingPlanModels')}</button>
                )}
              </div>
            </div>

            <button type="button" className="setup-secondary setup-wide" onClick={() => setManualOpen(!manualOpen)}>
              {t('setup.addProviderManually')}
            </button>

            {manualOpen && (
              <form className="provider-form" onSubmit={submitProvider}>
                <input value={providerName} onChange={(e) => setProviderName(e.target.value)} placeholder={t('setup.providerName')} />
                <input value={providerType} onChange={(e) => setProviderType(e.target.value)} placeholder={t('setup.providerType')} />
                <input value={model} onChange={(e) => setModel(e.target.value)} placeholder={t('setup.model')} />
                <input value={baseUrl} onChange={(e) => setBaseUrl(e.target.value)} placeholder={t('setup.baseUrl')} />
                <input value={apiKey} onChange={(e) => setApiKey(e.target.value)} placeholder={t('setup.apiKey')} type="password" />
                <button className="setup-primary setup-wide" type="submit">{t('setup.saveProvider')}</button>
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
              <span className="quick-action-label">{t(a.labelKey)}</span>
            </button>
          ))}
        </div>
      </div>
    </div>
  );
}
