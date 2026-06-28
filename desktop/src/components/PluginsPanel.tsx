/**
 * 插件面板 — 管理已安装的语法高亮插件
 */
import { useEffect, useState } from 'react';
import {
  getAllPlugins,
  installPlugin,
  uninstallPlugin,
  setPluginEnabled,
} from '../plugins/pluginManager';
import type { PluginPackage } from '../plugins/types';

interface PluginsPanelProps {
  apiBaseUrl: string;
}

export function PluginsPanel({ apiBaseUrl: _apiBaseUrl }: PluginsPanelProps) {
  const [plugins, setPlugins] = useState<PluginPackage[]>([]);
  const [loading, setLoading] = useState(true);
  const [installing, setInstalling] = useState(false);
  const [statusMsg, setStatusMsg] = useState<{ type: 'info' | 'success' | 'error'; text: string } | null>(null);

  async function refresh() {
    setLoading(true);
    setPlugins(getAllPlugins());
    setLoading(false);
  }

  useEffect(() => {
    refresh();
  }, []);

  function showStatus(type: 'info' | 'success' | 'error', text: string) {
    setStatusMsg({ type, text });
    if (type !== 'error') {
      setTimeout(() => setStatusMsg(null), 4000);
    }
  }

  async function handleInstall() {
    setInstalling(true);
    showStatus('info', 'Select a plugin package (.zip)...');
    try {
      const result = await installPlugin();
      if (result.success) {
        showStatus('success', `✅ ${result.plugin?.display || 'Plugin'} installed!`);
        await refresh();
      } else if (result.error !== 'Cancelled') {
        showStatus('error', `❌ ${result.error || 'Install failed'}`);
      } else {
        setStatusMsg(null);
      }
    } catch (err: any) {
      showStatus('error', `❌ ${err.message}`);
    } finally {
      setInstalling(false);
    }
  }

  async function handleUninstall(pkg: PluginPackage) {
    if (pkg.builtin) return;
    setInstalling(true);
    showStatus('info', `Uninstalling ${pkg.display}...`);
    try {
      const result = await uninstallPlugin(pkg.name);
      if (result.success) {
        showStatus('success', `✅ ${pkg.display} uninstalled`);
        await refresh();
      } else {
        showStatus('error', `❌ ${result.error || 'Uninstall failed'}`);
      }
    } catch (err: any) {
      showStatus('error', `❌ ${err.message}`);
    } finally {
      setInstalling(false);
    }
  }

  async function handleToggleDisable(pkg: PluginPackage) {
    const newState = !pkg.disabled;
    showStatus('info', newState ? `Enabling ${pkg.display}...` : `Disabling ${pkg.display}...`);
    try {
      const result = await setPluginEnabled(pkg.name, newState);
      if (result.success) {
        showStatus('success', `${pkg.display} ${newState ? 'enabled' : 'disabled'} (reload file to apply)`);
        await refresh();
      } else {
        showStatus('error', `❌ ${result.error || 'Toggle failed'}`);
      }
    } catch (err: any) {
      showStatus('error', `❌ ${err.message}`);
    }
  }

  const builtinPlugins = plugins.filter((p) => p.builtin);
  const installedPlugins = plugins.filter((p) => !p.builtin);

  return (
    <div className="pl-panel">
      {/* Header */}
      <div className="pl-header">
        <h2 className="pl-title">Plugins</h2>
        <span className="pl-count">{plugins.length} total</span>
      </div>

      {/* Status */}
      {statusMsg && (
        <div className={`pl-status pl-status-${statusMsg.type}`}>
          <span>{statusMsg.text}</span>
          <button className="pl-status-close" onClick={() => setStatusMsg(null)}>✕</button>
        </div>
      )}

      {/* Install bar */}
      <div className="pl-install-bar">
        <button
          className="pl-install-btn"
          onClick={handleInstall}
          disabled={installing}
        >
          {installing ? '⏳ Working...' : '📦 Install Plugin (.zip)'}
        </button>
      </div>

      {/* Built-in plugins */}
      <div className="pl-section">
        <div className="pl-section-header">
          <span className="pl-section-title">Built-in</span>
          <span className="pl-section-badge">{builtinPlugins.length}</span>
        </div>
        {builtinPlugins.length === 0 ? (
          <div className="pl-message">No built-in plugins.</div>
        ) : (
          <div className="pl-list">
            {builtinPlugins.map((pkg) => (
              <div className={'pl-card pl-card-builtin' + (pkg.disabled ? ' pl-card-disabled' : '')} key={pkg.name}>
                <div className="pl-card-icon">{pkg.disabled ? '⚫' : '✨'}</div>
                <div className="pl-card-body">
                  <div className="pl-card-name">{pkg.display}</div>
                  <div className="pl-card-id">{pkg.name}@{pkg.version}</div>
                  <div className="pl-card-langs">
                    <code>{pkg.languages?.join(', ')}</code>
                  </div>
                  {pkg.description && (
                    <div className="pl-card-desc">{pkg.description}</div>
                  )}
                </div>
                <button
                  className={'pl-toggle-btn' + (pkg.disabled ? ' pl-toggle-btn-disabled' : '')}
                  onClick={() => handleToggleDisable(pkg)}
                  title={pkg.disabled ? 'Enable' : 'Disable'}
                >
                  {pkg.disabled ? 'Enable' : 'Disable'}
                </button>
              </div>
            ))}
          </div>
        )}
      </div>

      {/* Installed plugins */}
      <div className="pl-section">
        <div className="pl-section-header">
          <span className="pl-section-title">Installed</span>
          <span className="pl-section-badge">{installedPlugins.length}</span>
        </div>
        {loading ? (
          <div className="pl-message">Loading...</div>
        ) : installedPlugins.length === 0 ? (
          <div className="pl-message pl-empty">
            No plugins installed. Click "Install Plugin" to add one.
          </div>
        ) : (
          <div className="pl-list">
            {installedPlugins.map((pkg) => (
              <div className={'pl-card' + (pkg.disabled ? ' pl-card-disabled' : '')} key={pkg.name}>
                <div className="pl-card-icon">{pkg.disabled ? '⚫' : '📦'}</div>
                <div className="pl-card-body">
                  <div className="pl-card-name">{pkg.display || pkg.name}</div>
                  <div className="pl-card-id">{pkg.name}@{pkg.version}</div>
                  {pkg.languages && (
                    <div className="pl-card-langs">
                      <code>{pkg.languages.join(', ')}</code>
                    </div>
                  )}
                  {pkg.description && (
                    <div className="pl-card-desc">{pkg.description}</div>
                  )}
                </div>
                <button
                  className={'pl-toggle-btn' + (pkg.disabled ? ' pl-toggle-btn-disabled' : '')}
                  onClick={() => handleToggleDisable(pkg)}
                  title={pkg.disabled ? 'Enable' : 'Disable'}
                >
                  {pkg.disabled ? 'Enable' : 'Disable'}
                </button>
                <button
                  className="pl-uninstall-btn"
                  onClick={() => handleUninstall(pkg)}
                  disabled={installing}
                >
                  {installing ? '...' : 'Uninstall'}
                </button>
              </div>
            ))}
          </div>
        )}
      </div>

      {/* Footer */}
      <div className="pl-footer">
        <span>💡 Install .zip packages with</span>
        <code>package.json</code>
        <span>and</span>
        <code>grammar.json</code>
        <span>to add syntax highlighting. Disabled plugins take effect after reload.</span>
      </div>
    </div>
  );
}
