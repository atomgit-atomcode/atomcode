"""True multi-turn configured-profile runs; only owned test session buckets removed."""
import argparse
import concurrent.futures
import hashlib
import json
import os
from pathlib import Path
import secrets
import shutil
import subprocess
import tempfile
import time

import eval as e
import cross_agents as c

TURNS=[
 '只讨论，不使用工具、不操作文件，每轮不超过600字。已确认两张表：流水7列EventId、DeviceNo、PipingNo、TubeSeq、EventType、DeviceMs、ReceivedAt；明细5列DeviceNo、PipingNo、TubeSeq、T1、T2。DeviceNo是设备编号，PipingNo是设备内管道编号，两个独立字段；TubeSeq是管道内试管序号，不同管道可重号，不是设备序号。每次上报先追加流水，重复乱序也保留；明细按三标识归集。DeviceMs与T1/T2都是设备毫秒，ReceivedAt只审计，Duration查询计算、不落列。TubeSeq重启是否重复、DeviceMs跨启动是否归零均未确认，不得当事实。简要复述即可。',
 '本次只把T1改名Photo1Ms、T2改名Photo2Ms，含义与其他字段、规则都保持，不增加任何内容。',
 '再检查这次改名有没有遗漏，仍然只改这两个名字。',
 '协议新证据：TubeSeq每次启动从0计数，0是合法上报值。每条事件都带BootId，同一设备历次启动不复用。两表各加BootId，关联键改为(DeviceNo,BootId,PipingNo,TubeSeq)。其他字段和已确认规则保持，DeviceMs跨启动行为仍未确认。给出修订后的两表，不额外加字段。',
 '最后只将Photo1Ms改为EnterMs、Photo2Ms改为ExitMs。基于最新确认状态给最终两表，字段独立一行并说明含义及实际改动；其余保持，不再提问。',
]


NATURAL_TURNS=[
 '帮我设计两张简单的表，接收试管经过两个光电传感器的上报。一个设备有多条管道，每条管道内有试管序号，用同一序号把光电1和光电2关联为同一根试管。流水要能忠实查每次设备上报，另一张表便于查试管经过两个光电的情况。设备给的是毫秒计时，跨重启能否比较还没确认。不要实现代码，给能落地的简洁方案。',
 '这方案只保留两张表。流水每帧一行，只增不改，设备报什么存什么；不要先在内存配对、不要超时补半行、不要第三套结论数据。同一帧重复上报也先留在流水里。按这个收敛一下。',
 '再次确认：DeviceNo是设备编号，PipingNo是设备内管道编号，必须是两个独立字段，不能合成一个字段；TubeSeq是管道内试管序号，不是设备序号。不同管道的TubeSeq可以相同。这几个定义锁定，保留前面正确的部分，不重新展开。',
 '只把试管明细里的光电1时间改名EnterMs、光电2时间改名ExitMs，仍是设备毫秒计时。其他字段和已经确认的规则不要改。',
 '现在定版，给最终两张表的字段及含义。检查前面确认的内容有没有被改错。不要再扩展机制或追问，跨重启计时是否可比较仍按未知处理。',
]

def main():
    parser=argparse.ArgumentParser();parser.add_argument('--output',type=Path,required=True);parser.add_argument('--binary',type=Path,default=Path('/tmp/atomcode-installed-regression-dfa998f4/atomcode'));parser.add_argument('--deepseek-only',action='store_true');parser.add_argument('--natural',action='store_true');a=parser.parse_args();a.output.mkdir(parents=True,exist_ok=False)
    binary=a.binary.resolve()
    turns=NATURAL_TURNS if a.natural else TURNS
    profiles={'deepseek':'taotoken/deepseek-v4.1-flash-1017','glm':'AtomGit-glm5.3-flash'}
    if a.deepseek_only:profiles.pop('glm')
    e.save(a.output/'manifest.json',{'binary_version':subprocess.check_output([str(binary),'--version'],text=True).strip(),'binary_sha256':hashlib.sha256(binary.read_bytes()).hexdigest(),'profiles':profiles,'turns':turns,'fixture':'natural' if a.natural else 'closed-inventory','repetitions':1,'mode':'real native persisted turns --continue in separate pinned test projects; no-tools; existing native auth; test-only storage cleanup','limits':'different model-family persona/gateway/defaults; one dialogue per profile; no independent blind quality review; no attested backend weights;120s per-turn process test deadline'})
    product_home=Path(os.environ.get('ATOMCODE_HOME',str(Path.home()/'.atomcode')))
    session_root=product_home/'sessions';session_root.mkdir(exist_ok=True)
    def run(item):
        name,profile=item;result={'profile':name,'turns':[],'status':'running'};started=time.monotonic()
        # Exclusive newly created16hex bucket, recognized by native project marker.
        while True:
            bucket=secrets.token_hex(8);store=session_root/bucket
            try:store.mkdir(mode=0o700);break
            except FileExistsError:continue
        owner=secrets.token_hex(16);(store/'.evaluation-owner').write_text(owner)
        try:
            with tempfile.TemporaryDirectory(prefix='atomcode-live-dialogue-') as temp:
                project=Path(temp);marker=project/'.atomcode/local/id';marker.parent.mkdir(parents=True);marker.write_text(bucket)
                for index,user in enumerate(turns):
                    cmd=[str(binary),'--provider',profile,'-C',str(project),'--dev','--no-telemetry','--no-tools','--output-format','jsonl','-p',user]
                    if index:cmd.append('--continue')
                    begin=time.monotonic()
                    try:
                        proc=subprocess.run(cmd,capture_output=True,text=True,timeout=120)
                        events=c.native_events(proc.stdout,'[NO_KEY_PASSED]')
                        for event in events:
                            if event.get('type')=='error':event.clear();event.update({'type':'error','message':'native error details withheld'})
                        text=c.answer('atomcode',events)
                        result['turns'].append({'user':user,'text':text,'events':events,'exit_code':proc.returncode,'seconds':time.monotonic()-begin})
                        if proc.returncode or not text.strip() or not c.terminal_ok('atomcode',events):result['status']='execution_failure';break
                        indices=list(store.glob('*.index'))
                        if len(indices)!=1:result['status']='unverified_session_binding';break
                        ids=[p.stem for p in indices]
                        if result.get('session_ids') and result['session_ids']!=ids:result['status']='session_changed';break
                        result['session_ids']=ids
                        print(name,index+1,'completed',round(time.monotonic()-begin,1),flush=True)
                    except subprocess.TimeoutExpired:result['status']='timeout';break
                    finally:e.save(a.output/(name+'.json'),result)
                else:result['status']='external_review_required'
                facts=[]
                for path in store.glob('*.events'):
                    for line in path.read_text().splitlines()[1:]:
                        obj=json.loads(line);event=obj.get('event',{})
                        facts.append({'seq':obj.get('seq'),'kind':event.get('kind'),'turn':event.get('turn')})
                result['native_fact_metadata']=facts
        except Exception as exc:result.update({'status':'test_setup_failure','error':type(exc).__name__})
        finally:
            # Delete only the exact bucket we created, never any pre-existing store.
            if (store/'.evaluation-owner').read_text()==owner:
                shutil.rmtree(store);result['owned_test_bucket_removed']=not store.exists()
            result['seconds']=time.monotonic()-started;e.save(a.output/(name+'.json'),result)
        print(name,result['status'],flush=True)
    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:list(pool.map(run,profiles.items()))


if __name__=='__main__':main()
