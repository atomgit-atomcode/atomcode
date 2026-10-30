/** A disposable browser display cache. Session persistence stays in the daemon. */
export interface TimelineArchive<T> {
  put(entries: readonly { key: string; value: T }[]): Promise<void>;
  get(key: string): Promise<T>;
  pin(keys: Iterable<string>): () => void;
  prune(retained: ReadonlySet<string>, throughSerial: number): Promise<void>;
  close(): void;
}
const DATABASE = 'atomcode.timeline-display.v2';
const STORE = 'payloads';
const LEASES = 'owners';
const STALE_AFTER = 7 * 24 * 60 * 60 * 1000;

export function createTimelineArchive<T>(): TimelineArchive<T> {
  const owner = `${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}`;
  let database: Promise<IDBDatabase> | null = null;
  let closed = false;
  let heartbeat: ReturnType<typeof setInterval> | undefined;
  const writes = new Set<Promise<void>>();
  const pins = new Map<string, number>();
  function pin(keys: Iterable<string>) {
    const held = [...keys];
    for (const key of held) pins.set(key, (pins.get(key) ?? 0) + 1);
    return () => { for (const key of held) { const count = (pins.get(key) ?? 1) - 1; if (count) pins.set(key, count); else pins.delete(key); } };
  }
  function removeOwner(tx: IDBTransaction, id: string) {
    const request = tx.objectStore(STORE).index('owner').openKeyCursor(IDBKeyRange.only(id));
    request.onsuccess = () => {
      const cursor = request.result;
      if (cursor) { tx.objectStore(STORE).delete(cursor.primaryKey); cursor.continue(); }
    };
    tx.objectStore(LEASES).delete(id);
  }
  function open() {
    if (closed) return Promise.reject(new Error('Timeline cache closed'));
    if (!database) database = new Promise<IDBDatabase>((resolve, reject) => {
      if (typeof indexedDB === 'undefined') { reject(new Error('Timeline cache unavailable')); return; }
      const request = indexedDB.open(DATABASE, 1);
      request.onupgradeneeded = () => {
        request.result.createObjectStore(STORE).createIndex('owner', 'owner');
        request.result.createObjectStore(LEASES);
      };
      request.onerror = () => reject(request.error);
      request.onblocked = () => reject(new Error('Timeline cache blocked'));
      request.onsuccess = () => {
        const db = request.result;
        if (closed) { db.close(); reject(new Error('Timeline cache closed')); return; }
        // Reclaim crashed/abandoned documents without clearing another tab's
        // active cache. A lease heartbeat keeps a long-running document alive.
        const tx = db.transaction([STORE, LEASES], 'readwrite');
        const cursor = tx.objectStore(LEASES).openCursor();
        cursor.onsuccess = () => {
          const entry = cursor.result;
          if (!entry) return;
          if (Number(entry.value) < Date.now() - STALE_AFTER) removeOwner(tx, String(entry.key));
          entry.continue();
        };
        tx.objectStore(LEASES).put(Date.now(), owner);
        tx.oncomplete = () => {
          if (closed) { removeAndClose(db); reject(new Error('Timeline cache closed')); return; }
          heartbeat = setInterval(() => {
            try { db.transaction(LEASES, 'readwrite').objectStore(LEASES).put(Date.now(), owner); } catch { /* disposed */ }
          }, 60000);
          resolve(db);
        };
        tx.onabort = tx.onerror = () => { db.close(); reject(tx.error ?? new Error('Timeline cache unavailable')); };
      };
    });
    return database;
  }
  function removeAndClose(db: IDBDatabase) {
    try {
      const tx = db.transaction([STORE, LEASES], 'readwrite');
      removeOwner(tx, owner);
      tx.oncomplete = tx.onabort = () => db.close();
    } catch { db.close(); }
  }
  function close() {
    if (closed) return;
    closed = true; clearInterval(heartbeat);
    if (typeof window !== 'undefined') window.removeEventListener('pagehide', pagehide);
    // Wait for pending writes so a committed late batch cannot leak after cleanup.
    if (database) void Promise.allSettled([...writes]).then(() => database!).then(removeAndClose).catch(() => {});
  }
  function pagehide(event: PageTransitionEvent) { if (!event.persisted) close(); }
  if (typeof window !== 'undefined') window.addEventListener('pagehide', pagehide);
  return {
    put(entries) {
      const job = (async () => {
        const db = await open();
        await new Promise<void>((resolve, reject) => {
          const tx = db.transaction([STORE, LEASES], 'readwrite');
          tx.oncomplete = () => resolve();
          tx.onerror = tx.onabort = () => reject(tx.error ?? new Error('Timeline cache write failed'));
          try {
            tx.objectStore(LEASES).put(Date.now(), owner);
            for (const entry of entries) tx.objectStore(STORE).put({ owner, value: entry.value }, entry.key);
          } catch (error) { tx.abort(); reject(error); }
        });
      })();
      writes.add(job);
      void job.then(() => writes.delete(job), () => writes.delete(job));
      return job;
    },
    async get(key) {
      const release = pin([key]);
      try {
        const db = await open();
        return await new Promise<T>((resolve, reject) => {
          const tx = db.transaction(STORE, 'readonly');
          const request = tx.objectStore(STORE).get(key);
          request.onsuccess = () => request.result?.owner === owner
            ? resolve(request.result.value as T) : reject(new Error('Timeline entry unavailable'));
          request.onerror = () => reject(request.error);
          tx.onabort = () => reject(tx.error);
        });
      } finally { release(); }
    },
    pin,
    async prune(retained, throughSerial) {
      if (!database || closed || !throughSerial) return;
      const db = await database;
      if (closed) return;
      await new Promise<void>((resolve, reject) => {
        const tx = db.transaction(STORE, 'readwrite');
        const request = tx.objectStore(STORE).index('owner').openKeyCursor(IDBKeyRange.only(owner));
        request.onsuccess = () => {
          const cursor = request.result;
          if (!cursor) return;
          const key = String(cursor.primaryKey);
          const serial = Number(key.slice(key.lastIndexOf(':') + 1));
          // Later serials may have committed to disk while their metadata is
          // still waiting for the next Preact render. Never delete that batch.
          if (serial <= throughSerial && !retained.has(key) && !pins.has(key)) tx.objectStore(STORE).delete(key);
          cursor.continue();
        };
        tx.oncomplete = () => resolve();
        tx.onabort = tx.onerror = () => reject(tx.error ?? new Error('Timeline cleanup failed'));
      });
    },
    close,
  };
}
