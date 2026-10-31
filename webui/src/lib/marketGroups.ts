// 市场的筛选与分组：从组件里抽出来，才能对着「点了分类会怎样」写测试，
// 而不是靠截图人工看。

export interface CategoryLike {
  id: string;
  title?: string;
}

export interface PluginLike {
  name: string;
  title: string;
  summary: string;
  category: string;
  tags: string[];
  version: string;
}

export interface InstalledLike {
  name: string;
  version: string;
}

/** 按分类与搜索词筛。`category === 'all'` 表示不筛分类。 */
export function filterPlugins<T extends PluginLike>(
  plugins: T[],
  category: string,
  query: string,
): T[] {
  const needle = query.trim().toLowerCase();
  return plugins.filter((plugin) => {
    if (category !== 'all' && plugin.category !== category) return false;
    if (!needle) return true;
    return (
      plugin.name.toLowerCase().includes(needle) ||
      plugin.title.toLowerCase().includes(needle) ||
      plugin.summary.toLowerCase().includes(needle) ||
      plugin.tags.some((tag) => tag.toLowerCase().includes(needle))
    );
  });
}

/** 按分类分组，顺序跟仓库定义的分类一致；未声明的分类排在末尾、按 id 排。 */
export function groupByCategory<T extends PluginLike>(
  plugins: T[],
  categories: CategoryLike[],
): Array<{ id: string; title: string; plugins: T[] }> {
  const buckets = new Map<string, T[]>();
  for (const plugin of plugins) {
    const list = buckets.get(plugin.category) ?? [];
    list.push(plugin);
    buckets.set(plugin.category, list);
  }
  const order = new Map(categories.map((entry, index) => [entry.id, index]));
  const titles = new Map(categories.map((entry) => [entry.id, entry.title ?? entry.id]));
  return [...buckets.entries()]
    .map(([id, items]) => ({
      id,
      title: titles.get(id) ?? id,
      plugins: items.slice().sort((left, right) => left.name.localeCompare(right.name)),
    }))
    .sort((left, right) => {
      const lo = order.get(left.id);
      const ro = order.get(right.id);
      if (lo !== undefined && ro !== undefined) return lo - ro;
      if (lo !== undefined) return -1;
      if (ro !== undefined) return 1;
      return left.id.localeCompare(right.id);
    });
}

/** 已装包的版本，或 null。 */
export function installedVersionOf(installed: InstalledLike[], name: string): string | null {
  return installed.find((record) => record.name === name)?.version ?? null;
}
