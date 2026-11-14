#!/usr/bin/env python3
"""把 assets/faces/src/*.jpg 轉成韌體 include_bytes! 直接用的 RGB565 點陣圖。

四張表情圖共用同一個裁切構圖(見 assets/faces/README.md),所以切換表情時
臉不會在螢幕上跳位置 —— 換的只是表情本身。

輸出格式:每像素 2 bytes、大端序(big-endian)、無檔頭,大小 = W*H*2。
對應韌體 `blit_face_bitmap()` 用的 `ImageRaw::<Rgb565>`(預設就是大端序)。

用法:
    python tools/build_face_assets.py            # 全部重建
    python tools/build_face_assets.py --size 200 # 換螢幕尺寸時改邊長
"""
import argparse
import os
import sys

try:
    from PIL import Image
except ImportError:
    sys.exit("需要 Pillow:pip install pillow")

FACES = ["neutral", "smile", "happy", "love"]
HERE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SRC_DIR = os.path.join(HERE, "assets", "faces", "src")
OUT_DIR = os.path.join(HERE, "assets", "faces")


def pack_rgb565(img):
    """RGB888 → RGB565 大端序 bytes。"""
    raw = img.tobytes()  # RGBRGBRGB...
    out = bytearray(len(raw) // 3 * 2)
    for i in range(0, len(raw), 3):
        v = ((raw[i] & 0xF8) << 8) | ((raw[i + 1] & 0xFC) << 3) | (raw[i + 2] >> 3)
        j = i // 3 * 2
        out[j] = (v >> 8) & 0xFF
        out[j + 1] = v & 0xFF
    return bytes(out)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--size", type=int, default=216,
                    help="輸出邊長(像素),需與 board_hw.rs 的 FACE_W/FACE_H 一致")
    args = ap.parse_args()

    for name in FACES:
        src = os.path.join(SRC_DIR, name + ".jpg")
        if not os.path.exists(src):
            sys.exit("缺少來源圖:%s" % src)
        img = Image.open(src).convert("RGB")
        if img.width != img.height:
            sys.exit("%s 不是正方形(%dx%d)" % (name, img.width, img.height))
        img = img.resize((args.size, args.size), Image.LANCZOS)
        data = pack_rgb565(img)
        assert len(data) == args.size * args.size * 2
        dst = os.path.join(OUT_DIR, name + ".rgb565")
        with open(dst, "wb") as f:
            f.write(data)
        print("%-8s -> %s  (%dx%d, %d bytes)"
              % (name, os.path.relpath(dst, HERE), args.size, args.size, len(data)))


if __name__ == "__main__":
    main()
