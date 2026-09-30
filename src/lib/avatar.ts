import { useEffect, useState } from "react";
import type { NoteApi } from "./apiContext";
import { useAttachmentUrl } from "./attachmentUrl";

/**
 * 自定义头像。
 *
 * ## 为什么存 sha 而不是存字节
 *
 * 头像是**这台机器上的外观偏好**，不是笔记内容。所以：
 *
 * - 字节进现有的附件存储（桌面端 SQLite，网页端服务端）—— 复用
 *   `saveAttachment` / `readAttachment`，不去新造一个"设置表存 BLOB"的路子；
 * - 设置（只有 64 个字符的 sha）落 localStorage，和字号档位同一个位置。
 *
 * 这么分是因为字节可能有几百 KB，而 localStorage 通常只有 5 MB 且是**同步**的
 * —— 每次渲染读一个大字符串会把首屏拖垮。sha 读起来是常量级的。
 *
 * 代价是头像不参与同步：换台机器要重新设一次。这是对的 —— 它描述的是"这台机器上
 * 屏幕里那个圆圈长什么样"，不是数据。
 */

/** localStorage 的键。和字号档位一样，两端各存各的。 */
const AVATAR_KEY = "messagenote.avatarSha";

/** 存着的 sha；没设过（或者被清掉了）返回 null。 */
export function loadAvatarSha(): string | null {
  const raw = localStorage.getItem(AVATAR_KEY);
  // 长度锁死 64 位小写 hex：localStorage 是可以被手改的，
  // 脏值会让后面每一次取字节都白跑一趟。
  return raw && /^[0-9a-f]{64}$/.test(raw) ? raw : null;
}

/** 存一张新头像，存好后返回它的 sha。 */
export async function uploadAvatar(api: NoteApi, file: File): Promise<string> {
  const bytes = new Uint8Array(await file.arrayBuffer());
  const sha = await api.saveAttachment(bytes);
  localStorage.setItem(AVATAR_KEY, sha);
  announceAvatarChange();
  return sha;
}

/** 换回默认。 */
export function clearAvatar() {
  localStorage.removeItem(AVATAR_KEY);
  announceAvatarChange();
}

/**
 * 设过之后立刻刷新。
 *
 * `storage` 事件在**别的**窗口里派发，规范明确说不在改动的那个窗口里发 ——
 * 所以本窗口得手动补一次，否则换完头像界面要等下次刷新才变。
 */
export function announceAvatarChange() {
  window.dispatchEvent(new StorageEvent("storage", { key: AVATAR_KEY }));
}

/**
 * 当前头像的字节 URL，没设过（或取不到字节）就是 null。
 *
 * ## 为什么复用附件缓存，而不是自己读一次
 *
 * 头像是**全应用唯一的一张图**，但界面上有 N 个 `Avatar`（每条消息一个）。
 * 每个实例各自 `readAttachment` 的话，77 条记录就是 77 次读盘、77 个
 * object URL —— 而它们要显示的其实是同一份字节。
 *
 * `attachmentUrl.ts` 的模块级缓存本来就写了"同一个 sha 的多个请求合并成
 * 一次"，正是为这种情况准备的。自己再写一份缓存就是 DRY 违反，而且会让
 * "字节到底存了没有"这个判断散落在两个地方。
 *
 * ## 为什么空串是安全的
 *
 * `useAttachmentUrl` 只在 `sha` 非空时才发起请求，所以"没设过"可以直接传空串
 * 进去。**条件调用 hook 才是真正的错误**（`sha ? useAttachmentUrl(sha) : null`
 * 在 sha 由有变无时会少调一个 hook），而这里两个分支调的是同一个 hook。
 */
export function useAvatarUrl(): string | null {
  const [sha, setSha] = useState(loadAvatarSha);

  // 别的窗口改了设置、或本页刚设过，localStorage 变更会在这里冒出来。
  useEffect(() => {
    const onStorage = (e: StorageEvent) => {
      if (e.key !== AVATAR_KEY) return;
      setSha(loadAvatarSha());
    };
    window.addEventListener("storage", onStorage);
    return () => window.removeEventListener("storage", onStorage);
  }, []);

  return useAttachmentUrl(sha ?? "");
}
