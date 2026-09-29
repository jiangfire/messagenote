"""查库断言：正文里写的 sha，和库里那份字节，对不对得上。

配合同目录的 paste-verify.ps1 使用（它负责合成真实按键，这里只负责验数据）。

验两件事，缺一不可：

1. **内容寻址是对的** —— 库里这条记录的字节算出来 sha256，必须等于正文里写的那个
   sha。附件是按内容寻址的，名字错了意味着"所有设备都信任一个错误的名字"，
   是灾难性的。
2. **字节真的是一张 $ProbeSize x $ProbeSize 的 PNG** —— 只断言"有字节"是不够的：
   字节全错时它照样通过。这和浏览器 E2E 里断言 `naturalWidth === 12` 是同一条
   思路（那边也是拒绝"img 存在"这种弱断言）。

尺寸直接从 PNG 的 IHDR 里读，不依赖 Pillow。
"""

import argparse
import hashlib
import os
import re
import sqlite3
import sys

ATTACHMENT_REF = re.compile(r"attachment:([0-9a-f]{64})")


def png_size(data: bytes):
    """从 IHDR 里读宽高。不是 PNG 就返回 None。"""
    if len(data) < 24 or data[:8] != b"\x89PNG\r\n\x1a\n":
        return None
    if data[12:16] != b"IHDR":
        return None
    return (int.from_bytes(data[16:20], "big"), int.from_bytes(data[20:24], "big"))


def connect(db_path: str) -> sqlite3.Connection:
    # 优先只读打开：应用正跑着，不该让验证脚本有机会写坏它。
    # WAL 模式下只读连接需要 -shm，应用在跑时它是存在的。
    try:
        conn = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
        conn.execute("SELECT 1 FROM message LIMIT 1").fetchall()
        return conn
    except sqlite3.Error as e:
        print(f"  只读打开失败（{e}），改用普通连接（不会写）")
        return sqlite3.connect(db_path)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--expect-size", type=int, default=12)
    ap.add_argument("--db", default=None)
    args = ap.parse_args()

    db = args.db or os.path.join(
        os.environ["APPDATA"], "com.messagenote.desktop", "messagenote.sqlite"
    )
    print(f"  库: {db}")
    if not os.path.exists(db):
        print(f"!! 找不到库文件")
        return 1

    conn = connect(db)
    conn.row_factory = sqlite3.Row

    # 这份库可能是**加了附件之前**的老库（SCHEMA_VERSION 1）。应用启动时会跑
    # v1 -> v2 迁移，迁移完这张表才存在。所以这里给一句能看懂的话，
    # 而不是一个 sqlite3.OperationalError 回溯。
    have = conn.execute(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name='attachment'"
    ).fetchone()
    if not have:
        print("!! 库里没有 attachment 表 —— 说明应用还没跑过 v1 -> v2 迁移")
        return 1

    # 只看最近几条 —— 刚粘贴的那条一定在里面，而且这样不会把整库扫一遍。
    rows = conn.execute(
        "SELECT id, body, created_at FROM message "
        "WHERE deleted_at IS NULL AND body LIKE '%attachment:%' "
        "ORDER BY created_at DESC LIMIT 5"
    ).fetchall()

    if not rows:
        print("!! 最近没有任何带附件的记录 —— 粘贴那一步大概没成功")
        return 1

    for row in rows:
        shas = ATTACHMENT_REF.findall(row["body"])
        if not shas:
            continue
        for sha in shas:
            blob = conn.execute(
                "SELECT mime, size, bytes FROM attachment WHERE sha256 = ?", (sha,)
            ).fetchone()
            if blob is None:
                print(f"  sha {sha[:12]}… 在正文里，但 attachment 表里没有这行")
                continue
            data = blob["bytes"]
            if data is None:
                print(f"  sha {sha[:12]}… 只有占位行（bytes 为 NULL，等于还没下载）")
                continue

            print(f"  记录 {row['id']}")
            print(f"  sha  {sha}")
            print(f"  mime {blob['mime']}   size 列={blob['size']}   实际字节={len(data)}")

            actual = hashlib.sha256(data).hexdigest()
            if actual != sha:
                print(f"!! 内容寻址错了：字节算出来是 {actual}")
                return 1
            print("  ✓ 字节 sha256 == 正文里写的 sha")

            dims = png_size(data)
            want = (args.expect_size, args.expect_size)
            if dims is None:
                print("!! 不是 PNG —— 字节可能被改过或截断了")
                return 1
            if dims != want:
                print(f"!! 尺寸是 {dims[0]}x{dims[1]}，期望 {want[0]}x{want[1]}")
                return 1
            print(f"  ✓ 解出来真的是 {dims[0]}x{dims[1]} 的 PNG")

            if blob["size"] != len(data):
                print(f"!! size 列（{blob['size']}）和实际字节数（{len(data)}）不一致")
                return 1
            return 0

    print("!! 没找到一条既引用附件、又能取到字节的记录")
    return 1


if __name__ == "__main__":
    sys.exit(main())
