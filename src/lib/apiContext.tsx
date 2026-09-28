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
  UpdateInfo,
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

  /**
   * 记一条。
   *
   * `id` 是**幂等键**，只有网页端用得上：它没有本地库，离线时要把"要记什么"
   * 排进队列、联网后重放，而重放天然会重试。给了 id 之后，同一条请求重放
   * 多少遍都只会落一条记录，且不会产生多余的变更日志。
   *
   * 桌面端忽略这个参数 —— 它在本地就生成了 id，再通过同步协议推上来，
   * 走的不是这条路径。
   */
  appendMessage(body: string, channelId: string | null, id?: string): Promise<Message>;
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

  // ------------------------------------------------------------ 附件
  //
  // 附件按内容寻址（sha256）。正文里用 `attachment:<sha>` 引用它，
  // 渲染时再换成字节。见 `messagenote_core::attachment`。

  /**
   * 存一份附件（粘贴或拖进来的图片），返回它的 sha256。
   *
   * 桌面端在本地算哈希，离线也能存；网页端没有本地库，由服务端算完返回
   * （浏览器里 `crypto.subtle` 在非安全上下文下根本不存在，而自建服务端
   * 常常就是明文 HTTP 的内网地址）。
   */
  saveAttachment(bytes: Uint8Array<ArrayBuffer>): Promise<string>;
  /** 取附件字节。字节还没到手时 reject。 */
  readAttachment(sha256: string): Promise<Uint8Array<ArrayBuffer>>;
  /** 字节在不在本地。界面据此显示占位图。 */
  hasAttachment(sha256: string): Promise<boolean>;
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

  // ------------------------------------------------------------ 自动更新
  //
  // 桌面端已经分发出去了。没有这套东西，用户手上那份就是**死版本** ——
  // 每次发新版都得让人重新下载安装包。

  /**
   * 查有没有新版本。没有就返回 `null`。
   *
   * 拿到结果之后必须调 [`installUpdate`](DesktopApi.installUpdate) ——
   * 更新的句柄留在实现里，不从这里透出去（那会把 Tauri 的类型泄漏到
   * 共享的界面代码里）。
   */
  checkForUpdate(): Promise<UpdateInfo | null>;
  /**
   * 下载、校验签名、安装、重启。**中途会换掉当前进程**，所以调用方
   * 之后不该再假设自己还在跑。
   */
  installUpdate(): Promise<void>;
}

export interface ApiBundle {
  api: NoteApi;
  desktop: DesktopApi | null;
  /**
   * 订阅"服务端有变更了"。返回取消订阅的函数。
   *
   * 两端实现方式完全不同：网页端是 SSE（`web/sse.ts`），桌面端由 Rust 侧的
   * SSE 线程推一个 Tauri 事件上来。但对界面来说都是同一件事 ——
   * **别的地方改动了数据，去重取一次**。
   *
   * 没有推送能力的实现传 `undefined`，界面就退回原来的行为（靠操作后自己刷新）。
   */
  subscribeChanges?: (handler: () => void) => () => void;
}

const Ctx = createContext<ApiBundle | null>(null);

export function ApiProvider({
  api,
  desktop = null,
  subscribeChanges,
  children,
}: {
  api: NoteApi;
  desktop?: DesktopApi | null;
  subscribeChanges?: (handler: () => void) => () => void;
  children: ReactNode;
}) {
  // 记住这个 bundle：不然每次渲染都换一个新对象，
  // 所有把 `api` 放进依赖数组的 effect 都会反复重跑。
  const value = useMemo(
    () => ({ api, desktop, subscribeChanges }),
    [api, desktop, subscribeChanges]
  );
  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}

export function useApi(): ApiBundle {
  const v = useContext(Ctx);
  if (!v) throw new Error("useApi 必须在 <ApiProvider> 里使用");
  return v;
}
