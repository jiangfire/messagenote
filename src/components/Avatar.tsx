import { useAvatarUrl } from "../lib/user";

/**
 * 消息流里的头像。**纯展示** —— 换头像的入口统一收在左下角的用户区
 * （`UserBadge` → `UserSettings`），一列消息里几十个可点的圆圈只会是噪音。
 *
 * 没有自定义头像时仍显示「我」：那个字符不是占位符，它在纯文字流里承担了
 * "这是对话不是日志"的视觉作用，去掉它整列会塌下去。
 */
export function Avatar() {
  const url = useAvatarUrl();
  return <span className="avatar">{url ? <img src={url} alt="" /> : "我"}</span>;
}
