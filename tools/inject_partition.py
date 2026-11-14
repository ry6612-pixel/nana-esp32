#!/usr/bin/env python3
"""把額外的資料分割區塞進 espflash 合成出來的單檔映像。

為什麼需要這支:`espflash save-image --merge` 只合 bootloader + 分割表 + app,
**不含任何 data 分割區**。本專案的語音喚醒模型 (srmodels.bin) 住在
partitions.csv 的 `model` 分割區 (0x420000),沒被合進去 → 燒了 Release 單檔
映像的人開機會看到:

    ESP-SR init failed: No WakeNet model found in partition

而且因為加了 --skip-padding,映像根本不會延伸到 0x420000,連「空白但存在」
都不是 —— 那段位址在檔案裡不存在。

用法:
    python tools/inject_partition.py 映像.bin 0x420000 srmodels.bin
    python tools/inject_partition.py 映像.bin --from-partitions partitions.csv \\
        --partition model --data-file srmodels.bin
"""
import argparse
import csv
import os
import sys

PAD = b"\xff"  # 未寫入的 flash 是 0xFF,補洞要用它才跟實際燒錄結果一致


def parse_offset(text):
    return int(text, 16) if text.lower().startswith("0x") else int(text)


def lookup_partition(csv_path, name):
    """從 partitions.csv 找出某個分割區的 (offset, size)。"""
    with open(csv_path, newline="", encoding="utf-8") as f:
        for row in csv.reader(f):
            if not row or row[0].strip().startswith("#"):
                continue
            if row[0].strip() == name:
                off = parse_offset(row[3].strip())
                size = parse_offset(row[4].strip())
                return off, size
    sys.exit("partitions.csv 裡找不到分割區 '%s'" % name)


def inject(image_path, offset, data_path, limit=None):
    data = open(data_path, "rb").read()
    if limit is not None and len(data) > limit:
        sys.exit("%s 有 %d bytes,超過分割區容量 %d bytes"
                 % (os.path.basename(data_path), len(data), limit))

    img = bytearray(open(image_path, "rb").read())
    before = len(img)

    # 映像可能還沒長到那個位址(--skip-padding 會截斷),先用 0xFF 補到位
    if len(img) < offset:
        img.extend(PAD * (offset - len(img)))
    end = offset + len(data)
    if len(img) < end:
        img.extend(PAD * (end - len(img)))

    img[offset:end] = data
    open(image_path, "wb").write(img)

    print("  已寫入 %s @ 0x%06x (%d bytes)" % (os.path.basename(data_path), offset, len(data)))
    print("  映像 %d -> %d bytes (%.2f MB)" % (before, len(img), len(img) / 1048576))

    # 立刻讀回來驗證,不要相信「寫了就是對的」
    check = open(image_path, "rb").read()[offset:end]
    if check != data:
        sys.exit("寫入後驗證失敗:讀回的內容與來源不符")
    print("  驗證:讀回內容與來源完全相符")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("image", help="espflash save-image --merge 產出的映像")
    ap.add_argument("offset", nargs="?", help="寫入位址,例如 0x420000")
    ap.add_argument("data", nargs="?", help="要寫進去的檔案")
    ap.add_argument("--from-partitions", help="改從 partitions.csv 查位址")
    ap.add_argument("--partition", help="搭配 --from-partitions 使用的分割區名稱")
    ap.add_argument("--data-file", dest="data_file", help="搭配 --from-partitions 使用")
    args = ap.parse_args()

    if args.from_partitions:
        if not args.partition or not args.data_file:
            sys.exit("--from-partitions 需要同時指定 --partition 與 --data-file")
        off, size = lookup_partition(args.from_partitions, args.partition)
        inject(args.image, off, args.data_file, limit=size)
    else:
        if not args.offset or not args.data:
            sys.exit("需要 offset 與 data,或改用 --from-partitions")
        inject(args.image, parse_offset(args.offset), args.data)


if __name__ == "__main__":
    main()
