"""분석한 REPORTX 버전의 순수 암호 연산만 에뮬레이션한다.

Windows API·진입점·서비스·인쇄 코드는 호출하지 않는다. 네트워크 접근도 없다.
"""

import hashlib
from pathlib import Path
import struct

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
