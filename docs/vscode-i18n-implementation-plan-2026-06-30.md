# VS Code i18n Implementation Plan

## Goal

Make the VS Code extension follow VS Code's locale for IDE-facing strings and make the AtomCode webview render Chinese or English consistently from the same locale signal.

## Current State

- `extensions/vscode/package.json` contains hardcoded English command titles, view names, and configuration descriptions.
- Extension host code uses hardcoded English strings in commands, code actions, status bar tooltips, input boxes, warning messages, and slash-command replies.
- `extensions/vscode/webview/index.html` hardcodes `<html lang="en">`.
- `extensions/vscode/webview-ui/src` has no translation catalog or translation hook. User-facing copy is hardcoded in React components.
- The root browser webui already has a useful catalog pattern in `webui/src/i18n.ts` and `webui/src/settings.tsx`, but the VS Code webview does not consume it.
- Rust core/TUI i18n is independent and should remain independent.

## Architecture

- VS Code manifest strings use the official `package.nls.json` and `package.nls.zh-cn.json` mechanism.
- Extension host runtime strings use `vscode.l10n.t`.
- Webview UI strings use a local TypeScript catalog and `I18nProvider`.
- `ChatViewProvider` passes `vscode.env.language` into webview HTML and the `init` message.
- The webview maps `zh`, `zh-CN`, and `zh-TW` to Simplified Chinese for now; all other locales fall back to English.
- Prompts sent to the model stay English unless they are purely user-visible UI labels. This keeps model behavior stable while localizing the interface.

## Files

- Create `extensions/vscode/package.nls.json`.
- Create `extensions/vscode/package.nls.zh-cn.json`.
- Create `extensions/vscode/webview-ui/src/i18n.ts`.
- Create `extensions/vscode/webview-ui/test/i18n-regression.test.ts`.
- Create `extensions/vscode/webview-ui/test/run-tests.js`.
- Modify `extensions/vscode/package.json`.
- Modify `extensions/vscode/src/chat/provider.ts`.
- Modify `extensions/vscode/src/editor/actions.ts`.
- Modify `extensions/vscode/src/extension.ts`.
- Modify `extensions/vscode/src/status.ts`.
- Modify `extensions/vscode/webview/index.html`.
- Modify React webview components under `extensions/vscode/webview-ui/src/components`.
- Modify `extensions/vscode/webview-ui/src/state/types.ts` and `ChatProvider.tsx` to carry locale.
- Modify `extensions/vscode/webview-ui/src/utils/format.ts` to accept translated time/token labels.

## Phases

### Phase 1: Test Harness and Catalog Foundation

Acceptance:
- `npm run test:webview` runs webview regression tests through esbuild and Node.
- Tests fail before implementation for missing locale normalization, missing Chinese strings, and missing manifest localization files.
- `i18n.ts` exports `normalizeLocale`, `createTranslator`, `messages`, `Lang`, and `MsgKey`.

### Phase 2: Webview Locale Wiring

Acceptance:
- `index.html` no longer hardcodes English language.
- `ChatViewProvider` injects locale into HTML and `init`.
- `ChatState` stores `locale`.
- `I18nProvider` sets `document.documentElement.lang`.
- Webview defaults to VS Code locale, with English fallback.

### Phase 3: Webview Copy Coverage

Acceptance:
- Home page, quick actions, setup flow, input area, attach menu, file picker, header tooltips, session list, search bar, model selector, permission request, tool call labels, assistant copy button, provider settings, and relative time use `t()`.
- Chinese copy follows `docs/i18n-style.md`.
- Model/provider names, file paths, command names, API keys, and daemon/model output remain untranslated data.

### Phase 4: VS Code IDE Integration

Acceptance:
- `package.json` contribution strings use `%key%` placeholders.
- English and Chinese `package.nls` files cover every placeholder.
- Runtime extension strings use `vscode.l10n.t`.
- Quick action display strings are localized, while the prompts sent to the model stay English.

### Phase 5: Verification

Acceptance:
- `npm run test:webview` passes.
- `npm run compile` passes.
- A search for known homepage English strings confirms they are no longer hardcoded in JSX.
- A search confirms no `package.json` contribution titles/descriptions remain hardcoded except stable product names and enum values.

## Out of Scope

- Translating daemon API error payloads returned by remote providers.
- Translating model output.
- Adding an in-webview language switcher independent of VS Code locale.
- Sharing Rust enum-based i18n directly with TypeScript.

## Risks

- VS Code static contribution strings and webview runtime strings use different localization mechanisms. Keep them separate.
- Over-translating prompts can change model behavior. Keep agent prompts stable.
- Some existing Chinese hardcoded text exists in session-management UI. Move it into the catalog rather than treating it as complete localization.
