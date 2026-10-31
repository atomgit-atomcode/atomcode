import { test } from 'node:test';
import assert from 'node:assert';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import {
  categoryCount,
  isCardOpen,
  isChosenVersion,
  isGroupCollapsed,
  resolveInstallTarget,
  showVersionPicker,
  stepPage,
  toggleGroupCollapsed,
  toggleOpenCard,
} from './marketView.ts';

const root = process.cwd();
const component = readFileSync(join(root, 'src/components/PackageMarket.tsx'), 'utf8');

/** A market entry, shaped like the ones `listMarket` returns. */
interface PluginLike {
  name: string;
  title: string;
  summary: string;
  category: string;
  tags: string[];
  version: string;
  size_bytes: number;
  sha256: string;
  installed: boolean;
}

const plugin = (name: string, category: string, extra: Partial<PluginLike> = {}): PluginLike => ({
  name,
  title: `${name} title`,
  summary: `${name} summary`,
  category,
  tags: [],
  version: '1.0.0',
  size_bytes: 1024,
  sha256: `sha-${name}`,
  installed: false,
  ...extra,
});

/** A historical version entry of the same plugin. */
interface VersionLike {
  version: string;
  size_bytes: number;
  sha256: string;
}

const version = (v: string, extra: Partial<VersionLike> = {}): VersionLike => ({
  version: v,
  size_bytes: 512,
  sha256: `sha-${v}`,
  ...extra,
});

test('a card that was never touched stays folded until it is clicked', () => {
  assert.equal(isCardOpen({}, 'numpy'), false);
  assert.equal(isCardOpen({ numpy: false }, 'numpy'), false);
});

test('clicking a card opens it and clicking again folds it back', () => {
  const opened = toggleOpenCard({}, 'numpy');
  assert.equal(isCardOpen(opened, 'numpy'), true);

  const closed = toggleOpenCard(opened, 'numpy');
  assert.equal(isCardOpen(closed, 'numpy'), false);
});

test('opening one card leaves the other cards alone', () => {
  const open = toggleOpenCard({ svg: true }, 'numpy');
  assert.equal(isCardOpen(open, 'numpy'), true);
  assert.equal(isCardOpen(open, 'svg'), true);
});

test('a category that was never folded stays expanded', () => {
  assert.equal(isGroupCollapsed({}, 'python'), false);
  assert.equal(isGroupCollapsed({ python: false }, 'python'), false);
});

test('clicking a category head folds it and clicking again unfolds it', () => {
  const folded = toggleGroupCollapsed({}, 'python');
  assert.equal(isGroupCollapsed(folded, 'python'), true);

  const unfolded = toggleGroupCollapsed(folded, 'python');
  assert.equal(isGroupCollapsed(unfolded, 'python'), false);
});

test('folding one category leaves the other categories alone', () => {
  const collapsed = toggleGroupCollapsed({ js: true }, 'python');
  assert.equal(isGroupCollapsed(collapsed, 'python'), true);
  assert.equal(isGroupCollapsed(collapsed, 'js'), true);
});

test('the "all" count is the whole list and a category count is its own plugins', () => {
  const plugins = [plugin('numpy', 'python'), plugin('cpython', 'python'), plugin('node', 'js')];
  assert.equal(categoryCount(plugins, 'all'), 3);
  assert.equal(categoryCount(plugins, 'python'), 2);
  assert.equal(categoryCount(plugins, 'js'), 1);
  assert.equal(categoryCount(plugins, 'rust'), 0);
});

test('an empty market counts zero in every category', () => {
  assert.equal(categoryCount([], 'all'), 0);
  assert.equal(categoryCount([], 'python'), 0);
});

test('the chosen version is the one that gets installed', () => {
  const target = resolveInstallTarget(
    plugin('numpy', 'python'),
    [version('1.0.0'), version('0.9.0')],
    '0.9.0',
  );
  assert.equal(target.version, '0.9.0');
  // 装的是那个历史版本的产物（校验值也是它的），不是插件顶层的。
  assert.equal((target as VersionLike).sha256, 'sha-0.9.0');
});

test('without a chosen version the newest entry is installed', () => {
  // 清单里最新的那个在最后：顶层那个（plugin 本身）就是最新。
  const target = resolveInstallTarget(
    plugin('numpy', 'python'),
    [version('1.0.0'), version('2.0.0')],
  );
  assert.equal(target.version, '2.0.0');
});

test('a plugin without any version entry falls back to the plugin itself', () => {
  const target = resolveInstallTarget(plugin('numpy', 'python'), [] as VersionLike[]);
  assert.equal(target.version, '1.0.0');
  assert.equal(target.sha256, 'sha-numpy');
});

test('a chosen version that the index no longer lists falls back to the newest one', () => {
  // 清单是仓库生成的，可能比页面上记住的选择旧；这时装最新版而不是装不上。
  const target = resolveInstallTarget(
    plugin('numpy', 'python'),
    [version('1.0.0'), version('2.0.0')],
    '0.5.0',
  );
  assert.equal(target.version, '2.0.0');
});

test('the highlighted version chip is the one that would be installed', () => {
  const chosen = resolveInstallTarget(
    plugin('numpy', 'python'),
    [version('1.0.0'), version('0.9.0')],
    '0.9.0',
  );
  assert.equal(isChosenVersion(chosen, version('0.9.0')), true);
  assert.equal(isChosenVersion(chosen, version('1.0.0')), false);
});

test('the version picker only shows when there is something to choose between', () => {
  assert.equal(showVersionPicker([]), false);
  assert.equal(showVersionPicker([version('1.0.0')]), false);
  assert.equal(showVersionPicker([version('1.0.0'), version('2.0.0')]), true);
});

test('next stops on the last page and prev stops on the first', () => {
  assert.equal(stepPage(0, 3, 1), 1);
  assert.equal(stepPage(1, 3, 1), 2);
  assert.equal(stepPage(2, 3, 1), 2);
  assert.equal(stepPage(1, 3, -1), 0);
  assert.equal(stepPage(0, 3, -1), 0);
});

test('a single page never moves', () => {
  assert.equal(stepPage(0, 1, 1), 0);
  assert.equal(stepPage(0, 1, -1), 0);
});

test('the component installs the version the person picked', () => {
  // 接线：解析出来的 chosen 必须原样交给 install，而不是重新取一次最新版。
  assert.match(component, /const chosen = resolveInstallTarget\(/);
  assert.match(component, /selectedVersion\[plugin\.name\]/);
  assert.match(component, /install\(plugin, chosen\)/);
  // 高亮的标签也必须是同一个 chosen。
  assert.match(component, /isChosenVersion\(chosen, entry\)/);
});

test('the component counts categories from the unfiltered market list', () => {
  // 接线：计数用的是 market.plugins，不是筛选后的 filtered —— 搜索词不该改变侧栏数字。
  assert.match(component, /return categoryCount\(market\?\.plugins \?\? \[\], id\)/);
  assert.doesNotMatch(component, /categoryCount\(filtered/);
});

test('the component folds cards and categories through the shared helpers', () => {
  assert.match(component, /toggleOpenCard\(current, name\)/);
  assert.match(component, /expanded=\{isCardOpen\(openCards, plugin\.name\)\}/);
  assert.match(component, /toggleGroupCollapsed\(previous, group\.id\)/);
  assert.match(component, /const folded = isGroupCollapsed\(collapsed, group\.id\)/);
});

test('the component pulls the stored page back into range when the list shrinks', () => {
  // 接线：筛选把列表缩短后，state 里的页号也要夹回存在的页，不能停在空页上。
  assert.match(
    component,
    /if \(!market\?\.success\) return;\s*setPage\(\(current\) => clampPage\(current, filtered\.length, PAGE_SIZE\)\)/,
  );
});
