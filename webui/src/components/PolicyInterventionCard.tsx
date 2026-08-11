import { useState } from 'preact/hooks';
import type { PolicyInterventionEvent, PolicyRecoveryAction } from '../api';
import type { MsgKey } from '../i18n';
import { useT } from '../settings';

export const COMPLETE_EXTERNALLY_MESSAGE =
  'I completed the blocked authenticated step outside AtomCode. Continue from the next safe step without requesting or handling the credential.';
export const SKIP_STEP_MESSAGE =
  'Skip the blocked authenticated step and continue with the remaining safe work. Do not request, read, or handle the credential.';

interface PolicyInterventionCardProps {
  intervention: PolicyInterventionEvent;
  onSubmit: (message: string) => Promise<boolean> | boolean;
  onClose: () => void;
  // The recovery event precedes the authoritative terminal by design. While the
  // runtime is still busy the actions are disabled (no submit before idle), but
  // the card stays mounted so its own loading/error state can render during and
  // after a recovery submit.
  busy?: boolean;
}

interface ActionCopy {
  label: MsgKey;
  description: MsgKey;
}

const ACTION_COPY: Record<PolicyRecoveryAction, ActionCopy> = {
  complete_externally: {
    label: 'policyRecovery.complete',
    description: 'policyRecovery.completeDesc',
  },
  skip_step: {
    label: 'policyRecovery.skip',
    description: 'policyRecovery.skipDesc',
  },
  view_safe_instructions: {
    label: 'policyRecovery.instructions',
    description: 'policyRecovery.instructionsDesc',
  },
  end_task: {
    label: 'policyRecovery.end',
    description: 'policyRecovery.endDesc',
  },
};

function isPolicyRecoveryAction(action: string): action is PolicyRecoveryAction {
  return action === 'complete_externally'
    || action === 'skip_step'
    || action === 'view_safe_instructions'
    || action === 'end_task';
}

export function PolicyInterventionCard({
  intervention,
  onSubmit,
  onClose,
  busy = false,
}: PolicyInterventionCardProps) {
  const t = useT();
  const [loading, setLoading] = useState(false);
  const [showInstructions, setShowInstructions] = useState(false);
  const [error, setError] = useState(false);
  const disabled = loading || busy;
  const actions = intervention.code === 'credential_shell_blocked'
    ? intervention.actions.filter(isPolicyRecoveryAction)
    : [];
  if (!actions.includes('end_task')) actions.push('end_task');

  async function choose(action: PolicyRecoveryAction) {
    if (disabled) return;
    if (action === 'view_safe_instructions') {
      setShowInstructions(true);
      return;
    }
    if (action === 'end_task') {
      onClose();
      return;
    }

    setLoading(true);
    setError(false);
    try {
      const accepted = await onSubmit(
        action === 'complete_externally'
          ? COMPLETE_EXTERNALLY_MESSAGE
          : SKIP_STEP_MESSAGE,
      );
      if (!accepted) throw new Error('recovery submit was not accepted');
      onClose();
    } catch {
      setLoading(false);
      setError(true);
    }
  }

  return (
    <div class="modal-overlay" onClick={(event) => event.stopPropagation()}>
      <div class="modal-card permission-card" role="dialog" aria-modal="true" aria-labelledby="policy-recovery-title">
        <div class="modal-header permission-header">
          <span class="permission-logo" aria-hidden="true">⚠</span>
          <h3 id="policy-recovery-title" class="permission-title">{t('policyRecovery.title')}</h3>
        </div>
        <div class="modal-body">
          <p class="permission-lead">{t('policyRecovery.question')}</p>
          <div class="field-group policy-recovery-actions">
            {actions.map((action) => (
                <button
                  key={action}
                  type="button"
                  class="user-input-option policy-recovery-action"
                  disabled={disabled}
                  onClick={() => void choose(action)}
                >
                  <span class="user-input-option-body">
                    <span class="user-input-option-label">{t(ACTION_COPY[action].label)}</span>
                    <span class="user-input-option-desc">{t(ACTION_COPY[action].description)}</span>
                  </span>
                </button>
              ))}
          </div>
          {showInstructions && <p class="policy-recovery-instructions">{t('policyRecovery.safeInstructions')}</p>}
          {error && <p class="user-input-error">{t('policyRecovery.submitError')}</p>}
        </div>
      </div>
    </div>
  );
}
