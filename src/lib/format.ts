const WEEKDAYS = ["周日", "周一", "周二", "周三", "周四", "周五", "周六"];

function pad(n: number): string {
  return n < 10 ? `0${n}` : String(n);
}

/** 同一天的时间戳归到同一个 key，用于插入日期分隔条。 */
export function dayKey(ms: number): string {
  const d = new Date(ms);
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
}

export function formatTime(ms: number): string {
  const d = new Date(ms);
  return `${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

/**
 * 日期分隔条文案。
 * 「今天 / 昨天」比具体日期更容易扫读 —— 记录类应用里绝大多数内容都在最近几天。
 */
export function formatDayLabel(ms: number): string {
  const d = new Date(ms);
  const today = new Date();
  const startOfToday = new Date(
    today.getFullYear(),
    today.getMonth(),
    today.getDate()
  ).getTime();
  const diffDays = Math.round((startOfToday - new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime()) / 86_400_000);

  if (diffDays === 0) return "今天";
  if (diffDays === 1) return "昨天";
  if (diffDays === 2) return "前天";

  const sameYear = d.getFullYear() === today.getFullYear();
  const base = sameYear
    ? `${d.getMonth() + 1}月${d.getDate()}日`
    : `${d.getFullYear()}年${d.getMonth() + 1}月${d.getDate()}日`;
  return `${base} ${WEEKDAYS[d.getDay()]}`;
}
