"""Isolated fixed-binary native no-tools stream timing probe; unchanged real config."""
import argparse,hashlib,json,os,subprocess,tempfile,time
from pathlib import Path
import eval as e
import cross_agents as c
from native_dialogue_controls import NATURAL_TURNS
p=argparse.ArgumentParser();p.add_argument('--binary',type=Path,required=True);p.add_argument('--output',type=Path,required=True);p.add_argument('--effort',choices=['omitted','max'],required=True);a=p.parse_args();a.output.mkdir(parents=True,exist_ok=False)
endpoint,key,cfg=e.configured_provider(Path.home()/'.atomcode/config.toml','taotoken/deepseek-v4.1-flash-1017')
relay=c.Relay(endpoint,key,cfg['model'],a.output,32768);relay.preserve_caps=True;relay.omit_effort=a.effort=='omitted'
e.save(a.output/'manifest.json',{'version':subprocess.check_output([str(a.binary),'--version'],text=True).strip(),'binary_sha256':hashlib.sha256(a.binary.read_bytes()).hexdigest(),'effort':a.effort,'model':cfg['model'],'prompt':NATURAL_TURNS[0],'deadline_seconds':120,'limitations':'single fresh conversation per requested effort; native streaming, isolated home, no tools; no independent judge, backend compute/weights not attested; no global config/default change'})
with tempfile.TemporaryDirectory(prefix='atomcode-latency-') as temp:
    root=Path(temp);home=root/'home';home.mkdir();project=root/'project';project.mkdir()
    (home/'config.toml').write_text('default_provider="eval"\n[providers.eval]\ntype="openai"\nmodel='+json.dumps(cfg['model'])+'\napi_key='+json.dumps(relay.token)+'\nbase_url='+json.dumps(relay.url(a.effort))+'\ncontext_window=512000\nmax_tokens=32768\n'+('reasoning_effort="max"\n' if a.effort=='max' else '')+'retry_max_attempts=1\n[coding]\nmax_rounds=2\n[telemetry]\nenabled=false\n')
    env={**os.environ,'ATOMCODE_HOME':str(home),'NO_PROXY':'127.0.0.1,localhost','no_proxy':'127.0.0.1,localhost'}
    cmd=[str(a.binary),'-C',str(project),'--dev','--no-telemetry','--no-tools','--output-format','jsonl','-p',NATURAL_TURNS[0]]
    started=time.monotonic();result={'status':'running'}
    try:
        proc=subprocess.run(cmd,env=env,capture_output=True,text=True,timeout=120);events=c.native_events(proc.stdout,relay.token)
        result.update(status='review_required' if proc.returncode==0 and c.terminal_ok('atomcode',events) else 'execution_failure',exit_code=proc.returncode,events=events,text=c.answer('atomcode',events))
    except subprocess.TimeoutExpired as error:
        raw=error.stdout or '';raw=raw.decode(errors='replace') if isinstance(raw,bytes) else raw
        events=c.native_events(raw,relay.token);result.update(status='timeout',events=events,partial_text=c.answer('atomcode',events))
    finally:
        result['seconds']=time.monotonic()-started
        # The child can finish after a client-visible terminal while side calls close.
        until=time.monotonic()+5
        while time.monotonic()<until and not relay.records:time.sleep(.05)
        with relay.lock:timings=[dict(x) for x in relay.active_records]
        e.save(a.output/'timing-at-exit.json',timings)
        result['wire_tools_empty']=bool(timings) and all(not x['tool_names'] for x in timings)
        e.save(a.output/'result.json',result);relay.server.shutdown()
print(a.effort,result['status'],round(result['seconds'],1),flush=True)
