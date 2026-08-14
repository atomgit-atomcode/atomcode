import assert from 'node:assert/strict';
import test from 'node:test';
import { formatTurnDuration, formatTurnTokens, turnCacheHit } from './turnStats.ts';

test('formats compact turn durations and token totals', () => {
  assert.equal(formatTurnDuration(10_740), '10.7s');
  assert.equal(formatTurnDuration(65_000), '1m5s');
  assert.equal(formatTurnTokens(267_000), '267K');
  assert.equal(formatTurnTokens(1_250_000), '1.3M');
});

test('cache hit is only shown when the provider reported cached input', () => {
  assert.equal(turnCacheHit({
    duration_ms: 1,
    rounds: 1,
    tool_calls: 0,
    prompt_tokens: 100,
    completion_tokens: 5,
    cached_tokens: 87,
  }), 87);
  assert.equal(turnCacheHit({
    duration_ms: 1,
    rounds: 1,
    tool_calls: 0,
    prompt_tokens: 100,
    completion_tokens: 5,
    cached_tokens: 0,
  }), null);
  assert.equal(turnCacheHit({
    duration_ms: 1,
    rounds: 1,
    tool_calls: 0,
    prompt_tokens: 3,
    completion_tokens: 1,
    cached_tokens: 2,
  }), 66, 'matches the TUI integer percentage');
});
