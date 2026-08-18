import assert from 'node:assert/strict';
import test from 'node:test';
import {
  completedTurnStats,
  formatTurnDuration,
  formatTurnTokens,
  turnCacheHit,
} from './turnStats.ts';

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

test('turn stats are exposed only for a naturally completed turn', () => {
  const stats = {
    duration_ms: 2_000,
    rounds: 0,
    tool_calls: 0,
    prompt_tokens: 0,
    completion_tokens: 0,
    cached_tokens: 0,
  };

  assert.equal(completedTurnStats(undefined, 'stopped'), null);
  assert.equal(completedTurnStats(stats, 'cancelled'), null);
  assert.equal(completedTurnStats(stats, 'max_rounds'), null);
  assert.equal(completedTurnStats(stats, 'stopped'), stats);
  assert.equal(completedTurnStats(stats, undefined), stats, 'supports older daemons');
});
