import type { ImageData } from '../api';

export interface PendingLiveSteer {
  id: string;
  text: string;
  images?: ImageData[];
  confirmed: boolean;
}

export interface FoldedLiveSteer {
  text: string;
  images: ImageData[];
}

function sameImages(left: ImageData[] | undefined, right: ImageData[]): boolean {
  const a = left ?? [];
  if (a.length !== right.length) return false;
  return a.every((image, index) => {
    const other = right[index];
    return image.media_type === other.media_type && image.data === other.data;
  });
}

/**
 * Consume authoritative kernel steer acknowledgements in FIFO order.
 * A mismatched event may belong to another synchronized client, so it must not
 * remove locally-owned pending input.
 */
export function acknowledgeLiveSteers(
  pending: PendingLiveSteer[],
  folded: FoldedLiveSteer[],
  clientInputIds: Array<string | null> = [],
): PendingLiveSteer[] {
  const remaining = [...pending];
  for (const [index, input] of folded.entries()) {
    const clientInputId = clientInputIds[index];
    if (clientInputId) {
      const owned = remaining.findIndex((item) => item.id === clientInputId);
      if (owned >= 0) remaining.splice(owned, 1);
      continue;
    }
    const front = remaining[0];
    if (front && front.text === input.text && sameImages(front.images, input.images)) {
      remaining.shift();
    }
  }
  return remaining;
}

export type SteerReceiptDisposition = 'started' | 'steered';
export type SteerReceiptOutcome = 'clear' | 'confirm' | 'release';

/**
 * A `/live/message` response may arrive after the user has selected another
 * provider. Only let the response undo the exact provider selection that was
 * submitted with that request; otherwise it is stale UI state.
 */
export function shouldApplySteerProviderFallback(
  submittedProvider: string | null,
  currentProvider: string | null,
  providerChangeApplied: boolean | undefined,
  effectiveProvider: string | undefined,
): effectiveProvider is string {
  return providerChangeApplied === false
    && Boolean(effectiveProvider)
    && currentProvider === submittedProvider;
}

/**
 * Reconcile a locally-submitted steer against its `/live/message` receipt.
 *
 * A `steered` receipt is authoritative: the runtime accepted the input into the
 * active turn. It must NEVER be rolled back into the composer — that both breaks
 * parity with the TUI (which folds the input or, if the turn ends first, re-runs
 * it as the next turn — but never bounces it back) and would duplicate the
 * prompt, because the kernel already re-runs any steer that arrived too late to
 * fold as the next turn (see `agent.rs` leftover-steer drain).
 *
 * - `started`  → `clear`: a new turn began; the submit IS that turn's input.
 * - `steered` while the client has not yet consumed the turn terminal → `confirm`:
 *   keep it pending; the `steered` ack removes it when the fold lands, or the
 *   turn terminal releases it if the turn ends first.
 * - `steered` after the terminal was consumed → `release`: the fold can no longer
 *   land, so drop the pending marker and defer to the runtime's re-run of the
 *   leftover steer. Do NOT restore to the composer.
 */
export function reconcileSteerReceipt(
  disposition: SteerReceiptDisposition,
  lifecycle: { running: boolean; terminalConsumed: boolean },
): SteerReceiptOutcome {
  if (disposition === 'started') return 'clear';
  return lifecycle.terminalConsumed ? 'release' : 'confirm';
}

/** Restore unacknowledged steers to an editable draft without losing order. */
export function pendingSteersToDraft(pending: PendingLiveSteer[]): {
  text: string;
  images: ImageData[];
} {
  return {
    text: pending.map((item) => item.text).filter(Boolean).join('\n'),
    images: pending.flatMap((item) => item.images ?? []),
  };
}
