import { useRef, useState } from "react";
import { useApi } from "../lib/apiContext";
import { errorText } from "../lib/errors";
import { announceAvatarChange, clearAvatarSha, uploadAvatar, useAvatarUrl } from "../lib/avatar";

/**
 * 消息流里的头像。
 *
 * 之前这里是一个写死的「我」字符 —— 它只表达"这是一个人在说话"，用户没有任何
 * 办法换成自己的图。现在它是一个可点的入口：点一下选张图，当场就换。
 *
 * 没有自定义头像时仍显示「我」：那个字符不是占位符，它在纯文字流里承担了
 * "这是对话不是日志"的视觉作用，去掉它整列会塌下去。
 *
 * 悬停时才露出提示文字，是为了让那一列在正常状态下保持安静。
 */
export function Avatar() {
  const { api } = useApi();
  const url = useAvatarUrl();
  const picker = useRef<HTMLInputElement>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function onPick(e: React.ChangeEvent<HTMLInputElement>) {
    const file = e.target.files?.[0];
    // 先清空：同一个文件连选两次时，第二次不派发 change。
    // 用户"换回去"最典型的操作就是重新选一次刚选过的那张。
    e.target.value = "";
    if (!file) return;

    setBusy(true);
    setError(null);
    try {
      await uploadAvatar(api, file);
      // 让本窗口和其它窗口都立刻看到新头像
      announceAvatarChange();
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy(false);
    }
  }

  function reset() {
    clearAvatarSha();
    announceAvatarChange();
  }

  return (
    <span className="avatar-wrap">
      <button
        type="button"
        className="avatar avatar-btn"
        onClick={() => picker.current?.click()}
        disabled={busy}
        title={url ? "更换头像" : "设置头像"}
        aria-label={url ? "更换头像" : "设置头像"}
      >
        {url ? <img src={url} alt="" /> : "我"}
      </button>

      {/* 换回默认。只有真的设过才给这个出口，否则它是个永远点不动的装饰。 */}
      {url && (
        <button
          type="button"
          className="avatar-clear"
          onClick={reset}
          title="换回默认头像"
          aria-label="换回默认头像"
        >
          ×
        </button>
      )}

      <input
        ref={picker}
        className="avatar-input"
        type="file"
        accept="image/*"
        onChange={(e) => void onPick(e)}
        aria-hidden="true"
        tabIndex={-1}
      />

      {/* 图片存不下时要说出来。上传失败如果一声不响，用户只会觉得"这功能坏了"。 */}
      {error && (
        <span className="avatar-error" role="alert">
          {error}
        </span>
      )}
    </span>
  );
}
