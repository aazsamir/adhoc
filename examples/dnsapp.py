import http.server, socket, socketserver, sys

class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        try:
            ip = socket.gethostbyname("example.com")
            body = ("resolve(example.com) -> %s\n" % ip).encode()
        except Exception as e:
            body = ("resolve FAILED: %s\n" % e).encode()
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *a):
        pass

port = int(sys.argv[1])
socketserver.TCPServer.allow_reuse_address = True
http.server.HTTPServer(("127.0.0.1", port), H).serve_forever()
