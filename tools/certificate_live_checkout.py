# /// script
# requires-python = ">=3.11"
# dependencies = ["pefile==2024.8.26", "unicorn==2.1.4", "lxml==6.1.3", "reportlab==5.1.0", "pillow==13.0.0"]
# ///
"""비공개 stdin 설정으로 학교 로그인부터 일반 출력 결제와 PDF 수신까지 연결한다."""

import json
from pathlib import Path
import sys

from certificate_checkout import Checkout, serve
from certificate_document import issuer
from certificate_worker import Worker


def main():
    config = json.loads(sys.stdin.readline())
    worker = Worker()
    assets = Path(config["assets"])
    for path in [assets / "REPORTX.exe", *(assets / "fonts" / name for name in (
            "NanumMyeongjo-Regular.ttf", "NanumMyeongjo-Bold.ttf",
            "NanumGothicCoding-Regular.ttf", "NanumGothicCoding-Bold.ttf"))]:
        if not path.is_file():
            raise ValueError("PDF 생성에 필요한 실행 파일이나 글꼴이 없습니다")
    import ipaddress
    ipaddress.ip_address(config["system_ip"])
    worker.init(config)
    worker.school.trace_dir = worker.root / "http-trace"
    worker.school.trace_dir.mkdir(mode=0o700, exist_ok=False)
    prepared = worker.prepare({"kind": "enrollment"})
    # 이 시점에는 학교에 미결제 주문이 존재한다. 금액 불일치 시 승인하지 않으며
    # 남은 미결제 주문은 학교 안내의 자동 취소 대상이다.
    if prepared["amount"] != 1000:
        raise ValueError("확인된 최저 금액 1,000원과 다릅니다. 결제 연결을 중단합니다")
    order = json.loads((worker.root / ("order-" + prepared["order_id"] + ".json")).read_text())
    checkout = Checkout({"fields": order["fields"], "public_base": config["public_base"],
                         "approval_path": "/wm1.0/jsp/pay/lg/smart/newcertres.jsp",
                         "product": "재학증명서 1통 · 서버에서 PDF 생성", "requires_phone": True},
                        worker.root / ("checkout-" + prepared["order_id"] + ".json"),
                        worker.school, allow_approval=True,
                        issue=issuer(exe=assets / "REPORTX.exe", fonts=assets / "fonts",
                                     output_dir=worker.root / prepared["order_id"],
                                     system_ip=config["system_ip"]))
    print(json.dumps({"amount": prepared["amount"],
                      "checkout_url": checkout.base + "/checkout/" + checkout.ticket}), flush=True)
    serve(checkout, config.get("port", 38131))


if __name__ == "__main__":
    main()
