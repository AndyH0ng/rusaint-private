"""학교 세션으로 발급된 일반 출력 증명서 1건을 받아 서버에서 PDF로 렌더링한다.

결제 여부와 무관하게 "이미 발급된 문서"만 처리한다. 신청·결제·승인은 이 모듈에서 하지 않는다.
출력 완료 통지는 남은 매수를 소비하므로 문서별 출력 디렉터리를 배타적으로 만들어 한 번만 실행한다.
"""

import base64
import ipaddress
import json
import re
from urllib.parse import urlsplit
import zlib

from certificate_checkout import Basket, write_private
from certificate_probe import ReportCipher
from certificate_render import render

GIWANNO = "021163"
NATIVE_BASKET_FIELDS = {"SERVICE_TYPE": "INTERNET", "GIWANNO": GIWANNO,
                        "LEFTNUM": "2", "SUBNUM": "1", "TOP": "SERVICE"}
DOCUMENT_PATHS = {"URLFile": "/wm1.0/jsp/report/sendfile.jsp",
                  "URLCheck": "/wm1.0/jsp/report/senddocno.jsp",
                  "URLPost": "/wm1.0/jsp/report/printcomplete.jsp"}
MAX_DOCUMENT = 32 * 1024 * 1024


def validate_system_ip(value, *, required=False):
    """상태 파일이나 출력 세션을 만들기 전에 실행 서버 IP 설정을 검증한다."""
    if not value:
        if required:
            raise ValueError("서버의 실제 출력 IP가 설정되지 않았습니다")
        return None
    try:
        return str(ipaddress.ip_address(value))
    except ValueError:
        raise ValueError("서버의 출력 IP 형식이 올바르지 않습니다") from None


def verified(metadata):
    return (metadata.get("print_completion_sent") is True
            and metadata.get("server_verification_success") is True)


def native_previews(school):
    """일반 출력 보관함의 출력 가능한 발급 내역을 읽는다."""
    source = school.post("/servlet/WMBASKET?COMMAND=BASKETLIST", dict(NATIVE_BASKET_FIELDS))
    return Basket(source.decode("utf-8", errors="replace")).previews(), source


def select(previews, *, minno=None, title=None):
    """문서 번호 또는 종류로 발급 내역 1건을 고른다. 모호하면 진행하지 않는다."""
    matched = [preview for preview in previews
               if (minno is None or preview["minno"] in (minno if isinstance(minno, (set, frozenset)) else {minno}))
               and (title is None or preview["job"] == title)]
    if len(matched) != 1:
        raise ValueError(f"발급 내역 1건을 식별하지 못했습니다 (후보 {len(matched)}건)")
    preview = matched[0]
    if preview["printed"] >= preview["count"]:
        raise ValueError("남은 출력 매수가 없습니다")
    return preview


def issue(school, preview, *, exe, fonts, output_dir, system_ip=None):
    """문서 수신·복호화·렌더링을 수행하고, system_ip가 있으면 출력 완료까지 통지한다."""
    system_ip = validate_system_ip(system_ip)
    # 재실행이면 여기서 FileExistsError가 나고 출력 매수를 다시 소비하지 않는다.
    output_dir.mkdir(mode=0o700, parents=True, exist_ok=False)
    minno = preview["minno"]
    check = school.post("/servlet/WMBASKET?COMMAND=EXECHECK",
                        {"MINNO": minno, "GB": preview["printgb"], "AB": preview["printable"]})
    tpid = re.search(rb"opener\.preview1\('[^']*','[^']*','([^']*)'", check)
    if tpid is None:
        raise ValueError("출력 세션 식별자를 찾지 못했습니다")
    response = json.loads(school.post("/servlet/WMBASKET", {
        "COMMAND": "PREVIEWENC", "TYPE": "INTERNET", "JOBNE": preview["job"], "MINNO": minno,
        "TPID": tpid[1].decode(), "CNT": str(preview["count"]), "RECEICNT": str(preview["printed"]),
        "KORYN": preview["korean"], "PRINTGB": preview["printgb"], "PRINTABLE": preview["printable"],
        "RECTYPE": "WEB", "GIWANNO": GIWANNO, "TOP": "50", "LEFT": "50"}))
    cipher = ReportCipher(exe)
    launch = cipher.decrypt(base64.b64decode(response["res"], validate=True), cipher.launch_password())
    write_private(output_dir / "launch.bin", launch, exclusive=True)
    params = dict(item.split("=", 1) for item in urlsplit(launch.decode("cp949")).query.split("|") if "=" in item)
    if params.get("MINNO") != minno:
        raise ValueError("수신한 문서 번호가 발급 보관함과 다릅니다")
    for key, path in DOCUMENT_PATHS.items():
        if params.get(key) != "certi.ssu.ac.kr" + path:
            raise ValueError("확인하지 않은 문서 서버 경로")
    encrypted = school.post(DOCUMENT_PATHS["URLFile"],
                            {"TPID": params["TPID"], "MIN_NO": minno, "GIWAN_NO": GIWANNO})
    inflater = zlib.decompressobj()
    document = inflater.decompress(cipher.decrypt(encrypted, minno.encode("ascii")), MAX_DOCUMENT + 1)
    if len(document) > MAX_DOCUMENT or not inflater.eof:
        raise ValueError("문서 압축 해제 한도 초과 또는 불완전한 응답")
    document_path = output_dir / "document.bin"
    write_private(document_path, document, exclusive=True)
    code = school.post(DOCUMENT_PATHS["URLCheck"],
                       {"MIN_NO": minno, "RECEIVE_TYPE": params["RECEIVE_TYPE"],
                        "RECEIVE_TARGET": params.get("RECEIVE_TARGET", "")}).rstrip(b"\0\r\n ").decode("ascii")
    if not re.fullmatch(r"[A-Za-z0-9]{16}", code):
        raise ValueError("문서확인번호를 발급받지 못했습니다")
    write_private(output_dir / "docno.txt", code.encode(), exclusive=True)
    pdf_path = output_dir / "certificate.pdf"
    metadata, _ = render(document_path, fonts, pdf_path, code)
    metadata.update(document_number=minno, certificate=preview["job"],
                    server_verification_success=False, print_completion_sent=False)
    if system_ip:
        # 파일이 완성된 뒤 한 번만 통지한다. 통지 없이는 서버 원본확인이 실패한다.
        write_private(output_dir / "completion-attempted.json", b'{"attempted":true}', exclusive=True)
        school.post(DOCUMENT_PATHS["URLPost"],
                    {"TPID": params["TPID"], "SYSTEM_IP": system_ip, "P_MODEL": "rusaint PDF renderer",
                     "MIN_DOC_NO": code, "RECEIVE_TYPE": params["RECEIVE_TYPE"]})
        metadata["print_completion_sent"] = True
        verification = json.loads(school.post("/servlet/WMVERIFY?COMMAND=GETPRINTVIEWEXE",
                                              {"DOC_NO": code, "GIWANNO": GIWANNO}))
        write_private(output_dir / "verification.json", json.dumps(verification).encode(), exclusive=True)
        metadata["server_verification_success"] = (verification.get("SUCC") == "Y"
                                                   and verification.get("MIN_NO") == minno)
    metadata["status"] = "verified" if verified(metadata) else "verification_pending"
    write_private(output_dir / "metadata.json", json.dumps(metadata).encode(), exclusive=True)
    return pdf_path, metadata


def issuer(*, exe, fonts, output_dir, system_ip=None):
    """결제 승인 뒤 호출할 발급 함수를 만든다. 문서 번호 집합 중 1건만 처리한다."""
    def run(school, numbers):
        # 보관함 원문은 호출자가 이미 보존한다. 여기서는 발급 내역만 다시 읽는다.
        previews, _ = native_previews(school)
        preview = select(previews, minno=frozenset(numbers))
        return issue(school, preview, exe=exe, fonts=fonts,
                     output_dir=output_dir / "document", system_ip=system_ip)
    return run
