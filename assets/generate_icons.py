#!/usr/bin/env python3
"""Generate status PNG and ICO assets using only Python's standard library."""
import struct
import zlib
from pathlib import Path

ROOT = Path(__file__).resolve().parent

def make_png(path, color):
    size = 64
    pixels = bytearray()
    for y in range(size):
        pixels.append(0)
        for x in range(size):
            inside = (x - 31.5) ** 2 + (y - 31.5) ** 2 <= 25 ** 2
            pixels.extend((*color, 255) if inside else (0, 0, 0, 0))
    def chunk(kind, data):
        return struct.pack('>I', len(data)) + kind + data + struct.pack('>I', zlib.crc32(kind + data) & 0xffffffff)
    data = b'\x89PNG\r\n\x1a\n'
    data += chunk(b'IHDR', struct.pack('>2I5B', size, size, 8, 6, 0, 0, 0))
    data += chunk(b'IDAT', zlib.compress(bytes(pixels), 9)) + chunk(b'IEND', b'')
    path.write_bytes(data)

def make_ico(path, color):
    size = 64
    pixels = bytearray()
    for y in range(size - 1, -1, -1):
        for x in range(size):
            inside = (x - 31.5) ** 2 + (y - 31.5) ** 2 <= 25 ** 2
            r, g, b = color if inside else (0, 0, 0)
            pixels.extend((b, g, r, 255 if inside else 0))
    mask = bytes(((size + 31) // 32) * 4 * size)
    dib = struct.pack('<IIIHHIIIIII', 40, size, size * 2, 1, 32, 0, len(pixels), 0, 0, 0, 0) + pixels + mask
    header = struct.pack('<HHH', 0, 1, 1) + struct.pack('<BBBBHHII', size, size, 0, 0, 1, 32, len(dib), 22)
    path.write_bytes(header + dib)

for name, color in [('idle', (47, 126, 220)), ('connected', (220, 58, 58))]:
    make_png(ROOT / f'{name}.png', color)
    make_ico(ROOT / f'{name}.ico', color)
