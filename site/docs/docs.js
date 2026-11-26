/* ==================== AtomCode 文档交互 ==================== */
(()=>{'use strict';
const $=(s,r=document)=>r.querySelector(s),$$=(s,r=document)=>[...r.querySelectorAll(s)];
const UI=window.DOCS_UI||{copy:'复制',copied:'已复制',noRes:'没有匹配的结果'};
const IDX=window.DOCS_INDEX||[];
const lang=document.body.dataset.lang||'zh';
const reduce=matchMedia('(prefers-reduced-motion: reduce)').matches;
try{localStorage.setItem('atomcode_lang',lang);localStorage.setItem('ac-lang',lang)}catch(e){}
const esc=s=>String(s).replace(/[&<>"]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c]));
let toastT;
function toast(m){const t=$('#toast');t.textContent=m;t.classList.add('on');clearTimeout(toastT);toastT=setTimeout(()=>t.classList.remove('on'),1600)}

/* ==================== 主题 ==================== */
$('#themeBtn').onclick=()=>{const l=document.documentElement.classList.toggle('light');try{localStorage.setItem('atomcode_theme',l?'light':'dark')}catch(e){}};

/* ==================== 侧栏抽屉（窄屏） ==================== */
const ds=$('#ds'),dsBg=$('#dsBg'),sbBtn=$('#sbBtn');
function side(on){ds.classList.toggle('open',on);dsBg.classList.toggle('on',on);sbBtn.setAttribute('aria-expanded',on)}
sbBtn.onclick=()=>side(!ds.classList.contains('open'));dsBg.onclick=()=>side(false);
// 当前页滚动到侧栏可见区
const cur=$('.sl.on',ds);if(cur)cur.scrollIntoView({block:'center'});

/* ==================== 标题锚点 + 代码复制 ==================== */
const prose=$('#prose');
$$('h2[id],h3[id]',prose).forEach(h=>{if(h.closest('.doc-card'))return;const a=document.createElement('a');a.className='anc';a.href='#'+h.id;a.textContent='#';a.setAttribute('aria-hidden','true');h.prepend(a)});
function copyText(text,btn){
  const ok=()=>{btn.textContent=UI.copied;btn.classList.add('ok');setTimeout(()=>{btn.textContent=UI.copy;btn.classList.remove('ok')},1400)};
  const fb=()=>{const ta=document.createElement('textarea');ta.value=text;ta.style.cssText='position:fixed;opacity:0';document.body.appendChild(ta);ta.select();try{document.execCommand('copy');ok()}catch(e){}ta.remove()};
  navigator.clipboard&&isSecureContext?navigator.clipboard.writeText(text).then(ok,fb):fb();
}
$$('pre',prose).forEach(pre=>{
  const w=document.createElement('div');w.className='pre-w';pre.parentNode.insertBefore(w,pre);w.appendChild(pre);
  const b=document.createElement('button');b.className='cp';b.type='button';b.textContent=UI.copy;
  // 复制时去掉提示符行首的 "$ "
  b.onclick=()=>copyText(pre.innerText.replace(/^\$ /gm,'').trimEnd(),b);w.appendChild(b);
});

/* ==================== 本页目录 + 滚动高亮 + 阅读进度 ==================== */
const heads=$$('h2[id],h3[id]',prose).filter(h=>!h.closest('.doc-card'));
const tocList=$('#tocList'),toc=$('#toc');
tocList.innerHTML=heads.map(h=>`<a href="#${esc(h.id)}" class="${h.tagName.toLowerCase()}">${esc(h.textContent.replace(/^#/,''))}</a>`).join('');
if(heads.length<2)toc.classList.add('empty');
const tocA=$$('a',tocList);
$('#toTop').onclick=()=>scrollTo({top:0,behavior:reduce?'auto':'smooth'});
const prog=$('#prog');let raf=0;
function onScroll(){if(raf)return;raf=requestAnimationFrame(()=>{raf=0;
  const h=document.documentElement.scrollHeight-innerHeight;prog.style.transform=`scaleX(${h>0?Math.min(1,scrollY/h):0})`;
  let k=-1;const y=innerHeight*.28;heads.forEach((hd,i)=>{if(hd.getBoundingClientRect().top<y)k=i});
  tocA.forEach((a,i)=>a.classList.toggle('on',i===k));
  const on=tocA[k];if(on){const r=on.getBoundingClientRect(),tr=toc.getBoundingClientRect();if(r.top<tr.top+40||r.bottom>tr.bottom-40)on.scrollIntoView({block:'nearest'})}
})}
addEventListener('scroll',onScroll,{passive:true});onScroll();

/* ==================== 搜索（⌘K / /） ==================== */
const sm=$('#sm'),sIn=$('#sIn'),sList=$('#sList');
let res=[],ri=0;
function hl(s,q){if(!q)return esc(s);const i=s.toLowerCase().indexOf(q);return i<0?esc(s):esc(s.slice(0,i))+'<mark>'+esc(s.slice(i,i+q.length))+'</mark>'+esc(s.slice(i+q.length))}
function snip(x,q){const i=x.toLowerCase().indexOf(q);if(i<0)return x.slice(0,90);const a=Math.max(0,i-30);return (a?'…':'')+x.slice(a,a+110)}
function search(){
  const q=sIn.value.trim().toLowerCase();res=[];
  if(!q){res=IDX.map(p=>({u:`./${p.s}.html`,y:p.y,t:p.t,sub:'',d:p.x.slice(0,90)}))}
  else IDX.forEach(p=>{
    const inT=p.t.toLowerCase().includes(q);
    if(inT)res.push({u:`./${p.s}.html`,y:p.y,t:p.t,sub:'',d:snip(p.x,q),sc:3});
    p.h.forEach(([id,txt])=>{if(txt.toLowerCase().includes(q))res.push({u:`./${p.s}.html#${id}`,y:p.y,t:txt,sub:p.t,d:'',sc:2})});
    if(!inT&&p.x.toLowerCase().includes(q))res.push({u:`./${p.s}.html`,y:p.y,t:p.t,sub:'',d:snip(p.x,q),sc:1});
  });
  res.sort((a,b)=>(b.sc||0)-(a.sc||0));res=res.slice(0,40);ri=0;render(q);
}
function render(q){
  sList.innerHTML=res.length?res.map((r,i)=>`<li class="${i===ri?'on':''}" role="option"><a href="${r.u}"><i>${esc(r.y)}</i><b>${hl(r.t,q)}${r.sub?`<small>${esc(r.sub)}</small>`:''}</b><span>${hl(r.d,q)}</span></a></li>`).join('')
    :`<li class="none">${UI.noRes}</li>`;
  const on=$('li.on',sList);on&&on.scrollIntoView({block:'nearest'});
}
function openS(){sm.classList.add('on');sIn.value='';search();setTimeout(()=>sIn.focus(),30)}
function closeS(){sm.classList.remove('on')}
$('#sBtn').onclick=openS;
sm.addEventListener('click',e=>{if(e.target===sm)closeS()});
sIn.addEventListener('input',search);
sIn.addEventListener('keydown',e=>{
  const n=Math.max(1,res.length);
  if(e.key==='ArrowDown'){e.preventDefault();ri=(ri+1)%n;render(sIn.value.trim().toLowerCase())}
  else if(e.key==='ArrowUp'){e.preventDefault();ri=(ri-1+n)%n;render(sIn.value.trim().toLowerCase())}
  else if(e.key==='Enter'){e.preventDefault();const r=res[ri];if(r)location.href=r.u}
});
sList.addEventListener('click',()=>closeS());
addEventListener('keydown',e=>{
  if((e.metaKey||e.ctrlKey)&&e.key.toLowerCase()==='k'){e.preventDefault();sm.classList.contains('on')?closeS():openS();return}
  if(e.key==='Escape'){closeS();side(false);return}
  const t=(document.activeElement||{}).tagName;
  if(e.key==='/'&&t!=='INPUT'&&t!=='TEXTAREA'&&!sm.classList.contains('on')){e.preventDefault();openS()}
});
})();
