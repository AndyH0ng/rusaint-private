# /// script
# requires-python = ">=3.11"
# dependencies = ["pefile==2024.8.26", "unicorn==2.1.4", "lxml==6.1.3", "reportlab==5.1.0", "pillow==13.0.0"]
# ///
"""rusaint-mcp 전용 JSON-lines worker. 자격 증명은 stdin으로만 받고 stdout은 JSON 전용이다."""

import base64
from html.parser import HTMLParser
import json
from pathlib import Path
import re
import sys
import time
from urllib.parse import urlencode, urlsplit
import uuid

from certificate_checkout import Basket, School, write_private
from certificate_document import issue, select, validate_system_ip, verified

COMMON = {"GIWANNO": "021163", "LOGIN_GBN": "2", "LOGIN_QRY_TYPE": "P", "GP": "2",
          "INNER_QRY_YN": "N", "POST_SERVICE_YN": "Y", "BROWSER": "NETSCAPE", "OS": "win"}
KINDS = {"scholarship": "장학금지급증명서", "enrollment": "재학증명서"}


class Forms(HTMLParser):
    def __init__(self, source):
        super().__init__(convert_charrefs=True)
        self.forms = []
        self.current = None
        self.select = None
        self.textarea = None
        self.feed(source)

    def handle_starttag(self, tag, attrs):
        a = dict(attrs)
        if tag == "form":
            self.current = {"action": a.get("action", ""), "fields": {}, "selects": {}, "id": a.get("id", "")}
            self.forms.append(self.current)
        if self.current is None:
            return
        if tag == "input" and a.get("name"):
            if a.get("type") not in ("checkbox", "radio") or "checked" in a:
                self.current["fields"][a["name"]] = a.get("value", "")
        elif tag == "select" and a.get("name"):
            self.select = a["name"]
            self.current["selects"][self.select] = []
        elif tag == "option" and self.select:
            value = a.get("value", "")
            self.current["selects"][self.select].append(value)
            if self.select not in self.current["fields"] or "selected" in a:
                self.current["fields"][self.select] = value
        elif tag == "textarea" and a.get("name"):
            self.textarea = a["name"]
            self.current["fields"][self.textarea] = ""

    def handle_endtag(self, tag):
        if tag == "form":
            self.current = None
        elif tag == "select":
            self.select = None
        elif tag == "textarea":
            self.textarea = None

    def handle_data(self, data):
        if self.current and self.textarea:
            self.current["fields"][self.textarea] += data


def first_form(source):
    forms = Forms(source).forms
    if not forms:
        raise ValueError("학교 응답에서 신청 양식을 찾지 못했습니다")
    return forms[0]


def internal_path(value):
    u = urlsplit(value)
    if u.netloc and u.netloc != "certi.ssu.ac.kr":
        raise ValueError("학교 외부 주소는 처리할 수 없습니다")
    if u.scheme and u.scheme not in ("http", "https"):
        raise ValueError("지원하지 않는 주소입니다")
    if not u.path.startswith("/") or u.path.startswith("//"):
        raise ValueError("학교 내부 주소가 필요합니다")
    return u.path + ("?" + u.query if u.query else "")


class Worker:
    def __init__(self):
        self.school = None
        self.root = None
        self.assets = None
        self.system_ip = ""

    def init(self, request):
        self.root = Path(request["work_dir"])
        self.root.mkdir(mode=0o700, parents=True, exist_ok=True)
        self.assets = Path(request["assets"])
        self.system_ip = request.get("system_ip", "")
        token, identity, address = request["token"], request["student_id"], request["client_ip"]
        if any(c in token for c in ";\r\n") or len(token) > 4096 or not re.fullmatch(r"[A-Za-z0-9]{1,20}", identity):
            raise ValueError("학교 인증 정보 형식 오류")
        import ipaddress
        address = str(ipaddress.ip_address(address)).replace(".", "%2E").replace(":", "%3A")
        # SafeAgent는 토큰뿐 아니라 SmartID가 설정한 sAddr 쿠키도 사용한다.
        self.school = School("sToken=" + token + "; sAddr=" + address)
        source = self.school.request("GET", "/servlet/WMLOGIN?" + urlencode({"sToken": token, "sIdno": identity})).decode("utf-8")
        for _ in range(4):
            if "javascript:logout();" in source:
                return {"authenticated": True}
            form = first_form(source)
            if form["fields"].get("SUBMITURL"):
                path = internal_path(form["fields"]["SUBMITURL"])
                if path not in ("/servlet/WMLOGIN?COMMAND=PORTALWAITING", "/servlet/WMLOGIN?COMMAND=SSOLOGINNEW"):
                    raise ValueError("학교 로그인 절차가 변경되었습니다")
            elif re.search(r"var error_code\s*=\s*0\s*;", source):
                path = "/servlet/WMLOGIN?COMMAND=SSOLOGINNEW"
            else:
                raise ValueError("증명서 로그인이 만료되었거나 거부됐습니다. 커넥터를 다시 연결해 주세요")
            source = self.school.post(path, form["fields"]).decode("utf-8")
        raise ValueError("증명서 로그인 단계가 끝나지 않았습니다")

    def native_basket(self):
        fields = dict(COMMON, SERVICE_TYPE="INTERNET", LEFTNUM="2", SUBNUM="1", TOP="SERVICE")
        source = self.school.post("/servlet/WMBASKET?COMMAND=BASKETLIST", fields).decode("utf-8")
        if "javascript:logout();" not in source:
            raise ValueError("증명서 세션이 만료됐습니다. 커넥터를 다시 연결해 주세요")
        return {preview["minno"]: preview for preview in Basket(source).previews()}

    def list(self, _):
        return {"items": [{"document_id": key, "name": v["job"], "copies": v["count"],
                            "printed": v["printed"], "available": v["count"] > v["printed"]}
                           for key, v in self.native_basket().items()]}

    def pdf(self, request):
        system_ip = validate_system_ip(self.system_ip, required=True)
        identity = request["document_id"]
        cache = self.root / ("pdf-" + self.safe_id(identity))
        if (cache / "metadata.json").exists():
            metadata = json.loads((cache / "metadata.json").read_text())
            if verified(metadata):
                return self.pdf_result(cache)
            return {"status": "verification_pending", "metadata": metadata,
                    "message": "PDF 원본확인이 완료되지 않았습니다. 재출력하지 않고 상태 확인이 필요합니다"}
        preview = select(list(self.native_basket().values()), minno=identity)
        if cache.exists():
            raise ValueError("이 문서의 이전 처리 상태를 확인해야 합니다. 출력 완료를 반복 요청하지 않습니다")
        _, metadata = issue(self.school, preview, exe=self.assets / "REPORTX.exe",
                            fonts=self.assets / "fonts", output_dir=cache, system_ip=system_ip)
        if not verified(metadata):
            return {"status": "verification_pending", "metadata": metadata,
                    "message": "PDF 원본확인이 완료되지 않았습니다. 재출력하지 않고 상태 확인이 필요합니다"}
        return self.pdf_result(cache)

    @staticmethod
    def safe_id(identity):
        if not re.fullmatch(r"[A-Za-z0-9_-]{1,100}", identity):
            raise ValueError("문서 식별자 형식 오류")
        return identity

    @staticmethod
    def pdf_result(cache):
        data = (cache / "certificate.pdf").read_bytes()
        if len(data) > 10 * 1024 * 1024:
            raise ValueError("PDF 크기 한도 초과")
        return {"pdf_base64": base64.b64encode(data).decode(), "metadata": json.loads((cache / "metadata.json").read_text())}

    def poll_free(self, ledger):
        """보관함만 재조회한다. 발급 요청은 여기서 반복하지 않는다."""
        state = json.loads(ledger.read_text())
        reason = "not_visible"
        for attempt in range(4):
            if attempt:
                time.sleep(1)
            try:
                after = self.native_basket()
            except Exception:
                reason = "lookup_failed"
                continue
            documents = [key for key, value in after.items()
                         if key not in state["before"] and value["job"] == state["name"]]
            if len(documents) == 1:
                result = {"status": "issued", "amount": 0, "document_id": documents[0], "name": state["name"]}
                state.update(result)
                write_private(ledger, json.dumps(state).encode())
                return result
            reason = "ambiguous" if documents else "not_visible"
        state.update(status="issuance_pending", reason=reason)
        write_private(ledger, json.dumps(state).encode())
        return {"status": "issuance_pending", "amount": 0, "name": state["name"], "reason": reason,
                "message": "이미 발급됐거나 처리 중일 수 있습니다. 다시 신청하지 말고 발급 목록을 확인하세요"}

    def submit_free(self, kind, fields):
        ledger = self.root / ("free-" + kind + ".json")
        if ledger.exists() and json.loads(ledger.read_text())["status"] == "issuance_pending":
            return self.poll_free(ledger)
        state = {"status": "issuance_pending", "name": KINDS[kind], "before": list(self.native_basket())}
        # 요청이 타임아웃돼도 다음 호출은 발급 재요청 없이 목록만 확인한다.
        write_private(ledger, json.dumps(state).encode())
        try:
            self.school.post("/servlet/WMPAY?COMMAND=NEWPAYMENTFREE", fields)
        except Exception:
            pass
        return self.poll_free(ledger)

    def prepare(self, request):
        kind = request["kind"]
        if kind not in KINDS:
            raise ValueError("scholarship 또는 enrollment만 지원합니다")
        ledger = self.root / ("free-" + kind + ".json")
        if ledger.exists() and json.loads(ledger.read_text())["status"] == "issuance_pending":
            return self.poll_free(ledger)
        fields = dict(COMMON, SERVICE_TYPE="INTERNET", LEFTNUM="1", SUBNUM="1", TOP="SERVICE", FILETYPE="")
        source = self.school.post("/servlet/WMMEMBER?COMMAND=GIWANLIST", fields).decode("utf-8")
        form = first_form(source)
        fields = form["fields"]
        if "Inner_hakbun0" not in fields or "Inner_hakbun1" in fields:
            raise ValueError("학적이 없거나 여러 개입니다. 학적 선택을 별도로 지원해야 합니다")
        mapping = {"HAKBUN": "hakbun", "HAKWI": "hakwi", "HAKWI2": "hakwi2", "HAKWA": "hakwa", "HAKWA2": "hakwa2",
                   "KNAME": "kname", "ENAME": "ename", "SANGTAE": "sangtae", "SANGTAENO": "sangtae_no", "JOBSUBGPCD": "job_subgp_cd"}
        fields.update({name: fields.get("Inner_" + source_name + "0", "") for name, source_name in mapping.items()})
        fields.update(KORENG="K", sel_hakwi="1")
        fields.pop("COMMAND", None)
        waiting = self.school.post(internal_path(form["action"]), fields).decode("utf-8")
        # CERTWAITING은 서버 작업이 끝나면 읽기 전용 목록으로 이동한다.
        source = self.school.post("/servlet/WMCERT?COMMAND=CERTILIST", fields).decode("utf-8")
        for _ in range(4):
            if f'value="{KINDS[kind]}"' in source or f"value='{KINDS[kind]}'" in source:
                break
            time.sleep(1)
            source = self.school.post("/servlet/WMCERT?COMMAND=CERTILIST", fields).decode("utf-8")
        form = first_form(source)
        fields = form["fields"]
        selected = next((re.search(r"\d+$", name).group() for name, value in fields.items()
                         if name.startswith("job_ne") and value == KINDS[kind]), None)
        if selected is None:
            raise ValueError("이 학적에서 신청 가능한 증명서가 아닙니다")
        if kind == "scholarship":
            for name, value in [("Inner_JANG_YEAR", request.get("start")), ("Inner_JANG_YEART", request.get("end"))]:
                if value not in form["selects"].get(name + selected, []):
                    raise ValueError("장학금 기간 선택이 올바르지 않습니다")
                fields[name + selected] = value
        names = ["job_subgp_cd", "job_no", "job_ne", "ko_en_doc", "in_out_user", "program_id", "SYSTEM_PARAM", "minwoncount",
                 "Inner_SUNGJUKPYOGI", "Inner_KYOUK_YEAR", "Inner_JANG_YEAR", "Inner_JANG_YEART", "Inner_JUMIN_PYOGI", "Inner_ARITHMETIC"]
        fields["HTEMPSUM"] = "⌒" + "∥".join(fields.get(name + selected, " ") for name in names) + "∥"
        fields["issue" + selected] = "issue" + selected
        fields.pop("COMMAND", None)
        source = self.school.post(internal_path(form["action"]), fields).decode("utf-8")
        form = first_form(source)
        if KINDS[kind] not in source or "MINNO1" in form["fields"]:
            raise ValueError("다른 신청이 섞여 있어 진행할 수 없습니다")
        source = self.school.post("/servlet/WMCERT?COMMAND=POSTGAJUBSU", form["fields"]).decode("utf-8")
        for _ in range(8):
            if 'gointernetfree();' in source or 'gointernet();' in source:
                break
            time.sleep(1)
            fields = first_form(source)["fields"]
            fields.pop("COMMAND", None)
            source = self.school.post("/servlet/WMBASKET?COMMAND=POSTBASKETLIST", fields).decode("utf-8")
        form = first_form(source)
        fields = form["fields"]
        fields.pop("COMMAND", None)
        if "javascript:gointernetfree();" in source:
            if kind != "scholarship":
                raise ValueError("아직 검증하지 않은 무료 증명서입니다")
            return self.submit_free(kind, fields)
        source = self.school.post("/servlet/WMPAY?COMMAND=NEWPAYMENT", fields).decode("utf-8")
        forms = Forms(source).forms
        payment = next((f for f in forms if "LGD_AMOUNT" in f["fields"]), None)
        if payment is None or int(payment["fields"]["LGD_AMOUNT"]) <= 0:
            raise ValueError("유료 결제 양식을 확인하지 못했습니다")
        identity = uuid.uuid4().hex
        order = {"fields": payment["fields"], "name": KINDS[kind], "status": "prepared",
                 "amount": int(payment["fields"]["LGD_AMOUNT"]), "approval_attempted": False}
        write_private(self.root / ("order-" + identity + ".json"), json.dumps(order).encode(), exclusive=True)
        return {"amount": order["amount"], "order_id": identity, "name": order["name"]}


def main():
    worker = Worker()
    for line in sys.stdin:
        try:
            request = json.loads(line)
            method = request.pop("method")
            if method not in ("init", "list", "pdf", "prepare"):
                raise ValueError("지원하지 않는 증명서 명령")
            result = getattr(worker, method)(request)
            print(json.dumps({"ok": True, "result": result}, ensure_ascii=False), flush=True)
        except Exception as error:
            # urllib 예외에는 토큰이 든 URL이 포함될 수 있으므로 그대로 출력하지 않는다.
            message = str(error) if isinstance(error, ValueError) else "증명서 처리 중 오류가 발생했습니다 (" + type(error).__name__ + ")"
            print(json.dumps({"ok": False, "error": message}, ensure_ascii=False), flush=True)


if __name__ == "__main__":
    main()
