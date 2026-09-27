import {
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type KeyboardEvent,
} from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { LogicalSize } from "@tauri-apps/api/dpi";
import { useApi } from "../lib/apiContext";
import { errorText } from "../lib/errors";

/** 浮层宽度固定，高度随内容增长（见下面的自适应逻辑）。 */
const WINDOW_W = 680;
/** 输入区最大高度，超过就内部滚动 —— 再高就不再是"随手记一笔"了。 */
const MAX_INPUT_H = 260;

/**
 * 捕获浮层 —— 整个产品最重要的一个界面。
 *
 * 它存在的唯一理由是**把「想记」到「记完」之间的动作压到最少**：
 * 按快捷键 → 打字 → 回车。没有标题、没有频道、没有标签、没有保存按钮。
 *
 * 注意：窗口是隐藏而不是销毁的，所以组件状态一直活着 ——
 * 半途按 Esc 收起的草稿，下次唤起还在。丢掉一个没写完的念头，
 * 比多留一行文字讨厌得多。
 */
export default function CaptureApp() {
  // 浮层只存在于桌面端，所以 `desktop` 一定不为 null。
  // 用 `?.` 是因为类型上它可以是 null —— 而且万一哪天真在网页端复用它，
  // 少收起一次窗口远好过整个界面崩掉。
  const { api, desktop } = useApi();
  const [draft, setDraft] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [sending, setSending] = useState(false);

  const taRef = useRef<HTMLTextAreaElement>(null);
  const shellRef = useRef<HTMLDivElement>(null);
  const lastHeight = useRef(0);

  const win = useMemo(() => getCurrentWindow(), []);

  // 每次浮层被唤起都要把光标放回输入框。
  // 从"按快捷键"到"能开始打字"之间不该插入任何多余动作。
  useEffect(() => {
    const pending = win.onFocusChanged(({ payload: focused }) => {
      if (focused) {
        taRef.current?.focus();
        setError(null);
      }
    });
    taRef.current?.focus();
    return () => {
      void pending.then((unlisten) => unlisten());
    };
  }, [win]);

  // 输入框随内容长高，窗口跟着一起长。
  // 高度是**量出来的**而不是算出来的：把内边距、提示条高度硬编码成常量，
  // 以后改 CSS 就会出现窗口与内容不匹配的缝隙。
  useLayoutEffect(() => {
    const ta = taRef.current;
    const shell = shellRef.current;
    if (!ta || !shell) return;

    ta.style.height = "auto";
    ta.style.height = `${Math.min(ta.scrollHeight, MAX_INPUT_H)}px`;

    const raf = requestAnimationFrame(() => {
      const h = Math.ceil(shell.getBoundingClientRect().height);
      // 只在真的变了才 setSize，否则会和布局形成来回抖动的循环
      if (Math.abs(h - lastHeight.current) > 1) {
        lastHeight.current = h;
        void win.setSize(new LogicalSize(WINDOW_W, h));
      }
    });
    return () => cancelAnimationFrame(raf);
  }, [draft, win]);

  async function send() {
    const body = draft.trim();
    if (!body || sending) return;

    setSending(true);
    try {
      // 永远落收件箱，刻意不给浮层任何"选择去向"的能力。
      // 一旦这里能选频道，捕获动作就重新带上了决策成本，
      // 这个浮层也就退化成一个难用的新建窗口。
      await api.appendMessage(body, null);
      setDraft("");
      setError(null);
      await desktop?.hideCapture();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setSending(false);
    }
  }

  function onKeyDown(e: KeyboardEvent<HTMLTextAreaElement>) {
    if (e.key === "Escape") {
      e.preventDefault();
      // 保留草稿，只收起窗口
      void desktop?.hideCapture();
      return;
    }

    if (e.key !== "Enter") return;
    if (e.shiftKey) return; // Shift+Enter 换行

    // 中文/日文输入法用回车确认候选词。不挡这一下，
    // 用户选词的同时消息就被发出去了。
    const native = e.nativeEvent as globalThis.KeyboardEvent & { keyCode?: number };
    if (native.isComposing || native.keyCode === 229) return;

    e.preventDefault();
    void send();
  }

  return (
    <div className="cap-shell" ref={shellRef}>
      <div className="cap-panel">
        <textarea
          ref={taRef}
          className="cap-input"
          rows={1}
          value={draft}
          placeholder="记点什么…"
          spellCheck={false}
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={onKeyDown}
        />
        <div className="cap-foot">
          <span className="cap-keys">
            <kbd>Enter</kbd> 存下 · <kbd>Shift</kbd>+<kbd>Enter</kbd> 换行 ·{" "}
            <kbd>Esc</kbd> 收起
          </span>
          {error ? (
            <span className="cap-error" title={error}>
              {error}
            </span>
          ) : (
            <span className="cap-target">📥 收件箱</span>
          )}
        </div>
      </div>
    </div>
  );
}
