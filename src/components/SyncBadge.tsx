import type { SyncStatus } from "../lib/types";

interface Props {
  configured: boolean;
  status: SyncStatus | null;
  onOpen: () => void;
}

/**
 * 顶栏上的同步状态胶囊。
 *
 * 刻意做得低调、可点：同步失败在笔记应用里通常只是"现在没网"，
 * 弹窗打断用户是不对的，但状态又必须看得见 —— 否则用户会怀疑
 * "我这条到底存哪儿了"。
 */
export function SyncBadge({ configured, status, onOpen }: Props) {
  let cls = "sync-badge";
  let label = "未配置同步";
  let title = "点击配置同步服务端";

  if (configured) {
    if (!status) {
      cls += " idle";
      label = "待同步";
      title = "点击打开同步设置";
    } else if (status.ok) {
      cls += " ok";
      label = "已同步";
      title = `推 ${status.pushed} · 拉 ${status.pulled}${
        status.conflicts > 0 ? ` · 新增冲突副本 ${status.conflicts}` : ""
      }`;
    } else {
      cls += " err";
      label = "同步失败";
      title = status.message;
    }
  }

  return (
    <button className={cls} onClick={onOpen} title={title}>
      <span className="sync-dot" />
      {label}
    </button>
  );
}
