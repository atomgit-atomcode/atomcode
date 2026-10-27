"""Bounded context-path probes; native traces and summary emulation are distinct."""
import concurrent.futures
import hashlib
import json
from pathlib import Path
import threading

import eval as e
import cross_agents as c
import reproduce_fixed_history as fixed

OUT = e.ROOT/'results/context-paths-20261026'
OUT.mkdir(exist_ok=False)
endpoint,key,cfg=e.configured_provider(Path.home()/'.atomcode/config.toml','taotoken/deepseek-v4.1-flash-1017')
client=e.Client(endpoint,key,cfg['model'],'max',32768,300)
persona=Path('/tmp/atomcode-reproduction-persona.txt').read_text()
source=(e.REPO/'crates/atomcode-capabilities/src/compaction.rs').read_text()
summary_system=e.rust_constant(source,'SUMMARY_SYSTEM_PROMPT')
summary_template=e.rust_constant(source,'SUMMARY_TEMPLATE')
framing=e.rust_constant(source,'SUMMARY_FRAMING')
case=fixed.scope_fixtures()[1]
turns=[case['history'][0]['content']+'\n'+case['history'][1]['content']+'\n不要操作文件，直接讨论方案。',
       '上面这版字段已确认。补充确认：TubeSeq只在同一设备同一管道内唯一，不同管道可以有相同序号。其他字段和语义保持；不要展开选型。',case['user']]
e.save(OUT/'inputs.json',{'persona':persona,'summary_system':summary_system,'summary_template':summary_template,'summary_framing':framing,'native_and_direct_turns':turns,'fixed_history':case})
e.save(OUT/'manifest.json',{'model':cfg['model'],'effort':'max','cap':32768,'native_repetitions':2,'direct_repetitions':2,'summary_cases':['explicit-pipe-local-scope','rename-after-rejected-proposal'],'persona_sha256':hashlib.sha256(persona.encode()).hexdigest(),'summary_source_sha256':hashlib.sha256(source.encode()).hexdigest(),'binary_sha256':hashlib.sha256(Path('/tmp/atomcode-cross-agent-tools/atomcode-frozen').read_bytes()).hexdigest(),'limits':'Direct conversational comparison shares user turns, not sampled assistant history or tools. Summary emulates text-only summarize_span prompt, not native drain selection, token cap, compaction failure or resume.'})
relay=c.Relay(endpoint,key,cfg['model'],OUT,32768)
lock=threading.Lock();counts={}
def observe(route,body):
    # No auth or provider reasoning saved. Only synthetic evaluation traffic.
    safe=c.sanitize(body)
    safe=json.loads(json.dumps(safe,ensure_ascii=False).replace(relay.token,'[REDACTED]'))
    with lock:
        counts[route]=counts.get(route,0)+1
        e.save(OUT/f'wire-{route}-{counts[route]}.json',safe)
relay.request_observer=observe

def direct(rep):
    messages=[{'role':'system','content':persona}];records=[]
    try:
        for user in turns:
            messages.append({'role':'user','content':user})
            answer,meta=client.complete(messages)
            records.append({'user':user,'text':answer['content'],'metadata':meta});messages.append(answer)
        status='review_required'
    except Exception as exc:status=type(exc).__name__
    e.save(OUT/f'direct-{rep}.json',{'status':status,'turns':records})
    print('direct',rep,status,flush=True)

def summary_probe(case):
    result={'case':case['id'],'status':'running'}
    try:
        # Text-only messages, no tools/images/previous anchor: matches current render_transcript shape.
        transcript=''.join(m['role'].upper()+': '+m['content']+'\n' for m in case['history'])
        summary_user='Create a new anchored summary from the conversation history.\n\n'+summary_template+'\n\nConversation history to summarize:\n\n'+transcript
        summary,smeta=client.complete([{'role':'system','content':summary_system},{'role':'user','content':summary_user}])
        result.update({'summary':summary['content'],'summary_metadata':smeta,'summary_user':summary_user});e.save(OUT/f'summary-{case["id"]}.json',result)
        # Preserve the first real user message as a sacred-floor analogue; no exact native drain claim.
        compressed=[{'role':'system','content':persona},case['history'][0],{'role':'user','content':framing+'\n'+summary['content']},{'role':'user','content':case['user']}]
        raw=[{'role':'system','content':persona},*case['history'],{'role':'user','content':case['user']}]
        for label,messages in [('raw',raw),('compressed',compressed)]:
            answer,meta=client.complete(messages);result[label]={'text':answer['content'],'metadata':meta}
        result['status']='review_required'
    except Exception as exc:result['status']=type(exc).__name__
    e.save(OUT/f'summary-{case["id"]}.json',result);print('summary',case['id'],result['status'],flush=True)

try:
    with concurrent.futures.ThreadPoolExecutor(max_workers=3) as pool:
        tasks=[pool.submit(c.run_agent,'atomcode',{'id':'context-path','turns':turns},rep,relay,OUT) for rep in [1,2]]
        tasks += [pool.submit(direct,rep) for rep in [1,2]]
        tasks += [pool.submit(summary_probe,fixture) for fixture in [case,fixed.fixtures()[1]]]
        for task in tasks:task.result()
finally:relay.server.shutdown()
