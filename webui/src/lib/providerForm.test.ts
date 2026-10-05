import { test } from 'node:test';
import assert from 'node:assert';
import {
  parseTokens,
  formatTokens,
  apiKeyProblem,
  validAccountId,
  validBaseUrl,
  firstBlocker,
  modelsBlocker,
  modelBody,
  adoptPicked,
  emptyModelDraft,
  reasoningFrom,
  toggleLevel,
  reasoningEditBody,
  type ProviderDraft,
} from './providerForm.ts';

test('token counts are read the way people write them', () => {
  assert.equal(parseTokens(''), undefined);
  assert.equal(parseTokens('128000'), 128000);
  assert.equal(parseTokens('128K'), 128000);
  assert.equal(parseTokens('256k'), 256000);
  assert.equal(parseTokens('1M'), 1000000);
  assert.equal(parseTokens('1.5m'), 1500000);
  assert.equal(parseTokens('1,000,000'), 1000000);
  assert.equal(parseTokens('0'), null);
  assert.equal(parseTokens('-5'), null);
  assert.equal(parseTokens('1.5'), null, 'not a whole number of tokens');
  assert.equal(parseTokens('big'), null);
});

test('token counts are shown compactly and round-trip', () => {
  assert.equal(formatTokens(1000000), '1M');
  assert.equal(formatTokens(1500000), '1.5M');
  assert.equal(formatTokens(128000), '128K');
  assert.equal(formatTokens(4097), '4097');
  for (const n of [1000000, 128000, 32768]) assert.equal(parseTokens(formatTokens(n)), n);
});

test('a pasted key is caught when it is an env line, quoted, or spaced', () => {
  assert.equal(apiKeyProblem(''), null);
  assert.equal(apiKeyProblem('sk-abc123'), null);
  assert.equal(apiKeyProblem('  sk-abc123  '), null, 'surrounding space is trimmed');
  assert.equal(apiKeyProblem('OPENAI_API_KEY=sk-abc'), 'env_line');
  assert.equal(apiKeyProblem('"sk-abc"'), 'quoted');
  assert.equal(apiKeyProblem('sk abc'), 'whitespace');
  assert.equal(apiKeyProblem('sk-密钥'), 'non_ascii');
});

test('account ids and base URLs follow the daemon rules', () => {
  assert.ok(validAccountId('my-gw'));
  assert.ok(validAccountId('gw.2'));
  assert.ok(!validAccountId('-gw'));
  assert.ok(!validAccountId('my gw'));
  assert.ok(!validAccountId('a/b'));
  assert.ok(validBaseUrl('https://api.example.com/v1'));
  assert.ok(validBaseUrl('http://localhost:11434'));
  assert.ok(!validBaseUrl('ftp://x'));
  assert.ok(!validBaseUrl('https://user:pw@x.example'));
  assert.ok(!validBaseUrl('not a url'));
});

const ctx = { takenIds: ['deepseek', 'Mine'], hasDefaultEndpoint: false, requiresKey: true };

function draft(over: Partial<ProviderDraft> = {}): ProviderDraft {
  return {
    mode: 'custom',
    provider: 'openai-compatible',
    id: 'gw',
    displayName: '',
    baseUrl: 'https://gw.example/v1',
    apiKey: 'sk-1',
    models: [emptyModelDraft('m1')],
    ...over,
  };
}

test('the form names the first thing that blocks it, in order', () => {
  assert.equal(firstBlocker(draft(), ctx), null);
  assert.deepEqual(firstBlocker(draft({ provider: '' }), ctx), { kind: 'provider' });
  assert.deepEqual(firstBlocker(draft({ id: '' }), ctx), { kind: 'id' });
  assert.deepEqual(firstBlocker(draft({ id: 'mine' }), ctx), { kind: 'id_taken' }, 'case-insensitive');
  assert.deepEqual(firstBlocker(draft({ baseUrl: '' }), ctx), { kind: 'base_url' });
  assert.deepEqual(firstBlocker(draft({ apiKey: 'K=v' }), ctx), { kind: 'api_key', problem: 'env_line' });
  assert.deepEqual(firstBlocker(draft({ apiKey: '' }), ctx), { kind: 'api_key', problem: 'missing' });
  assert.deepEqual(firstBlocker(draft({ models: [] }), ctx), { kind: 'no_models' });
  // Two problems at once: only the first is reported.
  assert.deepEqual(firstBlocker(draft({ id: '', baseUrl: '' }), ctx), { kind: 'id' });
});

test('a preset needs no id and no endpoint when it has its own', () => {
  const preset = draft({ mode: 'preset', provider: 'deepseek', id: '', baseUrl: '' });
  assert.equal(firstBlocker(preset, { ...ctx, hasDefaultEndpoint: true }), null);
  assert.equal(firstBlocker({ ...preset, apiKey: '' }, { ...ctx, hasDefaultEndpoint: true, requiresKey: false }), null);
});

test('model rows are judged one by one, against the account too', () => {
  const rows = [emptyModelDraft('a'), { ...emptyModelDraft('b'), window: '128K' }];
  assert.equal(modelsBlocker(rows), null);
  assert.deepEqual(modelsBlocker([...rows, emptyModelDraft(' ')]), { kind: 'model_name', row: 2 });
  assert.deepEqual(modelsBlocker([...rows, emptyModelDraft('a')]), { kind: 'model_duplicate', row: 2 });
  assert.deepEqual(modelsBlocker(rows, ['b']), { kind: 'model_duplicate', row: 1 });
  assert.deepEqual(modelsBlocker([{ ...emptyModelDraft('c'), window: 'lots' }]), { kind: 'model_window', row: 0 });
  assert.deepEqual(modelsBlocker([{ ...emptyModelDraft('c'), maxTokens: '0' }]), { kind: 'model_max_tokens', row: 0 });
});

test('a model row is sent with only what was set', () => {
  assert.deepEqual(modelBody(emptyModelDraft(' m ')), { model: 'm' });
  assert.deepEqual(
    modelBody({
      model: 'm',
      displayName: 'M',
      window: '1M',
      maxTokens: '32K',
      vision: true,
      reasoning: { enabled: true, levels: ['high', 'max'], effort: 'max' },
    }),
    {
      model: 'm',
      display_name: 'M',
      context_window: 1000000,
      max_tokens: 32000,
      supports_vision: true,
      reasoning_effort_levels: ['high', 'max'],
      reasoning_effort: 'max',
    },
  );
});

const ALL = ['low', 'medium', 'high', 'xhigh', 'max'];

test('reasoning starts from what the file declares', () => {
  assert.deepEqual(reasoningFrom(null, null, ALL), { enabled: false, levels: [], effort: null });
  assert.deepEqual(
    reasoningFrom([], 'high', ALL),
    { enabled: true, levels: ALL, effort: 'high' },
    'a default with no list offers every level, so the form opens valid',
  );
  assert.deepEqual(reasoningFrom(['high', 'max'], null, ALL), { enabled: true, levels: ['high', 'max'], effort: null });
  assert.deepEqual(reasoningFrom(null, 'auto', ALL).effort, 'auto');
});

test('toggling a level keeps canonical order and drops a default no longer offered', () => {
  let r = { enabled: true, levels: ['high'], effort: 'high' as string | null };
  r = toggleLevel(r, 'low', ALL);
  assert.deepEqual(r.levels, ['low', 'high'], 'canonical order, not click order');
  r = toggleLevel(r, 'high', ALL);
  assert.deepEqual(r.levels, ['low']);
  assert.equal(r.effort, null, 'the default went with its level');
  const auto = toggleLevel({ enabled: true, levels: ['high'], effort: 'auto' }, 'low', ALL);
  assert.equal(auto.effort, 'auto', 'auto is no level and stays');
});

test('reasoning switched on needs a level, and an edit clears what was turned off', () => {
  const on = { ...emptyModelDraft('m'), reasoning: { enabled: true, levels: [], effort: null } };
  assert.deepEqual(modelsBlocker([on]), { kind: 'model_reasoning', row: 0 });
  assert.deepEqual(reasoningEditBody({ enabled: false, levels: ['high'], effort: 'high' }), {
    clear_reasoning_effort: true,
    clear_reasoning_effort_levels: true,
  });
  assert.deepEqual(reasoningEditBody({ enabled: true, levels: ['high', 'max'], effort: null }), {
    reasoning_effort_levels: ['high', 'max'],
    clear_reasoning_effort: true,
  });
});

test('a discovery pick adds new rows with what the endpoint reported, never twice', () => {
  const tuned = { ...emptyModelDraft('a'), window: '64K' };
  const rows = adoptPicked(
    [tuned, emptyModelDraft()],
    [
      { id: 'a', context_window: 128000 },
      { id: 'b', name: 'Model B', context_window: 1000000, max_tokens: 32000 },
      { id: 'c', name: 'c' },
      { id: 'old' },
    ],
    ['old'],
  );
  assert.deepEqual(rows.map((r) => r.model), ['a', 'b', 'c'], 'placeholder dropped, existing skipped');
  assert.equal(rows[0].window, '64K', 'a tuned row is left alone');
  assert.equal(rows[1].displayName, 'Model B');
  assert.equal(rows[1].window, '1M');
  assert.equal(rows[1].maxTokens, '32K');
  assert.equal(rows[2].displayName, '', 'a name equal to the id adds nothing');
});
