import { createContext, useContext, useMemo, type ReactNode } from "react";
import type {
  Channel,
  Cursor,
  HealthResponse,
  Message,
  MessagePage,
  SearchPage,
  SyncConfig,
  SyncStatus,
  TagCount,
  TimelineStats,
} from "./types";

/**
 * 笔记数据的读写。
 *
 * 这一层存在的理由：**桌面端和网页端跑的是同一套界面**，区别只在数据从哪儿来。
 * 桌面端走 Tauri `invoke`（本地优先，SQLite 在自己机器上）；网页端走 HTTP
 * （写入由服务端代笔）。组件只认这个接口，两边一行都不用改。
 *
 * 形状刻意和 `api.ts` 里那个对象一致 —— 那份实现早就把所有 Tauri 命令名收在
 * 一个文件里了，收口点是现成的。
 */
export interface NoteApi {
  listChannels(): Promise<Channel[]>;
  createChannel(name: string): Promise<Channel>;
  renameChannel(id: string, name: string): Promise<void>;
  deleteChannel(id: string): Promise<void>;

  /**
   * 时间线查询。`before` 是**往前翻**的游标，两个字段必须一起给：
   * 只给时间戳会在同一毫秒内的多条消息处漏掉整批记录，而且不报错。
   */
  listTimeline(
    scope: "all" | "unfiled" | "channel" | "tag",
    target: { channelId?: string; tag?: string },
    limit?: number,
    before?: Cursor | null
  ): Promise<MessagePage>;
  timelineStats(): Promise<TimelineStats>;

  appendMessage(body: string, channelId: string | null): Promise<Message>;
  updateMessage(id: string, body: string): Promise<Message>;
  deleteMessage(id: string): Promise<void>;
  moveMessage(id: string, channelId: string): Promise<void>;

  /**
   * 检索。`offset` 是"加载更多结果"往后看的条数。
   *
   * 检索没有键集游标 —— 它的排序键是会随语料变化的 bm25 分数，
   * 拿它做游标不稳。见 `SearchPage` 上的说明。
   */
  searchMessages(query: string, limit?: number, offset?: number): Promise<SearchPage>;
  listTags(): Promise<TagCount[]>;
  setMessageTags(messageId: string, tags: string[]): Promise<void>;
}

/**
 * 桌面端专有的东西。网页端是 `null`。
 *
 * 同步配置、同步状态、捕获浮层在浏览器里没有对应物 —— 网页端的同步是
 * **服务端自己**在做，浏览器不需要知道这件事。
 */
export interface DesktopApi {
  hideCapture(): Promise<void>;
  /** 本次启动记下的非致命警告（快捷键被占用之类）。挂载时读一次。 */
  getStartupWarnings(): Promise<string[]>;
  getSyncConfig(): Promise<SyncConfig>;
  setSyncConfig(url: string, token: string): Promise<void>;
  syncNow(): Promise<void>;
  getSyncStatus(): Promise<SyncStatus | null>;
  testSyncConnection(url: string, token: string): Promise<HealthResponse>;
  /** 订阅后台同步状态，返回取消订阅函数。 */
  onSyncStatus(handler: (status: SyncStatus) => void): () => void;
}

export interface ApiBundle {
  api: NoteApi;
  desktop: DesktopApi | null;
}

const Ctx = createContext<ApiBundle | null>(null);

export function ApiProvider({
  api,
  desktop = null,
  children,
}: {
  api: NoteApi;
  desktop?: DesktopApi | null;
  children: ReactNode;
}) {
  // 记住这个 bundle：不然每次渲染都换一个新对象，
  // 所有把 `api` 放进依赖数组的 effect 都会反复重跑。
  const value = useMemo(() => ({ api, desktop }), [api, desktop]);
  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}

export function useApi(): ApiBundle {
  const v = useContext(Ctx);
  if (!v) throw new Error("useApi 必须在 <ApiProvider> 里使用");
  return v;
}
