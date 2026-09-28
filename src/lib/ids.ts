/**
 * 生成一个 UUID v4。
 *
 * ## 为什么不用 `crypto.randomUUID()`
 *
 * 它和 `crypto.subtle` 一样**只在安全上下文里存在**。自建服务端常常就是
 * 明文 HTTP 的内网地址，那里 `crypto.randomUUID` 是 `undefined` —— 而这条
 * 路径恰恰是离线队列要用的那条（断网时更不可能有 TLS，何况要是手机连着
 * 一个 http:// 的内网地址）。
 *
 * `crypto.getRandomValues` **没有**这个限制，它在非安全上下文里也能用，
 * 所以这里基于它自己拼一个 v4。
 *
 * 刻意不提供 `Math.random` 的兜底：那会静静地生成一批熵不足的 id，
 * 而 id 撞车的表现是"两条记录互相覆盖"，没人会想到去查随机数质量。
 * `crypto` 在现代浏览器里都在，真没有的话让它在调用点响亮地炸掉更好。
 */
export function uuidV4(): string {
  const b = new Uint8Array(16);
  crypto.getRandomValues(b);

  // 版本位（第 7 字节高 4 位 = 4）和变体位（第 9 字节高 2 位 = 10）。
  // 这两处不是装饰：少了它们，生成的东西就不是 v4，任何按版本解析的地方
  // 都会得出错误的结论。
  b[6] = (b[6] & 0x0f) | 0x40;
  b[8] = (b[8] & 0x3f) | 0x80;

  const hex = Array.from(b, (x) => x.toString(16).padStart(2, "0"));
  return [
    hex.slice(0, 4).join(""),
    hex.slice(4, 6).join(""),
    hex.slice(6, 8).join(""),
    hex.slice(8, 10).join(""),
    hex.slice(10, 16).join(""),
  ].join("-");
}
