"""Opt-in document editing probe using existing native read_file/edit_file tools."""
import concurrent.futures
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
import threading

import eval as e
import cross_agents as c

DOC='''# 已确认的两表方案

## 事件流水
- EventId：本地流水行标识；每次上报独立新增，重复也保留。
- DeviceNo：设备编号。
- PipingNo：设备内管道编号，与DeviceNo分开存储。
- TubeSeq：管道内试管序号，不同管道可以重号。
- EventType：光电事件类型，区分第一次和第二次光电。
- DeviceMs：设备上报的该次事件毫秒值。
- ReceivedAt：服务端接收时刻，仅审计。

## 试管明细
- DeviceNo：设备编号。
- PipingNo：设备内管道编号。
- TubeSeq：管道内试管序号。
- Photo1Ms：第一次光电的设备毫秒值。
- Photo2Ms：第二次光电的设备毫秒值。

## 关联键
(DeviceNo, PipingNo, TubeSeq)

## 已确认规则
- 流水只追加，重复、乱序均保留。
- 同一事务先追加流水，再更新明细。
- Duration查询时计算，不落列。
- 禁止将DeviceNo/PipingNo合并成设备路径；禁止将TubeSeq称为设备序号。

## 尚未确认
- DeviceMs跨启动是否归零：未确认，不能当作已知协议行为。
'''
TASKS={
 'rename': '将design.md中的Photo1Ms改成EnterMs、Photo2Ms改成ExitMs，其余内容逐字保持。',
 'boot': '协议新证据：每条事件都带BootId，同一设备历次启动不复用；TubeSeq重启归零且0是合法上报值。将design.md两张表各加BootId，关联键加BootId，在已确认规则中补充TubeSeq归零且0合法。其余既有字段含义与未确认事项保持，不新增机制。',
}


def main():
    import argparse
    p=argparse.ArgumentParser();p.add_argument('--output',type=Path,required=True)
    p.add_argument('--tasks',nargs='+')
    p.add_argument('--fixture',type=Path)
    p.add_argument('--inject-after-read',action='store_true')
    p.add_argument('--inject-completed-task',action='store_true')
    p.add_argument('--repetitions',type=int,default=2)
    p.add_argument('--max-rounds',type=int,default=6)
    p.add_argument('--binary',type=Path,default=Path('/tmp/atomcode-cross-agent-tools/atomcode-frozen'))
    a=p.parse_args()
    fixture=json.loads(a.fixture.read_text()) if a.fixture else {'document':DOC,'tasks':TASKS,'expected_documents':{'rename':DOC.replace('Photo1Ms','EnterMs').replace('Photo2Ms','ExitMs')}}
    document=fixture['document'];tasks=fixture['tasks'];expected=fixture.get('expected_documents',{})
    a.tasks=a.tasks or list(tasks)
    if not set(a.tasks)<=set(tasks):raise ValueError('Unknown task')
    if a.inject_completed_task and not set(a.tasks)<=set(expected):raise ValueError('Completed-task injection needs exact expected documents')
    a.output.mkdir(parents=True,exist_ok=False)
    endpoint,key,cfg=e.configured_provider(Path.home()/'.atomcode/config.toml','taotoken/deepseek-v4.1-flash-1017')
    relay=c.Relay(endpoint,key,cfg['model'],a.output,32768)
    active={};active_lock=threading.Lock()
    injected_note='\n## 并发维护说明\n- 外部维护者补充：AmountCents必须保持整数分，禁止改为浮点金额。\n'
    def observe(route,body):
        if not (a.inject_after_read or a.inject_completed_task):return
        # Simulate another writer only after a native read_file result reaches the provider.
        tool_text='\n'.join(str(m.get('content','')) for m in body.get('messages',[]) if m.get('role')=='tool')
        if document.splitlines()[0] not in tool_text:return
        with active_lock:
            entry=active.get(route)
            if entry and not entry['injected']:
                path=entry['path']
                text=expected[entry['task']] if a.inject_completed_task else path.read_text()
                path.write_text(text+injected_note);entry['injected']=True
    relay.request_observer=observe
    binary=a.binary.resolve()
    e.save(a.output/'inputs.json',fixture)
    e.save(a.output/'manifest.json',{'kind':'native opt-in edit-existing-document workflow; no production patch','binary_version':subprocess.check_output([str(binary),'--version'],text=True).strip(),'binary_sha256':hashlib.sha256(binary.read_bytes()).hexdigest(),'model':cfg['model'],'effort':'max','cap':32768,'repetitions':a.repetitions,'max_rounds':a.max_rounds,'tasks':a.tasks,'limits':'explicit file-edit authorization and canonical confirmed artifact differ from earlier discussion-only fixtures; no causal quality comparison or general solution claim'})
    def run(job):
        task,rep=job;name=f'{task}-{rep}';result={'task':task,'rep':rep};start=time.monotonic()
        with tempfile.TemporaryDirectory(prefix='atomcode-design-document-') as temp:
            root=Path(temp);project=root/'project';project.mkdir();home=root/'state';home.mkdir();path=project/'design.md';path.write_text(document)
            with active_lock:active[name]={'path':path,'task':task,'injected':False}
            config=root/'config.toml';quote=lambda x:json.dumps(x,ensure_ascii=False)
            config.write_text('default_provider="eval"\n[providers.eval]\ntype="openai"\n'+f'model={quote(cfg["model"])}\nbase_url={quote(relay.url(name))}\napi_key={quote(relay.token)}\nreasoning_effort="max"\ncontext_window=1000000\nmax_tokens=32768\nretry_max_attempts=1\n[coding]\nmax_rounds=6\n[permissions]\ndeny=["Bash(*)","Write(*)"]\n[telemetry]\nenabled=false\n')
            config.write_text(config.read_text().replace('max_rounds=6',f'max_rounds={a.max_rounds}'))
            env={**os.environ,'ATOMCODE_HOME':str(home),'ATOMCODE_FIRST_TOKEN_TIMEOUT_SECS':'180','NO_PROXY':'127.0.0.1,localhost','no_proxy':'127.0.0.1,localhost'}
            for var in list(env):
                if var.endswith(('API_KEY','ACCESS_TOKEN','AUTH_TOKEN')):env.pop(var,None)
            prompt='design.md是用户已确认的方案。本次已明确授权直接修改这一个测试文件，请先读取，再用edit_file做最小替换，完成后简短说明实际改动。不重新设计，不创建其他文件，不重开审批或访谈。'+tasks[task]
            try:
                proc=subprocess.run([str(binary),'--config',str(config),'--provider','eval','-C',str(project),'--dev','--no-telemetry','-y','--output-format','jsonl','-p',prompt],env=env,capture_output=True,text=True,timeout=240)
                events=c.native_events(proc.stdout,key)
                result.update({'exit_code':proc.returncode,'events':events,'text':c.answer('atomcode',events),'status':'external_review_required' if proc.returncode==0 and c.terminal_ok('atomcode',events) else 'execution_failure'})
            except subprocess.TimeoutExpired:result['status']='timeout'
            result['after']=path.read_text() if path.exists() else None
            with active_lock:result['injected_after_read']=active.pop(name)['injected']
            if task in expected:
                expected_after=expected[task]+(injected_note if result['injected_after_read'] else '')
                result['exact_requested_change']=result['after']==expected_after
            result['seconds']=time.monotonic()-start
            e.save(a.output/(name+'.json'),result);print(name,result['status'],round(result['seconds'],1),result.get('exact_requested_change'),flush=True)
    try:
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:list(pool.map(run,[(task,rep) for task in a.tasks for rep in range(1,a.repetitions+1)]))
    finally:relay.server.shutdown()


if __name__=='__main__':main()
