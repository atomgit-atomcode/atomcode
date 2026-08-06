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
