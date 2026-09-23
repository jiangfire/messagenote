import { useEffect, useRef, type KeyboardEvent } from "react";

interface Props {
  draft: string;
  setDraft: (v: string) => void;
  onSend: () => void;
  targetLabel: string;
  /** 数值变化即请求一次聚焦（发送后、切换频道后） */
  focusSignal: number;
  disabled?: boolean;
}

/**
 * 输入框 —— 整个产品最关键的一个组件。
 *
 * 它存在的意义就是把「记录」的启动成本压到接近零：没有标题、没有目录、
 * 没有保存按钮，写完了按回车就结束。所有整理都发生在事后。
 */
export function Composer({
  draft,
  setDraft,
  onSend,
  targetLabel,
  focusSignal,
  disabled,
}: Props) {
  const ref = useRef<HTMLTextAreaElement>(null);

  useEffect(() => {
    ref.current?.focus();
  }, [focusSignal]);

  // 随内容增高，但最多 14 行，超过就内部滚动
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, 320)}px`;
  }, [draft]);

  function handleKeyDown(e: KeyboardEvent<HTMLTextAreaElement>) {
    if (e.key !== "Enter") return;
    if (e.shiftKey) return; // Shift+Enter 换行

    // 中文/日文输入法用回车确认候选词。如果不挡这一下，
    // 用户选词的同时消息就被发出去了 —— 这是聊天类输入框最经典的 bug。
    const native = e.nativeEvent as globalThis.KeyboardEvent & { keyCode?: number };
    if (native.isComposing || native.keyCode === 229) return;

    e.preventDefault();
    if (!disabled && draft.trim()) onSend();
  }

  return (
    <div className="composer">
      <textarea
        ref={ref}
        className="composer-input"
        rows={1}
        value={draft}
        placeholder={`记点什么…（Enter 发送，Shift+Enter 换行）`}
        onChange={(e) => setDraft(e.target.value)}
        onKeyDown={handleKeyDown}
      />
      <div className="composer-bar">
        <span className="composer-target">
          发送到 <strong>{targetLabel}</strong>
        </span>
        <button
          className="send-btn"
          disabled={disabled || !draft.trim()}
          onClick={onSend}
          title="发送 (Enter)"
        >
          发送
        </button>
      </div>
    </div>
  );
}
