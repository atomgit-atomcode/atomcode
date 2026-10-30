import { useEffect, useRef, useState } from 'preact/hooks';
import { createTimelineArchive } from './timelineArchive';
import { archiveCandidates, archivedMessage, replaceArchived, residentStart, segmentMessages, type TimelineMessage } from './timelineMemory';
import { randomId } from './randomId';

/** Cold payloads are replaced only after their archive transaction commits.
 * Object identity fences writes against session switches, reconnect snapshots,
 * and streaming updates that happened while IndexedDB was writing. */
export function useTimelineMessages<T extends TimelineMessage>(resident: number,
  summarize: (message: T, key: string) => T = archivedMessage, byteBudget = 8 * 1024 * 1024) {
  const [messages, setState] = useState<T[]>([]);
  const archive = useRef<ReturnType<typeof createTimelineArchive<T>> | null>(null);
  if (!archive.current) archive.current = createTimelineArchive<T>();
  const prefix = useRef('');
  if (!prefix.current) prefix.current = randomId();
  const serial = useRef(0);
  const writing = useRef(false);
  const unavailable = useRef(false);
  const alive = useRef(true);
  const latest = useRef(messages); latest.current = messages;
  const pruneTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const [revision, setRevision] = useState(0);

  function setMessages(update: T[] | ((current: T[]) => T[])) {
    setState(current => segmentMessages(typeof update === 'function' ? update(current) : update));
  }
  function hold(snapshot: readonly T[]) { return archive.current!.pin(snapshot.flatMap(message => message.archiveKey ? [message.archiveKey] : [])); }
  function read(message: T): Promise<T> {
    return message.archiveKey ? archive.current!.get(message.archiveKey) : Promise.resolve(message);
  }

  useEffect(() => {
    if (writing.current || unavailable.current) return;
    const end = residentStart(messages, resident, byteBudget);
    const entries = archiveCandidates(messages, end).map(value => ({ key: `${prefix.current}:${++serial.current}`, value }));
    if (!entries.length) return;
    writing.current = true;
    void archive.current!.put(entries).then(() => {
      if (!alive.current) return;
      setState(current => replaceArchived(current, entries, summarize));
    }).catch(error => {
      if (!alive.current) return;
      // Keep the original payloads when storage is denied/full. DOM windowing
      // still applies; losing historical output to meet a quota is not allowed.
      unavailable.current = true;
      console.warn('Could not archive older timeline messages; preserving them in memory', error);
    }).finally(() => {
      writing.current = false;
      if (alive.current) setRevision(value => value + 1);
    });
  }, [messages, resident, byteBudget, revision]);

  useEffect(() => {
    // Coalesce cleanup while tokens stream; a snapshot replacement must not
    // leave all of the previous snapshot's payloads resident on disk forever.
    if (pruneTimer.current !== null) return;
    pruneTimer.current = setTimeout(() => {
      pruneTimer.current = null;
      const keys = new Set(latest.current.flatMap(message => message.archiveKey ? [message.archiveKey] : []));
      let through = 0;
      for (const key of keys) through = Math.max(through, Number(key.slice(key.lastIndexOf(':') + 1)) || 0);
      void archive.current!.prune(keys, through).catch(() => { /* retry on the next update */ });
    }, 1000);
  }, [messages]);
  useEffect(() => () => { alive.current = false; if (pruneTimer.current !== null) clearTimeout(pruneTimer.current); archive.current?.close(); }, []);
  return { messages, setMessages, read, hold };
}

/** Keep only the current page's hydrated payloads, even while live output arrives. */
export function useTimelinePage<T extends TimelineMessage>(messages: T[], start: number, end: number,
  read: (message: T) => Promise<T>) {
  const page = messages.slice(start, end);
  const signature = page.filter(message => message.archiveKey).map(message => message.archiveKey).join('|');
  const latest = useRef({ page, read }); latest.current = { page, read };
  const [loaded, setLoaded] = useState<Map<string, T>>(new Map());
  const [error, setError] = useState('');
  const [retry, setRetry] = useState(0);
  useEffect(() => {
    let cancelled = false;
    const { page, read } = latest.current;
    const archived = page.filter(message => message.archiveKey);
    const keys = new Set(archived.map(message => message.archiveKey!));
    setLoaded(current => new Map([...current].filter(([key]) => keys.has(key))));
    setError('');
    void Promise.all(archived.map(async message => [message.archiveKey!, await read(message)] as const))
      .then(entries => { if (!cancelled) setLoaded(new Map(entries)); })
      .catch(reason => { if (!cancelled) setError(String(reason)); });
    return () => { cancelled = true; };
  }, [signature, retry]);
  const items = page.map((message, offset) => ({ origIdx: start + offset,
    msg: message.archiveKey ? loaded.get(message.archiveKey) : message }));
  return { items, error, loading: items.some(item => !item.msg), retry: () => setRetry(value => value + 1) };
}

/** Search immutable archived entries once per query; live tokens must not keep
 * cancelling a long historical search before it can finish. */
export function useTimelineSearch<T extends TimelineMessage>(messages: T[], query: string, enabled: boolean,
  scope: string, read: (message: T) => Promise<T>, textOf: (message: T) => string, hold: (snapshot: readonly T[]) => () => void) {
  const latest = useRef({ messages, read, textOf, hold }); latest.current = { messages, read, textOf, hold };
  const [matches, setMatches] = useState<number[]>([]);
  const [error, setError] = useState('');
  const [loading, setLoading] = useState(false);
  useEffect(() => {
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout>;
    let previous: T[] | null = null;
    const memory = new WeakMap<T, boolean>();
    const archived = new Map<string, boolean>();
    setMatches([]); setError(''); setLoading(Boolean(enabled && query));
    if (!enabled || !query) return;
    async function scan() {
      const { messages: snapshot, read, textOf, hold } = latest.current;
      const release = hold(snapshot);
      try {
        if (snapshot !== previous) {
          const result: number[] = [];
          for (let index = 0; index < snapshot.length; index++) {
            if (cancelled) return;
            const message = snapshot[index];
            let match = message.archiveKey ? archived.get(message.archiveKey) : memory.get(message);
            if (match === undefined) {
              match = textOf(await read(message)).toLowerCase().includes(query);
              if (message.archiveKey) archived.set(message.archiveKey, match); else memory.set(message, match);
            }
            if (match) result.push(index);
          }
          if (cancelled) return;
          previous = snapshot;
          setMatches(current => current.length === result.length && current.every((item, i) => item === result[i]) ? current : result);
          setLoading(false); setError('');
        }
      } catch (reason) { if (!cancelled) { setError(String(reason)); setLoading(false); } }
      finally { release(); }
      if (!cancelled) timer = setTimeout(scan, 250);
    }
    timer = setTimeout(scan, 150);
    return () => { cancelled = true; clearTimeout(timer); };
  }, [query, enabled, scope]);
  return { matches, error, loading };
}
