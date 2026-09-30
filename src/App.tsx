import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
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
import { OverflowMenu } from "./components/OverflowMenu";
import { UpdateNotice } from "./components/UpdateNotice";

const PAGE_SIZE = 200;
const SEARCH_DEBOUNCE_MS = 160;
/** 检索一页给多少条。比时间线小 —— 结果是按相关性排的，翻太多反而更难挑。 */
const SEARCH_PAGE_SIZE = 60;

/**
 * 时间范围筛选的档位。
 *
 * 界面上只有这几档，不提供自由选日期 —— 档位回答的是"我最近记了什么"，
 * 这才是时间筛选的主要用途；"查某个具体日子"是检索的活。
 */
type TimeFilter = "all" | "today" | "7d" | "30d";

const TIME_FILTERS: TimeFilter[] = ["all", "today", "7d", "30d"];

const TIME_FILTER_LABEL: Record<TimeFilter, string> = {
  all: "不限",
  today: "今天",
  "7d": "近 7 天",
  "30d": "近 30 天",
};

/**
 * 档位折算成 `since`（绝对 epoch 毫秒，含端点）。
 *
 * 在**客户端**折算而不是传"7d"让后端算：后端不必知道任何时区约定，
 * 「今天」这种和本地日历相关的概念本来也只有客户端说得清。
 */
function sinceMsOf(f: TimeFilter): number | null {
  switch (f) {
    case "all":
      return null;
    case "today": {
      const now = new Date();
      return new Date(now.getFullYear(), now.getMonth(), now.getDate()).getTime();
    }
    case "7d":
      return Date.now() - 7 * 86_400_000;
    case "30d":
      return Date.now() - 30 * 86_400_000;
  }
}

/**
 * 把视图映射成时间线的 scope 参数。
 *
 * `api` 从外面传进来而不是从模块里 import：这个应用要同时跑在桌面端（Tauri
 * invoke）和网页端（HTTP）上，数据从哪儿来只有注入点知道。
 */
function fetchPage(
  api: NoteApi,
  v: View,
  limit: number,
  before: Cursor | null,
  since: number | null
) {
  switch (v.type) {
    case "timeline":
      // 时间线是主视图；"未归档"只是它上方的一个筛选，不是另一个导航项
      return api.listTimeline(v.unfiledOnly ? "unfiled" : "all", {}, limit, before, since);
    case "channel":
      return api.listTimeline("channel", { channelId: v.id }, limit, before, since);
    case "tag":
      return api.listTimeline("tag", { tag: v.name }, limit, before, since);
  }
}

// ---------------- 字号 ----------------

const FONT_SCALE_KEY = "messagenote.fontScale";

/**
 * 字号档位。所有 font-size 都用 rem 表达，所以缩放 html 的根字号
 * （`--font-scale`，见 styles.css）一处生效、全局跟随。
 */
const FONT_SCALES = [
  { value: 0.85, label: "小" },
  { value: 1, label: "标准" },
  { value: 1.15, label: "大" },
  { value: 1.3, label: "特大" },
];

/** 读不出来的（没存过、存了非法值）一律回标准档。 */
function loadFontScale(): number {
  const raw = Number(localStorage.getItem(FONT_SCALE_KEY));
  return FONT_SCALES.some((s) => s.value === raw) ? raw : 1;
}

/**
 * 同步状态**值不值得占顶栏一个位置**。
 *
 * 只有"用户该知道但可能不知道"的情况才占：
 * - 没配同步：新装的桌面端，用户还不知道多设备那件事，值得说一句
 * - 同步失败：数据可能没存下来，这是必须打断的事
 * - 已同步 / 待同步：没有新信息，藏进 ⋯ 就行
 *
 * 判断放在这里而不是 `SyncBadge` 里，是为了让"顶栏那个胶囊"和"菜单里那一行"
 * 用同一套措辞 —— 两处各写一遍的话，迟早会分叉成"顶栏说失败、菜单说已同步"。
 */
function syncNeedsAttention(configured: boolean, status: SyncStatus | null): boolean {
  if (!configured) return true;
  return status !== null && !status.ok;
}

/** 菜单里的那一行同步状态。词和顶栏胶囊一致，只是没有那个圆点。 */
function SyncRow({
  configured,
  status,
  onOpen,
}: {
  configured: boolean;
  status: SyncStatus | null;
  onOpen: () => void;
}) {
  let text = "未配置同步";
  if (configured) {
    if (!status) text = "待同步";
    else if (status.ok) {
      text =
        status.conflicts > 0
          ? `已同步 · 新增冲突副本 ${status.conflicts}`
          : `已同步 · 推 ${status.pushed} 拉 ${status.pulled}`;
    } else text = `同步失败 · ${status.message}`;
  }

  return (
    <button className="overflow-item" onClick={onOpen} title="打开同步设置">
      {text}
    </button>
  );
}

export default function App() {
  // `desktop` 在网页端是 null —— 同步配置、同步状态、捕获浮层在浏览器里
  // 都没有对应物（网页端的同步是服务端自己在做）。
  const { api, desktop } = useApi();

  const [stats, setStats] = useState<TimelineStats>({ total: 0, unfiled: 0 });
  const [channels, setChannels] = useState<Channel[]>([]);
  const [tags, setTags] = useState<TagCount[]>([]);
  const [view, setView] = useState<View>({ type: "timeline", unfiledOnly: false });
  /** 时间范围筛选。只在时间线上出现（频道/标签有自己的归属语境，不吃这个筛选）。 */
  const [timeFilter, setTimeFilter] = useState<TimeFilter>("all");
  /** 整体字号档位。落 localStorage，两端（桌面/网页）各存各的。 */
  const [fontScale, setFontScale] = useState(loadFontScale);
  const [messages, setMessages] = useState<Message[]>([]);
  const [hasMore, setHasMore] = useState(false);
  const [loadingOlder, setLoadingOlder] = useState(false);
  const [draft, setDraft] = useState("");
  const [query, setQuery] = useState("");
  const [results, setResults] = useState<SearchHit[] | null>(null);
  const [resultsHasMore, setResultsHasMore] = useState(false);
  const [loadingMoreResults, setLoadingMoreResults] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [focusSignal, setFocusSignal] = useState(0);
  const [busy, setBusy] = useState(false);
  const [syncConfigured, setSyncConfigured] = useState(false);
  const [syncStatus, setSyncStatus] = useState<SyncStatus | null>(null);
  const [settingsOpen, setSettingsOpen] = useState(false);
  /** 本次启动的非致命警告（快捷键被占用之类）。见下面的 effect。 */
  const [notices, setNotices] = useState<string[]>([]);

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
      // 时间筛选是时间线的属性：频道/标签视图不筛（和筛选条只在时间线出现保持一致）
      const since = v.type === "timeline" ? sinceMsOf(timeFilterRef.current) : null;
      const page = await fetchPage(api, v, limit, null, since);
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
      // 同 loadMessages：时间筛选只作用于时间线视图
      const since = view.type === "timeline" ? sinceMsOf(timeFilterRef.current) : null;
      const page = await fetchPage(api, view, PAGE_SIZE, cursor, since);
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

  /**
   * 订阅推送时要用"当前视图"，但**不该因为视图变化而重建订阅** ——
   * 那会每切一次视图就断线重连一次 SSE，中间那几十毫秒的推送就丢了。
   */
  const viewRef = useRef(view);
  useEffect(() => {
    viewRef.current = view;
  }, [view]);

  /** 同 viewRef 的理由：SSE 推送触发的重取也用得到当前档位，但不该进依赖。 */
  const timeFilterRef = useRef(timeFilter);
  useEffect(() => {
    timeFilterRef.current = timeFilter;
  }, [timeFilter]);

  /**
   * 服务端说"有东西变了"就重取一次。
   *
   * **必须防重入。** 一次同步可能连着推好几个信号（每批变更一个），
   * 而每次都完整重取一遍是白费 —— 后一次取到的一定包含前一次的结果。
   * 已经在跑的时候直接跳过即可，正在跑的那次本来就会读到最新状态。
   */
  const refreshingByPush = useRef(false);
  const { subscribeChanges } = useApi();
  useEffect(() => {
    if (!subscribeChanges) return;
    return subscribeChanges(() => {
      if (refreshingByPush.current) return;
      refreshingByPush.current = true;
      void refresh(viewRef.current).finally(() => {
        refreshingByPush.current = false;
      });
    });
  }, [subscribeChanges, refresh]);

  useEffect(() => {
    void refresh(view);
  }, [view, timeFilter, refresh]);

  // 切换视图时把焦点交还输入框：用户的下一个动作几乎总是"写"。
  // 换时间档位不算换视图，不打断输入。
  useEffect(() => {
    setFocusSignal((n) => n + 1);
  }, [view]);

  // 检索：防抖，且为空时立刻退回普通视图
  useEffect(() => {
    const q = query.trim();
    if (!q) {
      setResults(null);
      setResultsHasMore(false);
      return;
    }
    const timer = setTimeout(async () => {
      try {
        setError(null);
        const page = await api.searchMessages(q, SEARCH_PAGE_SIZE, 0);
        setResults(page.items);
        setResultsHasMore(page.hasMore);
      } catch (e) {
        setError(errorText(e));
      }
    }, SEARCH_DEBOUNCE_MS);
    return () => clearTimeout(timer);
  }, [api, query]);

  /**
   * 加载更多检索结果。
   *
   * 用 offset 往后看，而不是像时间线那样用游标往前翻 —— 检索按相关性排序，
   * 排序键是会随语料变化的 bm25 分数，拿它做游标不稳。
   */
  const loadMoreResults = useCallback(async () => {
    if (loadingMoreResults || !resultsHasMore || results === null) return;
    const q = query.trim();
    if (!q) return;

    setLoadingMoreResults(true);
    try {
      const page = await api.searchMessages(q, SEARCH_PAGE_SIZE, results.length);
      setResults((prev) => [...(prev ?? []), ...page.items]);
      setResultsHasMore(page.hasMore);
    } catch (e) {
      setError(errorText(e));
    } finally {
      setLoadingMoreResults(false);
    }
  }, [api, loadingMoreResults, resultsHasMore, results, query]);

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
      // 去向跟随当前视图：在频道里发就进频道（targetLabel 已经把这件事
      // 告诉用户了，说一套做一套最伤信任）；其余视图落收件箱等着整理。
      // 捕获浮层那条零摩擦路径不受影响，依旧永远落收件箱。
      const channelId = view.type === "channel" ? view.id : null;
      await api.appendMessage(body, channelId);
      setDraft("");
      setFocusSignal((n) => n + 1);
      await refresh(view);
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  }

  // 字号档位：写到根元素的 CSS 变量上（styles.css 里 html 的 font-size
  // 用它计算），所有 rem 表达的字号一起缩放；顺手持久化。
  // 用 layout effect：在首次绘制**之前**写好变量，存过非默认档位的用户
  // 不会看到一帧标准字号然后跳一下。
  useLayoutEffect(() => {
    document.documentElement.style.setProperty("--font-scale", String(fontScale));
    localStorage.setItem(FONT_SCALE_KEY, String(fontScale));
  }, [fontScale]);

  // 全局快捷键是 Rust 侧注册的；这里只处理窗口内的 Esc。
  //
  // 菜单开着的时候 Esc 归菜单管（它在 capture 阶段就 `stopPropagation` 了，
  // 事件根本走不到这里）—— 关菜单和清检索词是两件事，不该一次按完。
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

  // 启动警告。挂载时**主动读一次**：它们产生于 Rust 侧的 setup 阶段，
  // 那时窗口还没加载，事件推送根本没人听得到。
  //
  // 不显示的话，用户感受到的只是"这个软件有时候按快捷键没反应" ——
  // 一个他永远查不出原因的现象。
  useEffect(() => {
    if (!desktop) return;
    desktop
      .getStartupWarnings()
      .then(setNotices)
      .catch(() => {
        // 读不到警告不影响使用，静默即可
      });
  }, [desktop]);

  const channelNameOf = useCallback(
    (id: string) => channels.find((c) => c.id === id)?.name ?? "未知频道",
    [channels]
  );

  // 输入框的去向**跟随当前视图**：在哪个频道里，就写进哪个频道。
  // 「捕获不做决策」说的是全局快捷键唤起的捕获浮层 —— 那条零摩擦路径
  // 依旧永远落收件箱（见 capture/CaptureApp.tsx），两条路径互不绑架。
  // 时间线/标签视图没有归属语境，落收件箱等着整理。
  const targetLabel = view.type === "channel" ? `#${channelNameOf(view.id)}` : "📥 收件箱";

  const title = useMemo(() => {
    if (results !== null) return `检索「${query.trim()}」`;
    const timeSuffix = timeFilter === "all" ? "" : ` · ${TIME_FILTER_LABEL[timeFilter]}`;
    switch (view.type) {
      case "timeline":
        return (view.unfiledOnly ? "时间线 · 未归档" : "时间线") + timeSuffix;
      case "channel":
        return `#${channelNameOf(view.id)}`;
      case "tag":
        return `#${view.name}`;
    }
  }, [view, results, query, timeFilter, channelNameOf]);

  /** 「空」在不同视图里含义完全不同，文案也跟着变。 */
  const emptyCopy = useMemo(() => {
    // 时间档位筛出来的空必须先说：不然用户明明有记录，
    // 界面却告诉他"这里还是空的"，他会以为数据丢了。
    if (view.type === "timeline" && timeFilter !== "all") {
      return {
        title: `${TIME_FILTER_LABEL[timeFilter]}还没有记录`,
        body: <p className="muted">换个更长的时间档位。</p>,
      };
    }
    if (view.type === "channel") {
      return {
        title: "这个频道还是空的",
        body: <p>在时间线里找到要归档的记录，点它右侧的 ⇄ 移过来。</p>,
      };
    }
    if (view.type === "timeline" && view.unfiledOnly) {
      return {
        title: "没有未归档的了",
        body: <p>所有记录都已经归到某个频道。</p>,
      };
    }
    // 网页端没有全局快捷键、也没有捕获浮层，不能说"按 Ctrl+Shift+Space 唤起窗口"
    // —— 用户会去找一个不存在的快捷键。`Stream` 里那段默认文案是给桌面端写的，
    // 这里是网页端的替身。
    if (!desktop) {
      return {
        title: undefined,
        body: <p>在下面写点什么，按 Enter 就记下了。</p>,
      };
    }
    return { title: undefined, body: undefined };
  }, [view, timeFilter, desktop]);

  /**
   * 导出全部记录到用户挑的目录。
   *
   * **桌面端专有**：导出要往磁盘上写一棵目录树（按频道分目录 + `attachments/`），
   * 浏览器里做不到。
   *
   * 偏移传 `-getTimezoneOffset()`：它返回的是"本地转 UTC 要加多少分钟"，
   * 取负才是"东为正"的口径，也正是 Rust 那边要的。
   */
  async function exportAll() {
    if (!desktop) return;
    const dir = await desktop.pickDirectory();
    if (!dir) return; // 用户取消 —— 不该弹一句"导出失败"
    try {
      const s = await desktop.exportMarkdown(dir, -new Date().getTimezoneOffset());
      // **取不到的附件必须说出来。** 不说的话用户只会以为导出漏了东西 ——
      // 而导出物里那条 `attachment:<sha>` 他看不懂是什么意思。
      const missed = s.missingAttachments
        ? `\n\n注意：有 ${s.missingAttachments} 个附件本地还没有字节，没有带出来` +
          `（正文里仍是 attachment: 引用）。等同步把它们拿下来再导一次即可。`
        : "";
      window.alert(
        `导出完成：\n\n${s.messages} 条记录\n${s.attachments} 个附件\n` +
          `${s.channels} 个频道目录\n\n位置：${dir}${missed}`
      );
    } catch (e) {
      window.alert(`导出失败：${errorText(e)}`);
    }
  }

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

          {/* 同步状态**只在出问题时**留在顶栏。
              正常同步是个没有信息量的绿点，而"我这条到底存哪儿了"是用户会问的
              问题 —— 答案不该藏起来。而一个四档下拉框常年占着检索框的位置、
              几个月才用一次，就该收进 ⋯。见 OverflowMenu 的说明。 */}
          {desktop && syncNeedsAttention(syncConfigured, syncStatus) && (
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

          {/* 低频功能都收在这儿：字号、导出、同步设置。 */}
          <OverflowMenu title="更多功能">
            {(close) => (
              <>
                <div className="overflow-sec">
                  <span className="overflow-label">界面字号</span>
                  <select
                    className="font-select"
                    value={fontScale}
                    onChange={(e) => {
                      setFontScale(Number(e.target.value));
                      // 调完就走：菜单糊在脸上会挡住用户接下来要看的记录
                      close();
                    }}
                  >
                    {FONT_SCALES.map((s) => (
                      <option key={s.value} value={s.value}>
                        {s.label}
                      </option>
                    ))}
                  </select>
                </div>

                {desktop && (
                  <div className="overflow-sec">
                    <span className="overflow-label">同步</span>
                    <SyncRow
                      configured={syncConfigured}
                      status={syncStatus}
                      onOpen={() => {
                        setSettingsOpen(true);
                        close();
                      }}
                    />
                  </div>
                )}

                {/* 导出**只有桌面端有**（浏览器里写不出一棵目录树），
                    所以网页端的菜单里不出现这一行 —— 而不是渲染一个点下去
                    没反应的按钮。 */}
                {desktop && (
                  <button
                    className="overflow-item"
                    onClick={() => {
                      close();
                      void exportAll();
                    }}
                  >
                    导出全部记录…
                  </button>
                )}
              </>
            )}
          </OverflowMenu>
        </header>

        {notices.length > 0 && (
          <div className="notice-bar">
            <span>{notices.join("；")}</span>
            <button
              className="icon-btn"
              onClick={() => setNotices([])}
              title="知道了"
            >
              ×
            </button>
          </div>
        )}

        {/* 有新版本时的一条提示。网页端不显示（`desktop` 是 null）。 */}
        <UpdateNotice />

        {/* 筛选条只在时间线上出现。
            把"未归档"放在这里而不是侧边栏，是为了让它明确是**时间线的一个筛选**，
            而不是和时间线平级的第二个视图 —— 上一版那两个并列项几乎一模一样。
            时间档位同理：它筛的是"看哪一段"，和"看哪个范围"（全部/未归档）正交。 */}
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
            <span className="filter-divider" aria-hidden="true" />
            <span className="filter-label">时间</span>
            {TIME_FILTERS.map((f) => (
              <button
                key={f}
                className={`filter-chip ${timeFilter === f ? "active" : ""}`}
                onClick={() => setTimeFilter(f)}
              >
                {TIME_FILTER_LABEL[f]}
              </button>
            ))}
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
                // 检索结果是往**下**翻的，而且用明确的按钮而不是滚动自动加载
                moreAt="bottom"
                hasMore={resultsHasMore}
                loadingOlder={loadingMoreResults}
                onLoadOlder={loadMoreResults}
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
          // 图片存不进本地库时就地说一声。复用页面上那条 error-bar，
          // 不另造一套提示 —— 用户已经知道红色横条是什么意思了。
          onError={setError}
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
