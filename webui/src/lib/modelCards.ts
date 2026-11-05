export interface Evaluation {
  model: string; bench: string; score: number; source: string; source_id: string;
  by: string; date?: string; note?: string; mode?: unknown; settings?: unknown;
  [field: string]: unknown;
}
export interface CardTask {
  id: string; label: string; rank: number | null; total: number;
  g: number | null; n: number; comparison_set: { id: string; label: string } | null;
}
export interface ModelCard {
  id: string; name: string; maker: string; checkpoint: string | null;
  identity_note: string | null; valid_benchmark_count: number;
  evaluations: Evaluation[]; tasks: CardTask[]; best_task: string | null; worst_task: string | null;
}
export interface ModelCards {
  models: Record<string, ModelCard>;
  benchmarks: Record<string, { name: string; kind: string; unit?: string; direction?: string; raw_scale_min?: number; raw_scale_max?: number }>;
  upstream: { updated: string; commit: string; repository: string };
  attribution: { credit: string; license: string; license_url: string; upstream_notices: string; notice: string };
  sources: Record<string, { name: string; attribution?: string; license: string; license_url?: string; terms_url: string }>;
}
export function scoreLabel(score: number, bench: ModelCards['benchmarks'][string]): string {
  if (bench.kind === 'elo') return score.toFixed(1);
  if (bench.kind === 'linear') return `${score.toFixed(4)} ${bench.unit ?? ''}`.trim();
  if (bench.raw_scale_min !== undefined && bench.raw_scale_max !== undefined)
    return `${(bench.raw_scale_min + score * (bench.raw_scale_max - bench.raw_scale_min)).toFixed(2)} / ${bench.raw_scale_max}`;
  return `${(score * 100).toFixed(1)}%`;
}
// Matching a benchmark name alone does not establish comparable evaluation conditions.
export function sameConditions(a: Evaluation, b: Evaluation): boolean {
  return ['bench', 'source', 'source_id', 'by', 'date', 'note', 'mode', 'settings',
    'configuration', 'api_configuration', 'strong_prompting', 'evaluator', 'engine_label', 'evaluation_run_group', 'voice']
    .every(field => JSON.stringify(a[field] ?? null) === JSON.stringify(b[field] ?? null));
}
export function taskComparable(a: CardTask, b: CardTask): boolean {
  return a.id === b.id && !!a.comparison_set?.id && a.comparison_set.id === b.comparison_set?.id;
}
export function safeSource(url: string | undefined): string | undefined {
  if (!url) return undefined;
  try { const u = new URL(url); return ['https:', 'http:'].includes(u.protocol) ? u.href : undefined; }
  catch { return undefined; }
}
