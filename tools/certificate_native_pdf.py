# /// script
# requires-python = ">=3.11"
# dependencies = ["pefile==2024.8.26", "unicorn==2.1.4", "lxml==6.1.3", "reportlab==5.1.0", "pillow==13.0.0"]
# ///
"""학교 세션에서 이미 발급된 일반 출력 증명서를 받아 서버 PDF로 렌더링한다.

uv run tools/certificate_native_pdf.py --session private/session.json --exe REPORTX.exe \
    --fonts fonts --output-dir private/output --title 장학금지급증명서
--title 또는 --minno로 보관함의 발급 내역 1건을 지정한다. 신청·결제는 실행하지 않는다.
--complete를 지정하면 PDF를 저장한 뒤 원래 문서의 출력 완료를 한 번 알리고 원본확인을 조회한다.
"""

import argparse
import json
from pathlib import Path

from certificate_checkout import School, write_private
from certificate_document import issue, native_previews, select


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--session", type=Path, required=True)
    parser.add_argument("--exe", type=Path, required=True)
    parser.add_argument("--fonts", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--minno", help="보관함의 문서 번호")
    parser.add_argument("--title", help="증명서 종류 이름")
    parser.add_argument("--complete", action="store_true")
    parser.add_argument("--system-ip", default="")
    args = parser.parse_args()
    if not (args.minno or args.title):
        parser.error("--minno 또는 --title로 발급 내역을 지정하세요")
    if args.complete and not args.system_ip:
        parser.error("출력 완료에는 실제 실행 서버의 IP가 필요합니다")
    args.output_dir.mkdir(mode=0o700, parents=True, exist_ok=False)
    school = School(json.loads(args.session.read_text())["cookie_header"], args.output_dir / "http-trace")
    previews, source = native_previews(school)
    write_private(args.output_dir / "native-basket.html", source, exclusive=True)
    preview = select(previews, minno=args.minno, title=args.title)
    _, metadata = issue(school, preview, exe=args.exe, fonts=args.fonts,
                        output_dir=args.output_dir / "document",
                        system_ip=args.system_ip if args.complete else None)
    print(json.dumps(metadata))


if __name__ == "__main__":
    main()
