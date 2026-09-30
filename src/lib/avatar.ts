import { useEffect, useState } from "react";
import { useApi, type NoteApi } from "./apiContext";

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

export function saveAvatarSha(sha: string) {
  localStorage.setItem(AVATAR_KEY, sha);
}

export function clearAvatarSha() {
  localStorage.removeItem(AVATAR_KEY);
}

/**
 * 当前头像的字节 URL（object URL），没设过就是 null。
 *
 * **跨窗口同步**用 `storage` 事件而不是轮询：头像是在设置窗口里改的，
 * 主窗口和它不是同一个组件树，靠 React 状态传不过去。而 localStorage 变更本来
 * 就会在**其他**同源标签页里派发 `storage` 事件 —— 这是平台白送的通知，
 * 不用自己起一个订阅系统。
 *
 * 注意 `storage` 事件**不会在改动的那个窗口里触发**（规范如此），
 * 所以本窗口是靠 setState 立即生效的。
 */
export function useAvatarUrl(): string | null {
  const { api } = useApi();
  const [sha, setSha] = useState(loadAvatarSha);
  const [url, setUrl] = useState<string | null>(null);

  useEffect(() => {
    if (!sha) {
      setUrl(null);
      return;
    }
    let cancelled = false;
    let objectUrl: string | null = null;
    api
      .readAttachment(sha)
      .then((bytes) => {
        if (cancelled) return;
        // 和正文图片走同一种形态：Blob → object URL。
        // 包一层 Uint8Array 是因为 Tauri 的 ArrayBuffer 可能带 offset。
        objectUrl = URL.createObjectURL(new Blob([new Uint8Array(bytes)]));
        setUrl(objectUrl);
      })
      .catch(() => {
        // 字节取不到（清过库、换过机器）就当没设过 —— 界面上退回「我」，
        // 而不是一个永远转圈的破洞。
        if (!cancelled) setUrl(null);
      });
    return () => {
      cancelled = true;
      if (objectUrl) URL.revokeObjectURL(objectUrl);
    };
  }, [api, sha]);

  useEffect(() => {
    const onStorage = (e: StorageEvent) => {
      if (e.key !== AVATAR_KEY) return;
      setSha(loadAvatarSha());
    };
    window.addEventListener("storage", onStorage);
    return () => window.removeEventListener("storage", onStorage);
  }, []);

  return url;
}

/** 存一张新头像，存好后返回它的 sha。 */
export async function uploadAvatar(api: NoteApi, file: File): Promise<string> {
  const bytes = new Uint8Array(await file.arrayBuffer());
  const sha = await api.saveAttachment(bytes);
  saveAvatarSha(sha);
  return sha;
}

/**
 * 设过之后立刻刷新。
 *
 * `storage` 事件在**别的**窗口里派发，规范明确说不在改动的那个窗口里发 ——
 * 所以本窗口得手动补一次。这条也是为什么 `useAvatarUrl` 的 `setSha` 会被触发。
 */
export function announceAvatarChange() {
  window.dispatchEvent(new StorageEvent("storage", { key: AVATAR_KEY }));
}
