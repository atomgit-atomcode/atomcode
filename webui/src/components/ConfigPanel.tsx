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
      class="fixed inset-0 bg-black/40 flex items-center justify-center p-4 z-50"
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div class="bg-white rounded-2xl shadow-2xl max-w-lg w-full overflow-hidden">
        {/* Header */}
        <div class="px-5 py-4 border-b border-slate-200 flex items-center gap-2">
          <span class="text-xl">⚙</span>
          <h3 class="font-semibold text-base">配置</h3>
          <span class="ml-2 text-xs text-slate-400">只读</span>
          <button
            onClick={onClose}
            class="ml-auto text-slate-400 hover:text-slate-700 text-lg leading-none"
            aria-label="关闭"
          >
            ×
          </button>
        </div>

        {/* Body */}
        <div class="px-5 py-4 space-y-4 max-h-[70vh] overflow-y-auto">
          {error && (
            <div class="text-sm text-rose-500">加载失败: {error}</div>
          )}
          {!config && !error && (
            <div class="text-sm text-slate-400">加载中…</div>
          )}
          {config && (
            <>
              {/* Basic info */}
              <div class="space-y-2">
                <Row label="默认 Provider" value={config.default_provider} />
                {config.default_workdir && (
                  <Row label="默认工作目录" value={config.default_workdir} mono />
                )}
                <Row label="配置文件" value={config.path} mono small />
              </div>

              {/* Providers list */}
              <div>
                <div class="text-xs font-semibold text-slate-400 uppercase mb-2">
                  Providers ({config.providers.length})
                </div>
                <div class="space-y-2">
                  {config.providers.map((p) => (
                    <div
                      key={p.name}
                      class={
                        'rounded-lg border px-3 py-2.5 text-sm ' +
                        (p.is_default
                          ? 'border-indigo-200 bg-indigo-50'
                          : 'border-slate-200 bg-slate-50')
                      }
                    >
                      <div class="flex items-center gap-2 mb-1">
                        <span class="font-semibold text-slate-800">{p.name}</span>
                        {p.is_default && (
                          <span class="text-xs px-1.5 py-0.5 rounded-full bg-indigo-600 text-white">
                            默认
                          </span>
                        )}
                        <span class="ml-auto text-xs text-slate-400 font-mono">
                          {p.type}
                        </span>
                      </div>
                      <div class="text-xs text-slate-500 space-y-0.5">
                        <div>
                          <span class="text-slate-400">模型: </span>
                          <span class="font-mono">{p.model}</span>
                        </div>
                        {p.base_url && (
                          <div>
                            <span class="text-slate-400">base_url: </span>
                            <span class="font-mono">{p.base_url}</span>
                          </div>
                        )}
                        {p.context_window && (
                          <div>
                            <span class="text-slate-400">上下文窗口: </span>
                            <span>{(p.context_window / 1000).toFixed(0)}k tokens</span>
                          </div>
                        )}
                        <div>
                          <span class="text-slate-400">API Key: </span>
                          <span class={p.has_api_key ? 'text-emerald-600' : 'text-rose-500'}>
                            {p.has_api_key ? '已配置' : '未配置'}
                          </span>
                        </div>
                      </div>
                    </div>
                  ))}
                </div>
              </div>
            </>
          )}
        </div>

        {/* Footer */}
        <div class="px-5 py-3 bg-slate-50 border-t border-slate-200 flex justify-end">
          <button
            onClick={onClose}
            class="px-4 py-1.5 rounded-lg bg-white border border-slate-300 text-sm hover:bg-slate-100"
          >
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
  small,
}: {
  label: string;
  value: string;
  mono?: boolean;
  small?: boolean;
}) {
  return (
    <div class="flex items-start gap-2">
      <span class="text-xs text-slate-400 w-24 shrink-0 pt-0.5">{label}</span>
      <span
        class={
          (mono ? 'font-mono ' : '') +
          (small ? 'text-xs ' : 'text-sm ') +
          'text-slate-700 break-all'
        }
      >
        {value}
      </span>
    </div>
  );
}
