// New-chat landing suggestions: fetches dynamic, project-based suggestions for
// the current working dir and renders them as clickable chips. Clicking a chip
// fills the input (handled by the parent). Falls back to static scenario chips
// when the backend returns nothing or errors. Module-level cache (keyed by cwd)
// avoids refetching on remount within a session; the backend caches too.

import { useEffect, useState } from 'preact/hooks';
import { getSuggestions, Suggestion } from '../api';
import { useT } from '../settings';

/** Per-cwd cache of real (non-fallback) suggestions, survives remounts. */
const cache = new Map<string, Suggestion[]>();

interface SuggestionsProps {
  cwd: string;
  /** Called with the full prompt when a chip is clicked. */
  onPick: (prompt: string) => void;
}

export function Suggestions({ cwd, onPick }: SuggestionsProps) {
  const t = useT();
  const [items, setItems] = useState<Suggestion[] | null>(() => cache.get(cwd) ?? null);
  const [loading, setLoading] = useState(false);

  const fallback: Suggestion[] = [
    { label: t('landing.fallback.readLabel'), prompt: t('landing.fallback.readPrompt') },
    { label: t('landing.fallback.bugLabel'), prompt: t('landing.fallback.bugPrompt') },
    { label: t('landing.fallback.testLabel'), prompt: t('landing.fallback.testPrompt') },
    { label: t('landing.fallback.explainLabel'), prompt: t('landing.fallback.explainPrompt') },
  ];

  async function load(refresh: boolean) {
    setLoading(true);
    try {
      const res = await getSuggestions(refresh);
      if (res.suggestions && res.suggestions.length > 0) {
        cache.set(cwd, res.suggestions);
        setItems(res.suggestions);
      } else {
        // Empty result → show fallback but don't cache, so we retry next time.
        setItems(fallback);
      }
    } catch {
      setItems(fallback);
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => {
    const cached = cache.get(cwd);
    if (cached) {
      setItems(cached);
      return;
    }
    setItems(null);
    load(false);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [cwd]);

  const showSkeleton = loading && items === null;
  const chips = items ?? [];

  return (
    <div class="landing-suggestions">
      {showSkeleton
        ? [0, 1, 2, 3].map((i) => <span key={i} class="suggest-chip suggest-skeleton" />)
        : chips.map((s, i) => (
            <button
              key={i}
              class="suggest-chip"
              onClick={() => onPick(s.prompt)}
              title={s.prompt}
            >
              {s.label}
            </button>
          ))}
      <button
        class="suggest-refresh"
        onClick={() => load(true)}
        disabled={loading}
        title={t('landing.refresh')}
        aria-label={t('landing.refresh')}
      >
        <svg
          width="14"
          height="14"
          viewBox="0 0 16 16"
          fill="none"
          aria-hidden="true"
          class={loading ? 'spin' : ''}
        >
          <path
            d="M13.5 8a5.5 5.5 0 1 1-1.6-3.9"
            stroke="currentColor"
            stroke-width="1.3"
            stroke-linecap="round"
          />
          <path
            d="M13.5 2.5V5H11"
            stroke="currentColor"
            stroke-width="1.3"
            stroke-linecap="round"
            stroke-linejoin="round"
          />
        </svg>
      </button>
    </div>
  );
}
