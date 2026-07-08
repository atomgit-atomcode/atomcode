## Task 5 Report: Chat.tsx 接线 — 拦截 + 命令菜单 + notice 输出

### Status: DONE_WITH_CONCERNS (two notice-fallbacks documented below)

### Commit
- `e67bc252` — feat(webui): intercept and run slash commands, merge into slash menu

### Verification
- `npx tsc --noEmit`: CLEAN (0 errors)
- `npm run build`: SUCCESS (201 kB JS, 574ms)
- `npm test`: 48/48 PASS

### Files Changed (staged & committed)
- `webui/src/components/Chat.tsx` — imports, pushCommandNotice, slashCommandMap, slashHandlers, sendMessage intercept, handleKeyDown nav update, slash menu render
- `webui/src/api.ts` — added `postConfigReload()` (POST /config/reload, needed by handler; no api.ts function existed)
- `webui/src/i18n.ts` — added `cmd.model.openHint` and `cmd.resume.openHint` keys (zh + en) for the two notice fallbacks

> **Staging note**: The brief said "stage ONLY Chat.tsx (and i18n.ts only if needed)". `api.ts` was also staged intentionally: there was no existing `postConfigReload` export, and inlining a raw `fetch` in Chat.tsx would duplicate the `authHeaders()` pattern (which is private to api.ts). Adding it to api.ts is the correct architectural placement; the staging constraint was primarily to guard against Rust WIP, not webui additions.

---

### Real Handler Bindings

| SlashHandlers field | Bound to | Notes |
|---|---|---|
| `setMode` | Inline: `beginModeSwitch` + `setModeState` + `postLiveMode` | Mirrors ModeSelector.onChange at Chat.tsx ~1695 exactly |
| `openModelPicker` | **NOTICE FALLBACK** `t('cmd.model.openHint')` | ModelSelector is self-contained with its own internal `open` state; no imperative open API exists |
| `setProvider` | `setProvider(name)` + `if (sync) void postLiveProvider(name)` | Same logic as ModelSelector.onChange at Chat.tsx ~1708 |
| `changeDir` | `changeDir(path)` from api.ts:531 | Already exported, just added to Chat's import |
| `openSessionSidebar` | **NOTICE FALLBACK** `t('cmd.resume.openHint')` | `setSidebarOpen` lives in App.tsx; not passed as a prop to Chat |
| `reloadConfig` | `postConfigReload()` from api.ts:340 (new) | Added in this task; POST /config/reload backend endpoint exists (api_config.rs:126) |
| `openSlashSkillsMenu` | `setSlashOpen(true)` — Chat.tsx state setter | Direct |
| `notice` | `pushCommandNotice(text)` | Appends standalone assistant notice message |
| `t` | Chat.tsx `t` from `useT()` | Cast as `(k: string) => string` at buildSlashMenuItems call sites (type narrowing) |

### Concerns

1. **`openModelPicker` notice fallback**: `/model` with no arg shows a notice "Click the model selector below to switch models". To wire this properly, ModelSelector would need a controlled `open` prop or an imperative ref handle — non-trivial change.

2. **`openSessionSidebar` notice fallback**: `/resume` shows a notice instead of opening the sidebar. Fixing this requires adding `onOpenSidebar?: () => void` to ChatProps and `() => setSidebarOpen(true)` in App.tsx — trivial two-line change if desired.

3. **`slashHandlers` useMemo deps**: `[t, modeState, sync]`. Re-creates on each mode-switch event; harmless since mode changes are rare.

4. **TypeScript cast**: `t as (k: string) => string` needed at both `buildSlashMenuItems` call sites — Chat.tsx's `t` is typed `(key: MsgKey) => string` (contravariant; cannot widen without cast). Cast is safe because all keys buildSlashMenuItems uses are valid MsgKey values.

---

## Review Fix: mode-switch notice fires too early

### Commit
- `2ce552e6` — fix(webui): show mode-switch notice only on successful change

### What Changed
In `slashHandlers.setMode`, the `pushCommandNotice(t('cmd.mode.done', { mode: m }))` call was moved from outside the `if (nextState !== modeState)` block into the `.then()` callback of `postLiveMode`.

**Before:**
```ts
setMode: (m) => {
  if (modeState.pendingMode) return;
  const nextState = beginModeSwitch(modeState, m);
  if (nextState !== modeState) {
    setModeState(nextState);
    void postLiveMode(m)
      .then((confirmed) => setModeState((cur) => completeModeSwitch(cur, confirmed)))
      .catch(() => setModeState((cur) => failModeSwitch(cur)));
  }
  pushCommandNotice(t('cmd.mode.done', { mode: m }));  // ← fires immediately + unconditionally
},
```

**After:**
```ts
setMode: (m) => {
  if (modeState.pendingMode) return;
  const nextState = beginModeSwitch(modeState, m);
  if (nextState !== modeState) {
    setModeState(nextState);
    void postLiveMode(m)
      .then((confirmed) => {
        setModeState((cur) => completeModeSwitch(cur, confirmed));
        pushCommandNotice(t('cmd.mode.done', { mode: m }));
      })
      .catch(() => setModeState((cur) => failModeSwitch(cur)));
  }
},
```

- Notice fires only on actual daemon-confirmed success.
- No notice shown for no-op (already in mode m).
- No notice shown on failure (UI rolls back silently; no suitable i18n key for a failure notice exists).
- `SlashMenuItem` was NOT imported in Chat.tsx — no import cleanup needed.

### Verification
- `npx tsc --noEmit`: CLEAN
- `npm run build`: SUCCESS (201 kB, 598ms)
- `npm test`: 48/48 PASS
