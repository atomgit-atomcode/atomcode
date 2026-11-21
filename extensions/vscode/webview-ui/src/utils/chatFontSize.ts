import { getVSCodeApi, updateWebviewState } from '../vscode';

export function normalizeChatFontSize(value: unknown): number {
  return typeof value === 'number' && Number.isFinite(value)
    ? Math.min(40, Math.max(9, Math.round(value))) : 13;
}

/** Panel-local zoom. Never changes VS Code settings or the active conversation. */
export function mountChatFontSize(doc: Document) {
  let configured = normalizeChatFontSize(Number(doc.documentElement.dataset.fontSize));
  const state = getVSCodeApi().getState() as { chatFontSize?: { configured?: unknown; size?: unknown } } | null;
  let size = state?.chatFontSize?.configured === configured
    ? normalizeChatFontSize(state.chatFontSize.size) : configured;

  const apply = () => {
    const messages = doc.querySelector<HTMLElement>('.messages-container');
    const atBottom = messages && messages.scrollHeight - messages.scrollTop - messages.clientHeight <= 4;
    const top = messages?.getBoundingClientRect().top ?? 0;
    const anchor = messages && Array.from(messages.children).find(child => child.getBoundingClientRect().bottom > top);
    const anchorTop = anchor?.getBoundingClientRect().top ?? 0;
    doc.documentElement.style.setProperty('--app-font-size', `${size}px`);
    // Let the input's owner remeasure it before restoring the reading position.
    doc.dispatchEvent(new Event('atomcode-font-size'));
    if (messages) {
      if (atBottom) messages.scrollTop = messages.scrollHeight - messages.clientHeight;
      else if (anchor) messages.scrollTop += anchor.getBoundingClientRect().top - anchorTop;
    }
    updateWebviewState({ chatFontSize: { configured, size } });
  };
  apply();

  let accumulated = 0;
  let lastWheel = 0;
  const wheel = (event: WheelEvent) => {
    const target = event.target as Element | null;
    if (!(event.ctrlKey || event.metaKey) || !Number.isFinite(event.deltaY) || event.deltaY === 0
      || !target?.closest?.('.messages-container')) return;
    // A non-passive native listener is required to suppress Chromium's own zoom.
    event.preventDefault();
    const now = Date.now();
    if (now - lastWheel > 250 || Math.sign(accumulated) !== Math.sign(event.deltaY)) accumulated = 0;
    lastWheel = now;
    accumulated += event.deltaY * (event.deltaMode === 1 ? 16 : event.deltaMode === 2 ? 400 : 1);
    // Accumulate high-resolution trackpad deltas; one mouse notch is one pixel.
    if (Math.abs(accumulated) < 40) return;
    const next = normalizeChatFontSize(size - Math.sign(accumulated));
    accumulated = 0;
    if (next !== size) { size = next; apply(); }
  };
  doc.addEventListener('wheel', wheel, { passive: false });
  return {
    configure(value: unknown) {
      const next = normalizeChatFontSize(value);
      if (next === configured) return; // Ready replay must preserve local zoom.
      configured = next;
      size = next;
      accumulated = 0;
      apply();
    },
    dispose() { doc.removeEventListener('wheel', wheel); },
  };
}
