function renderFamilyPage(slug) {
  const f=state.families.find(x=>x.slug===slug);
  if(!f){$('#home-content').textContent='找不到该家族';return;}
  $('#metrics').innerHTML=[['模型',f.count],['衍生模型',f.derived_count],['参与者',f.num_authors],['近30天下载',f.downloads]].map(([label,value])=>`<div class="metric"><label>${esc(f.name)} · ${label}</label><strong>${fmt(value)}</strong><span>当前家族统计</span></div>`).join('');
  $('#home-hero').innerHTML=`<h1>${esc(f.name)}</h1><p>${esc(f.focus)}</p><div id="status"><a href="/families">← 返回全部家族</a></div>`;
  $('#home-content').innerHTML='<section class="section"><div class="section-head"><h2>家族生态</h2><label>展示方式 <select id="family-view" class="btn"><option value="models">模型明细</option><option value="organizations">组织聚合</option></select></label></div><p>近30天下载量降序 · 每页20条。点击发布者行在侧面查看模型。</p><div class="card table-scroll" id="family-list"></div></section>';
  const select=$('#family-view'),target=$('#family-list');
  select.onchange=()=>{closePublisherDrawer?.();renderDetailList(target,slug,select.value,0);};
  return renderDetailList(target,slug,'models',0);
}
const listRequests=new WeakMap();
const exact=n=>n==null?'—':Number(n).toLocaleString('zh-CN');
async function renderDetailList(target,slug,mode,offset,organization=null){
  listRequests.get(target)?.abort();
  const controller=new AbortController();listRequests.set(target,controller);
  target.innerHTML='<div class="loading" role="status">加载中…</div>';
  const query=new URLSearchParams({family:slug,offset:String(offset)});
  if(organization)query.set('organization',organization);
  try{
    const response=await fetch(`/api/${mode==='organizations'?'family-organizations':'family-models'}?${query}`,{signal:controller.signal});
    if(!response.ok)throw Error(`HTTP ${response.status}`);
    const data=await response.json();
    if(!target.isConnected||listRequests.get(target)!==controller)return;
    const rows=mode==='organizations'?data.organizations:data.items;
    const headers=mode==='organizations'?['序号','发布者 / namespace','身份','模型数','Likes','近30天下载 ↓']:['序号','模型','关系','发布时间（HF仓库创建时间）','Likes','近30天下载 ↓'];
    target.innerHTML=`<table class="table"><thead><tr>${headers.map(h=>`<th>${h}</th>`).join('')}</tr></thead><tbody>${rows.map((m,i)=>mode==='organizations'?`<tr class="click org-row" tabindex="0" role="button" aria-label="查看 ${esc(m.name)} 的模型" data-org="${esc(m.name)}"><td>${offset+i+1}</td><td>${esc(m.name)}</td><td>${m.ownership==='official'?'官方':m.ownership==='non_official'?'非官方':'未确认'}</td><td>${exact(m.count)}</td><td>${exact(m.likes)}</td><td>${exact(m.downloads)}</td></tr>`:`<tr><td>${offset+i+1}</td><td class="model" title="${esc(m.id)}"><a href="https://huggingface.co/${m.id.split('/').map(encodeURIComponent).join('/')}" target="_blank" rel="noopener noreferrer">${esc(m.id)}</a><small>${esc(m.author)}</small></td><td>${esc(m.relation||m.variant||'未声明')}</td><td>${m.created_at&&Number.isFinite(Date.parse(m.created_at))?esc(new Date(m.created_at).toLocaleString()):'—'}</td><td>${exact(m.likes)}</td><td>${exact(m.downloads)}</td></tr>`).join('')||'<tr><td colspan="6">暂无记录</td></tr>'}</tbody></table><div class="pager"><button class="btn page-prev" ${offset===0?'disabled':''}>上一页</button><span>第 ${Math.floor(offset/20)+1} 页 · 每页20条</span><button class="btn page-next" ${data.next==null?'disabled':''}>下一页</button></div>`;
    target.querySelector('.page-prev').onclick=()=>renderDetailList(target,slug,mode,Math.max(0,offset-20),organization);
    target.querySelector('.page-next').onclick=()=>renderDetailList(target,slug,mode,data.next,organization);
    target.querySelectorAll('.org-row').forEach(row=>{
      row.onclick=()=>openOrganizationDrawer(slug,row.dataset.org,row);
      row.onkeydown=e=>{if(e.key==='Enter'||e.key===' '){e.preventDefault();row.click();}};
    });
  }catch(error){
    if(error.name==='AbortError'||!target.isConnected||listRequests.get(target)!==controller)return;
    target.innerHTML=`<div class="empty" role="alert">加载失败：${esc(error.message)} <button class="btn retry">重试</button></div>`;
    target.querySelector('.retry').onclick=()=>renderDetailList(target,slug,mode,offset,organization);
  }
}
let closePublisherDrawer=null;
function openOrganizationDrawer(slug,organization,trigger){
  closePublisherDrawer?.();
  const host=$('#drawer');
  host.innerHTML=`<div class="publisher-overlay"><aside class="publisher-panel" role="dialog" aria-modal="true" aria-labelledby="publisher-title"><div class="drawer-title"><h2 id="publisher-title">${esc(organization)} 的模型</h2><button class="btn drawer-close">关闭</button></div><p>近30天下载量降序 · 每页20条 · Likes 为采集时快照</p><div class="table-scroll publisher-models"></div></aside></div>`;
  const target=host.querySelector('.publisher-models'),button=host.querySelector('.drawer-close');
  const oldOverflow=document.body.style.overflow;document.body.style.overflow='hidden';
  const key=e=>{if(e.key==='Escape')closePublisherDrawer?.();};
  document.addEventListener('keydown',key);
  closePublisherDrawer=()=>{listRequests.get(target)?.abort();host.innerHTML='';document.body.style.overflow=oldOverflow;document.removeEventListener('keydown',key);closePublisherDrawer=null;if(trigger?.isConnected)trigger.focus();};
  button.onclick=()=>closePublisherDrawer?.();
  host.querySelector('.publisher-overlay').onclick=e=>{if(e.target.classList.contains('publisher-overlay'))closePublisherDrawer?.();};
  button.focus();renderDetailList(target,slug,'models',0,organization);
}
