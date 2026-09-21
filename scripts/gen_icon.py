# -*- coding: utf-8 -*-
"""生成应用图标 icon.ico（纯标准库，无需 PIL）。

图标样式：蓝色渐变圆角方块 + 白色摄像头镜头圆环 + 绿色在线状态点。
尺寸：16/24/32/48/64/128（BMP 编码）与 256（PNG 编码）。
"""
import math
import os
import struct
import zlib

OUT = os.path.join(os.path.dirname(__file__), "..", "src-tauri", "icons", "icon.ico")


def draw(size: int):
    """返回 size x size 的 RGBA 像素阵列（list[list[(r,g,b,a)]]）。"""
    px = []
    r = size * 0.22          # 圆角半径
    cx = cy = size / 2
    ring_r = size * 0.30     # 镜头圆环半径
    ring_w = size * 0.055
    pupil = size * 0.125     # 中心瞳孔
    dot_r = size * 0.09      # 在线状态点
    dot_cx, dot_cy = size * 0.74, size * 0.26
    for y in range(size):
        row = []
        for x in range(size):
            ax, ay = min(x, size - 1 - x), min(y, size - 1 - y)
            if ax < r and ay < r:
                dx, dy = r - ax, r - ay
                if dx * dx + dy * dy > r * r:
                    row.append((0, 0, 0, 0))
                    continue
            t = y / size
            col = (int(0x27 + (0x14 - 0x27) * t),
                   int(0x74 + (0x42 - 0x74) * t),
                   int(0xd2 + (0x91 - 0xd2) * t), 255)
            d = math.hypot(x - cx, y - cy)
            if abs(d - ring_r) < ring_w or d < pupil:
                col = (255, 255, 255, 255)
            if math.hypot(x - dot_cx, y - dot_cy) < dot_r:
                col = (0x2e, 0xcc, 0x71, 255)
            row.append(col)
        px.append(row)
    return px


def make_png(size: int, pixels) -> bytes:
    raw = b"".join(b"\x00" + b"".join(struct.pack("BBBB", *p) for p in row) for row in pixels)

    def chunk(tag: bytes, data: bytes) -> bytes:
        return (struct.pack(">I", len(data)) + tag + data
                + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF))

    ihdr = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr)
            + chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b""))


def make_bmp(size: int, pixels) -> bytes:
    # BITMAPINFOHEADER（biHeight 为 2 倍：像素 + AND 掩码）
    header = struct.pack("<IiiHHIIiiII", 40, size, size * 2, 1, 32, 0,
                         size * size * 4, 0, 0, 0, 0)
    data = b""
    for row in reversed(pixels):
        for (r, g, b, a) in row:
            data += struct.pack("BBBB", b, g, r, a)
    mask_row = ((size + 31) // 32) * 4
    data += b"\x00" * (mask_row * size)
    return header + data


def main():
    sizes = [16, 24, 32, 48, 64, 128, 256]
    images = {s: draw(s) for s in sizes}
    count = len(sizes)
    header = struct.pack("<HHH", 0, 1, count)
    offset = 6 + 16 * count
    dirs, blobs = b"", b""
    for s in sizes:
        if s >= 256:
            img, wb, hb = make_png(256, images[256]), 0, 0
        else:
            img, wb, hb = make_bmp(s, images[s]), s, s
        dirs += struct.pack("<BBBBHHII", wb, hb, 0, 0, 1, 32, len(img), offset)
        blobs += img
        offset += len(img)
    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    with open(OUT, "wb") as f:
        f.write(header + dirs + blobs)
    # 同步输出 PNG（托盘图标用）
    png_path = os.path.join(os.path.dirname(OUT), "icon.png")
    with open(png_path, "wb") as f:
        f.write(make_png(64, images[64]))
    print("icon.ico written:", os.path.abspath(OUT), len(header + dirs + blobs), "bytes")
    print("icon.png written:", os.path.abspath(png_path))


if __name__ == "__main__":
    main()
