import React, { useReducer, useEffect } from 'react';
import { createRoot } from 'react-dom/client';
import { uiReducer, initialUiState } from './state/reducer';
import { App } from './components/App';
import { postMessage } from './bridge';
import type { ChatViewModel } from './state/types';

function WebviewRoot() {
  const [state, dispatch] = useReducer(uiReducer, initialUiState);

  useEffect(() => {
    window.dispatchRender = (json: string) => {
      const model: ChatViewModel = JSON.parse(json);
      dispatch({ type: 'FULL_SYNC', model });
    };
    postMessage({ type: 'ready' });
  }, []);

  return <App model={state.model} />;
}

const root = createRoot(document.getElementById('root')!);
root.render(<WebviewRoot />);
