#!/usr/bin/env python3
"""
把一張 PNG/JPG 轉成 ESP32 韌體可直接讀的 RGB565 原始位元圖(.rgb565)。

用法:
    python png_to_rgb565.py 輸入.png 輸出.rgb565 --width 176 --height 220

輸出格式:每個像素 2 bytes(大端序 big-endian,配合 embedded-graphics
ImageRaw 的預設 byte order,也是 ST7789 這類 SPI TFT 慣用的傳輸順序)。
沒有檔頭,純像素資料,大小 = width * height * 2 bytes。

轉好後在 board_hw.rs 用:
    static FACE_HAPPY: &[u8] = include_bytes!("../assets/faces/happy.rgb565");
搭配 blit_face_bitmap() 顯示。
"""
import argparse
import sys

try:
    from PIL import Image
except ImportError:
    sys.exit("缺少 Pillow,請先執行: pip install pillow")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("input", help="來源圖片(PNG/JPG)")
    ap.add_argument("output", help="輸出檔(.rgb565)")
    ap.add_argument("--width", type=int, default=176, help="目標寬度(預設 176)")
    ap.add_argument("--height", type=int, default=220, help="目標高度(預設 220)")
    ap.add_argument("--crop-square", action="store_true",
                     help="先從中央裁成正方形再縮放(適合大頭照)")
    args = ap.parse_args()

    img = Image.open(args.input).convert("RGB")

    if args.crop_square:
        w, h = img.size
        side = min(w, h)
        left = (w - side) // 2
        top = (h - side) // 2
        img = img.crop((left, top, left + side, top + side))

    img = img.resize((args.width, args.height), Image.LANCZOS)

    out = bytearray()
    for y in range(args.height):
        for x in range(args.width):
            r, g, b = img.getpixel((x, y))
            rgb565 = ((r & 0xF8) << 8) | ((g & 0xFC) << 3) | (b >> 3)
            out.append((rgb565 >> 8) & 0xFF)  # 大端序:高位元組先
            out.append(rgb565 & 0xFF)

    with open(args.output, "wb") as f:
        f.write(out)

    print(f"OK: {args.output} ({args.width}x{args.height}, {len(out)} bytes)")


if __name__ == "__main__":
    main()
