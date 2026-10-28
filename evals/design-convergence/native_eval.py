#!/usr/bin/env python3
"""Bounded native CLI/resume trials, with isolated product homes and skill copies."""
import argparse
import concurrent.futures
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile

spec = importlib.util.spec_from_file_location('design_eval', Path(__file__).with_name('eval.py'))
e = importlib.util.module_from_spec(spec)
spec.loader.exec_module(e)


def visible_events(stdout, credential):
    events = []
    for line in stdout.splitlines():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(event, dict) and event.get("type") != "reasoning.delta":
            events.append(event)
    return json.loads(json.dumps(events).replace(credential, "[REDACTED]"))


def completed(events):
    return any(event.get("type") == "turn.completed" and event.get("exit_code") == 0 for event in events)


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--output', type=Path, required=True)
    p.add_argument('--config', type=Path, default=Path.home()/'.atomcode/config.toml')
    p.add_argument('--selection', default='taotoken/deepseek-v4.1-flash-1017')
    p.add_argument('--repetitions', type=int, default=1)
    p.add_argument('--arms', nargs='+', choices=['skill-original', 'skill-scoped'], default=['skill-original', 'skill-scoped'])
    args = p.parse_args()
    endpoint, key, model = e.configured_provider(args.config, args.selection)
    binary = e.REPO/'target/debug/atomcode'
    version = subprocess.check_output([str(binary), '--version'], text=True).strip()
    args.output.mkdir(parents=True, exist_ok=False)
    cases = [
        '帮我设计接收试管输送事件的数据库方案，简单可靠就好，两张表：事件流水、试管明细。每次上报先存流水，计算结果在明细。DeviceNo 是设备编号，PipingNo 是设备内管道编号，两个独立字段；TubeSeq 是管道内试管序号，用来配对同一根试管的两次光电事件，不是设备序号。先讨论方案，不操作文件。',
        '字段有点多，能不能简单一些？DeviceNo、PipingNo、TubeSeq 三个字段的独立性和含义我已经确认，别改这几个。',
        '不要为了稳妥加一堆扩展机制，重复上报和乱序的原始事件也都要留在流水。',
        '时间用设备上报的毫秒值，收到时间仅审计。明细的两个时间字段叫 Photo1Ms、Photo2Ms；Duration 查询时相减，不落列。其他已确认的内容保留。',
        '不要再讨论扩展，给一个最小可用的推荐方案。',
        '给最终两张表的字段清单，每个字段独立一行，写清含义和关联条件，并说明实际改动，不再提问。',
    ]
    e.save(args.output/'manifest.json', {'version':version, 'selection':args.selection,
        'requested_model':model['model'], 'effort':model.get('reasoning_effort'),
        'turns':cases, 'repetitions':args.repetitions,
        'limitations':'Native CLI resumes on each user turn; other shared skill catalog entries remain present. Headless cannot provide an interactive question UI. No traffic interception; configured model identity is not backend attestation.'})

    def work(arm, repetition):
        result = {'arm':arm, 'repetition':repetition, 'turns':[], 'status':'running'}
        path = args.output/f'{arm}-{repetition}.json'
        # The credential config is outside the model workspace, mode 0600, removed at exit.
        with tempfile.TemporaryDirectory(prefix='atomcode-design-native-') as tmp:
            root = Path(tmp); project=root/'project';project.mkdir(); home=root/'product';home.mkdir()
            snapshot=json.loads((e.ROOT/f'results/{arm}-prompts.json').read_text())
            skill=project/'.atomcode/skills/brainstorming/SKILL.md';skill.parent.mkdir(parents=True)
            skill.write_text(snapshot['loaded_skill'])
            result['skill_sha256']=hashlib.sha256(skill.read_bytes()).hexdigest()
            config=root/'config.toml'
            quote=lambda value:json.dumps(value,ensure_ascii=False)
            config.write_text('default_provider = "eval"\n[providers.eval]\ntype = "openai"\n'
                +f'model = {quote(model["model"])}\nbase_url = {quote(endpoint.removesuffix("/chat/completions"))}\napi_key = {quote(key)}\n'
                +'context_window = 1000000\nmax_tokens = 16000\nreasoning_effort = "max"\nretry_max_attempts = 1\n'
                +'[coding]\nmax_rounds = 12\n[permissions]\ndeny = ["Bash(*)", "Write(*)", "Edit(*)"]\n[telemetry]\nenabled = false\n')
            config.chmod(0o600)
            env={**os.environ,'ATOMCODE_HOME':str(home),'ATOMCODE_FIRST_TOKEN_TIMEOUT_SECS':'300'}
            for index, user in enumerate(cases):
                cmd=[str(binary),'--config',str(config),'--provider','eval','-C',str(project),
                     '--dev','--no-telemetry','--output-format','jsonl','-p',user]
                if index:cmd.append('--continue')
                try:
                    proc=subprocess.run(cmd,env=env,capture_output=True,text=True,timeout=360)
                    events = visible_events(proc.stdout, key)
                    result['turns'].append({'user':user,'exit_code':proc.returncode,'events':events,
                        'text':''.join(x.get('text','') for x in events if x.get('type')=='message.delta')})
                    if proc.returncode or not completed(events):
                        result['status']='execution_failure'
                        result['stderr_tail']=proc.stderr[-2000:].replace(key,'[REDACTED]')
                        break
                except subprocess.TimeoutExpired:
                    result['status']='timeout';break
                finally:e.save(path,result)
                print(json.dumps({'arm':arm,'rep':repetition,'turn':index+1,'exit_code':proc.returncode}),flush=True)
            else:result['status']='review_required'
            # Event-log facts explain why a turn continued; retain only the nudge
            # counters, never model reasoning or private session contents.
            counts = {}
            for journal in home.rglob('*.events'):
                for line in journal.read_text().splitlines()[1:]:
                    fact = json.loads(line).get('event', {})
                    if fact.get('kind') == 'injected' and fact.get('text', '').startswith('Before you finish: the task list still has open items'):
                        turn = str(fact.get('turn'))
                        counts[turn] = counts.get(turn, 0) + 1
            result['todo_completion_nudges_by_turn'] = counts
            e.save(path,result)
        return result['status']
    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
        statuses=list(pool.map(lambda job:work(*job),[(arm,rep+1) for rep in range(args.repetitions) for arm in args.arms]))
    return int(any(s!='review_required' for s in statuses))

if __name__=='__main__':raise SystemExit(main())
