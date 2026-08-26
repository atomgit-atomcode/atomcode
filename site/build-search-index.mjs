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
  { name: '使用',   slugs: ['basic-usage', 'slash-commands', 'keybindings', 'sessions', 'interactive-questions'] },
  { name: '进阶',   slugs: ['tools', 'subagents', 'approvals', 'skills', 'mcp', 'plugins', 'memory', 'project-instructions', 'webui', 'webui-remote-access'] },
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

// Collect ordered h1–h3 headings from the main content, assigning each a stable
// anchor id: an existing `id=""` wins; otherwise `slugify(headingText)`,
// de-duplicated within the page (`-2`, `-3`, …) so the anchors are valid HTML.
// The SAME ids feed both the search index (`sectionsOf`) and the id injected
// back into the HTML (`injectHeadingIds`), so a search hit's `#id` always
// resolves to a real element on the page.
function collectHeadings(mainHtml) {
  const re = /<(h[1-3])\b([^>]*)>([\s\S]*?)<\/\1>/gi;
  const heads = [];
  const used = new Set();
  let m;
  while ((m = re.exec(mainHtml)) !== null) {
    const raw = m[0];
    const headingText = toText(m[3]);
    const existing = attr(raw, 'id');
    let id = existing || slugify(headingText);
    let base = id, n = 2;
    while (used.has(id)) id = `${base}-${n++}`;
    used.add(id);
    heads.push({
      tag: m[1].toLowerCase(),
      attrs: m[2],
      headingHtml: m[3],
      start: m.index,
      end: m.index + raw.length,
      headingText,
      id,
      hadId: !!existing,
    });
  }
  return heads;
}

// Search-index sections: heading + plain-text body until the next heading. h1
// (the page title) is included so title-matching searches hit its section.
function sectionsOf(heads, mainHtml) {
  return heads.map((h, i) => ({
    id: h.id,
    heading: h.headingText,
    body: toText(mainHtml.slice(h.end, i + 1 < heads.length ? heads[i + 1].start : mainHtml.length)),
  }));
}

// Rewrite the main content so every heading that lacked an id carries the id
// assigned above — this is what makes `#<id>` search links actually scroll.
// Headings that already had an id are left byte-for-byte unchanged.
function injectHeadingIds(mainHtml, heads) {
  let out = '';
  let last = 0;
  for (const h of heads) {
    out += mainHtml.slice(last, h.start);
    out += h.hadId
      ? mainHtml.slice(h.start, h.end)
      : `<${h.tag}${h.attrs} id="${h.id}">${h.headingHtml}</${h.tag}>`;
    last = h.end;
  }
  out += mainHtml.slice(last);
  return out;
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
  let injectedFiles = 0;
  for (const file of files) {
    const slug = file.replace(/\.html$/, '');
    if (!orderedSlugs.includes(slug)) {
      console.warn(`[search-index] ${lang}/ skip ungrouped: ${file}`);
      continue;
    }
    const filePath = path.join(dir, file);
    const html = await fs.readFile(filePath, 'utf8');
    const main = getMain(html);
    const heads = collectHeadings(main);
    // Write missing anchor ids back into the HTML so search `#id` links resolve.
    // Function replacement avoids `$` in the new HTML being treated as a
    // back-reference; `main` is a unique region so only that occurrence changes.
    const newMain = injectHeadingIds(main, heads);
    if (newMain !== main) {
      await fs.writeFile(filePath, html.replace(main, () => newMain));
      injectedFiles++;
    }
    out.push({
      slug,
      title:    extractTitle(html) || slug,
      group:    groupOf(slug),
      lede:     extractLede(main),
      sections: sectionsOf(heads, main),
    });
  }

  const outFile = path.join(DOCS_DIR, `search-index.${lang}.json`);
  await fs.writeFile(outFile, JSON.stringify(out));
  const bytes = (await fs.stat(outFile)).size;
  console.log(`[search-index] ${lang}: ${out.length} pages, ${(bytes/1024).toFixed(1)} KB, injected ids into ${injectedFiles} page(s) → ${path.relative(process.cwd(), outFile)}`);
}

async function build() {
  // Remove legacy flat index if present
  const legacy = path.join(DOCS_DIR, 'search-index.json');
  try { await fs.unlink(legacy); } catch (e) {}
  for (const lang of LANGS) await buildOne(lang);
}

build().catch(err => { console.error(err); process.exit(1); });
