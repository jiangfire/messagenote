import { useAvatarUrl, useUserProfile } from "../lib/user";

/**
 * 侧边栏左下角的用户区：头像 + 用户名 + 备注。
 *
 * 它是"用户自己"的唯一设置入口 —— 头像、用户名、备注都从这里进（点开
 * `UserSettings`）。头像设置原先散在消息流的每一行上（那里现在是纯展示的
 * `Avatar`），2026-10-12 用户拍板收拢到这里。
 *
 * 没设过用户名时仍显示「我」：和消息流头像的默认字符一致，
 * 不引入第二个"默认叫什么"的说法。
 */
export function UserBadge({ onOpen }: { onOpen: () => void }) {
  const url = useAvatarUrl();
  const { name, bio } = useUserProfile();

  return (
    <button type="button" className="user-badge" onClick={onOpen} title="用户资料设置">
      <span className="avatar user-badge-avatar">
        {url ? <img src={url} alt="" /> : "我"}
      </span>
      <span className="user-badge-text">
        <span className="user-badge-name">{name.trim() || "我"}</span>
        {bio.trim() && <span className="user-badge-bio">{bio.trim()}</span>}
      </span>
    </button>
  );
}
