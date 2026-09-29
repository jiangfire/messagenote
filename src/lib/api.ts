import { invoke } from "@tauri-apps/api/core";
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
 * 对 Tauri 命令的类型化封装。
 *
 * 集中在这里的唯一理由：命令名和参数名一旦拼错，报错只会出现在运行时，
 * 而 TypeScript 拦不住裸字符串。收拢成一层后，全应用只有这一个文件可能写错。
 *
 * 注意：Tauri v2 会把 Rust 侧的 snake_case 参数名映射成 camelCase，
 * 所以这里传的是 channelId 而不是 channel_id。
 */
export const api = {
  listChannels: () => invoke<Channel[]>("list_channels"),

  createChannel: (name: string) => invoke<Channel>("create_channel", { name }),

  renameChannel: (id: string, name: string) =>
    invoke<void>("rename_channel", { id, name }),

  deleteChannel: (id: string) => invoke<void>("delete_channel", { id }),

  /**
   * 时间线查询。`scope` 取 `"all"` / `"unfiled"` / `"channel"` / `"tag"`。
   *
   * `before` 是**往前翻**的游标，必须同时带 `createdAt` 和 `id`：
   * 只给时间戳会在同一毫秒内的多条消息处漏掉整批记录，
   * 而且不报错——只是往前翻的时候有东西不见了。
   */
  listTimeline: (
    scope: "all" | "unfiled" | "channel" | "tag",
    target: { channelId?: string; tag?: string },
    limit?: number,
    before?: Cursor | null,
    since?: number | null
  ) =>
    invoke<MessagePage>("list_timeline", {
      scope,
      channelId: target.channelId ?? null,
      tag: target.tag ?? null,
      limit: limit ?? null,
      since: since ?? null,
      beforeCreatedAt: before?.createdAt ?? null,
      beforeId: before?.id ?? null,
    }),

  /** 侧边栏的两个计数。 */
  timelineStats: () => invoke<TimelineStats>("timeline_stats"),

  appendMessage: (body: string, channelId: string | null) =>
    invoke<Message>("append_message", { body, channelId }),
  updateMessage: (id: string, body: string) =>
    invoke<Message>("update_message", { id, body }),

  deleteMessage: (id: string) => invoke<void>("delete_message", { id }),

  moveMessage: (id: string, channelId: string) =>
    invoke<void>("move_message", { id, channelId }),

  /**
   * 检索。`offset` 是"加载更多结果"往后看的条数 —— 检索用 offset 而不是
   * 键集游标，因为它的排序键是会随语料变化的 bm25 分数。
   */
  searchMessages: (query: string, limit?: number, offset?: number) =>
    invoke<SearchPage>("search_messages", {
      query,
      limit: limit ?? null,
      offset: offset ?? null,
    }),

  listTags: () => invoke<TagCount[]>("list_tags"),

  setMessageTags: (messageId: string, tags: string[]) =>
    invoke<void>("set_message_tags", { messageId, tags }),

  // ------------------------------------------------------------ 附件

  saveAttachment: (bytes: Uint8Array) =>
    // Tauri 的 invoke 走 JSON，Uint8Array 会被当成普通对象序列化成一堆数字键。
    // 转成 number[] 之后是规整的 JSON 数组，Rust 侧 `Vec<u8>` 正好这么收。
    invoke<string>("save_attachment", { bytes: Array.from(bytes) }),

  readAttachment: async (sha256: string) => {
    // 这个命令返回 tauri::ipc::Response，也就是**原始字节**而不是 JSON。
    // ArrayBuffer 就是 Tauri 为此准备的返回形态。
    const buf = await invoke<ArrayBuffer>("read_attachment", { sha256 });
    return new Uint8Array(buf);
  },

  hasAttachment: (sha256: string) => invoke<boolean>("has_attachment", { sha256 }),

  /** 收起捕获浮层。走 Rust 侧命令，保证"如何收起"只有一个实现。 */
  hideCapture: () => invoke<void>("hide_capture"),

  /**
   * 本次启动记下的**非致命**警告（比如全局快捷键被别的程序占用）。
   *
   * 挂载时读一次。这些警告产生于 Rust 侧的 setup 阶段 —— 那时界面还没加载，
   * 事件推送没人听得到，所以必须有这个"可查询"的入口。
   */
  getStartupWarnings: () => invoke<string[]>("get_startup_warnings"),

  // ------------------------------------------------------------ 同步

  getSyncConfig: () => invoke<SyncConfig>("get_sync_config"),

  setSyncConfig: (url: string, token: string) =>
    invoke<void>("set_sync_config", { url, token }),

  /** 请求立刻同步一次。命令立刻返回，结果通过 `sync://status` 事件推上来。 */
  syncNow: () => invoke<void>("sync_now"),

  /**
   * 最近一次同步的结果。
   *
   * 挂载时必须主动读一次：后台线程在应用启动时就会同步一次，
   * 那一刻前端往往还没注册好事件监听器，只靠监听会漏掉它。
   */
  getSyncStatus: () => invoke<SyncStatus | null>("get_sync_status"),

  /**
   * 测试连接。注意它打的是**鉴权过的** handshake 端点，
   * 所以令牌不对时会明确失败 —— 而不是给出"连接成功"的假象。
   */
  testSyncConnection: (url: string, token: string) =>
    invoke<HealthResponse>("test_sync_connection", { url, token }),
};

// `errorText` 搬去了 `./errors` —— 网页端不该为了一个错误格式化函数
// 把 @tauri-apps/api 拉进 bundle。这里转出一次，免得老调用点全要改。
export { errorText } from "./errors";
