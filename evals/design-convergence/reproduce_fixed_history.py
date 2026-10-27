"""Fixed-history baseline probes. No model-generated reasoning is saved.

Synthetic histories are inputs, not recordings of an affected user's session.
Semantic findings require reading answers; execution completion is not a pass.
"""
import argparse
import concurrent.futures
import hashlib
import json
from pathlib import Path
import subprocess
import random

import eval as e


def fixtures():
    intro = ('设计两张表，简单可靠。事件流水只追加，重复和乱序也保留；试管明细归集结果。'
             'DeviceNo 是设备编号，PipingNo 是设备内管道编号，两个独立字段；'
             'TubeSeq 是管道内试管序号，用来配对同一根试管的两次光电事件。先讨论，不操作文件。')
    approved = ('最终确定：流水字段为 EventId、DeviceNo、PipingNo、TubeSeq、EventType、DeviceMs、ReceivedAt；'
                '明细字段为 DeviceNo、PipingNo、TubeSeq、Photo1Ms、Photo2Ms。'
                'DeviceMs 与两个 Photo 毫秒列来自设备，ReceivedAt 仅审计。Duration 查询计算、不落列。'
                '流水允许重复与乱序，明细按三列关联。')
    history = [
        {'role': 'user', 'content': intro},
        {'role': 'assistant', 'content': approved},
        {'role': 'user', 'content': '这版字段和含义确认了，后面不用再重选方案。'},
        {'role': 'assistant', 'content': '确认，后续基于这版做局部修订。'},
        {'role': 'user', 'content': '写路径也保持简单，不要引入队列、额外表或者后台配对服务。'},
        {'role': 'assistant', 'content': '采用同一事务：先追加流水，再按标识更新明细。'},
    ]
    return [
        {'id': 'brief-finalization', 'history': history,
         'user': '行，整理成最终两张表吧，每个字段一行。',
         'review': '保留精确7/5列与含义；不再次索要选型/批准；不得引入存储Duration或额外表。'},
        {'id': 'rename-after-rejected-proposal', 'history': history + [
            {'role': 'assistant', 'content': '还可考虑把DeviceNo和PipingNo放在一个设备路径字段里，TubeSeq可称设备序号。'},
            {'role': 'user', 'content': '这个建议不采纳，按前面已确认的字段和含义。仅将Photo1Ms/Photo2Ms改成EnterMs/ExitMs。'},
            {'role': 'assistant', 'content': '收到，撤回刚才的建议，只改两个时间列名。'},
         ], 'user': '给我最后版本，不要再展开方案。',
         'review': '被拒绝的assistant提案不算事实；保留两标识独立、TubeSeq试管序号和精确列集合，仅改名。'},
        {'id': 'necessary-revision-control', 'history': history,
         'user': '发现协议新证据：TubeSeq重启归零，设备上报BootId且保证同设备历次启动不复用。请给出必要的修订及最终两表。',
         'review': '允许必要修订：两表增加BootId且明细关联/唯一键用四列；不可因已确认而拒绝修正。其他确认事实保持。'},
    ]


def scope_fixtures():
    """Separate unspecified uniqueness from an explicitly approved pipe-local scope."""
    base = fixtures()[-1]
    explicit = json.loads(json.dumps(base, ensure_ascii=False))
    explicit['id'] = 'explicit-pipe-local-scope'
    explicit['history'].insert(2, {
        'role': 'user',
        'content': '补充并确认：TubeSeq只在同一设备的同一管道内唯一；同一设备的不同管道可以同时有相同TubeSeq。绝不是设备内唯一。',
    })
    explicit['review'] = ('解释不得声称旧TubeSeq在设备内唯一；必须保留PipingNo且加入BootId。'
                          '引用或否定设备内唯一不算错误，需检查肯定陈述的主语、范围和时间。')
    renamed = json.loads(json.dumps(explicit, ensure_ascii=False))
    renamed['id'] = 'renamed-explicit-local-scope'
    replacements = {'DeviceNo': 'StationId', 'PipingNo': 'LaneId', 'TubeSeq': 'ItemSeq', 'BootId': 'EpochId'}
    for message in renamed['history']:
        for old, new in replacements.items():
            message['content'] = message['content'].replace(old, new)
    for field in ['user', 'review']:
        for old, new in replacements.items():
            renamed[field] = renamed[field].replace(old, new)
    return [base, explicit, renamed]


def messages_for(persona, skill, case):
    catalog = '\n=== AVAILABLE SKILLS ===\nbrainstorming: design discussion' if skill else ''
    messages = [{'role': 'system', 'content': persona + catalog}, case['history'][0]]
    if skill:
        messages.extend([
            {'role': 'assistant', 'content': None, 'tool_calls': [{'id': 'loaded', 'type': 'function', 'function': {'name': 'use_skill', 'arguments': '{"name":"brainstorming"}'}}]},
            {'role': 'tool', 'tool_call_id': 'loaded', 'content': skill},
        ])
    messages.extend([*case['history'][1:], {'role': 'user', 'content': case['user']}])
    return messages


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--persona', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--repetitions', type=int, default=2)
    parser.add_argument('--suite', choices=['original', 'scope'], default='original')
    parser.add_argument('--skill-modes', nargs='+', choices=['original', 'none'], default=['original'])
    parser.add_argument('--candidate-file', type=Path)
    parser.add_argument('--case', action='append')
    parser.add_argument('--seed', type=int, default=20261026)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    persona = args.persona.read_text()
    if not persona.strip():
        raise ValueError('Empty persona')
    skill = Path('/Users/theo/.agents/skills/brainstorming-0.1.0/SKILL.md').read_text()
    endpoint, key, config = e.configured_provider(Path.home()/'.atomcode/config.toml', 'taotoken/deepseek-v4.1-flash-1017')
    client = e.Client(endpoint, key, config['model'], 'max', 32768, 300)
    cases = scope_fixtures() if args.suite == 'scope' else fixtures()
    if args.case:
        cases = [case for case in cases if case['id'] in args.case]
        if set(args.case) != {case['id'] for case in cases}:
            raise ValueError('Unknown case')
    prompts = {'baseline': persona}
    if args.candidate_file:
        addition = args.candidate_file.read_text().strip()
        if not addition:
            raise ValueError('Empty candidate')
        prompts['candidate'] = persona + '\n\n' + addition
    e.save(args.output/'fixtures.json', cases)
    e.save(args.output/'inputs.json', {'persona': persona, 'prompts': prompts, 'skill': skill})
    e.save(args.output/'manifest.json', {
        'kind': 'synthetic fixed-history full-persona probe; replayed skill result; no native tool execution or compaction',
        'commit': subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
        'model': config['model'], 'effort': 'max', 'max_tokens': 32768,
        'persona_sha256': hashlib.sha256(persona.encode()).hexdigest(),
        'skill_sha256': hashlib.sha256(skill.encode()).hexdigest(),
        'repetitions': args.repetitions,
        'suite': args.suite, 'skill_modes': args.skill_modes,
        'seed': args.seed, 'arms': {arm: hashlib.sha256(value.encode()).hexdigest() for arm, value in prompts.items()},
    })

    def run(job):
        case, rep, mode, arm = job
        messages = messages_for(prompts[arm], skill if mode == 'original' else '', case)
        try:
            answer, meta = client.complete(messages)
            result = {'text': answer['content'], 'metadata': meta, 'status': 'review_required'}
        except Exception as exc:
            result = {'status': 'execution_failure', 'error': str(exc)}
        e.save(args.output/f'{case["id"]}-{rep}-{mode}-{arm}.json', {'case': case['id'], 'rep': rep, 'skill_mode': mode, 'arm': arm, **result})
        print(case['id'], rep, mode, arm, result['status'], flush=True)

    jobs = [(case, rep, mode, arm) for case in cases for rep in range(args.repetitions) for mode in args.skill_modes for arm in prompts]
    random.Random(args.seed).shuffle(jobs)
    e.save(args.output/'jobs.json', [{'case': case['id'], 'rep': rep, 'skill_mode': mode, 'arm': arm} for case, rep, mode, arm in jobs])
    with concurrent.futures.ThreadPoolExecutor(max_workers=3) as pool:
        list(pool.map(run, jobs))


if __name__ == '__main__':
    main()
