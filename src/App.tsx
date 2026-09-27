import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { errorText } from "./lib/errors";
import { useApi, type NoteApi } from "./lib/apiContext";
import type {
  Channel,
  Cursor,
  Message,
  SearchHit,
  SyncStatus,
  TagCount,
  TimelineStats,
  View,
} from "./lib/types";
import { Sidebar } from "./components/Sidebar";
import { Stream } from "./components/Stream";
import { Composer } from "./components/Composer";
import { SyncBadge } from "./components/SyncBadge";
import { SyncSettings } from "./components/SyncSettings";

const PAGE_SIZE = 200;
const SEARCH_DEBOUNCE_MS = 160;

/**
 * 把视图映射成时间线的 scope 参数。
 *
 * `api` 从外面传进来而不是从模块里 import：这个应用要同时跑在桌面端（Tauri
 * invoke）和网页端（HTTP）上，数据从哪儿来只有注入点知道。
 */
function fetchPage(api: NoteApi, v: View, limit: number, before: Cursor | null) {
  switch (v.type) {
    case "timeline":
      // 时间线是主视图；"未归档"只是它上方的一个筛选，不是另一个导航项
      return api.listTimeline(v.unfiledOnly ? "unfiled" : "all", {}, limit, before);
    case "channel":
      return api.listTimeline("channel", { channelId: v.id }, limit, before);
    case "tag":
      return api.listTimeline("tag", { tag: v.name }, limit, before);
  }
}

export default function App() {
  // `desktop` 在网页端是 null —— 同步配置、同步状态、捕获浮层在浏览器里
  // 都没有对应物（网页端的同步是服务端自己在做）。
  const { api, desktop } = useApi();

  const [stats, setStats] = useState<TimelineStats>({ total: 0, unfiled: 0 });
  const [channels, setChannels] = useState<Channel[]>([]);
  const [tags, setTags] = useState<TagCount[]>([]);
  const [view, setView] = useState<View>({ type: "timeline", unfiledOnly: false });
  const [messages, setMessages] = useState<Message[]>([]);
  const [hasMore, setHasMore] = useState(false);
  const [loadingOlder, setLoadingOlder] = useState(false);
  const [draft, setDraft] = useState("");
  const [query, setQuery] = useState("");
  const [results, setResults] = useState<SearchHit[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [focusSignal, setFocusSignal] = useState(0);
  const [busy, setBusy] = useState(false);
  const [syncConfigured, setSyncConfigured] = useState(false);
  const [syncStatus, setSyncStatus] = useState<SyncStatus | null>(null);
  const [settingsOpen, setSettingsOpen] = useState(false);

  const searchRef = useRef<HTMLInputElement>(null);

  const refreshSyncConfig = useCallback(async () => {
    if (!desktop) return;
    try {
      const cfg = await desktop.getSyncConfig();
      setSyncConfigured(cfg.url.trim() !== "" && cfg.token.trim() !== "");
    } catch {
      setSyncConfigured(false);
    }
  }, [desktop]);

  const refreshMeta = useCallback(async () => {
    const [st, ch, tg] = await Promise.all([
      api.timelineStats(),
      api.listChannels(),
      api.listTags(),
    ]);
    setStats(st);
    setChannels(ch);
    setTags(tg);
  }, [api]);

  /**
   * 当前已载入的条数。
   *
   * 用 ref 而不是 state：`refresh` 要用到它，但**不该因为它而重建** ——
   * 否则 refresh 的依赖里带上 messages.length，而 refresh 又是视图变化
   * effect 的依赖，会形成无限循环。
   */
  const loadedCount = useRef(0);
  useEffect(() => {
    loadedCount.current = messages.length;
  }, [messages.length]);

  const loadMessages = useCallback(
    async (v: View) => {
      // 重取时**保持已展开的窗口大小**。否则用户往回翻了很久、随手改一条记录，
      // 列表会立刻缩回最近 200 条，滚动位置也跟着跳 ——
      // 编辑一条不该让你丢掉正在看的那段历史。
      const limit = Math.max(PAGE_SIZE, loadedCount.current);
      const page = await fetchPage(api, v, limit, null);
      // 后端按时间倒序返回（便于分页），界面按正序渲染
      setMessages([...page.items].reverse());
      setHasMore(page.hasMore);
    },
    [api]
  );

  /**
   * 往前翻一页。
   *
   * 游标取当前**最早**那条消息的 `(createdAt, id)`：两个字段缺一不可，
   * 只给时间戳会在同一毫秒内的多条消息处漏掉整批记录。
   */
  const loadOlder = useCallback(async () => {
    if (loadingOlder || !hasMore) return;
    const oldest = messages[0];
    if (!oldest) return;

    setLoadingOlder(true);
    try {
      const cursor = { createdAt: oldest.createdAt, id: oldest.id };
      const page = await fetchPage(api, view, PAGE_SIZE, cursor);
      // 追加到**前面**：时间线是正序渲染的
      setMessages((prev) => [...[...page.items].reverse(), ...prev]);
      setHasMore(page.hasMore);
    } catch (e) {
      setError(errorText(e));
    } finally {
      setLoadingOlder(false);
    }
  }, [api, loadingOlder, hasMore, messages, view]);

  /** 数据变了就重取。单机 + SQLite，直接重取比维护本地缓存更不容易出 bug。 */
  const refresh = useCallback(
    async (v: View) => {
      try {
        setError(null);
        await Promise.all([refreshMeta(), loadMessages(v)]);
      } catch (e) {
        setError(errorText(e));
      }
    },
    [refreshMeta, loadMessages]
  );

  useEffect(() => {
    void refresh(view);
    // 切换视图时把焦点交还输入框：用户的下一个动作几乎总是"写"
    setFocusSignal((n) => n + 1);
  }, [view, refresh]);

  // 检索：防抖，且为空时立刻退回普通视图
  useEffect(() => {
    const q = query.trim();
    if (!q) {
      setResults(null);
      return;
    }
    const timer = setTimeout(async () => {
      try {
        setError(null);
        setResults(await api.searchMessages(q, 80));
      } catch (e) {
        setError(errorText(e));
      }
    }, SEARCH_DEBOUNCE_MS);
    return () => clearTimeout(timer);
  }, [api, query]);

  async function run(action: () => Promise<unknown>) {
    try {
      setBusy(true);
      setError(null);
      await action();
      await refresh(view);
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  }

  async function handleSend() {
    const body = draft.trim();
    if (!body) return;
    try {
      setBusy(true);
      setError(null);
      // 捕获永远落到默认落点，不做任何去向决策 ——
      // 这是整个产品"零摩擦"那一半的落点。
      // 消息落下来时还没有标签，所以它会出现在收件箱里等着整理。
      await api.appendMessage(body, null);
      setDraft("");
      setFocusSignal((n) => n + 1);
      await refresh(view);
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  }

  // 全局快捷键是 Rust 侧注册的；这里只处理窗口内的 Esc
  useEffect(() => {
    function onKey(e: KeyboardEvent) {
      if (e.key === "Escape" && query) {
        setQuery("");
        searchRef.current?.blur();
      }
      if (e.key === "f" && (e.ctrlKey || e.metaKey)) {
        e.preventDefault();
        searchRef.current?.focus();
      }
    }
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [query]);

  // 同步状态：启动时读一次配置和**最近一次同步结果**，然后持续监听后台
  // 线程推上来的状态。两个都要：启动时那次同步的事件通常在前端挂好监听器
  // 之前就已经发出了，只靠事件的话界面会一直停在"待同步"。
  //
  // 网页端整段跳过 —— 浏览器里没有后台同步线程，同步是服务端自己的事。
  useEffect(() => {
    if (!desktop) return;
    void refreshSyncConfig();
    void (async () => {
      try {
        const s = await desktop.getSyncStatus();
        if (s) setSyncStatus(s);
      } catch {
        // 读不到状态不影响使用，界面会显示"待同步"
      }
    })();
    return desktop.onSyncStatus(setSyncStatus);
  }, [desktop, refreshSyncConfig]);

  // 捕获永远落收件箱：按快捷键、打字、回车，没有"去哪儿"这一步。
  const targetLabel = "📥 收件箱";

  const channelNameOf = useCallback(
    (id: string) => channels.find((c) => c.id === id)?.name ?? "未知频道",
    [channels]
  );

  const title = useMemo(() => {
    if (results !== null) return `检索「${query.trim()}」`;
    switch (view.type) {
      case "timeline":
        return view.unfiledOnly ? "时间线 · 未归档" : "时间线";
      case "channel":
        return `#${channelNameOf(view.id)}`;
      case "tag":
        return `#${view.name}`;
    }
  }, [view, results, query, channelNameOf]);

  /** 「空」在不同视图里含义完全不同，文案也得跟着变。 */
  const emptyCopy = useMemo(() => {
    if (view.type === "channel") {
      return {
        title: "这个频道还是空的",
        body: (
          <>
            <p>从时间线里找到要归档的记录，点它右侧的 ⇄ 移过来。</p>
            <p className="muted">频道管归属，一条记录只属于一个频道。</p>
          </>
        ),
      };
    }
    if (view.type === "timeline" && view.unfiledOnly) {
      return {
        title: "没有未归档的了",
        body: (
          <>
            <p>所有记录都已经归到某个频道。</p>
            <p className="muted">
              新记下来的东西会先落到收件箱 —— 它在时间线的「全部」里。
            </p>
          </>
        ),
      };
    }
    return { title: undefined, body: undefined };
  }, [view]);

  return (
    <div className="app">
      <Sidebar
        channels={channels}
        tags={tags}
        view={view}
        stats={stats}
        onSelect={(v) => {
          setQuery("");
          setView(v);
        }}
        onCreateChannel={(name) => void run(() => api.createChannel(name))}
        onDeleteChannel={(id) => void run(() => api.deleteChannel(id))}
      />

      <main className="main">
        <header className="topbar">
          <h1 className="title">{title}</h1>
          {desktop && (
            <SyncBadge
              configured={syncConfigured}
              status={syncStatus}
              onOpen={() => setSettingsOpen(true)}
            />
          )}
          <div className="search-wrap">
            <input
              ref={searchRef}
              className="search-input"
              value={query}
              placeholder="搜全部记录…  (Ctrl+F)"
              onChange={(e) => setQuery(e.target.value)}
            />
            {query && (
              <button className="search-clear" onClick={() => setQuery("")} title="清空 (Esc)">
                ×
              </button>
            )}
          </div>
        </header>

        {/* 筛选条只在时间线上出现。
            把"未归档"放在这里而不是侧边栏，是为了让它明确是**时间线的一个筛选**，
            而不是和时间线平级的第二个视图 —— 上一版那两个并列项几乎一模一样。 */}
        {view.type === "timeline" && results === null && (
          <div className="filter-bar">
            <button
              className={`filter-chip ${!view.unfiledOnly ? "active" : ""}`}
              onClick={() => setView({ type: "timeline", unfiledOnly: false })}
            >
              全部 <span className="tag-count">{stats.total}</span>
            </button>
            <button
              className={`filter-chip ${view.unfiledOnly ? "active" : ""}`}
              onClick={() => setView({ type: "timeline", unfiledOnly: true })}
              title="还没归档到任何频道的记录"
            >
              未归档 <span className="tag-count">{stats.unfiled}</span>
            </button>
          </div>
        )}

        {error && (
          <div className="error-bar">
            <span>{error}</span>
            <button className="icon-btn" onClick={() => setError(null)}>
              ×
            </button>
          </div>
        )}

        <div className="stream-wrap">
          {results !== null ? (
            results.length === 0 ? (
              <div className="stream-empty">
                <div className="empty-card">
                  <h3>没有匹配的记录</h3>
                  <p className="muted">试试更短的关键词，中文双字词（如「笔记」）检索效果最好。</p>
                </div>
              </div>
            ) : (
              <Stream
                messages={results}
                channels={channels}
                channelLabel={(m) => channelNameOf(m.channelId)}
                onEdit={(id, body) => run(() => api.updateMessage(id, body))}
                onDelete={(id) => run(() => api.deleteMessage(id))}
                onMove={(id, cid) => run(() => api.moveMessage(id, cid))}
                onTags={(id, t) => run(() => api.setMessageTags(id, t))}
              />
            )
          ) : (
            <Stream
              messages={messages}
              channels={channels}
              onEdit={(id, body) => run(() => api.updateMessage(id, body))}
              onDelete={(id) => run(() => api.deleteMessage(id))}
              onMove={(id, cid) => run(() => api.moveMessage(id, cid))}
              onTags={(id, t) => run(() => api.setMessageTags(id, t))}
              hasMore={hasMore}
              loadingOlder={loadingOlder}
              onLoadOlder={loadOlder}
              emptyTitle={emptyCopy.title}
              emptyBody={emptyCopy.body}
            />
          )}
        </div>

        <Composer
          draft={draft}
          setDraft={setDraft}
          onSend={() => void handleSend()}
          targetLabel={targetLabel}
          focusSignal={focusSignal}
          disabled={busy}
        />
      </main>

      {desktop && settingsOpen && (
        <SyncSettings
          onClose={() => setSettingsOpen(false)}
          onSaved={() => {
            void refreshSyncConfig();
            // 刚配好就立刻同步一次，用户不必等下一个自动周期
            void desktop.syncNow();
          }}
        />
      )}
    </div>
  );
}
