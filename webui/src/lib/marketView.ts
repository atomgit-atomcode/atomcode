// 市场页的「视图决策」：从组件里抽出来，才能对着「点了会怎样」写测试，而不是靠截图。
//
// 组件里原来把这些判断写在 JSX 与 setState 回调中间，于是「选了历史版本到底装哪个产物」
// 「分类计数是过滤前的还是过滤后的」只能靠人点一遍看。这里全是纯函数：进去的是当前状态
// （哪些卡片展开、哪些分类折叠、选了哪个版本、清单里有哪些插件），出来的是下一步该显示什么。

export interface VersionLike {
  version: string;
}

export interface CategoryPluginLike {
  category: string;
}

/** 卡片展开状态：点一下展开，再点一下收起。 */
export function toggleOpenCard(
  open: Record<string, boolean>,
  name: string,
): Record<string, boolean> {
  return { ...open, [name]: !open[name] };
}

/** 卡片是否展开。没记录过的卡片算折叠——市场页一打开是一行一个插件。 */
export function isCardOpen(open: Record<string, boolean>, name: string): boolean {
  return open[name] === true;
}

/** 分类折叠状态：点一下折叠，再点一下展开。 */
export function toggleGroupCollapsed(
  collapsed: Record<string, boolean>,
  id: string,
): Record<string, boolean> {
  return { ...collapsed, [id]: collapsed[id] !== true };
}

/** 分类是否折叠。没记录过的分类算展开——空表表示全展开。 */
export function isGroupCollapsed(collapsed: Record<string, boolean>, id: string): boolean {
  return collapsed[id] === true;
}

/** 侧栏分类计数：`all` 是全量，其余按分类精确匹配。
 *
 *  计数用的是**过滤前**的清单：搜索框里打字不该让侧栏的分类数字跟着变，否则
 *  「这个分类一共几个」与「搜出来几个」会混成同一个数。 */
export function categoryCount(plugins: CategoryPluginLike[], id: string): number {
  if (id === 'all') return plugins.length;
  return plugins.filter((plugin) => plugin.category === id).length;
}

/** 装哪一个产物：选了历史版本就装那个，没选就装清单顶层（最新的那个），
 *  清单里没有版本条目时退回插件本身。
 *
 *  取不到所选版本时不报错而是退回最新版：清单是仓库生成的，可能比页面上的选择旧，
 *  那时装最新版总比装不上强。 */
export function resolveInstallTarget<
  TPlugin extends VersionLike,
  TVersion extends VersionLike,
>(plugin: TPlugin, versions: readonly TVersion[], selected?: string): TPlugin | TVersion {
  const match = versions.find((entry) => entry.version === selected);
  if (match) return match;
  const latest = versions.length > 0 ? versions[versions.length - 1] : undefined;
  return latest ?? plugin;
}

/** 版本标签列表要不要显示：只有一个版本时没什么可挑的，不显示。
 *
 *  没显示的版本也照样能装——默认装清单顶层那个。 */
export function showVersionPicker(versions: readonly VersionLike[]): boolean {
  return versions.length > 1;
}

/** 翻页的落点：next 走到最后一页为止，prev 停在第一页。
 *
 *  这里只管「翻一页」这个动作本身。筛选把列表缩短后，落点可能已经不存在了，
 *  那由组件在列表变短时用 `clampPage` 把页号夹回来。 */
export function stepPage(current: number, pages: number, delta: number): number {
  if (pages <= 1) return 0;
  return Math.min(Math.max(0, current + delta), pages - 1);
}

/** 版本标签是不是当前选中的那个（用来高亮）。
 *
 *  比较的是版本号而不是对象：清单里的版本条目与插件顶层是两个不同的对象，
 *  但人看到的标签是同一个版本号。 */
export function isChosenVersion(chosen: VersionLike, entry: VersionLike): boolean {
  return chosen.version === entry.version;
}
