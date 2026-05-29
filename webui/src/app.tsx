import { useState } from 'preact/hooks';
import { Chat } from './components/Chat';
import { PermissionCard } from './components/PermissionCard';

export function App() {
  const [pending, setPending] = useState<any | null>(null);
  return (
    <div class="h-screen flex flex-col bg-slate-50 text-slate-800">
      <header class="h-12 shrink-0 flex items-center px-4 border-b border-slate-200 bg-white">
        <span class="font-mono font-bold text-indigo-600">▲ atomcode webui</span>
      </header>
      <main class="flex-1 min-h-0">
        <Chat onPermission={setPending} />
      </main>
      {pending && <PermissionCard req={pending} onDone={() => setPending(null)} />}
    </div>
  );
}
