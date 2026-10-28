"""User-workflow comparison: open omission audit vs revision anchored to prior text."""
import argparse
import concurrent.futures
import copy
import json
from pathlib import Path
import random
import time

import eval as e
import reproduce_fixed_history as f


def main():
    p=argparse.ArgumentParser();p.add_argument('--output',type=Path,required=True)
    a=p.parse_args();a.output.mkdir(parents=True,exist_ok=False)
    prior=json.loads((e.ROOT/'results/reference-tail-controls-20261027/inputs.json').read_text())
    inputs={name:{'baseline':data['baseline'],'candidate':copy.deepcopy(data['baseline'])} for name,data in prior.items()}
    old='再检查改名有没有遗漏，仍然只改这两个名字。'
    replacement='以上一版实际列出的字段、含义和规则为基准，核对这次改名造成的差异。只列本次确实遗漏的名称替换；如果没有就直接说没有。不要重新构造原方案，也不要把未提供的索引、接口或实现当作已存在。'
    target=next(m for m in inputs['rename-omission-audit']['candidate'] if m.get('role')=='user' and m.get('content')==old)
    target['content']=replacement
    inputs['new-evidence-control']['candidate'][-1]['content']+='请先以上一版为基准列必要变更，再给最终两表；有新证据就做必要修订，未受影响的内容不要重新设计。'
    # A new-domain scope control, with only one latest user wording change.
    frozen=json.loads((e.ROOT/'results/fixed-history-reproduction-20261026/inputs.json').read_text())
    payment=json.loads((e.ROOT/'payment-edit-fixture.json').read_text())
    case={'history':[{'role':'user','content':'以下是已确认的完整方案，只讨论不操作文件。\n'+payment['document']},{'role':'assistant','content':'确认，后续按此版本做局部修订。'}],
          'user':'仅把ChargedCents改名为PaidCents，其他保持。给出最终两表字段及含义，不提问，不操作文件。'}
    ms=f.messages_for(frozen['persona'],frozen['skill'],case)
    inputs['payment-rename-control']={'baseline':ms,'candidate':copy.deepcopy(ms)}
    inputs['payment-rename-control']['candidate'][-1]['content']+='请基于上一版先列出本次差异，再给最终字段表，未变字段沿用原文。'
    e.save(a.output/'inputs.json',inputs)
    e.save(a.output/'manifest.json',{'kind':'direct fixed-history user-workflow comparison; latest user wording/output contract changes; not a hidden system fix','model':'deepseek-v4.1-flash-1017','effort':'max','cap':32768,'repetitions':2,'limits':'two previously inspected cases and a related payment case, no independent blind judge; different personas between cases, identical within each pair; no production/native integration'})
    endpoint,key,cfg=e.configured_provider(Path.home()/'.atomcode/config.toml','taotoken/deepseek-v4.1-flash-1017')
    client=e.Client(endpoint,key,cfg['model'],'max',32768,90)
    jobs=[(case,arm,rep) for case in inputs for arm in ['baseline','candidate'] for rep in [1,2]]
    random.Random(20261031).shuffle(jobs);e.save(a.output/'jobs.json',[{'case':c,'arm':a,'rep':r} for c,a,r in jobs])
    def run(job):
        case,arm,rep=job;start=time.monotonic();d={'case':case,'arm':arm,'rep':rep}
        try:
            answer,meta=client.complete(inputs[case][arm]);d.update({'text':answer['content'],'metadata':meta,'status':'external_review_required'})
        except Exception as exc:d.update({'status':'execution_failure','error':str(exc)})
        d['seconds']=time.monotonic()-start;e.save(a.output/f'{case}-{arm}-{rep}.json',d);print(case,arm,rep,d['status'],round(d['seconds'],1),flush=True)
    with concurrent.futures.ThreadPoolExecutor(max_workers=3) as pool:list(pool.map(run,jobs))


if __name__=='__main__':main()
