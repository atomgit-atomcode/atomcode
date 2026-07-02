import React, { useState, useEffect, useCallback, useRef } from 'react';
import { SkillInfo } from '../state/types';
import { MsgKey, useT } from '../i18n';
import { ensureActiveDescendantVisible } from '../utils/atMention';

interface SlashCommand {
  name: string;
  label: string;
  descriptionKey: MsgKey;
}

const slashCommands: SlashCommand[] = [
  { name: 'login', label: '/login', descriptionKey: 'slash.login' },
  { name: 'logout', label: '/logout', descriptionKey: 'slash.logout' },
  { name: 'whoami', label: '/whoami', descriptionKey: 'slash.whoami' },
  { name: 'status', label: '/status', descriptionKey: 'slash.status' },
  { name: 'config', label: '/config', descriptionKey: 'slash.config' },
  { name: 'reload', label: '/reload', descriptionKey: 'slash.reload' },
];

interface SlashPickerProps {
  filter: string;
  skills?: SkillInfo[];
  onSelect: (command: string) => void;
  onClose: () => void;
}

export function SlashPicker({ filter, skills = [], onSelect, onClose }: SlashPickerProps) {
  const [activeIndex, setActiveIndex] = useState(0);
  const [allowHoverHighlight, setAllowHoverHighlight] = useState(true);
  const listRef = useRef<HTMLDivElement>(null);
  const t = useT();

  const localNames = new Set(slashCommands.map((cmd) => cmd.name));
  const skillCommands: Array<{ name: string; label: string; description: string }> = skills
    .filter((skill) => skill.name && !localNames.has(skill.name))
    .map((skill) => ({
      name: skill.name,
      label: `/${skill.name}`,
      description: skill.description || t('slash.skill'),
    }));
  const commands = [
    ...slashCommands.map((cmd) => ({ name: cmd.name, label: cmd.label, description: t(cmd.descriptionKey) })),
    ...skillCommands,
  ];
  const lowerFilter = filter.toLowerCase();
  const filtered = commands.filter((cmd) => cmd.name.toLowerCase().startsWith(lowerFilter));

  useEffect(() => {
    setActiveIndex(0);
    setAllowHoverHighlight(true);
  }, [filter]);

  const handleKeyDown = useCallback(
    (e: KeyboardEvent) => {
      if (filtered.length === 0) return;
      // Don't intercept keystrokes while IME is composing
      if (e.isComposing || e.key === 'Process') return;

      if (e.key === 'ArrowDown') {
        e.preventDefault();
        setAllowHoverHighlight(false);
        setActiveIndex((i) => (i + 1) % filtered.length);
      } else if (e.key === 'ArrowUp') {
        e.preventDefault();
        setAllowHoverHighlight(false);
        setActiveIndex((i) => (i - 1 + filtered.length) % filtered.length);
      } else if (e.key === 'Enter' || e.key === 'Tab') {
        e.preventDefault();
        onSelect(filtered[activeIndex].label);
      } else if (e.key === 'Escape') {
        e.preventDefault();
        onClose();
      }
    },
    [filtered, activeIndex, onSelect, onClose],
  );

  useEffect(() => {
    document.addEventListener('keydown', handleKeyDown);
    return () => document.removeEventListener('keydown', handleKeyDown);
  }, [handleKeyDown]);

  useEffect(() => {
    requestAnimationFrame(() => {
      const container = listRef.current;
      const active = container?.querySelector<HTMLButtonElement>('.slash-item.active');
      if (container && active) ensureActiveDescendantVisible(container, active);
    });
  }, [activeIndex, filtered.length]);

  if (filtered.length === 0) return null;

  return (
    <div className={`slash-picker${allowHoverHighlight ? ' allow-hover' : ''}`} ref={listRef}>
      {filtered.map((cmd, i) => (
        <button
          key={cmd.name}
          className={`slash-item${i === activeIndex ? ' active' : ''}`}
          onMouseMove={() => {
            setAllowHoverHighlight(true);
            setActiveIndex(i);
          }}
          onClick={() => onSelect(cmd.label)}
        >
          <span className="slash-item-label">{cmd.label}</span>
          <span className="slash-item-desc">{cmd.description}</span>
        </button>
      ))}
    </div>
  );
}
