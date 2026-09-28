/**
 * 服务端推送的订阅（SSE，自己读流）。
 *
 * ## 为什么不用 `EventSource`
 *
 * `EventSource` **不能自定义请求头**，而这个服务的鉴权走
 * `Authorization: Bearer`。退而求其次把会话令牌放进查询串是不行的 ——
 * URL 会进访问日志、进浏览器历史、进 Referer，等于把凭据到处撒。
 * 所以用 `fetch` 自己读流、自己解析 SSE 帧，代价是几十行解析代码。
 *
 * ## 帧格式（规范里我们真正用到的部分）
 *
 * - 事件之间用**空行**分隔。
 * - 以 `:` 开头的是注释 —— 心跳就是它，必须忽略，不能当成事件。
 * - 行尾可能是 `\n` 也可能是 `\r\n`。
 *
 * 不需要完整实现（服务端只发 `event: changed` + `data: 1`），但基本形状
 * 还是老实按规范来：解析错了的表现是**静默收不到推送**，一个不报错的失败。
 */

/** 断线之后第一次重连等多久。之后翻倍，到上限为止。 */
const RETRY_BASE_MS = 1000;
const RETRY_MAX_MS = 30_000;

export interface ChangeFeedOptions {
  /** 完整的 `/api/events` 地址 */
  url: string;
  /** 当前会话令牌 */
  session: string;
  /** 收到一次"有东西变了" */
  onChanged: () => void;
  /** 会话失效。**不会重试** —— 重试一个 401 只是白费力气。 */
  onUnauthorized?: () => void;
}

/**
 * 订阅变更。返回一个取消订阅的函数。
 *
 * 断线会自动重连并退避。这件事必须自己做：SSE 的自动重连是 `EventSource`
 * 的功能，而我们已经不用它了。
 */
export function subscribeChanges(opts: ChangeFeedOptions): () => void {
  const ac = new AbortController();
  let stopped = false;
  let retry = RETRY_BASE_MS;
  let timer: ReturnType<typeof setTimeout> | undefined;

  const schedule = () => {
    if (stopped) return;
    timer = setTimeout(() => void connect(), retry);
    retry = Math.min(retry * 2, RETRY_MAX_MS);
  };

  const connect = async () => {
    if (stopped) return;
    try {
      const resp = await fetch(opts.url, {
        headers: {
          Authorization: `Bearer ${opts.session}`,
          Accept: "text/event-stream",
        },
        signal: ac.signal,
      });

      if (resp.status === 401) {
        // 令牌过期了，重试多少次都是 401 —— 交给上层去重新登录
        opts.onUnauthorized?.();
        return;
      }
      if (!resp.ok || !resp.body) throw new Error(`HTTP ${resp.status}`);

      retry = RETRY_BASE_MS; // 连上了，退避清零
      await readFrames(resp.body, () => {
        if (!stopped) opts.onChanged();
      });
    } catch {
      // 网络断了、被 abort 了、服务端重启了 —— 都走重连。
      // 这里刻意不区分：对用户来说它们的下一步动作完全一样。
      if (stopped) return;
    }
    schedule();
  };

  void connect();

  return () => {
    stopped = true;
    if (timer !== undefined) clearTimeout(timer);
    // abort 会让挂着的 read() 抛错，pump 循环随即退出
    ac.abort();
  };
}

/** 一帧里有没有 `data:` 行。只有注释（心跳）的帧不算事件。 */
function hasData(frame: string): boolean {
  return frame.split("\n").some((line) => line.startsWith("data:"));
}

async function readFrames(
  body: ReadableStream<Uint8Array>,
  onEvent: () => void
): Promise<void> {
  const reader = body.getReader();
  const decoder = new TextDecoder();
  let buf = "";

  for (;;) {
    const { done, value } = await reader.read();
    if (done) return;

    // 先把 \r\n 归一化再找空行。**必须在拼接之后做** ——
    // 一个 chunk 以 \r 结尾、下一个以 \n 开头时，分开处理就漏了。
    buf += decoder.decode(value, { stream: true });
    buf = buf.replace(/\r\n/g, "\n");

    let idx = buf.indexOf("\n\n");
    while (idx !== -1) {
      const frame = buf.slice(0, idx);
      buf = buf.slice(idx + 2);
      if (hasData(frame)) onEvent();
      idx = buf.indexOf("\n\n");
    }
  }
}
