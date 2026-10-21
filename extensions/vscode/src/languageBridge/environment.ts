/**
 * Storage schemes that still name a directory on this host's own disk.
 *
 * The bridge publishes its connection file through Node `fs` and the AtomCode client
 * reads it from the same host, so the only real precondition is that the extension
 * host owns a local filesystem. Desktop VS Code reports `vscode-userdata` for
 * extension storage in some versions (observed on 1.133.0, where `globalStorageUri`
 * is `vscode-userdata:/c%3A/Users/<user>/...`); that scheme maps to the same local
 * user-data directory, so treating it as "not a filesystem" would disable the bridge
 * on those hosts. Anything else (web or virtual file systems) does not qualify.
 */
export function isLocalStorageScheme(scheme: string): boolean {
  return scheme === 'file' || scheme === 'vscode-userdata';
}
