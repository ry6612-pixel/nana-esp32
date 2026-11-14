#!/usr/bin/env python3
"""把韌體實際使用的 .rgb565 表情圖解回 PNG，供 README 展示。

刻意從 .rgb565 解回來而不是直接用原始 JPG：README 上看到的就會是螢幕上
一模一樣的畫面（含降到 RGB565 之後的實際色階），不是另外找一張漂亮的宣傳圖。

用法：
    python tools/faces_to_png.py                 # 產生四張 PNG + 一張四連拼圖
"""
import os
import struct
import sys

try:
    from PIL import Image, ImageDraw
except ImportError:
    sys.exit("需要 Pillow：pip install pillow")

HERE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SRC = os.path.join(HERE, "assets", "faces")
OUT = os.path.join(HERE, "docs", "faces")
SIZE = 216  # 與 board_hw.rs 的 FACE_W / FACE_H 一致

FACES = [
    ("neutral", "常規"),
    ("smile", "微笑"),
    ("happy", "大笑"),
    ("love", "比愛心"),
]


def decode(path, size):
    """RGB565 大端序 → PIL Image。"""
    data = open(path, "rb").read()
    expect = size * size * 2
    if len(data) != expect:
        sys.exit("%s 大小不符：%d != %d" % (path, len(data), expect))
    img = Image.new("RGB", (size, size))
    px = img.load()
    for i in range(size * size):
        (v,) = struct.unpack_from(">H", data, i * 2)
        r = (v >> 11) & 0x1F
        g = (v >> 5) & 0x3F
        b = v & 0x1F
        # 5/6 bit 還原成 8 bit：複製高位補低位，避免整體偏暗
        px[i % size, i // size] = (
            (r << 3) | (r >> 2),
            (g << 2) | (g >> 4),
            (b << 3) | (b >> 2),
        )
    return img


def main():
    os.makedirs(OUT, exist_ok=True)
    imgs = []
    for name, label in FACES:
        src = os.path.join(SRC, name + ".rgb565")
        img = decode(src, SIZE)
        dst = os.path.join(OUT, name + ".png")
        img.save(dst)
        imgs.append((img, label))
        print("  %-8s → %s" % (name, os.path.relpath(dst, HERE)))

    # 四連拼圖：一眼看完全部表情
    gap = 10
    w = SIZE * 4 + gap * 5
    h = SIZE + gap * 2
    strip = Image.new("RGB", (w, h), (250, 248, 244))
    for i, (img, _) in enumerate(imgs):
        strip.paste(img, (gap + i * (SIZE + gap), gap))
    dst = os.path.join(OUT, "all-faces.png")
    strip.save(dst)
    print("  拼圖     → %s (%dx%d)" % (os.path.relpath(dst, HERE), w, h))


if __name__ == "__main__":
    main()
