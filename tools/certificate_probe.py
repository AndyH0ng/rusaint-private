# /// script
# requires-python = ">=3.11"
# dependencies = ["pefile==2024.8.26", "unicorn==2.1.4"]
# ///
"""Linux에서 REPORTX 문서 수신을 검증하는 실험 도구. Windows 프로그램은 실행하지 않는다.

uv run tools/certificate_probe.py --exe REPORTX.exe --preview preview.b64 --output-dir private-output

preview.b64는 본인 세션의 PREVIEWENC 응답 res 값이다. 출력에는 개인정보와 문서 접근 토큰이 들어간다.
이 도구는 원본 PDF 생성기가 아니다. 받은 FastReport 데이터를 보존하며 출력 완료 처리는 하지 않는다.
"""

import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import struct
from urllib.parse import urlencode, urlsplit
from urllib.request import urlopen
import zlib

import pefile
from unicorn import UC_ARCH_X86, UC_MODE_32, Uc
from unicorn.x86_const import UC_X86_REG_EAX, UC_X86_REG_ECX, UC_X86_REG_EDX, UC_X86_REG_EIP, UC_X86_REG_ESP


class ReportCipher:
    """특정 REPORTX 버전의 순수 연산 함수 두 개만 에뮬레이션한다."""

    def __init__(self, executable: Path):
        self.binary = executable.read_bytes()
        self.pe = pefile.PE(data=self.binary)
        if self.pe.OPTIONAL_HEADER.ImageBase != 0x400000:
            raise ValueError("지원하지 않는 REPORTX 이미지 베이스")
        # 다른 버전의 코드를 잘못 실행하지 않도록 분석한 함수의 시작 바이트를 확인한다.
        for address, signature in [(0x54C14C, bytes.fromhex("53565783c4c4")),
                                   (0x54C714, bytes.fromhex("558bec81c4c4feffff"))]:
            offset = self.pe.get_offset_from_rva(address - 0x400000)
            if self.binary[offset:offset + len(signature)] != signature:
                raise ValueError("분석한 버전과 다른 REPORTX 실행 파일")
        self.machine = Uc(UC_ARCH_X86, UC_MODE_32)
        self.machine.mem_map(0x400000, 0x400000)
        self.machine.mem_write(0x400000, self.pe.get_memory_mapped_image())
        self.machine.mem_map(0x1000000, 0x200000)

    def _call(self, address, eax, edx, ecx, arguments=()):
        u = self.machine
        stack, stop = 0x11FF000, 0x11FFF00
        u.mem_write(stack, struct.pack("<I", stop) + b"".join(struct.pack("<I", a) for a in arguments))
        for register, value in [(UC_X86_REG_ESP, stack), (UC_X86_REG_EAX, eax),
                                (UC_X86_REG_EDX, edx), (UC_X86_REG_ECX, ecx)]:
            u.reg_write(register, value)
        u.emu_start(address, stop, count=10_000_000)
        if u.reg_read(UC_X86_REG_EIP) != stop:
            raise ValueError("암호 함수 실행 한도 초과")
        return u.reg_read(UC_X86_REG_EAX)

    def launch_password(self):
        offset = self.pe.get_offset_from_rva(0x5815CC - 0x400000)
        length = struct.unpack_from("<I", self.binary, offset - 4)[0]
        if length != 5:
            raise ValueError("지원하지 않는 실행 인자 상수")
        return self.binary[offset:offset + length]

    def decrypt(self, encrypted, password):
        if not encrypted or len(encrypted) % 16:
            raise ValueError("암호문 블록 길이 오류")
        obj, key, schedule, source, destination = (0x1000000, 0x1001000, 0x1002000, 0x1003000, 0x1004000)
        u = self.machine
        u.mem_write(obj, b"\0" * 512)
        u.mem_write(obj + 0x38, struct.pack("<I", 192))
        u.mem_write(key, hashlib.md5(password).hexdigest().encode("ascii"))
        rounds = self._call(0x54C14C, obj, key, schedule)
        decoded = bytearray()
        length = None
        for offset in range(0, len(encrypted), 16):
            u.mem_write(source, encrypted[offset:offset + 16])
            self._call(0x54C714, obj, source, rounds, (destination, schedule))
            decoded.extend(u.mem_read(destination, 16))
            if length is None:
                length = struct.unpack_from("<I", decoded)[0]
                if length > len(encrypted) - 4:
                    raise ValueError("복호화된 길이 오류")
            if len(decoded) >= length + 4:
                return bytes(decoded[4:4 + length])
        raise ValueError("암호문이 잘렸습니다")


def private_write(path, data):
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, "wb") as stream:
        stream.write(data)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--exe", type=Path, required=True)
    parser.add_argument("--preview", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    args = parser.parse_args()
    args.output_dir.mkdir(mode=0o700, parents=True, exist_ok=False)
    cipher = ReportCipher(args.exe)
    launch = cipher.decrypt(base64.b64decode(args.preview.read_bytes(), validate=True), cipher.launch_password())
    text = launch.decode("cp949")
    url = urlsplit(text)
    if url.scheme != "http" or url.netloc != "127.0.0.1" or url.path != "/SHOWREPORT":
        raise ValueError("지원하지 않는 실행 인자 형식")
    params = dict(item.split("=", 1) for item in url.query.split("|") if "=" in item)
    file_url = urlsplit("https://" + params["URLFile"])
    if file_url.netloc != "certi.ssu.ac.kr" or file_url.path != "/wm1.0/jsp/report/sendfile.jsp" or file_url.query or file_url.fragment:
        raise ValueError("지원하지 않는 문서 수신 주소")
    private_write(args.output_dir / "launch.bin", launch)
    query = urlencode({"TPID": params["TPID"], "MIN_NO": params["MINNO"], "GIWAN_NO": params["GIWAN_NO"]})
    with urlopen(file_url.geturl() + "?" + query, timeout=30) as response:
        encrypted = response.read(16 * 1024 * 1024 + 1)
        if len(encrypted) > 16 * 1024 * 1024:
            raise ValueError("문서 응답 크기 한도 초과")
    private_write(args.output_dir / "document.encrypted", encrypted)
    compressed = cipher.decrypt(encrypted, params["MINNO"].encode("ascii"))
    inflater = zlib.decompressobj()
    document = inflater.decompress(compressed, 32 * 1024 * 1024 + 1)
    if len(document) > 32 * 1024 * 1024 or not inflater.eof:
        raise ValueError("압축 문서 크기 한도 초과 또는 불완전한 응답")
    private_write(args.output_dir / "document.bin", document)
    xml_length = struct.unpack_from("<I", document)[0]
    if xml_length > len(document) - 4 or b"<preparedreport>" not in document[4:256]:
        raise ValueError("지원하지 않는 FastReport 문서 형식")
    private_write(args.output_dir / "preparedreport.xml", document[4:4 + xml_length])
    metadata = {"format": "FastReport preparedreport", "encrypted_bytes": len(encrypted),
                "document_bytes": len(document), "xml_bytes": xml_length,
                "pdf_generated": False, "print_completion_sent": False}
    private_write(args.output_dir / "metadata.json", (json.dumps(metadata, indent=2) + "\n").encode())
    print(json.dumps(metadata))


if __name__ == "__main__":
    main()
