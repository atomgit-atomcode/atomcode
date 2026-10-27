#!/usr/bin/env python3
"""Native cross-agent trials. Loopback relay normalizes only model/effort/output cap."""
import argparse, concurrent.futures, hashlib, http.server, importlib.util, json, os
from pathlib import Path
import secrets, subprocess, tempfile, threading, time, urllib.error, urllib.request

spec=importlib.util.spec_from_file_location('design_eval',Path(__file__).with_name('eval.py'))
e=importlib.util.module_from_spec(spec);spec.loader.exec_module(e)
TOOLS=Path('/tmp/atomcode-cross-agent-tools/node_modules')
DSH=Path('/tmp/atomcode-cross-dsh-tools/node_modules/.bin/dsh')
BUN=TOOLS/'.bin/bun'
OMP=TOOLS/'@oh-my-pi/pi-coding-agent/src/cli.ts'

def sanitize(value):
    if isinstance(value,dict):
        kind=str(value.get('type','')).lower()
        if 'reasoning' in kind or 'thinking' in kind:return None
        return {k:x for k,v in value.items() if k not in ['reasoning','thinking','reasoning_content','providerPayload','encrypted_content','signature'] and (x:=sanitize(v)) is not None}
    if isinstance(value,list):return [x for v in value if (x:=sanitize(v)) is not None]
    return value

class Relay:
    def __init__(self,endpoint,key,model,out,cap=16000):
        self.endpoint=endpoint.removesuffix('/chat/completions');self.key=key;self.model=model
        self.out=out;self.cap=cap;self.token=secrets.token_hex(24);self.lock=threading.Lock();self.records=[];self.active_records=[]
        relay=self
        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self,*_):pass
            def do_POST(self):
                started=time.monotonic();record={'route':self.path.split('/')[1],'path':self.path.split('/')[-1]}
                if self.headers.get('Authorization')!='Bearer '+relay.token:
                    self.send_error(401);return
                if record['path'] not in ['completions','responses']:
                    self.send_error(404);return
                body=json.loads(self.rfile.read(int(self.headers['Content-Length'])))
                record.update({'requested_model':body.get('model'),'stream':body.get('stream'),
                    'requested_effort':body.get('reasoning_effort',body.get('reasoning',{}).get('effort')),
                    'requested_cap':body.get('max_output_tokens',body.get('max_completion_tokens',body.get('max_tokens'))),
                    'message_count':len(body.get('messages',body.get('input',[]))) if isinstance(body.get('messages',body.get('input',[])),list) else None,
                    'tool_names':[t.get('name',t.get('function',{}).get('name')) for t in body.get('tools',[])],
                    'brainstorming_visible':'brainstorming' in json.dumps(body,ensure_ascii=False).lower()})
                body['model']=relay.model
                if record['path']=='responses':
                    body['reasoning']={**body.get('reasoning',{}),'effort':'max'};body['max_output_tokens']=relay.cap
                    body.pop('reasoning_effort',None)
                    suffix='/responses'
                else:
                    requested_cap=body.get('max_tokens',body.get('max_completion_tokens'))
                    body['reasoning_effort']='max';body.pop('max_completion_tokens',None)
                    body['max_tokens']=requested_cap if getattr(relay,'preserve_caps',False) and requested_cap else relay.cap
                    suffix='/chat/completions'
                if getattr(relay,'omit_effort',False):
                    body.pop('reasoning_effort',None)
                    body.pop('thinking',None)
                    body.pop('enable_thinking',None)
                record.update({'effective_model':relay.model,'effective_effort':body.get('reasoning_effort',body.get('reasoning',{}).get('effort')),'effective_cap':body.get('max_tokens',body.get('max_output_tokens'))})
                observer = getattr(relay, 'request_observer', None)
                if observer:
                    observer(record['route'], body)
                with relay.lock:relay.active_records.append(record)
                request=urllib.request.Request(relay.endpoint+suffix,json.dumps(body).encode(),
                    {'Authorization':'Bearer '+relay.key,'Content-Type':'application/json','Accept-Encoding':'identity'})
                try:
                    with urllib.request.build_opener(e.RejectRedirects()).open(request,timeout=300) as response:
                        record['status']=response.status;record['headers_seconds']=time.monotonic()-started
                        self.send_response(response.status);self.send_header('Content-Type',response.headers.get('Content-Type','application/json'))
                        self.send_header('Connection','close');self.end_headers()
                        pending=b'';terminal=False
                        while chunk:=response.read1(65536):
                            record.setdefault('first_body_seconds',time.monotonic()-started)
                            try:self.wfile.write(chunk);self.wfile.flush()
                            except (BrokenPipeError,ConnectionResetError):break
                            pending+=chunk
                            while b'\n' in pending:
                                line,pending=pending.split(b'\n',1)
                                if line.startswith(b'data:'):
                                    payload=line[5:].strip()
                                    if payload==b'[DONE]':terminal=True;break
                                    try:
                                        event=json.loads(payload);obj=event.get('response',event)
                                        if event.get('type') in ['response.completed','response.incomplete','response.failed']:terminal=True
                                        if obj.get('model'):record['actual_model']=obj['model']
                                        if obj.get('usage'):record['usage']=obj['usage']
                                        for choice in event.get('choices',[]):
                                            delta=choice.get('delta',{})
                                            reasoning=delta.get('reasoning_content') or delta.get('reasoning')
                                            content=delta.get('content')
                                            if isinstance(reasoning,str) and reasoning:
                                                record.setdefault('first_reasoning_seconds',time.monotonic()-started)
                                                record['reasoning_chars']=record.get('reasoning_chars',0)+len(reasoning)
                                            if isinstance(content,str) and content:
                                                record.setdefault('first_text_seconds',time.monotonic()-started)
                                                record['text_chars']=record.get('text_chars',0)+len(content)
                                            if choice.get('finish_reason'):record['finish_reason']=choice['finish_reason']
                                    except (ValueError,TypeError):pass
                            if terminal:break
                        self.close_connection=True
                except urllib.error.HTTPError as error:
                    record['status']=error.code
                    self.send_response(error.code);self.send_header('Content-Type','application/json');self.end_headers()
                    self.wfile.write(json.dumps({'error':{'message':'Upstream HTTP '+str(error.code),'type':'upstream_error'}}).encode())
                except Exception as error:
                    record['error_type']=type(error).__name__
                    try:self.send_error(502)
                    except OSError:pass
                finally:
                    record['seconds']=time.monotonic()-started
                    with relay.lock:
                        relay.records.append(record);e.save(relay.out/'relay.json',relay.records)
        self.server=http.server.ThreadingHTTPServer(('127.0.0.1',0),Handler)
        threading.Thread(target=self.server.serve_forever,daemon=True).start()
    def url(self,route):return f'http://127.0.0.1:{self.server.server_port}/{route}'

def native_events(stdout,key):
    events=[]
    for line in stdout.splitlines():
        try:v=json.loads(line)
        except ValueError:continue
        if isinstance(v,dict) and (v:=sanitize(v)) is not None:events.append(v)
    return json.loads(json.dumps(events).replace(key,'[REDACTED]'))

def answer(agent,events):
    if agent=='atomcode':return ''.join(x.get('text','') for x in events if x.get('type')=='message.delta')
    if agent=='opencode':return '\n'.join(x.get('part',{}).get('text','') for x in events if x.get('type')=='text')
    if agent=='codex':return '\n'.join(x.get('item',{}).get('text','') for x in events if x.get('type')=='item.completed' and x.get('item',{}).get('type')=='agent_message')
    if agent=='omp':return '\n'.join(c.get('text','') for x in events if x.get('type')=='message_end' and x.get('message',{}).get('role')=='assistant' for c in x['message'].get('content',[]) if c.get('type')=='text')
    if agent=='dsh':return '\n'.join(x.get('text','') for x in events if x.get('type')=='final')
    return ''

def terminal_ok(agent, events):
    if agent == 'atomcode':return any(x.get('type')=='turn.completed' and x.get('exit_code')==0 for x in events)
    if agent == 'codex':return any(x.get('type')=='turn.completed' for x in events)
    if agent == 'opencode':return any(x.get('type')=='step_finish' and x.get('part',{}).get('reason')=='stop' for x in events)
    if agent == 'dsh':return any(x.get('type')=='final' for x in events)
    if agent == 'omp':
        messages=[x.get('message',{}) for x in events if x.get('type')=='message_end' and x.get('message',{}).get('role')=='assistant']
        return any(x.get('type')=='agent_end' for x in events) and bool(messages) and messages[-1].get('stopReason')=='stop'
    return False

def run_agent(agent,case,rep,relay,out):
    route=f'{agent}-{case["id"]}-{rep}';path=out/(route+'.json')
    result={'agent':agent,'case':case['id'],'repetition':rep,'status':'running','turns':[]}
    with tempfile.TemporaryDirectory(prefix='atomcode-cross-') as tmp:
        root=Path(tmp);project=root/'project';project.mkdir();home=root/'state';home.mkdir();base=relay.url(route)
        env={**os.environ,'NO_PROXY':'127.0.0.1,localhost','no_proxy':'127.0.0.1,localhost',
            'XDG_CONFIG_HOME':str(home/'xdg-config'),'XDG_DATA_HOME':str(home/'xdg-data'),'XDG_CACHE_HOME':str(home/'xdg-cache'),
            'CROSS_AGENT_TOKEN':relay.token}
        # Do not give a child the real model credential inherited from this host.
        for key in list(env):
            if key.endswith(('API_KEY','ACCESS_TOKEN','AUTH_TOKEN')) and key!='CROSS_AGENT_TOKEN':env.pop(key,None)
        original=json.loads((e.ROOT/'results/skill-original-prompts.json').read_text())['loaded_skill']
        skill=project/'.agents/skills/brainstorming/SKILL.md';skill.parent.mkdir(parents=True);skill.write_text(original)
        result['skill_sha256']=hashlib.sha256(original.encode()).hexdigest()
        config=root/'config.toml';q=lambda x:json.dumps(x,ensure_ascii=False)
        session=None
        if agent=='atomcode':
            env['ATOMCODE_HOME']=str(home)
            native=project/'.atomcode/skills/brainstorming/SKILL.md';native.parent.mkdir(parents=True);native.write_text(original)
            config.write_text('default_provider="eval"\n[providers.eval]\ntype="openai"\n'+f'model={q(relay.model)}\nbase_url={q(base)}\napi_key={q(relay.token)}\nreasoning_effort="max"\ncontext_window=1000000\nmax_tokens=16000\nretry_max_attempts=1\n[coding]\nmax_rounds=12\n[permissions]\ndeny=["Bash(*)","Write(*)","Edit(*)"]\n')
        elif agent=='codex':
            env['CODEX_HOME']=str(home)
            (home/'config.toml').write_text(f'model={q(relay.model)}\nmodel_provider="eval"\nmodel_reasoning_effort="max"\napproval_policy="never"\nsandbox_mode="read-only"\n[model_providers.eval]\nname="eval"\nbase_url={q(base)}\nenv_key="CROSS_AGENT_TOKEN"\nwire_api="responses"\nrequest_max_retries=0\nstream_max_retries=0\n')
        elif agent=='opencode':
            cfg={'model':'eval/'+relay.model,'small_model':'eval/'+relay.model,'provider':{'eval':{'npm':'@ai-sdk/openai-compatible','name':'eval','options':{'baseURL':base,'apiKey':relay.token},'models':{relay.model:{'name':relay.model,'reasoning':True,'limit':{'context':1000000,'output':relay.cap},'options':{'reasoningEffort':'max'}}}}},'permission':{'*':'deny','skill':'allow','read':'allow','glob':'allow','grep':'allow','todowrite':'allow'}}
            f=root/'opencode.json';e.save(f,cfg)
            env.update({'OPENCODE_CONFIG':str(f),'OPENCODE_CONFIG_DIR':str(home),'OPENCODE_DISABLE_AUTOUPDATE':'1','OPENCODE_DISABLE_MODELS_FETCH':'1','OPENCODE_EXPERIMENTAL_PLAN_MODE':'0'})
        elif agent=='omp':
            env['PI_CODING_AGENT_DIR']=str(home)
            e.save(home/'models.yml',{'providers':{'eval':{'baseUrl':base,'apiKey':relay.token,'api':'openai-completions','models':[{'id':relay.model,'name':relay.model,'reasoning':True,'input':['text'],'contextWindow':1000000,'maxTokens':relay.cap,'cost':{'input':0,'output':0,'cacheRead':0,'cacheWrite':0},'compat':{'supportsReasoningEffort':True,'thinkingFormat':'openai'}}]}}})
        elif agent=='dsh':
            env['DSH_HOME']=str(home)
            patch=[{'id':'llm-pi-ai','config':{'providers':{'eval':{'apiKeyEnv':'CROSS_AGENT_TOKEN','api':'openai-completions','baseURL':base,'reasoning':'max','defaultMaxTokens':relay.cap,'defaultContextWindow':1000000,'compat':{'thinkingFormat':'deepseek'},'models':[{'id':relay.model,'contextWindow':1000000,'maxTokens':relay.cap,'reasoningEfforts':{'max':'max'}}]}}}}, {'id':'agent-default-model','config':{'provider':'eval','model':relay.model}}]
            e.save(root/'patch.yml',patch)
        if agent=='atomcode':config.write_text(config.read_text().replace('max_tokens=16000',f'max_tokens={relay.cap}'))
        for index,user in enumerate(case['turns']):
            if agent=='atomcode':cmd=[str(getattr(relay,'atomcode_binary',Path('/tmp/atomcode-cross-agent-tools/atomcode-frozen'))),'--config',str(config),'--provider','eval','-C',str(project),'--dev','--no-telemetry','--output-format','jsonl','-p',user]+(['--continue'] if index else [])
            elif agent=='codex':cmd=['/usr/local/bin/codex','exec']+(['resume',session] if index else [])+['--skip-git-repo-check','--json',user]
            elif agent=='opencode':cmd=['/Users/theo/.opencode/bin/opencode','run','--pure','--dir',str(project),'--model','eval/'+relay.model,'--format','json']+(['--session',session] if index else [])+[user]
            elif agent=='omp':cmd=[str(BUN),str(OMP),'--cwd',str(project),'--model','eval/'+relay.model,'--thinking','max','--no-lsp','--no-extensions','--mode','json','--print']+(['--continue'] if index else [])+[user]
            else:cmd=[str(DSH),'--profile','headless','--patch',str(root/'patch.yml'),'--json']+(['--session-id',session] if index else [])+[user]
            started=time.monotonic()
            try:
                proc=subprocess.run(cmd,cwd=project,env=env,text=True,capture_output=True,timeout=300)
                events=native_events(proc.stdout,relay.key);text=answer(agent,events)
                for event in events:
                    if agent=='codex' and event.get('type')=='thread.started':session=event.get('thread_id')
                    elif agent=='opencode' and event.get('sessionID'):session=event['sessionID']
                    elif agent=='dsh' and event.get('sessionId'):session=event['sessionId']
                turn={'user':user,'text':text,'events':events,'exit_code':proc.returncode,'seconds':time.monotonic()-started}
                result['turns'].append(turn)
                if proc.returncode or not text.strip() or not terminal_ok(agent,events):
                    result['status']='execution_failure';result['stderr_tail']=proc.stderr[-3000:].replace(relay.key,'[REDACTED]');break
                if agent in ['codex','opencode','dsh'] and len(case['turns'])>1 and session is None:
                    result['status']='session_id_missing';break
            except subprocess.TimeoutExpired:
                result['status']='timeout';break
            finally:e.save(path,result)
            print(json.dumps({'agent':agent,'case':case['id'],'rep':rep,'turn':index+1,'chars':len(text)}),flush=True)
        else:result['status']='review_required'
        result['session_id']=session;e.save(path,result)
    return result['status']

def main():
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('--output',type=Path,required=True)
    p.add_argument('--agents',nargs='+',choices=['atomcode','codex','opencode','omp','dsh'],default=['atomcode','codex','opencode','omp','dsh'])
    p.add_argument('--max-output-tokens',type=int,default=16000);p.add_argument('--cases',type=Path,default=e.ROOT/'cross-cases.json');p.add_argument('--smoke',action='store_true');p.add_argument('--repetitions',type=int,default=1);p.add_argument('--concurrency',type=int,default=4)
    p.add_argument('--atomcode-binary',type=Path,default=Path('/tmp/atomcode-cross-agent-tools/atomcode-frozen'))
    a=p.parse_args();a.output.mkdir(parents=True,exist_ok=False)
    endpoint,key,cfg=e.configured_provider(Path.home()/'.atomcode/config.toml','taotoken/deepseek-v4.1-flash-1017')
    relay=Relay(endpoint,key,cfg['model'],a.output,a.max_output_tokens)
    relay.atomcode_binary=a.atomcode_binary.resolve()
    cases=[{'id':'smoke','turns':['只回复 READY，不调用任何工具。']}] if a.smoke else json.loads(a.cases.read_text())
    e.save(a.output/'manifest.json',{'model':cfg['model'],'effort':'max','output_cap':a.max_output_tokens,'agents':a.agents,'repetitions':a.repetitions,'cases':cases,'atomcode_binary_sha256':hashlib.sha256(relay.atomcode_binary.read_bytes()).hexdigest(),'relay':'loopback; native protocol retained; model, effort and output cap normalized; API key stays in relay'})
    try:
        jobs=[(agent,case,rep+1) for rep in range(a.repetitions) for case in cases for agent in a.agents]
        with concurrent.futures.ThreadPoolExecutor(max_workers=a.concurrency) as pool:
            statuses=list(pool.map(lambda job:run_agent(*job,relay,a.output),jobs))
    finally:relay.server.shutdown()
    print(json.dumps({'statuses':statuses}),flush=True)
    return int(any(s!='review_required' for s in statuses))
if __name__=='__main__':raise SystemExit(main())
