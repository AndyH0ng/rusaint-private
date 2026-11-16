"""완성된 PDF 한 파일만 만료되는 무작위 링크로 전달한다."""

import argparse
from http.server import BaseHTTPRequestHandler, HTTPServer
import json
from pathlib import Path
import secrets
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pdf", type=Path, required=True)
    parser.add_argument("--base", required=True)
    parser.add_argument("--port", type=int, default=38130)
    args = parser.parse_args()
    pdf = args.pdf.read_bytes()
    if not pdf.startswith(b"%PDF-"):
        raise ValueError("PDF 파일이 아닙니다")
    route = "/certificate/" + secrets.token_urlsafe(32) + ".pdf"
    deadline = time.monotonic() + 1800

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def respond(self, body):
            valid = self.path == route and time.monotonic() < deadline
            content = pdf if valid else b"Expired or unknown link"
            self.send_response(200 if valid else 404)
            self.send_header("Content-Type", "application/pdf" if valid else "text/plain")
            self.send_header("Content-Length", str(len(content)))
            self.send_header("Cache-Control", "no-store")
            self.send_header("Referrer-Policy", "no-referrer")
            self.send_header("X-Content-Type-Options", "nosniff")
            if valid:
                self.send_header("Content-Disposition", 'attachment; filename="scholarship-certificate.pdf"')
            self.end_headers()
            if body:
                self.wfile.write(content)

        def do_GET(self):
            self.respond(True)

        def do_HEAD(self):
            self.respond(False)

    print(json.dumps({"download_url": args.base.rstrip("/") + route}), flush=True)
    HTTPServer(("127.0.0.1", args.port), Handler).serve_forever()


if __name__ == "__main__":
    main()
