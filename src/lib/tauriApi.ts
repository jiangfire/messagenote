import { listen } from "@tauri-apps/api/event";
import { api as commands } from "./api";
import type { DesktopApi, NoteApi } from "./apiContext";
import type { SyncStatus } from "./types";

/** 后台同步线程推上来的状态事件名，与 `sync_worker.rs` 的 `STATUS_EVENT` 一致。 */
const SYNC_STATUS_EVENT = "sync://status";

/**
 * 桌面端实现：走 Tauri `invoke`，就是原来那一份。
 *
 * 不重新包一层薄封装，而是直接把 `api.ts` 那个对象当 `NoteApi` 用 ——
 * 包一层只会多一个需要同步维护的地方，而它多出来的方法（同步相关的）
 * 正好是 `DesktopApi` 要的。
 */
export const tauriApi: NoteApi = commands;

export const tauriDesktop: DesktopApi = {
  hideCapture: () => commands.hideCapture(),
  getSyncConfig: () => commands.getSyncConfig(),
  setSyncConfig: (url, token) => commands.setSyncConfig(url, token),
  syncNow: () => commands.syncNow(),
  getSyncStatus: () => commands.getSyncStatus(),
  testSyncConnection: (url, token) => commands.testSyncConnection(url, token),

  onSyncStatus(handler) {
    let unlisten: (() => void) | null = null;
    let cancelled = false;

    void listen<SyncStatus>(SYNC_STATUS_EVENT, (e) => handler(e.payload)).then((un) => {
      // 组件完全可能在 listen 完成**之前**就卸载了。那种情况下这个 unlisten
      // 没人会调用，监听器会一直挂着，每次同步都往一个已卸载的组件里 setState。
      if (cancelled) un();
      else unlisten = un;
    });

    return () => {
      cancelled = true;
      unlisten?.();
    };
  },
};
