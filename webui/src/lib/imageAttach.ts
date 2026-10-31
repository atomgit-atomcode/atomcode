// Attaching an image from a phone.
//
// The composer used to refuse anything over 2 MB, which on Android rejected
// essentially every camera photo (3–8 MB, 4000 px wide) before it was ever
// uploaded — the picker opened, the file was chosen, and nothing appeared. The
// daemon already downscales oversized images (`atomcode_capabilities::image_normalize`,
// longest edge 1568 px), but that happens AFTER the bytes crossed the wire, and a
// rejected file never crosses it at all.
//
// So the browser does the same thing first: decode, fit the longest edge to
// `MAX_EDGE` (the same 1568 px the daemon uses, so the daemon has nothing left to
// do), re-encode as JPEG down a quality ladder until it fits `MAX_UPLOAD_BYTES`.
// What lands in `pendingImages` is then small enough to render as a thumbnail and
// to store in the conversation.
//
// Any decode/encode failure returns `null` and the caller reports it: an image the
// browser cannot read is not silently dropped without a word.

/** Longest-edge cap in pixels. Mirrors `image_normalize::MAX_EDGE`. */
export const MAX_EDGE = 1568;

/** Post-compression byte cap for one attached image (raw bytes, pre-base64). */
export const MAX_UPLOAD_BYTES = 2 * 1024 * 1024;

/**
 * Largest raw file we will even try to decode.
 *
 * A phone photo is 3–8 MB; a 50 MP DSLR scan or a RAW-ish export can be far
 * larger, and decoding it in a WebView is how the page gets killed. Above this the
 * file is refused with a message instead.
 */
export const MAX_RAW_BYTES = 40 * 1024 * 1024;

/** JPEG quality ladder, tried in order until the encoded bytes fit the budget. */
export const QUALITY_LADDER = [0.9, 0.82, 0.72, 0.6];

export interface ImagePayload {
  media_type: string;
  /** base64, no `data:` prefix. */
  data: string;
}

/** Fit `width`×`height` inside a square of `maxEdge`, preserving aspect ratio. */
export function fitWithin(width: number, height: number, maxEdge: number = MAX_EDGE): { width: number; height: number } {
  if (width <= 0 || height <= 0) return { width: 0, height: 0 };
  const longest = Math.max(width, height);
  if (longest <= maxEdge) return { width: Math.round(width), height: Math.round(height) };
  const scale = maxEdge / longest;
  return {
    // `max(1, …)` so a 20000×3 panorama does not collapse to zero pixels.
    width: Math.max(1, Math.round(width * scale)),
    height: Math.max(1, Math.round(height * scale)),
  };
}

/**
 * What to do with a decoded image.
 *
 * `null` = the bytes are already within both budgets and can be sent as-is; the
 * returned size is the one to draw when re-encoding is needed.
 */
export function planUpload(
  width: number,
  height: number,
  bytes: number,
  maxEdge: number = MAX_EDGE,
  maxBytes: number = MAX_UPLOAD_BYTES,
): { width: number; height: number } | null {
  const oversized = bytes > maxBytes || Math.max(width, height) > maxEdge;
  if (!oversized) return null;
  return fitWithin(width, height, maxEdge);
}

/** Whether a raw file is worth handing to the decoder at all. */
export function isDecodable(file: { type: string; size: number }): boolean {
  return file.type.startsWith('image/') && file.size <= MAX_RAW_BYTES && file.size > 0;
}

/**
 * Pick the encoding for an image that had to be re-encoded.
 *
 * PNG keeps text crisp but bloats a photo; a photo has no small lossless form. So:
 * keep PNG only for what is likely a screenshot/diagram (the caller passes whether
 * the source was PNG and the image is small in pixels), otherwise JPEG.
 */
export function encodedType(sourceType: string, pixels: number): string {
  const screenshotish = sourceType === 'image/png' || sourceType === 'image/gif' || sourceType === 'image/bmp';
  // A 1568-px-edge PNG of text stays well under the budget; a photograph does not.
  return screenshotish && pixels <= 1_200_000 ? 'image/png' : 'image/jpeg';
}

function readAsDataUrl(blob: Blob): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(String(reader.result || ''));
    reader.onerror = () => reject(reader.error ?? new Error('could not read the file'));
    reader.readAsDataURL(blob);
  });
}

function stripDataUrl(dataUrl: string): string {
  const comma = dataUrl.indexOf(',');
  return comma >= 0 ? dataUrl.slice(comma + 1) : '';
}

async function decode(file: File): Promise<{ source: CanvasImageSource; width: number; height: number; close: () => void } | null> {
  // `createImageBitmap` is what the phone's WebView has; the `Image` path is the
  // fallback for anything older.
  const bitmapFactory = (globalThis as { createImageBitmap?: (input: Blob) => Promise<ImageBitmap> }).createImageBitmap;
  if (bitmapFactory) {
    try {
      const bitmap = await bitmapFactory(file);
      return { source: bitmap, width: bitmap.width, height: bitmap.height, close: () => bitmap.close() };
    } catch {
      // fall through to the element path
    }
  }
  const url = URL.createObjectURL(file);
  try {
    const image = await new Promise<HTMLImageElement | null>((resolve) => {
      const el = new Image();
      el.onload = () => resolve(el);
      el.onerror = () => resolve(null);
      el.src = url;
    });
    if (!image) {
      URL.revokeObjectURL(url);
      return null;
    }
    return {
      source: image,
      width: image.naturalWidth || image.width,
      height: image.naturalHeight || image.height,
      close: () => URL.revokeObjectURL(url),
    };
  } catch {
    URL.revokeObjectURL(url);
    return null;
  }
}

function encode(canvas: HTMLCanvasElement, type: string, quality: number): Promise<Blob | null> {
  return new Promise((resolve) => {
    canvas.toBlob((blob) => resolve(blob), type, quality);
  });
}

/**
 * Read a picked/pasted File into an attachable payload.
 *
 * Returns `null` when the file is not an image, is too large to decode, or could
 * not be decoded/encoded — the caller decides what to tell the person.
 */
export async function fileToImageData(file: File): Promise<ImagePayload | null> {
  if (!isDecodable(file)) return null;
  const decoded = await decode(file);
  if (!decoded) return null;
  try {
    const plan = planUpload(decoded.width, decoded.height, file.size);
    if (!plan) {
      // Already small enough: send the original bytes unchanged (no re-encode, so
      // no generation loss on the common case).
      const dataUrl = await readAsDataUrl(file);
      const data = stripDataUrl(dataUrl);
      return data ? { media_type: file.type, data } : null;
    }
    const canvas = document.createElement('canvas');
    canvas.width = plan.width;
    canvas.height = plan.height;
    const context = canvas.getContext('2d');
    if (!context) return null;
    context.imageSmoothingEnabled = true;
    context.imageSmoothingQuality = 'high';
    context.drawImage(decoded.source, 0, 0, plan.width, plan.height);

    const type = encodedType(file.type, plan.width * plan.height);
    let best: Blob | null = null;
    for (const quality of type === 'image/png' ? [1] : QUALITY_LADDER) {
      const blob = await encode(canvas, type, quality);
      if (!blob) continue;
      if (blob.size <= MAX_UPLOAD_BYTES) {
        best = blob;
        break;
      }
      // The ladder's smallest attempt still goes out rather than dropping the
      // image: the daemon's own normalizer will shave whatever is left.
      if (!best || blob.size < best.size) best = blob;
    }
    if (!best) return null;
    const data = stripDataUrl(await readAsDataUrl(best));
    return data ? { media_type: type, data } : null;
  } finally {
    decoded.close();
  }
}
