import test from 'node:test';
import assert from 'node:assert/strict';
import { deriveTurnNavigation, navigationSummary, turnWindow, turnAtReadingLine } from './turnNavigation.ts';
const message = (role: string, text = '', extra = {}) => ({ role, parts: [{ kind: 'text', text }], ...extra });
test('maps user anchors exactly, including identical prompts and transparent notices/tools', () => {
  const rows = deriveTurnNavigation([message('assistant', 'orphan'), message('user', 'repeat'),
    message('assistant', 'First'), message('system', 'notice'),
    { role: 'assistant', parts: [{ kind: 'tool' }] }, message('assistant', 'second'),
    message('user', 'repeat'), message('assistant', 'next')]);
  assert.deepEqual(rows.map(r => [r.messageIndex, r.endIndex, r.user, r.assistant]),
    [[1, 5, 'repeat', 'First second'], [6, 7, 'repeat', 'next']]);
  assert.notEqual(rows[0].key, rows[1].key);
});
test('preserves retained provenance and pending/image-only user nodes without inventing text', () => {
  const rows = deriveTurnNavigation([message('user', 'old', { retainedHistory: true }),
    message('user', '', { pendingSteerId: 's', images: [{}] })], new Set(['s']));
  assert.equal(rows[0].retained, true);
  assert.equal(rows[1].pending, true);
  assert.equal(rows[1].user, '');
  assert.equal(deriveTurnNavigation([message('user', 'applied', { pendingSteerId: 's' })])[0].pending, false,
    'an acknowledged or withdrawn steer must not stay pending');
});
test('streamed summaries grow; snapshots, rewind and session replacement rebuild anchor indexes', () => {
  const before = [message('user', 'a'), message('assistant', 'hel')];
  assert.equal(deriveTurnNavigation(before)[0].assistant, 'hel');
  assert.equal(deriveTurnNavigation([before[0], message('assistant', 'hello')])[0].assistant, 'hello');
  assert.deepEqual(deriveTurnNavigation(before.slice(0, 1)).map(r => r.endIndex), [0]);
  assert.deepEqual(deriveTurnNavigation([]), []);
  assert.equal(deriveTurnNavigation([message('user', 'new')])[0].messageIndex, 0);
});
test('summaries are bounded and whitespace normalized', () => {
  assert.equal(navigationSummary(' a\n b\t c '), 'a b c');
  assert.equal(navigationSummary('123456', 3), '123…');
  assert.ok(deriveTurnNavigation([message('user', 'x'.repeat(10000))])[0].user.length <= 161);
});
test('rail windows stay bounded for thousands of turns and include the last item', () => {
  assert.deepEqual(turnWindow(0, 0, 300, 24), { start: 0, end: 0 });
  const range = turnWindow(10000, 5000 * 24, 300, 24);
  assert.ok(range.end - range.start < 30);
  assert.ok(range.start <= 5000 && range.end > 5000);
  assert.equal(turnWindow(10000, 10000 * 24 - 300, 300, 24).end, 10000);
});
test('reading position is located with logarithmic anchor measurements', () => {
  let calls = 0;
  assert.equal(turnAtReadingLine(10000, i => { calls++; return i * 100; }, 543210), 5432);
  assert.ok(calls < 20);
  assert.equal(turnAtReadingLine(2, i => i * 100, -20), 0);
});

test('smooth jumps hold follow off until arrival, including archived record loading', async () => {
  const { createTurnNavigationIntent } = await import('./turnNavigation.ts');
  const intent = createTurnNavigationIntent();
  const viewport = { scrollTop: 900, scrollHeight: 1500, clientHeight: 600 };
  assert.equal(intent.onScroll(viewport), null);
  intent.jumpTo(Infinity);
  assert.equal(intent.onScroll(viewport), false, 'archive is loading while still at bottom');
  intent.jumpTo(100);
  assert.equal(intent.onScroll(viewport), false, 'first animation frame is still near bottom');
  viewport.scrollTop = 100;
  assert.equal(intent.onScroll(viewport), null, 'ordinary scroll resumes at the target');
});
test('returning to bottom keeps following through content growth; gestures and session resets win', async () => {
  const { createTurnNavigationIntent } = await import('./turnNavigation.ts');
  const intent = createTurnNavigationIntent();
  const viewport = { scrollTop: 100, scrollHeight: 1500, clientHeight: 600 };
  intent.followBottom();
  assert.equal(intent.onScroll(viewport), true);
  viewport.scrollHeight += 500;
  assert.equal(intent.onScroll(viewport), true);
  viewport.scrollTop = 1400;
  assert.equal(intent.onScroll(viewport), true);
  assert.equal(intent.onScroll(viewport), null);
  intent.followBottom(); intent.interact();
  assert.equal(intent.onScroll(viewport), null);
  intent.jumpTo(Infinity); intent.interact();
  assert.equal(intent.onScroll(viewport), null);
});

test('unified navigation uses timestamps, retains undone history and adds unfinished live turns', async () => {
  const { mergeTurnNavigation } = await import('./turnNavigation.ts');
  const items = deriveTurnNavigation([message('user', 'repeat', { ts: 1000, retainedHistory: true }),
    message('user', 'repeat', { ts: 2000 }), message('assistant', 'answer'), message('user', 'new', { ts: 3000 })]);
  const records = [{ turn_id: 1, user: 'repeat', started_at: 1000, undone: false },
    { turn_id: 2, user: 'repeat', started_at: 2000, undone: false },
    { turn_id: 3, user: 'undone', started_at: 2500, undone: true }];
  assert.deepEqual(mergeTurnNavigation(records, items).map(e => [e.record?.turn_id ?? null, e.live]),
    [[1, -1], [2, 1], [3, -1], [null, 2]]);
});
test('unified navigation never maps repeated or truncated legacy previews onto the wrong current message', async () => {
  const { mergeTurnNavigation } = await import('./turnNavigation.ts');
  const items = deriveTurnNavigation([message('user', 'repeat')]);
  const records = [1, 2].map(turn_id => ({ turn_id, user: 'repeat', undone: false }));
  assert.deepEqual(mergeTurnNavigation(records, items).map(e => e.live), [-1, -1, 0]);
  assert.deepEqual(mergeTurnNavigation([{ ...records[0], user_truncated: true }], items).map(e => e.live), [-1, 0]);
});
