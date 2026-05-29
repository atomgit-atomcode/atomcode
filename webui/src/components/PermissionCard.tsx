// Task 14 — Tool approval modal card

import { useState } from 'preact/hooks';
import { respondPermission } from '../api';

interface PermissionRequest {
  session_id: string;
  tool_name: string;
  reason: string;
  call_id: string;
  arguments: unknown;
}

interface PermissionCardProps {
  req: PermissionRequest;
  onDone: () => void;
}

function formatArgs(args: unknown): string {
  if (typeof args === 'string') {
    // Try to pretty-print if it looks like JSON
    try {
      const parsed = JSON.parse(args);
      return JSON.stringify(parsed, null, 2);
    } catch {
      return args;
    }
  }
  try {
    return JSON.stringify(args, null, 2);
  } catch {
    return String(args);
  }
}

export function PermissionCard({ req, onDone }: PermissionCardProps) {
  const [loading, setLoading] = useState(false);

  async function decide(decision: 'allow' | 'deny' | 'always_allow') {
    if (loading) return;
    setLoading(true);
    try {
      await respondPermission(req.session_id, decision);
    } catch {
      // Best-effort; proceed to dismiss regardless
    } finally {
      setLoading(false);
      onDone();
    }
  }

  const argsDisplay = formatArgs(req.arguments);

  return (
    // Overlay
    <div class="fixed inset-0 bg-black/40 flex items-center justify-center p-4 z-50">
      <div class="bg-white rounded-2xl shadow-2xl max-w-lg w-full overflow-hidden">
        {/* Header */}
        <div class="px-5 py-4 border-b border-slate-200 flex items-center gap-2">
          <span class="text-amber-500 text-xl">⚠</span>
          <h3 class="font-semibold text-base">工具请求批准</h3>
          <span class="ml-auto font-mono text-xs px-2 py-0.5 rounded bg-amber-100 text-amber-700">
            {req.tool_name}
          </span>
        </div>

        {/* Body */}
        <div class="px-5 py-4 space-y-3">
          {req.reason && (
            <p class="text-sm text-slate-600">{req.reason}</p>
          )}
          <div>
            <div class="text-xs font-semibold text-slate-400 uppercase mb-1.5">
              参数
            </div>
            <pre class="bg-slate-900 text-slate-100 rounded-lg p-3 text-xs overflow-x-auto leading-relaxed max-h-64 overflow-y-auto">
              {argsDisplay}
            </pre>
          </div>
        </div>

        {/* Buttons */}
        <div class="px-5 py-3 bg-slate-50 border-t border-slate-200 flex gap-2 justify-end">
          <button
            disabled={loading}
            onClick={() => decide('deny')}
            class="px-3.5 py-1.5 rounded-lg bg-white border border-slate-300 text-sm hover:bg-slate-100 disabled:opacity-50"
          >
            拒绝
          </button>
          <button
            disabled={loading}
            onClick={() => decide('allow')}
            class="px-3.5 py-1.5 rounded-lg bg-indigo-600 text-white text-sm hover:bg-indigo-700 disabled:opacity-50"
          >
            批准
          </button>
          <button
            disabled={loading}
            onClick={() => decide('always_allow')}
            class="px-3.5 py-1.5 rounded-lg bg-emerald-600 text-white text-sm hover:bg-emerald-700 disabled:opacity-50"
          >
            本会话总是允许
          </button>
        </div>
      </div>
    </div>
  );
}
