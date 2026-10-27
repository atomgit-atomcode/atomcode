#!/usr/bin/env python3
"""Execution metadata only; does not infer semantic correctness from keywords."""
import collections, importlib.util, json
from pathlib import Path
spec=importlib.util.spec_from_file_location('cross',Path(__file__).with_name('cross_agents.py'))
c=importlib.util.module_from_spec(spec);spec.loader.exec_module(c)
r=c.e.ROOT/'results'

def tool_calls(agent,events):
    found=[]
    for event in events:
        kind=event.get('type')
        if agent=='atomcode' and kind=='tool.started':found.append((event.get('name'),event.get('arguments')))
        elif agent=='codex' and kind=='item.completed':
            item=event.get('item',{})
            if item.get('type')=='command_execution':found.append(('command_execution',item.get('command')))
            elif item.get('type')=='mcp_tool_call':found.append((item.get('tool'),item.get('arguments')))
        elif agent=='opencode' and kind=='tool_use':
            part=event.get('part',{});found.append((part.get('tool'),part.get('state',{}).get('input')))
        elif agent=='omp' and kind=='tool_execution_start':found.append((event.get('toolName'),event.get('args')))
        elif agent=='dsh' and kind=='tool_call':found.append((event.get('tool'),event.get('input')))
    return found

def result_texts(agent,events):
    found=[]
    for event in events:
        if agent=='atomcode' and event.get('type')=='tool.completed':found.append(event.get('content',''))
        elif agent=='codex' and event.get('type')=='item.completed':found.append(event.get('item',{}).get('aggregated_output',''))
        elif agent=='opencode' and event.get('type')=='tool_use':found.append(event.get('part',{}).get('state',{}).get('output',''))
        elif agent=='omp' and event.get('type')=='tool_execution_end':found.append(event.get('result',{}))
        elif agent=='dsh' and event.get('type')=='tool_result':found.append(event.get('result',''))
    return json.dumps(found,ensure_ascii=False)

def row(run,path):
    data=json.loads(path.read_text());agent=data['agent'];events=[x for t in data['turns'] for x in t['events']]
    records=json.loads((r/run/'relay.json').read_text()) if (r/run/'relay.json').exists() else []
    records=[x for x in records if x['route']==path.stem]
    sessions=[]
    for event in events:
        session=None
        if agent=='codex' and event.get('type')=='thread.started':session=event.get('thread_id')
        elif agent=='opencode':session=event.get('sessionID')
        elif agent=='dsh' and event.get('type')=='session':session=event.get('sessionId')
        elif agent=='omp' and event.get('type')=='session':session=event.get('id')
        if session:sessions.append(session)
    texts=[('\n'.join(x.get('text','') for x in t['events'] if x.get('type')=='text') if agent=='dsh' else t['text']) for t in data['turns']]
    return {'run':run,'packet':path.stem,'agent':agent,'case':data['case'],
        'status':data['status'],'visited_turns':len(data['turns']),
        'native_completed_turns':sum(t['exit_code']==0 and bool(t['text'].strip()) and c.terminal_ok(agent,t['events']) for t in data['turns']),
        'observed_session_ids':sorted(set(sessions)),
        'requests_including_auxiliary':len(records),
        'actual_models':sorted({x['actual_model'] for x in records if x.get('actual_model')}),
        'requests_without_model_report':sum(not x.get('actual_model') for x in records),
        'tool_calls':len(tool_calls(agent,events)),
        'tool_names':dict(collections.Counter(name for name,_ in tool_calls(agent,events))),
        'original_skill_body_in_tool_result':'This applies to EVERY project regardless of perceived simplicity.' in result_texts(agent,events),
        'visible_text_characters':sum(map(len,texts)),
        'initial_visible_characters':len(texts[0]) if texts else 0,
        'human_validated':False,'semantic_status':'requires_assistant_review_not_an_automatic_pass'}

def main():
    selected=[]
    for run in ['cross-round-1','cross-round-2','cross-explicit-skill']:
        for path in sorted((r/run).glob('*.json')):
            if path.name in ['manifest.json','relay.json']:continue
            if json.loads(path.read_text()).get('agent')!='atomcode':selected.append(row(run,path))
    for run in ['cross-atomcode-frozen-round-1','cross-atomcode-frozen-round-2','cross-atomcode-frozen-explicit','cross-cap-32768']:
        if not (r/run).exists():continue
        for path in sorted((r/run).glob('*.json')):
            if path.name in ['manifest.json','relay.json']:continue
            selected.append(row(run,path))
    c.e.save(r/'cross-summary.json',{'selected':selected,
        'excluded_atomcode_runs':['cross-round-1','cross-round-2','cross-explicit-skill'],
        'exclusion_reason':'Shared development binary rebuilt during sampling; retain originals and replace AtomCode cohort with frozen-binary runs. No other agent excluded for its content.'})
    for a in ['atomcode','codex','opencode','omp','dsh']:
        for x in selected:
            if x['agent']==a:print(a,x['run'],x['case'],x['status'],x['native_completed_turns'],x['requests_including_auxiliary'],x['tool_calls'],x['original_skill_body_in_tool_result'],x['visible_text_characters'])
if __name__=='__main__':main()
