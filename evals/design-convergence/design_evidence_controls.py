"""Fixed-input evidence/simplicity probe; no production prompt or user configuration edits."""
import argparse, concurrent.futures, copy, hashlib, json, random, time
from pathlib import Path
import eval as e
import native_dialogue_controls as dialogue
import reproduce_fixed_history as f


def fixtures():
    capture=e.ROOT/'results/feedback-current-natural-isolated-20261027'
    wanted=dialogue.NATURAL_TURNS[0]
    for path in sorted(capture.glob('wire-*.json')):
        wire=json.loads(path.read_text())
        if any(m.get('role')=='user' and wanted in str(m.get('content','')) for m in wire.get('messages',[])):
            persona=next(m['content'] for m in wire['messages'] if m.get('role')=='system')
            break
    else:raise ValueError('missing native initial-design system capture')
    scope='已确认：同一设备同次启动内，DeviceMs单调且两次光电的设备毫秒可以相减；只有跨重启是否可比未知。两张简单表，一张逐帧追加流水保留重复，一张试管明细。DeviceNo是设备，PipingNo是设备内管道，TubeSeq是管道内试管序号，不同管道可重号。给最小可用方案，明确跨重启无法判定的部分，不加入设备尚未提供的启动号或配对窗口，也不替换成服务端接收耗时。'
    control=f.scope_fixtures()[1]
    return {
        'natural-start':{'messages':[{'role':'system','content':persona},{'role':'user','content':wanted}], 'review':'Unrequested windows/state/epoch inference/fallback clocks remain optional, not mandatory. Unconfirmed cross-restart comparability must not invalidate all device timing. Identifiers separate and pipe-local.'},
        'local-uncertainty':{'messages':[{'role':'system','content':persona},{'role':'user','content':scope}], 'review':'Use same-boot device delta; only cross-restart unknown. No boot identifier/window/server elapsed invention; preserve independent identities and duplicates.'},
        'new-evidence':{'messages':f.messages_for(persona,'',control), 'review':'New non-reused BootId and reset evidence requires adding BootId to both tables and the association key; preserve pipe-local identity and 8/6 approved column inventories.'},
    }


def main():
    p=argparse.ArgumentParser();p.add_argument('--output',type=Path,required=True);a=p.parse_args();a.output.mkdir(parents=True,exist_ok=False)
    candidate=(e.ROOT/'design_evidence_candidate.txt').read_text()
    cases=fixtures();inputs={}
    for name,case in cases.items():
        base=copy.deepcopy(case['messages']);modified=copy.deepcopy(base);modified[0]['content']+='\n\n'+candidate
        inputs[name]={'baseline':base,'candidate':modified,'review':case['review']}
    e.save(a.output/'inputs.json',inputs)
    endpoint,key,cfg=e.configured_provider(Path.home()/'.atomcode/config.toml','taotoken/deepseek-v4.1-flash-1017')
    client=e.Client(endpoint,key,cfg['model'],None,16384,90)
    e.save(a.output/'manifest.json',{'model':cfg['model'],'effort':'omitted','max_tokens':16384,'timeout_seconds':90,'repetitions':2,'candidate_sha256':hashlib.sha256(candidate.encode()).hexdigest(),'limits':'same native captured persona, synthesized fixed histories, no tool declarations/execution, reasoning not replayed, not native A/B or original user session; non-blind assistant review; no repeated sampling to select passing answers'})
    jobs=[(case,arm,rep) for case in inputs for arm in ['baseline','candidate'] for rep in [1,2]];random.Random(20261027).shuffle(jobs)
    e.save(a.output/'jobs.json',jobs)
    def run(job):
        case,arm,rep=job;start=time.monotonic();result={'case':case,'arm':arm,'rep':rep}
        try:
            answer,meta=client.complete(inputs[case][arm]);result.update(text=answer['content'],metadata=meta,status='review_required')
        except Exception as error:result.update(status='execution_failure',error=str(error))
        result['seconds']=time.monotonic()-start;e.save(a.output/f'{case}-{arm}-{rep}.json',result);print(case,arm,rep,result['status'],round(result['seconds'],1),flush=True)
    with concurrent.futures.ThreadPoolExecutor(max_workers=3) as pool:list(pool.map(run,jobs))

if __name__=='__main__':main()
