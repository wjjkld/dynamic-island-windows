"""生成 assets/island.ico：白边黑色药丸，和托盘图标（src/tray.rs 的 pill_pixels）同一画法。

用法：python assets/make_icon.py
"""

import math
import struct
import zlib
from pathlib import Path

SIZES = [16, 20, 24, 32, 40, 48, 64, 256]


def pill_pixels(n: int) -> bytes:
    """n×n 的 BGRA（非预乘），从上到下逐行。"""
    s = float(n)
    hw, hh = s * 0.47, s * 0.24
    r = hh
    border = max(s / 16.0, 1.0)
    out = bytearray()
    for y in range(n):
        for x in range(n):
            px, py = x + 0.5 - s / 2, y + 0.5 - s / 2
            qx, qy = abs(px) - (hw - r), abs(py) - (hh - r)
            d = math.hypot(max(qx, 0.0), max(qy, 0.0)) + min(max(qx, qy), 0.0) - r
            alpha = min(max(0.5 - d, 0.0), 1.0)
            white = min(max(d + border + 0.5, 0.0), 1.0)
            v = int(white * 235)
            out += bytes((v, v, v, int(alpha * 255)))
    return bytes(out)


def dib(n: int) -> bytes:
    """小尺寸：BITMAPINFOHEADER（高度写两倍）+ 自下而上的 BGRA + 全零 AND 掩码。"""
    px = pill_pixels(n)
    row = n * 4
    rows = b"".join(px[y * row:(y + 1) * row] for y in reversed(range(n)))
    mask_row = ((n + 31) // 32) * 4
    header = struct.pack("<IiiHHIIiiII", 40, n, n * 2, 1, 32, 0, len(rows), 0, 0, 0, 0)
    return header + rows + bytes(mask_row * n)


def png(n: int) -> bytes:
    """大尺寸用 PNG 存，体积小得多。"""
    px = pill_pixels(n)
    raw = bytearray()
    for y in range(n):
        raw.append(0)  # 每行的滤波类型：无
        for x in range(n):
            b, g, r, a = px[(y * n + x) * 4:(y * n + x) * 4 + 4]
            raw += bytes((r, g, b, a))

    def chunk(kind: bytes, data: bytes) -> bytes:
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))

    ihdr = struct.pack(">IIBBBBB", n, n, 8, 6, 0, 0, 0)
    signature = bytes((0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A))
    return signature + chunk(b"IHDR", ihdr) + chunk(b"IDAT", zlib.compress(bytes(raw), 9)) + chunk(b"IEND", b"")


def main() -> None:
    images = [png(n) if n >= 256 else dib(n) for n in SIZES]
    out = struct.pack("<HHH", 0, 1, len(SIZES))
    offset = 6 + 16 * len(SIZES)
    for n, img in zip(SIZES, images):
        side = 0 if n >= 256 else n
        out += struct.pack("<BBBBHHII", side, side, 0, 0, 1, 32, len(img), offset)
        offset += len(img)
    out += b"".join(images)
    path = Path(__file__).with_name("island.ico")
    path.write_bytes(out)
    print(f"{path.name}: {len(out)} bytes")


if __name__ == "__main__":
    main()
