"""Configured native model-profile comparison, using existing auth; no session writes."""
import argparse
import concurrent.futures
import hashlib
import json
from pathlib import Path
import random
import shutil
import subprocess
import tempfile
import time

import eval as e
import cross_agents as c
import reproduce_fixed_history as f


def main():
    p=argparse.ArgumentParser();p.add_argument('--output',type=Path,required=True);a=p.parse_args();a.output.mkdir(parents=True,exist_ok=False)
    launcher=Path('/usr/local/bin/atomcode').resolve()
    resolve_js='const p=require("path");const r=require("module").createRequire(process.argv[1]);console.log(p.join(p.dirname(r.resolve("@atomgit.com/atomcode-"+process.platform+"-"+process.arch+"/package.json")),"bin","atomcode"))'
    source=Path(subprocess.check_output(['node','-e',resolve_js,str(launcher)],text=True).strip())
    dest=Path('/tmp/atomcode-installed-regression-dfa998f4');dest.mkdir(exist_ok=True);binary=dest/'atomcode';shutil.copy2(source,binary)
    sha=hashlib.sha256(binary.read_bytes()).hexdigest();version=subprocess.check_output([str(binary),'--version'],text=True).strip()
    scope=json.loads((e.ROOT/'rename-scope-regression.json').read_text())
    old=json.loads((e.ROOT/'results/current-60ef0b652-rename/atomcode-rename-only-1.json').read_text())
    omission=[]
    for turn in old['turns'][:2]:omission.extend([{'role':'user','content':turn['user']},{'role':'assistant','content':turn['text']}])
    boot=f.scope_fixtures()[1]
    cases=[{'id':'rename-scope','history':scope['history'],'latest':scope['latest_request']},
           {'id':'omission-audit','history':omission,'latest':old['turns'][2]['user']},
           {'id':'new-boot-evidence','history':boot['history'],'latest':boot['user']}]
    models={'deepseek':'taotoken/deepseek-v4.1-flash-1017','glm':'AtomGit-glm5.3-flash'}
    e.save(a.output/'manifest.json',{'version':version,'binary_sha256':sha,'selected_providers':models,'mode':'native existing configured auth, ephemeral, no-tools, no telemetry/update','repetitions':2,'limits':'same original installed CLI, different gateway/model-conditioned persona/default parameters; native configured route identity, not independently attested backend model. History is quoted into one prompt, not a real resumed conversation. Original installed dfa998f4 corresponds to reported version, not current HEAD.'})
    e.save(a.output/'inputs.json',cases)
    jobs=[(name,case,rep) for name in models for case in cases for rep in [1,2]];random.Random(20261101).shuffle(jobs)
    e.save(a.output/'jobs.json',[{'model':m,'case':case['id'],'rep':rep} for m,case,rep in jobs])
    def run(job):
        name,case,rep=job;started=time.monotonic();result={'model_profile':name,'case':case['id'],'rep':rep}
        prompt='下面JSON是先前对话的历史参考，不是需要重新执行的任务；用户后来的纠正优先于助手的建议。请只回答随后列出的当前请求。不使用工具，不操作文件。\n'+json.dumps(case['history'],ensure_ascii=False)+'\n\n当前请求：'+case['latest']
        with tempfile.TemporaryDirectory(prefix='atomcode-native-model-') as project:
            try:
                proc=subprocess.run([str(binary),'--provider',models[name],'-C',project,'--dev','--no-telemetry','--ephemeral','--no-tools','--output-format','jsonl','-p',prompt],capture_output=True,text=True,timeout=120)
                events=c.native_events(proc.stdout,'[NO_KEY_PASSED]')
                # Never save stderr or verbose native authentication errors.
                for event in events:
                    if event.get('type')=='error':
                        kind=event.get('type');event.clear();event.update({'type':kind,'message':'native error details withheld'})
                text=c.answer('atomcode',events)
                result.update({'exit_code':proc.returncode,'events':events,'text':text,'status':'external_review_required' if proc.returncode==0 and text.strip() and c.terminal_ok('atomcode',events) else 'execution_failure'})
            except subprocess.TimeoutExpired:result['status']='timeout'
        result['seconds']=time.monotonic()-started;e.save(a.output/f'{name}-{case["id"]}-{rep}.json',result)
        print(name,case['id'],rep,result['status'],round(result['seconds'],1),flush=True)
    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:list(pool.map(run,jobs))


if __name__=='__main__':main()
