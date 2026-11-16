"""gcloud 임시 서버에서 결제 링크·취소 콜백을 확인한다. 결제 승인/매입은 하지 않는다.

python3 certificate_payment_probe.py --form /private/form.json --port 38127 --state /private/state.json
공개 HTTPS 터널을 연 다음 --form JSON의 public_base에 그 주소를 설정하고 시작한다.
학교 결제 폼 필드와 public_base만 입력으로 받으며 요청/토큰/카드정보를 로그에 남기지 않는다.
"""

import argparse
import html
from http.server import BaseHTTPRequestHandler, HTTPServer
import json
import os
from pathlib import Path
import secrets
import time
from urllib.parse import parse_qs, urlsplit


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--form", type=Path, required=True)
    parser.add_argument("--state", type=Path, required=True)
    parser.add_argument("--port", type=int, default=38127)
    args = parser.parse_args()
    config = json.loads(args.form.read_text())
    base = config["public_base"].rstrip("/")
    parsed = urlsplit(base)
    if parsed.scheme != "https" or not parsed.netloc or parsed.path:
        raise ValueError("공개 HTTPS 주소가 필요합니다")
    fields = dict(config["fields"])
    amount = int(fields["LGD_AMOUNT"])
    if amount <= 0 or not fields.get("LGD_HASHDATA") or not fields.get("LGD_OID"):
        raise ValueError("학교가 생성한 유료 결제 폼이 필요합니다")
    ticket = secrets.token_urlsafe(32)
    deadline = time.monotonic() + 600
    state = {"status": "ready", "amount": amount, "approval_sent": False}

    def save():
        fd = os.open(args.state, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, "w") as stream:
            json.dump(state, stream)

    fields["LGD_RETURNURL"] = base + "/result/" + ticket
    fields["CST_WINDOW_TYPE"] = "submit"
    fields["LGD_CUSTOM_SWITCHINGTYPE"] = "SUBMIT"
    save()

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def respond(self, message, status=200):
            body = ("<!doctype html><html lang=ko><meta charset=utf-8>"
                    "<meta name=referrer content=no-referrer><title>증명서 결제 연결 시험</title>"
                    "<body>" + message + "</body></html>").encode()
            self.send_response(status)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Cache-Control", "no-store")
            self.send_header("Referrer-Policy", "no-referrer")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def valid(self, prefix):
            return self.path == prefix + ticket and time.monotonic() < deadline

        def do_GET(self):
            if not self.valid("/checkout/"):
                self.respond("링크가 없거나 만료되었습니다.", 404)
                return
            self.respond(f"<p>재학증명서 PDF 결제창 연결 시험 · {amount:,}원</p>"
                         "<p>카드 선택 화면까지만 확인하고 결제를 취소하세요.</p>"
                         f'<form method=post action="/start/{ticket}"><button>결제창 열기</button></form>')

        def do_POST(self):
            if not (self.valid("/start/") or self.valid("/result/")):
                self.respond("링크가 없거나 만료되었습니다.", 404)
                return
            size = int(self.headers.get("Content-Length", "0"))
            if not 0 <= size <= 65536:
                self.respond("응답 크기 초과", 413)
                return
            payload = self.rfile.read(size)
            if self.valid("/start/"):
                if state["status"] != "ready":
                    self.respond("이미 사용한 연결입니다.", 409)
                    return
                state["status"] = "checkout_opened"
                save()
                inputs = "".join(f'<input type=hidden name="{html.escape(k, quote=True)}" '
                                 f'value="{html.escape(str(v), quote=True)}">' for k, v in fields.items())
                self.respond('<form id=pay method=post action="https://checkout.tosspayments.com/xpay/Request.do">'
                             + inputs + '<button>토스페이먼츠로 이동</button></form>'
                             '<script>document.getElementById("pay").submit()</script>')
                return
            if state["status"] != "checkout_opened":
                self.respond("대기 중인 결제가 없습니다.", 409)
                return
            result = parse_qs(payload.decode("utf-8", errors="replace"))
            code = result.get("LGD_RESPCODE", [""])[0]
            # 브라우저 콜백은 결제 완료의 증거가 아니다. PAYKEY 승인 요청은 의도적으로 보내지 않는다.
            state["status"] = "authentication_returned_unverified" if code == "0000" else "non_success_returned"
            state["response_code"] = code[:16]
            state["has_paykey"] = bool(result.get("LGD_PAYKEY", [""])[0])
            save()
            self.respond("<p>결제 결과가 시험 서버로 돌아왔습니다.</p><p>승인 요청은 전송하지 않았습니다.</p>")

    print(json.dumps({"checkout_url": base + "/checkout/" + ticket}), flush=True)
    HTTPServer(("127.0.0.1", args.port), Handler).serve_forever()


if __name__ == "__main__":
    main()
