/** Composer-local navigation over loaded user turns. Never writes session history. */
export class PromptHistory<T> {
  private scope = '';
  private entries: readonly T[] = [];
  private index = 0;
  private draft = '';
  private value: string | null = null;

  reset() {
    this.entries = [];
    this.value = null;
  }

  move(scope: string, entries: readonly T[], input: string, direction: -1 | 1):
    { entry: T } | { text: string } | null {
    if (scope !== this.scope || this.value !== input) {
      this.reset();
      this.scope = scope;
    }
    if (this.value === null) {
      if (direction === 1 || !entries.length) return null;
      this.entries = entries;
      this.index = entries.length;
      this.draft = input;
      this.value = input;
    }
    const index = Math.max(0, Math.min(this.entries.length, this.index + direction));
    if (index === this.index) return null;
    this.index = index;
    if (index === this.entries.length) {
      const text = this.draft;
      this.reset();
      return { text };
    }
    return { entry: this.entries[index] };
  }

  accept(text: string) { this.value = text; }
}

/** Leave selections, modified chords and ordinary multiline cursor movement alone. */
export function promptHistoryDirection(event: {
  key: string; shiftKey: boolean; ctrlKey: boolean; altKey: boolean; metaKey: boolean;
}, text: string, start: number, end: number): -1 | 1 | null {
  if (event.shiftKey || event.ctrlKey || event.altKey || event.metaKey || start !== end) return null;
  if (event.key === 'ArrowUp' && !text.slice(0, start).includes('\n')) return -1;
  if (event.key === 'ArrowDown' && !text.slice(end).includes('\n')) return 1;
  return null;
}
