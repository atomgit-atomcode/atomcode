import React, { useState, useEffect, useCallback } from 'react';

interface SlashCommand {
  name: string;
  label: string;
  description: string;
}

const slashCommands: SlashCommand[] = [
  { name: 'login', label: '/login', description: 'Sign in with AtomGit' },
  { name: 'codingplan', label: '/codingplan', description: 'Sync CodingPlan models' },
  { name: 'explain', label: '/explain', description: 'Explain selected code' },
  { name: 'fix', label: '/fix', description: 'Fix issues in code' },
  { name: 'test', label: '/test', description: 'Write tests' },
  { name: 'refactor', label: '/refactor', description: 'Refactor code' },
  { name: 'docs', label: '/docs', description: 'Add documentation' },
  { name: 'review', label: '/review', description: 'Review code' },
  { name: 'optimize', label: '/optimize', description: 'Optimize performance' },
];

interface SlashPickerProps {
  filter: string;
  onSelect: (command: string) => void;
  onClose: () => void;
}

export function SlashPicker({ filter, onSelect, onClose }: SlashPickerProps) {
  const [activeIndex, setActiveIndex] = useState(0);

  const filtered = slashCommands.filter((cmd) =>
    cmd.name.startsWith(filter.toLowerCase()),
  );

  useEffect(() => {
    setActiveIndex(0);
  }, [filter]);

  const handleKeyDown = useCallback(
    (e: KeyboardEvent) => {
      if (filtered.length === 0) return;
      // Don't intercept keystrokes while IME is composing
      if (e.isComposing || e.key === 'Process') return;

      if (e.key === 'ArrowDown') {
        e.preventDefault();
        setActiveIndex((i) => (i + 1) % filtered.length);
      } else if (e.key === 'ArrowUp') {
        e.preventDefault();
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

  if (filtered.length === 0) return null;

  return (
    <div className="slash-picker">
      {filtered.map((cmd, i) => (
        <button
          key={cmd.name}
          className={`slash-item${i === activeIndex ? ' active' : ''}`}
          onMouseEnter={() => setActiveIndex(i)}
          onClick={() => onSelect(cmd.label)}
        >
          <span className="slash-item-label">{cmd.label}</span>
          <span className="slash-item-desc">{cmd.description}</span>
        </button>
      ))}
    </div>
  );
}
