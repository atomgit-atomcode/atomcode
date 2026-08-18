import type { ComponentChildren } from 'preact';
import { useEffect, useRef } from 'preact/hooks';

interface InteractionDockProps {
  title: string;
  icon: ComponentChildren;
  children: ComponentChildren;
  footer?: ComponentChildren;
  tag?: string;
  close?: {
    label: string;
    disabled?: boolean;
    onClick: () => void;
  };
}

/**
 * Shared non-modal surface for interactions that park the active turn.
 *
 * The parent mounts this in the composer seat, leaving the transcript visible
 * and scrollable. Request ownership and response lifecycles stay with each
 * concrete card; this component owns presentation only.
 */
export function InteractionDock({ title, icon, children, footer, tag, close }: InteractionDockProps) {
  const rootRef = useRef<HTMLElement>(null);

  useEffect(() => {
    const root = rootRef.current;
    const preferred = root?.querySelector<HTMLElement>('[data-interaction-autofocus]:not([disabled])');
    (preferred ?? root)?.focus({ preventScroll: true });
  }, []);

  return (
    <section
      ref={rootRef}
      class="interaction-dock-card permission-card"
      role="region"
      aria-label={title}
      tabIndex={-1}
    >
      <div class="interaction-dock-header modal-header permission-header">
        <span class="permission-logo" aria-hidden="true">{icon}</span>
        <h3 class="permission-title">{title}</h3>
        {tag && <span class="modal-tag permission-tag">{tag}</span>}
        {close && (
          <button
            type="button"
            class="ghost-btn modal-close"
            disabled={close.disabled}
            onClick={close.onClick}
            aria-label={close.label}
            title={close.label}
          >
            ×
          </button>
        )}
      </div>
      <div class="interaction-dock-body modal-body">{children}</div>
      {footer && <div class="interaction-dock-footer modal-footer permission-footer">{footer}</div>}
    </section>
  );
}
