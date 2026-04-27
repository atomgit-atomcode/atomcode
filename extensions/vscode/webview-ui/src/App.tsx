import React from 'react';
import { ChatProvider, useChatContext } from './state/ChatProvider';
import { Header } from './components/Header';
import { WelcomeScreen } from './components/WelcomeScreen';
import { MessageList } from './components/MessageList';
import { InputArea } from './components/InputArea';
import { SessionList } from './components/SessionList';

function ChatApp() {
  const { state } = useChatContext();
  const hasMessages = state.messages.length > 0 || state.isGenerating;

  return (
    <div className="app">
      <Header />
      {state.historyOpen && <SessionList />}
      <div className="session-body">
        {hasMessages ? <MessageList /> : <WelcomeScreen />}
        <InputArea />
      </div>
    </div>
  );
}

export function App() {
  return (
    <ChatProvider>
      <ChatApp />
    </ChatProvider>
  );
}
