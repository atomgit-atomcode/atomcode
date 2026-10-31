// A plugin card that starts folded.
//
// The list grew past the point where every card could show its description,
// commands and buttons at once: two dozen packages filled several screens with
// text nobody was reading. Folded, a card is one line — the name, and its size
// at the far right — so the list reads as a list.
//
// Version and the "installed 1.2.3" badge deliberately live in the body, not the
// header: on a 360 dp phone a header carrying name + version + badge + size wraps
// onto two lines for anything with a date-like version (`quickjs 2025-09-13`),
// which is exactly the clutter this card exists to remove. The one thing that
// stays in the header besides the name is a status dot, because "installed but
// fails to load" must be visible without opening the card.
//
// The whole header is the button, and `aria-expanded` rides along so a screen
// reader (and the browser's own hit-testing) sees the same state the caret shows.

import { ComponentChildren } from 'preact';

interface PluginCardProps {
  name: string;
  /** Short right-aligned fact: size in bytes already formatted, or a status. */
  meta?: string;
  /** Tiny dot kept visible when folded — health, or "installed". */
  dot?: 'ok' | 'bad';
  dotTitle?: string;
  /** Version / installed badge line, shown only when expanded. */
  details?: ComponentChildren;
  expanded: boolean;
  onToggle: () => void;
  children?: ComponentChildren;
}

export function PluginCard({
  name,
  meta,
  dot,
  dotTitle,
  details,
  expanded,
  onToggle,
  children,
}: PluginCardProps) {
  return (
    <article class={expanded ? 'pkg-card plugin-card plugin-card-open' : 'pkg-card plugin-card'}>
      <button class="plugin-card-head" aria-expanded={expanded} onClick={onToggle}>
        <span class="plugin-caret">{expanded ? '▾' : '▸'}</span>
        <span class="mono plugin-card-name">{name}</span>
        <span class="plugin-card-spacer" />
        {dot && (
          <span
            class={dot === 'ok' ? 'plugin-dot plugin-dot-ok' : 'plugin-dot plugin-dot-bad'}
            title={dotTitle}
          />
        )}
        {meta && <span class="pkg-card-size">{meta}</span>}
      </button>
      {expanded && (
        <div class="plugin-card-body">
          {details && <div class="plugin-card-details">{details}</div>}
          {children}
        </div>
      )}
    </article>
  );
}
