#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
AtomCode 文档站静态构建脚本（零依赖，仅用 Python 标准库）

用法（在 site/ 目录或仓库根目录执行均可）：
  python3 site/build_docs.py                 # 就地重建：读取 docs/{zh,en}/*.html 的正文，套用当前外壳
  python3 site/build_docs.py --raw <目录>     # 从其他目录读取原始页面（<目录>/zh/*.html、<目录>/en/*.html）
  npm run build:search                       # 同上（package.json 已指向本脚本）

修改文档：直接编辑 docs/<lang>/<slug>.html 里 <article class="prose" id="prose"> 内的正文，
然后重跑本脚本；侧栏、目录、翻页、搜索索引都会自动更新。新增页面需在下方 GROUPS 中登记。

产物（写入 docs/）：
  docs/index.html                按语言跳转
  docs/{zh,en}/<slug>.html       中英文文档（changelog 从仓库根目录的 Markdown 生成）
  docs/search-index.{zh,en}.js   全文搜索索引（file:// 下也能用，无需 fetch）
  docs/docs.css / docs/docs.js   手写维护，构建时不覆盖

正文内容原样保留，只替换外壳（头部 / 侧栏 / 目录 / 翻页 / 搜索）。
兼容两种输入：旧版外壳（<main class="dmain prose-docs">）与本脚本生成的新外壳，可反复执行。
"""
import html
import json
import os
import re
import sys

ROOT = os.path.dirname(os.path.abspath(__file__))          # site/
OUT = os.path.join(ROOT, 'docs')


def read_version():
    """版本号跟随 site/package.json，发版时无需改脚本"""
    try:
        with open(os.path.join(ROOT, 'package.json'), encoding='utf-8') as f:
            return 'v' + json.load(f)['version']
    except Exception:
        return 'v5.2.8'


VERSION = read_version()

# ==================== 目录结构（与线上文档侧栏一致） ====================
# (slug, 中文标题, 英文标题, 元素符号)
GROUPS = [
    (('概览', 'Overview'), [('index', '文档首页', 'Docs home', 'Ac')]),
    (('开始', 'Start'), [
        ('getting-started', '快速开始', 'Quickstart', 'Qs'),
        ('login', '登录方式', 'Login methods', 'Lg'),
        ('configuration', '配置文件', 'Configuration', 'Cf'),
    ]),
    (('使用', 'Usage'), [
        ('basic-usage', '基本使用', 'Basic usage', 'Bu'),
        ('slash-commands', '斜杠命令', 'Slash commands', 'Sc'),
        ('keybindings', '快捷键', 'Keybindings', 'Kb'),
        ('sessions', '会话与撤销', 'Sessions & undo', 'Ud'),
        ('interactive-questions', '交互提问', 'Interactive questions', 'Iq'),
    ]),
    (('进阶', 'Advanced'), [
        ('tools', '内置工具', 'Built-in tools', 'Tl'),
        ('subagents', '子代理', 'Subagents', 'Sa'),
        ('approvals', '权限审批', 'Approvals', 'Ap'),
        ('skills', 'Skills 扩展', 'Skills', 'Sk'),
        ('mcp', 'MCP 集成', 'MCP', 'Mc'),
        ('plugins', 'Plugin 系统', 'Plugins', 'Pl'),
        ('memory', '永久记忆', 'Memory', 'Me'),
        ('project-instructions', '项目指令文件', 'Project instructions', 'Pi'),
        ('webui', 'WebUI 界面', 'WebUI', 'Wb'),
        ('webui-remote-access', '远程访问指南', 'Remote access', 'Ra'),
    ]),
    (('问题', 'Help'), [('faq', '常见问题', 'FAQ', 'Fq')]),
    (('发布', 'Releases'), [('changelog', '更新日志', 'Changelog', 'Cl')]),
]
# 不在侧栏中、但由其他页面链接到的页面：(slug, 中文, 英文, 符号, 所属分组)
HIDDEN = [('headless-daemon', 'Headless 与 Daemon', 'Headless & Daemon', 'Hd', ('进阶', 'Advanced'))]
PAGES = [(g, p) for g, ps in GROUPS for p in ps]
SYM = {p[0]: p[3] for _, p in PAGES}
SYM.update({h[0]: h[3] for h in HIDDEN})

UI = {
    'zh': dict(docs='文档', search='搜索文档…', repo='仓库', home='官网', toc='本页目录', top='回到顶部',
               prev='上一篇', next='下一篇', issue='报告问题', edit='在 AtomGit 上反馈', menu='目录',
               theme='切换主题', lang='EN', langName='English', esc='ESC', noRes='没有匹配的结果',
               hint='↑↓ 选择 · ↵ 打开 · esc 关闭', copy='复制', copied='已复制', read='分钟阅读'),
    'en': dict(docs='Docs', search='Search docs…', repo='Repository', home='Home', toc='On this page', top='Back to top',
               prev='Previous', next='Next', issue='Report an issue', edit='Feedback on AtomGit', menu='Menu',
               theme='Toggle theme', lang='中', langName='中文', esc='ESC', noRes='No results',
               hint='↑↓ select · ↵ open · esc close', copy='Copy', copied='Copied', read='min read'),
}


# ==================== 读取原始页面 ====================
def load_raw(lang, slug, raw_dir):
    path = os.path.join(raw_dir or OUT, lang, slug + '.html')
    if not os.path.exists(path):
        return None
    with open(path, encoding='utf-8') as f:
        return f.read()


def extract(src):
    """取出正文并做最小清洗；新外壳取 <article id="prose">，旧外壳取 <main class="dmain prose-docs">"""
    m = (re.search(r'<article class="prose" id="prose">\n(.*?)\n    </article>', src, re.S)
         or re.search(r'<main class="dmain prose-docs">(.*?)<footer class="dftr">', src, re.S))
    if not m:
        raise ValueError('找不到正文区')
    body = m.group(1).strip()
    body = re.sub(r'\s+data-i18n(?:-[a-z]+)?="[^"]*"', '', body)       # 去掉旧站的 i18n 钩子
    body = re.sub(r'<script.*?</script>', '', body, flags=re.S)        # 正文里不允许脚本
    body = re.sub(r'\son\w+="[^"]*"', '', body)                        # 去掉内联事件
    if '<div class="tw">' not in body:                                 # 表格包一层横向滚动容器（只包一次）
        body = body.replace('<table', '<div class="tw"><table').replace('</table>', '</table></div>')
    # 首页导览卡片：加上元素符号
    def card(mm):
        slug = mm.group(1)
        return f'<a class="doc-card" data-sym="{SYM.get(slug, "")}" href="./{slug}.html">'
    body = re.sub(r'<a class="doc-card" href="\./([\w-]+)\.html">', card, body)
    t = re.search(r'<title>(.*?)</title>', src, re.S)
    title = html.unescape(t.group(1).split(' · ')[0].strip()) if t else ''
    d = re.search(r'<meta name="description" content="([^"]*)"', src)
    return title, body, (d.group(1) if d else '')


def plain(s):
    s = re.sub(r'<[^>]+>', ' ', s)
    return re.sub(r'\s+', ' ', html.unescape(s)).strip()


def search_sections(body):
    headings = list(re.finditer(r'<h[23] id="([^"]+)"[^>]*>(.*?)</h[23]>', body, re.S))
    boundaries = [heading.start() for heading in headings] + [len(body)]
    sections = [
        (heading[1], plain(heading[2]), plain(body[heading.end():end]))
        for heading, end in zip(headings, boundaries[1:])
    ]
    return plain(body[:boundaries[0]]), sections


# ==================== 更新日志（Markdown 为唯一来源） ====================
RELEASE_HEADING = re.compile(r'^## (v\d+\.\d+\.\d+)\b([^\n]*)$', re.M)
ISSUES_HEADING = re.compile(r'^### Issues[ \t]*$', re.M)
RELEASE_DATE = re.compile(r' \((\d{4}-\d{2}-\d{2})\)$')
# Changelog uses inline code, bold leads, and issue links with bracketed labels.
RELEASE_INLINE = re.compile(
    r'`(?P<code>[^`]+)`|\*\*(?P<bold>.+?)\*\*|'
    r'\[(?P<label>(?:[^\[\]]|\[[^\[\]]*\])+)\]\((?P<url>https?://[^\s()]+)\)'
)


def release_inline(text):
    out, start = [], 0
    for match in RELEASE_INLINE.finditer(text):
        out.append(html.escape(text[start:match.start()]))
        if match['code'] is not None:
            out.append(f'<code>{html.escape(match["code"])}</code>')
        elif match['bold'] is not None:
            out.append(f'<strong>{release_inline(match["bold"])}</strong>')
        else:
            out.append(f'<a href="{html.escape(match["url"])}">{release_inline(match["label"])}</a>')
        start = match.end()
    return ''.join(out) + html.escape(text[start:])


def release_blocks(markdown):
    out, paragraph, items = [], [], []

    def flush():
        if paragraph:
            out.append(f'<p>{release_inline(" ".join(paragraph))}</p>')
            paragraph.clear()
        if items:
            out.append('<ul>' + ''.join(f'<li>{release_inline(item)}</li>' for item in items) + '</ul>')
            items.clear()

    for line in markdown.splitlines():
        if not line.strip():
            flush()
        elif line.startswith('- '):
            if paragraph:
                flush()
            items.append(line[2:])
        elif line.startswith('### '):
            flush()
            out.append(f'<h3>{release_inline(line[4:])}</h3>')
        else:
            if items:
                flush()
            paragraph.append(line)
    flush()
    return '\n'.join(out)


def read_releases(filename):
    with open(os.path.join(ROOT, '..', filename), encoding='utf-8') as f:
        text = re.sub(r'<!--.*?-->', '', f.read(), flags=re.S)
    headings = list(RELEASE_HEADING.finditer(text))
    if not headings:
        raise ValueError(f'No version sections in {filename}')
    boundaries = [heading.start() for heading in headings[1:]] + [len(text)]
    releases = {}
    for heading, end in zip(headings, boundaries):
        version, suffix = heading.groups()
        if version in releases:
            raise ValueError(f'Duplicate changelog version {version} in {filename}')
        parts = ISSUES_HEADING.split(text[heading.end():end], maxsplit=1)
        releases[version] = (suffix, parts[0], parts[1] if len(parts) == 2 else '')
    return releases


def changelog_page(lang, zh_releases, en_releases):
    zh = lang == 'zh'
    title = '更新日志' if zh else 'Changelog'
    desc = '按版本查看 AtomCode 的新功能、修复和关联议题。' if zh else 'Browse AtomCode features, fixes and related issues by release.'
    versions = sorted(zh_releases.keys() | en_releases.keys(),
                      key=lambda version: tuple(map(int, version[1:].split('.'))), reverse=True)
    jump = '按版本浏览' if zh else 'Browse by version'
    issues_label = '关联议题' if zh else 'Related issues'
    source_label = '查看发行版和安装包' if zh else 'View releases and downloads'
    links = ''.join(f'<li><a href="#{v}">{v}</a></li>' for v in versions)
    body = [
        f'<h1>{title}</h1>',
        f'<p class="lede">{desc}</p>',
        f'<p><a href="https://atomgit.com/atomgit_atomcode/atomcode/releases">{source_label} ↗</a></p>',
        f'<details class="release-jump"><summary>{jump}</summary><ul>{links}</ul></details>',
    ]
    localized = zh_releases if zh else en_releases
    for version in versions:
        entry = localized.get(version) or zh_releases.get(version) or en_releases[version]
        suffix, notes, issues = entry
        issues = issues.strip() or zh_releases.get(version, ('', '', ''))[2].strip()
        date = RELEASE_DATE.search(suffix)
        label = html.escape(version + (suffix[:date.start()] if date else suffix))
        if date:
            label += f' <time datetime="{date[1]}">{date[1]}</time>'
        body.append(f'<h2 id="{version}">{label}</h2>')
        if version not in localized:
            source_lang = 'en' if zh else 'zh-CN'
            body.append(f'<div lang="{source_lang}" class="release-fallback">{release_blocks(notes)}</div>')
        else:
            body.append(release_blocks(notes))
        if issues:
            body.append(f'<details class="release-issues"><summary>{issues_label}</summary>'
                        f'{release_blocks(issues)}</details>')
    return title, '\n'.join(body), desc


# ==================== 页面模板 ====================
def side_html(lang, cur):
    out, n = [], 0
    for (gz, ge), items in GROUPS:
        out.append(f'<div class="sg"><div class="sg-t">{gz if lang == "zh" else ge}</div>')
        for slug, zh, en, sym in items:
            n += 1
            on = ' on' if slug == cur else ''
            aria = ' aria-current="page"' if on else ''
            out.append(f'<a class="sl{on}" href="./{slug}.html"{aria}><i>{sym}</i><span>{zh if lang == "zh" else en}</span></a>')
        out.append('</div>')
    return '\n'.join(out)


def page_html(lang, slug, title, desc, body, minutes):
    u = UI[lang]
    other = 'en' if lang == 'zh' else 'zh'
    slugs = [p[0] for _, p in PAGES]
    if slug in slugs:
        idx = slugs.index(slug)
        grp = PAGES[idx][0][0 if lang == 'zh' else 1]
        prev = PAGES[idx - 1][1] if idx > 0 else None
        nxt = PAGES[idx + 1][1] if idx < len(PAGES) - 1 else None
    else:                                                               # 隐藏页：不进侧栏和翻页序列
        h = next(x for x in HIDDEN if x[0] == slug)
        grp, prev, nxt = h[4][0 if lang == 'zh' else 1], None, None
    def pg(p, cls, label):
        if not p:
            return '<span></span>'
        return (f'<a class="pg {cls}" href="./{p[0]}.html"><small>{label}</small>'
                f'<b><i>{p[3]}</i>{p[1] if lang == "zh" else p[2]}</b></a>')
    esc = html.escape
    return f'''<!doctype html>
<html lang="{'zh-CN' if lang == 'zh' else 'en'}">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1,viewport-fit=cover">
<title>{esc(title)} · AtomCode {u['docs']}</title>
<meta name="description" content="{desc}">
<meta name="theme-color" content="#050605">
<link rel="icon" type="image/svg+xml" href="../../assets/atomgit-mark.svg">
<link rel="preconnect" href="https://fonts.googleapis.com">
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link href="https://fonts.googleapis.com/css2?family=Inter:wght@400;500;600;700&family=JetBrains+Mono:wght@400;500;700&display=swap" rel="stylesheet">
<link rel="stylesheet" href="../docs.css">
<script>try{{if(localStorage.getItem('atomcode_theme')==='light')document.documentElement.classList.add('light')}}catch(e){{}}</script>
</head>
<body data-page="{slug}" data-lang="{lang}">
<div class="prog" id="prog"></div>

<!-- ==================== 头部 ==================== -->
<header class="dh">
  <button class="ib sb-btn" id="sbBtn" aria-label="{u['menu']}" aria-expanded="false">☰</button>
  <a class="dh-brand" href="../../index.html"><img class="dh-mark" src="../../assets/atomgit-mark.svg" width="24" height="24" alt=""><span>AtomCode</span><em>{u['docs'].upper() if lang == 'en' else 'DOCS'}</em><small>{VERSION}</small></a>
  <button class="dh-search" id="sBtn"><b>/</b><span>{u['search']}</span><kbd>⌘K</kbd></button>
  <nav class="dh-r">
    <a class="ib txt" href="../../index.html">{u['home']}</a>
    <a class="ib" href="../{other}/{slug}.html" id="langBtn" hreflang="{other}" aria-label="{u['langName']}">{u['lang']}</a>
    <button class="ib" id="themeBtn" aria-label="{u['theme']}"><span class="i-moon">◐</span></button>
    <a class="ib txt" href="https://atomgit.com/atomgit_atomcode/atomcode">{u['repo']} ↗</a>
  </nav>
</header>

<div class="dl">
  <!-- ==================== 侧栏 ==================== -->
  <aside class="ds" id="ds" aria-label="{u['menu']}">
{side_html(lang, slug)}
  </aside>
  <div class="ds-bg" id="dsBg"></div>

  <!-- ==================== 正文 ==================== -->
  <main class="dm">
    <div class="crumb"><a href="./index.html">{u['docs']}</a><span>/</span><span>{grp}</span><span>/</span><b>{esc(title)}</b><em>{minutes} {u['read']}</em></div>
    <article class="prose" id="prose">
{body}
    </article>
    <nav class="pager">{pg(prev, 'prev', '← ' + u['prev'])}{pg(nxt, 'next', u['next'] + ' →')}</nav>
    <footer class="df"><span>© 2026 AtomCode · MIT</span><a href="https://atomgit.com/atomgit_atomcode/atomcode/issues">{u['issue']} ↗</a></footer>
  </main>

  <!-- ==================== 本页目录 ==================== -->
  <aside class="toc" id="toc" aria-label="{u['toc']}"><p>{u['toc']}</p><nav id="tocList"></nav><button id="toTop">↑ {u['top']}</button></aside>
</div>

<!-- ==================== 搜索 ==================== -->
<div class="sm" id="sm" role="dialog" aria-modal="true" aria-label="{u['search']}">
  <div class="sm-box">
    <div class="sm-in"><b>/</b><input id="sIn" type="search" autocomplete="off" spellcheck="false" placeholder="{u['search']}"><kbd>{u['esc']}</kbd></div>
    <ul class="sm-list" id="sList" role="listbox"></ul>
    <div class="sm-ft">{u['hint']}</div>
  </div>
</div>
<div class="toast" id="toast" role="status"></div>
<script>window.DOCS_UI={json.dumps({k: u[k] for k in ('copy', 'copied', 'noRes')}, ensure_ascii=False)};</script>
<script src="../search-index.{lang}.js"></script>
<script src="../docs.js"></script>
</body>
</html>
'''


def build(raw_dir=None):
    zh_releases = read_releases('CHANGELOG.md')
    en_releases = read_releases('CHANGELOG.en.md')
    for lang in ('zh', 'en'):
        os.makedirs(os.path.join(OUT, lang), exist_ok=True)
        index = []
        for slug, zh, en, sym in [p for _, p in PAGES] + [h[:4] for h in HIDDEN]:
            if slug == 'changelog':
                title, body, desc = changelog_page(lang, zh_releases, en_releases)
            else:
                src = load_raw(lang, slug, raw_dir)
                if src is None:
                    print(f'  跳过缺失页面 {lang}/{slug}.html')
                    continue
                title, body, desc = extract(src)
            title = title or (zh if lang == 'zh' else en)
            text = plain(body)
            # 阅读时长：中文按 400 字/分钟，英文按 220 词/分钟
            minutes = max(1, round((len(text) / 400) if lang == 'zh' else (len(text.split()) / 220)))
            with open(os.path.join(OUT, lang, slug + '.html'), 'w', encoding='utf-8') as f:
                f.write(page_html(lang, slug, title, desc, body, minutes))
            intro, sections = search_sections(body)
            index.append({'s': slug, 't': title, 'y': sym, 'h': sections, 'x': intro})
        with open(os.path.join(OUT, f'search-index.{lang}.js'), 'w', encoding='utf-8') as f:
            f.write('window.DOCS_INDEX=' + json.dumps(index, ensure_ascii=False, separators=(',', ':')) + ';\n')
        print(f'[{lang}] {len(index)} 页')
    with open(os.path.join(OUT, 'index.html'), 'w', encoding='utf-8') as f:
        f.write('''<!doctype html>
<html lang="zh-CN"><head><meta charset="utf-8"><title>AtomCode 文档 / Documentation</title>
<link rel="icon" type="image/svg+xml" href="../assets/atomgit-mark.svg">
<meta http-equiv="refresh" content="0; url=./zh/index.html">
<script>(function(){var l='zh';try{if(localStorage.getItem('atomcode_lang')==='en')l='en'}catch(e){}
var q=new URLSearchParams(location.search).get('lang');if(q==='en'||q==='zh')l=q;location.replace('./'+l+'/index.html'+location.hash)})();</script>
</head><body style="background:#050605;color:#a3a99f;font:14px monospace;padding:40px">Redirecting… <a style="color:#c6f24e" href="./zh/index.html">中文文档</a> / <a style="color:#c6f24e" href="./en/index.html">English docs</a></body></html>
''')


if __name__ == '__main__':
    raw = sys.argv[sys.argv.index('--raw') + 1] if '--raw' in sys.argv else None
    build(raw)
