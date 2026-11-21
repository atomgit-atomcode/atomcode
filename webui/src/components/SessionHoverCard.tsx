import { ComponentChildren } from 'preact';
import { createPortal } from 'preact/compat';
import { useEffect, useId, useLayoutEffect, useRef, useState } from 'preact/hooks';

/** A session's full identity, outside the sidebar's scrolling/clipping surface. */
export function SessionHoverCard({ title, directory, onSelect, children }: {
  title: string;
  directory: string;
  onSelect: () => void;
  children: ComponentChildren;
}) {
  const id = useId();
  const anchor = useRef<HTMLButtonElement>(null);
  const card = useRef<HTMLDivElement>(null);
  const timer = useRef<ReturnType<typeof setTimeout>>();
  const [open, setOpen] = useState(false);
  const [position, setPosition] = useState({ left: 0, top: 0 });
  const cancel = () => { clearTimeout(timer.current); };
  const close = () => { cancel(); setOpen(false); };
  const show = () => { cancel(); timer.current = setTimeout(() => setOpen(true), 350); };
  const leave = () => {
    cancel();
    timer.current = setTimeout(() => {
      // Keep the description while its trigger has keyboard focus, or while
      // the pointer is selecting text / scrolling inside the preview itself.
      if (document.activeElement !== anchor.current && !card.current?.matches(':hover')) setOpen(false);
    }, 120);
  };

  useEffect(() => () => cancel(), []);
  useEffect(() => { close(); }, [title, directory]);
  useLayoutEffect(() => {
    if (!open || !anchor.current || !card.current) return;
    const rect = anchor.current.getBoundingClientRect();
    const bubble = card.current.getBoundingClientRect();
    const pad = 8;
    // Prefer the space beside the sidebar; use the opposite side or clamp on
    // narrow screens. Measure after wrapping so bottom rows stay in view too.
    const left = rect.right + pad + bubble.width <= window.innerWidth - pad
      ? rect.right + pad : rect.left - bubble.width - pad;
    setPosition({
      left: Math.max(pad, Math.min(left, window.innerWidth - bubble.width - pad)),
      top: Math.max(pad, Math.min(rect.top, window.innerHeight - bubble.height - pad)),
    });
  }, [open, title, directory]);
  useEffect(() => {
    if (!open) return;
    const key = (e: KeyboardEvent) => { if (e.key === 'Escape') close(); };
    const scroll = (e: Event) => { if (!(e.target instanceof Node && card.current?.contains(e.target))) close(); };
    const outside = (e: Event) => {
      if (!(e.target instanceof Node && (card.current?.contains(e.target) || anchor.current?.contains(e.target)))) close();
    };
    window.addEventListener('keydown', key);
    window.addEventListener('scroll', scroll, true);
    window.addEventListener('resize', close);
    window.addEventListener('pointerdown', outside);
    return () => {
      window.removeEventListener('keydown', key);
      window.removeEventListener('scroll', scroll, true);
      window.removeEventListener('resize', close);
      window.removeEventListener('pointerdown', outside);
    };
  }, [open]);

  return <>
    <button ref={anchor} class="session-item-main" aria-describedby={open ? id : undefined}
      onMouseEnter={() => { if (window.matchMedia('(hover: hover)').matches) show(); }}
      onMouseLeave={leave} onFocus={show} onBlur={leave}
      onClick={() => { close(); onSelect(); }}>
      {children}
    </button>
    {open && createPortal(
      <div ref={card} id={id} role="tooltip" class="session-hover-card" style={position}
        onMouseEnter={cancel} onMouseLeave={leave}>
        <div class="session-hover-card-title">{title}</div>
        <div class="session-hover-card-directory"><svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" aria-hidden="true"><path d="M3 7V5a2 2 0 0 1 2-2h5l3 3h6a2 2 0 0 1 2 2v11a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2Z"/></svg><span>{directory}</span></div>
      </div>, document.body,
    )}
  </>;
}
