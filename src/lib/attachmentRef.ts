/**
 * 附件引用的**文本格式**。
 *
 * 单独一个文件，是因为这个格式要被两处认出来，而且必须认得**一模一样**：
 * Markdown 渲染时要把 `attachment:<sha>` 换成真实字节，输入框插入时又要
 * 按同样的规则生成它。两边各写一份正则，早晚会在某次改动里悄悄错开 ——
 * 那时表现是"新粘贴的图不显示，老图还在"，很难联想到是格式不一致。
 *
 * 附件按内容寻址（sha256），正文里只存这个 hash，不存路径也不存 URL：
 * 换台机器、换个服务端，同一份内容仍然是同一个名字。
 */

/** 正文里引用的前缀。和 Rust 侧 `messagenote_core::attachment` 约定一致。 */
export const ATTACHMENT_SCHEME = "attachment:";

/** 只认小写 hex，长度锁死 64 —— 别让半截 hash 或大写混进来。 */
const SHA256_RE = /^[0-9a-f]{64}$/;

/**
 * 生成一段可以直接写进正文的图片 Markdown。
 *
 * alt 写死成「图片」而不是原文件名：正文里那串 hash 对人没有意义，
 * 而占位文案（图片取不到时显示的字）用它更合适 —— 文件名通常是一串
 * 截图工具给的随机字符，反而像是出错信息。
 */
export function attachmentMarkdown(sha256: string): string {
  return `![图片](${ATTACHMENT_SCHEME}${sha256})`;
}

/**
 * 生成一段非图片附件的 Markdown 链接。
 *
 * 为什么是**链接**而不是图片语法：把一个 PDF 写成 `![report.pdf](attachment:…)`
 * 的话，渲染出来是一个永远显示不出来的破图（浏览器解不了这个 MIME），
 * 占位文案还写着"图片还没下载下来"。写成链接，用户点得到、能存下来，
 * 显示的也还是他自己的文件名 —— 一眼就知道哪个文件。
 *
 * 文件名要转义 `]` 和换行：正文是 Markdown，一个裸的 `]` 会把链接截断，
 * 而 Windows 上叫"备注]草稿.txt"的文件是真实存在的。
 */
export function attachmentFileMarkdown(sha256: string, name: string): string {
  const label = name.replace(/[[\]\r\n]/g, " ").trim() || "附件";
  return `[${label}](${ATTACHMENT_SCHEME}${sha256})`;
}

/** 从 `src`/`href` 里取出 sha256；不是本应用认得的附件引用就返回 null。 */
export function parseAttachmentSrc(src: string | null): string | null {
  if (!src || !src.startsWith(ATTACHMENT_SCHEME)) return null;
  const sha = src.slice(ATTACHMENT_SCHEME.length);
  return SHA256_RE.test(sha) ? sha : null;
}
