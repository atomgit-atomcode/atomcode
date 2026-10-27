"""Paired fixed-input high/max effort probes; user/product settings are untouched."""
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
    a=parser.parse_args();a.output.mkdir(parents=True,exist_ok=False)
    frozen=json.loads((e.ROOT/'results/fixed-history-reproduction-20261026/inputs.json').read_text())
    persona,skill=frozen['persona'],frozen['skill']
    cases=[f.scope_fixtures()[1],f.fixtures()[1]]
    inputs={case['id']:f.messages_for(persona,skill,case) for case in cases}
    e.save(a.output/'inputs.json',{'cases':cases,'messages':inputs})
    endpoint,key,cfg=e.configured_provider(Path.home()/'.atomcode/config.toml','taotoken/deepseek-v4.1-flash-1017')
    clients={effort:e.Client(endpoint,key,cfg['model'],effort,32768,120) for effort in ['high','max']}
    jobs=[(case['id'],rep,effort) for case in cases for rep in [1,2] for effort in clients]
    random.Random(20261028).shuffle(jobs)
    e.save(a.output/'manifest.json',{'kind':'direct fixed-history effort comparison; no native runtime/compaction or prompt edits','model':cfg['model'],'efforts':['high','max'],'output_cap':32768,'socket_timeout_seconds':120,'repetitions':2,'input_hashes':{name:hashlib.sha256(json.dumps(ms,ensure_ascii=False,sort_keys=True).encode()).hexdigest() for name,ms in inputs.items()},'limits':'requested effort only; gateway does not attest backend compute or weights; two related synthetic fixtures, small sample, no blind independent quality judge'})
    e.save(a.output/'jobs.json',[{'case':case,'rep':rep,'effort':effort} for case,rep,effort in jobs])
    def run(job):
        case,rep,effort=job;started=time.monotonic();result={'case':case,'rep':rep,'effort':effort}
        try:
            answer,meta=clients[effort].complete(inputs[case])
            result.update({'text':answer['content'],'metadata':meta,'status':'external_review_required'})
        except Exception as exc:result.update({'status':'execution_failure','error':str(exc)})
        result['seconds']=time.monotonic()-started
        e.save(a.output/f'{case}-{rep}-{effort}.json',result)
        print(case,rep,effort,result['status'],round(result['seconds'],1),flush=True)
    with concurrent.futures.ThreadPoolExecutor(max_workers=3) as pool:list(pool.map(run,jobs))


if __name__=='__main__':main()
