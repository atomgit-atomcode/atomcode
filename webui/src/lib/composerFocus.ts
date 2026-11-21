import { useCallback, useEffect, useRef } from 'preact/hooks';

/**
 * Marks a composer whose toolbar must stay expanded.
 */
export const COMPOSER_TOOLBAR_HELD = 'toolbar-held';

/** What the hold needs from a composer: a way to mark and unmark it. */
export interface ComposerHoldTarget {
  mark: () => void;
  unmark: () => void;
}

/**
 * Runs a task after the pending click and hands back a cancel function.
 *
 * The contract is deliberately "returns a cancel function" rather than "returns
 * whatever the timer API returns": the core has to be able to drop a stale
 * release, and a timer id cannot express that.
 */
export type ComposerHoldDefer = (task: () => void) => () => void;

export interface ComposerHoldCoreOptions {
  defer: ComposerHoldDefer;
  /** Subscribes to releases that happen anywhere in the document. */
  watch: (onPointer: (kind: 'up' | 'cancel') => void) => () => void;
}

export interface ComposerToolbarHoldCore {
  press: (composer: ComposerHoldTarget | null) => void;
  lift: () => void;
  cancel: () => void;
  dispose: () => void;
}

/**
 * The hold core, free of DOM and timer APIs so it can be unit tested.
 *
 * The mobile composer collapses on `:focus-within` alone. Tapping one of its
 * controls dismisses the soft keyboard and blurs the field — on iOS Safari a
 * tapped button does not take focus in exchange — so the toolbar collapses out
 * from under the tap: the control becomes `display:none` and its click never
 * reaches it.
 *
 * The hold is a marker on the composer rather than a focus change, because the
 * collapse rule can exclude a marker but not "a focus that may be taken away
 * again": the blur follows the tap and outlives any focus the tap tried to
 * claim. Cancelling the press instead would suppress the emulated click, and
 * the menus close on mousedown.
 *
 * The marker is set and cleared on the DOM directly. The blur that collapses
 * the composer happens between events, so the mark has to be in place before
 * the browser restyles — a state update would land too late.
 */
export function createComposerToolbarHoldCore(
  options: ComposerHoldCoreOptions,
): ComposerToolbarHoldCore {
  let held: ComposerHoldTarget | null = null;
  // A press that is released before the next task leaves a pending release
  // behind. A newer press must not be undone by that stale release, so the
  // pending task is dropped whenever the held target changes.
  let pending: (() => void) | null = null;
  let unwatch: (() => void) | null = null;

  const release = () => {
    const target = held;
    held = null;
    if (pending) {
      pending();
      pending = null;
    }
    if (unwatch) {
      unwatch();
      unwatch = null;
    }
    target?.unmark();
  };

  // The click is dispatched right after the pointerup, so the mark has to
  // outlive this task. A menu that opened keeps the composer expanded on its
  // own; a press that never became a click releases the mark here.
  const releaseLater = () => {
    const target = held;
    if (!target) return;
    // A second release must not leave the first one scheduled: the older task
    // would fire under the newer press and drop the mark too early.
    if (pending) {
      pending();
      pending = null;
    }
    pending = options.defer(() => {
      pending = null;
      if (held === target) release();
    });
  };

  const hold = (composer: ComposerHoldTarget | null) => {
    if (pending) {
      pending();
      pending = null;
    }
    if (held && held !== composer) held.unmark();
    held = composer;
    // One subscription per hold. A previous press on the same composer would
    // otherwise leave a second subscription behind, and every release would
    // then run the release path twice.
    if (unwatch) {
      unwatch();
      unwatch = null;
    }
    if (!composer) return;
    composer.mark();
    unwatch = options.watch(kind => {
      if (kind === 'cancel') release();
      else releaseLater();
    });
  };

  return {
    press: hold,
    lift: releaseLater,
    cancel: release,
    dispose: release,
  };
}

export interface ComposerToolbarHold {
  onPointerDown: (event: { currentTarget: HTMLElement }) => void;
  onPointerUp: () => void;
  onPointerCancel: () => void;
}

export interface ComposerHoldDomOptions {
  defer: ComposerHoldDefer;
  /** The composer that owns a pressed control, if any. */
  composerOf: (control: HTMLElement) => Element | null;
  /** Subscribes to releases that happen anywhere in the document. */
  watch: (onPointer: (kind: 'up' | 'cancel') => void) => () => void;
}

/**
 * Wires the hold core to the DOM. Everything the DOM offers comes in as an
 * option, so the wiring itself is unit testable.
 */
export function createComposerToolbarHoldDom(
  options: ComposerHoldDomOptions,
): ComposerToolbarHold & { dispose: () => void } {
  const core = createComposerToolbarHoldCore({ defer: options.defer, watch: options.watch });

  const onPointerDown = (event: { currentTarget: HTMLElement }) => {
    const element = options.composerOf(event.currentTarget);
    if (!element) {
      core.press(null);
      return;
    }
    core.press({
      mark: () => element.classList.add(COMPOSER_TOOLBAR_HELD),
      unmark: () => element.classList.remove(COMPOSER_TOOLBAR_HELD),
    });
  };

  return {
    onPointerDown,
    onPointerUp: () => core.lift(),
    onPointerCancel: () => core.cancel(),
    dispose: () => core.dispose(),
  };
}

/**
 * Holds the mobile composer expanded while one of its controls is pressed.
 *
 * A press that is released outside the control — a drag that slides off the
 * button — never delivers `pointerup` to the control, so the hold also
 * watches the document; otherwise the mark would stay and the composer would
 * never collapse again.
 */
export function useComposerToolbarHold(): ComposerToolbarHold {
  const hold = useRef<(ComposerToolbarHold & { dispose: () => void }) | null>(null);
  if (!hold.current) {
    hold.current = createComposerToolbarHoldDom({
      defer: task => {
        // Touch-generated clicks may arrive in a later task than pointerup.
        // Keep the target visible until that click finishes. If the gesture
        // produces no click (for example a drag), release the hold anyway.
        let timer: number;
        const cleanup = () => {
          window.clearTimeout(timer);
          document.removeEventListener('click', clicked, true);
        };
        const finish = () => { cleanup(); task(); };
        const clicked = () => {
          document.removeEventListener('click', clicked, true);
          window.clearTimeout(timer);
          timer = window.setTimeout(finish, 0);
        };
        document.addEventListener('click', clicked, true);
        timer = window.setTimeout(finish, 500);
        return cleanup;
      },
      composerOf: control => control.closest('.input-box'),
      watch: onPointer => {
        const release = () => onPointer('up');
        const cancel = () => onPointer('cancel');
        document.addEventListener('pointerup', release);
        document.addEventListener('pointercancel', cancel);
        return () => {
          document.removeEventListener('pointerup', release);
          document.removeEventListener('pointercancel', cancel);
        };
      },
    });
  }

  const onPointerDown = useCallback(
    (event: { currentTarget: HTMLElement }) => hold.current?.onPointerDown(event),
    [],
  );
  const onPointerUp = useCallback(() => hold.current?.onPointerUp(), []);
  const onPointerCancel = useCallback(() => hold.current?.onPointerCancel(), []);

  useEffect(() => () => hold.current?.dispose(), []);

  return { onPointerDown, onPointerUp, onPointerCancel };
}
