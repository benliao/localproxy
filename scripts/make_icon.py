#!/usr/bin/env python3
"""Generate a minimal 512x512 app icon PNG (no third-party deps)."""
import struct
import zlib
from pathlib import Path

SIZE = 512
BG = (31, 34, 41, 255)
FG = (79, 140, 255, 255)


def pixel(x: int, y: int) -> tuple:
    cx = cy = SIZE / 2
    dx, dy = x - cx, y - cy
    r = (dx * dx + dy * dy) ** 0.5
    # Ring + inner dot: a simple "signal relay" mark.
    if r < SIZE * 0.16 or SIZE * 0.30 < r < SIZE * 0.38:
        return FG
    return BG


def main() -> None:
    raw = bytearray()
    for y in range(SIZE):
        raw.append(0)  # filter type 0
        for x in range(SIZE):
            raw.extend(pixel(x, y))

    def chunk(tag: bytes, data: bytes) -> bytes:
        return (
            struct.pack(">I", len(data))
            + tag
            + data
            + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
        )

    png = b"\x89PNG\r\n\x1a\n"
    # color type 6 = RGBA, required by Tauri's icon codegen
    png += chunk(b"IHDR", struct.pack(">IIBBBBB", SIZE, SIZE, 8, 6, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(bytes(raw), 9))
    png += chunk(b"IEND", b"")

    out = Path(__file__).resolve().parent.parent / "src-tauri" / "icons" / "icon.png"
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_bytes(png)
    print(f"wrote {out} ({len(png)} bytes)")


if __name__ == "__main__":
    main()
