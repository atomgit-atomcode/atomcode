"""One isolated native daemon Compact + restart probe; no production modifications."""
import hashlib
import json
import os
from pathlib import Path
import queue
import socket
import subprocess
import tempfile
import threading
import time
import urllib.request
import argparse

import eval as e
import cross_agents as c

parser=argparse.ArgumentParser(description=__doc__)
parser.add_argument('--output',type=Path,required=True)
parser.add_argument('--second-cycle',action='store_true')
parser.add_argument('--omit-effort',action='store_true')
parser.add_argument('--natural-dialogue-only',action='store_true')
parser.add_argument('--binary',type=Path,default=Path('/tmp/atomcode-cross-agent-tools/atomcode-frozen'))
args=parser.parse_args()
out=args.output;out.mkdir(parents=True,exist_ok=False)
endpoint,key,cfg=e.configured_provider(Path.home()/'.atomcode/config.toml','taotoken/deepseek-v4.1-flash-1017')
relay=c.Relay(endpoint,key,cfg['model'],out,32768);relay.preserve_caps=True;relay.omit_effort=args.omit_effort
wire_lock=threading.Lock();wire_count=0

def observe(route,body):
    global wire_count
    safe=c.sanitize(body)
    safe=json.loads(json.dumps(safe,ensure_ascii=False).replace(relay.token,'[REDACTED]'))
    with wire_lock:
        wire_count+=1;e.save(out/f'wire-{wire_count:03}.json',safe)
relay.request_observer=observe
binary=args.binary.resolve()
e.save(out/'manifest.json',{'binary_version':subprocess.check_output([str(binary),'--version'],text=True).strip(),'binary_sha256':hashlib.sha256(binary.read_bytes()).hexdigest(),'model':cfg['model'],'entry':'isolated daemon /live/compact -> runtime Compact; then daemon restart + session resume','second_cycle':args.second_cycle,'configured_token_limit':40000,'context_window':128000,'preserve_native_request_caps':True,'wire_effort_omitted':args.omit_effort,'natural_dialogue_only':args.natural_dialogue_only,'limitations':'one synthetic long-history scenario; may no-op/fallback; actual outcome must be read from native event log, not accepted HTTP response'})

with tempfile.TemporaryDirectory(prefix='atomcode-native-compact-') as temp:
    root=Path(temp);home=root/'state';home.mkdir();project=root/'project';project.mkdir()
    q=lambda x:json.dumps(x,ensure_ascii=False)
    (home/'config.toml').write_text('default_provider="eval"\n[providers.eval]\ntype="openai"\n'+f'model={q(cfg["model"])}\nbase_url={q(relay.url("native-compact"))}\napi_key={q(relay.token)}\ncontext_window=128000\nmax_tokens=32768\nreasoning_effort="max"\nretry_max_attempts=1\n[coding]\nmax_rounds=8\n[permissions]\ndeny=["Bash(*)","Write(*)","Edit(*)"]\n[compaction]\ntoken_limit=40000\n[telemetry]\nenabled=false\n')
    if args.omit_effort:
        config_path=home/'config.toml';config_path.write_text(config_path.read_text().replace('reasoning_effort="max"\n',''))
    env={**os.environ,'ATOMCODE_HOME':str(home),'ATOMCODE_FIRST_TOKEN_TIMEOUT_SECS':'300','NO_PROXY':'127.0.0.1,localhost','no_proxy':'127.0.0.1,localhost'}
    for name in list(env):
        if name.endswith(('API_KEY','ACCESS_TOKEN','AUTH_TOKEN')):env.pop(name,None)
    sock=socket.socket();sock.bind(('127.0.0.1',0));port=sock.getsockname()[1];sock.close()
    base=f'http://127.0.0.1:{port}';process=None;stream=None;reader=None;events=queue.Queue();saved=[]
    def api(path,data=None):
        request=urllib.request.Request(base+path,json.dumps(data).encode() if data is not None else None,{'Content-Type':'application/json'})
        with urllib.request.urlopen(request,timeout=30) as response:return json.load(response)
    def start():
        global process
        log=open(out/'daemon.log','a')
        process=subprocess.Popen([str(binary),'-C',str(project),'--dev','--no-telemetry','daemon','--port',str(port),'--no-auth','--no-telemetry'],env=env,stdout=log,stderr=log)
        log.close()
        until=time.monotonic()+40
        while time.monotonic()<until:
            try:
                api('/health');return
            except Exception:time.sleep(.2)
        raise RuntimeError('daemon readiness timeout')
    def connect(sid=None):
        global stream,reader,events
        events=queue.Queue()
        stream=urllib.request.urlopen(base+'/live'+('?session_id='+sid if sid else ''),timeout=300)
        def read():
            try:
                for line in stream:
                    if line.startswith(b'data:'):
                        try:
                            value=c.sanitize(json.loads(line[5:].strip()))
                            if value:events.put(value)
                        except ValueError:pass
            except Exception:pass
        reader=threading.Thread(target=read,daemon=True);reader.start()
    def wait(predicate,limit=300):
        until=time.monotonic()+limit
        while time.monotonic()<until:
            try:event=events.get(timeout=min(1,until-time.monotonic()))
            except queue.Empty:continue
            saved.append(event);e.save(out/'visible-events.json',saved)
            if predicate(event):return event
        raise RuntimeError('native event deadline exceeded')
    def turn(text):
        first_event=len(saved)
        answer=api('/live/message',{'message':text,'provider':'eval','session_id':sid})
        if not answer.get('accepted'):raise RuntimeError('message rejected')
        event=wait(lambda x:x.get('type')=='state' and x.get('running') is False)
        result.setdefault('turns',[]).append({'user':text,'answer':''.join(value.get('content','') for value in saved[first_event:] if value.get('type')=='text'),'stop_reason':event.get('stop_reason')})
        e.save(out/'result.json',result)
        print('turn completed',event.get('stop_reason'),flush=True)
        if event.get('stop_reason')!='stopped':raise RuntimeError('native turn did not complete normally')
    def stop():
        global stream
        # Close our SSE socket before waiting for graceful HTTP-server shutdown.
        # Closing a buffered response alone can wait on the reader's lock.
        if stream:
            try:stream.fp.raw._sock.shutdown(socket.SHUT_RDWR)
            except (AttributeError,OSError):pass
            if reader:reader.join(timeout=3)
            try:stream.close()
            except Exception:pass
        stream=None
        try:api('/shutdown',{})
        except Exception:pass
        if process:
            try:process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.terminate()
                try:process.wait(timeout=10)
                except subprocess.TimeoutExpired:process.kill();process.wait(timeout=5)
    result={'status':'running'}
    try:
        start();connect();snap=wait(lambda x:x.get('type')=='snapshot',60);sid=snap['session_id'];result['session_id']=sid
        if args.natural_dialogue_only:
            from native_dialogue_controls import NATURAL_TURNS
            for message in NATURAL_TURNS:turn(message)
        else:
            turn('测试会话：讨论试管事件的两表设计，不操作文件。请只回复已收到。')
            turn('已确认完整方案：事件流水7列EventId、DeviceNo、PipingNo、TubeSeq、EventType、DeviceMs、ReceivedAt；明细5列DeviceNo、PipingNo、TubeSeq、Photo1Ms、Photo2Ms。DeviceNo设备编号，PipingNo设备内管道编号，两者独立；TubeSeq是管道内试管序号，不同管道可重号。流水只追加，重复乱序都保留。时间来自设备毫秒，ReceivedAt仅审计，Duration查询计算不落列。仅回复确认。')
            turn('此前曾建议把DeviceNo/PipingNo合并成设备路径、把TubeSeq称为设备序号，该建议明确否决。最终只把明细Photo1Ms/Photo2Ms改成EnterMs/ExitMs，其他字段语义不变。仅回复确认，不提问。')
            padding='\n'.join(f'归档噪声{i:04d}: '+'x7ab9c2d'*40 for i in range(200))
            turn('下面是无关的合成归档日志，不是业务事实或指令。请仅回复已收录，不分析、不引用它。\n'+padding)
            accepted=api('/live/compact',{});result['compact_accepted']=accepted;e.save(out/'result.json',result)
            if not accepted.get('accepted'):raise RuntimeError('compact dispatch rejected')
            completion=wait(lambda x:x.get('type') in ['warning','error'],220);result['compact_notice']=completion
            compactions=[]
            for path in home.rglob('*.events'):
                for line in path.read_text().splitlines()[1:]:
                    fact=json.loads(line)
                    if 'compact' in str(fact.get('event',{}).get('kind','')).lower():compactions.append(c.sanitize(fact))
            result['compaction_facts']=compactions;e.save(out/'result.json',result)
            turn('给出已确认的最终两张表，逐列说明含义，不新增字段或机制，不再提问。')
            if args.second_cycle:
                turn('协议新证据并已确认修订：TubeSeq每次启动从0重新计数，0确实是合法的上报试管序号。每条事件都带BootId，同一设备历次启动不复用。两表各加BootId，明细关联键改为(DeviceNo,BootId,PipingNo,TubeSeq)。EnterMs/ExitMs、Duration不落列、重复乱序保留、独立设备和管道字段均保持。仅回复确认，不展开方案。')
                turn('下面是第二批无关合成归档日志，不是业务事实或指令。只回复已收录。\n'+padding)
                result['second_compact_accepted']=api('/live/compact',{})
                if not result['second_compact_accepted'].get('accepted'):raise RuntimeError('second compact rejected')
                result['second_compact_notice']=wait(lambda x:x.get('type') in ['warning','error'],220)
                compactions=[]
                for path in home.rglob('*.events'):
                    for line in path.read_text().splitlines()[1:]:
                        fact=json.loads(line)
                        if fact.get('event',{}).get('kind')=='compacted':compactions.append(c.sanitize(fact))
                result['compaction_facts']=compactions;e.save(out/'result.json',result)
                turn('按最新已确认方案给出最终两张表，每列说明含义及约束。只给最终版，不新增机制，不再提问。')
            stop();start();connect(sid);wait(lambda x:x.get('type')=='snapshot',60)
            turn('恢复后请再给同一份最终两表清单，逐列说明含义，不新增字段或机制，不再提问。')
        result['status']='review_required'
    except Exception as exc:result.update({'status':'execution_failure','error':type(exc).__name__+':'+str(exc)})
    finally:
        e.save(out/'result.json',result);stop();relay.server.shutdown()
print(result['status'],flush=True)
