import React, { useCallback, useEffect } from 'react';
import { useChatContext } from '../state/ChatProvider';
import { useT } from '../i18n';
import { imageDataUrl } from '../utils/format';

/**
 * Fullscreen image preview (lightbox) driven by `state.imagePreview`.
 * Renders nothing while closed. Supports multi-image navigation (‹/› buttons
 * and ArrowLeft/ArrowRight) and three close paths: backdrop click, ESC, and
 * the close button. Locks body scroll while open and restores it on close.
 */
export function ImageLightbox() {
  const { state, dispatch } = useChatContext();
  const t = useT();
  const preview = state.imagePreview;

  const close = useCallback(() => dispatch({ type: 'CLOSE_IMAGE_PREVIEW' }), [dispatch]);
  const prev = useCallback(() => dispatch({ type: 'IMAGE_PREVIEW_PREV' }), [dispatch]);
  const next = useCallback(() => dispatch({ type: 'IMAGE_PREVIEW_NEXT' }), [dispatch]);

  // Lock body scroll while the lightbox is open; restore on close/unmount.
  useEffect(() => {
    if (!preview) return;
    const previousOverflow = document.body.style.overflow;
    document.body.style.overflow = 'hidden';
    return () => {
      document.body.style.overflow = previousOverflow;
    };
  }, [preview !== null]);

  // ESC closes; ArrowLeft/ArrowRight navigate (only meaningful for multi-image).
  useEffect(() => {
    if (!preview) return;
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === 'Escape') close();
      else if (e.key === 'ArrowLeft') prev();
      else if (e.key === 'ArrowRight') next();
    };
    window.addEventListener('keydown', onKeyDown);
    return () => window.removeEventListener('keydown', onKeyDown);
  }, [preview, close, prev, next]);

  // If the current slot has no renderable image (session restore edge case),
  // close instead of showing a broken lightbox.
  useEffect(() => {
    if (!preview) return;
    const img = preview.images[preview.index];
    if (!img || img.missing || !img.data) close();
  }, [preview, close]);

  if (!preview) return null;

  const { images, index } = preview;
  const total = images.length;
  const img = images[index];
  if (!img || img.missing || !img.data) return null;

  return (
    <div
      className="image-lightbox"
      role="dialog"
      aria-modal="true"
      aria-label={t('image.preview')}
      onClick={close}
    >
      {total > 1 && (
        <button
          type="button"
          className="image-lightbox-nav image-lightbox-prev"
          aria-label={t('image.previous')}
          onClick={(e) => {
            e.stopPropagation();
            prev();
          }}
        >
          ‹
        </button>
      )}
      <div className="image-lightbox-stage" onClick={(e) => e.stopPropagation()}>
        <img src={imageDataUrl(img)} alt="" />
      </div>
      {total > 1 && (
        <button
          type="button"
          className="image-lightbox-nav image-lightbox-next"
          aria-label={t('image.next')}
          onClick={(e) => {
            e.stopPropagation();
            next();
          }}
        >
          ›
        </button>
      )}
      <button
        type="button"
        className="image-lightbox-close"
        aria-label={t('image.closePreview')}
        onClick={close}
      >
        ×
      </button>
      {total > 1 && (
        <div className="image-lightbox-counter">
          {index + 1}/{total}
        </div>
      )}
    </div>
  );
}
