import { useEffect, useState } from "react";
import { api, errorText } from "../lib/api";

interface Props {
  onClose: () => void;
  /** 保存成功后通知上层：刷新"已配置"状态并触发一次同步 */
  onSaved: () => void;
}

export function SyncSettings({ onClose, onSaved }: Props) {
  const [url, setUrl] = useState("");
  const [token, setToken] = useState("");
  const [busy, setBusy] = useState(false);
  const [loaded, setLoaded] = useState(false);
  const [note, setNote] = useState<{ ok: boolean; text: string } | null>(null);

  useEffect(() => {
    void (async () => {
      try {
        const cfg = await api.getSyncConfig();
        setUrl(cfg.url);
        setToken(cfg.token);
      } catch (e) {
        setNote({ ok: false, text: errorText(e) });
      } finally {
        setLoaded(true);
      }
    })();
  }, []);

  async function test() {
    setBusy(true);
    setNote(null);
    try {
      // 打的是鉴权过的 handshake 端点，所以令牌不对时会真的失败
      const h = await api.testSyncConnection(url, token);
      setNote({ ok: true, text: `连接成功 · 协议版本 ${h.protocol}` });
    } catch (e) {
      setNote({ ok: false, text: errorText(e) });
    } finally {
      setBusy(false);
    }
  }

  async function save() {
    setBusy(true);
    setNote(null);
    try {
      await api.setSyncConfig(url, token);
      setNote({ ok: true, text: "已保存，正在同步…" });
      onSaved();
    } catch (e) {
      setNote({ ok: false, text: errorText(e) });
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="modal-backdrop" onMouseDown={onClose}>
      <div className="modal" onMouseDown={(e) => e.stopPropagation()}>
        <h2>同步服务端</h2>
        <p className="muted small">
          填入自建服务端的地址和令牌。配置存在本地数据库里 ——
          换机器时把数据库文件一起复制过去，不用重配。
        </p>

        <label className="field">
          <span>地址</span>
          <input
            value={url}
            placeholder="https://notes.example.com"
            disabled={!loaded}
            onChange={(e) => setUrl(e.target.value)}
          />
        </label>

        <label className="field">
          <span>令牌</span>
          <input
            value={token}
            type="password"
            placeholder="与服务端的 MESSAGENOTE_TOKEN 一致"
            disabled={!loaded}
            onChange={(e) => setToken(e.target.value)}
          />
        </label>

        {note && <div className={`note ${note.ok ? "ok" : "err"}`}>{note.text}</div>}

        <div className="modal-actions">
          <button
            className="btn"
            onClick={() => void test()}
            disabled={busy || !url.trim() || !token.trim()}
          >
            测试连接
          </button>
          <button className="btn primary" onClick={() => void save()} disabled={busy || !loaded}>
            保存
          </button>
          <button className="btn ghost" onClick={onClose}>
            关闭
          </button>
        </div>
      </div>
    </div>
  );
}
