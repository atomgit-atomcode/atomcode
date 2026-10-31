import { test } from 'node:test';
import assert from 'node:assert';
import { filterPlugins, groupByCategory, installedVersionOf } from './marketGroups.ts';

const plugin = (name: string, category: string, extra: Record<string, unknown> = {}) => ({
  name,
  title: `${name} title`,
  summary: `${name} summary`,
  category,
  tags: [] as string[],
  version: '1.0.0',
  ...extra,
});

test('category filtering keeps only that category', () => {
  const plugins = [plugin('cpython', 'python'), plugin('numpy', 'python'), plugin('node', 'js')];
  assert.deepEqual(
    filterPlugins(plugins, 'python', '').map((item) => item.name),
    ['cpython', 'numpy'],
  );
  assert.equal(filterPlugins(plugins, 'all', '').length, 3);
});

test('search matches name, title, summary and tags', () => {
  const plugins = [
    plugin('cpython', 'python'),
    plugin('svg', 'render', { summary: '把矢量光栅化', tags: ['png'] }),
  ];
  assert.deepEqual(filterPlugins(plugins, 'all', 'svg').map((item) => item.name), ['svg']);
  assert.deepEqual(filterPlugins(plugins, 'all', 'png').map((item) => item.name), ['svg']);
  assert.deepEqual(filterPlugins(plugins, 'all', '矢量').map((item) => item.name), ['svg']);
  assert.equal(filterPlugins(plugins, 'all', '不存在的词').length, 0);
});

test('groups follow the repository category order, unknown categories last', () => {
  const plugins = [
    plugin('zzz', 'unknown'),
    plugin('numpy', 'python'),
    plugin('node', 'js'),
    plugin('cpython', 'python'),
  ];
  const groups = groupByCategory(plugins, [
    { id: 'python', title: 'Python' },
    { id: 'js', title: 'JavaScript' },
  ]);
  assert.deepEqual(groups.map((group) => group.id), ['python', 'js', 'unknown']);
  // 组内按名字排序：别让“最近更新”决定位置，列表要稳定。
  assert.deepEqual(groups[0].plugins.map((item) => item.name), ['cpython', 'numpy']);
  // 未声明的分类显示 id 本身，不至于空白。
  assert.equal(groups[2].title, 'unknown');
});

test('a missing category title falls back to the id', () => {
  const groups = groupByCategory([plugin('lua', 'lua')], [{ id: 'lua' }]);
  assert.equal(groups[0].title, 'lua');
});

test('installed lookup returns the version or null', () => {
  const installed = [{ name: 'numpy', version: '1.26.2' }];
  assert.equal(installedVersionOf(installed, 'numpy'), '1.26.2');
  assert.equal(installedVersionOf(installed, 'svg'), null);
});
