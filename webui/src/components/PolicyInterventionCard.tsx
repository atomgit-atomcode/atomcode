import { useState } from 'preact/hooks';
import type { PolicyInterventionEvent, PolicyRecoveryAction } from '../api';
import type { MsgKey } from '../i18n';
import { useT } from '../settings';
import { InteractionDock } from './InteractionDock';

interface PolicyInterventionCardProps {
  intervention: PolicyInterventionEvent;
  onResolve: (
    action: Exclude<PolicyRecoveryAction, 'view_safe_instructions'>,
  ) => Promise<boolean> | boolean;
  onClose: () => void;
  // The recovery event precedes the authoritative terminal by design. While the
  // runtime is still busy the actions are disabled (no submit before idle), but
  // the card stays mounted so its own loading/error state can render during and
  // after a recovery decision.
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
  onResolve,
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
    // This is a structured control-plane acknowledgement, never a model prompt.
    // A new model turn could reconstruct credential material from prior context.
    setLoading(true);
    setError(false);
    try {
      const accepted = await onResolve(action);
      if (!accepted) throw new Error('policy intervention resolution was not accepted');
      onClose();
    } catch {
      setLoading(false);
      setError(true);
    }
  }

  return (
    <InteractionDock title={t('policyRecovery.title')} icon="⚠">
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
    </InteractionDock>
  );
}
