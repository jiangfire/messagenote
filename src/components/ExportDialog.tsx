import { useState } from "react";
import { useApi } from "../lib/apiContext";
import { errorText } from "../lib/errors";
import type { Channel, ExportFilter, ExportSummary, TagCount } from "../lib/types";

interface Props {
  channels: Channel[];
  tags: TagCount[];
  onClose: () => void;
}

/** 选项值 `""` 表示"不限" —— 和 `<select>` 里那个空 option 一一对应。 */
const ANY = "";

/**
 * `YYYY-MM-DD`（`<input type="date">` 的格式）→ 本地时间当天的起点/终点毫秒。
 *
 * **不能用 `new Date("2026-03-01")`**：那个字符串按 ISO 规则解析成 **UTC**
 * 午夜，在东八区就变成了 3 月 1 日早上 8 点 —— 于是"从 3 月 1 日开始"会漏掉
 * 3 月 1 日凌晨记的东西，而界面上看不出任何异常。所以这里显式拆成年月日，
 * 交给 `new Date(y, m, d)` 走**本地**日历。
 *
 * 和文件名那套一致：时区语义只在客户端折算，Rust 侧只收到一个绝对毫秒数。
 */
function dayEdgeMs(day: string, edge: "start" | "end"): number {
  const [y, m, d] = day.split("-").map(Number);
  return edge === "start"
    ? new Date(y, m - 1, d).getTime()
    : new Date(y, m - 1, d, 23, 59, 59, 999).getTime();
}

/**
 * 导出：先定范围，再挑目录 / 下一个 zip。
 *
 * **两端形态不同，但筛选 UI 是同一份** —— 两套筛选控件会漂移，而漂移的后果
 * 是"我以为导的是 3 月，用户在另一个界面上导的是 4 月"。
 *
 * - 桌面端：挑一个目录，写出一棵 Markdown 目录树（`desktop.exportMarkdown`）
 * - 网页端：服务端打成一个 zip，浏览器下载（`exportZip`）
 *
 * 后者是前者的降级形态，**不是等价替代**：浏览器里写不出一棵目录树，只能
 * 给一个文件。界面上要说清这一点，别让用户以为两种做法产出一样的东西。
 *
 * 导出结果就地说在这儿，不用 `window.alert`：一是它能和用户刚填的筛选条件
 * 待在一起（"筛了这些 → 出了这些"），二是 alert 会挡住目录选择器的收尾，
 * 在 Windows 上偶尔得点两次才消失。
 */
export function ExportDialog({ channels, tags, onClose }: Props) {
  const { desktop, exportZip } = useApi();

  const [channelId, setChannelId] = useState(ANY);
  const [tag, setTag] = useState(ANY);
  const [from, setFrom] = useState("");
  const [to, setTo] = useState("");
  const [busy, setBusy] = useState(false);
  /**
   * 结果提示。三档而不是两档：
   * - `ok` —— 导出来了，报告数量和位置
   * - `info` —— 做完了、但结果为空。**这不是错误**，用红的那档会读成"出错了"
   * - `err` —— 真的失败了（或者条件写反了）
   */
  const [note, setNote] = useState<{ kind: "ok" | "info" | "err"; text: string } | null>(null);

  // 两种形态都不支持 = 这个环境没有导出能力。**不要渲染一个点下去没反应的
  // 按钮**：调用方按这个条件决定要不要挂这个面板。
  const webReady = !desktop && !!exportZip;
  if (!desktop && !exportZip) return null;

  // 起止都填了才比较。先后颠倒就**先拦住**，而不是导出一棵空目录树 ——
  // 空结果会被当成"这个筛选下没有记录"，而真相是条件写反了。
  const reversed = from !== "" && to !== "" && dayEdgeMs(from, "start") > dayEdgeMs(to, "end");

  async function run() {
    setBusy(true);
    setNote(null);
    try {
      const filter: ExportFilter = {
        channelId: channelId || null,
        tag: tag || null,
        fromMs: from ? dayEdgeMs(from, "start") : null,
        toMs: to ? dayEdgeMs(to, "end") : null,
      };
      // 偏移传 `-getTimezoneOffset()`：它返回"本地转 UTC 要加多少分钟"，
      // 取负才是"东为正"的口径，也正是 Rust 那边要的。
      const offset = -new Date().getTimezoneOffset();

      let where: string;
      let s: ExportSummary;
      if (desktop) {
        // 挑目录也放在 try 里：它自己会失败（权限、被策略拦），而抛在 try 外面
        // 就是一个没人接的 rejection —— 面板上什么都不显示。
        const dir = await desktop.pickDirectory();
        if (!dir) return; // 用户取消 —— 不该弹一句"导出失败"
        s = await desktop.exportMarkdown(dir, offset, filter);
        where = dir;
      } else if (webReady) {
        s = await exportZip!(filter, offset);
        where = "浏览器下载目录";
      } else {
        return;
      }

      if (s.messages === 0) {
        setNote({ kind: "info", text: "这个筛选下没有记录，没有导出任何东西。" });
        return;
      }
      // **取不到的附件必须说出来。** 不说的话用户只会以为导出漏了东西 ——
      // 而导出物里那条 `attachment:<sha>` 他看不懂是什么意思。
      const missed = s.missingAttachments
        ? `\n有 ${s.missingAttachments} 个附件还没有字节，没有带出来（正文里仍是 attachment: 引用）。` +
          `等同步把它们拿下来再导一次即可。`
        : "";
      setNote({
        kind: "ok",
        text:
          `已导出 ${s.messages} 条记录、${s.attachments} 个附件` +
          `${desktop ? `到 ${s.channels} 个频道目录` : ""}。\n` +
          `位置：${where}${missed}`,
      });
    } catch (e) {
      setNote({ kind: "err", text: `导出失败：${errorText(e)}` });
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="modal-backdrop" onMouseDown={onClose}>
      <div className="modal" onMouseDown={(e) => e.stopPropagation()}>
        <h2>导出记录</h2>
        <p className="muted small">
          按频道、标签、时间范围挑出一部分导出（都是可选的，全不选就是导出全部）。
          产物是一棵 Markdown 目录树：按频道分目录，图片进 attachments/。
          {webReady && " 网页端下载的是一个 zip 文件，解开后是同一棵目录树。"}
        </p>

        <label className="field">
          <span>频道</span>
          <select
            value={channelId}
            disabled={busy}
            onChange={(e) => setChannelId(e.target.value)}
          >
            <option value={ANY}>全部频道</option>
            {channels.map((c) => (
              <option key={c.id} value={c.id}>
                {c.name}（{c.messageCount}）
              </option>
            ))}
          </select>
        </label>

        <label className="field">
          <span>标签</span>
          <select value={tag} disabled={busy} onChange={(e) => setTag(e.target.value)}>
            <option value={ANY}>全部标签</option>
            {tags.map((t) => (
              <option key={t.name} value={t.name}>
                {t.name}（{t.count}）
              </option>
            ))}
          </select>
        </label>

        <label className="field">
          <span>起始日期</span>
          <input
            type="date"
            value={from}
            max={to || undefined}
            disabled={busy}
            onChange={(e) => setFrom(e.target.value)}
          />
        </label>

        <label className="field">
          <span>结束日期</span>
          <input
            type="date"
            value={to}
            min={from || undefined}
            disabled={busy}
            onChange={(e) => setTo(e.target.value)}
          />
        </label>

        <p className="muted small">
          两个日期都算在内（含当天 00:00 到 23:59:59.999）。留空表示不限。
        </p>

        {reversed && <div className="note err">起始日期晚于结束日期，这样导不出任何东西。</div>}
        {note && <div className={`note ${note.kind}`}>{note.text}</div>}

        <div className="modal-actions">
          <button className="btn primary" onClick={() => void run()} disabled={busy || reversed}>
            {busy ? "正在导出…" : webReady ? "导出为 zip" : "选择目录并导出"}
          </button>
          <button className="btn ghost" onClick={onClose}>
            关闭
          </button>
        </div>
      </div>
    </div>
  );
}
