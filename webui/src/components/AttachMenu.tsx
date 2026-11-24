// Attachment actions and user-invocable skills for the composer.

import { useEffect, useLayoutEffect, useRef, useState } from 'preact/hooks';
import { placeAttachPopover } from '../lib/attachPopoverPlacement';
import { useSkills, refreshSkills } from '../skills';
import { useT } from '../settings';
import { useComposerToolbarHold } from '../lib/composerFocus';

interface AttachMenuProps {
/** Insert text into the chat input at the cursor (used for `/<skill> `).
 *  When the second arg `replaceSkill` is true, the implementation should
 *  strip any existing skill prefix before inserting. */
onInsert: (text: string, replaceSkill?: boolean) => void;
  /** Open the server-side file picker. */
  onPickFile: () => void;
  onImportPatch?: (file: File) => void;
  onManageSpecs?:()=>void;
  patchDisabled?: boolean;
  /** Attach picked image files (native picker → base64 handled by parent). */
  onAddImages: (files: FileList) => void;
}

function PlusIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" fill="none" aria-hidden="true">
      <line x1="8" y1="3" x2="8" y2="13" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" />
      <line x1="3" y1="8" x2="13" y2="8" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" />
    </svg>
  );
}

function ImageIcon() {
  return (
    <svg width="15" height="15" viewBox="0 0 16 16" fill="none" aria-hidden="true">
      <rect x="2" y="3" width="12" height="10" rx="1.5" stroke="currentColor" stroke-width="1.2" />
      <circle cx="5.5" cy="6.5" r="1.1" fill="currentColor" />
      <path d="M3 12l3.5-3.5 2.5 2.5 2-2L14 11.5" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round" />
    </svg>
  );
}

function FileIcon() {
  return (
    <svg width="15" height="15" viewBox="0 0 16 16" fill="none" aria-hidden="true">
      <path d="M4 2h5l3 3v9H4z" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round" />
      <path d="M9 2v3h3" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round" />
    </svg>
  );
}

function SkillIcon() {
  return (
    <svg width="15" height="15" viewBox="0 0 16 16" fill="none" aria-hidden="true">
      <path d="M8 1.8l1.7 3.5 3.8.5-2.8 2.7.7 3.8L8 10.9 4.6 12.6l.7-3.8L2.5 5.8l3.8-.5z" stroke="currentColor" stroke-width="1.1" stroke-linejoin="round" />
    </svg>
  );
}

export function AttachMenu({ onInsert, onPickFile, onAddImages, onImportPatch, patchDisabled, onManageSpecs }: AttachMenuProps) {
  const t = useT();
  const [open, setOpen] = useState(false);
  const { items: skills, loading, failed, error: skillsError } = useSkills(open);
  const rootRef = useRef<HTMLDivElement>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  const popover = useRef<HTMLDivElement>(null);
  const fixedRows = useRef<HTMLDivElement>(null);
  const skillList = useRef<HTMLDivElement>(null);
  const patchInputRef = useRef<HTMLInputElement>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const toolbarHold = useComposerToolbarHold();

  // A viewport-positioned menu can escape the welcome page's scroll clip
  // while remaining inside the composer for focus and outside-click handling.
  useLayoutEffect(() => {
    if (!open) return;
    const button = trigger.current;
    const panel = popover.current;
    const fixed = fixedRows.current;
    const list = skillList.current;
    if (!button || !panel || !fixed || !list) return;
    const viewport = window.visualViewport;

    const position = (event?: Event) => {
      if (event?.type === 'scroll' && event.target instanceof Node && panel.contains(event.target)) return;
      const top = viewport?.offsetTop ?? 0;
      const left = viewport?.offsetLeft ?? 0;
      const bounds = {
        top, left,
        bottom: top + (viewport?.height ?? window.innerHeight),
        right: left + (viewport?.width ?? window.innerWidth),
      };
      const panelCss = getComputedStyle(panel);
      const chrome = [panelCss.paddingTop, panelCss.paddingBottom, panelCss.borderTopWidth, panelCss.borderBottomWidth]
        .reduce((sum, value) => sum + parseFloat(value), 0);
      const skillHeight = Math.min(list.scrollHeight, parseFloat(getComputedStyle(list).maxHeight));
      const preferredHeight = fixed.offsetHeight + skillHeight + chrome;
      const placement = placeAttachPopover(button.getBoundingClientRect(), bounds, preferredHeight);
      panel.style.left = `${placement.left}px`;
      panel.style.width = `${placement.width}px`;
      panel.style.maxHeight = `${placement.maxHeight}px`;
      panel.style.top = placement.opensBelow ? `${placement.edge}px` : 'auto';
      panel.style.bottom = placement.opensBelow ? 'auto' : `${window.innerHeight - placement.edge}px`;
    };

    position();
    const observer = typeof ResizeObserver === 'undefined' ? null : new ResizeObserver(() => position());
    observer?.observe(button);
    observer?.observe(fixed);
    window.addEventListener('resize', position);
    window.addEventListener('scroll', position, true);
    viewport?.addEventListener('resize', position);
    viewport?.addEventListener('scroll', position);
    return () => {
      observer?.disconnect();
      window.removeEventListener('resize', position);
      window.removeEventListener('scroll', position, true);
      viewport?.removeEventListener('resize', position);
      viewport?.removeEventListener('scroll', position);
    };
  }, [open, skills, loading, failed, skillsError]);

  // Close on outside click.
  useEffect(() => {
    if (!open) return;
    const h = (e: MouseEvent) => {
      if (rootRef.current && !rootRef.current.contains(e.target as Node)) {
        setOpen(false);
      }
    };
    document.addEventListener('mousedown', h);
    return () => document.removeEventListener('mousedown', h);
  }, [open]);

  function pickSkill(name: string) {
    onInsert(`/${name} `, true);
    setOpen(false);
  }

  return (
    <div class="attach-menu" ref={rootRef} onKeyDown={event => {
      if (!open || event.key !== 'Escape') return;
      event.stopPropagation();
      setOpen(false);
      trigger.current?.focus();
    }}>
      <button
        type="button"
        ref={trigger}
        class="attach-btn"
        onPointerDown={toolbarHold.onPointerDown}
        onPointerUp={toolbarHold.onPointerUp}
        onPointerCancel={toolbarHold.onPointerCancel}
        onClick={() => setOpen((o) => !o)}
        title={t('attach.menu')}
        aria-label={t('attach.menu')}
        aria-expanded={open}
      >
        <PlusIcon />
      </button>

      <input
        ref={fileInputRef}
        type="file"
        accept="image/*"
        multiple
        style="display:none"
        onChange={(e) => {
          const input = e.target as HTMLInputElement;
          if (input.files && input.files.length) onAddImages(input.files);
          input.value = ''; // allow re-selecting the same file
        }}
      />

      <input ref={patchInputRef} type="file" accept=".patch,.diff" style="display:none" aria-label={t('patch.upload')}
        onChange={event => { const input = event.currentTarget; const file = input.files?.[0]; input.value = ''; if (file) onImportPatch?.(file); }} />
      {open && (
        <div class="attach-popover" ref={popover}>
          <div class="attach-fixed-rows" ref={fixedRows}>
            <button
              class="attach-row"
              onClick={() => {
                setOpen(false);
                fileInputRef.current?.click();
              }}
            >
              <ImageIcon />
              <span class="attach-row-label">{t('attach.image')}</span>
            </button>

            <button
              class="attach-row"
              onClick={() => {
                setOpen(false);
                onPickFile();
              }}
            >
              <FileIcon />
              <span class="attach-row-label">{t('attach.file')}</span>
            </button>

            {onImportPatch && <button class="attach-row" title={patchDisabled ? t('patch.needReady') : undefined} disabled={patchDisabled} onClick={() => { setOpen(false); patchInputRef.current?.click(); }}>
              <FileIcon /><span class="attach-row-label">{t('patch.upload')}</span>
            </button>}
            {onManageSpecs&&<button type="button" class="attach-row" onClick={()=>{setOpen(false);onManageSpecs();}}><FileIcon/><span class="attach-row-label">{t('spec.manage')}</span></button>}
            <div class="attach-divider" />

            <div class="attach-section-label">
              <SkillIcon />
              <span>{t('attach.skill')}</span>
            </div>
          </div>
          <div class="attach-skill-list" ref={skillList}>
            {failed && <button class="attach-row" onClick={(e) => { e.stopPropagation(); void refreshSkills(); }}>{t(skills?.length ? 'skills.refreshFailedRetry' : 'skills.loadFailedRetry')}{skillsError && <small>{skillsError}</small>}</button>}
            {loading && <div class="attach-note">{t('attach.skillsLoading')}</div>}
            {!loading && !failed && skills !== null && skills.length === 0 && (
              <div class="attach-note">{t('attach.skillsEmpty')}</div>
            )}
            {!loading &&
              [...(skills ?? [])].sort((a, b) => a.name.localeCompare(b.name)).map((s) => (
                <button
                  key={s.name}
                  class="attach-skill-row"
                  onClick={() => pickSkill(s.name)}
                  title={s.description}
                >
                  <span class="attach-skill-name">{s.display_name ? `${s.display_name} · /${s.name}` : `/${s.name}`}</span>
                  {s.description && (
                    <span class="attach-skill-desc">{s.description}</span>
                  )}
                </button>
              ))}
          </div>
        </div>
      )}
    </div>
  );
}
