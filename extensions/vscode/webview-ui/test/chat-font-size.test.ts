import assert from 'node:assert/strict';
import { mountChatFontSize, normalizeChatFontSize } from '../src/utils/chatFontSize';
import { updateWebviewState } from '../src/vscode';

let saved: any = { sessionId: 'session-a', projectHash: 'project-a' };
(globalThis as any).acquireVsCodeApi = () => ({
  getState: () => saved,
  setState: (value: unknown) => { saved = value; },
  postMessage: () => { throw new Error('Zoom must not write host settings'); },
});
let listener: ((event: any) => void) | undefined;
const properties = new Map<string, string>();
const doc = {
  documentElement: { dataset: { fontSize: '13' }, style: { setProperty: (key: string, value: string) => properties.set(key, value) } },
  querySelector: () => null,
  dispatchEvent: (event: Event) => { assert.equal(event.type, 'atomcode-font-size'); return true; },
  addEventListener: (type: string, cb: any, options: any) => {
    assert.equal(type, 'wheel');
    assert.equal(options.passive, false);
    listener = cb;
  },
  removeEventListener: (_type: string, cb: any) => { assert.equal(cb, listener); listener = undefined; },
} as unknown as Document;
function wheel(patch: Record<string, unknown> = {}) {
  let prevented = false;
  listener?.({ ctrlKey: true, deltaY: -100, deltaMode: 0, target: { closest: () => ({}) },
    preventDefault: () => { prevented = true; }, ...patch });
  return prevented;
}
const size = () => properties.get('--app-font-size');
assert.equal(normalizeChatFontSize(Infinity), 13);
assert.equal(normalizeChatFontSize('30'), 13);
const font = mountChatFontSize(doc);
assert.equal(size(), '13px');
assert.equal(wheel({ ctrlKey: false }), false);
assert.equal(wheel({ deltaY: 0 }), false);
assert.equal(wheel({ deltaY: NaN }), false);
assert.equal(wheel({ target: { closest: () => null } }), false);
assert.equal(wheel({ ctrlKey: false, metaKey: true }), true);
assert.equal(size(), '14px');
assert.equal(saved.sessionId, 'session-a');
updateWebviewState({ sessionId: 'session-b' });
font.configure(13); // A ready replay must not undo local zoom.
assert.equal(size(), '14px');
font.dispose();
const restored = mountChatFontSize(doc);
assert.equal(size(), '14px');
assert.equal(saved.sessionId, 'session-b');
restored.configure(20);
assert.equal(size(), '20px');
for (let i = 0; i < 3; i++) wheel({ deltaY: -10 });
assert.equal(size(), '20px');
wheel({ deltaY: -10 });
assert.equal(size(), '21px');
restored.configure(40);
assert.equal(wheel(), true);
assert.equal(size(), '40px');
restored.configure(9);
wheel({ deltaY: 100 });
assert.equal(size(), '9px');
restored.dispose();
// Settings changed while the document was destroyed: stale local zoom is discarded.
(doc.documentElement.dataset as any).fontSize = '30';
const changed = mountChatFontSize(doc);
assert.equal(size(), '30px');
changed.dispose();
assert.equal(listener, undefined);
console.log('chat font sizing: shortcuts, bounds, state merge, restore and live settings passed');
