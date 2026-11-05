#!/usr/bin/env node
// Run with Node >= 22.18 (native TypeScript stripping) or Node >= 24.
// Only maintenance uses the whatmodel checkout; consumers read the JSON snapshot.
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { mkdirSync, readFileSync, readdirSync, renameSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

export const COMMON_MODELS = [
  'deepseek-v4-flash', 'deepseek-v4-pro', 'deepseek-v4-1-flash-1017',
  'glm-5-3', 'glm-5-3-flash', 'glm-5-2', 'qwen3-8-27b',
  'qwen3-vl-8b-instruct', 'mimo-v2-5', 'mimo-v2-5-pro',
  'longcat-2-0', 'minimax-m3-1-flash',
];

// Reviewed wire identities only. Keep floating/provider-private aliases absent.
// New cards and verified bindings are maintained here, not in either frontend.
const WIRE_NAMES = {
  'deepseek-v4-flash': ['deepseek-v4-flash-0731', 'deepseek-ai/deepseek-v4-flash-0731'],
  'deepseek-v4-pro': ['deepseek-v4-pro-0813', 'deepseek-ai/deepseek-v4-pro-0813'],
  'deepseek-v4-1-flash-1017': ['deepseek-v4.1-flash-1017', 'deepseek-v4-1-flash-1017', 'deepseek-ai/deepseek-v4.1-flash-1017'],
  'glm-5-3': ['glm-5.3', 'glm-5-3'],
  'glm-5-3-flash': ['glm5.3-flash', 'glm-5.3-flash', 'glm-5-3-flash'],
  'glm-5-2': ['glm-5.2', 'glm-5-2', 'zai-org/glm-5.2'],
  'qwen3-8-27b': ['qwen3.8-27b', 'qwen3-8-27b', 'qwen/qwen3.8-27b'],
  'qwen3-vl-8b-instruct': ['qwen/qwen3-vl-8b-instruct', 'qwen3-vl-8b-instruct'],
  'mimo-v2-5': ['mimo-v2.5', 'mimo-v2-5'],
  'mimo-v2-5-pro': ['mimo-v2.5-pro', 'mimo-v2-5-pro'],
  'longcat-2-0': ['longcat-2.0', 'longcat-2-0'],
  'minimax-m3-1-flash': ['minimax-m3.1-flash', 'minimax-m3-1-flash'],
};

// Confirmed missing from this data catalogue, not a claim that the model itself
// does not exist. Do not guess an underlying model for these names.
const NO_PUBLIC_EVALUATIONS = ['lumen-alpha'];

export async function exportCards(checkout, output, selected = COMMON_MODELS) {
  const root = resolve(checkout);
  const read = name => readFileSync(join(root, name), 'utf8');
  const json = name => JSON.parse(read(`data/${name}.json`));
  const upstreamModels = json('models');
  if (new Set(selected).size !== selected.length) throw new Error('Duplicate selected model ID');
  for (const name of NO_PUBLIC_EVALUATIONS) {
    if (upstreamModels.some(model => [model.id, model.name].some(value => value.toLowerCase() === name)))
      throw new Error(`Previously unlisted model now exists upstream; review its card binding: ${name}`);
  }
  const modelById = new Map(upstreamModels.map(model => [model.id, model]));
  for (const id of selected) if (!modelById.has(id)) throw new Error(`Unknown whatmodel model: ${id}`);

  // Import before filtering: rankings and strengths must use the full population.
  const module = name => import(pathToFileURL(join(root, `lib/${name}.ts`)));
  const [{ strengths }, { fitFor, comparisonForModel }, { boardData }, { sourceOf }, { TASKS }] =
    await Promise.all(['strengths', 'data', 'board-data', 'sources', 'tasks'].map(module));
  const allResults = json('results');
  const allBenchmarks = new Map(json('benchmarks').map(bench => [bench.id, bench]));
  const sources = new Map();
  const benchmarks = new Map();
  const cards = {};
  for (const id of selected) {
    const model = modelById.get(id);
    const evaluations = allResults.filter(row => row.model === id && !row.invalid).map(row => {
      if (!allBenchmarks.has(row.bench)) throw new Error(`Unknown benchmark: ${row.bench}`);
      if (!Number.isFinite(row.score)) throw new Error(`Invalid score: ${id}/${row.bench}`);
      const source = sourceOf(row.source);
      if (source?.verdict !== 'allowed') throw new Error(`Unreviewed evaluation source: ${row.source}`);
      sources.set(source.id, source);
      benchmarks.set(row.bench, allBenchmarks.get(row.bench));
      return { ...row, source_id: source.id };
    });
    const performance = strengths(id);
    const category = model.category ?? 'llm';
    const overallSet = comparisonForModel(id);
    const taskPerformance = performance?.tasks.map(({ tests, ...task }) => {
      const set = boardData(category).tasks?.find(t => t.id === task.id)?.sets
        .find(s => s.models.includes(id));
      return {
        ...task,
        comparison_set: set ? { id: set.id, label: set.label } : null,
        test_benchmarks: tests.map(test => test.bench),
      };
    }) ?? [];
    cards[id] = {
      id, name: model.name, maker: model.provider,
      checkpoint: model.model_repo ?? null,
      identity_note: model.identity_note ?? null,
      // Deliberately not a wire-model alias list. Runtime matching needs an
      // independently verified provider + model binding before attaching scores.
      evaluations,
      valid_benchmark_count: new Set(evaluations.map(row => row.bench)).size,
      overall: overallSet ? {
        comparison_set: { id: overallSet.id, label: overallSet.label },
        score: fitFor(id).scores.get(id) ?? null,
        rank: performance?.overall ?? null,
      } : null,
      tasks: taskPerformance,
      best_task: performance?.best?.id ?? null,
      worst_task: performance?.worst?.id ?? null,
    };
  }

  const digest = text => createHash('sha256').update(text).digest('hex');
  const hashedFiles = ['data', 'lib'].flatMap(dir => readdirSync(join(root, dir))
    .filter(name => name.endsWith(dir === 'data' ? '.json' : '.ts'))
    .map(name => `${dir}/${name}`)).concat('DATA-LICENSE.md').sort();
  const files = Object.fromEntries(hashedFiles.map(file => [file, digest(read(file))]));
  const commit = execFileSync('git', ['-C', root, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
  const sorted = map => Object.fromEntries([...map].sort(([a], [b]) => a.localeCompare(b, 'en')));
  const taskIds = new Set(Object.values(cards).flatMap(card => card.tasks.map(task => task.id)));
  const snapshot = {
    schema_version: 1,
    upstream: {
      name: 'whatmodel', repository: 'https://github.com/jakemanger/whatmodel',
      commit, updated: json('meta').updated, files_sha256: files,
      exporter_sha256: digest(readFileSync(fileURLToPath(import.meta.url), 'utf8')),
      score_scope: 'Full whatmodel dataset and original scoring; selection happens after computation. Compare only within the same task and comparison set.',
    },
    attribution: {
      license: 'CC-BY-SA-4.0', license_url: 'https://creativecommons.org/licenses/by-sa/4.0/',
      credit: 'whatmodel.sh / Merlinsbeard Pty Ltd. Underlying source licences and notices also apply. No endorsement.',
      changes: `Selected ${selected.length} explicit model identities and valid evaluation records; retained original task and overall calculations. No model capability or context overrides.`,
      notice: read('DATA-LICENSE.md'),
      upstream_notices: `https://github.com/jakemanger/whatmodel/tree/${commit}/licenses`,
    },
    benchmarks: sorted(benchmarks),
    task_definitions: TASKS.filter(task => taskIds.has(task.id)),
    sources: sorted(sources),
    models: cards,
    no_public_evaluation_models: Object.fromEntries(NO_PUBLIC_EVALUATIONS.map(name => [name, {
      reason: 'not_in_upstream_catalog', upstream_updated: json('meta').updated,
    }])),
    wire_model_bindings: Object.fromEntries(selected.flatMap(id => (WIRE_NAMES[id] ?? []).map(name => [name, id]))),
    matching_policy: 'Model keys are whatmodel identities. wire_model_bindings contains explicit reviewed wire identifiers only; no fuzzy profile, floating alias, gateway-prefix or private-endpoint inference.',
  };
  const text = JSON.stringify(snapshot, null, 2) + '\n';
  // No draft research paths, credentials, or local checkout paths in the export.
  if (/\/(?:Users|private|tmp)\//.test(text)) throw new Error('Private filesystem path in snapshot');
  const target = resolve(output);
  mkdirSync(dirname(target), { recursive: true });
  const temporary = `${target}.${process.pid}.tmp`;
  try {
    writeFileSync(temporary, text, { flag: 'wx' });
    renameSync(temporary, target);
  } finally {
    rmSync(temporary, { force: true });
  }
  return snapshot;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const args = process.argv.slice(2);
  const take = flag => {
    const i = args.indexOf(flag);
    if (i < 0 || !args[i + 1] || args[i + 1].startsWith('--')) throw new Error(`Missing ${flag}`);
    return args[i + 1];
  };
  try {
    const snapshot = await exportCards(take('--whatmodel'), take('--output'));
    console.log(`Exported ${Object.keys(snapshot.models).length} model cards.`);
  } catch (error) {
    console.error(error.message);
    process.exitCode = 1;
  }
}
