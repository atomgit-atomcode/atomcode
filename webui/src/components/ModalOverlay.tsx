import type { ComponentChildren } from 'preact';

/** Management dialogs dismiss only through their explicit controls. */
export function ModalOverlay({ children, class: extraClass }: {
  children: ComponentChildren;
  class?: string;
}) {
  return <div class={'modal-overlay' + (extraClass ? ` ${extraClass}` : '')} onKeyDown={event => {
    // Keep Escape inside the modal, without interfering with IME cancellation
    // or a nested control that already handled the key.
    if (event.key === 'Escape' && !event.isComposing) {
      event.preventDefault();
      event.stopPropagation();
    }
  }}>{children}</div>;
}
