/* AtomCode docs — shared chrome behavior
 * Loads a pre-built JSON index for search; no per-page HTTP fetching.
 */
(function(){
  'use strict';

  // ── theme ──
  const html=document.documentElement;
  function isLight(){return html.classList.contains('light')||html.getAttribute('data-theme')==='light'}
  function applyTheme(theme){
    const light=theme==='light';
    html.classList.toggle('light',light);
    if(light) html.setAttribute('data-theme','light'); else html.removeAttribute('data-theme');
    const btn=document.getElementById('themeBtn');
    if(btn){
      btn.innerHTML=light
        ? '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><circle cx="12" cy="12" r="4"/><path d="M12 2v2M12 20v2M4.93 4.93l1.41 1.41M17.66 17.66l1.41 1.41M2 12h2M20 12h2M4.93 19.07l1.41-1.41M17.66 6.34l1.41-1.41"/></svg>'
        : '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79z"/></svg>';
      btn.setAttribute('aria-label',light?'Switch to dark':'Switch to light');
    }
    try{localStorage.setItem('atomcode_theme',theme)}catch(e){}
  }
  function readTheme(){
    try{
      const s=localStorage.getItem('atomcode_theme');
      if(s==='light'||s==='dark') return s;
      const old=localStorage.getItem('atomcode-theme');
      if(old==='light'||old==='dark') return old;
    }catch(e){}
    return matchMedia('(prefers-color-scheme: light)').matches?'light':'dark';
  }
  applyTheme(readTheme());
  window.toggleDocsTheme=function(){applyTheme(isLight()?'dark':'light')};

  // ── i18n (chrome only — page content stays as authored) ──
  const T={
    'badge.docs':{zh:'DOCS',en:'DOCS'},
    'search.placeholder':{zh:'搜索文档…',en:'Search docs…'},
    'search.trigger.text':{zh:'搜索文档…',en:'Search docs…'},
    'search.empty':{zh:'没有匹配结果',en:'No matches'},
    'search.notloaded':{zh:'搜索索引未加载（请通过 http:// 服务后再试）',en:'Search index not loaded (serve over http:// and retry)'},
    'aria.theme':{zh:'切换主题',en:'Toggle theme'},
    'aria.lang':{zh:'切换语言',en:'Toggle language'},
    'aria.sidebar':{zh:'目录',en:'Menu'},
    'aria.search':{zh:'搜索文档',en:'Search docs'},
    'hdr.repo':{zh:'仓库 →',en:'Repo →'},
    'ftr.copy':{zh:'© 2026 AtomCode · MIT',en:'© 2026 AtomCode · MIT'},
    'ftr.issue':{zh:'报告问题',en:'Report an issue'},
    // sidebar group titles
    'side.g.overview':{zh:'概览',en:'Overview'},
    'side.g.start':{zh:'开始',en:'Get Started'},
    'side.g.usage':{zh:'使用',en:'Usage'},
    'side.g.advanced':{zh:'进阶',en:'Advanced'},
    'side.g.ops':{zh:'问题',en:'Help'},
    // sidebar page labels
    'side.index':{zh:'文档首页',en:'Documentation Home'},
    'side.getting-started':{zh:'快速开始',en:'Quickstart'},
    'side.login':{zh:'登录方式',en:'Login Methods'},
    'side.configuration':{zh:'配置文件',en:'Configuration'},
    'side.basic-usage':{zh:'基本使用',en:'Basic Usage'},
    'side.slash-commands':{zh:'斜杠命令',en:'Slash Commands'},
    'side.keybindings':{zh:'快捷键',en:'Keybindings'},
    'side.sessions':{zh:'会话与撤销',en:'Sessions & Undo'},
    'side.tools':{zh:'内置工具',en:'Built-in Tools'},
    'side.approvals':{zh:'权限审批',en:'Permissions'},
    'side.skills':{zh:'Skills 扩展',en:'Skills'},
    'side.mcp':{zh:'MCP 集成',en:'MCP Integration'},
    'side.plugins':{zh:'Plugin 系统',en:'Plugin System'},
    'side.memory':{zh:'永久记忆',en:'Persistent Memory'},
    'side.project-instructions':{zh:'项目指令文件',en:'Project Instructions'},
    'side.webui':{zh:'WebUI 界面',en:'Web UI'},
    'side.webui-remote-access':{zh:'远程访问指南',en:'Remote Access'},
    'side.faq':{zh:'常见问题',en:'FAQ'},
    // index page hero
    'hero.eyebrow':{zh:'AtomCode · 开源终端 AI 编码助手',en:'AtomCode · Open-source Terminal AI Coding Assistant'},
    'hero.tag.license':{zh:'许可证 MIT',en:'License: MIT'},
    'hero.tag.lang':{zh:'语言 Rust 1.88+',en:'Language: Rust 1.88+'},
    'hero.tag.platforms':{zh:'平台 macOS · Linux · HarmonyOS PC · Windows',en:'Platforms: macOS · Linux · HarmonyOS PC · Windows'},
    'hero.tag.ai':{zh:'100% AI 生成',en:'100% AI-generated'},
  };
  // Determine lang from URL path first (/docs/zh/ vs /docs/en/), fall back
  // to localStorage, default zh.
  function detectLang(){
    const m=location.pathname.match(/\/docs\/(zh|en)\//);
    if(m) return m[1];
    try{const s=localStorage.getItem('atomcode_lang');if(s==='zh'||s==='en') return s;}catch(e){}
    return 'zh';
  }
  let LANG=detectLang();
  try{localStorage.setItem('atomcode_lang',LANG)}catch(e){}
  function t(k){const v=T[k];if(!v)return k;return v[LANG]||v.zh||k;}
  function applyI18n(){
    document.documentElement.lang=LANG==='zh'?'zh-CN':'en';
    document.querySelectorAll('[data-i18n]').forEach(el=>{el.textContent=t(el.dataset.i18n)});
    document.querySelectorAll('[data-i18n-aria]').forEach(el=>{el.setAttribute('aria-label',t(el.dataset.i18nAria))});
    document.querySelectorAll('[data-i18n-placeholder]').forEach(el=>{el.setAttribute('placeholder',t(el.dataset.i18nPlaceholder))});
    const lb=document.getElementById('langBtn');
    if(lb){lb.textContent=LANG==='zh'?'EN':'中';lb.setAttribute('aria-label',t('aria.lang'))}
  }
  function setLang(l){
    LANG=l;
    try{localStorage.setItem('atomcode_lang',LANG)}catch(e){}
    // Navigate to the equivalent page in the other lang directory
    const p=location.pathname;
    const m=p.match(/^(.*\/docs\/)(zh|en)\/([^/]+)$/);
    if(m){location.href=m[1]+l+'/'+m[3]+location.hash;return}
    applyI18n();
  }
  window.toggleDocsLang=function(){setLang(LANG==='zh'?'en':'zh')};

  function onReady(fn){
    if(document.readyState!=='loading') fn();
    else document.addEventListener('DOMContentLoaded',fn);
  }
  onReady(function(){
    applyI18n();
    const tb=document.getElementById('themeBtn');
    if(tb) tb.addEventListener('click',window.toggleDocsTheme);
    const lb=document.getElementById('langBtn');
    if(lb) lb.addEventListener('click',window.toggleDocsLang);

    const hdr=document.querySelector('.dhdr');
    if(hdr){
      const onScroll=()=>hdr.classList.toggle('scrolled',scrollY>30);
      onScroll();addEventListener('scroll',onScroll,{passive:true});
    }

    const sb=document.getElementById('dside');
    const sbToggle=document.getElementById('sbToggle');
    if(sb&&sbToggle){
      const setOpen=(open)=>{
        sb.classList.toggle('open',open);
        sbToggle.textContent=open?'✕':'☰';
        sbToggle.setAttribute('aria-expanded',open?'true':'false');
        document.body.style.overflow=open?'hidden':'';
      };
      sbToggle.addEventListener('click',e=>{e.stopPropagation();setOpen(!sb.classList.contains('open'))});
      sb.addEventListener('click',e=>{if(e.target.closest('a')) setOpen(false)});
      document.addEventListener('click',e=>{
        if(!sb.classList.contains('open'))return;
        if(sb.contains(e.target)||sbToggle.contains(e.target))return;
        setOpen(false);
      });
      addEventListener('resize',()=>{if(innerWidth>768&&sb.classList.contains('open'))setOpen(false)});
    }

    const page=document.body.getAttribute('data-page');
    if(page&&sb){
      sb.querySelectorAll('.dside-link').forEach(a=>{
        const slug=(a.getAttribute('data-slug')||a.getAttribute('href')||'').replace(/^\.?\//,'').replace(/\.html$/,'');
        if(slug===page){a.classList.add('active');a.setAttribute('aria-current','page')}
      });
    }

    initSearch();
  });

  // ── Search ──
  let SEARCH=null;
  let LOAD_PROMISE=null;
  let CUR_RESULTS=[];
  let CUR_SEL=0;

  function resolveIndexUrl(){
    // page lives at /docs/<lang>/<slug>.html — index lives at /docs/search-index.<lang>.json
    const m=location.pathname.match(/^(.*\/docs\/)(zh|en)\//);
    if(m) return m[1]+'search-index.'+m[2]+'.json';
    // fallback: legacy flat layout
    const path=location.pathname;
    const lastSlash=path.lastIndexOf('/');
    return (lastSlash>=0?path.slice(0,lastSlash+1):'./')+'search-index.'+LANG+'.json';
  }
  function loadIndex(){
    if(SEARCH) return Promise.resolve(SEARCH);
    if(LOAD_PROMISE) return LOAD_PROMISE;
    LOAD_PROMISE=fetch(resolveIndexUrl(),{cache:'no-cache'})
      .then(r=>r.ok?r.json():Promise.reject(new Error('HTTP '+r.status)))
      .then(j=>{SEARCH=j;return j})
      .catch(err=>{console.warn('[docs] search index load failed:',err);return null});
    return LOAD_PROMISE;
  }

  function escapeHtml(s){return String(s||'').replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]))}
  function escapeRe(s){return s.replace(/[.*+?^${}()|[\]\\]/g,'\\$&')}

  function initSearch(){
    const modal=document.getElementById('searchModal');
    const input=document.getElementById('searchInput');
    const results=document.getElementById('searchResults');
    const triggers=document.querySelectorAll('[data-open-search]');
    if(!modal||!input||!results) return;
    const bg=modal.querySelector('.search-modal-bg');

    function open(){
      modal.classList.add('open');
      loadIndex().then(()=>{
        input.value='';render('');
        setTimeout(()=>input.focus(),0);
      });
    }
    function close(){modal.classList.remove('open')}

    triggers.forEach(t=>t.addEventListener('click',e=>{e.preventDefault();open()}));
    bg.addEventListener('click',close);

    addEventListener('keydown',e=>{
      const isModK=(e.metaKey||e.ctrlKey)&&e.key.toLowerCase()==='k';
      const tag=document.activeElement&&document.activeElement.tagName||'';
      const isSlash=e.key==='/'&&!modal.classList.contains('open')&&!/^(INPUT|TEXTAREA)$/i.test(tag);
      if(isModK||isSlash){e.preventDefault();open();return}
      if(!modal.classList.contains('open')) return;
      if(e.key==='Escape'){e.preventDefault();close()}
      else if(e.key==='ArrowDown'){e.preventDefault();move(1)}
      else if(e.key==='ArrowUp'){e.preventDefault();move(-1)}
      else if(e.key==='Enter'){
        e.preventDefault();
        const hit=CUR_RESULTS[CUR_SEL];
        if(hit) location.href=hit.href;
      }
    });
    input.addEventListener('input',()=>render(input.value));

    function move(d){
      if(!CUR_RESULTS.length) return;
      CUR_SEL=(CUR_SEL+d+CUR_RESULTS.length)%CUR_RESULTS.length;
      results.querySelectorAll('.search-item').forEach((el,i)=>el.classList.toggle('active',i===CUR_SEL));
      const sel=results.querySelector('.search-item.active');
      if(sel) sel.scrollIntoView({block:'nearest'});
    }

    function score(text,term){
      if(!text) return 0;
      const t=text.toLowerCase();
      if(t===term) return 10;
      if(t.startsWith(term)) return 6;
      if(t.indexOf(term)>=0) return 3;
      return 0;
    }
    function bodyMatch(body,term){
      if(!body) return {score:0,snippet:''};
      const t=body.toLowerCase();
      const idx=t.indexOf(term);
      if(idx<0) return {score:0,snippet:''};
      const start=Math.max(0,idx-40);
      const end=Math.min(body.length,idx+term.length+80);
      return {score:1,snippet:(start>0?'…':'')+body.slice(start,end)+(end<body.length?'…':'')};
    }
    function highlight(snip,term){
      if(!snip||!term) return escapeHtml(snip||'');
      const safe=escapeHtml(snip);
      return safe.replace(new RegExp(escapeRe(term),'gi'),m=>'<mark>'+m+'</mark>');
    }

    function pageHref(slug){return slug==='index'?'./index.html':'./'+slug+'.html'}

    function render(q){
      const term=(q||'').trim().toLowerCase();
      if(!SEARCH){
        results.innerHTML='<div class="search-empty">'+t('search.notloaded')+'</div>';
        CUR_RESULTS=[];return;
      }
      if(!term){
        CUR_RESULTS=SEARCH.map(p=>({
          href:pageHref(p.slug),page:p.group||p.title,heading:p.title,snippet:p.lede||''
        }));
        renderList('');return;
      }
      const hits=[];
      const seenPage={};
      for(const p of SEARCH){
        const pageMatch=score(p.title,term);
        let sectionHit=false;
        for(const s of p.sections){
          const hMatch=score(s.heading,term);
          const bMatch=bodyMatch(s.body,term);
          if(pageMatch+hMatch+bMatch.score===0) continue;
          hits.push({
            score:pageMatch*3+hMatch*2+bMatch.score,
            href:pageHref(p.slug)+(s.id?'#'+s.id:''),
            page:p.title,heading:s.heading||p.title,snippet:bMatch.snippet||s.body.slice(0,120),
          });
          sectionHit=true;
        }
        if(pageMatch>0&&!sectionHit){
          if(!seenPage[p.slug]){
            seenPage[p.slug]=true;
            hits.push({score:pageMatch*3,href:pageHref(p.slug),page:p.title,heading:p.title,snippet:p.lede||''});
          }
        }
      }
      hits.sort((a,b)=>b.score-a.score);
      CUR_RESULTS=hits.slice(0,30);
      renderList(term);
    }

    function renderList(term){
      CUR_SEL=0;
      if(!CUR_RESULTS.length){
        results.innerHTML='<div class="search-empty">'+t('search.empty')+'</div>';return;
      }
      results.innerHTML=CUR_RESULTS.map((h,i)=>`
        <a class="search-item${i===0?' active':''}" href="${h.href}">
          <span class="search-item-page">${escapeHtml(h.page)}</span>
          <span class="search-item-h">${escapeHtml(h.heading)}</span>
          ${h.snippet?`<span class="search-item-snip">${highlight(h.snippet,term||'')}</span>`:''}
        </a>`).join('');
    }
  }
})();
