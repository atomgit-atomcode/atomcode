"""Single-generation scope rule A/B with legitimate-addition and new-domain controls."""
import argparse
import concurrent.futures
import hashlib
import json
from pathlib import Path
import random
import time

import eval as e
import reproduce_fixed_history as f


def main():
    parser=argparse.ArgumentParser();parser.add_argument('--output',type=Path,required=True)
    args=parser.parse_args();args.output.mkdir(parents=True,exist_ok=False)
    frozen=json.loads((e.ROOT/'results/fixed-history-reproduction-20261026/inputs.json').read_text())
    persona,skill=frozen['persona'],frozen['skill']
    payment=json.loads((e.ROOT/'payment-edit-fixture.json').read_text())
    cases=[f.fixtures()[1],f.scope_fixtures()[1],{
        'id':'payment-local-rename','history':[
            {'role':'user','content':'下面是完整已确认方案，后续按这版修订；只讨论，不操作文件。\n'+payment['document']},
            {'role':'assistant','content':'确认，后续按这版字段和语义做局部修改。'}],
        'user':'仅将ChargedCents改名PaidCents，字段含义和其他内容保持。给最终两张表的字段与含义，不展开选型，不提问，不操作文件。'}]
    addition=(e.ROOT/'closed_inventory_candidate.txt').read_text().strip()
    prompts={'baseline':persona,'candidate':persona+'\n\n'+addition}
    inputs={case['id']:{arm:f.messages_for(prompt,skill,case) for arm,prompt in prompts.items()} for case in cases}
    e.save(args.output/'inputs.json',{'cases':cases,'messages':inputs,'candidate_addition':addition})
    e.save(args.output/'manifest.json',{'kind':'single-generation direct fixed-history A/B; no native integration','model':'deepseek-v4.1-flash-1017','effort':'max','cap':32768,'repetitions':2,'input_hashes':{case:{arm:hashlib.sha256(json.dumps(ms,ensure_ascii=False,sort_keys=True).encode()).hexdigest() for arm,ms in arms.items()} for case,arms in inputs.items()},'limits':'small related synthetic cases, known observed failure, no independent blind quality judge; loaded skill replay identical'})
    endpoint,key,cfg=e.configured_provider(Path.home()/'.atomcode/config.toml','taotoken/deepseek-v4.1-flash-1017')
    client=e.Client(endpoint,key,cfg['model'],'max',32768,120)
    jobs=[(case['id'],arm,rep) for case in cases for arm in prompts for rep in [1,2]]
    random.Random(20261029).shuffle(jobs)
    e.save(args.output/'jobs.json',[{'case':case,'arm':arm,'rep':rep} for case,arm,rep in jobs])
    def run(job):
        case,arm,rep=job;started=time.monotonic();result={'case':case,'arm':arm,'rep':rep}
        try:
            answer,meta=client.complete(inputs[case][arm]);result.update({'text':answer['content'],'metadata':meta,'status':'external_review_required'})
        except Exception as exc:result.update({'status':'execution_failure','error':str(exc)})
        result['seconds']=time.monotonic()-started;e.save(args.output/f'{case}-{arm}-{rep}.json',result)
        print(case,arm,rep,result['status'],round(result['seconds'],1),flush=True)
    with concurrent.futures.ThreadPoolExecutor(max_workers=3) as pool:list(pool.map(run,jobs))


if __name__=='__main__':main()
