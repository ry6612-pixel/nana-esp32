#!/usr/bin/env python3
"""驗證單檔韌體映像:該在的區段是不是真的都在裡面。

這支存在的理由:`espflash save-image --merge` 只合 bootloader + 分割表 + app,
**不含任何 data 分割區**。語音喚醒模型因此整整缺席了一版,而症狀完全不像
「映像做壞了」—— 韌體正常開機、其他功能都好,只有喚醒詞不動,使用者會先懷疑
自己的麥克風。這種靜默失敗必須靠自動檢查擋下來。

用法:
    python tools/verify_image.py SerialSoul-v5.3.0.bin partitions.csv
"""
import collections
import csv
import hashlib
import os
import struct
import sys

# 開機時載入的關鍵資料分割區:必須有實際內容,不能整段空白
REQUIRED_DATA_PARTITIONS = {
    "model": "語音喚醒模型 (ESP-SR WakeNet)",
}
# 可用來做 sha256 比對的原始檔(相對於執行目錄)。找不到就只做「非空白」檢查。
EXPECTED_SOURCE = {}

# 使用者資料分割區:必須留白,燒錄時才不會蓋掉裝置上已有的設定
MUST_BE_BLANK = {
    "nvs": "使用者設定(WiFi/金鑰/Telegram 綁定/記憶)",
}


def parse_offset(t):
    t = t.strip()
    return int(t, 16) if t.lower().startswith("0x") else int(t)


def read_partitions(path):
    out = []
    with open(path, newline="", encoding="utf-8") as f:
        for row in csv.reader(f):
            if not row or row[0].strip().startswith("#"):
                continue
            out.append({
                "name": row[0].strip(),
                "type": row[1].strip(),
                "offset": parse_offset(row[3]),
                "size": parse_offset(row[4]),
            })
    return out


def main():
    if len(sys.argv) < 3:
        sys.exit("用法: verify_image.py <映像.bin> <partitions.csv> [srmodels.bin]")
    img_path, part_path = sys.argv[1], sys.argv[2]
    if len(sys.argv) > 3:
        EXPECTED_SOURCE["model"] = sys.argv[3]
    d = open(img_path, "rb").read()
    parts = read_partitions(part_path)

    fails = []

    def check(label, ok, detail=""):
        print("  %s  %s%s" % ("PASS" if ok else "FAIL", label,
                              ("  — " + detail) if detail else ""))
        if not ok:
            fails.append(label)

    print("映像: %s (%.2f MB)" % (img_path, len(d) / 1048576))
    print()
    print("固定區段:")
    check("0x000000 bootloader magic 0xE9", len(d) > 0 and d[0] == 0xE9)
    check("0x008000 分割表 magic 0xAA50",
          len(d) > 0x8002 and struct.unpack_from("<H", d, 0x8000)[0] == 0x50AA)

    app = next((p for p in parts if p["type"] == "app"), None)
    if app:
        o = app["offset"]
        check("0x%06x app magic 0xE9" % o, len(d) > o and d[o] == 0xE9)
        ok_desc = len(d) > o + 0x24 and struct.unpack_from("<I", d, o + 0x20)[0] == 0xABCD5432
        ver = d[o + 0x30:o + 0x50].split(b"\x00")[0].decode("ascii", "replace") if ok_desc else ""
        check("0x%06x app descriptor" % o, ok_desc, ("版本 %s" % ver) if ver else "")
        # 描述元有內容才有意義:版本欄是 "1"、專案名是 libespidf 等於沒填
        check("app 版本號有意義(非空、非 '1')", ok_desc and ver not in ("", "1"),
              "目前 %r" % ver)

    print()
    print("資料分割區:")
    for p in parts:
        name = p["name"]
        o, size = p["offset"], p["size"]
        if name in REQUIRED_DATA_PARTITIONS:
            if len(d) <= o:
                check("%s @0x%06x 有內容" % (name, o), False,
                      "映像只有 0x%06x,根本沒延伸到這個位址" % len(d))
                continue
            seg = d[o:min(o + 4096, len(d))]
            blank = len(collections.Counter(seg)) == 1 and seg[0] in (0x00, 0xFF)
            check("%s @0x%06x 有內容 (%s)" % (name, o, REQUIRED_DATA_PARTITIONS[name]),
                  not blank, "整段空白" if blank else "")

            # 只看前 4096 bytes 是不是空白太寬鬆:映像被截斷、模型只寫了一半、
            # 或根本寫錯檔案,這三種都能矇混過關 —— 而那正是這支腳本要擋的東西。
            # 有原始檔可比對時,直接對整段做 sha256。
            src = EXPECTED_SOURCE.get(name)
            if src and os.path.exists(src):
                want = open(src, "rb").read()
                enough = len(d) >= o + len(want)
                check("%s 完整長度 (%d bytes)" % (name, len(want)), enough,
                      "" if enough else "映像只到 0x%06x,少了 %d bytes"
                                        % (len(d), o + len(want) - len(d)))
                if enough:
                    got = d[o:o + len(want)]
                    same = hashlib.sha256(got).digest() == hashlib.sha256(want).digest()
                    check("%s 內容 sha256 與來源相符" % name, same,
                          "" if same else "映像內的資料與 %s 不一致" % os.path.basename(src))
            elif src:
                print("  跳過  %s sha256 比對 — 找不到來源 %s" % (name, src))
        elif name in MUST_BE_BLANK:
            if len(d) <= o:
                check("%s @0x%06x 留白" % (name, o), True, "不在映像範圍內")
                continue
            seg = d[o:min(o + 4096, len(d))]
            blank = len(collections.Counter(seg)) == 1 and seg[0] in (0x00, 0xFF)
            check("%s @0x%06x 留白 (%s)" % (name, o, MUST_BE_BLANK[name]), blank,
                  "" if blank else "有資料!燒這顆映像會覆蓋使用者設定")

    print()
    if fails:
        print("失敗 %d 項:" % len(fails))
        for f in fails:
            print("  -", f)
        sys.exit(1)
    print("全部通過")


if __name__ == "__main__":
    main()
