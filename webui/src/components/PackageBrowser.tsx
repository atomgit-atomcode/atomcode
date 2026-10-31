// The package page.
//
// It replaced a modal whose scan result was one `<option>` per archive, so the
// same package at three versions read as three unrelated entries and the list
// told you nothing about which of them you already had installed. Here the scan
// result is grouped by package name, each group shows its versions as chips, and
// the installed state sits on the same card as the versions it describes.
//
// Served from the same embedded webui as everything else — no second UI stack.

import { useEffect, useRef, useState } from 'preact/hooks';
import {
  listPackages,
  listPackageHealth,
  listHelperStatus,
  listLeftovers,
  cleanLeftovers,
  discoverLocalPackages,
  downloadHelperApk,
  listMarket,
  installPackage,
  installPackageStreaming,
  removePackage,
  rollbackPackage,
  HelperStatus,
  LeftoverReport,
  MarketHelper,
  LocalPackage,
  PackageHealth,
  PackageProgress,
  PackageRecord,
} from '../api';
import {
  hasAndroidHost,
  installHelperApk,
  installedProviders,
  sessionsPrivate,
  setSessionsPrivate,
  startExternalDaemon,
  stopExternalDaemon,
} from '../lib/androidHost';
import { useT } from '../settings';
import { PackageMarket } from './PackageMarket';
import { PluginCard } from './PluginCard';

/** One package name and every bundle the scan found for it. */
interface PackageGroup {
  name: string;
  /** Highest version first. */
  bundles: LocalPackage[];
  /** Version currently installed, if any. */
  installed?: string;
}

/** Sort dotted versions numerically, falling back to text for odd ones. */
function compareVersions(left: string, right: string): number {
  const leftParts = left.split(/[.\-+]/);
  const rightParts = right.split(/[.\-+]/);
  const length = Math.max(leftParts.length, rightParts.length);
  for (let index = 0; index < length; index += 1) {
    const leftPart = leftParts[index];
    const rightPart = rightParts[index];
    if (leftPart === undefined) return -1;
    if (rightPart === undefined) return 1;
    const leftNumeric = /^\d+$/.test(leftPart);
    const rightNumeric = /^\d+$/.test(rightPart);
    if (leftNumeric && rightNumeric) {
      const difference = Number(leftPart) - Number(rightPart);
      if (difference !== 0) return difference;
      continue;
    }
    if (leftNumeric !== rightNumeric) {
      // Numeric segments sort above pre-release text (`1.0.0` > `1.0.0-rc1`).
      return leftNumeric ? 1 : -1;
    }
    const comparison = leftPart.localeCompare(rightPart);
    if (comparison !== 0) return comparison;
  }
  return 0;
}

/** How good a filename looks as the canonical bundle for its package.
 *
 *  Scanning finds every copy on the phone — the same release downloaded twice,
 *  a bundle renamed by hand, a build output sitting next to the release. They
 *  are the same version, so they must collapse into one chip; this picks which
 *  path that chip installs from without asking the person to choose between
 *  identical-looking files. */
function canonicalScore(bundle: LocalPackage): number {
  const file = bundle.path.split(/[/\\]/).pop() ?? '';
  let score = 0;
  if (file.includes(bundle.name)) score += 2;
  if (file.includes(bundle.version)) score += 2;
  return score;
}

/** One entry per version: highest canonical score wins, then shortest path. */
function dedupeVersions(bundles: LocalPackage[]): LocalPackage[] {
  const byVersion = new Map<string, LocalPackage>();
  for (const bundle of bundles) {
    const existing = byVersion.get(bundle.version);
    if (!existing) {
      byVersion.set(bundle.version, bundle);
      continue;
    }
    const score = canonicalScore(bundle) - canonicalScore(existing);
    if (score > 0 || (score === 0 && bundle.path.length < existing.path.length)) {
      byVersion.set(bundle.version, bundle);
    }
  }
  return [...byVersion.values()];
}

/** Merge the flat scan result into per-name groups, newest version first. */
function groupPackages(found: LocalPackage[], installed: PackageRecord[]): PackageGroup[] {
  const byName = new Map<string, LocalPackage[]>();
  for (const bundle of found) {
    const bucket = byName.get(bundle.name) ?? [];
    bucket.push(bundle);
    byName.set(bundle.name, bucket);
  }
  const installedVersions = new Map(installed.map((entry) => [entry.name, entry.version]));
  const groups: PackageGroup[] = [];
  for (const [name, bundles] of byName) {
    const versions = dedupeVersions(bundles);
    versions.sort((left, right) => compareVersions(right.version, left.version));
    groups.push({ name, bundles: versions, installed: installedVersions.get(name) });
  }
  groups.sort((left, right) => left.name.localeCompare(right.name));
  return groups;
}

/** Installed state of one provider APK.
 *
 *  `unknown` is its own answer: without the host bridge (a desktop browser) the page
 *  cannot tell, and saying "missing" there would offer an install that leads to a
 *  dead end. */
function providerState(
  provider: MarketHelper,
  installedPackages: string[] | null,
): 'installed' | 'missing' | 'unknown' {
  if (!installedPackages) return 'unknown';
  return installedPackages.includes(provider.package ?? provider.name) ? 'installed' : 'missing';
}

function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return '—';
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(0)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

function errText(failure: unknown): string {
  return failure instanceof Error ? failure.message : String(failure);
}

export interface PackageBrowserProps {
  onClose: () => void;
}

export function PackageBrowser({ onClose }: PackageBrowserProps) {
  const t = useT();
  const [packages, setPackages] = useState<PackageRecord[]>([]);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [source, setSource] = useState('');
  const [sha256, setSha256] = useState('');
  const [progress, setProgress] = useState<PackageProgress | null>(null);
  // Which bundle the bar belongs to: "installing…" alone does not say.
  const [installingName, setInstallingName] = useState('');
  const [health, setHealth] = useState<Record<string, PackageHealth>>({});
  const [helper, setHelper] = useState<HelperStatus>({ installed: false, tools: [] });
  // Downloading the helper APK takes a while (it carries the toolchain), and
  // the install itself is a system prompt — so the button says which half is
  // running.
  const [helperBusy, setHelperBusy] = useState(false);
  const [helperError, setHelperError] = useState<string | null>(null);
  // The provider APKs the market publishes — one row each (`tools`, `zig`, …).
  // Fetched from the market index; empty when the market is unreachable, in which
  // case the single "install the helper" button below is still offered.
  const [providers, setProviders] = useState<MarketHelper[]>([]);
  // Which provider packages the *host* says are installed (null: no bridge, so the
  // page cannot know). The daemon's `<data>/bin` status cannot answer this: a link
  // does not say which APK a tool came from.
  const [installedPackages, setInstalledPackages] = useState<string[] | null>(null);
  const [providerBusy, setProviderBusy] = useState<string | null>(null);
  const [providerError, setProviderError] = useState<string | null>(null);
  // Data a removed runtime left outside its package dir. Reported so an
  // uninstall is not silent about the tens of megabytes it keeps.
  const [leftovers, setLeftovers] = useState<LeftoverReport>({ leftovers: [], totalBytes: 0 });
  const [abort, setAbort] = useState<AbortController | null>(null);
  const [found, setFound] = useState<LocalPackage[]>([]);
  const [discoveryState, setDiscoveryState] = useState<'idle' | 'scanning' | 'done' | 'error'>(
    'scanning',
  );
  const [discoveryStats, setDiscoveryStats] = useState({ directories: 0, archives: 0, skipped: 0 });
  const [discoveryTruncated, setDiscoveryTruncated] = useState(false);
  const [discoveryError, setDiscoveryError] = useState('');
  const [filter, setFilter] = useState('');
  // Which bundle each group has selected. Keyed by package name so a rescan that
  // reorders versions does not silently move the selection to another version.
  const [chosen, setChosen] = useState<Record<string, string>>({});
  /** 哪些卡片被展开。默认全折叠：列表先当列表读，点开再看细节。 */
  const [expandedCards, setExpandedCards] = useState<Record<string, boolean>>({});
  function toggleCard(key: string) {
    setExpandedCards((current) => ({ ...current, [key]: !current[key] }));
  }
  const discoveryAbort = useRef<AbortController | null>(null);

  async function reload() {
    try {
      setPackages(await listPackages());
      setError(null);
    } catch (failure) {
      setError(errText(failure));
    } finally {
      setLoading(false);
    }
  }

  /** Probe the optional helper APK so `make`/`tcc` have a visible source. */
  async function reloadHelper() {
    try {
      setHelper(await listHelperStatus());
    } catch {
      setHelper({ installed: false, tools: [] });
    }
  }

  /**
   * Fetch the helper APK and hand it to the system installer.
   *
   * The daemon does the download (it already knows the market base and checks
   * the published hash); the app then asks Android to install the file.
   *
   * The install happens in the system UI, so its result is not available here:
   * the only truthful signal is the helper status, which is polled until the
   * package appears (or the wait times out, which is the cancelled case).
   */
  async function handleInstallHelper() {
    setHelperBusy(true);
    setHelperError(null);
    try {
      const { path } = await downloadHelperApk();
      if (!installHelperApk(path)) {
        // No host bridge at all: a desktop browser can download but cannot
        // install an APK.
        setHelperError(t('pkg.helperInstallUnavailable'));
        return;
      }
      // The installer is up; wait for the package to appear. Sixty seconds is
      // generous for a 70 MB local install and short enough to give up.
      for (let attempt = 0; attempt < 30; attempt += 1) {
        await new Promise((resolve) => setTimeout(resolve, 2000));
        const status = await listHelperStatus().catch(() => null);
        if (status?.installed) {
          setHelper(status);
          return;
        }
      }
      // Nothing landed: the person cancelled, or the install failed.
      setHelperError(t('pkg.helperInstallUnavailable'));
    } catch (failure) {
      setHelperError(errText(failure));
    } finally {
      setHelperBusy(false);
    }
  }

  /**
   * Ask the market which provider APKs it publishes, and the host which of them
   * are installed.
   *
   * Both halves are best-effort: an offline market leaves `providers` empty (the
   * single-helper button is still correct there), and a desktop page has no bridge,
   * so the per-provider state reads "unknown" rather than "missing".
   */
  async function reloadProviders() {
    setInstalledPackages(installedProviders());
    try {
      const market = await listMarket();
      setProviders(market.helpers ?? []);
    } catch {
      setProviders([]);
    }
  }

  /**
   * Download one provider APK and hand it to the system installer.
   *
   * Same shape as the helper install, but per provider: the market publishes one
   * row per APK and each is its own package, so "installed" is answered per row.
   * A provider is a file holder for a process-model runtime (`make`, `zig`) —
   * Android refuses to execute our data directory, so this is the only route.
   */
  async function handleInstallProvider(provider: MarketHelper) {
    if (providerBusy) return;
    setProviderBusy(provider.name);
    setProviderError(null);
    try {
      const { path } = await downloadHelperApk(provider.name);
      if (!installHelperApk(path)) {
        setProviderError(t('pkg.helperInstallUnavailable'));
        return;
      }
      const packageId = provider.package ?? provider.name;
      // The installer runs in its own UI and its verdict is not delivered back on
      // every OEM, so the only truthful signal is asking again (90 s is generous
      // for a local install of a large APK).
      for (let attempt = 0; attempt < 45; attempt += 1) {
        await new Promise((resolve) => setTimeout(resolve, 2000));
        const now = installedProviders();
        if (now) {
          setInstalledPackages(now);
          if (now.includes(packageId)) {
            void reloadHelper();
            return;
          }
        } else {
          const status = await listHelperStatus().catch(() => null);
          if (status?.installed) {
            setHelper(status);
            return;
          }
        }
      }
      setProviderError(t('pkg.helperInstallUnavailable'));
    } catch (failure) {
      setProviderError(errText(failure));
    } finally {
      setProviderBusy(null);
    }
  }

  /** Probe what a removed runtime left on disk. */
  async function reloadLeftovers() {
    try {
      setLeftovers(await listLeftovers());
    } catch {
      // Diagnostic only: never block the page over a size probe.
      setLeftovers({ leftovers: [], totalBytes: 0 });
    }
  }

  /** Probe installed packages. Separate from `reload` so a slow probe never
   *  hides the list itself, and so it can be re-run on its own. */
  async function reloadHealth() {
    try {
      const entries = await listPackageHealth();
      const next: Record<string, PackageHealth> = {};
      for (const entry of entries) next[entry.name] = entry;
      setHealth(next);
    } catch {
      setHealth({});
    }
  }

  function startDiscovery() {
    discoveryAbort.current?.abort();
    const controller = new AbortController();
    discoveryAbort.current = controller;
    setFound([]);
    setDiscoveryStats({ directories: 0, archives: 0, skipped: 0 });
    setDiscoveryTruncated(false);
    setDiscoveryError('');
    setDiscoveryState('scanning');

    void discoverLocalPackages(
      (event) => {
        if (controller.signal.aborted) return;
        switch (event.type) {
          case 'found':
            setFound((current) =>
              current.some((entry) => entry.path === event.package.path)
                ? current
                : [...current, event.package],
            );
            break;
          case 'progress':
            setDiscoveryStats({
              directories: event.directories,
              archives: event.archives,
              skipped: event.skipped,
            });
            break;
          case 'done':
            setDiscoveryStats({
              directories: event.directories,
              archives: event.archives,
              skipped: event.skipped,
            });
            setDiscoveryTruncated(event.truncated);
            setDiscoveryState('done');
            break;
          case 'error':
            break;
        }
      },
      controller.signal,
    )
      .catch((failure) => {
        if (controller.signal.aborted) return;
        setDiscoveryError(errText(failure));
        setDiscoveryState('error');
      })
      .finally(() => {
        if (discoveryAbort.current === controller) discoveryAbort.current = null;
      });
  }

  function cancelDiscovery() {
    discoveryAbort.current?.abort();
    setDiscoveryState('done');
  }

  useEffect(() => {
    void reload();
    void reloadHealth();
    void reloadHelper();
    void reloadProviders();
    void reloadLeftovers();
    startDiscovery();
    return () => discoveryAbort.current?.abort();
  }, []);

  /** Install one bundle. `path` comes from a group chip; the manual field is the
   *  fallback for a URL or a path the scan did not reach. */
  async function install(path?: string, name?: string) {
    const trimmed = (path ?? source).trim();
    if (!trimmed) {
      setError(t('pkg.sourceRequired'));
      return;
    }
    if (busy) return;
    const controller = new AbortController();
    setAbort(controller);
    setBusy('install');
    setError(null);
    setNotice(null);
    setProgress({ type: 'progress', stage: 'download', received: 0 });
    setInstallingName(name ?? (trimmed.split('/').pop() ?? trimmed));
    try {
      const isUrl = /^https?:\/\//i.test(trimmed);
      const request = {
        url: isUrl ? trimmed : undefined,
        path: isUrl ? undefined : trimmed,
        sha256: sha256.trim() || undefined,
      };
      let sawEvent = false;
      let record: PackageRecord;
      try {
        record = await installPackageStreaming(
          request,
          (event) => {
            sawEvent = true;
            if (event.type === 'progress') setProgress(event);
          },
          controller.signal,
        );
      } catch (failure) {
        if (controller.signal.aborted) {
          setNotice(t('pkg.cancelled'));
          await reload();
          return;
        }
        // Nothing arrived at all: most likely the endpoint is not there (a dev
        // frontend talking to an older daemon). Fall back to the one-shot call.
        if (sawEvent) throw failure;
        record = await installPackage(request);
      }
      setNotice(t('pkg.installed', { name: record.name, version: record.version }));
      if (!path) {
        setSource('');
        setSha256('');
      }
      await reload();
      await reloadHealth();
    } catch (failure) {
      if (controller.signal.aborted) {
        setNotice(t('pkg.cancelled'));
      } else {
        setError(errText(failure));
      }
      await reload();
    } finally {
      setBusy('');
      setProgress(null);
      setAbort(null);
    }
  }

  async function handleRollback(name: string) {
    if (busy) return;
    setBusy(`rollback:${name}`);
    setError(null);
    setNotice(null);
    try {
      const record = await rollbackPackage(name);
      setNotice(t('pkg.rolledBack', { name: record.name, version: record.version }));
      await reload();
      await reloadHealth();
    } catch (failure) {
      setError(errText(failure));
      await reload();
    } finally {
      setBusy('');
    }
  }

  async function handleRemove(name: string) {
    if (busy) return;
    setBusy(`remove:${name}`);
    setError(null);
    setNotice(null);
    try {
      const removed = await removePackage(name);
      setNotice(removed ? t('pkg.removed', { name }) : t('pkg.notInstalled', { name }));
      await reload();
      // Removing the last runtime is exactly when its outside-the-package data
      // becomes unreachable; surface it here instead of on the next visit.
      await reloadLeftovers();
    } catch (failure) {
      setError(errText(failure));
      await reload();
    } finally {
      setBusy('');
    }
  }

  async function handleCleanLeftovers() {
    if (busy) return;
    setBusy('leftovers');
    setError(null);
    setNotice(null);
    try {
      const freed = await cleanLeftovers();
      setNotice(t('pkg.leftoversCleaned', { size: formatBytes(freed) }));
      await reloadLeftovers();
    } catch (failure) {
      setError(errText(failure));
      await reloadLeftovers();
    } finally {
      setBusy('');
      setProgress(null);
      setInstallingName('');
    }
  }

  const disabled = busy !== '';
  // `null` when the total is unknown (e.g. the link stage) — the bar then animates
  // instead of pretending to know a number.
  const progressPercent =
    progress && typeof progress.total === 'number' && progress.total > 0
      ? Math.min(100, Math.round(((progress.received ?? 0) / progress.total) * 100))
      : null;
  const groups = groupPackages(found, packages);
  const needle = filter.trim().toLowerCase();
  const visibleGroups = needle
    ? groups.filter(
        (group) =>
          group.name.toLowerCase().includes(needle) ||
          group.bundles.some((bundle) => bundle.description.toLowerCase().includes(needle)),
      )
    : groups;

  /** The bundle a group is pointed at: the explicit choice, else the newest. */
  const pickOf = (group: PackageGroup): LocalPackage => {
    const explicit = chosen[group.name];
    return group.bundles.find((bundle) => bundle.path === explicit) ?? group.bundles[0];
  };

  const discoveryStatus =
    discoveryState === 'scanning'
      ? t('pkg.localProgress', {
          directories: discoveryStats.directories,
          archives: discoveryStats.archives,
          found: found.length,
        })
      : discoveryState === 'error'
        ? t('pkg.localFailed', { message: discoveryError })
        : discoveryTruncated
          ? t('pkg.localTruncated', { found: found.length })
          : t('pkg.localDone', {
              archives: discoveryStats.archives,
              found: found.length,
              skipped: discoveryStats.skipped,
            });

  return (
    <div class="pkg-page" role="main" aria-label={t('pkg.title')}>
      <header class="pkg-page-header">
        <button class="btn pkg-page-back" onClick={onClose} aria-label={t('pkg.back')}>
          ←
        </button>
        <div class="pkg-page-heading">
          <div class="pkg-page-title">{t('pkg.title')}</div>
          <div class="pkg-page-sub">{t('pkg.subtitle')}</div>
        </div>
      </header>

      <div class="pkg-page-body">
        {error && <p class="scm-note scm-error">{error}</p>}
        {notice && <p class="scm-note scm-ok">{notice}</p>}

        {/* Installing a 24 MB runtime is thousands of files; a button that just
            spins is indistinguishable from a hang. This sits above the lists so
            it is on screen, and shows the stage, a real percentage (compressed
            bytes consumed) and how many files are out. */}
        {progress && (
          <div class="pkg-installing">
            <div class="pkg-progress-label">
              <span>
                {t('pkg.installing')}
                {installingName ? ` ${installingName}` : ''}
              </span>
              <span class="mono">{t(`pkg.stage.${progress.stage ?? 'download'}`)}</span>
            </div>
            {progressPercent !== null ? (
              <div class="pkg-progress-bar">
                <div class="pkg-progress-fill" style={{ width: `${progressPercent}%` }} />
              </div>
            ) : (
              <div class="pkg-progress-bar pkg-progress-indeterminate">
                <div class="pkg-progress-fill" />
              </div>
            )}
            <div class="pkg-progress-meta">
              {progressPercent !== null && <span>{progressPercent}%</span>}
              {typeof progress.files === 'number' && (
                <span>{t('pkg.progressFiles', { files: progress.files })}</span>
              )}
              {typeof progress.total === 'number' && progress.total > 0 && (
                <span class="mono">
                  {formatBytes(progress.received ?? 0)} / {formatBytes(progress.total)}
                </span>
              )}
            </div>
          </div>
        )}

        {/* Where `make`, a C compiler and `zig` come from, otherwise unexplained. */}
        <p class={helper.installed ? 'scm-hint pkg-helper pkg-helper-ok' : 'scm-hint pkg-helper'}>
          {helper.installed
            ? t('pkg.helperInstalled', { tools: helper.tools.join(', ') })
            : t('pkg.helperMissing')}
        </p>
        {helperError && <p class="scm-note scm-error">{helperError}</p>}
        {/* One row per provider APK the market publishes. They are separate apps
            (`tools` for make/tcc, `zig` for zig) because each holds a program that
            Android only runs from an APK's library directory, so "installed" has to
            be answered per package rather than once for all of them. */}
        {providers.length > 0 ? (
          <div class="pkg-providers">
            {providers.map((provider) => {
              const state = providerState(provider, installedPackages);
              const busy = providerBusy === provider.name;
              return (
                <div class="pkg-provider-row" key={provider.name}>
                  <div class="pkg-provider-text">
                    <span class="pkg-provider-title">
                      {provider.title || provider.name}
                      {provider.version && <span class="scm-hint"> {provider.version}</span>}
                    </span>
                    <span class={state === 'installed' ? 'scm-hint pkg-helper-ok' : 'scm-hint'}>
                      {state === 'installed'
                        ? t('pkg.providerInstalled')
                        : state === 'unknown'
                          ? t('pkg.providerUnknown')
                          : t('pkg.providerMissing')}
                    </span>
                  </div>
                  {state !== 'installed' && (
                    <button
                      class="btn"
                      disabled={disabled || busy || providerBusy !== null}
                      onClick={() => void handleInstallProvider(provider)}
                    >
                      {busy
                        ? t('pkg.providerInstalling', { title: provider.title || provider.name })
                        : t('pkg.providerInstall', { title: provider.title || provider.name })}
                    </button>
                  )}
                </div>
              );
            })}
          </div>
        ) : (
          // Old market (or unreachable): keep the single-helper button usable.
          !helper.installed && (
            <div class="pkg-helper-action">
              <button
                class="btn btn-primary"
                disabled={disabled || helperBusy}
                onClick={() => void handleInstallHelper()}
              >
                {helperBusy ? t('pkg.helperInstalling') : t('pkg.helperInstall')}
              </button>
            </div>
          )
        )}
        {providerError && <p class="scm-note scm-error">{providerError}</p>}
        {/* Only the Android app can spawn the bundled CLI; on the desktop there
            is no bridge, so the entry is hidden rather than shown dead. */}
        {helper.installed && hasAndroidHost() && (
          <div class="pkg-helper-action">
            <button
              class="btn"
              disabled={disabled}
              onClick={() => {
                if (!startExternalDaemon()) setHelperError(t('pkg.externalUnavailable'));
              }}
            >
              {t('pkg.externalStart')}
            </button>
            <button
              class="btn"
              disabled={disabled}
              onClick={() => {
                stopExternalDaemon();
              }}
            >
              {t('pkg.externalStop')}
            </button>
          </div>
        )}
        {/* Session log location: shared lets a hand-started daemon read the same
            history; private keeps the whole conversation out of shared storage.
            Applies on the next process start, and says so. */}
        {hasAndroidHost() && (
          <div class="pkg-helper-action">
            <button
              class="btn"
              disabled={disabled}
              onClick={() => {
                const next = !(sessionsPrivate() === true);
                if (!setSessionsPrivate(next)) {
                  setHelperError(t('pkg.externalUnavailable'));
                }
              }}
            >
              {sessionsPrivate() === true ? t('pkg.sessionsToShared') : t('pkg.sessionsToPrivate')}
            </button>
            <span class="scm-hint">
              {sessionsPrivate() === true ? t('pkg.sessionsPrivateNow') : t('pkg.sessionsSharedNow')}
            </span>
          </div>
        )}

        {leftovers.totalBytes > 0 && (
          <section class="pkg-section">
            <div class="pkg-section-head">
              <h2 class="pkg-section-title">
                {t('pkg.leftoversTitle')}
                <span class="pkg-count pkg-count-warn">{formatBytes(leftovers.totalBytes)}</span>
              </h2>
              <button
                class="btn pkg-section-action pkg-danger"
                disabled={disabled}
                onClick={() => void handleCleanLeftovers()}
              >
                {busy === 'leftovers' ? t('pkg.cleaning') : t('pkg.leftoversClean')}
              </button>
            </div>
            <article class="pkg-card pkg-leftover">
              <div class="pkg-card-desc">{t('pkg.leftoversHint')}</div>
              <div class="pkg-leftover-list">
                {leftovers.leftovers.map((entry) => (
                  <div class="pkg-leftover-row" key={entry.name}>
                    <span class="mono">{entry.name}/</span>
                    <span class="pkg-card-size">{formatBytes(entry.bytes)}</span>
                  </div>
                ))}
              </div>
            </article>
          </section>
        )}

        {/* 市场：清单来自独立仓库，卡片只依赖索引，不必先下载包体。 */}
        <PackageMarket
          installed={packages}
          disabled={disabled}
          onChanged={async () => {
            await reload();
            await reloadHealth();
          }}
        />

        <section class="pkg-section">
          <div class="pkg-section-head">
            <h2 class="pkg-section-title">
              {t('pkg.installedTitle')}
              <span class="pkg-count">{packages.length}</span>
            </h2>
            <button class="btn pkg-section-action" disabled={disabled} onClick={() => void reloadHealth()}>
              {t('pkg.recheck')}
            </button>
          </div>

          {loading && <p class="scm-hint">{t('pkg.loading')}</p>}
          {!loading && packages.length === 0 && <p class="scm-hint">{t('pkg.empty')}</p>}

          {packages.map((entry) => {
            const key = `installed:${entry.name}`;
            const folded = expandedCards[key] !== true;
            return (
              <PluginCard
                key={entry.name}
                name={entry.name}
                meta={formatBytes(entry.size_bytes)}
                // 折叠时只留一行名字，但“能不能用”这件事必须一眼看到：
                // 装好了却加载失败，是这个页面最需要提前说清的状态。
                dot={health[entry.name] ? (health[entry.name].ok ? 'ok' : 'bad') : undefined}
                dotTitle={
                  health[entry.name]
                    ? health[entry.name].ok
                      ? t('pkg.healthOk', { detail: health[entry.name].detail })
                      : t('pkg.healthBad', { detail: health[entry.name].detail })
                    : undefined
                }
                details={<span class="pkg-card-version">{entry.version}</span>}
                expanded={!folded}
                onToggle={() => toggleCard(key)}
              >
                {entry.description && <div class="pkg-card-desc">{entry.description}</div>}
                <div class="pkg-commands">
                  {t('pkg.commands')}
                  {entry.commands.map((command) => (
                    <code class="pkg-command" key={command}>
                      {command}
                    </code>
                  ))}
                </div>
                {health[entry.name] && (
                  <div
                    class={
                      health[entry.name].ok
                        ? 'pkg-health pkg-health-ok'
                        : 'pkg-health pkg-health-bad'
                    }
                  >
                    {health[entry.name].ok
                      ? t('pkg.healthOk', { detail: health[entry.name].detail })
                      : t('pkg.healthBad', { detail: health[entry.name].detail })}
                  </div>
                )}
                <div class="pkg-card-actions">
                  {entry.previous_version && (
                    <button
                      class="btn"
                      disabled={disabled}
                      title={t('pkg.rollbackHint', { version: entry.previous_version })}
                      onClick={() => void handleRollback(entry.name)}
                    >
                      {busy === `rollback:${entry.name}`
                        ? t('pkg.rollingBack')
                        : t('pkg.rollback', { version: entry.previous_version })}
                    </button>
                  )}
                  <button
                    class="btn pkg-danger"
                    disabled={disabled}
                    onClick={() => void handleRemove(entry.name)}
                  >
                    {busy === `remove:${entry.name}` ? t('pkg.removing') : t('pkg.remove')}
                  </button>
                </div>
              </PluginCard>
            );
          })}
        </section>

        <section class="pkg-section">
          <div class="pkg-section-head">
            <h2 class="pkg-section-title">
              {t('pkg.availableTitle')}
              <span class="pkg-count">{groups.length}</span>
            </h2>
            <button
              class="btn pkg-section-action"
              disabled={disabled}
              onClick={discoveryState === 'scanning' ? cancelDiscovery : startDiscovery}
            >
              {discoveryState === 'scanning' ? t('pkg.cancelScan') : t('pkg.rescan')}
            </button>
          </div>
          <p class={discoveryState === 'error' ? 'scm-note scm-error' : 'scm-hint'}>
            {discoveryStatus}
          </p>

          {discoveryState === 'scanning' && (
            <div class="pkg-progress">
              <div class="pkg-progress-bar pkg-progress-indeterminate">
                <div class="pkg-progress-fill" />
              </div>
            </div>
          )}

          {groups.length > 0 && (
            <input
              class="menu-input pkg-search"
              value={filter}
              placeholder={t('pkg.searchHint')}
              disabled={disabled}
              onInput={(event) => setFilter((event.target as HTMLInputElement).value)}
            />
          )}

          {visibleGroups.map((group) => {
            const pick = pickOf(group);
            const installing = busy === 'install';
            const key = `local:${group.name}`;
            return (
              <PluginCard
                key={group.name}
                name={group.name}
                meta={formatBytes(pick.size_bytes)}
                details={
                  <>
                    <span class="pkg-card-version">{pick.version}</span>
                    {group.installed ? (
                      <span class="pkg-badge">
                        {t('pkg.installedVersion', { version: group.installed })}
                      </span>
                    ) : (
                      <span class="pkg-group-count">
                        {t('pkg.versionCount', { count: group.bundles.length })}
                      </span>
                    )}
                  </>
                }
                expanded={expandedCards[key] === true}
                onToggle={() => toggleCard(key)}
              >
                {pick.description && <div class="pkg-group-desc">{pick.description}</div>}
                <div class="pkg-chips">
                  {group.bundles.map((bundle) => (
                    <button
                      key={bundle.path}
                      class={
                        bundle.path === pick.path ? 'pkg-chip pkg-chip-selected' : 'pkg-chip'
                      }
                      disabled={disabled}
                      title={bundle.path}
                      onClick={() => setChosen((current) => ({ ...current, [group.name]: bundle.path }))}
                    >
                      <span class="pkg-chip-version">{bundle.version}</span>
                      <span class="pkg-chip-size">{formatBytes(bundle.size_bytes)}</span>
                    </button>
                  ))}
                </div>
                <div class="pkg-group-foot">
                  <span class="pkg-group-path mono" title={pick.path}>
                    {pick.path}
                  </span>
                  {installing && abort ? (
                    <button class="btn btn-primary" onClick={() => abort.abort()}>
                      {t('pkg.cancel')}
                    </button>
                  ) : (
                    <button
                      class="btn btn-primary"
                      disabled={disabled}
                      onClick={() => void install(pick.path, pick.name)}
                    >
                      {t('pkg.installThis', { version: pick.version })}
                    </button>
                  )}
                </div>
              </PluginCard>
            );
          })}

          {needle && visibleGroups.length === 0 && <p class="scm-hint">{t('pkg.noMatch')}</p>}
          {discoveryState === 'done' && groups.length === 0 && (
            <p class="scm-hint">{t('pkg.localEmpty')}</p>
          )}
        </section>

        <details class="pkg-manual">
          <summary>{t('pkg.manual')}</summary>
          <div class="pkg-manual-body">
            <label class="modal-label">{t('pkg.source')}</label>
            <input
              class="menu-input"
              value={source}
              placeholder={t('pkg.sourceHint')}
              disabled={disabled}
              onInput={(event) => setSource((event.target as HTMLInputElement).value)}
            />
            <label class="modal-label">{t('pkg.sha256')}</label>
            <input
              class="menu-input mono"
              value={sha256}
              placeholder={t('pkg.sha256Hint')}
              disabled={disabled}
              onInput={(event) => setSha256((event.target as HTMLInputElement).value)}
            />
            <button class="btn btn-primary" disabled={disabled} onClick={() => void install()}>
              {t('pkg.install')}
            </button>
            <p class="scm-hint">{t('pkg.note')}</p>
          </div>
        </details>
      </div>
    </div>
  );
}