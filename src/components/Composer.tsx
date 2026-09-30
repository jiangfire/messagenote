import { useEffect, useRef, useState, type ClipboardEvent, type KeyboardEvent } from "react";
import { useApi } from "../lib/apiContext";
import { errorText } from "../lib/errors";
import {
  imageFilesFromClipboard,
  insertImages,
  makeImageDropHandlers,
} from "../lib/imageInsert";

interface Props {
  draft: string;
  setDraft: (v: string) => void;
  onSend: () => void;
  targetLabel: string;
  /** 数值变化即请求一次聚焦（发送后、切换频道后） */
  focusSignal: number;
  disabled?: boolean;
  /** 图片入库失败之类要说给用户听的话。由 App 统一显示，见那边的 error-bar。 */
  onError?: (message: string) => void;
}

/**
 * 输入框 —— 整个产品最关键的一个组件。
 *
 * 它存在的意义就是把「记录」的启动成本压到接近零：没有标题、没有目录、
 * 没有保存按钮，写完了按回车就结束。所有整理都发生在事后。
 *
 * 图片走同一个原则：贴进来就存下、就在光标处出现引用，不问"存哪儿"。
 * 字节进本地库（网页端进服务端），正文里只留 `attachment:<sha256>`。
 */
export function Composer({
  draft,
  setDraft,
  onSend,
  targetLabel,
  focusSignal,
  disabled,
  onError,
}: Props) {
  const { api } = useApi();
  const ref = useRef<HTMLTextAreaElement>(null);
  const filePicker = useRef<HTMLInputElement>(null);
  /** 拖拽悬停中。只用来加高亮类名 —— 拖拽的判定全在 dataTransfer 上。 */
  const [dropActive, setDropActive] = useState(false);

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

  /**
   * 粘贴图片。
   *
   * 只在剪贴板里真的有图片时才 `preventDefault`：如果是普通文字，
   * 让浏览器按默认行为插到光标处就好 —— 自己接管纯文本粘贴会丢掉
   * 富文本转换、撤销栈这些我们没打算重写的东西。
   */
  function handlePaste(e: ClipboardEvent<HTMLTextAreaElement>) {
    const files = imageFilesFromClipboard(e.clipboardData?.items ?? null);
    if (files.length === 0 || disabled) return;
    e.preventDefault(); // 不挡的话 WebView 会自己插一段它理解的图片 HTML
    void insertImages(api, e.currentTarget, files, setDraft).catch((err) =>
      onError?.(errorText(err))
    );
  }

  const drop = makeImageDropHandlers(
    api,
    ref,
    setDraft,
    setDropActive,
    (err) => onError?.(errorText(err))
  );

  /**
   * 通过按钮选文件。
   *
   * 拖拽和粘贴都有各自的盲区：图在手机上、在一段聊天记录里、在剪贴板里
   * （截图工具没开）时，这两条路都够不着。留一个"去选一张"的入口，
   * 用户至少永远有一条路能把图送进来。
   *
   * 选完立刻清空 `value`：同一个文件连选两次时浏览器不会再派发 change，
   * 而"重新选一次同一张图"恰恰是换回来最自然的操作。
   */
  function onPickFiles(e: React.ChangeEvent<HTMLInputElement>) {
    const files = Array.from(e.target.files ?? []);
    e.target.value = "";
    const el = ref.current;
    if (files.length === 0 || !el || disabled) return;
    el.focus();
    void insertImages(api, el, files, setDraft).catch((err) => onError?.(errorText(err)));
  }

  return (
    // 拖拽事件挂在整个 composer 上而不是 textarea 上：用户瞄的是"这个输入框"，
    // 落在它周边一圈的边距里也该算数。
    <div
      className={`composer${dropActive ? " drop-active" : ""}`}
      onDragOver={disabled ? undefined : drop.onDragOver}
      onDragLeave={disabled ? undefined : drop.onDragLeave}
      onDrop={disabled ? undefined : drop.onDrop}
    >
      <textarea
        ref={ref}
        className="composer-input"
        rows={1}
        value={draft}
        placeholder="记点什么…"
        onChange={(e) => setDraft(e.target.value)}
        onKeyDown={handleKeyDown}
        onPaste={handlePaste}
      />
      <div className="composer-bar">
        <span className="composer-target">
          {dropActive ? (
            "松手就把图片存进来"
          ) : (
            <>
              发送到 <strong>{targetLabel}</strong>
            </>
          )}
        </span>
        <div className="composer-actions">
          {/* 上传附件。放在发送键旁边而不是里面：发送是这个输入框的主要动作，
              附件是一次性的补充，两者混在一起会让"回车就发出去"这件事变模糊。 */}
          <button
            type="button"
            className="attach-btn"
            onClick={() => filePicker.current?.click()}
            disabled={disabled}
            title="选一张图片（也可以直接粘贴或拖进来）"
            aria-label="选择图片"
          >
            📎
          </button>
          <input
            ref={filePicker}
            className="attach-input"
            type="file"
            accept="image/*"
            multiple
            onChange={onPickFiles}
            aria-hidden="true"
            tabIndex={-1}
          />
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
    </div>
  );
}
