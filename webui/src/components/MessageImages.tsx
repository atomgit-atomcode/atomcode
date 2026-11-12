import { useEffect, useRef, useState } from 'preact/hooks';
import { createPortal } from 'preact/compat';
import type { ImageData } from '../api';
import { useT } from '../settings';

/** Local image attachments only: preview uses the existing payload, never uploads it. */
export function MessageImages({ images }: { images?: ImageData[] }) {
  const t = useT();
  const [selected, setSelected] = useState<number | null>(null);
  const dialog = useRef<HTMLDialogElement>(null);
  const available = images?.filter(image => !image.missing && image.data.length > 0) ?? [];
  const image = selected == null ? undefined : available[selected];
  const src = (item: ImageData) => `data:${item.media_type};base64,${item.data}`;
  useEffect(() => {
    if (image && dialog.current && !dialog.current.open) dialog.current.showModal();
  }, [image]);
  if (!available.length) return null;
  return <>
    <div class="msg-images">
      {available.map((item, index) => <button type="button" class="msg-image-button" key={index}
        onClick={() => setSelected(index)} aria-label={t('chat.previewImage', { n: index + 1 })}>
        <img loading="lazy" decoding="async" class="msg-image" src={src(item)} alt={t('tool.image', { n: index + 1 })} />
      </button>)}
    </div>
    {image && createPortal(<dialog ref={dialog} class="message-image-preview" aria-label={t('chat.previewImage', { n: selected! + 1 })}
      onClose={() => setSelected(null)} onClick={event => { if (event.target === event.currentTarget) dialog.current?.close(); }}
      onKeyDown={event => { if (event.key === 'Escape') event.stopPropagation(); }}>
      <button type="button" class="message-image-close" autoFocus onClick={() => dialog.current?.close()}
        aria-label={t('chat.closeImagePreview')}>×</button>
      <img src={src(image)} alt={t('tool.image', { n: selected! + 1 })} />
    </dialog>, document.body)}
  </>;
}
