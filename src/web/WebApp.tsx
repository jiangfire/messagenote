import { useCallback, useEffect, useMemo, useState, type FormEvent } from "react";
import App from "../App";
import { ApiProvider } from "../lib/apiContext";
import { errorText } from "../lib/errors";
import { httpApi, login, type Session } from "../lib/httpApi";
import { uuidV4 } from "../lib/ids";
import { count, enqueue, replay } from "./outbox";
import { subscribeChanges as subscribeEvents } from "./sse";

/**
 * 网页端入口：登录 → 复用同一套界面。
 *
 * 和桌面端**跑的是同一份 `App`**，区别只在注入的 `api` 是 HTTP 实现，
 * 而且 `desktop` 是 null（浏览器里没有同步配置、没有捕获浮层）。
 */
const STORAGE_KEY = "messagenote.session";

interface Stored {
  baseUrl: string;
  session: string;
  expiresAt: number;
}

/**
 * 读出来，**不管过期没过期**。
 *
 * 过期的会话也要留着 —— 里面的 `baseUrl` 是用户手填的，重新登录时不该再让他
 * 填一遍。会话本身过不过期由 [`isFresh`] 单独判断。
 */
function loadStored(): Stored | null {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return null;
    const v = JSON.parse(raw) as Stored;
    if (!v.baseUrl || !v.session || !v.expiresAt) return null;
    return v;
  } catch {
    return null;
  }
}

function isFresh(s: Stored): boolean {
  // 本地先判一次，省掉一次注定 401 的请求。服务端那边才是权威 ——
  // 而且服务端会滑动续期，所以这里判出"过期"通常意味着真的很久没用了。
  return s.expiresAt > Date.now();
}

export default function WebApp() {
  const [stored, setStored] = useState<Stored | null>(loadStored);
  const [needLogin, setNeedLogin] = useState(() => {
    const s = loadStored();
    return !s || !isFresh(s);
  });

  const onUnauthorized = useCallback(() => setNeedLogin(true), []);

  /** 离线队列里压着几条。0 表示没有，界面不显示任何东西。 */
  const [queued, setQueued] = useState(0);
  /**
   * 队列卡住了吗（以及为什么）。
   *
   * 没有它的话，一条**永久失败**的记录（毒丸，比如服务端一直说这条太长）
   * 会把整个队列堵死，而界面上只是「N 条正在补发…」停在那里不动 ——
   * 用户分不清是"还没联网"还是"有东西发不出去"，也没有任何办法自己解决。
   */
  const [queueStuck, setQueueStuck] = useState<string | null>(null);

  const accept = useCallback((baseUrl: string, s: Session) => {
    const value: Stored = { baseUrl, session: s.session, expiresAt: s.expiresAt };
    localStorage.setItem(STORAGE_KEY, JSON.stringify(value));
    setStored(value);
    setNeedLogin(false);
  }, []);

  /**
   * 不带离线兜底的**原始** httpApi。
   *
   * 单独拆出来，是因为有两套语义在打架：
   * - **交互发送**（下面那个 `api`）：失败就入队、返回假回执、不抛错 ——
   *   让用户写的东西看起来安全了，输入框也能清空。
   * - **重放**（`web/outbox.ts`）：失败就停、留在队列、抛错 ——
   *   队列是按时间排的，坏一条就得整体停在这儿等下次。
   *
   * 曾经重放用的是**带兜底的那个**，于是两层撞在一起：网络抖一下、
   * `fetch` 抛 TypeError，兜底层把它**重新入队**并返回假成功，重放层以为发成了
   * 就 `drop(item.id)` —— 整队条目既没到服务端、又从队列里消失了。
   * 重放必须直连 `base`。
   */
  /**
 * 服务端滑动续期了，写回本地 —— **刻意不更新 React state**。
 *
 * 只写 localStorage 就够了：`isFresh` 读的是 localStorage 里的值，
 * 下次打开页面就是新的。
 *
 * 为什么不走 `setStored`：那会让 `stored` 变 → `api` 的 useMemo 依赖变 →
 * `base` 和 `api` 全部重建 → **离线队列的重放 effect 重跑一遍**
 * （见下面那个 effect，依赖是 `base`）。续期是每个请求都可能发生的，
 * 也就是说这一轮同步可能顺手重放好几次 —— 队列里的东西虽然是幂等的，
 * 但那是没有必要的网络往返，而且在弱网下会拖慢真正要发的东西。
 */
const onSessionRenewed = useCallback((expiresAt: number) => {
    try {
      const raw = localStorage.getItem(STORAGE_KEY);
      if (!raw) return;
      const v = JSON.parse(raw) as Stored;
      if (!v.baseUrl || !v.session) return;
      localStorage.setItem(STORAGE_KEY, JSON.stringify({ ...v, expiresAt }));
    } catch {
      // 存不进去就算了：最坏结果是下次打开页面多登一次，
      // 而一个同步的存储失败不该把整个应用搞崩。
    }
  }, []);

  const base = useMemo(() => {
    if (!stored) return null;
    return httpApi({
      baseUrl: stored.baseUrl,
      session: stored.session,
      onUnauthorized,
      onSessionRenewed,
    });
  }, [stored, onUnauthorized, onSessionRenewed]);

  const api = useMemo(() => {
    if (!base) return null;

    return {
      ...base,
      /**
       * 断网时不报错，而是**存进离线队列**。
       *
       * 这是网页端唯一必须离线可用的操作 —— 桌面端本来就有一份本地库，
       * 而浏览器里没有。见 `web/outbox.ts`。
       *
       * 返回一条**本地回执**（一个长得像 Message 的对象）：调用方拿到它就会
       * 清空输入框，这正是我们要的效果 —— 用户写的东西已经安全地躺在队列里，
       * 不该再留在输入框里让他担心。这条回执**不会**出现在时间线上；
       * 真正那条等联网重放之后才由服务端返回。
       */
      appendMessage: async (body: string, channelId: string | null, id?: string) => {
        const key = id ?? uuidV4();
        const now = Date.now();

        const stash = async () => {
          await enqueue({ id: key, body, createdAt: now, channelId });
          setQueued(await count());
          return {
            id: key,
            channelId: channelId ?? "inbox",
            body,
            createdAt: now,
            updatedAt: now,
            tags: [],
          };
        };

        // 先看浏览器自己怎么说，省掉一次注定失败的请求
        if (!navigator.onLine) return stash();

        try {
          return await base.appendMessage(body, channelId, key);
        } catch (e) {
          // fetch 在网络不可达时抛 TypeError。服务端返回的错误被我们包装成
          // 普通 Error，**不该**当成离线 —— 那会让一条 400 永远躺在队列里
          // 反复重放，而且每次都被拒。
          if (e instanceof TypeError) return stash();
          throw e;
        }
      },
    };
  }, [base]);

  /**
   * 重放离线队列。
   *
   * 挂载时也跑一次：用户很可能正是"断网时关掉页面、联网后才重新打开"，
   * 只监听 `online` 事件的话那一批会一直躺在里面。
   *
   * **直连 `base`，不走上面那个带兜底的 `api`** —— 兜底层会把失败的条目
   * 重新入队并返回假成功，重放层就会把刚重新入队的条目删掉（丢数据）。
   * 重放的"失败就停、留在队列"语义由 `replay` 自己的 catch 负责。
   */
  useEffect(() => {
    if (!base) return;
    let alive = true;

    const run = async () => {
      const r = await replay(async (item) => {
        // 频道要还原成**入队时**的那个，不是重放时界面正开着的那个
        await base.appendMessage(item.body, item.channelId ?? null, item.id);
      });
      if (alive) {
        setQueued(r.remaining);
        setQueueStuck(r.stopped?.reason ?? null);
      }
    };

    void run();
    const onOnline = () => void run();
    window.addEventListener("online", onOnline);
    return () => {
      alive = false;
      window.removeEventListener("online", onOnline);
    };
  }, [base]);

  /**
   * 服务端推送的订阅。
   *
   * 用 `new URL` 而不是字符串拼接：用户填的服务端地址末尾有没有斜杠都可能，
   * 而 `//api/events` 这种地址的失败方式是一个 404，看起来和推送无关。
   */
  const subscribeChanges = useMemo(
    () =>
      stored
        ? (handler: () => void) =>
            subscribeEvents({
              url: new URL("/api/events", stored.baseUrl).toString(),
              session: stored.session,
              onChanged: handler,
              onUnauthorized,
            })
        : undefined,
    [stored, onUnauthorized]
  );

  // 首次使用：连服务端地址都还没有，只能整屏登录
  if (!stored || !api) {
    return <LoginScreen onSuccess={accept} />;
  }

  return (
    <>
      {/*
        `App` **始终挂载**。
        会话中途失效时如果把它卸载掉，用户正在写的那段文字会跟着一起消失 ——
        那就成了"因为登录过期而丢掉一条笔记"，正是这个项目最不能接受的事。
        让重新登录以浮层的形式盖在上面，草稿就还在。

        另外：重登录会让 `stored` 变化 → `api` 换新 → App 的 effect 依赖它，
        所以数据会自动重取一遍，不需要额外通知。
      */}
      <ApiProvider api={api} subscribeChanges={subscribeChanges}>
        <App />
      </ApiProvider>

      {/*
        离线队列的提示。
        **必须有。** 断网时输入框会正常清空（东西确实存下来了），
        但时间线上暂时看不到它 —— 没有一个明确的说法，用户只会以为丢了一条。
      */}
      {queued > 0 && (
        <div className="offline-pill" role="status">
          {queueStuck
            ? `${queued} 条发不出去（${queueStuck}）—— 内容有问题的可以改短再试`
            : navigator.onLine
              ? `${queued} 条正在补发…`
              : `${queued} 条已离线保存，联网后自动发送`}
        </div>
      )}

      {needLogin && (
        <LoginScreen baseUrl={stored.baseUrl} overlay onSuccess={accept} />
      )}
    </>
  );
}

function LoginScreen({
  baseUrl: fixedBaseUrl,
  overlay = false,
  onSuccess,
}: {
  /** 已经填过一次就不再让用户填 —— 地址不会变 */
  baseUrl?: string;
  /** 盖在界面之上（会话失效），而不是整屏接管（首次使用） */
  overlay?: boolean;
  onSuccess: (baseUrl: string, s: Session) => void;
}) {
  // 同源部署时地址栏是多余的 —— 页面自己就知道 origin。
  // 而且它顺便消掉了"地址填错"这一整类问题：用户不可能把服务端地址填成别的。
  const [baseUrl, setBaseUrl] = useState(
    fixedBaseUrl ?? (typeof window === "undefined" ? "" : window.location.origin)
  );
  const [token, setToken] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function submit(e: FormEvent) {
    e.preventDefault();
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      const s = await login(baseUrl, token);
      onSuccess(baseUrl.trim().replace(/\/+$/, ""), s);
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className={`login-wrap${overlay ? " login-overlay" : ""}`}>
      <form className="login-card" onSubmit={submit}>
        {overlay ? (
          <>
            <h1>登录已过期</h1>
            <p className="muted">
              <b>你正在写的内容还在</b> —— 重新登录后会回到原来那一屏，
              再点一次发送就行。
            </p>
          </>
        ) : (
          <>
            <h1>MessageNote</h1>
            <p className="muted">
              连上你自己的同步服务端。这里填的是服务端的<b>长期令牌</b>，
              它只用来换一个会过期的会话 —— 换到之后长期令牌不会留在这个浏览器里。
            </p>
          </>
        )}

        {fixedBaseUrl ? (
          <p className="muted small">服务端：{fixedBaseUrl}</p>
        ) : (
          <label>
            服务端地址
            <input
              type="text"
              value={baseUrl}
              onChange={(e) => setBaseUrl(e.target.value)}
              placeholder="https://notes.example.com"
              autoComplete="url"
              spellCheck={false}
            />
          </label>
        )}

        <label>
          令牌
          <input
            type="password"
            value={token}
            onChange={(e) => setToken(e.target.value)}
            placeholder="服务端的 MESSAGENOTE_TOKEN"
            autoComplete="current-password"
            spellCheck={false}
          />
        </label>

        {error && <div className="login-error">{error}</div>}

        <button type="submit" disabled={busy || !token.trim() || !baseUrl.trim()}>
          {busy ? "连接中…" : overlay ? "重新登录" : "连接"}
        </button>
      </form>
    </div>
  );
}
