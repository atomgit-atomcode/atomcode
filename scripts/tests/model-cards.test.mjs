import { createHash } from 'node:crypto';
import assert from 'node:assert/strict';
import { readFileSync, mkdtempSync, mkdirSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';
import { pathToFileURL } from 'node:url';
import { COMMON_MODELS, exportCards } from '../export-model-cards.mjs';

const snapshot = JSON.parse(readFileSync(new URL('../../data/model-cards.json', import.meta.url), 'utf8'));
test('common snapshot has complete source references and no capability overrides or guessed aliases', () => {
  assert.equal(snapshot.schema_version, 1);
  const exporter = readFileSync(new URL('../export-model-cards.mjs', import.meta.url), 'utf8');
  assert.equal(snapshot.upstream.exporter_sha256, createHash('sha256').update(exporter).digest('hex'),
    'regenerate the snapshot after changing its export rules or wire bindings');
  assert.deepEqual(Object.keys(snapshot.models), COMMON_MODELS);
  assert.match(snapshot.upstream.commit, /^[a-f0-9]{40}$/);
  assert.equal(snapshot.attribution.license, 'CC-BY-SA-4.0');
  for (const [id, card] of Object.entries(snapshot.models)) {
    assert.equal(card.id, id);
    for (const forbidden of ['context_window', 'context_k', 'supports_vision', 'supports_tools', 'aliases']) {
      assert.equal(Object.hasOwn(card, forbidden), false);
    }
    assert.equal(card.valid_benchmark_count, new Set(card.evaluations.map(row => row.bench)).size);
    for (const row of card.evaluations) {
      assert.equal(row.model, id);
      assert.ok(!row.invalid);
      assert.ok(Number.isFinite(row.score));
      assert.ok(snapshot.benchmarks[row.bench]);
      assert.equal(snapshot.sources[row.source_id].verdict, 'allowed');
      assert.ok(['http:', 'https:'].includes(new URL(row.source).protocol));
    }
    for (const task of card.tasks) {
      assert.ok(snapshot.task_definitions.some(def => def.id === task.id));
      if (task.rank !== null) {
        assert.ok(task.total >= task.rank);
        assert.ok(task.comparison_set?.id);
      }
    }
    assert.ok(!card.best_task || card.tasks.some(t => t.id === card.best_task));
    assert.ok(!card.worst_task || card.tasks.some(t => t.id === card.worst_task));
  }
  assert.equal(snapshot.no_public_evaluation_models['lumen-alpha'].reason, 'not_in_upstream_catalog');
  assert.equal(snapshot.models['lumen-alpha'], undefined);
  assert.equal(snapshot.wire_model_bindings['lumen-alpha'], undefined);
  for (const name of Object.keys(snapshot.no_public_evaluation_models)) {
    assert.equal(snapshot.wire_model_bindings[name], undefined);
    assert.equal(snapshot.models[name], undefined);
  }
  for (const [wire, id] of Object.entries(snapshot.wire_model_bindings)) {
    assert.equal(wire, wire.trim().toLowerCase());
    assert.ok(snapshot.models[id]);
  }
  for (const alias of ['deepseek-flash', 'deepseek-v4-flash', 'ark-code-latest', 'glm5.3-flash-pro']) {
    assert.equal(snapshot.wire_model_bindings[alias], undefined);
  }
  assert.equal(snapshot.models['minimax-m3-1-flash'].valid_benchmark_count, 2);
  assert.doesNotMatch(JSON.stringify(snapshot), /\/(?:Users|private|tmp)\//);
});

test('an upstream appearance of a known no-data model requires review before replacing output', async () => {
  const root = mkdtempSync(join(tmpdir(), 'atomcode-new-model-evidence-'));
  const output = join(root, 'cards.json');
  try {
    mkdirSync(join(root, 'data'));
    writeFileSync(join(root, 'data/models.json'), JSON.stringify([{ id: 'lumen-alpha', name: 'Lumen Alpha' }]));
    writeFileSync(output, 'previous valid snapshot');
    await assert.rejects(exportCards(root, output), /Previously unlisted model now exists upstream/);
    assert.equal(readFileSync(output, 'utf8'), 'previous valid snapshot');
  } finally { rmSync(root, { recursive: true, force: true }); }
});

test('live export preserves upstream calculations, is deterministic, and rejects unknown IDs without overwriting', {
  skip: !process.env.WHATMODEL_CHECKOUT && 'Set WHATMODEL_CHECKOUT for the live upstream integration test',
}, async () => {
  const root = process.env.WHATMODEL_CHECKOUT;
  const temporary = mkdtempSync(join(tmpdir(), 'atomcode-model-cards-'));
  const output = join(temporary, 'cards.json');
  try {
    const exported = await exportCards(root, output);
    const first = readFileSync(output, 'utf8');
    await exportCards(root, output);
    assert.equal(readFileSync(output, 'utf8'), first);
    const { strengths } = await import(pathToFileURL(join(root, 'lib/strengths.ts')));
    const sourceResults = JSON.parse(readFileSync(join(root, 'data/results.json'), 'utf8'));
    for (const [id, card] of Object.entries(exported.models)) {
      const original = strengths(id);
      assert.equal(card.best_task, original?.best?.id ?? null);
      assert.equal(card.worst_task, original?.worst?.id ?? null);
      assert.deepEqual(card.tasks.map(({ id, rank, total, g, pm, n }) => ({ id, rank, total, g, pm, n })),
        (original?.tasks ?? []).map(({ id, rank, total, g, pm, n }) => ({ id, rank, total, g, pm, n })));
      assert.deepEqual(card.evaluations.map(({ model, bench, score, source }) => ({ model, bench, score, source })),
        sourceResults.filter(row => row.model === id && !row.invalid)
          .map(({ model, bench, score, source }) => ({ model, bench, score, source })));
    }
    // Exporting fewer cards must not recalculate ranks against a smaller population.
    const subset = await exportCards(root, output, COMMON_MODELS.slice(0, 2));
    for (const id of COMMON_MODELS.slice(0, 2)) assert.deepEqual(subset.models[id], exported.models[id]);
    writeFileSync(output, 'existing output');
    await assert.rejects(exportCards(root, output, ['unverified-channel-alias']), /Unknown whatmodel model/);
    assert.equal(readFileSync(output, 'utf8'), 'existing output');
    await assert.rejects(exportCards(root, output, [COMMON_MODELS[0], COMMON_MODELS[0]]), /Duplicate/);
    assert.equal(readFileSync(output, 'utf8'), 'existing output');
  } finally {
    rmSync(temporary, { recursive: true, force: true });
  }
});
