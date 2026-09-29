import type { NoteApi } from "./apiContext";
import { uuidV4 } from "./ids";

/**
 * 网页端实现：走 HTTP。
 *
 * ## 写入由服务端代笔
 *
 * 服务端以**一台设备的身份**生成 HLC、写进变更日志、分配 seq。所以这里
 * 不需要任何冲突处理、不需要本地库、也不需要 HLC —— 写完重新拉一次当前
 * 状态即可。裁定权仍然只有 `core::merge` 那一份。
 *
 * 代价很明确：**写入需要联网**。离线的捕获队列留给 S3 的 outbox。
 *
 * ## 凭据
 *
 * 这里是**短期会话**，不是长期令牌。换会话见 [`login`]。
 * 长期令牌放进浏览器的 localStorage 等于把整个库的读写权限交给任何一次
 * XSS —— 而正文是要渲染用户 Markdown 的。
 */

/** 会话过期或被吊销。界面收到它就该清掉会话、退回登录页。 */
export class UnauthorizedError extends Error {
  constructor() {
    super("登录已过期，请重新登录");
    this.name = "UnauthorizedError";
  }
}

export interface Session {
  session: string;
  expiresAt: number;
}

function trimBase(url: string): string {
  return url.trim().replace(/\/+$/, "");
}

/** 服务端的错误响应是一句人话（`ServerError::BadRequest/Unauthorized`），直接用。 */
async function errorText(resp: Response): Promise<string> {
  const text = (await resp.text()).trim();
  return text || `服务端返回 HTTP ${resp.status}`;
}

/** 用长期令牌换一个短期会话。 */
export async function login(baseUrl: string, token: string): Promise<Session> {
  const resp = await fetch(`${trimBase(baseUrl)}/api/session`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ token }),
  });

  if (resp.status === 401) throw new Error("令牌不对");
  if (!resp.ok) throw new Error(await errorText(resp));

  const data = (await resp.json()) as { session: string; expiresAt: number };
  return { session: data.session, expiresAt: data.expiresAt };
}

export interface HttpApiOptions {
  baseUrl: string;
  session: string;
  /** 收到 401 时调用 —— 界面据此清掉会话、回到登录页。 */
  onUnauthorized?: () => void;
}

export function httpApi(opts: HttpApiOptions): NoteApi {
  const base = trimBase(opts.baseUrl);

  async function req<T>(method: string, path: string, body?: unknown): Promise<T> {
    const resp = await fetch(`${base}${path}`, {
      method,
      headers: {
        Authorization: `Bearer ${opts.session}`,
        ...(body === undefined ? {} : { "Content-Type": "application/json" }),
      },
      body: body === undefined ? undefined : JSON.stringify(body),
    });

    if (resp.status === 401) {
      opts.onUnauthorized?.();
      throw new UnauthorizedError();
    }
    if (!resp.ok) throw new Error(await errorText(resp));
    // 删除类端点回 204，没有 body
    if (resp.status === 204) return undefined as T;
    return (await resp.json()) as T;
  }

  const msg = (id: string) => `/api/message/${encodeURIComponent(id)}`;

  return {
    listChannels: () => req("GET", "/api/channels"),
    createChannel: (name) => req("POST", "/api/channel", { name }),
    renameChannel: (id, name) =>
      req("PATCH", `/api/channel/${encodeURIComponent(id)}`, { name }),
    deleteChannel: (id) => req("DELETE", `/api/channel/${encodeURIComponent(id)}`),

    listTimeline: (scope, target, limit, before, since) => {
      const p = new URLSearchParams({ scope });
      if (target.channelId) p.set("channelId", target.channelId);
      if (target.tag) p.set("tag", target.tag);
      if (limit != null) p.set("limit", String(limit));
      // 时间范围筛选：客户端折算好的绝对时刻（epoch 毫秒），服务端不解释它
      if (since != null) p.set("since", String(since));
      // 游标两个字段必须一起给。只给时间戳等于退回单键游标，
      // 同一毫秒内的多条会被整批跳过，往前翻就凭空少一段。
      if (before) {
        p.set("beforeCreatedAt", String(before.createdAt));
        p.set("beforeId", before.id);
      }
      return req("GET", `/api/timeline?${p}`);
    },
    timelineStats: () => req("GET", "/api/timeline/stats"),

    // **每次都带 id。** 不给的话服务端自己生成一个，那这条写入就不幂等 ——
    // 而"不幂等"的代价在一次网络抖动里就会兑现：请求发出去了、响应没回来、
    // 客户端重试，于是同一条记录出现两遍，且没有任何地方报错。
    // 现在就把 id 定下来，重试多少次都只落一条。
    appendMessage: (body, channelId, id) =>
      req("POST", "/api/message", { body, channelId, id: id ?? uuidV4() }),
    updateMessage: (id, body) => req("PATCH", msg(id), { body }),
    deleteMessage: (id) => req("DELETE", msg(id)),
    moveMessage: (id, channelId) => req("POST", `${msg(id)}/move`, { channelId }),

    searchMessages: (query, limit, offset) => {
      const p = new URLSearchParams({ q: query });
      if (limit != null) p.set("limit", String(limit));
      if (offset != null) p.set("offset", String(offset));
      return req("GET", `/api/search?${p}`);
    },
    listTags: () => req("GET", "/api/tags"),
    setMessageTags: (messageId, tags) => req("PUT", `${msg(messageId)}/tags`, { tags }),

    // ---------------------------------------------------------- 附件

    saveAttachment: async (bytes) => {
      // **不报 sha**：服务端对收到的字节现算，再告诉我们叫什么。
      // 这样网页端不必依赖 crypto.subtle —— 它在非安全上下文（明文 HTTP 的
      // 内网地址）下根本不存在，而自建服务端常常正是那种地址。
      const resp = await fetch(`${base}/api/blob`, {
        method: "POST",
        headers: {
          Authorization: `Bearer ${opts.session}`,
          "Content-Type": "application/octet-stream",
        },
        body: bytes,
      });
      if (resp.status === 401) {
        opts.onUnauthorized?.();
        throw new UnauthorizedError();
      }
      if (!resp.ok) throw new Error(await errorText(resp));
      const data = (await resp.json()) as { sha256: string };
      return data.sha256;
    },

    readAttachment: async (sha256) => {
      const resp = await fetch(`${base}/api/blob/${encodeURIComponent(sha256)}`, {
        headers: { Authorization: `Bearer ${opts.session}` },
      });
      if (resp.status === 401) {
        opts.onUnauthorized?.();
        throw new UnauthorizedError();
      }
      if (!resp.ok) throw new Error(await errorText(resp));
      return new Uint8Array(await resp.arrayBuffer());
    },

    hasAttachment: async (sha256) => {
      // 网页端没有本地库 —— 字节要么在服务端，要么还没有。
      // 直接用 HEAD 问一下，比取一遍再判断便宜得多。
      const resp = await fetch(`${base}/api/blob/${encodeURIComponent(sha256)}`, {
        method: "HEAD",
        headers: { Authorization: `Bearer ${opts.session}` },
      });
      return resp.ok;
    },
  };
}
