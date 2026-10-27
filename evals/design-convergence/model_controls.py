"""Fixed-input model/provider comparison; no production configuration changes."""
import argparse
import concurrent.futures
import hashlib
import json
from pathlib import Path
import random

import eval as e
import reproduce_fixed_history as fixed


def main():
    p=argparse.ArgumentParser()
    p.add_argument('--output',type=Path,required=True)
    a=p.parse_args();a.output.mkdir(parents=True,exist_ok=False)
    frozen=json.loads((e.ROOT/'results/fixed-history-reproduction-20261026/inputs.json').read_text())
    persona,skill=frozen['persona'],frozen['skill']
    cases=[fixed.scope_fixtures()[1],fixed.scope_fixtures()[2],fixed.fixtures()[1]]
    selections=['taotoken/deepseek-v4.1-flash-1017','amd/MiMo-V2.6-Flash']
    clients={};identities={}
    for selection in selections:
        endpoint,key,cfg=e.configured_provider(Path.home()/'.atomcode/config.toml',selection)
        clients[selection]=e.Client(endpoint,key,cfg['model'],'max',32768,300)
        identities[selection]=cfg['model']
    e.save(a.output/'manifest.json',{'kind':'fixed-input direct model/provider comparison; no native runtime, compaction or auto skill selection',
        'requested_models':identities,'requested_effort':'max','cap':32768,'repetitions':2,
        'limitations':'Different model families AND configured gateways; cannot isolate model from provider. Same DeepSeek-rendered frozen persona for both; not each model native product prompt. Requested effort is not proof of matched compute.',
        'persona_sha256':hashlib.sha256(persona.encode()).hexdigest(),'skill_sha256':hashlib.sha256(skill.encode()).hexdigest()})
    e.save(a.output/'inputs.json',{'persona':persona,'skill':skill,'fixtures':cases})
    jobs=[(selection,case,rep) for selection in selections for case in cases for rep in [1,2]]
    random.Random(20261026).shuffle(jobs)
    e.save(a.output/'jobs.json',[{'selection':s,'case':c['id'],'rep':r} for s,c,r in jobs])
    def run(job):
        selection,case,rep=job
        result={'selection':selection,'case':case['id'],'rep':rep}
        try:
            answer,meta=clients[selection].complete(fixed.messages_for(persona,skill,case))
            result.update({'text':answer['content'],'metadata':meta,'status':'review_required'})
        except Exception as exc:result.update({'status':'execution_failure','error':str(exc)})
        slug='deepseek' if selection.startswith('taotoken') else 'mimo'
        e.save(a.output/f'{slug}-{case["id"]}-{rep}.json',result)
        print(slug,case['id'],rep,result['status'],flush=True)
    with concurrent.futures.ThreadPoolExecutor(max_workers=3) as pool:list(pool.map(run,jobs))


if __name__=='__main__':main()
