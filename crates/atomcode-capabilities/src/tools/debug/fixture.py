# Deterministic stdio DAP peer. Delays launch response until configurationDone.
import json
import os
import subprocess
import sys

seq = 0
launch = None
running = False

def send(value):
    global seq
    seq += 1
    value['seq'] = seq
    data = json.dumps(value).encode()
    sys.stdout.buffer.write(('Content-Length: %s\r\n\r\n' % len(data)).encode() + data)
    sys.stdout.buffer.flush()

def reply(req, body=None):
    send(dict(type='response', request_seq=req['seq'], command=req['command'], success=True, body=body or {}))

def stopped():
    send(dict(type='event', event='stopped', body=dict(threadId=1, reason='breakpoint')))

while True:
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            sys.exit(0)
        if line == b'\r\n':
            break
        k, v = line.decode().strip().split(':', 1)
        headers[k.lower()] = v.strip()
    req = json.loads(sys.stdin.buffer.read(int(headers['content-length'])))
    command = req['command']
    args = req.get('arguments', {})
    if command == 'initialize':
        reply(req, dict(supportsConfigurationDoneRequest=True))
    elif command == 'launch':
        launch = req
        if args['program'].endswith('reverse.py'):
            send(dict(type='request', command='runInTerminal', arguments=dict(args=['must-not-execute'])))
            continue
        if args['program'].endswith('failure.py'):
            send(dict(type='response', request_seq=req['seq'], command='launch', success=False, message='launch rejected'))
            # No initialized event: client must notice launch failure immediately.
            continue
        if args['program'].endswith('child.py'):
            child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(120)'])
            with open('pids', 'w') as f:
                f.write('%s %s' % (os.getpid(), child.pid))
        running = args['program'].endswith('running.py')
        send(dict(type='event', event='initialized'))
    elif command == 'setBreakpoints':
        reply(req, dict(breakpoints=[dict(verified=True, line=b['line']) for b in args['breakpoints']]))
    elif command == 'configurationDone':
        reply(req)
        reply(launch)
        if not running:
            stopped()
    elif command == 'threads':
        reply(req, dict(threads=[dict(id=1, name='main')]))
    elif command == 'stackTrace':
        reply(req, dict(stackFrames=[dict(id=10, name='main', line=2, column=1)], totalFrames=1))
    elif command == 'scopes':
        reply(req, dict(scopes=[dict(name='Locals', variablesReference=20, expensive=False)]))
    elif command == 'variables':
        reply(req, dict(variables=[dict(name='answer', value='42', variablesReference=0)] * 100))
    elif command == 'evaluate':
        reply(req, dict(result='42', variablesReference=0))
    elif command in ('continue', 'next', 'stepIn', 'stepOut'):
        send(dict(type='event', event='continued'))
        stopped()  # Deliberately BEFORE response to expose event/response races.
        reply(req)
    elif command == 'disconnect':
        reply(req)
    else:
        raise RuntimeError(command)
