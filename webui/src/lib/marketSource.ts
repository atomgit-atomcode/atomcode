// What a market card hands to the installer.
//
// Two things can silently go wrong here, so it is a function with tests rather
// than inline code: picking the wrong source (a stale URL when a local bundle is
// right there), and dropping the hash (an unverified install). A market entry
// without a sha256 is refused outright — the repository always publishes one.

export interface MarketInstallSource {
  path?: string;
  url?: string;
  sha256: string;
}

/** The install source for `plugin`, or `null` when the entry is unusable.
 *
 *  A local `download_path` wins: the daemon reads the file directly instead of
 *  fetching it back through its own loopback server. `download_url` covers the
 *  remote market (raw / release / object storage), which the daemon already
 *  knows how to fetch. */
/** 只用到位置与校验值，所以插件条目和历史版本条目都能传进来。 */
export interface InstallCandidate {
  sha256: string;
  download_path?: string | null;
  download_url?: string | null;
}

export function marketInstallSource(plugin: InstallCandidate): MarketInstallSource | null {
  if (!plugin.sha256) return null;
  if (plugin.download_path) return { path: plugin.download_path, sha256: plugin.sha256 };
  if (plugin.download_url) return { url: plugin.download_url, sha256: plugin.sha256 };
  return null;
}
