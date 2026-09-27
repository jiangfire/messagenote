import { useCallback, useMemo, useState, type FormEvent } from "react";
import App from "../App";
import { ApiProvider } from "../lib/apiContext";
import { errorText } from "../lib/errors";
import { httpApi, login, type Session } from "../lib/httpApi";

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

  const accept = useCallback((baseUrl: string, s: Session) => {
    const value: Stored = { baseUrl, session: s.session, expiresAt: s.expiresAt };
    localStorage.setItem(STORAGE_KEY, JSON.stringify(value));
    setStored(value);
    setNeedLogin(false);
  }, []);

  const api = useMemo(
    () =>
      stored
        ? httpApi({
            baseUrl: stored.baseUrl,
            session: stored.session,
            onUnauthorized,
          })
        : null,
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
      <ApiProvider api={api}>
        <App />
      </ApiProvider>

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
