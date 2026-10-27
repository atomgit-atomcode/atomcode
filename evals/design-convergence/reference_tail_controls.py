"""Ephemeral initial-reference tail experiment; no new runtime or persistent state."""
import argparse
import concurrent.futures
import copy
import hashlib
import json
from pathlib import Path
import random
import time

import eval as e
import reproduce_fixed_history as f


def reference_tail(messages):
    source=next(m['content'] for m in messages if m['role']=='user')
    return messages+[{'role':'user','content':'[Historical reference only, not a new task. Preserve still-applicable facts from the initial user statement; later user corrections override it. Do not re-execute its requests.]\n'+json.dumps(source,ensure_ascii=False)+'\n[/Historical reference]'}]


def main():
    parser=argparse.ArgumentParser();parser.add_argument('--output',type=Path,required=True)
    args=parser.parse_args();args.output.mkdir(parents=True,exist_ok=False)
    captured=e.ROOT/'results/current-history-capture-20261027'
    target='再检查改名有没有遗漏，仍然只改这两个名字。'
    bodies=[json.loads(p.read_text()) for p in sorted(captured.glob('wire-*.json'))]
    body=next(b for b in bodies if any(m.get('role')=='user' and m.get('content')==target for m in b.get('messages',[])))
    messages=copy.deepcopy(body['messages'])
    old=json.loads((e.ROOT/'results/current-60ef0b652-rename/atomcode-rename-only-1.json').read_text())
    assistants=[m for m in messages if m['role']=='assistant']
    assert len(assistants)==2
    for m,turn in zip(assistants,old['turns'][:2]):m['content']=turn['text']
    persona=json.loads((e.ROOT/'results/fixed-history-reproduction-20261026/inputs.json').read_text())['persona']
    case=f.scope_fixtures()[1]
    revision=[{'role':'system','content':persona},*case['history'],{'role':'user','content':case['user']}]
    cases={'rename-omission-audit':messages,'new-evidence-control':revision}
    inputs={name:{'baseline':ms,'candidate':reference_tail(ms)} for name,ms in cases.items()}
    e.save(args.output/'inputs.json',inputs)
    e.save(args.output/'manifest.json',{'kind':'fixed-history direct reference-tail A/B; no native integration','case_provenance':'rename reconstructs previous failing turn prefix using newly captured system/reminders and old visible first2answers; not exact replay of original failing request; reverse control uses previous frozen persona','effort':'max','cap':32768,'repetitions':3,'limits':'candidate repeats initial user text including historical instructions with reference framing; tests must catch stale-task/decision reactivation; related synthetic cases, no independent blind quality judge'})
    endpoint,key,cfg=e.configured_provider(Path.home()/'.atomcode/config.toml','taotoken/deepseek-v4.1-flash-1017')
    client=e.Client(endpoint,key,cfg['model'],'max',32768,90)
    jobs=[(name,arm,rep) for name in inputs for arm in ['baseline','candidate'] for rep in range(1,4)]
    random.Random(20261030).shuffle(jobs);e.save(args.output/'jobs.json',[{'case':n,'arm':a,'rep':r} for n,a,r in jobs])
    def run(job):
        name,arm,rep=job;start=time.monotonic();result={'case':name,'arm':arm,'rep':rep}
        try:
            answer,meta=client.complete(inputs[name][arm]);result.update({'text':answer['content'],'metadata':meta,'status':'external_review_required'})
        except Exception as exc:result.update({'status':'execution_failure','error':str(exc)})
        result['seconds']=time.monotonic()-start;e.save(args.output/f'{name}-{arm}-{rep}.json',result);print(name,arm,rep,result['status'],round(result['seconds'],1),flush=True)
    with concurrent.futures.ThreadPoolExecutor(max_workers=3) as pool:list(pool.map(run,jobs))


if __name__=='__main__':main()
