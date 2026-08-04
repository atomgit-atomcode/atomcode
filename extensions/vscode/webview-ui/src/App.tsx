import React from 'react';
import { ChatProvider, useChatContext } from './state/ChatProvider';
import { Header } from './components/Header';
import { WelcomeScreen } from './components/WelcomeScreen';
import { MessageList } from './components/MessageList';
import { InputArea } from './components/InputArea';
import { SessionList } from './components/SessionList';
import { I18nProvider } from './i18n';

function ChatApp() {
  const { state, dispatch } = useChatContext();
  const hasMessages = state.messages.length > 0 || state.isGenerating;

  if (state.isSessionList) {
    return (
      <div className="app app-sidebar">
        <SessionList variant="sidebar" />
      </div>
    );
  }

  return (
    <div className="app">
      <Header />
      {state.historyOpen && <SessionList variant="overlay" />}
      <div className="session-body">
        {hasMessages ? <MessageList /> : <WelcomeScreen />}
        {state.persistenceWarning && (
          <div className="persistence-warning" role="status">
            <span aria-hidden="true">⚠</span>
            <span>{state.persistenceWarning}</span>
            <button
              type="button"
              aria-label="Dismiss"
              onClick={() => dispatch({ type: 'SET_PERSISTENCE_WARNING' })}
            >
              ×
            </button>
          </div>
        )}
        <InputArea />
      </div>
    </div>
  );
}

export function App() {
  return (
    <ChatProvider>
      <LocalizedChatApp />
    </ChatProvider>
  );
}

function LocalizedChatApp() {
  const { state } = useChatContext();

  return (
    <I18nProvider locale={state.locale}>
      <ChatApp />
    </I18nProvider>
  );
}
