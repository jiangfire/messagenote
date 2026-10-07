import { useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import type { Channel, Message, TagSuggestion } from "../lib/types";
import { dayKey, formatDayLabel, formatTime } from "../lib/format";
import { Markdown } from "./Markdown";
import { Avatar } from "./Avatar";
import { useApi } from "../lib/apiContext";
import { errorText } from "../lib/errors";

interface Props {
  messages: Message[];
  channels: Channel[];
  /** 检索结果视图下用来标注消息原始所属频道 */
  channelLabel?: (m: Message) => string | null;
  onEdit: (id: string, body: string) => Promise<void>;
  onDelete: (id: string) => Promise<void>;
  /** 归档到频道 —— 这是主要的整理动作 */
  onMove: (id: string, channelId: string) => Promise<void>;
  onTags: (id: string, tags: string[]) => Promise<void>;
  /**
   * 采纳 AI 建议之后重新读一次列表。
   *
   * **它和 `onTags` 是两件事**：采纳时标签已经由 Rust 命令写进库了，
   * 这里只需要重新读取来反映出来。走 `onTags` 会用渲染快照**整体重写**
   * 标签，把刚采纳的那条悄悄覆盖掉。
   */
  onAccepted?: (id: string) => Promise<void>;
  /** 还有更早的记录可以往前翻 */
  hasMore?: boolean;
  loadingOlder?: boolean;
  /** 请求加载更早的一页（由父组件负责取数并 prepend） */
  onLoadOlder?: () => Promise<void>;
  /** 空列表时说什么。不同视图的"空"含义完全不同，所以由调用方决定。 */
  emptyTitle?: string;
  emptyBody?: ReactNode;
  /**
   * "加载更多"放哪一头。
   *
   * - `top`（默认）：时间线。往上是**更早**的记录，滚到接近顶部就自动加载。
   * - `bottom`：检索结果。更**相关**的在上面，往下才是更多，用一个明确的按钮 ——
   *   检索是瞬态的，自动加载会让人以为"就这些"。
   */
  moreAt?: "top" | "bottom";
}

/** 距顶部多少像素开始预加载下一页。留出余量，用户不会撞到"墙"。 */
const LOAD_MORE_THRESHOLD_PX = 320;

/**
 * 把一段文字放进剪贴板，成功返回 true。
 *
 * Clipboard API 只在安全上下文（HTTPS / localhost）里存在，而服务端部署
 * 常常就是明文 HTTP —— 这时候 `navigator.clipboard` 是 undefined，
 * 直接调会抛异常，用户看到的是"点了没反应"。
 *
 * 降级路径用一个屏幕外的 textarea + `execCommand("copy")`：它早就被标记为
 * 废弃，但在不安全的上下文里是唯一还能用的办法。拿不到用户手势之外的
 * 权限时会失败，所以返回 false 而不是假装成功。
 */
async function writeClipboard(text: string): Promise<boolean> {
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(text);
      return true;
    }
  } catch {
    // 落到下面的降级路径
  }

  try {
    const ta = document.createElement("textarea");
    ta.value = text;
    // 放在视口外而不是 display:none —— 隐藏元素无法被选中，命令会失败。
    ta.setAttribute("readonly", "");
    ta.style.position = "fixed";
    ta.style.top = "0";
    ta.style.opacity = "0";
    document.body.appendChild(ta);
    ta.select();
    const ok = document.execCommand("copy");
    document.body.removeChild(ta);
    return ok;
  } catch {
    return false;
  }
}

export function Stream({
  messages,
  channels,
  channelLabel,
  onEdit,
  onDelete,
  onMove,
  onTags,
  onAccepted,
  hasMore = false,
  loadingOlder = false,
  onLoadOlder,
  emptyTitle,
  emptyBody,
  moreAt = "top",
}: Props) {
  const scroller = useRef<HTMLDivElement>(null);
  const pinned = useRef(true);
  /** 触发加载时记下的"距底部距离"，用来在内容插入后把视觉位置还原。 */
  const restoreFromBottom = useRef<number | null>(null);
  const prevFirstId = useRef<string | null>(null);
  const inFlight = useRef(false);

  async function triggerLoadOlder() {
    const el = scroller.current;
    if (!el || !hasMore || inFlight.current || !onLoadOlder) return;
    inFlight.current = true;
    // 记住"距底部的距离"：往上方插入内容不会改变这个值，
    // 所以之后把它还原回去，视觉位置就完全不动 —— 用户不会感觉被弹走。
    restoreFromBottom.current = el.scrollHeight - el.scrollTop;
    try {
      await onLoadOlder();
    } finally {
      inFlight.current = false;
    }
  }

  function onScroll() {
    const el = scroller.current;
    if (!el) return;
    // 只有用户本来就在底部时才自动跟随，否则往上翻历史会被不断拽回底部
    pinned.current = el.scrollHeight - el.scrollTop - el.clientHeight < 80;
    if (el.scrollTop < LOAD_MORE_THRESHOLD_PX) void triggerLoadOlder();
  }

  useLayoutEffect(() => {
    const el = scroller.current;
    if (!el) return;

    const nowFirst = messages[0]?.id ?? null;
    const prepended =
      prevFirstId.current !== null &&
      nowFirst !== prevFirstId.current &&
      restoreFromBottom.current !== null;
    prevFirstId.current = nowFirst;

    if (prepended) {
      el.scrollTop = el.scrollHeight - (restoreFromBottom.current ?? 0);
      restoreFromBottom.current = null;
      return;
    }

    // 不是"上方插入"的情况：清掉残留，避免下一次无关更新触发错误的还原
    restoreFromBottom.current = null;
    if (pinned.current) el.scrollTop = el.scrollHeight;
  }, [messages]);

  if (messages.length === 0) {
    return (
      <div className="stream-empty">
        <div className="empty-card">
          <h3>{emptyTitle ?? "这里还是空的"}</h3>
          {emptyBody ?? <p className="muted">在下面写点什么，按 Enter 就记下了。</p>}
        </div>
      </div>
    );
  }

  let lastDay = "";

  return (
    <div
      className="stream"
      ref={scroller}
      // 只有时间线才"滚到顶自动加载"。检索结果是往下翻的，
      // 滚回顶部去加载更多会让人一头雾水。
      onScroll={moreAt === "top" ? onScroll : undefined}
    >
      {moreAt === "top" && (
        <>
          {hasMore || loadingOlder ? (
            <div className="load-older">
              {loadingOlder
                ? "正在加载更早的记录…"
                : `继续往上翻可以看更早的记录（已载入 ${messages.length} 条）`}
            </div>
          ) : (
            <div className="load-older start">
              —— 这里是你能回溯到的最早一条（共 {messages.length} 条）——
            </div>
          )}
        </>
      )}
      {messages.map((m) => {
        const key = dayKey(m.createdAt);
        const showDay = key !== lastDay;
        lastDay = key;

        return (
          <div key={m.id}>
            {showDay && (
              <div className="day-sep">
                <span>{formatDayLabel(m.createdAt)}</span>
              </div>
            )}
            <MessageRow
              message={m}
              channels={channels}
              label={channelLabel?.(m) ?? null}
              onEdit={onEdit}
              onDelete={onDelete}
              onMove={onMove}
              onTags={onTags}
              onAccepted={onAccepted}
            />
          </div>
        );
      })}

      {moreAt === "bottom" && (
        <div className="load-more-row">
          {hasMore ? (
            <button
              className="btn"
              onClick={() => void triggerLoadOlder()}
              disabled={loadingOlder}
            >
              {loadingOlder ? "正在加载…" : "加载更多结果"}
            </button>
          ) : (
            <span className="muted small">—— 没有更多了（共 {messages.length} 条）——</span>
          )}
        </div>
      )}
    </div>
  );
}

interface RowProps {
  message: Message;
  channels: Channel[];
  label: string | null;
  onEdit: (id: string, body: string) => Promise<void>;
  onDelete: (id: string) => Promise<void>;
  onMove: (id: string, channelId: string) => Promise<void>;
  onTags: (id: string, tags: string[]) => Promise<void>;
  onAccepted?: (id: string) => Promise<void>;
}

function MessageRow({ message, channels, label, onEdit, onDelete, onMove, onTags, onAccepted }: RowProps) {
  const { desktop } = useApi();

  /**
   * 复制这一条。
   *
   * 桌面端复制的是**渲染过的 Markdown**（带 front-matter 的频道/标签/时间），
   * 和全量导出**走同一个渲染器** —— 各写一份的话，同一段内容会变成两种样子。
   *
   * 附件带不走（剪贴板里放不了文件），所以正文里仍是 `attachment:<sha>`。
   * 取不到渲染结果时退回复制原文：那仍然是这条笔记的文字，丢的只是外挂的
   * 元信息，比让「复制」什么都没发生要好。
   *
   * **而且必须给出反馈。** 复制这个动作看不见、摸不到，而明文 HTTP 下
   * Clipboard API 根本不存在 —— 不给反馈的话用户只能反复点，
   * 分不清是没点到还是失败了。
   */
  const [copyState, setCopyState] = useState<"ok" | "fail" | null>(null);

  async function copyMarkdown() {
    let text = message.body;
    if (desktop) {
      try {
        const md = await desktop.renderMessageMarkdown(
          message.id,
          -new Date().getTimezoneOffset()
        );
        // null = 这条已经不在了（在别处被删掉）。退回原文比什么都不做强。
        text = md ?? message.body;
      } catch {
        text = message.body;
      }
    }

    const ok = await writeClipboard(text);
    setCopyState(ok ? "ok" : "fail");
    // 成功也撤掉：不然「已复制」会一直挂着，变成一个假状态
    setTimeout(() => setCopyState(null), 2500);
  }

  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(message.body);
  const [tagging, setTagging] = useState(false);
  const [tagDraft, setTagDraft] = useState("");
  const [moving, setMoving] = useState(false);

  // ---------------------------------------------------------- AI 标签建议
  //
  // **建议态，不是标签。** 点一下才生效，而且生效前它和真标签在视觉上必须
  // 分得开 —— 见下面 `.tag-chip.suggested` 的样式。
  const [suggestions, setSuggestions] = useState<TagSuggestion[]>([]);
  const [suggesting, setSuggesting] = useState(false);
  /**
   * 采纳中的锁。连点两个建议 chip 时，第二次必须等第一次落库完再发 ——
   * 不加锁的话两个 `set_message_tags` 会并发，而它是**整体替换**。
   */
  const [accepting, setAccepting] = useState(false);
  /** 一次性提示：这一条 AI 没给出标签 / 正文被截了 / 端点报错了。 */
  const [suggestNote, setSuggestNote] = useState<string | null>(null);

  // 建议是**按消息取的**，而每一行都是一个独立的组件实例 —— 所以挂载时
  // 各读各的。没有这一下的话，用户滚回去会看到一条"凭空多了几个标签"的记录。
  useEffect(() => {
    if (!desktop) return;
    let alive = true;
    void (async () => {
      try {
        const list = await desktop.listTagSuggestions(message.id);
        if (alive) setSuggestions(list);
      } catch {
        // 读失败不该在每条记录上冒一个错 —— 没读到就是没有建议
      }
    })();
    return () => {
      alive = false;
    };
  }, [desktop, message.id]);

  async function runSuggest() {
    if (!desktop || suggesting) return;
    setSuggesting(true);
    setSuggestNote(null);
    try {
      const r = await desktop.suggestTags(message.id);
      if (r.tags.length > 0) {
        setSuggestions(await desktop.listTagSuggestions(message.id));
      }
      // **两个"没结果"都必须说出来。** 静默失败的话，用户只会以为 AI
      // 什么都没想出来 —— 而真实原因可能是正文太长、端点错了、key 不对。
      setSuggestNote(
        r.tags.length > 0
          ? null
          : r.truncated
            ? "模型没有给出标签（正文很长，它只读到了开头 4000 字）"
            : "模型没有给出标签（可能这条内容确实没什么可标的）"
      );
    } catch (e) {
      setSuggestNote(errorText(e));
    } finally {
      setSuggesting(false);
    }
  }

  /**
   * 采纳一条建议。
   *
   * **点一下才生效** —— 这是 ROADMAP 里"AI 绝不能静默改数据"那条约束在
   * 界面上的形状：模型说完就闭嘴，改不改由用户决定。
   *
   * 失败时**不能把 chip 藏掉**：那是"看起来成功了"的假状态，
   * 用户会以为标签已经加上而它并没有。
   *
   * **这里只负责刷新列表，不再调 `onTags`。** `acceptTagSuggestion`
   * 已经在 Rust 里把标签写进库了；而 `set_message_tags` 是**整体替换**，
   * 不是合并。再用 `message.tags` 这份渲染快照重写一次，同一行连点两个
   * chip 就会静默丢标签：第二次点时 Rust 写的是 A+B，随后前端拿着
   * **仍不含 A** 的旧快照发出 `[旧, B]`，A 被覆盖删除，且没有任何报错。
   */
  async function acceptSuggestion(name: string) {
    if (!desktop) return;
    if (accepting) return; // in-flight 锁：连点不该发出两次写请求
    setAccepting(true);
    try {
      await desktop.acceptTagSuggestion(message.id, name);
      setSuggestions((prev) => prev.filter((s) => s.name !== name));
      // 只刷新，不重写：数据已经在库里了。
      await onAccepted?.(message.id);
    } catch (e) {
      setSuggestNote(errorText(e));
    } finally {
      setAccepting(false);
    }
  }

  useEffect(() => {
    setDraft(message.body);
  }, [message.body]);

  async function saveEdit() {
    const body = draft.trim();
    if (!body || body === message.body) {
      setEditing(false);
      return;
    }
    await onEdit(message.id, body);
    setEditing(false);
  }

  async function addTag(raw: string) {
    const name = raw.trim().replace(/^#/, "");
    if (!name) return;
    if (!message.tags.includes(name)) {
      await onTags(message.id, [...message.tags, name]);
    }
    setTagDraft("");
  }

  async function removeTag(name: string) {
    await onTags(
      message.id,
      message.tags.filter((t) => t !== name)
    );
  }

  return (
    <div className="msg-row">
      <div className="msg-gutter">
        <span className="msg-time">{formatTime(message.createdAt)}</span>
      </div>

      <Avatar />

      <div className="msg-body">
        {label && <div className="msg-origin">来自 #{label}</div>}

        {editing ? (
          <div className="edit-box">
            <textarea
              className="edit-input"
              autoFocus
              value={draft}
              rows={Math.min(16, draft.split("\n").length + 1)}
              onChange={(e) => setDraft(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Escape") {
                  setDraft(message.body);
                  setEditing(false);
                }
                if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
                  e.preventDefault();
                  void saveEdit();
                }
              }}
            />
            <div className="edit-actions">
              <span className="muted small">Ctrl+Enter 保存 · Esc 取消</span>
              <button className="btn" onClick={() => void saveEdit()}>
                保存
              </button>
              <button
                className="btn ghost"
                onClick={() => {
                  setDraft(message.body);
                  setEditing(false);
                }}
              >
                取消
              </button>
            </div>
          </div>
        ) : (
          <div className="bubble">
            <Markdown source={message.body} />
          </div>
        )}

        {(message.tags.length > 0 || tagging || desktop) && (
          <div className="msg-tags">
            {message.tags.map((t) => (
              <span key={t} className="tag-chip small">
                {t}
                <button className="tag-x" title="移除标签" onClick={() => void removeTag(t)}>
                  ×
                </button>
              </span>
            ))}

            {/*
                建议态 chip。**必须和真标签长得不一样。**

                它是虚线边框 + 半透明的，同一行里"实心 chip"是真标签、
                "虚线 chip"是建议 —— 一眼能分出来。不这么做的后果不是"不好看"，
                而是用户分不清哪个已经生效了，于是不敢点，或者以为没生效
                又点一次（结果反而把它删了）。
            */}
            {suggestions.map((s) => (
              <button
                key={s.name}
                className="tag-chip small suggested"
                title={`模型建议的标签（${s.model}），点一下就变成真标签`}
                onClick={() => void acceptSuggestion(s.name)}
              >
                {s.name}
                <span className="tag-x">＋</span>
              </button>
            ))}

            {suggesting ? (
              <span className="muted small">正在想标签…</span>
            ) : (
              /* 按钮**恒常显示**（桌面端），不按"配没配模型"藏起来。
                 曾经想藏，但那样会带来一个更糟的问题：每行自己读一次配置，
                 用户在设置里配好了之后，**已经挂在屏幕上的那些行不会刷新**，
                 于是出现"我明明配了但按钮就是不出现"——而他没有任何办法
                 知道自己哪里没对上。
                 改成点一下给一句可操作的话（"AI 还没配好：还没填模型地址"），
                 问题就消失了。 */
              <button
                className="btn tiny ghost"
                title="让模型给这条记录提几个标签（只是建议，不会自动改你的数据）"
                onClick={() => void runSuggest()}
              >
                ＋ AI 建议标签
              </button>
            )}

            {tagging && (
              <input
                className="tag-input"
                autoFocus
                value={tagDraft}
                placeholder="标签名，回车添加"
                onChange={(e) => setTagDraft(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") {
                    e.preventDefault();
                    void addTag(tagDraft);
                  }
                  if (e.key === "Escape" || (e.key === "Backspace" && !tagDraft)) {
                    setTagging(false);
                  }
                }}
                onBlur={() => setTagging(false)}
              />
            )}
          </div>
        )}

        {suggestNote && <div className="muted small suggest-note">{suggestNote}</div>}

        {moving && (
          <div className="move-row">
            <span className="muted small">归档到：</span>
            {channels.map((c) => (
              <button
                key={c.id}
                className="btn tiny"
                disabled={c.id === message.channelId}
                onClick={() => {
                  void onMove(message.id, c.id);
                  setMoving(false);
                }}
              >
                {c.kind === "inbox" ? `📥 ${c.name}` : `#${c.name}`}
              </button>
            ))}
            <button className="btn tiny ghost" onClick={() => setMoving(false)}>
              取消
            </button>
          </div>
        )}

      </div>
      <div className="msg-actions">
        <button
          className="icon-btn"
          title="编辑"
          onClick={() => setEditing((v) => !v)}
        >
          ✎
        </button>
        <button
          className="icon-btn"
          title={desktop ? "复制成 Markdown（带频道/标签/时间）" : "复制原文"}
          onClick={() => void copyMarkdown()}
        >
          <span
            className={
              copyState === "ok" ? "copy-mark ok" : copyState === "fail" ? "copy-mark fail" : "copy-mark"
            }
          >
            {copyState === "ok" ? "✓" : copyState === "fail" ? "!" : "⧉"}
          </span>
        </button>
        <button className="icon-btn" title="添加标签" onClick={() => setTagging(true)}>
          #
        </button>
        <button
          className="icon-btn"
          title="归档到频道"
          onClick={() => setMoving((v) => !v)}
        >
          ⇄
        </button>
        <button
          className="icon-btn danger"
          title="删除"
          onClick={() => {
            if (confirm("删除这条记录？")) void onDelete(message.id);
          }}
        >
          ✕
        </button>
      </div>
    </div>
  );
}
