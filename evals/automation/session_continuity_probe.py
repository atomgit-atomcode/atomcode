#!/usr/bin/env python3
import http.server, threading, json, tempfile, pathlib, subprocess, os, fcntl
import argparse
parser = argparse.ArgumentParser(description='Offline Unix task/session continuity probe; requires a built CLI, no real OS task registration.')
parser.add_argument('--binary', type=pathlib.Path, default=pathlib.Path('target/debug/atomcode'))
binary = str(parser.parse_args().binary.resolve())
requests = []

class Handler(http.server.BaseHTTPRequestHandler):

    def log_message(self, *args):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        requests.append(body)
        text = 'AUTOMATION_RESULT_' + str(len(requests))
        if body.get('stream'):
            payload = ('data: ' + json.dumps({'id': 'fixture', 'choices': [{'index': 0, 'delta': {'content': text}, 'finish_reason': 'stop'}]}) + '\n\ndata: ' + json.dumps({'choices': [], 'usage': {'prompt_tokens': 1000, 'completion_tokens': 10}}) + '\n\ndata: [DONE]\n\n').encode()
            content = 'text/event-stream'
        else:
            payload = json.dumps({'choices': [{'message': {'role': 'assistant', 'content': text}, 'finish_reason': 'stop'}], 'usage': {'prompt_tokens': 1000, 'completion_tokens': 10}}).encode()
            content = 'application/json'
        self.send_response(200)
        self.send_header('Content-Type', content)
        self.send_header('Content-Length', str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)
server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()
with tempfile.TemporaryDirectory(prefix='automation-continuity-') as tmp:
    home = pathlib.Path(tmp)
    project = home / 'project'
    project.mkdir()
    (home / 'schedules').mkdir()
    (home / 'config.toml').write_text('default_provider="probe"\n[providers.probe]\ntype="openai"\nmodel="deepseek-v4.1-flash-1017"\napi_key="offline-dummy"\nbase_url="http://127.0.0.1:' + str(server.server_port) + '/v1"\nretry_max_attempts=1\n[telemetry]\nenabled=false\n[notifications]\nenabled=false\n')
    task = {'id': 'hourly-probe', 'title': 'Hourly probe', 'prompt': 'AUTOMATION_CONTINUITY_CHECK: perform the hourly check', 'cwd': str(project), 'schedule': {'kind': 'hourly'}, 'permission_mode': 'plan', 'notify': 'off', 'enabled': True}
    (home / 'schedules/hourly-probe.json').write_text(json.dumps(task))
    env = {**os.environ, 'ATOMCODE_HOME': tmp, 'NO_PROXY': '127.0.0.1,localhost', 'no_proxy': '127.0.0.1,localhost'}

    def run(ok=True):
        result = subprocess.run([binary, '--dev', '--no-telemetry', 'schedule', 'run', 'hourly-probe'], env=env, capture_output=True, text=True, timeout=60)
        assert (result.returncode == 0) == ok, (result.returncode, result.stderr[-2000:], result.stdout[-1000:])

    def facts():
        return [json.loads(x) for x in (home / 'schedules/runs/hourly-probe/events.jsonl').read_text().splitlines()]
    run()
    first_request_count = len(requests)
    run()
    ids = [x['session_id'] for x in facts() if x['kind'] == 'session']
    assert len(ids) == 2 and len(set(ids)) == 1, ids
    events = list(home.rglob('*.events'))
    assert len(events) == 1, events
    raw = events[0].read_text()
    assert raw.count('AUTOMATION_CONTINUITY_CHECK') >= 2
    assert any((any((m.get('role') == 'assistant' and 'AUTOMATION_RESULT_' in str(m.get('content')) for m in r.get('messages', []))) and sum(m.get('role') == 'user' and 'AUTOMATION_CONTINUITY_CHECK' in str(m.get('content')) for m in r.get('messages', [])) >= 2 for r in requests[first_request_count:])), 'previous assistant reply or two task turns missing from resumed request'
    index = events[0].with_suffix('.index')
    assert json.loads(index.read_text())['origin'] == 'scheduled'
    print('PASS two real CLI executions reuse one native log and earlier model context', flush=True)
    before = len(requests)
    with events[0].with_suffix('.lease').open('r+') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        run(False)
    assert len(requests) == before and len(list(home.rglob('*.events'))) == 1
    print('PASS busy session fails without fork/model request', flush=True)
    other = home / 'other-project'
    other.mkdir()
    task['cwd'] = str(other)
    (home / 'schedules/hourly-probe.json').write_text(json.dumps(task))
    run(False)
    assert len(requests) == before and len(list(home.rglob('*.events'))) == 1
    assert facts()[-1]['status'] == 'error'
    print('PASS changed project fails without moving the bound conversation', flush=True)
    task['cwd'] = str(project)
    (home / 'schedules/hourly-probe.json').write_text(json.dumps(task))
    events[0].rename(events[0].with_suffix('.events.saved'))
    run(False)
    assert len(requests) == before and (not list(home.rglob('*.events')))
    print('PASS missing session fails without fresh fallback', flush=True)
server.shutdown()
