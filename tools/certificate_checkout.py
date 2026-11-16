"""원격 증명서 결제 이어받기 실험 서버.

학교가 생성한 fields, cookie_header, public_base를 담은 비공개 JSON을 입력한다.
--allow-approval을 지정하면 사용자의 PG 인증 후 학교 승인 경로를 한 번 호출한다.
서버 세션·주문은 입력 파일에 고정되며 브라우저의 주문/금액으로 덮어쓰지 않는다.
"""

import argparse
from datetime import datetime
from html import escape
from html.parser import HTMLParser
from http.cookiejar import Cookie, CookieJar
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import re
import secrets
import threading
import time
from urllib.parse import parse_qs, urlencode, urlsplit
from urllib.error import HTTPError
from urllib.request import HTTPCookieProcessor, HTTPRedirectHandler, Request, build_opener
from zoneinfo import ZoneInfo

SCHOOL = "https://certi.ssu.ac.kr"
APPROVAL_PATH = "/wm1.0/jsp/pay/lg/filepay/newcertres.jsp"
MAX_RESPONSE = 32 * 1024 * 1024


def write_private(path, data, *, exclusive=False):
    flags = os.O_WRONLY | os.O_CREAT | (os.O_EXCL if exclusive else os.O_TRUNC)
    descriptor = os.open(path, flags, 0o600)
    with os.fdopen(descriptor, "wb") as stream:
        stream.write(data)
        stream.flush()
        os.fsync(stream.fileno())


class Basket(HTMLParser):
    def __init__(self, source):
        super().__init__(convert_charrefs=True)
        self.fields = {}
        self.rows = []
        self.stack = []
        self.feed(source)

    def handle_starttag(self, tag, attrs):
        attributes = dict(attrs)
        if tag == "tr":
            self.stack.append({"values": [], "links": []})
        if tag == "input" and attributes.get("name"):
            self.fields[attributes["name"]] = attributes.get("value", "")
        for row in self.stack[-1:]:
            row["values"].extend(attributes.values())
            if tag == "a":
                row["links"].append(attributes.get("href", ""))

    def handle_endtag(self, tag):
        if tag == "tr" and self.stack:
            self.rows.append(self.stack.pop())

    def download_index(self, document_numbers):
        for row in self.rows:
            if not any(number and number in row["values"] for number in document_numbers):
                continue
            for link in row["links"]:
                match = re.fullmatch(r"javascript:certdown\(\s*['\"]?(\d+)['\"]?\s*,\s*['\"][AB]['\"]\s*\);?", link)
                if match:
                    return match[1]
        return None

    def has_native_document(self, document_numbers):
        return any(preview["minno"] in document_numbers for preview in self.previews())

    def previews(self):
        """일반 출력 보관함의 preview() 인자를 발급 내역으로 읽는다."""
        found = []
        for row in self.rows:
            for link in row["links"]:
                if not re.match(r"javascript:(?:reportx\.)?preview\(", link):
                    continue
                values = re.findall(r"'([^']*)'", link)
                if len(values) != 8 or not values[1] or not values[3].isdigit() or not values[4].isdigit():
                    continue
                found.append({"job": values[0], "minno": values[1], "count": int(values[3]),
                              "printed": int(values[4]), "korean": values[5],
                              "printgb": values[6], "printable": values[7]})
        return found


class SchoolOnlyRedirect(HTTPRedirectHandler):
    def __init__(self, record=None):
        super().__init__()
        self.record = record

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        if self.record:
            self.record("redirect", {"from": req.full_url, "to": newurl, "status": code,
                                      "headers": list(headers.items())}, fp.read(MAX_RESPONSE + 1))
        parsed = urlsplit(newurl)
        if parsed.scheme != "https" or parsed.netloc != "certi.ssu.ac.kr":
            raise ValueError("학교 외부 리디렉션은 처리하지 않습니다")
        return super().redirect_request(req, fp, code, msg, headers, newurl)


class School:
    def __init__(self, cookie_header=None, trace_dir=None):
        if cookie_header is not None and (not cookie_header or "\r" in cookie_header or "\n" in cookie_header):
            raise ValueError("학교 세션 쿠키가 필요합니다")
        jar = CookieJar()
        self.jar = jar
        for part in (cookie_header or "").split(";"):
            name, separator, value = part.strip().partition("=")
            if separator:
                jar.set_cookie(Cookie(0, name, value, None, False, "certi.ssu.ac.kr", False,
                                      False, "/", True, True, None, True, None, None, {}))
        self.trace_dir = trace_dir
        self.trace_sequence = 0
        if trace_dir:
            trace_dir.mkdir(mode=0o700, parents=True, exist_ok=False)
        self.opener = build_opener(HTTPCookieProcessor(jar), SchoolOnlyRedirect(self.record))

    def record(self, kind, metadata, body):
        if self.trace_dir is None:
            return
        self.trace_sequence += 1
        prefix = self.trace_dir / f"{self.trace_sequence:04d}-{kind}"
        write_private(prefix.with_suffix(".json"), json.dumps(metadata).encode(), exclusive=True)
        write_private(prefix.with_suffix(".bin"), body, exclusive=True)

    def post(self, path, fields):
        return self.request("POST", path, fields)

    def request(self, method, path, fields=None):
        if not path.startswith("/") or path.startswith("//"):
            raise ValueError("학교 내부 경로가 필요합니다")
        body = urlencode(fields or {}).encode("utf-8") if method == "POST" else None
        self.record("request", {"path": path, "method": method}, body or b"")
        request = Request(SCHOOL + path, body,
                          {"Content-Type": "application/x-www-form-urlencoded", "Referer": SCHOOL + "/"})
        try:
            with self.opener.open(request, timeout=30) as response:
                body = response.read(MAX_RESPONSE + 1)
                self.record("response", {"url": response.url, "status": response.status,
                                         "headers": list(response.headers.items())}, body)
                if len(body) > MAX_RESPONSE:
                    raise ValueError("학교 응답 크기 초과")
                return body
        except HTTPError as error:
            self.record("http-error", {"status": error.code, "headers": list(error.headers.items())},
                        error.read(MAX_RESPONSE + 1))
            raise
        except Exception as error:
            self.record("transport-error", {"type": type(error).__name__}, b"")
            raise


class Checkout:
    def __init__(self, config, state_path, school, *, allow_approval=False, issue=None):
        self.fields = dict(config["fields"])
        self.approval_path = config.get("approval_path", APPROVAL_PATH)
        if self.approval_path not in (APPROVAL_PATH, "/wm1.0/jsp/pay/lg/smart/newcertres.jsp"):
            raise ValueError("확인하지 않은 학교 승인 경로")
        self.product = config.get("product", "재학증명서 PDF 1통")
        self.requires_phone = config.get("requires_phone", False)
        self.amount = int(self.fields["LGD_AMOUNT"])
        if self.amount <= 0 or not all(self.fields.get(k) for k in ("LGD_OID", "LGD_HASHDATA", "LGD_MID")):
            raise ValueError("학교가 생성한 유료 주문이 필요합니다")
        self.base = config["public_base"].rstrip("/")
        url = urlsplit(self.base)
        if url.scheme != "https" or not url.netloc or url.path or url.query or url.fragment:
            raise ValueError("공개 HTTPS 주소가 필요합니다")
        self.state_path = state_path
        self.school = school
        self.allow_approval = allow_approval
        self.issue = issue
        self.lock = threading.Lock()
        self.school_lock = threading.Lock()
        self.ticket = secrets.token_urlsafe(32)
        self.deadline = time.monotonic() + 1800
        self.status = {"status": "ready", "amount": self.amount, "approval_attempted": False,
                       "issue_attempted": False, "paid_verified": False, "pdf_ready": False}
        # 기존 상태를 덮어쓰면 승인 재시도로 이어질 수 있으므로 새 경로만 허용한다.
        write_private(state_path, json.dumps(self.status).encode(), exclusive=True)
        self.pdf_path = state_path.with_suffix(".pdf")

    def save(self):
        temporary = self.state_path.with_suffix(".tmp")
        write_private(temporary, json.dumps(self.status).encode())
        temporary.replace(self.state_path)

    def snapshot(self):
        with self.lock:
            return dict(self.status)

    def open(self, phone=""):
        with self.lock:
            if self.status["status"] != "ready":
                return None
            if self.requires_phone:
                if not re.fullmatch(r"010\d{8}", phone):
                    raise ValueError("010으로 시작하는 휴대폰 번호 11자리를 입력하세요")
                self.fields.update(BUYERTEL=phone, USERMOBILE1=phone[:3], USERMOBILE2=phone[3:7], USERMOBILE3=phone[7:])
            self.status["status"] = "checkout_opened"
            self.save()
        fields = dict(self.fields)
        fields.update(LGD_RETURNURL=self.base + "/result/" + self.ticket,
                      CST_WINDOW_TYPE="submit", LGD_CUSTOM_SWITCHINGTYPE="SUBMIT")
        return fields

    def receive(self, result):
        with self.lock:
            if self.status["status"] != "checkout_opened":
                return False
            if any(len(values) != 1 for values in result.values()):
                return False
            for name in ("LGD_OID", "LGD_AMOUNT", "LGD_MID"):
                if name in result and result[name][0] != self.fields[name]:
                    return False
            code = result.get("LGD_RESPCODE", [""])[0]
            key = result.get("LGD_PAYKEY", [""])[0]
            if code != "0000":
                if not code:
                    return False
                self.status.update(status="cancelled_or_failed", response_code=code[:16])
                self.save()
                return True
            if not key or len(key) > 4096:
                return False
            if not self.allow_approval:
                self.status["status"] = "authenticated_approval_disabled"
                self.save()
                return True
            write_private(self.state_path.with_suffix(".callback.json"), json.dumps(result).encode(), exclusive=True)
            # 이 상태를 네트워크 호출보다 먼저 기록한다. 타임아웃/중복 콜백에서도 다시 승인하지 않는다.
            self.status.update(status="approval_pending", approval_attempted=True)
            self.save()
        approval = dict(self.fields)
        approval.update(LGD_RESPCODE="0000", LGD_RESPMSG="", LGD_PAYKEY=key)
        try:
            with self.school_lock:
                response = self.school.post(self.approval_path, approval)
                # 승인 후 자동 제출 폼이 필요한 경우도 분석할 수 있도록 원래 응답을 비공개 보존한다.
                # HTTP 200 또는 응답 저장 성공은 결제/발급 완료를 의미하지 않는다.
                write_private(self.state_path.with_suffix(".approval-response.bin"), response, exclusive=True)
            with self.lock:
                self.status.update(status="approval_returned_unverified", approval_response_recorded=True)
                self.save()
        except Exception:
            with self.lock:
                self.status["status"] = "approval_unknown"
                self.save()
        self.reconcile()
        return True

    def issue_native(self, numbers):
        """결제 승인으로 보관함에 등록된 일반 출력 문서를 받아 PDF까지 이어서 만든다.

        학교 서버의 출력 완료 통지는 남은 매수를 소비하므로 주문 하나당 한 번만 시도한다.
        호출자는 school_lock을 쥐고 들어오며, 네트워크 작업 중에는 상태 잠금을 쥐지 않는다.
        """
        with self.lock:
            if self.issue is None:
                self.status["status"] = "issued_native_document_ready"
                self.save()
                return
            if self.status["issue_attempted"]:
                return
            # 네트워크 호출보다 먼저 기록한다. 중복 요청이나 재시작이 출력 매수를 또 쓰지 않게 한다.
            self.status.update(status="issue_pending", issue_attempted=True)
            self.save()
        try:
            pdf_path, metadata = self.issue(self.school, numbers)
            data = pdf_path.read_bytes()
        except Exception:
            # 어디까지 진행됐는지 알 수 없으므로 자동 재시도하지 않는다.
            with self.lock:
                self.status["status"] = "issue_unknown"
                self.save()
            return
        with self.lock:
            # 렌더링 결과가 PDF인지는 바이트로 확인한다. 결제 영수증 검증과는 구분한다.
            if not data.startswith(b"%PDF-"):
                self.status["status"] = "issued_document_format_unverified"
                self.save()
                return
            if not (metadata.get("print_completion_sent") is True
                    and metadata.get("server_verification_success") is True):
                self.status.update(status="issued_document_verification_pending", pdf_ready=False,
                                   print_completion_sent=bool(metadata.get("print_completion_sent")),
                                   server_verification_success=bool(metadata.get("server_verification_success")))
                self.save()
                return
            write_private(self.pdf_path, data)
            self.status.update(status="issued_pdf_ready", pdf_ready=True,
                               print_completion_sent=bool(metadata.get("print_completion_sent")),
                               server_verification_success=bool(metadata.get("server_verification_success")))
            self.save()

    def reconcile(self):
        with self.lock:
            if not self.status["approval_attempted"] or self.status["pdf_ready"]:
                return
        try:
            with self.school_lock:
                basket_fields = {key: self.fields[key] for key in ("GIWANNO", "LOGIN_GBN", "LOGIN_QRY_TYPE")
                                 if key in self.fields}
                native = self.fields.get("SERVICE_TYPE") == "INTERNET"
                basket_fields.update(SERVICE_TYPE="INTERNET" if native else "FILE", FILETYPE="" if native else "P",
                                     LEFTNUM="2" if native else "5", SUBNUM="1" if native else "2", TOP="SERVICE")
                source = self.school.post("/servlet/WMBASKET?COMMAND=BASKETLIST", basket_fields)
                basket = Basket(source.decode("utf-8", errors="replace"))
                numbers = set(filter(None, re.split(r"[@,$]", self.fields.get("MINNOS", ""))))
                numbers.update(filter(None, [self.fields.get("MINNO", "")]))
                if native:
                    if basket.has_native_document(numbers):
                        write_private(self.state_path.with_suffix(".native-basket.html"), source)
                        self.issue_native(numbers)
                    return
                index = basket.download_index(numbers)
                if index is None:
                    return
                download_fields = dict(basket.fields)
                download_fields.pop("COMMAND", None)
                download_fields.update(INDEX=index, SGTS=datetime.now(ZoneInfo("Asia/Seoul")).strftime("%Y%m%d%H%M%S"))
                data = self.school.post("/servlet/WMBASKET?COMMAND=CERTPDFDOWN", download_fields)
                # 이름이나 HTTP 200만으로 PDF라고 판단하지 않는다.
                if not data.startswith(b"%PDF-"):
                    with self.lock:
                        self.status["status"] = "issued_download_format_unverified"
                        self.save()
                    return
                write_private(self.pdf_path, data)
            with self.lock:
                # 보관함의 동일 문서와 실제 PDF 수신 확인. 결제 영수증 검증과는 구분한다.
                self.status.update(status="issued_pdf_ready", pdf_ready=True)
                self.save()
        except Exception:
            # 상태를 알 수 없으면 승인 재시도 대신 보관함 조회만 허용한다.
            return


def serve(checkout, port):
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def send(self, data, status=200, content_type="text/html; charset=utf-8"):
            self.send_response(status)
            for name, value in [("Content-Type", content_type), ("Content-Length", str(len(data))),
                                ("Cache-Control", "no-store"), ("Referrer-Policy", "no-referrer"),
                                ("X-Content-Type-Options", "nosniff")]:
                self.send_header(name, value)
            self.end_headers()
            self.wfile.write(data)

        def page(self, body, status=200):
            self.send(("<!doctype html><html lang=ko><meta charset=utf-8><title>증명서 결제</title>"
                       "<meta name=referrer content=no-referrer><body>" + body + "</body></html>").encode(), status)

        def valid(self, prefix):
            return self.path == prefix + checkout.ticket and time.monotonic() < checkout.deadline

        def do_GET(self):
            if self.valid("/status/"):
                self.send(json.dumps(checkout.snapshot()).encode(), content_type="application/json")
            elif self.valid("/download/") and checkout.snapshot()["pdf_ready"]:
                self.send(checkout.pdf_path.read_bytes(), content_type="application/pdf")
            elif self.valid("/checkout/"):
                notice = ("토스페이먼츠에서 직접 인증하면 증명서 발급 결제를 승인합니다."
                          if checkout.allow_approval else "결제 연결 시험입니다. 승인 요청은 보내지 않습니다.")
                contact = ('<label>긴급연락처 <input name=phone type=tel autocomplete=tel '
                           'pattern="010[0-9]{8}" placeholder="01012345678" required></label>'
                           if checkout.requires_phone else "")
                self.page(f"<p>{escape(checkout.product)} · {checkout.amount:,}원</p>"
                          f"<p>{notice}</p>"
                          + ("<p>일반 출력 주문은 학교 출력 데이터를 PDF로 변환합니다. 전자서명 PDF 상품은 아닙니다. "
                             "유료 발급의 전체 연결은 아직 실거래 검증 전이며, 실패하면 추가 결제하지 마세요.</p>"
                             if checkout.fields.get("SERVICE_TYPE") == "INTERNET" else "")
                          +
                          f'<form method=post action="/start/{checkout.ticket}">{contact}<button>결제창 열기</button></form>')
            else:
                self.page("링크가 없거나 만료되었습니다.", 404)

        def do_POST(self):
            prefix = next((p for p in ("/start/", "/result/", "/refresh/") if self.valid(p)), None)
            if prefix is None:
                self.page("링크가 없거나 만료되었습니다.", 404)
                return
            try:
                length = int(self.headers.get("Content-Length", "0"))
                if not 0 <= length <= 65536:
                    raise ValueError()
            except ValueError:
                self.page("요청 크기 오류", 400)
                return
            data = self.rfile.read(length)
            if prefix == "/start/":
                inputs = parse_qs(data.decode("utf-8", errors="replace"))
                try:
                    fields = checkout.open(inputs.get("phone", [""])[0])
                except ValueError as error:
                    self.page(escape(str(error)), 400)
                    return
                if fields is None:
                    self.page("이미 사용한 결제 링크입니다.", 409)
                    return
                inputs = "".join(f'<input type=hidden name="{escape(k, quote=True)}" value="{escape(str(v), quote=True)}">'
                                 for k, v in fields.items())
                self.page('<form id=pay method=post action="https://checkout.tosspayments.com/xpay/Request.do">'
                          + inputs + '<button>결제창으로 이동</button></form>'
                          '<script>document.getElementById("pay").submit()</script>')
                return
            if prefix == "/result/":
                self_callback = {"headers": list(self.headers.items()), "method": "POST"}
                if hasattr(checkout.school, "record"):
                    checkout.school.record("browser-callback", self_callback, data)
                if not checkout.receive(parse_qs(data.decode("utf-8", errors="replace"), keep_blank_values=True)):
                    self.page("주문과 맞지 않거나 이미 처리한 응답입니다.", 409)
                    return
            else:
                checkout.reconcile()
            state = checkout.snapshot()
            if state["pdf_ready"]:
                self.page(f'<p>발급 파일을 받았습니다.</p><a href="/download/{checkout.ticket}">PDF 다운로드</a>')
            elif state["status"] == "cancelled_or_failed":
                self.page("결제가 취소되었거나 인증되지 않았습니다. 승인 요청을 보내지 않았습니다.")
            elif state["status"] == "issued_native_document_ready":
                self.page("발급 보관함에서 증명서를 확인했습니다. 원본 문서 데이터의 서버 처리가 남아 있습니다.")
            elif state["status"] in ("issue_pending", "issue_unknown"):
                self.page("결제는 처리되었고 증명서 문서 생성 결과를 확인하지 못했습니다. "
                          "다시 결제하지 말고 서버 기록을 확인하세요.")
            else:
                recorded = ("승인 응답 원문은 서버에 보존했습니다. "
                            if state.get("approval_response_recorded") else "")
                self.page(f"<p>{recorded}발급 완료를 아직 확인하지 못했습니다. 다시 결제하지 말고 상태를 확인하세요.</p>"
                          f'<form method=post action="/refresh/{checkout.ticket}"><button>발급 상태 확인</button></form>')

    ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--form", type=Path, required=True)
    parser.add_argument("--state", type=Path, required=True)
    parser.add_argument("--port", type=int, default=38127)
    parser.add_argument("--allow-approval", action="store_true")
    parser.add_argument("--consume-form", action="store_true", help="설정을 메모리에 읽은 뒤 입력 파일을 삭제")
    parser.add_argument("--exe", type=Path, help="일반 출력 문서 복호화에 사용할 REPORTX 실행 파일")
    parser.add_argument("--fonts", type=Path, help="PDF 렌더링에 사용할 공개 글꼴 디렉터리")
    parser.add_argument("--system-ip", default="", help="출력 완료 통지에 사용할 실행 서버 IP")
    args = parser.parse_args()
    config = json.loads(args.form.read_text())
    issue = None
    if args.exe or args.fonts:
        # 결제 승인 뒤 PDF까지 이어서 만들려면 복호화 실행 파일과 글꼴이 모두 필요하다.
        if not (args.exe and args.fonts):
            parser.error("--exe와 --fonts는 함께 지정해야 합니다")
        if not args.system_ip:
            parser.error("PDF 발급 완료와 원본확인에는 --system-ip가 필요합니다")
        import ipaddress
        ipaddress.ip_address(args.system_ip)
        if not args.exe.is_file() or not all((args.fonts / name).is_file() for name in (
                "NanumMyeongjo-Regular.ttf", "NanumMyeongjo-Bold.ttf",
                "NanumGothicCoding-Regular.ttf", "NanumGothicCoding-Bold.ttf")):
            parser.error("REPORTX 실행 파일 또는 PDF 글꼴이 없습니다")
        from certificate_document import issuer

        issue = issuer(exe=args.exe, fonts=args.fonts, output_dir=args.state.parent,
                       system_ip=args.system_ip)
    checkout = Checkout(config, args.state,
                        School(config["cookie_header"], args.state.with_suffix(".http-trace")),
                        allow_approval=args.allow_approval, issue=issue)
    if args.consume_form:
        args.form.unlink()
    print(json.dumps({"checkout_url": checkout.base + "/checkout/" + checkout.ticket}), flush=True)
    serve(checkout, args.port)


if __name__ == "__main__":
    main()
