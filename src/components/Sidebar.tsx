import { useState } from "react";
import type { Channel, TagCount, TimelineStats, View } from "../lib/types";

interface Props {
  channels: Channel[];
  tags: TagCount[];
  view: View;
  stats: TimelineStats;
  onSelect: (v: View) => void;
  onCreateChannel: (name: string) => void;
  onDeleteChannel: (id: string) => void;
  /**
   * 导出全部记录。
   *
   * **只有桌面端会传它。** 浏览器里写不出一棵目录树（按频道分目录 +
   * `attachments/`），所以不传时整个入口都不渲染 —— 而不是渲染一个
   * 点下去没反应的按钮。
   */
  onExport?: () => void;
}

/**
 * 侧边栏。分工是明确的：**频道管归属（互斥），标签管横切（可多个）**。
 *
 * 两个刻意的决定：
 *
 * 1. **时间线是唯一的浏览面，"未归档"只是它上方的一个筛选**（见 App 里的
 *    筛选条），不是这里的第二个导航项。上一版把「时间线 13」和「收件箱 12」
 *    并排列出来，只要用户不怎么打标签，两个数字就几乎一样，
 *    看起来就是同一个视图重复了两遍。
 *
 * 2. **收件箱不出现在频道列表里。** 它是"未归档"这个状态的代表，
 *    再列一次就是重复。它仍然存在（消息默认落在那儿），
 *    只是你通过"移动到频道"菜单把东西移回去。
 */
export function Sidebar({
  channels,
  tags,
  view,
  stats,
  onSelect,
  onCreateChannel,
  onDeleteChannel,
  onExport,
}: Props) {
  const [adding, setAdding] = useState(false);
  const [draft, setDraft] = useState("");

  // 收件箱（kind = 'inbox'）不在频道列表里露脸，理由见上面的文档注释
  const named = channels.filter((c) => c.kind !== "inbox");

  function commit() {
    const name = draft.trim();
    if (name) onCreateChannel(name);
    setDraft("");
    setAdding(false);
  }

  return (
    <aside className="sidebar">
      {/* 没有品牌块：窗口标题栏已经写着应用名了，这里再来一遍就是重复 ——
          省下的垂直空间让给频道和标签列表。 */}
      <nav className="nav">
        <button
          className={`nav-item ${view.type === "timeline" ? "active" : ""}`}
          onClick={() => onSelect({ type: "timeline", unfiledOnly: false })}
          title="全部记录，可以一直往前翻"
        >
          <span className="nav-name">时间线</span>
          <span className="nav-count">{stats.total}</span>
        </button>
      </nav>

      <div className="section">
        <div className="section-head">
          <span>频道</span>
          <button className="icon-btn" title="新建频道" onClick={() => setAdding((v) => !v)}>
            ＋
          </button>
        </div>

        {adding && (
          <input
            className="inline-input"
            autoFocus
            value={draft}
            placeholder="频道名，回车确认"
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") commit();
              if (e.key === "Escape") {
                setDraft("");
                setAdding(false);
              }
            }}
            onBlur={commit}
          />
        )}

        {named.map((c) => (
          <div
            key={c.id}
            className={`nav-item group ${
              view.type === "channel" && view.id === c.id ? "active" : ""
            }`}
          >
            <button
              className="nav-main"
              onClick={() => onSelect({ type: "channel", id: c.id })}
            >
              <span className="nav-name">#{c.name}</span>
              <span className="nav-count">{c.messageCount}</span>
            </button>
            <button
              className="row-action"
              title="删除频道（其中的记录会回到收件箱）"
              onClick={() => {
                if (confirm(`删除频道「${c.name}」？其中的记录会回到收件箱。`)) {
                  onDeleteChannel(c.id);
                }
              }}
            >
              ×
            </button>
          </div>
        ))}

        {named.length === 0 && !adding && (
          <p className="hint">
            还没有频道。想按主题归档就建一个 —— 记录时不用管它们，
            先落到收件箱，以后再移。
          </p>
        )}
      </div>

      <div className="section">
        <div className="section-head">
          <span>标签</span>
        </div>
        {tags.length === 0 ? (
          <p className="hint">标签是横切的补充标记，和频道正交。可以不打。</p>
        ) : (
          <div className="tag-cloud">
            {tags.map((t) => (
              <button
                key={t.name}
                className={`tag-chip ${
                  view.type === "tag" && view.name === t.name ? "active" : ""
                }`}
                onClick={() => onSelect({ type: "tag", name: t.name })}
              >
                {t.name}
                <span className="tag-count">{t.count}</span>
              </button>
            ))}
          </div>
        )}
      </div>
      {onExport && (
        <div className="section sidebar-foot">
          <button
            className="nav-item"
            onClick={onExport}
            title="把全部记录导出成 Markdown（按频道分目录，图片一起带走）"
          >
            <span className="nav-name">导出全部…</span>
          </button>
        </div>
      )}
    </aside>
  );
}
