// Input "+" attach menu: opens a popover above the button with three actions —
// upload image (disabled / coming soon), upload file (opens FilePicker via
// parent), and choose a user-invocable skill (inserts `/<name> ` into the
// input). Skills are fetched lazily on first open.

import { useEffect, useRef, useState } from 'preact/hooks';
import { getSkills, SkillInfo } from '../api';
import { useT } from '../settings';

interface AttachMenuProps {
/** Insert text into the chat input at the cursor (used for `/<skill> `).
 *  When the second arg `replaceSkill` is true, the implementation should
 *  strip any existing skill prefix before inserting. */
onInsert: (text: string, replaceSkill?: boolean) => void;
  /** Open the server-side file picker. */
  onPickFile: () => void;
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

export function AttachMenu({ onInsert, onPickFile, onAddImages }: AttachMenuProps) {
  const t = useT();
  const [open, setOpen] = useState(false);
  const [skills, setSkills] = useState<SkillInfo[] | null>(null);
  const [loading, setLoading] = useState(false);
  const rootRef = useRef<HTMLDivElement>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);

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

  // Lazily fetch skills the first time the menu opens.
  useEffect(() => {
    if (!open || skills !== null || loading) return;
    setLoading(true);
    getSkills()
      .then(setSkills)
      .catch(() => setSkills([]))
      .finally(() => setLoading(false));
  }, [open, skills, loading]);

  function pickSkill(name: string) {
    onInsert(`/${name} `, true);
    setOpen(false);
  }

  return (
    <div class="attach-menu" ref={rootRef}>
      <button
        class="attach-btn"
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

      {open && (
        <div class="attach-popover">
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

          <div class="attach-divider" />

          <div class="attach-section-label">
            <SkillIcon />
            <span>{t('attach.skill')}</span>
          </div>
          <div class="attach-skill-list">
            {loading && <div class="attach-note">{t('attach.skillsLoading')}</div>}
            {!loading && skills !== null && skills.length === 0 && (
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
                  <span class="attach-skill-name">/{s.name}</span>
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
