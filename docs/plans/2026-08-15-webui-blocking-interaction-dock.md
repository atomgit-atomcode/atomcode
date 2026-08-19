# WebUI Blocking Interaction Dock Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Replace full-screen overlays for turn-blocking approvals, user questions, and policy recovery choices with one composer-area interaction dock that leaves conversation context visible and scrollable.

**Architecture:** Keep `/chat` and `/live` request ownership, response APIs, and terminal cleanup unchanged. Move only presentation into `Chat`: app-owned `/chat` permission state is passed down, while live permission, structured user input, and policy intervention retain their existing state owners. A shared dock shell replaces the normal composer while one blocking interaction is pending.

**Tech Stack:** Preact, TypeScript, CSS, Node test runner.

---

### Task 1: Lock the interaction-shell contract with tests

**Files:**
- Modify: `webui/src/lib/userInputCard.test.ts`
- Create: `webui/src/lib/interactionDock.test.ts`

1. Add failing source-contract tests proving turn-blocking cards no longer use `.modal-overlay`.
2. Add a failing test proving the dock replaces the composer and has an internally scrollable body.
3. Run the focused WebUI tests and confirm they fail for the expected old overlay markup.

### Task 2: Add the shared dock presentation

**Files:**
- Create: `webui/src/components/InteractionDock.tsx`
- Modify: `webui/src/components/PermissionCard.tsx`
- Modify: `webui/src/components/UserInputCard.tsx`
- Modify: `webui/src/components/PolicyInterventionCard.tsx`
- Modify: `webui/src/styles/app.css`

1. Add a non-modal region shell with fixed header/footer and scrollable body.
2. Convert all three blocking cards to the shared shell without changing decisions or response payloads.
3. Add responsive height caps and mobile-safe sizing.
4. Run the component/source-contract tests.

### Task 3: Route every blocking interaction through the composer seat

**Files:**
- Modify: `webui/src/components/Chat.tsx`
- Modify: `webui/src/app.tsx`
- Modify: `webui/src/lib/interactionDock.test.ts`

1. Pass the app-owned `/chat` permission request into `Chat`.
2. Select the active blocking interaction from `/chat` permission, `/live` permission, user input, and policy recovery.
3. Replace the landing and regular composer contents with the dock while pending; restore the composer on terminal cleanup.
4. Verify ordinary management dialogs still use `.modal-overlay`.

### Task 4: Verify behavior and regression surface

1. Run focused WebUI tests.
2. Run the WebUI build/type check.
3. Review the final diff for accidental Rust or management-dialog changes.
4. Report that browser visual/mobile behavior still requires a manual screenshot check if no browser fixture is available.
