import React from 'react';
import { ChatProvider } from './state/ChatProvider';

export function App() {
  return (
    <ChatProvider>
      <div className="app">AtomCode React Webview Loading...</div>
    </ChatProvider>
  );
}
