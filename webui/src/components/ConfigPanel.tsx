// Task 15 — Config panel (read-only modal)

import { useEffect, useState } from 'preact/hooks';
import { getConfig, ConfigInfo } from '../api';

interface ConfigPanelProps {
  onClose: () => void;
}

export function ConfigPanel({ onClose }: ConfigPanelProps) {
  const [config, setConfig] = useState<ConfigInfo | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    getConfig()
      .then(setConfig)
      .catch((e: unknown) =>
        setError(e instanceof Error ? e.message : String(e)),
      );
  }, []);

  return (
    <div
      class="modal-overlay"
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div class="modal-card">
        <div class="modal-header">
          <span>⚙</span>
          <h3>配置</h3>
          <span class="modal-sub">只读</span>
          <button class="ghost-btn modal-close" onClick={onClose} aria-label="关闭">
            ×
          </button>
        </div>

        <div class="modal-body">
          {error && <div class="modal-error">加载失败: {error}</div>}
          {!config && !error && <div class="modal-loading">加载中…</div>}
          {config && (
            <>
              <div class="field-group">
                <Row label="默认 Provider" value={config.default_provider} />
                {config.default_workdir && (
                  <Row label="默认工作目录" value={config.default_workdir} mono />
                )}
                <Row label="配置文件" value={config.path} mono />
              </div>

              <div class="field-group">
                <span class="modal-label">Providers ({config.providers.length})</span>
                {config.providers.map((p) => (
                  <div
                    key={p.name}
                    class={'provider-card' + (p.is_default ? ' default' : '')}
                  >
                    <div class="provider-card-head">
                      <span class="provider-name">{p.name}</span>
                      {p.is_default && <span class="provider-default-badge">默认</span>}
                      <span class="provider-type">{p.type}</span>
                    </div>
                    <div class="provider-card-body">
                      <div>
                        <span class="pk">模型: </span>
                        <span class="pv">{p.model}</span>
                      </div>
                      {p.base_url && (
                        <div>
                          <span class="pk">base_url: </span>
                          <span class="pv">{p.base_url}</span>
                        </div>
                      )}
                      {p.context_window && (
                        <div>
                          <span class="pk">上下文窗口: </span>
                          <span>{(p.context_window / 1000).toFixed(0)}k tokens</span>
                        </div>
                      )}
                      <div>
                        <span class="pk">API Key: </span>
                        <span class={p.has_api_key ? 'ok' : 'nok'}>
                          {p.has_api_key ? '已配置' : '未配置'}
                        </span>
                      </div>
                    </div>
                  </div>
                ))}
              </div>
            </>
          )}
        </div>

        <div class="modal-footer">
          <button class="btn" onClick={onClose}>
            关闭
          </button>
        </div>
      </div>
    </div>
  );
}

function Row({
  label,
  value,
  mono,
}: {
  label: string;
  value: string;
  mono?: boolean;
}) {
  return (
    <div class="config-row">
      <span class="config-key">{label}</span>
      <span class={'config-val' + (mono ? ' mono' : '')}>{value}</span>
    </div>
  );
}
