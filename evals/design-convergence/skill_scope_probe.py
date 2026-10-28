import sys,json,subprocess,concurrent.futures,hashlib
from pathlib import Path
sys.path.insert(0,str(Path(__file__).resolve().parent))
import eval as e
# Frozen tested fragments; candidate was withdrawn from production after this trial.
prompts=json.loads((e.ROOT/'results/skill-scope-probe-exact-20261026/prompts.json').read_text())
endpoint,key,cfg=e.configured_provider(Path.home()/'.atomcode/config.toml','taotoken/deepseek-v4.1-flash-1017')
client=e.Client(endpoint,key,cfg['model'],'max',32768,300)
skill=Path('/Users/theo/.agents/skills/brainstorming-0.1.0/SKILL.md').read_text()
context='完整已确认表结构：事件流水字段只有EventId、DeviceNo、PipingNo、TubeSeq、EventType、DeviceMs、ReceivedAt；试管明细字段只有DeviceNo、PipingNo、TubeSeq、Photo1Ms、Photo2Ms。流水中的DeviceMs是该次光电事件的设备毫秒；明细中Photo1Ms/Photo2Ms按EventType归集。已确认：只用事件流水和试管明细两张表。DeviceNo是设备编号，PipingNo是设备内管道编号，两个独立字段。TubeSeq是管道内试管序号，不是设备序号。每次上报先追加流水，包括重复和乱序。Photo1Ms/Photo2Ms是设备毫秒，ReceivedAt只供审计；Duration查询计算，不落列。已完成方案讨论和确认，不操作文件。'
cases={'rename':'只把Photo1Ms和Photo2Ms改名为EnterMs和ExitMs，其他已确认字段、语义和行为保持。直接给修改后的两张表，每字段一行，不再讨论选型。','new-evidence':'协议新证据：TubeSeq每次设备重启从0开始，设备同时上报BootId；以前没考虑这个事实。仅修复关联冲突，给出两张表的必要字段和关联键，说明真正需要改什么，不重开其余已确认设计。'}
import argparse
parser=argparse.ArgumentParser()
parser.add_argument('--output',type=Path,required=True)
out=parser.parse_args().output;out.mkdir(parents=True,exist_ok=False)
e.save(out/'manifest.json',{'kind':'isolated SKILLS fragment fixed-history probe; not native CLI or full persona','model':cfg['model'],'effort':'max','cap':32768,'prompt_sha':{a:hashlib.sha256(p.encode()).hexdigest() for a,p in prompts.items()},'skill_sha':hashlib.sha256(skill.encode()).hexdigest()})
def run(job):
 arm,case=job
 messages=[{'role':'system','content':prompts[arm]+"\n=== AVAILABLE SKILLS ===\nbrainstorming: design discussion"},{'role':'user','content':context},{'role':'assistant','content':None,'tool_calls':[{'id':'loaded','type':'function','function':{'name':'use_skill','arguments':'{"name":"brainstorming"}'}}]},{'role':'tool','tool_call_id':'loaded','content':skill},{'role':'assistant','content':'方案已按上述约束确认，后续只按你的修改要求修订。'},{'role':'user','content':cases[case]}]
 try:
  answer,meta=client.complete(messages);result={'arm':arm,'case':case,'text':answer['content'],'metadata':meta,'status':'review_required'}
 except Exception as exc:result={'arm':arm,'case':case,'status':'execution_failure','error':type(exc).__name__+':'+str(exc)}
 e.save(out/(arm+'-'+case+'.json'),result);print(arm,case,result['status'],flush=True)
with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:list(pool.map(run,[(a,c) for a in prompts for c in cases]))
