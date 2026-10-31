// The plugin market.
//
// The package page lists what is on the device; this lists what the project
// publishes. The split matters: a marketplace entry has to be browsable without
// downloading 25 MB, so the repository ships an index (`market.json`) that names
// each plugin's category, size and hash — the cards below render from that
// alone.
//
// Grouping is by category and the sidebar is the filter, because the question a
// person arrives with is "is there anything for Python", not "what is the name
// of the thing I want". Pagination is client-side (12 per page): the whole index
// is a few kilobytes, and paging on the server would only add a round-trip.

import { useEffect, useMemo, useState } from 'preact/hooks';
import {
  installPackageStreaming,
  listMarket,
  removePackage,
  MarketIndex,
  MarketPlugin,
  MarketPluginVersion,
  setMarketBase,
  PackageProgress,
  PackageRecord,
} from '../api';
import { useT } from '../settings';
import { clampPage, pageCount, pageSlice } from '../lib/marketPaging';
import { marketInstallSource } from '../lib/marketSource';
import { filterPlugins, groupByCategory, installedVersionOf } from '../lib/marketGroups';
import { upgradeFrom } from '../lib/marketUpdates';
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
} from '../lib/marketView';
import { PluginCard } from './PluginCard';

/** One page of cards. Twelve is three rows of four on a wide pane, six rows of
 *  two on the phone — either way a full page fits without scrolling far. */
const PAGE_SIZE = 12;

function formatSize(bytes: number): string {
  if (bytes >= 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  if (bytes >= 1024) return `${Math.round(bytes / 1024)} KB`;
  return `${bytes} B`;
}

interface PackageMarketProps {
  /** Installed packages, so a card can say "already have it" without a second fetch. */
  installed: PackageRecord[];
  /** Called after an install or uninstall, so the package list refreshes too. */
  onChanged: () => void | Promise<void>;
  /** Something else is busy (another install/removal); market actions wait. */
  disabled?: boolean;
}

export function PackageMarket({ installed, onChanged, disabled = false }: PackageMarketProps) {
  const t = useT();
  const [market, setMarket] = useState<MarketIndex | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [category, setCategory] = useState('all');
  const [query, setQuery] = useState('');
  const [page, setPage] = useState(0);
  /** 市场源编辑：值、是否展开、是否正在保存。 */
  const [baseInput, setBaseInput] = useState('');
  const [baseEditing, setBaseEditing] = useState(false);
  const [baseSaving, setBaseSaving] = useState(false);

  /** 展开卡片里选了哪个版本去装；缺省是最新（清单顶层那个）。 */
  const [selectedVersion, setSelectedVersion] = useState<Record<string, string>>({});

  /** 卡片级折叠：默认全折叠，一行一个插件，点开看摘要与操作。 */
  const [openCards, setOpenCards] = useState<Record<string, boolean>>({});
  function toggleCard(name: string) {
    setOpenCards((current) => toggleOpenCard(current, name));
  }

  /** Categories the person folded shut; empty means all expanded. */
  const [collapsed, setCollapsed] = useState<Record<string, boolean>>({});
  const [busyName, setBusyName] = useState('');
  const [progress, setProgress] = useState<PackageProgress | null>(null);

  async function load(refresh: boolean) {
    setLoading(true);
    setError(null);
    try {
      const index = await listMarket(refresh);
      setMarket(index);
      if (!index.success) setError(index.error ?? t('market.unavailable'));
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : String(failure));
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => {
    void load(false);
  }, []);

  const order = useMemo(() => {
    const map = new Map<string, number>();
    (market?.categories ?? []).forEach((entry, index) => map.set(entry.id, index));
    return map;
  }, [market]);

  const filtered = useMemo(
    () => filterPlugins(market?.plugins ?? [], category, query),
    [market, category, query],
  );

  // 分页计算在 marketPaging.ts 里，带边界用例（筛完变短时不能停在空页）。
  const pages = pageCount(filtered.length, PAGE_SIZE);
  const currentPage = clampPage(page, filtered.length, PAGE_SIZE);
  const pageItems = pageSlice(filtered, currentPage, PAGE_SIZE);

  // 清单重新拉回来后可能变短，state 里存的页号会停在一个已经不存在的页上。
  // 显示本身靠 clampPage 兜住了，但 state 也要跟着夹回去：否则清单再变长时视图
  // 会自己跳回那一页，人并没有翻过。
  // 市场源读不出来时不清：那次失败不代表清单真的变短了，不该把人翻到的页数抹掉。
  useEffect(() => {
    if (!market?.success) return;
    setPage((current) => clampPage(current, filtered.length, PAGE_SIZE));
  }, [market?.success, filtered.length]);

  /** 当前页按分类分组，顺序跟仓库定义一致。 */
  const groups = useMemo(
    () => groupByCategory(pageItems, market?.categories ?? []),
    [pageItems, market],
  );

  function counts(id: string): number {
    // 侧栏计数用过滤前的全量清单：搜索词不该改变「这个分类一共几个」。
    return categoryCount(market?.plugins ?? [], id);
  }

  function installedVersion(name: string): string | null {
    return installedVersionOf(installed, name);
  }

  async function install(
    plugin: MarketPlugin,
    target: MarketPlugin | MarketPluginVersion = plugin,
  ) {
    if (disabled || busyName) return;
    setBusyName(plugin.name);
    setError(null);
    setNotice(null);
    setProgress({ type: 'progress', stage: 'download', received: 0 });
    try {
      // 选源与校验值由 marketSource 决定：拿不到可校验的来源就不装。
      const source = marketInstallSource(target);
      if (!source) {
        setError(t('market.noArtifact', { name: plugin.name }));
        return;
      }
      const record = await installPackageStreaming(source, (event) => {
        if (event.type === 'progress') setProgress(event);
      });
      setNotice(t('market.installed', { name: record.name, version: record.version }));
      await onChanged();
      // The index carries `installed`, so re-read it rather than guess.
      await load(false);
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : String(failure));
    } finally {
      setBusyName('');
      setProgress(null);
    }
  }

  /** 把市场源写进 config.toml，然后按新地址重新拉清单。 */
  async function saveBase() {
    setBaseSaving(true);
    setError(null);
    setNotice(null);
    try {
      const result = await setMarketBase(baseInput.trim());
      if (!result.success) {
        setError(result.error ?? t('market.unavailable'));
        return;
      }
      setBaseEditing(false);
      setNotice(t('market.baseSaved'));
      await load(true);
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : String(failure));
    } finally {
      setBaseSaving(false);
    }
  }

  async function uninstall(plugin: MarketPlugin) {
    if (disabled || busyName) return;
    setBusyName(plugin.name);
    setError(null);
    setNotice(null);
    try {
      const removed = await removePackage(plugin.name);
      setNotice(
        removed
          ? t('market.removed', { name: plugin.name })
          : t('pkg.notInstalled', { name: plugin.name }),
      );
      await onChanged();
      await load(false);
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : String(failure));
    } finally {
      setBusyName('');
    }
  }

  const percent =
    progress && typeof progress.total === 'number' && progress.total > 0
      ? Math.min(100, Math.round(((progress.received ?? 0) / progress.total) * 100))
      : null;

  return (
    <section class="pkg-section market">
      <div class="pkg-section-head">
        <h2 class="pkg-section-title">
          {t('market.title')}
          {market?.success && <span class="pkg-count">{counts('all')}</span>}
        </h2>
        <button
          class="btn pkg-section-action"
          disabled={loading || disabled || busyName !== ''}
          onClick={() => void load(true)}
        >
          {loading ? t('market.loading') : t('market.refresh')}
        </button>
        <button
          class="btn pkg-section-action"
          disabled={disabled}
          onClick={() => {
            setBaseInput(market?.base ?? '');
            setBaseEditing((open) => !open);
          }}
        >
          {t('market.edit')}
        </button>
      </div>

      {error && <p class="scm-note scm-error">{error}</p>}
      {!market?.success && market?.hint && <p class="scm-hint mono">{market.hint}</p>}

      {(baseEditing || !market?.success) && (
        <div class="market-base">
          <input
            class="menu-input mono"
            value={baseInput}
            placeholder={t('market.baseHint')}
            disabled={baseSaving}
            onInput={(event) => setBaseInput((event.target as HTMLInputElement).value)}
          />
          <button
            class="btn btn-primary"
            disabled={baseSaving}
            onClick={() => void saveBase()}
          >
            {t('market.save')}
          </button>
          {market?.success && (
            <button class="btn" disabled={baseSaving} onClick={() => setBaseEditing(false)}>
              {t('market.cancel')}
            </button>
          )}
        </div>
      )}
      {notice && <p class="scm-note scm-ok">{notice}</p>}

      {progress && (
        <div class="pkg-installing">
          <div class="pkg-progress-label">
            <span>
              {t('pkg.installing')} {busyName}
            </span>
            <span class="mono">{t(`pkg.stage.${progress.stage ?? 'download'}`)}</span>
          </div>
          {percent !== null ? (
            <div class="pkg-progress-bar">
              <div class="pkg-progress-fill" style={{ width: `${percent}%` }} />
            </div>
          ) : (
            <div class="pkg-progress-bar pkg-progress-indeterminate">
              <div class="pkg-progress-fill" />
            </div>
          )}
          <div class="pkg-progress-meta">
            {percent !== null && <span>{percent}%</span>}
            {typeof progress.files === 'number' && (
              <span>{t('pkg.progressFiles', { files: progress.files })}</span>
            )}
          </div>
        </div>
      )}

      {market?.success && (
        <div class="market-body">
          <aside class="market-sidebar" aria-label={t('market.categories')}>
            <button
              class={
                category === 'all' ? 'market-cat market-cat-active' : 'market-cat'
              }
              onClick={() => {
                setCategory('all');
                setPage(0);
              }}
            >
              <span class="market-cat-title">{t('market.all')}</span>
              <span class="market-cat-count">{counts('all')}</span>
            </button>
            {(market.categories ?? []).map((entry) => (
              <button
                key={entry.id}
                class={
                  category === entry.id ? 'market-cat market-cat-active' : 'market-cat'
                }
                onClick={() => {
                  setCategory(entry.id);
                  setPage(0);
                }}
              >
                <span class="market-cat-title">{entry.title}</span>
                <span class="market-cat-count">{counts(entry.id)}</span>
              </button>
            ))}
            <input
              class="menu-input market-search"
              value={query}
              placeholder={t('market.search')}
              onInput={(event) => {
                setQuery((event.target as HTMLInputElement).value);
                setPage(0);
              }}
            />
          </aside>

          <div class="market-list">
            {groups.length === 0 && <p class="scm-hint">{t('market.noMatch')}</p>}
            {groups.map((group) => {
              const folded = isGroupCollapsed(collapsed, group.id);
              return (
                <div class="market-group" key={group.id}>
                  <button
                    class="market-group-head"
                    aria-expanded={!folded}
                    onClick={() =>
                      setCollapsed((previous) => toggleGroupCollapsed(previous, group.id))
                    }
                  >
                    <span class={folded ? 'market-caret' : 'market-caret market-caret-open'}>
                      ▸
                    </span>
                    <span class="market-group-title">{group.title}</span>
                    <span class="pkg-count">{group.plugins.length}</span>
                  </button>
                  {!folded && (
                    <div class="market-cards">
                      {group.plugins.map((plugin) => {
                        const have = installedVersion(plugin.name);
                        // 装了旧版就提示可升级；同版本或本地更新的版本不打扰。
                        const upgrade = upgradeFrom(have, plugin.version);
                        // 历史版本：清单里有才显示；顶层那个是最新版。
                        // 显式标注：`plugin.versions ?? []` 的类型是 `MarketPluginVersion[]
                        // | never[]`，交给泛型推断会多绕一圈，不如写死。
                        const versions: MarketPluginVersion[] = plugin.versions ?? [];
                        // 装哪一个：选了历史版本就装那个，没选就装清单顶层（最新版）。
                        const chosen = resolveInstallTarget(
                          plugin,
                          versions,
                          selectedVersion[plugin.name],
                        );
                        return (
                          <PluginCard
                            key={plugin.name}
                            name={plugin.name}
                            meta={formatSize(plugin.size_bytes)}
                            dot={have ? 'ok' : undefined}
                            details={
                              <>
                                <span class="pkg-card-version">{plugin.version}</span>
                                {have && !upgrade && (
                                  <span class="pkg-badge">
                                    {t('market.have', { version: have })}
                                  </span>
                                )}
                                {upgrade && (
                                  <span class="pkg-badge pkg-badge-update">
                                    {t('market.update', { version: upgrade })}
                                  </span>
                                )}
                              </>
                            }
                            expanded={isCardOpen(openCards, plugin.name)}
                            onToggle={() => toggleCard(plugin.name)}
                          >
                            <div class="pkg-card-desc">{plugin.summary}</div>
                            {showVersionPicker(versions) && (
                              <div class="market-versions">
                                <span class="market-versions-label">{t('market.version')}</span>
                                {versions.map((entry) => (
                                  <button
                                    key={entry.version}
                                    class={
                                      isChosenVersion(chosen, entry)
                                        ? 'pkg-chip pkg-chip-selected'
                                        : 'pkg-chip'
                                    }
                                    disabled={disabled || busyName !== ''}
                                    onClick={() =>
                                      setSelectedVersion((current) => ({
                                        ...current,
                                        [plugin.name]: entry.version,
                                      }))
                                    }
                                  >
                                    <span class="pkg-chip-version">{entry.version}</span>
                                    <span class="pkg-chip-size">
                                      {formatSize(entry.size_bytes)}
                                    </span>
                                  </button>
                                ))}
                              </div>
                            )}
                            {plugin.tags.length > 0 && (
                              <div class="market-tags">
                                {plugin.tags.map((tag) => (
                                  <span class="market-tag" key={tag}>
                                    {tag}
                                  </span>
                                ))}
                              </div>
                            )}
                            <div class="pkg-card-actions">
                              {have ? (
                                <>
                                  {upgrade && (
                                    <button
                                      class="btn btn-primary"
                                      disabled={disabled || busyName !== ''}
                                      onClick={() => void install(plugin, chosen)}
                                    >
                                      {busyName === plugin.name
                                        ? t('pkg.installing')
                                        : t('market.upgrade')}
                                    </button>
                                  )}
                                  <button
                                    class="btn pkg-danger"
                                    disabled={disabled || busyName !== ''}
                                    onClick={() => void uninstall(plugin)}
                                  >
                                    {busyName === plugin.name ? t('pkg.removing') : t('pkg.remove')}
                                  </button>
                                </>
                              ) : (
                                <button
                                  class="btn btn-primary"
                                  disabled={disabled || busyName !== ''}
                                  onClick={() => void install(plugin, chosen)}
                                >
                                  {busyName === plugin.name
                                    ? t('pkg.installing')
                                    : t('market.install')}
                                </button>
                              )}
                            </div>
                          </PluginCard>
                        );
                      })}
                    </div>
                  )}
                </div>
              );
            })}

            {pages > 1 && (
              <div class="market-pager">
                <button
                  class="btn"
                  disabled={currentPage === 0}
                  onClick={() => setPage(stepPage(currentPage, pages, -1))}
                >
                  {t('market.prev')}
                </button>
                <span class="market-page-info">
                  {t('market.pageOf', { page: currentPage + 1, total: pages })}
                </span>
                <button
                  class="btn"
                  disabled={currentPage >= pages - 1}
                  onClick={() => setPage(stepPage(currentPage, pages, 1))}
                >
                  {t('market.next')}
                </button>
              </div>
            )}
          </div>
        </div>
      )}

      {market?.success && (market.generated_at || market.base) && (
        <p class="scm-hint market-meta">
          {t('market.source', {
            base: market.base ?? '',
            when: market.generated_at ?? '',
          })}
          {market.from_cache ? ` · ${t('market.cached')}` : ''}
        </p>
      )}
    </section>
  );
}
