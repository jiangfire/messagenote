/** 与 Rust 侧 models.rs 一一对应（Rust 用 serde camelCase 序列化）。 */

export interface Channel {
  id: string;
  name: string;
  kind: string;
  sortOrder: number;
  createdAt: number;
  updatedAt: number;
  messageCount: number;
}

export interface Message {
  id: string;
  channelId: string;
  body: string;
  createdAt: number;
  updatedAt: number;
  tags: string[];
}

export interface MessagePage {
  items: Message[];
  hasMore: boolean;
}

/**
 * 时间线往前翻的游标。
 *
 * **两个字段必须一起用。** 只带 `createdAt` 等于退回单键游标：
 * 同一毫秒内写入的多条消息会被整批跳过，往前翻就会凭空少一段记录，
 * 而且不报任何错。`id` 是 UUIDv7，字典序即时间序，天然是第二排序键。
 */
export interface Cursor {
  createdAt: number;
  id: string;
}

export interface SearchHit extends Message {
  channelName: string;
}

/**
 * 一页检索结果。
 *
 * 检索的翻页用 **offset** 而不是游标：时间线是"往前翻更早的"，可以拿
 * `(createdAt, id)` 做键集游标；而检索是"更相关的"，排序键是 bm25 分数 ——
 * 一个会随语料变化的浮点值，拿它做游标不稳。所以是"加载更多结果"。
 */
export interface SearchPage {
  items: SearchHit[];
  hasMore: boolean;
}

export interface TagCount {
  name: string;
  count: number;
}

/**
 * 当前视图。
 *
 * 分工：**频道管归属（互斥），标签管横切（可多个）**。
 * 所以"时间线 + 未归档筛选 + 频道 + 标签"不是四个平级的东西：
 * - `timeline` 是唯一的浏览面，`unfiledOnly` 只是它上方的一个筛选
 * - `channel` 是归档目标
 * - `tag` 是横切标记
 *
 * 收件箱**不作为导航项出现**：它就是"未归档"这个筛选。
 * 否则侧边栏会同时列出「未归档 6」和「📥 收件箱 6」——
 * 又是同一个东西重复两遍。
 */
export type View =
  | { type: "timeline"; unfiledOnly: boolean }
  | { type: "channel"; id: string }
  | { type: "tag"; name: string };

/** 侧边栏与筛选条需要的两个计数。 */
export interface TimelineStats {
  total: number;
  /** 还没归档到任何频道的 —— 仍在收件箱里等整理的那些 */
  unfiled: number;
}

// ---------------------------------------------------------------- 同步

export interface SyncConfig {
  url: string;
  token: string;
}

/**
 * 有一个可用的新版本。
 *
 * 只有桌面端用得上：网页端每次打开都是最新版，"装在机器上的旧版本"这件事
 * 在浏览器里不存在。
 */
export interface UpdateInfo {
  version: string;
  /**
   * 发行说明。清单文件（`latest.json`）里叫 `notes`，
   * 而更新插件在 JS 侧把它暴露成 `body` —— 名字错位在那边，不在我们这边。
   */
  notes: string | null;
  /** 发布日期，可能没有。 */
  date: string | null;
}

/** 服务端 handshake 的返回。`protocol` 与客户端不一致时必须拒绝同步。 */
export interface HealthResponse {
  ok: boolean;
  protocol: number;
  serverTimeMs: number;
}

/** 后台同步线程通过 `sync://status` 事件推上来的结果。 */
export interface SyncStatus {
  ok: boolean;
  message: string;
  pushed: number;
  pulled: number;
  conflicts: number;
}

/** 一次导出的结果摘要（全量或筛选后的）。 */
export interface ExportSummary {
  messages: number;
  attachments: number;
  /**
   * 正文引用了、但**本地还没有字节**的附件数。
   *
   * 这些引用在导出物里原样保留 `attachment:<sha>`。界面**必须**把它说出来
   * （"有 N 张图本地还没有，没能带出来"），否则用户只会以为导出漏了东西。
   */
  missingAttachments: number;
  channels: number;
}

/**
 * 导出筛选。四个字段都是可选的，全不给 = 全量导出（这一条功能之前的行为）。
 *
 * 和**时间线**上那个时间档位不是一回事：档位回答"我最近记了什么"，只给下界；
 * 导出要的是一个**区间**（写周报、交存档常常要"3 月 1 日到 3 月 31 日"），
 * 所以这里上下界都有，而且**两端都算在内**。
 */
export interface ExportFilter {
  /** 只导这个频道。收件箱也是一个频道（id 固定为 `inbox`）。 */
  channelId?: string | null;
  /** 只导打了这个标签的记录。和频道同时给时取交集。 */
  tag?: string | null;
  /** 时间范围下界（epoch 毫秒，**含端点**）。 */
  fromMs?: number | null;
  /** 时间范围上界（epoch 毫秒，**含端点**）。 */
  toMs?: number | null;
}
