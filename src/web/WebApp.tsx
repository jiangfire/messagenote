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

interface Stored extends Session {
  baseUrl: string;
}

function loadStored(): Stored | null {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return null;
    const v = JSON.parse(raw) as Stored;
    if (!v.baseUrl || !v.session || !v.expiresAt) return null;
    // 本地先判一次过期，省掉一次注定 401 的请求。
    // 服务端那边才是权威 —— 这里只是省一次往返。
    if (v.expiresAt <= Date.now()) return null;
    return v;
  } catch {
    return null;
  }
}

export default function WebApp() {
  const [stored, setStored] = useState<Stored | null>(() => loadStored());

  const clear = useCallback(() => {
    localStorage.removeItem(STORAGE_KEY);
    setStored(null);
  }, []);

  // 会话过期或被吊销时 `httpApi` 会回调到这里：界面自动退回登录页，
  // 而不是把用户留在一屏永远失败的界面上。
  const api = useMemo(
    () =>
      stored
        ? httpApi({
            baseUrl: stored.baseUrl,
            session: stored.session,
            onUnauthorized: clear,
          })
        : null,
    [stored, clear]
  );

  if (!stored || !api) {
    return <LoginScreen onSuccess={setStored} />;
  }

  return (
    <ApiProvider api={api}>
      <App />
    </ApiProvider>
  );
}

function LoginScreen({ onSuccess }: { onSuccess: (s: Stored) => void }) {
  const [baseUrl, setBaseUrl] = useState("http://127.0.0.1:8787");
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
      const value: Stored = { ...s, baseUrl: baseUrl.trim().replace(/\/+$/, "") };
      localStorage.setItem(STORAGE_KEY, JSON.stringify(value));
      onSuccess(value);
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="login-wrap">
      <form className="login-card" onSubmit={submit}>
        <h1>MessageNote</h1>
        <p className="muted">
          连上你自己的同步服务端。这里填的是服务端的<b>长期令牌</b>，
          它只用来换一个会过期的会话 —— 换到之后长期令牌不会留在这个浏览器里。
        </p>

        <label>
          服务端地址
          <input
            value={baseUrl}
            onChange={(e) => setBaseUrl(e.target.value)}
            placeholder="https://notes.example.com"
            autoComplete="url"
            spellCheck={false}
          />
        </label>

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
          {busy ? "连接中…" : "连接"}
        </button>
      </form>
    </div>
  );
}
