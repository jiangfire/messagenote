import { useEffect, useState } from "react";
import { useApi } from "../lib/apiContext";
import { errorText } from "../lib/errors";
import type { LlmConfig } from "../lib/types";

/**
 * AI（标签建议）的模型配置。**只有桌面端有。**
 *
 * 这里用注入的 `desktop` 而不是直接 import `lib/api`：后者会把
 * `@tauri-apps/api` 拖进模块图，而 App.tsx 是静态引入本组件的 ——
 * 结果是**网页端的 bundle 里也带上了 Tauri 的 IPC 代码**。
 *
 * ## 隐私：这件事必须说清楚
 *
 * 填了之后，**点「＋ AI 建议标签」会把那条记录的正文发给你填的那个端点**。
 * 这是这个产品最敏感的一件事 —— 笔记正文就是用户的生活。所以：
 *
 * - **默认什么都不发。** 不填这个端点，点「＋ AI 建议标签」只会得到一句
 *   "AI 还没配好：还没填模型地址"，不会有任何请求离开你的机器。
 * - 文案明写"会把正文发出去"，而不是藏在某个折叠里。
 * - 建议**优先填本地端点**（Ollama / LM Studio），文案里直接给出例子。
 */
export function AiSettings({ onClose }: { onClose: () => void }) {
  const { desktop } = useApi();
  const [cfg, setCfg] = useState<LlmConfig>({ baseUrl: "", apiKey: "", model: "" });
  const [busy, setBusy] = useState(false);
  const [loaded, setLoaded] = useState(false);
  const [note, setNote] = useState<{ ok: boolean; text: string } | null>(null);

  useEffect(() => {
    if (!desktop) return;
    void (async () => {
      try {
        setCfg(await desktop.getLlmConfig());
      } catch (e) {
        setNote({ ok: false, text: errorText(e) });
      } finally {
        setLoaded(true);
      }
    })();
  }, [desktop]);

  async function save() {
    if (!desktop) return;
    setBusy(true);
    setNote(null);
    try {
      await desktop.setLlmConfig(cfg);
      setNote({
        ok: true,
        text: "已保存。现在每条记录下面都有「＋ AI 建议标签」了。",
      });
    } catch (e) {
      setNote({ ok: false, text: errorText(e) });
    } finally {
      setBusy(false);
    }
  }

  // 网页端渲染不到这里，兜一下而已
  if (!desktop) return null;

  return (
    <div className="modal-backdrop" onMouseDown={onClose}>
      <div className="modal" onMouseDown={(e) => e.stopPropagation()}>
        <h2>AI 标签建议</h2>
        <p className="muted small">
          填一个 <b>OpenAI 兼容</b>的模型端点。OpenAI / DeepSeek / 通义 / Kimi /
          GLM，以及本地的 Ollama、LM Studio、vLLM 都用同一种请求格式，换一家不用改设置。
        </p>

        <div className="note err">
          点「＋ AI 建议标签」会把那条记录的正文<b>发给你填的端点</b>。
          建议优先填<b>本地</b>模型（如 <code>http://localhost:11434/v1</code>）——
          笔记正文是你的生活，不该发给第三方。什么都不填就没有这个功能。
        </div>

        <label className="field">
          <span>端点地址（填到 /v1 为止）</span>
          <input
            value={cfg.baseUrl}
            placeholder="https://api.openai.com/v1 或 http://localhost:11434/v1"
            disabled={!loaded}
            onChange={(e) => setCfg({ ...cfg, baseUrl: e.target.value })}
          />
        </label>

        <label className="field">
          <span>模型名</span>
          <input
            value={cfg.model}
            placeholder="gpt-4o-mini / deepseek-chat / qwen2.5:7b"
            disabled={!loaded}
            onChange={(e) => setCfg({ ...cfg, model: e.target.value })}
          />
        </label>

        <label className="field">
          <span>API key（本地模型可以留空）</span>
          <input
            value={cfg.apiKey}
            type="password"
            placeholder="sk-…"
            disabled={!loaded}
            onChange={(e) => setCfg({ ...cfg, apiKey: e.target.value })}
          />
        </label>

        <p className="muted small">
          key 存在本机数据库里，跟着你的备份走，但<b>不会同步到别的设备</b>
          ——另一台机器不该突然冒出一个指向这台机器 key 的配置。
        </p>

        <p className="muted small">
          模型只会给<b>建议</b>：建议显示成虚线的灰色标签，<b>点一下才会变成真标签</b>。
          它不会自动改你的任何数据，也不会把正文发给别的设备。
        </p>

        {note && <div className={`note ${note.ok ? "ok" : "err"}`}>{note.text}</div>}

        <div className="modal-actions">
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