/** One current UI state per session, including views awaiting an older save. */
export class WorkbenchCache<T> extends Map<string, T> {
  private observers = new Map<string, Set<(state: T) => void>>();
  hasSubscribers(session: string): boolean {
    return !!this.observers.get(session)?.size;
  }
  override set(session: string, state: T): this {
    super.set(session, state);
    this.observers.get(session)?.forEach((notify) => notify(state));
    return this;
  }
  subscribe(session: string, notify: (state: T) => void): () => void {
    const observers = this.observers.get(session) || new Set();
    observers.add(notify);
    this.observers.set(session, observers);
    return () => {
      observers.delete(notify);
      if (!observers.size) this.observers.delete(session);
    };
  }
}
