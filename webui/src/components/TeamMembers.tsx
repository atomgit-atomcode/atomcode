import { createPortal } from 'preact/compat';
import { useEffect, useRef, useState } from 'preact/hooks';
import { getTeamMembers, getTeamTranscript, type TeamMember, type TeamTranscript } from '../api';
import { useT } from '../settings';

export function TeamMembers({ project, lead, panelHost }: {
  project: string; lead: string; panelHost: HTMLDivElement | null;
}) {
  const t = useT();
  const [members, setMembers] = useState<TeamMember[]>([]);
  const [open, setOpen] = useState(false);
  const [selected, select] = useState<TeamMember | null>(null);
  const root = useRef<HTMLDivElement>(null);
  const trigger = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    const controller = new AbortController();
    let busy = false;
    const refresh = async () => {
      if (busy || document.hidden) return;
      busy = true;
      try {
        const roster = await getTeamMembers(project, lead, controller.signal);
        if (!controller.signal.aborted) {
          setMembers(roster);
          if (!roster.length) {
            setOpen(false);
            select(null);
          }
        }
      } catch {
        // A temporary roster failure leaves the last known entries visible.
      } finally {
        busy = false;
      }
    };
    void refresh();
    const timer = window.setInterval(refresh, 6000);
    document.addEventListener('visibilitychange', refresh);
    return () => {
      controller.abort();
      window.clearInterval(timer);
      document.removeEventListener('visibilitychange', refresh);
    };
  }, [project, lead]);

  useEffect(() => {
    if (!open) return;
    function dismiss(event: PointerEvent) {
      if (!root.current?.contains(event.target as Node)) setOpen(false);
    }
    function escape(event: KeyboardEvent) {
      if (event.key === 'Escape' && root.current?.contains(document.activeElement)) {
        setOpen(false);
        trigger.current?.focus();
      }
    }
    document.addEventListener('pointerdown', dismiss);
    document.addEventListener('keydown', escape);
    return () => {
      document.removeEventListener('pointerdown', dismiss);
      document.removeEventListener('keydown', escape);
    };
  }, [open]);

  function closePanel() {
    select(null);
    trigger.current?.focus();
  }

  if (!members.length) return null;
  const member = selected && (members.find(item => item.id === selected.id) ?? selected);
  return <div class="team-switcher" ref={root}>
    <button ref={trigger} type="button" class="team-trigger" aria-expanded={open}
      aria-controls="team-roster" onClick={() => setOpen(value => !value)}>
      <span>{t('team.button', { count: members.length })}</span><span aria-hidden="true">▾</span>
    </button>
    {open && <div class="team-roster" id="team-roster" aria-label={t('team.title')}>
      <button type="button" class="team-row" aria-current={!member ? 'true' : undefined}
        onClick={() => { setOpen(false); closePanel(); }}>main</button>
      {members.map(item => <button key={item.id} type="button" class="team-row"
        aria-current={member?.id === item.id ? 'true' : undefined}
        onClick={() => { setOpen(false); select(item); }}>
        <span class="team-member-name">{item.name}</span>
        <span class="team-member-status">{t(`team.${item.activity ?? 'offline'}`)}</span>
      </button>)}
    </div>}
    {member && panelHost && createPortal(<MemberTranscript key={member.id} project={project} lead={lead} member={member} onClose={closePanel} />, panelHost)}
  </div>;
}

function MemberTranscript({ project, lead, member, onClose }: {
  project: string; lead: string; member: TeamMember; onClose: () => void;
}) {
  const t = useT();
  const [data, setData] = useState<TeamTranscript | null>(null);
  const [failed, setFailed] = useState(false);
  const [retry, setRetry] = useState(0);
  const close = useRef<HTMLButtonElement>(null);
  const active = member.activity === 'running' || member.activity === 'waiting';
  useEffect(() => { close.current?.focus(); }, []);
  useEffect(() => {
    const controller = new AbortController();
    let busy = false;
    const refresh = async () => {
      if (busy || document.hidden) return;
      busy = true;
      try {
        const next = await getTeamTranscript(project, lead, member.id, controller.signal);
        if (!controller.signal.aborted) {
          setData(next);
          setFailed(false);
        }
      } catch {
        if (!controller.signal.aborted) setFailed(true);
      } finally {
        busy = false;
      }
    };
    void refresh();
    const timer = window.setInterval(refresh, active ? 4000 : 12000);
    document.addEventListener('visibilitychange', refresh);
    return () => {
      controller.abort();
      window.clearInterval(timer);
      document.removeEventListener('visibilitychange', refresh);
    };
  }, [project, lead, member.id, active, retry]);

  return <aside class="team-transcript" aria-labelledby="team-transcript-heading" onKeyDown={event => {
    if (event.key === 'Escape' && !event.defaultPrevented) {
      event.stopPropagation();
      onClose();
    }
  }}>
    <header class="team-transcript-header">
      <div>
        <h2 id="team-transcript-heading">{member.name}</h2>
        <div class="team-member-status">{member.role} · {t(`team.${member.activity ?? 'offline'}`)} · {t('team.readOnly')}</div>
      </div>
      <button type="button" class="ghost-btn" ref={close} onClick={onClose} aria-label={t('settings.close')}>×</button>
    </header>
    <div class="team-transcript-body">
      {failed && <div class="team-transcript-note" role="status">
        {t('team.failed')} <button type="button" class="ghost-btn" onClick={() => { setFailed(false); setRetry(value => value + 1); }}>{t('cards.retry')}</button>
      </div>}
      {!data && !failed && <p class="team-transcript-note" role="status">{t('chat.loadingHistory')}</p>}
      {data?.truncated && <p class="team-transcript-note">{t('team.truncated')}</p>}
      {data && !data.entries.length && !failed && <p class="team-transcript-note">{t('team.empty')}</p>}
      {data?.entries.map(entry => entry.kind === 'tool_result' || entry.kind === 'tool_error'
        ? <details class="team-transcript-entry is-tool" key={entry.seq}>
            <summary>{t(`team.${entry.kind}`)}</summary>
            <div class="team-entry-text">{entry.text}</div>
          </details>
        : <section class={`team-transcript-entry${entry.kind === 'tool' ? ' is-tool' : ''}`} key={entry.seq}>
            <div class="team-entry-label">{t(`team.${entry.kind}`)}</div>
            <div class="team-entry-text">{entry.text}</div>
          </section>)}
    </div>
  </aside>;
}
