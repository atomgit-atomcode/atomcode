import type { UiCallback } from './state/types';

declare global {
  interface Window {
    jsQuery: (json: string) => void;
    dispatchRender: (json: string) => void;
  }
}

export function postMessage(msg: UiCallback): void {
  if (typeof window.jsQuery === 'function') {
    window.jsQuery(JSON.stringify(msg));
  }
}
