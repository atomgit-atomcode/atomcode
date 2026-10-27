import { DatabaseSync } from 'node:sqlite';

// A page and its next cursor commit together: retries cannot lose or double-count rows.
export class Store {
  constructor(path) {
    this.db = new DatabaseSync(path);
    this.db.exec(`PRAGMA journal_mode=WAL;
      PRAGMA busy_timeout=5000;
      CREATE TABLE IF NOT EXISTS runs (
        id INTEGER PRIMARY KEY, kind TEXT NOT NULL, endpoint TEXT NOT NULL,
        cursor TEXT, status TEXT NOT NULL, pages INTEGER NOT NULL DEFAULT 0,
        started INTEGER NOT NULL, updated INTEGER NOT NULL, error TEXT,
        rejected INTEGER NOT NULL DEFAULT 0);
      CREATE TABLE IF NOT EXISTS records (
        run_id INTEGER NOT NULL, id TEXT NOT NULL, body TEXT NOT NULL,
        PRIMARY KEY(run_id,id));
      CREATE INDEX IF NOT EXISTS runs_kind ON runs(kind,id);
      CREATE TABLE IF NOT EXISTS reports(id INTEGER PRIMARY KEY, body TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS membership (generation TEXT NOT NULL, family TEXT NOT NULL, model_id TEXT NOT NULL, PRIMARY KEY(generation,family,model_id));
      CREATE TABLE IF NOT EXISTS membership_ready (generation TEXT PRIMARY KEY);
      CREATE TABLE IF NOT EXISTS organization_summary (
        run_id INTEGER NOT NULL, generation TEXT NOT NULL, family TEXT NOT NULL,
        name TEXT NOT NULL, count INTEGER NOT NULL, downloads INTEGER NOT NULL, likes INTEGER NOT NULL,
        PRIMARY KEY(run_id,generation,family,name));
      CREATE INDEX IF NOT EXISTS organization_summary_rank ON organization_summary(run_id,generation,family,downloads DESC,name ASC);
      CREATE TABLE IF NOT EXISTS organization_summary_ready (
        run_id INTEGER NOT NULL, generation TEXT NOT NULL, family TEXT NOT NULL,
        PRIMARY KEY(run_id,generation,family));`);
    const columns = this.db.prepare('PRAGMA table_info(runs)').all().map(column => column.name);
    if (!columns.includes('rejected')) this.db.exec('ALTER TABLE runs ADD COLUMN rejected INTEGER NOT NULL DEFAULT 0');
    if (!columns.includes('record_count')) {
      this.db.exec(`BEGIN IMMEDIATE;
        ALTER TABLE runs ADD COLUMN record_count INTEGER NOT NULL DEFAULT 0;
        UPDATE runs SET record_count=(SELECT count(*) FROM records WHERE run_id=runs.id);
        COMMIT;`);
    }
    this.db.exec(`CREATE TRIGGER IF NOT EXISTS records_count_insert AFTER INSERT ON records
      BEGIN UPDATE runs SET record_count=record_count+1 WHERE id=NEW.run_id; END;
      CREATE TRIGGER IF NOT EXISTS records_count_delete AFTER DELETE ON records
      BEGIN UPDATE runs SET record_count=record_count-1 WHERE id=OLD.run_id; END;`);
  }
  begin(kind, endpoint) {
    const previous = this.db.prepare("SELECT * FROM runs WHERE kind=? AND endpoint=? AND status IN ('running','failed','paused') ORDER BY id DESC LIMIT 1").get(kind, endpoint);
    if (previous) {
      this.db.prepare("UPDATE runs SET status='running',error=NULL WHERE id=?").run(previous.id);
      return previous;
    }
    const now = Date.now();
    const result = this.db.prepare("INSERT INTO runs(kind,endpoint,cursor,status,started,updated) VALUES(?,?,?,'running',?,?)").run(kind, endpoint, endpoint, now, now);
    return this.db.prepare('SELECT * FROM runs WHERE id=?').get(result.lastInsertRowid);
  }
  page(run, rows, cursor, status, rejected = 0) {
    this.db.exec('BEGIN IMMEDIATE');
    try {
      const insert = this.db.prepare('INSERT INTO records VALUES(?,?,?) ON CONFLICT(run_id,id) DO UPDATE SET body=excluded.body');
      for (const row of rows) {
        if (typeof row.id !== 'string' || !row.id) throw new Error('Invalid repository identifier');
        insert.run(run, row.id, JSON.stringify(row));
      }
      this.db.prepare('UPDATE runs SET cursor=?,status=?,pages=pages+1,updated=?,error=NULL,rejected=rejected+? WHERE id=?').run(cursor, status, Date.now(), rejected, run);
      this.db.exec('COMMIT');
    } catch (error) { this.db.exec('ROLLBACK'); throw error; }
  }
  fail(run, error) { this.db.prepare("UPDATE runs SET status='failed',error=?,updated=? WHERE id=?").run(error.message, Date.now(), run); }
  batch(run, after = '', limit = 1000) { return this.db.prepare('SELECT id,body FROM records WHERE run_id=? AND id>? ORDER BY id LIMIT ?').all(run, after, limit).map(row => JSON.parse(row.body)); }
  *iterate(run) { for (const row of this.db.prepare('SELECT body FROM records WHERE run_id=?').iterate(run)) yield JSON.parse(row.body); }
  rows(run) { return this.db.prepare('SELECT body FROM records WHERE run_id=?').all(run).map(r => JSON.parse(r.body)); }
  count(run) { return this.db.prepare('SELECT record_count AS n FROM runs WHERE id=?').get(run).n; }
  jobs() { return this.db.prepare('SELECT *, record_count AS count FROM runs ORDER BY id DESC LIMIT 30').all(); }
  saveReport(body, timing) {
    const start = performance.now();
    const result = this.db.prepare('INSERT INTO reports(body) VALUES(?)').run(JSON.stringify(body));
    if (timing) {
      const finished = Date.now();
      body.timing = {
        collection_ms: timing.collection_ms,
        aggregation_ms: timing.aggregation_ms,
        report_write_ms: performance.now() - start,
        execution_ms: finished - timing.execution_started,
        total_elapsed_ms: finished - timing.run_started,
        committed_at: new Date(finished).toISOString(),
      };
      this.db.prepare('UPDATE reports SET body=? WHERE id=?').run(JSON.stringify(body), result.lastInsertRowid);
    }
  }
  latestReport() { const row=this.db.prepare('SELECT body FROM reports ORDER BY id DESC LIMIT 1').get(); return row ? JSON.parse(row.body) : null; }
  reports() { return this.db.prepare('SELECT body FROM reports ORDER BY id DESC LIMIT 30').all().reverse().map(r => JSON.parse(r.body)); }
  clearIndex(generation) {
    this.db.exec('BEGIN IMMEDIATE');
    try { this.db.prepare('DELETE FROM membership WHERE generation=?').run(generation); this.db.prepare('DELETE FROM membership_ready WHERE generation=?').run(generation); this.db.exec('COMMIT'); }
    catch(error) { this.db.exec('ROLLBACK'); throw error; }
  }
  indexMembers(generation, entries) {
    this.db.exec('BEGIN IMMEDIATE');
    try {
      const insert=this.db.prepare('INSERT OR IGNORE INTO membership VALUES(?,?,?)');
      for(const entry of entries) insert.run(generation,entry.family,entry.id);
      this.db.exec('COMMIT');
    } catch(error) {this.db.exec('ROLLBACK');throw error;}
  }
  finishIndex(generation) {this.db.prepare('INSERT OR IGNORE INTO membership_ready VALUES(?)').run(generation);}
  hasIndex(generation) {return Boolean(this.db.prepare('SELECT 1 FROM membership_ready WHERE generation=?').get(generation));}
  memberPage(run,generation,family,offset,limit=101,organization=null) {
    const owner = "COALESCE(NULLIF(json_extract(r.body,'$.author'),''), CASE WHEN instr(r.id,'/')>0 THEN substr(r.id,1,instr(r.id,'/')-1) ELSE r.id END)";
    const orgFilter = organization ? ` AND lower(${owner})=lower(?)` : '';
    const args = organization ? [run,generation,family,organization,limit,offset] : [run,generation,family,limit,offset];
    return this.db.prepare(`SELECT r.body FROM membership m JOIN records r ON r.run_id=? AND r.id=m.model_id
      WHERE m.generation=? AND m.family=?${orgFilter}
      ORDER BY COALESCE(CAST(json_extract(r.body,'$.downloads') AS REAL),0) DESC, m.model_id ASC LIMIT ? OFFSET ?`).all(...args).map(r=>JSON.parse(r.body));
  }
  memberOrganizations(run,generation,family,offset=0,limit=21) {
    const ready = this.db.prepare('SELECT 1 FROM organization_summary_ready WHERE run_id=? AND generation=? AND family=?').get(run,generation,family);
    if (!ready) {
      this.db.exec('BEGIN IMMEDIATE');
      try {
        this.db.prepare(`INSERT OR REPLACE INTO organization_summary
          SELECT ?,?,?, COALESCE(NULLIF(json_extract(r.body,'$.author'),''), CASE WHEN instr(r.id,'/')>0 THEN substr(r.id,1,instr(r.id,'/')-1) ELSE r.id END) AS name,
          count(*), sum(COALESCE(json_extract(r.body,'$.downloads'),0)), sum(COALESCE(json_extract(r.body,'$.likes'),0))
          FROM membership m JOIN records r ON r.run_id=? AND r.id=m.model_id
          WHERE m.generation=? AND m.family=? GROUP BY name`).run(run,generation,family,run,generation,family);
        this.db.prepare('INSERT OR IGNORE INTO organization_summary_ready VALUES(?,?,?)').run(run,generation,family);
        this.db.exec('COMMIT');
      } catch(error) { this.db.exec('ROLLBACK'); throw error; }
    }
    return this.db.prepare(`SELECT name,count,downloads,likes FROM organization_summary
      WHERE run_id=? AND generation=? AND family=? ORDER BY downloads DESC,name ASC LIMIT ? OFFSET ?`).all(run,generation,family,limit,offset);
  }
  close() { this.db.close(); }
}
