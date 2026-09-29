"""检查 export-verify.mjs 真的导出了什么。

**重点不是"文件在不在"，而是"正文里写的相对路径能不能真取到那份字节，
而且字节和库里那份逐字节一样"。** 只断言"有个 ../attachments/xxx.png 字符串"
是弱断言 —— 文件名算错时它照样通过。
"""

import argparse
import hashlib
import json
import os
import re
import sys

# 工作目录默认落在仓库根的 .scratch/ 下（gitignore 的临时区）。
# **用脚本位置推仓库根，不写死路径。** `--work` 可以覆盖。
_REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
_ap = argparse.ArgumentParser()
_ap.add_argument("--work", default=os.path.join(_REPO, ".scratch", "export-verify"))
_args = _ap.parse_args()
WORK = _args.work
OUT = os.path.join(WORK, "out")
RESULT = os.path.join(WORK, "result.json")
PNG = bytes([137, 80, 78, 71, 13, 10, 26, 10] + [0] * 24)

fails = []


def check(cond, what):
    if cond:
        print(f"  ✓ {what}")
    else:
        print(f"  !! {what}")
        fails.append(what)


def all_files(root):
    out = []
    for d, _, fs in os.walk(root):
        for f in fs:
            out.append(os.path.join(d, f))
    return out


res = json.load(open(RESULT, encoding="utf-8"))
TAG = res["tag"]
s = res["export"]

print(f"本次运行的标记: {TAG}")
print()

# 只按标记找**本次运行**写下的东西。
#
# 断言里**不写死"一共几条"**：这套脚本会真的往库里写数据（走应用自己的写命令），
# 所以第二次跑的时候库里还有上一次留下的记录 —— 那**不是**产品的问题，
# 却会让"messages == 2"这种断言红掉。带上标记之后脚本可以反复跑。
mine = sorted(
    p for p in all_files(OUT) if p.endswith(".md") and TAG in os.path.basename(p)
)

print("=== 1) 本次运行的文件树 ===")
for p in mine:
    print(f"  {os.path.relpath(p, OUT)}")
check(len(mine) == 2, f"本次运行写下 2 个 .md（实得 {len(mine)}）")
if len(mine) != 2:
    print()
    print("!! 数量不对，后面的检查没法继续（导出或写命令可能坏了）")
    for f in fails:
        print(f"   - {f}")
    sys.exit(1)

dirs = {os.path.dirname(os.path.relpath(p, OUT)) for p in mine}
check(len(dirs) == 2, f"它们落在 2 个目录里：{sorted(dirs)}")
check(any(d.startswith("项目A") for d in dirs), "有按频道分的目录「项目A」")
check("收件箱" in dirs, "收件箱也是一个目录（未归档的落在这里）")
check(os.path.isdir(os.path.join(OUT, "attachments")), "有 attachments/")

print()
print("=== 2) 频道那条：front-matter 与附件路径 ===")
chan = next(p for p in mine if os.path.basename(os.path.dirname(p)).startswith("项目A"))
name = os.path.basename(chan)
print(f"  文件名：{name}")
check(
    re.match(r"^\d{4}-\d{2}-\d{2}-\d{4} .+\.md$", name) is not None,
    "文件名形如 `<本地时间标签> <摘要>.md`",
)
check(not any(c in name for c in '<>:"/\\|?*'), "文件名里没有 Windows 非法字符")

md = open(chan, encoding="utf-8").read()
check(md.startswith("---\n"), "以 front-matter 开头")
check(f'channel: "项目A {TAG}"' in md, "front-matter 里有频道名")
# 标签顺序由 store 决定（按名字排），不是插入顺序 —— 所以断言内容而不是顺序。
# 它是**稳定**的，导出可复现这条才成立。
check("tags:" in md and "重要" in md and "待办" in md, "front-matter 里有两个标签")
check(
    re.search(r"created: \d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\+08:00", md) is not None,
    "created 是带 +08:00 偏移的本地时间（偏移是调用方给的）",
)
check("attachment:" not in md, "旧的 attachment:<sha> 引用已经被替换掉")

m = re.search(r"\]\(\.\./attachments/([0-9a-f]{64}\.\w+)\)", md)
check(m is not None, "正文里的图指向 ../attachments/<sha>.<ext>")

print()
print("=== 3) 那个路径真的指得到字节吗（最强的一条）===")
if m is None:
    print("  （没有相对路径可解 —— 上面那条已经红了）")
else:
    rel = m.group(1)
    # 相对路径是相对**那个 .md 文件**的，不是相对导出根
    resolved = os.path.normpath(
        os.path.join(os.path.dirname(chan), "..", "attachments", rel)
    )
    check(os.path.isfile(resolved), f"解出来的路径存在：{os.path.relpath(resolved, OUT)}")
    if os.path.isfile(resolved):
        data = open(resolved, "rb").read()
        check(
            hashlib.sha256(data).hexdigest() == rel.split(".")[0],
            "正文里写的 sha == 那份字节的 sha256（内容寻址没被破坏）",
        )
        check(data == PNG, "导出的字节和存进去的逐字节一致")
        check(rel.endswith(".png"), "png 没有退化成别的扩展名")

print()
print("=== 4) 收件箱那条 ===")
inbox = next(p for p in mine if os.path.basename(os.path.dirname(p)) == "收件箱")
md2 = open(inbox, encoding="utf-8").read()
check('channel: "收件箱"' in md2, "收件箱那条的频道名是「收件箱」")
check("地铁里随手记的一句" in md2, "正文在")

print()
print("=== 5) 摘要和文件树对不对得上 ===")
# 拿摘要和实际文件树对照，而不是写死数字 —— 既避开了"库里还有上次残留"，
# 又比"等于 2"更强：它验的是**命令自己报的数准不准**。
print(f"  {s}")
all_md = [p for p in all_files(OUT) if p.endswith(".md")]
att_dir = os.path.join(OUT, "attachments")
att_files = os.listdir(att_dir) if os.path.isdir(att_dir) else []
dirs_all = [
    d
    for d in os.listdir(OUT)
    if os.path.isdir(os.path.join(OUT, d)) and d != "attachments"
]
check(s["messages"] == len(all_md), f"条数对得上（{s['messages']} vs {len(all_md)}）")
check(
    s["attachments"] == len(att_files),
    f"附件数对得上（{s['attachments']} vs {len(att_files)}）",
)
check(
    s["channels"] == len(dirs_all),
    f"频道目录数对得上（{s['channels']} vs {len(dirs_all)}）",
)
# 「取不到的附件」那条路走单元测试：这里每次跑都会新增一个悬空引用，
# 而摘要数的是**去重后的 sha**，和"几个文件里还留着引用"本来就不是一回事。
check(s["missingAttachments"] >= 1, "悬空引用被数进了摘要（取值由单元测试钉死）")

print()
print("=== 6) 单条复制那条路 ===")
one = res["one"]
check(one is not None, "render_message_markdown 返回了内容")
check(
    one is not None and one.startswith("---\n"),
    "也带 front-matter（和导出同一个渲染器）",
)
# 这条**改过**：单条复制现在把图内联成 data URI（剪贴板里没有"文件"这个概念）。
# 原来的断言是"保留 attachment:<sha>"，那是旧行为。
check(
    one is not None and "attachment:" not in one,
    "不给别的程序留看不懂的 attachment: 引用",
)
check(one is not None and "data:image/png;base64," in one, "图被内联成 data URI")
check(res["gone"] is None, "不存在的 id 返回 null，而不是抛错")

print()
if fails:
    print(f"!! {len(fails)} 条断言没过")
    for f in fails:
        print(f"   - {f}")
    sys.exit(1)
print("全部通过")
