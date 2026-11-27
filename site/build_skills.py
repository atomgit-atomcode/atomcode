#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
/* ==================== SkillHub 数据构建 ==================== */
把官方插件仓库的 marketplace.json 编译成 skillhub/skills-data.js（window.SKILLS_DATA），
页面双击即开、file:// 可用，不依赖任何后端。

数据来源（按优先级）：
  1. 官方目录：atomcode-plugins-official/.atomcode-plugin/marketplace.json（唯一权威来源）
     - 传入本地路径：python3 build_skills.py /path/to/atomcode-plugins-official
     - 不传则 git clone --depth 1 到临时目录
  2. 可选增强：旧 Plugin Hub API 的 star 数与作者头像（API 下线后自动跳过，保留上次快照里的值）

仅用 Python 标准库。
"""
import json, os, re, subprocess, sys, tempfile, urllib.request

ROOT = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(ROOT, 'skillhub', 'skills-data.js')
REPO = 'https://gitcode.com/atomgit_atomcode/atomcode-plugins-official.git'
REPO_WEB = 'https://atomgit.com/atomgit_atomcode/atomcode-plugins-official'
API = 'https://atomcode-plugin.atomgit.com/api'

# ==================== 分类（旧 API 的配置，固化在这里，API 下线后不受影响）====================
CATS = [
    ('development', '开发工具与环境', 'Development'),
    ('productivity', '生产力', 'Productivity'),
    ('testing', '测试与质量保障', 'Testing & QA'),
    ('ai-ml', 'AI/ML 工程', 'AI / ML'),
    ('creative', '创意与媒体', 'Creative & Media'),
    ('industry', '行业垂直', 'Industry'),
    ('security', '安全', 'Security'),
    ('automation', '自动化', 'Automation'),
    ('devops', 'DevOps', 'DevOps'),
    ('data-analytics', '数据分析', 'Data & Analytics'),
    ('other', '其他', 'Other'),
]
# 作者显示名（缺省用 username）
AUTHOR_NAMES = {'atomcode': 'AtomCode', 'anthropics': 'Anthropic', 'openai': 'OpenAI', 'google-gemini': 'Google Gemini',
                'huggingface': 'HuggingFace', 'microsoft': 'Microsoft', 'supabase': 'Supabase', 'pytorch': 'PyTorch',
                'n8n-io': 'n8n', 'NousResearch': 'Nous Research', 'openclaw': 'OpenClaw'}


def load_catalog(arg):
    """读取官方 marketplace.json；arg 为本地仓库路径或 None（自动浅克隆）"""
    if arg:
        path = arg if arg.endswith('.json') else os.path.join(arg, '.atomcode-plugin', 'marketplace.json')
        return json.load(open(path, encoding='utf-8'))
    tmp = tempfile.mkdtemp(prefix='ac-plugins-')
    subprocess.run(['git', 'clone', '-q', '--depth', '1', REPO, tmp], check=True)
    return json.load(open(os.path.join(tmp, '.atomcode-plugin', 'marketplace.json'), encoding='utf-8'))


def api_get(path):
    try:
        with urllib.request.urlopen(API + path, timeout=20) as r:
            d = json.load(r)
    except Exception:
        # macOS 自带 Python 常缺根证书，回退到系统 curl
        out = subprocess.run(['curl', '-fsS', '-m', '20', API + path], capture_output=True, text=True, check=True).stdout
        d = json.loads(out)
    if d.get('code') != 0:
        raise RuntimeError(d.get('message'))
    return d['data']


def load_extras():
    """best-effort：从旧 API 取 star / 头像；失败则回退到上次生成的快照"""
    extras = {}
    try:
        page = 1
        while True:
            d = api_get(f'/skills?page={page}&page_size=100&lang=en')
            for it in d['items']:
                a = it.get('author') or {}
                extras[it['name']] = {'stars': it.get('stars') or 0, 'avatar': a.get('avatar') or '',
                                      'gh': it.get('github_homepage') or ''}
            if page * 100 >= d['total']:
                break
            page += 1
        print(f'[api] 取到 {len(extras)} 条 star/头像')
    except Exception as e:
        print(f'[api] 不可用（{e}），沿用上次快照')
        if os.path.exists(OUT):
            old = open(OUT, encoding='utf-8').read()
            m = re.search(r'window\.SKILLS_DATA=(\{.*\});?\s*$', old, re.S)
            if m:
                for s in json.loads(m.group(1))['skills']:
                    extras[s['n']] = {'stars': s.get('st', 0), 'avatar': s.get('av', ''), 'gh': s.get('gh', '')}
    return extras


def symbol(name, used):
    """由名称生成两字母元素符号（Pp / Gw …），尽量避免与已用符号冲突"""
    parts = [p for p in re.split(r'[^A-Za-z0-9]+', name) if p] or ['x']
    cands = []
    if len(parts) > 1:
        cands.append(parts[0][0].upper() + parts[1][0].lower())
    w = parts[0]
    cands += [w[0].upper() + c.lower() for c in w[1:]]
    cands.append(w[0].upper() + (parts[-1][-1].lower() if len(parts[-1]) > 1 else 'x'))
    for c in cands:
        if c not in used:
            used.add(c)
            return c
    return cands[0]


def main():
    cat = load_catalog(sys.argv[1] if len(sys.argv) > 1 else None)
    extras = load_extras()
    used, skills, authors = set(), [], {}
    for i, p in enumerate(cat['plugins']):
        src = p.get('source') or {}
        au = (p.get('author') or {}).get('name') or 'unknown'
        ex = extras.get(p['name'], {})
        # 官方仓库内的插件（plugins/ 子目录）= 已验证
        verified = src.get('url', '').startswith(REPO.rsplit('.git', 1)[0]) and str(src.get('path', '')).startswith('plugins/')
        skills.append({
            'n': p['name'], 'sy': symbol(p['name'], used), 'c': p.get('category') or 'other', 'a': au,
            'd': p.get('description') or '', 'dz': p.get('description_zh') or p.get('description') or '',
            'v': p.get('version') or '', 'h': p.get('homepage') or '', 'gh': ex.get('gh', ''),
            'src': src.get('url', ''), 'st': ex.get('stars', 0), 'av': ex.get('avatar', ''), 'ok': 1 if verified else 0,
        })
        authors.setdefault(au, {'u': au, 'n': AUTHOR_NAMES.get(au, au), 'k': 0, 'av': ex.get('avatar', '')})
        authors[au]['k'] += 1
    counts = {}
    for s in skills:
        counts[s['c']] = counts.get(s['c'], 0) + 1
    data = {
        'v': cat.get('version', ''), 'repo': REPO_WEB, 'marketplace': cat['name'], 'total': len(skills),
        'cats': [{'k': k, 'zh': zh, 'en': en, 'n': counts.get(k, 0)} for k, zh, en in CATS if counts.get(k)],
        'authors': sorted(authors.values(), key=lambda a: -a['k']),
        'skills': skills,
    }
    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    with open(OUT, 'w', encoding='utf-8') as f:
        f.write('/* 由 build_skills.py 生成，请勿手改。来源：' + REPO_WEB + ' */\n')
        f.write('window.SKILLS_DATA=' + json.dumps(data, ensure_ascii=False, separators=(',', ':')) + ';\n')
    print(f'[ok] {len(skills)} 个技能 · {len(data["cats"])} 个分类 · {len(authors)} 位作者 → {os.path.relpath(OUT, ROOT)}')


if __name__ == '__main__':
    main()
