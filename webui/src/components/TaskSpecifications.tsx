import { ModalOverlay } from './ModalOverlay';
import { useLayoutEffect, useRef, useState } from 'preact/hooks';
import { taskSpecs, type TaskSpecView } from '../api';
import { useT } from '../settings';

/** Original-message review; all mutations are explicit user button actions. */
export function TaskSpecifications({sessionId, running, initial, onClose, onChanged}:{sessionId:string; running:boolean; initial:string; onClose:()=>void; onChanged?:(seq:number,pinned:boolean)=>void}) {
  const t=useT();
  const [view,setView]=useState<TaskSpecView|null>(null);
  const [error,setError]=useState('');
  const [loading,setLoading]=useState(false);
  const request=useRef<AbortController|null>(null);
  const mode=initial==='pin'||initial==='unpin'?initial:'';
  useLayoutEffect(()=>{
    const previous=document.activeElement;
    const frame=requestAnimationFrame(()=>document.querySelector<HTMLElement>('.task-specifications button')?.focus());
    void load(initial.startsWith('show ')?initial:'list');
    return ()=>{cancelAnimationFrame(frame);request.current?.abort();request.current=null;if(previous instanceof HTMLElement&&previous.isConnected)previous.focus();};
  },[sessionId]);
  async function load(args:string) {
    request.current?.abort();
    const controller=new AbortController();request.current=controller;
    setLoading(true);setError('');
    try {
      const result=await taskSpecs(sessionId,args,controller.signal);
      if(controller.signal.aborted)return;
      if(args.startsWith('pin ')||args.startsWith('unpin ')) {
        const next=await taskSpecs(sessionId,'show '+result.seq,controller.signal);
        if(!controller.signal.aborted){setView(next);onChanged?.(result.seq!,Boolean(result.pinned));}
      } else setView(result);
    } catch(e){if(!controller.signal.aborted)setError(/Unbound|RuntimeUnavailable/.test(String(e))?t('spec.connect'):String(e));}
    finally{if(request.current===controller)setLoading(false);}
  }
  const items=view?.items?.filter(item=>mode==='pin'?!item.pinned:mode==='unpin'?item.pinned:true);
  return <ModalOverlay>
    <section class="modal-card task-specifications" role="dialog" aria-modal="true" aria-label={t('spec.title')} onClick={e=>e.stopPropagation()}>
      <header><h3>{t('spec.title')}</h3><button type="button" class="btn" onClick={onClose}>{t('spec.close')}</button></header>
      <p>{t('spec.explanation')}</p>
      {loading&&<p role="status">{t('spec.loading')}</p>}
      {error&&<p role="alert">{error} <button class="btn" onClick={()=>void load('list')}>{t('spec.back')}</button></p>}
      {view&&<p>{t('spec.budget',{used:view.pinned_bytes,limit:view.budget_bytes})}</p>}
      {view?.items&&<div class="task-spec-list">
        {!items?.length&&<p>{t('spec.empty')}</p>}
        {items?.map(item=><button key={item.seq} type="button" class="btn" disabled={loading} onClick={()=>void load('show '+item.seq)}>
          <span>{item.pinned?'📌 ':''}{new Date(item.at).toLocaleString()} · {item.bytes} bytes</span><span>{item.preview}</span>
        </button>)}
        {mode!=='unpin'&&view.next_before!=null&&<button class="btn" disabled={loading} onClick={()=>void load('list '+view.next_before)}>{t('spec.older')}</button>}
      </div>}
      {view?.text!=null&&<div>
        <pre>{view.text}</pre>
        <footer><button class="btn" disabled={loading} onClick={()=>void load('list')}>{t('spec.back')}</button>
          <button class="btn" disabled={loading||running||(!view.pinned&&view.pinned_bytes+(view.bytes??0)>view.budget_bytes)} onClick={()=>void load((view.pinned?'unpin ':'pin ')+view.seq)}>{t(view.pinned?'spec.unpin':'spec.pin')}</button>
        </footer>
        {running&&<p>{t('spec.running')}</p>}
      </div>}
    </section>
  </ModalOverlay>;
}
