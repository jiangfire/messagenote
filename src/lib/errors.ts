/**
 * 把抛出来的东西整理成能直接显示的一句话。
 *
 * 刻意放在这里而不是 `api.ts`：网页端的 bundle 不该因为一个错误格式化函数
 * 就把 `@tauri-apps/api` 整个拉进来 —— 那在浏览器里是一堆用不上的代码，
 * 而且真的调用起来会直接抛异常。
 */
export function errorText(e: unknown): string {
  if (typeof e === "string") return e;
  if (e instanceof Error) return e.message;
  return String(e);
}
