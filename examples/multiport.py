# Binds every port given on the command line and answers with which port was hit.
import http.server, socketserver, sys, threading, time

class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = ("port %d\n" % self.server.server_port).encode()
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *a):
        pass

class S(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True
    allow_reuse_address = True

ports = [int(p) for p in sys.argv[1:]]
delay = float(sys.argv[-1]) if sys.argv[-1].startswith("delay=") else 0
if delay:
    time.sleep(delay)   # simulate a slow starter
for p in ports:
    srv = S(("127.0.0.1", p), H)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
print("listening on", ports, flush=True)
while True:
    time.sleep(3600)
