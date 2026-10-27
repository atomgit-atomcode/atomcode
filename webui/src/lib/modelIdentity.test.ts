import test from 'node:test';
import assert from 'node:assert/strict';
import { displayedModel, mayFollowModelDefault } from './modelIdentity.ts';
import type { ModelInfo } from '../api.ts';
const endpoint = (provider: string, model: string, is_default = false): ModelInfo => ({
  provider, model, is_default, provider_type: 'openai', effort_applicable: true,
  reasoning_effort: 'high', effort_levels: ['high'],
});
const catalog = [endpoint('configured-default', 'deepseek-v4.1-flash', true), endpoint('alias', 'configured-name')];

test('a live model is authoritative even when config now maps its alias to another model', () => {
  const current = displayedModel(catalog, 'alias', { provider: 'alias', model: 'lumen-alpha' })!;
  assert.equal(current.model, 'lumen-alpha');
  assert.equal(current.provider, 'alias');
  assert.equal(current.effort_applicable, false, 'do not borrow capabilities from the future configured endpoint');
});
test('a running model removed from config never becomes the configured default', () => {
  assert.equal(displayedModel(catalog, 'removed', { provider: 'removed', model: 'lumen-alpha' })?.model, 'lumen-alpha');
  assert.equal(displayedModel(catalog, 'removed')?.model, 'removed');
});
test('a default config only seeds an unspecified selection', () => {
  assert.equal(displayedModel(catalog, null)?.model, 'deepseek-v4.1-flash');
  assert.equal(displayedModel(catalog, null, { provider: 'removed', model: 'actual' })?.model, 'actual');
});
test('matching endpoint retains declared capabilities while no mounted model does not fabricate one', () => {
  assert.equal(displayedModel(catalog, 'alias', { provider: 'alias', model: 'configured-name' }), catalog[1]);
  assert.equal(displayedModel(catalog, 'alias', { provider: 'alias', model: null })?.model, 'alias');
});
test('a pending selection can display its candidate until the authoritative event accepts or rejects it', () => {
  const runtime = { provider: 'alias', model: 'lumen-alpha' };
  assert.equal(displayedModel(catalog, 'configured-default', runtime), catalog[0]);
  assert.equal(displayedModel(catalog, 'alias', runtime)?.model, 'lumen-alpha', 'rejection restores the actual name');
});
test('old server provider-only events stay compatible without guessing an unknown default', () => {
  assert.equal(displayedModel(catalog, 'alias', { provider: 'alias' }), catalog[1]);
  assert.equal(displayedModel(catalog, 'unknown', { provider: 'unknown' })?.model, 'unknown');
});
test('config polls cannot overwrite shared or busy runtime identity; idle non-shared chat still follows defaults', () => {
  const runtime = { provider: 'alias', model: 'lumen-alpha' };
  assert.equal(mayFollowModelDefault(false, runtime, true), false);
  assert.equal(mayFollowModelDefault(false, runtime, false), true);
  assert.equal(mayFollowModelDefault(false, null, true), true);
  assert.equal(mayFollowModelDefault(true, null, false), false);
});
