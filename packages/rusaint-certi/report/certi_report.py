# /// script
# requires-python = ">=3.11"
# dependencies = ["pefile==2024.8.26", "unicorn==2.1.4", "lxml==6.1.3", "reportlab==5.1.0", "pillow==13.0.0"]
# ///
"""증명서 문서의 복호화와 PDF 렌더링만 담당하는 필터.

네트워크에 접근하지 않고, 학교 세션·주문·결제 상태를 알지 못한다. 입력은 파일, 출력은 파일이며
표준출력에는 JSON 한 줄만 쓴다. 호출 순서와 학교 통신은 rusaint-certi(Rust)가 책임진다.

  uv run certi_report.py decrypt --exe REPORTX.exe --mode launch    --input in.bin --output out.bin
  uv run certi_report.py decrypt --exe REPORTX.exe --mode document  --input in.bin --output out.bin --document-number 2026ABCD
  uv run certi_report.py render  --document doc.bin --document-code 0123456789ABCDEF --output cert.pdf \\
      --font NanumMyeongjo-Regular=/path/NanumMyeongjo.ttf --font ... (네 가지 모두)

decrypt 의 document 모드는 복호화한 뒤 압축까지 풀어 FastReport 문서를 그대로 돌려준다.
"""

import argparse
import json
import os
from pathlib import Path
import sys
import zlib

from cipher import ReportCipher
from render import render

MAX_DOCUMENT = 32 * 1024 * 1024


def write_private(path, data):
    """0600 으로만 만든다. 증명서 문서와 중간 결과는 개인정보다."""
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, "wb") as stream:
        stream.write(data)


def decrypt(args):
    encrypted = args.input.read_bytes()
    if not encrypted:
        raise ValueError("복호화할 입력이 비어 있습니다")
    cipher = ReportCipher(args.exe)
    if args.mode == "launch":
        plain = cipher.decrypt(encrypted, cipher.launch_password())
    else:
        if not args.document_number:
            raise ValueError("document 모드에는 --document-number 가 필요합니다")
        compressed = cipher.decrypt(encrypted, args.document_number.encode("ascii"))
        inflater = zlib.decompressobj()
        plain = inflater.decompress(compressed, MAX_DOCUMENT + 1)
        if len(plain) > MAX_DOCUMENT or not inflater.eof:
            raise ValueError("문서 압축 해제 한도 초과 또는 불완전한 응답")
    write_private(args.output, plain)
    return {"bytes": len(plain)}


def render_document(args):
    faces = {}
    for item in args.font:
        name, separator, path = item.partition("=")
        if not separator or not name or not path:
            raise ValueError("--font 는 이름=경로 형식입니다")
        faces[name] = Path(path)
    metadata, _ = render(args.document, faces, args.output, args.document_code)
    return metadata


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)

    one = commands.add_parser("decrypt", help="REPORTX 암호문을 평문으로 바꾼다")
    one.add_argument("--exe", type=Path, required=True)
    one.add_argument("--mode", choices=("launch", "document"), required=True)
    one.add_argument("--input", type=Path, required=True)
    one.add_argument("--output", type=Path, required=True)
    one.add_argument("--document-number")
    one.set_defaults(run=decrypt)

    two = commands.add_parser("render", help="FastReport 문서를 PDF 로 렌더링한다")
    two.add_argument("--document", type=Path, required=True)
    two.add_argument("--font", action="append", default=[], metavar="이름=경로",
                     help="인쇄 글꼴. NanumMyeongjo-Regular/Bold, NanumGothicCoding-Regular/Bold 네 가지")
    two.add_argument("--document-code", required=True, help="학교가 발급한 16자리 문서확인번호")
    two.add_argument("--output", type=Path, required=True)
    two.set_defaults(run=render_document)

    args = parser.parse_args()
    try:
        print(json.dumps(args.run(args), ensure_ascii=False), flush=True)
    except Exception as error:
        # 표준출력은 JSON 전용이므로 진단은 stderr 로만 쓴다. 문서 내용은 남기지 않는다.
        message = str(error) if isinstance(error, ValueError) else type(error).__name__
        print(message, file=sys.stderr)
        raise SystemExit(1)


if __name__ == "__main__":
    main()
