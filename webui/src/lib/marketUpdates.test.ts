import { test } from 'node:test';
import assert from 'node:assert';
import { compareVersions, upgradeFrom } from './marketUpdates.ts';

test('dotted numeric versions compare by number, not by text', () => {
  // "0.9.10" 与 "0.9.9"：按字典序 10 < 9，按数值 10 > 9 —— 这里必须是后者。
  assert.equal(compareVersions('0.9.10', '0.9.9'), 1);
  assert.equal(compareVersions('3.12.10', '3.12.2'), 1);
  assert.equal(compareVersions('1.10.0', '1.9.0'), 1);
  assert.equal(compareVersions('3.12.2', '3.12.10'), -1);
  assert.equal(compareVersions('2.1.1', '2.1.1'), 0);
});

test('date-like versions compare through their segments', () => {
  // quickjs 的版本是 2025-09-13 这种，`-` 与 `.` 都当分隔符。
  assert.equal(compareVersions('2025-09-13', '2025-08-30'), 1);
  assert.equal(compareVersions('2025-09-13', '2025-09-13'), 0);
  assert.equal(compareVersions('2025-09-13', '2024-12-31'), 1);
});

test('missing segments count as zero', () => {
  assert.equal(compareVersions('1.0', '1.0.0'), 0);
  assert.equal(compareVersions('1.0.1', '1.0'), 1);
});

test('only a newer market version counts as an upgrade', () => {
  assert.equal(upgradeFrom('1.0.0', '1.1.0'), '1.1.0');
  assert.equal(upgradeFrom('1.1.0', '1.1.0'), null);
  // 装了更新的版本（本地开发构建）时不该提示“升级”到更旧的市场版本。
  assert.equal(upgradeFrom('1.2.0', '1.1.0'), null);
  assert.equal(upgradeFrom(null, '1.1.0'), null);
  assert.equal(upgradeFrom('1.0.0', ''), null);
});
