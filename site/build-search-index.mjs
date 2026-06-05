#!/usr/bin/env node
// Build site/docs/search-index.json from site/docs/*.html
//
// For each page we extract:
//   { slug, title, lede, group, sections: [{ id, heading, body }, ...] }
//
// "body" is plain-text content under a heading (next heading terminates).
// The shared sidebar grouping is mirrored here so search results can show
// the group name when no query is typed yet.

import { promises as fs } from 'node:fs';
import path from 'node:path';
import url from 'node:url';

const __dirname = path.dirname(url.fileURLToPath(import.meta.url));
const DOCS_DIR  = path.join(__dirname, 'docs');
const LANGS     = ['zh', 'en'];

// Mirror of docs sidebar groups (keep in sync with sidebar markup).
const GROUPS = [
  { name: '概览',   slugs: ['index'] },
  { name: '开始',   slugs: ['getting-started', 'login', 'configuration'] },
  { name: '使用',   slugs: ['basic-usage', 'slash-commands', 'keybindings', 'sessions'] },
  { name: '进阶',   slugs: ['tools', 'approvals', 'skills', 'mcp', 'plugins', 'memory', 'project-instructions', 'webui', 'webui-remote-access'] },
  { name: '运维',   slugs: ['faq'] },
];

function groupOf(slug) {
  for (const g of GROUPS) if (g.slugs.includes(slug)) return g.name;
  return null;
}

// ── tiny HTML utilities (no deps) ────────────────────────────────────────────

function stripTags(html) {
  return html
    .replace(/<script\b[^>]*>[\s\S]*?<\/script>/gi, ' ')
    .replace(/<style\b[^>]*>[\s\S]*?<\/style>/gi, ' ')
    .replace(/<[^>]+>/g, ' ');
}
function decodeEntities(s) {
  return s
    .replace(/&nbsp;/g, ' ')
    .replace(/&amp;/g, '&')
    .replace(/&lt;/g, '<')
    .replace(/&gt;/g, '>')
    .replace(/&quot;/g, '"')
    .replace(/&#39;/g, "'")
    .replace(/&#(\d+);/g, (_, n) => String.fromCharCode(+n));
}
function squashWhitespace(s) {
  return s.replace(/\s+/g, ' ').trim();
}
function toText(html) {
  return squashWhitespace(decodeEntities(stripTags(html)));
}

function attr(tagStr, name) {
  const re = new RegExp(name + '\\s*=\\s*"([^"]*)"', 'i');
  const m = tagStr.match(re);
  return m ? m[1] : '';
}
function slugify(s) {
  return squashWhitespace(s)
    .toLowerCase()
    .replace(/[^\w一-鿿 -]/g, '')
    .replace(/\s+/g, '-')
    .slice(0, 60) || 'section';
}

// Extract the <main>...</main> region; fall back to <body> if no main.
function getMain(html) {
  let m = html.match(/<main\b[^>]*>([\s\S]*?)<\/main>/i);
  if (m) return m[1];
  m = html.match(/<body\b[^>]*>([\s\S]*?)<\/body>/i);
  return m ? m[1] : html;
}

// Parse main content into ordered chunks: heading + plain-text body until
// the next heading. We treat h1 as the page title (separately) but also
// include it as the first section so searches matching the title hit it.
function parseSections(mainHtml) {
  const sections = [];
  const re = /<(h[1-3])\b([^>]*)>([\s\S]*?)<\/\1>/gi;
  const heads = [];
  let m;
  while ((m = re.exec(mainHtml)) !== null) {
    heads.push({
      tag: m[1].toLowerCase(),
      attrs: m[2],
      raw: m[0],
      start: m.index,
      end: m.index + m[0].length,
      headingHtml: m[3],
    });
  }
  for (let i = 0; i < heads.length; i++) {
    const h = heads[i];
    const headingText = toText(h.headingHtml);
    const id = attr(h.raw, 'id') || slugify(headingText);
    const bodyStart = h.end;
    const bodyEnd   = i + 1 < heads.length ? heads[i + 1].start : mainHtml.length;
    const body      = toText(mainHtml.slice(bodyStart, bodyEnd));
    sections.push({ id, heading: headingText, body });
  }
  return sections;
}

function extractTitle(html) {
  const m = html.match(/<title>([\s\S]*?)<\/title>/i);
  if (!m) return '';
  // "快速开始 · AtomCode 文档" → "快速开始"
  return toText(m[1]).split(/[·|—–-]/)[0].trim();
}

// Find the first paragraph after h1 — used as a sidebar/result lede.
function extractLede(mainHtml) {
  // The site convention: <p class="lede">...</p> right under h1
  const m = mainHtml.match(/<p[^>]*class="[^"]*\blede\b[^"]*"[^>]*>([\s\S]*?)<\/p>/i);
  if (m) return toText(m[1]);
  // Otherwise first <p> in main
  const p = mainHtml.match(/<p\b[^>]*>([\s\S]*?)<\/p>/i);
  return p ? toText(p[1]).slice(0, 240) : '';
}

// ── main ─────────────────────────────────────────────────────────────────────

async function buildOne(lang) {
  const dir = path.join(DOCS_DIR, lang);
  let files;
  try { files = (await fs.readdir(dir)).filter(f => f.endsWith('.html')); }
  catch (e) { console.warn(`[search-index] ${lang}/ not found, skipping`); return; }
  const orderedSlugs = GROUPS.flatMap(g => g.slugs);
  files.sort((a, b) => {
    const ai = orderedSlugs.indexOf(a.replace(/\.html$/, ''));
    const bi = orderedSlugs.indexOf(b.replace(/\.html$/, ''));
    return (ai === -1 ? 999 : ai) - (bi === -1 ? 999 : bi);
  });

  const out = [];
  for (const file of files) {
    const slug = file.replace(/\.html$/, '');
    if (!orderedSlugs.includes(slug)) {
      console.warn(`[search-index] ${lang}/ skip ungrouped: ${file}`);
      continue;
    }
    const html = await fs.readFile(path.join(dir, file), 'utf8');
    const main = getMain(html);
    out.push({
      slug,
      title:    extractTitle(html) || slug,
      group:    groupOf(slug),
      lede:     extractLede(main),
      sections: parseSections(main),
    });
  }

  const outFile = path.join(DOCS_DIR, `search-index.${lang}.json`);
  await fs.writeFile(outFile, JSON.stringify(out));
  const bytes = (await fs.stat(outFile)).size;
  console.log(`[search-index] ${lang}: ${out.length} pages, ${(bytes/1024).toFixed(1)} KB → ${path.relative(process.cwd(), outFile)}`);
}

async function build() {
  // Remove legacy flat index if present
  const legacy = path.join(DOCS_DIR, 'search-index.json');
  try { await fs.unlink(legacy); } catch (e) {}
  for (const lang of LANGS) await buildOne(lang);
}

build().catch(err => { console.error(err); process.exit(1); });
