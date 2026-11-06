#!/usr/bin/env python3
import hashlib, http.server, json, os, sys
class UploadHandler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        if self.path != '/upload':
            self.send_error(404)
            return
        target_path = self.headers.get('X-Target-Path', '/workspace/upload.tar.gz')
        content_length = int(self.headers.get('Content-Length', 0))
        os.makedirs(os.path.dirname(target_path), exist_ok=True)
        md5 = hashlib.md5()
        bytes_written = 0
        with open(target_path, 'wb') as f:
            remaining = content_length
            while remaining > 0:
                chunk = self.rfile.read(min(remaining, 65536))
                if not chunk:
                    break
                f.write(chunk)
                md5.update(chunk)
                bytes_written += len(chunk)
                remaining -= len(chunk)
        result = json.dumps({'ok': True, 'path': target_path, 'bytes': bytes_written, 'md5': md5.hexdigest()})
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(result)))
        self.end_headers()
        self.wfile.write(result.encode())
    def do_GET(self):
        if self.path == '/health':
            body = b'ok'
            self.send_response(200)
            self.send_header('Content-Length', str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        else:
            self.send_error(404)
    def log_message(self, format, *args):
        pass
if __name__ == '__main__':
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8888
    token = sys.argv[2] if len(sys.argv) > 2 else ''
    server = http.server.HTTPServer(('127.0.0.1', port), UploadHandler)
    server.upload_token = token
    server.serve_forever()
