import { DatabaseSync } from 'node:sqlite';
import { resolve, dirname } from 'node:path';
import { mkdir, writeFile, rename } from 'node:fs/promises';

const path = process.env.HF_INSIGHTS_DB || resolve('data/models-live.sqlite');
const waitSeconds = Number(process.env.HF_INSIGHTS_VERIFY_WAIT_SECONDS || 0);
if (!Number.isFinite(waitSeconds) || waitSeconds < 0) throw new Error('Invalid verification timeout');
const deadline = Date.now() + waitSeconds * 1000;
const db = new DatabaseSync(path, {readOnly:true});
try {
  const requested = process.env.HF_INSIGHTS_VERIFY_RUN_ID;
  const runId = requested === undefined
    ? db.prepare("SELECT id FROM runs WHERE kind='models' ORDER BY id DESC LIMIT 1").get()?.id
    : Number(requested);
  if (!Number.isSafeInteger(runId) || runId < 1) throw new Error('Invalid or missing collection run');
  const readRun = () => db.prepare("SELECT * FROM runs WHERE id=? AND kind='models'").get(runId);
  const readReport = () => {
    const row = db.prepare("SELECT body FROM reports WHERE json_extract(body,'$.jobs.models.run_id')=? ORDER BY id DESC LIMIT 1").get(runId);
    return row ? JSON.parse(row.body) : null;
  };
  if (!readRun()) throw new Error('Collection run not found');
  while (Date.now() < deadline) {
    const run = readRun();
    if (run.status === 'failed' || run.status === 'paused') break;
    if (run.status === 'complete' && run.cursor === null && readReport()) break;
    await new Promise(resolve => setTimeout(resolve, Math.min(30000, Math.max(0, deadline - Date.now()))));
  }
  db.exec('BEGIN');
  const run = readRun();
  const count = db.prepare('SELECT count(*) AS n FROM records WHERE run_id=?').get(runId).n;
  const report = readReport();
  const checks = {
    catalog_finished: run.status === 'complete' && run.cursor === null,
    report_finished: report?.summary?.complete === true,
    report_matches_run: report?.jobs?.models?.run_id === run.id,
    counts_match: report?.summary?.model_count === count,
    family_counts_match: report?.families?.reduce((n,f)=>n+f.count,0) === count,
    no_rejected_records: run.rejected === 0,
  };
  db.exec('COMMIT');
  const passed = Object.values(checks).every(Boolean);
  const result = {passed, checks, checked_at:new Date().toISOString(), run_id:run.id, status:run.status, models:count, pages:run.pages, elapsed_ms:run.updated-run.started, timing:report?.timing || null, error:run.error};
  const text = JSON.stringify(result,null,2);
  const output = process.env.HF_INSIGHTS_VERIFY_OUTPUT;
  if (output) {
    const destination = resolve(output);
    await mkdir(dirname(destination),{recursive:true});
    const temporary = `${destination}.${process.pid}.tmp`;
    await writeFile(temporary, text + '\n', {flag:'wx'});
    await rename(temporary, destination);
  }
  console.log(text);
  if (!passed) process.exitCode = 2;
} finally { db.close(); }
