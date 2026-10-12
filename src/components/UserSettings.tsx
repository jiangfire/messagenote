import { useRef, useState } from "react";
import { useApi } from "../lib/apiContext";
import { errorText } from "../lib/errors";
import {
  clearAvatar,
  uploadAvatar,
  useAvatarUrl,
  useUserProfile,
  saveUserName,
  saveUserBio,
} from "../lib/user";

/**
 * 用户资料设置：头像、用户名、备注。
 *
 * 入口是左下角的用户区（`UserBadge`）。头像设置原来散在消息流的**每一行**上
 * （那里现在是纯展示的 `Avatar`），2026-10-12 用户拍板收拢到这里，和用户名、
 * 备注一起构成"用户自己"的唯一入口。
 *
 * **没有保存按钮**：三样都是本机外观偏好（localStorage / 附件存储），改了即存
 * —— `lib/user.ts` 的 announce 机制让左下角用户区和消息流当场跟着变。
 * 一个"保存"按钮在这里只会制造"我到底存没存"的犹豫。
 */
export function UserSettings({ onClose }: { onClose: () => void }) {
  const { api } = useApi();
  const url = useAvatarUrl();
  const { name, bio } = useUserProfile();
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
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="modal-backdrop" onMouseDown={onClose}>
      <div className="modal user-settings" onMouseDown={(e) => e.stopPropagation()}>
        <h2>用户资料</h2>

        <div className="avatar-row">
          <span className="avatar-wrap">
            <button
              type="button"
              className="avatar avatar-btn avatar-lg"
              onClick={() => picker.current?.click()}
              disabled={busy}
              title={url ? "更换头像" : "设置头像"}
              aria-label={url ? "更换头像" : "设置头像"}
            >
              {url ? <img src={url} alt="" /> : "我"}
            </button>

            {/* 换回默认。只有真的设过才给这个出口，否则它是个永远点不动的装饰。
                上传进行中禁用：否则"上传完成写回 sha"会赢过刚点的清除，
                头像看起来清不掉。 */}
            {url && (
              <button
                type="button"
                className="avatar-clear"
                onClick={clearAvatar}
                disabled={busy}
                title="换回默认头像"
                aria-label="换回默认头像"
              >
                ×
              </button>
            )}

            {/* 隐藏的 file input。不能 display:none（Tauri 里点它不会打开系统
                文件对话框），压成 1px 挪出可视区，既能点又看不见。 */}
            <input
              ref={picker}
              className="avatar-input"
              type="file"
              accept="image/*"
              onChange={(e) => void onPick(e)}
              aria-hidden="true"
              tabIndex={-1}
            />
          </span>

          <div className="avatar-row-text">
            <button
              type="button"
              className="btn tiny"
              onClick={() => picker.current?.click()}
              disabled={busy}
            >
              {url ? "换一张图" : "选一张图"}
            </button>
            {/* 图片存不下时要说出来。上传失败如果一声不响，用户只会觉得"这功能坏了"。 */}
            {error && (
              <span className="user-settings-error" role="alert">
                {error}
              </span>
            )}
          </div>
        </div>

        <label className="field">
          <span>用户名</span>
          <input
            className="user-name-input"
            value={name}
            placeholder="我"
            maxLength={30}
            onChange={(e) => saveUserName(e.target.value)}
          />
        </label>

        <label className="field">
          <span>备注</span>
          <textarea
            className="user-bio-input"
            value={bio}
            rows={3}
            maxLength={200}
            placeholder="一句话介绍自己，显示在左下角"
            onChange={(e) => saveUserBio(e.target.value)}
          />
        </label>

        <p className="muted small">
          这些只保存在这台机器上（和字号一样），不会同步到别的设备 ——
          它们描述的是"这台机器上的我"，不是笔记内容。
        </p>

        <div className="modal-actions">
          <button className="btn primary" onClick={onClose}>
            完成
          </button>
        </div>
      </div>
    </div>
  );
}
