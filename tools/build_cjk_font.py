#!/usr/bin/env python3
"""把系統中文字型轉成韌體能用的 16x16 點陣字型。

為什麼需要這個:embedded-graphics 內建的 mono_font 全是 ASCII 字型,
中文字一律畫成 '?'。螢幕上的回覆文字要看得懂中文,就得自己帶字型。

輸出格式(assets/font16.bin,小端序):
    u16  count                — 收錄字數
    u16 * count               — 已排序的 Unicode 碼位(供二分搜尋)
    32 bytes * count          — 字形點陣,16 列 x 每列 2 bytes(MSB 在左)

半形(碼位 < 0x80)一樣存 16x16,只用左半 8 欄,由韌體端決定前進寬度 8;
全形前進寬度 16。這樣索引與字形都是等長的,查表最單純。

字型來源:Noto Sans TC(SIL Open Font License 1.1,可隨韌體再散布)。
點陣化後的 font16.bin 同樣適用 OFL,授權全文在 assets/font16.OFL.txt。
不要改用 Windows 內建字型(微軟正黑體等)—— 那些不能跟著開源專案散布。

用法:
    # 先下載字型(約 12MB,不放進 repo):
    #   https://github.com/google/fonts/raw/main/ofl/notosanstc/NotoSansTC%5Bwght%5D.ttf
    python tools/build_cjk_font.py --font NotoSansTC[wght].ttf    # Big5 全集 + ASCII
    python tools/build_cjk_font.py --font ... --size 15           # 微調字面大小
"""
import argparse
import os
import struct
import sys

try:
    from PIL import Image, ImageDraw, ImageFont
except ImportError:
    sys.exit("需要 Pillow:pip install pillow")

HERE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(HERE, "assets", "font16.bin")
CELL = 16

# 可變字型的字重。實測 400 在 15pt、門檻 110 下筆畫清楚,與原本的效果最接近。
DEFAULT_WEIGHT = 400


def charset():
    """ASCII 可見字 + 所有 Big5 可編碼的中文字與全形標點。"""
    chars = set(chr(c) for c in range(0x20, 0x7F))
    for hi in range(0xA1, 0xFA):
        for lo in list(range(0x40, 0x7F)) + list(range(0xA1, 0xFF)):
            try:
                ch = bytes([hi, lo]).decode("big5")
            except (UnicodeDecodeError, ValueError):
                continue
            if len(ch) == 1 and ord(ch) <= 0xFFFF:
                chars.add(ch)
    # AI 回覆常出現、但不在 Big5 裡的符號
    for ch in "　—…‧※→←↑↓°±×÷℃":
        if ord(ch) <= 0xFFFF:
            chars.add(ch)
    return sorted(chars, key=ord)


def load_font(path, size, weight):
    if not os.path.exists(path):
        sys.exit("找不到字型檔:%s(下載網址見檔案開頭說明)" % path)
    font = ImageFont.truetype(path, size)
    try:
        font.set_variation_by_axes([weight])
    except (OSError, ValueError):
        pass  # 非可變字型就用它本身的字重
    return font


def render(font, ch, threshold):
    """把一個字畫進 16x16 的格子,回傳 32 bytes。"""
    img = Image.new("L", (CELL, CELL), 0)
    d = ImageDraw.Draw(img)
    # 用 anchor 讓字在格子裡置中,避免不同字忽高忽低
    try:
        d.text((CELL // 2, CELL // 2), ch, font=font, fill=255, anchor="mm")
    except (ValueError, TypeError):
        d.text((0, 0), ch, font=font, fill=255)

    out = bytearray()
    px = img.load()
    for y in range(CELL):
        row = 0
        for x in range(CELL):
            if px[x, y] >= threshold:
                row |= 1 << (15 - x)
        out += struct.pack("<H", row)
    return bytes(out)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--font", required=True, help="Noto Sans TC 字型檔路徑(OFL 授權)")
    ap.add_argument("--weight", type=int, default=DEFAULT_WEIGHT, help="可變字型字重,預設 400")
    ap.add_argument("--size", type=int, default=15, help="字面大小(點),預設 15 塞進 16px 格子")
    ap.add_argument("--threshold", type=int, default=110, help="二值化門檻,越低筆畫越粗")
    args = ap.parse_args()

    font = load_font(args.font, args.size, args.weight)
    chars = charset()
    print("字型:%s  字重:%d  大小:%d  收錄:%d 字" % (os.path.basename(args.font), args.weight, args.size, len(chars)))

    index = bytearray()
    glyphs = bytearray()
    blank = 0
    for ch in chars:
        index += struct.pack("<H", ord(ch))
        g = render(font, ch, args.threshold)
        if ch != " " and not any(g):
            blank += 1
        glyphs += g

    with open(OUT, "wb") as f:
        f.write(struct.pack("<H", len(chars)))
        f.write(index)
        f.write(glyphs)

    size = os.path.getsize(OUT)
    print("輸出:%s  %d bytes (%.0f KB)" % (os.path.relpath(OUT, HERE), size, size / 1024))
    if blank:
        print("警告:有 %d 個字渲染出來是空白(字型可能缺字)" % blank)


if __name__ == "__main__":
    main()
