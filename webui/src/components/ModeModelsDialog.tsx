import { ModalOverlay } from './ModalOverlay';
import { useEffect, useRef, useState } from 'preact/hooks';
import { createPortal } from 'preact/compat';
import { getModeModels, saveModeModels, type ModeModelsInfo, type ModeBindings } from '../api';
import { useSettings } from '../settings';

export function ModeModelsDialog({onClose}: {onClose: () => void}) {
  const {t} = useSettings();
  const [data,setData] = useState<ModeModelsInfo|null>(null);
  const [draft,setDraft] = useState<ModeBindings>({plan:{},build:{}});
  const [error,setError] = useState('');
  const [saving,setSaving] = useState(false);
  const alive=useRef(true);
  const dialog=useRef<HTMLElement>(null);
  useEffect(()=>{
    const previous=document.activeElement as HTMLElement|null;
    dialog.current?.querySelector<HTMLButtonElement>('button')?.focus();
    return()=>{if(previous?.isConnected)previous.focus();};
  },[]);
  useEffect(() => {alive.current=true; getModeModels().then(value=>{if(alive.current){setData(value);setDraft(value.bindings);}}).catch(e=>{if(alive.current)setError(String(e));});return()=>{alive.current=false;};},[]);
  useEffect(()=>{const key=(event:KeyboardEvent)=>{
    if(event.key==='Tab'){
      const nodes=Array.from(dialog.current?.querySelectorAll<HTMLElement>('button:not(:disabled), select:not(:disabled)')??[]);
      const first=nodes[0],last=nodes[nodes.length-1];
      if(event.shiftKey&&document.activeElement===first){event.preventDefault();last?.focus();}
      else if(!event.shiftKey&&document.activeElement===last){event.preventDefault();first?.focus();}
    }
};window.addEventListener('keydown',key);return()=>window.removeEventListener('keydown',key);},[saving]);
  async function save() {
    if(saving||!data)return;
    setSaving(true);setError('');
    try {
      const result=await saveModeModels(draft);
      if(!alive.current)return;
      setData(result);setDraft(result.bindings);
      onClose();
    }catch(e){if(alive.current)setError(String(e));}
    finally{if(alive.current)setSaving(false);}
  }
  return createPortal(<ModalOverlay>
    <section ref={dialog} class="modal-card mode-models-modal" role="dialog" aria-modal="true" aria-labelledby="mode-bindings-title">
      <header class="modal-header"><h3 id="mode-bindings-title">{t('modeBindings.title')}</h3><button class="ghost-btn modal-close" type="button" onClick={onClose} disabled={saving} aria-label={t('modeBindings.close')}>×</button></header>
      <div class="modal-body mode-bindings-body">
        <p>{t('modeBindings.description')}</p>
        {!data&&!error&&<p role="status">{t('settings.loading')}</p>}
        {(['plan','build'] as const).map(mode=>{
          const binding=draft[mode];
          const choice=data?.models.find(item=>item.id===binding.model);
          const levels=choice?.effort_levels ?? data?.reasoning_effort_levels ?? [];
          return <fieldset disabled={!data||saving} key={mode}><legend>{mode==='plan'?'Plan':'Build'}</legend>
            <label>{t('modeBindings.model')}<select value={binding.model??''} onChange={event=>{
              const model=event.currentTarget.value;
              setDraft(old=>({...old,[mode]:{...(model?{model}:{})}}));
            }}><option value="">{t(mode==='plan'?'modeBindings.follow':'modeBindings.restore')}</option>
              {data?.models.map(model=><option key={model.id} value={model.id}>{model.label}</option>)}
            </select></label>
            <label>{t('modeBindings.effort')}<select value={binding.reasoning_effort??''} onChange={event=>{
              const value=event.currentTarget.value;setDraft(old=>({...old,[mode]:{...old[mode],reasoning_effort:value||undefined}}));
            }}><option value="">{t('modeBindings.profile')}</option>
              {levels.length>0&&<option value="default">{t('modeBindings.apiDefault')}</option>}
              {levels.map(level=><option key={level} value={level}>{level}</option>)}
            </select></label>
          </fieldset>;
        })}
        <p>{t('modeBindings.temporary')}</p>
        {error&&<p class="modal-error" role="alert">{error}</p>}
      </div>
      <footer class="modal-footer"><button class="btn" onClick={onClose} disabled={saving}>{t('modeBindings.cancel')}</button><button class="btn btn-primary" onClick={save} disabled={!data||saving}>{saving?t('modeBindings.saving'):t('modeBindings.save')}</button></footer>
    </section>
  </ModalOverlay>,document.body);
}
