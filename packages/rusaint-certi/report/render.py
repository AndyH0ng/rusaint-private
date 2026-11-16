"""수신한 FastReport preparedreport 문서의 인쇄 내용을 PDF로 렌더링한다.

서명된 전자증명서 PDF를 생성하는 도구가 아니다. 원래 페이지의 문자·도형·그림 배치를 사용한다.
Windows 글꼴 대신 공개 라이선스 Nanum 글꼴을 사용한다.
"""

from collections import Counter
import io
import os
from pathlib import Path
import re
import struct

from lxml import etree
from PIL import Image
from reportlab.lib.utils import ImageReader
from reportlab.pdfbase import pdfmetrics
from reportlab.pdfbase.ttfonts import TTFont
from reportlab.pdfgen.canvas import Canvas

PX = 72 / 96
MM = 72 / 25.4


def parse_document(data):
    if len(data) < 4:
        raise ValueError("문서가 잘렸습니다")
    length = struct.unpack_from("<I", data)[0]
    if length > len(data) - 4 or length > 32 * 1024 * 1024:
        raise ValueError("문서 XML 길이 오류")
    xml = data[4:4 + length]
    # 실제 문서는 UTF-8 선언과 달리 일부 속성에 CP949가 섞여 있다.
    # errors=replace/recover로 한글을 버리지 않고 속성마다 엄격하게 복원한다.
    def normalize(match):
        raw = match.group(2)
        try:
            value = raw.decode("utf-8")
        except UnicodeDecodeError:
            value = raw.decode("cp949")
        return match.group(1) + b'="' + value.encode("utf-8") + b'"'
    xml = re.sub(rb'([A-Za-z_][A-Za-z0-9_.]*)="([^"\r\n]*)"', normalize, xml)
    root = etree.fromstring(xml, etree.XMLParser(resolve_entities=False, no_network=True))
    if root.tag != "preparedreport" or root.find("previewpages") is None:
        raise ValueError("지원하지 않는 FastReport 형식")
    return root


def number(node, name, default=0):
    return float(node.get(name, default))


def color(value):
    # Delphi TColor는 BGR 정수다. 시스템 색은 검정으로 처리한다.
    value = int(value)
    if value < 0:
        return (0, 0, 0)
    return ((value & 255) / 255, ((value >> 8) & 255) / 255, ((value >> 16) & 255) / 255)


def picture(node):
    value = node.get("Picture.PropData", "")
    if not value:
        return None
    binary = bytes.fromhex(value)
    # Delphi TPicture 속성 스트림의 클래스·속성 헤더 뒤에 이미지가 들어 있다.
    for marker in (b"BM", b"\x89PNG\r\n\x1a\n", b"\xff\xd8\xff"):
        offset = binary.find(marker)
        if offset < 0:
            continue
        image = Image.open(io.BytesIO(binary[offset:]))
        image.load()
        image = image.convert("RGBA")
        if node.get("Transparent") == "True":
            key = image.getpixel((0, 0))[:3]
            image.putdata([pixel[:3] + (0 if pixel[:3] == key else pixel[3],) for pixel in image.get_flattened_data()])
        return image
    raise ValueError("지원하지 않는 그림 스트림")


FACES = ("NanumMyeongjo-Regular", "NanumMyeongjo-Bold",
         "NanumGothicCoding-Regular", "NanumGothicCoding-Bold")


def register_fonts(faces):
    """글꼴 이름 → 파일 경로. 파일 이름 규칙은 호출자가 정한다(배포판마다 다름)."""
    missing = [name for name in FACES if name not in faces]
    if missing:
        raise ValueError("인쇄에 필요한 글꼴이 없습니다: " + ", ".join(missing))
    for name in FACES:
        pdfmetrics.registerFont(TTFont(name, str(faces[name])))


def font(node):
    family = "NanumGothicCoding" if "굴림" in node.get("Font.Name", "") else "NanumMyeongjo"
    style = "Bold" if int(node.get("Font.Style", "0")) & 1 else "Regular"
    return family + "-" + style


def glyph_font(name, character):
    if ord(character) in pdfmetrics.getFont(name).face.charToGlyph:
        return name
    fallback = "NanumGothicCoding-" + name.rsplit("-", 1)[1]
    if ord(character) not in pdfmetrics.getFont(fallback).face.charToGlyph:
        raise ValueError(f"인쇄 글꼴에 없는 문자 U+{ord(character):04X}")
    return fallback


def text_width(text, name, size, spacing):
    return sum(pdfmetrics.stringWidth(c, glyph_font(name, c), size) for c in text) + max(0, len(text) - 1) * spacing


def wrap(text, width, name, size, spacing, word_wrap):
    lines = []
    for paragraph in text.replace("\r\n", "\n").replace("\r", "\n").split("\n"):
        if not word_wrap:
            lines.append(paragraph)
            continue
        # 글자마다 줄 전체를 다시 재면 줄 길이의 제곱이 된다. 폭을 누적해 한 번씩만 잰다.
        current, extent = "", 0.0
        for character in paragraph:
            advance = pdfmetrics.stringWidth(character, glyph_font(name, character), size)
            if current and extent + spacing + advance > width:
                lines.append(current)
                current, extent = character, advance
            else:
                extent += advance + (spacing if current else 0)
                current += character
        lines.append(current)
    return lines


def render(document, faces, output, doc_no=None):
    register_fonts(faces)
    root = parse_document(document.read_bytes())
    serial_fields = 0
    if doc_no is not None:
        if not re.fullmatch(r"[A-Za-z0-9]{16}", doc_no):
            raise ValueError("서버 문서확인번호 형식 오류")
        formatted = "-".join(doc_no[i:i + 4] for i in range(0, 16, 4))
        for page in root.find("previewpages"):
            for node in page.iter("TfrxMemoView"):
                if node.get("Name", "").startswith("__SERIAL__"):
                    node.set("Text", "【 문서확인번호 : " + formatted + " 】")
                    serial_fields += 1
        if serial_fields != len(root.find("previewpages")):
            raise ValueError("페이지별 문서확인번호 객체가 일치하지 않습니다")
    counts = Counter()
    expected_text = []
    buffer = io.BytesIO()
    canvas = Canvas(buffer, pageCompression=1)
    canvas.setTitle("수신 문서 인쇄 렌더링")
    canvas.setCreator("rusaint FastReport renderer")

    def draw(node, parent_x, parent_y, page_height):
        left = parent_x + number(node, "Left") * PX
        top = parent_y + number(node, "Top") * PX
        width = number(node, "Width") * PX
        height = number(node, "Height") * PX
        bottom = page_height - top - height
        tag = node.tag
        if tag == "TfrxMemoView":
            counts["text_objects"] += 1
            text = node.get("Text", "")
            expected_text.append(text)
            size = abs(number(node, "Font.Height", -13)) * PX
            name = font(node)
            spacing = number(node, "CharSpacing") * PX
            gap_x = number(node, "GapX", 2) * PX
            gap_y = number(node, "GapY", 1) * PX
            inner_width = max(1, width - 2 * gap_x)
            lines = wrap(text, inner_width, name, size, spacing, node.get("WordWrap", "True") == "True")
            leading = size + number(node, "LineSpacing", 2) * PX
            ascent = pdfmetrics.getAscent(name) * size / 1000
            descent = pdfmetrics.getDescent(name) * size / 1000
            text_height = ascent - descent + (len(lines) - 1) * leading
            alignment = node.get("VAlign", "vaTop")
            offset = gap_y
            if alignment == "vaCenter":
                offset = max(gap_y, (height - text_height) / 2)
            elif alignment == "vaBottom":
                offset = max(gap_y, height - text_height - gap_y)
            canvas.saveState()
            path = canvas.beginPath()
            path.rect(left, bottom, width, height)
            canvas.clipPath(path, stroke=0)
            canvas.setFillColorRGB(*color(node.get("Font.Color", "0")))
            for index, line in enumerate(lines):
                line_width = text_width(line, name, size, spacing)
                x = left + gap_x
                align = node.get("HAlign", "haLeft")
                if align == "haCenter":
                    x = left + (width - line_width) / 2
                elif align == "haRight":
                    x = left + width - gap_x - line_width
                obj = canvas.beginText(x, page_height - top - offset - ascent - index * leading)
                obj.setCharSpace(spacing)
                active_font = None
                run = ""
                for character in line:
                    current_font = glyph_font(name, character)
                    if active_font != current_font:
                        if run:
                            obj.textOut(run)
                            run = ""
                        obj.setFont(current_font, size)
                        active_font = current_font
                    run += character
                if run:
                    obj.textOut(run)
                canvas.drawText(obj)
            canvas.restoreState()
            frame = int(node.get("Frame.Typ", "0"))
            if frame:
                canvas.setLineWidth(PX)
                if frame & 1: canvas.line(left, bottom, left, bottom + height)
                if frame & 2: canvas.line(left + width, bottom, left + width, bottom + height)
                if frame & 4: canvas.line(left, bottom + height, left + width, bottom + height)
                if frame & 8: canvas.line(left, bottom, left + width, bottom)
        elif tag == "TfrxPictureView":
            image = picture(node)
            if image:
                counts["pictures"] += 1
                keep = node.get("KeepAspectRatio", "True") == "True"
                w, h = width, height
                if keep:
                    scale = min(width / image.width, height / image.height)
                    w, h = image.width * scale, image.height * scale
                x, y = left, page_height - top - h
                if node.get("Center", "True") == "True":
                    x += (width - w) / 2
                    y -= (height - h) / 2
                canvas.drawImage(ImageReader(image), x, y, w, h, mask="auto")
        elif tag == "TfrxLineView":
            counts["lines"] += 1
            canvas.setStrokeColorRGB(0, 0, 0)
            canvas.setLineWidth(PX)
            canvas.line(left, page_height - top, left + width, page_height - top - height)
        elif tag == "TfrxShapeView":
            counts["shapes"] += 1
            canvas.setStrokeColorRGB(0, 0, 0)
            canvas.setLineWidth(number(node, "Frame.Width", 1) * PX)
            canvas.rect(left, bottom, width, height, fill=0, stroke=1)
        elif tag not in {"TfrxReportPage", "TfrxPageHeader", "TfrxPageFooter", "TfrxMasterData", "TfrxNullBand"}:
            raise ValueError("지원하지 않는 인쇄 객체: " + str(tag))
        for child in node:
            draw(child, left, top, page_height)

    for page in root.find("previewpages"):
        width = number(page, "PaperWidth", 210) * MM
        height = number(page, "PaperHeight", 297) * MM
        canvas.setPageSize((width, height))
        for child in page:
            draw(child, number(page, "LeftMargin") * MM, number(page, "TopMargin") * MM, height)
        canvas.showPage()
        counts["pages"] += 1
    canvas.save()
    descriptor = os.open(output, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, "wb") as stream:
        stream.write(buffer.getvalue())
    return {**counts, "pdf_bytes": len(buffer.getvalue()), "font_substitution": True,
            "electronically_signed_pdf": False, "server_docno_applied": bool(serial_fields)}, expected_text
