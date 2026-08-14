import type { TurnStats } from '../api';

export function formatTurnDuration(ms: number): string {
  const seconds = ms / 1000;
  if (seconds < 60) return `${Math.round(seconds * 10) / 10}s`;
  const rounded = Math.round(seconds);
  return `${Math.floor(rounded / 60)}m${rounded % 60}s`;
}

export function formatTurnTokens(tokens: number): string {
  if (tokens < 1000) return String(tokens);
  if (tokens < 1_000_000) return `${Math.round(tokens / 100) / 10}K`;
  return `${Math.round(tokens / 100_000) / 10}M`;
}

export function turnCacheHit(stats: TurnStats): number | null {
  if (stats.cached_tokens === 0 || stats.prompt_tokens === 0) return null;
  return Math.min(100, Math.floor(stats.cached_tokens / stats.prompt_tokens * 100));
}
