"""A terminal SSE event must finish the relay without waiting for HTTP EOF."""
import http.server,json,tempfile,threading,time,unittest,urllib.request
from pathlib import Path
import cross_agents as c

class RelayTerminalTests(unittest.TestCase):
    def test_done_closes_an_upstream_body_that_stays_open(self):
        release=threading.Event()
        class Upstream(http.server.BaseHTTPRequestHandler):
            protocol_version='HTTP/1.1'
            def log_message(self,*args):pass
            def do_POST(self):
                self.rfile.read(int(self.headers['Content-Length']))
                payload=b'data: {"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]}\n\ndata: [DONE]\n\n'
                self.send_response(200);self.send_header('Content-Type','text/event-stream');self.send_header('Content-Length',str(len(payload)+100));self.end_headers();self.wfile.write(payload);self.wfile.flush()
                release.wait(3)
        server=http.server.ThreadingHTTPServer(('127.0.0.1',0),Upstream);threading.Thread(target=server.serve_forever,daemon=True).start()
        with tempfile.TemporaryDirectory() as temp:
            relay=c.Relay('http://127.0.0.1:'+str(server.server_port),'offline-key','model',Path(temp))
            try:
                req=urllib.request.Request(relay.url('test')+'/completions',json.dumps({'messages':[],'stream':True}).encode(),{'Authorization':'Bearer '+relay.token,'Content-Type':'application/json'})
                with urllib.request.urlopen(req,timeout=2) as response:
                    for line in response:
                        if line.strip()==b'data: [DONE]':break
                deadline=time.monotonic()+1
                while not relay.records and time.monotonic()<deadline:time.sleep(.01)
                self.assertEqual(len(relay.records),1,'SSE completion must not wait for upstream HTTP EOF')
                self.assertEqual(relay.records[0]['finish_reason'],'stop')
                self.assertEqual(relay.records[0]['text_chars'],2)
            finally:
                release.set();relay.server.shutdown();relay.server.server_close();server.shutdown();server.server_close()

if __name__=='__main__':unittest.main()
