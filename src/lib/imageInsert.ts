import type { DragEvent, RefObject } from "react";
import type { NoteApi } from "./apiContext";
import { attachmentFileMarkdown, attachmentMarkdown } from "./attachmentRef";

/**
 * 输入框里的附件粘贴与拖拽。
 *
 * **附件不限于图片**：图片内联成 `![图片](attachment:sha)`，其它文件
 * 成 `[名字](attachment:sha)` 链接。后端本来就存得下任意字节
 * （`messagenote_core::attachment::sniff_mime` 会给未知类型兜底成
 * `application/octet-stream`），前端原先按 `image/` 过滤只是没走到那一步。
 *
 * ## 为什么单独一个文件
 *
 * 产品里有两个输入框要支持它：主时间线的 `Composer`，和桌面端的捕获浮层。
 * 两者的区别只是"发送之后去哪儿"，插入附件这件事一模一样。复制一份的话，
 * 光标处理这种细节（下面全是坑）就会在两次改动里慢慢分叉。
 *
 * ## 为什么不是"追加到末尾"
 *
 * 用户在写到一半时粘贴图片，图就该落在**光标那里**。追加到末尾看着实现了
 * 功能，实际用起来是：你先写了两句、想起要配张图、粘上去 —— 图跑到结尾，
 * 还得自己回去把段落顺序调对。这个产品全部的价值就在"记的时候不用想排版"，
 * 这种细节不能糊过去。
 */

/**
 * 文本插入后怎么停笔。
 *
 * `value` 是新的全文，`caret` 是插入内容**之后**的位置 —— 一次粘三张图时，
 * 第二张要接在第一张后面，所以必须把光标算下去，不能每次都按初始位置插。
 */
export interface TextEdit {
  value: string;
  caret: number;
}

/**
 * 往光标处插入文本。
 *
 * `selectionStart`/`End` 在"还没聚焦过"的 textarea 上是 null（不是 0），
 * 所以统一退回末尾 —— 否则会出现"insertAtCursor 拼出 undefined"这种
 * 只在不聚焦时复现的坏值。
 */
export function insertAtCursor(value: string, start: number, end: number, text: string): TextEdit {
  const before = value.slice(0, start);
  const after = value.slice(end);

  // 前置一个换行：图片是块级元素，紧贴在上一行文字尾巴上会变成一行里
  // 夹一张大图，看着像是排版坏了。
  //
  // 判断依据是"光标前面这一行有没有内容"，而不是"前面有没有字符"：
  // 用户在空行上敲了两个空格再粘图是常事，那种情况再补一个换行，
  // 结果就是正文里凭空多出一个空行 —— 而这个应用的全部价值就在于
  // 记的时候不用管排版，它不该自己往里加空格。
  const lineStart = before.lastIndexOf("\n") + 1;
  const lineHasText = /\S/.test(before.slice(lineStart));
  const insert = (lineHasText ? "\n" : "") + text;

  return { value: before + insert + after, caret: before.length + insert.length };
}

/** 把 File 读成 `saveAttachment` 要的形态。File 本身就是 Blob，不用再包。 */
async function fileBytes(file: File): Promise<Uint8Array<ArrayBuffer>> {
  return new Uint8Array(await file.arrayBuffer());
}

/**
 * 判断是不是图片。
 *
 * 不看扩展名：粘贴板和拖拽给的 File 没有可信的扩展名，
 * 而剪贴板里的截图 `type` 一定是 `image/png` 这类 MIME。
 */
function isImage(file: File): boolean {
  return file.type.startsWith("image/");
}

/**
 * 这份 File 到底能不能当附件存进去。
 *
 * **空文件要挡掉**，而判据不是 `size === 0` 一刀切 —— 拖进来的**文件夹**
 * 在 DataTransfer 里就是一个 size 为 0、type 为空的项，它显然不是附件。
 * 零字节的 txt 却是合法附件（用户就是想记一个空行）。
 *
 * 所以按"有没有文件名"来分：拖文件夹时浏览器给的 `name` 是空的，
 * 真实文件一定有名字。这个判据和类型无关，因此对图片和非图片一样成立。
 */
function isUsableFile(file: File): boolean {
  return file.name !== "" && file.size > 0;
}

/**
 * 存下这些附件，并把 Markdown 引用插到光标处。
 *
 * 图片插成 `![图片](attachment:sha)`，非图片插成 `[名字](attachment:sha)`
 * —— 后者能点开存下来，前者渲染成破图。
 *
 * 多份按顺序插入（一次拖进来三个，顺序就该和用户选的一样），
 * 单份失败不影响其余的 —— 失败的那些攒起来在最后一起报，由调用方显示。
 *
 * 素材（正文、光标位置）在**进入函数时取一次快照**：这里面的 await 会让出
 * 事件循环，用户完全可能在"文件正在存"的时候继续打字，而边打字边往
 * `textarea.value` 上追加会把刚敲的字吃掉。
 */
export async function insertAttachments(
  api: NoteApi,
  textarea: HTMLTextAreaElement,
  files: File[],
  setValue: (v: string) => void
): Promise<void> {
  const usable = files.filter(isUsableFile);
  if (usable.length === 0) return;

  let value = textarea.value;
  let caret = textarea.selectionStart ?? value.length;
  let caretEnd = textarea.selectionEnd ?? caret;
  const failures: string[] = [];

  for (const file of usable) {
    try {
      const sha = await api.saveAttachment(await fileBytes(file));
      // 图片内联、非图片成链接：同一个 sha，语法按能不能渲染来选。
      const snippet = isImage(file)
        ? attachmentMarkdown(sha)
        : attachmentFileMarkdown(sha, file.name);
      // 每插一份都重新算光标：上一份插进正文之后，光标已经不在原位了。
      const edit = insertAtCursor(value, caret, caretEnd, snippet);
      value = edit.value;
      caret = edit.caret;
      caretEnd = edit.caret;
      setValue(value);

      // 受控 textarea 的 value 要等 React 重渲染才落到 DOM 上，
      // 但**选区是 DOM 属性**，现在设下去不会被 React 覆盖掉 ——
      // 所以不用 requestAnimationFrame 绕一圈，那样反而会在
      // 快速连粘时和用户的手动点击抢光标。
      textarea.setSelectionRange(caret, caret);
    } catch (e) {
      failures.push(`${file.name || "附件"}：${e instanceof Error ? e.message : String(e)}`);
    }
  }

  // 让输入框重新可见：粘贴/拖拽常常发生在输入框没聚焦的时候（比如从资源管理器拖进来）
  textarea.focus();

  if (failures.length > 0) throw new Error(failures.join("；"));
}

/**
 * 从剪贴板里挑出**文件**（图片和其它附件都要）。
 *
 * 非文件的项原样留给浏览器处理 —— 纯文本粘贴是这个输入框的主用途，
 * 不能因为我们想支持附件就把它也拦下来。
 */
export function filesFromClipboard(items: DataTransferItemList | null): File[] {
  if (!items) return [];
  const out: File[] = [];
  for (const item of items) {
    if (item.kind !== "file") continue;
    const file = item.getAsFile();
    if (file && isUsableFile(file)) out.push(file);
  }
  return out;
}

/**
 * 从拖拽里挑出文件。图片和其它附件都要，但文件夹要挡掉 ——
 * 见 [`isUsableFile`]，那里说明了为什么不能只看 size。
 */
export function filesFromDrop(dt: DataTransfer | null): File[] {
  if (!dt) return [];
  return Array.from(dt.files).filter(isUsableFile);
}

export interface ImageDropHandlers {
  onDragOver: (e: DragEvent<HTMLElement>) => void;
  onDragLeave: (e: DragEvent<HTMLElement>) => void;
  onDrop: (e: DragEvent<HTMLElement>) => void;
}

/**
 * 给容器装上"拖进来放图"。
 *
 * `setActive` 由调用方决定怎么表达（加类名之类）—— 拖拽期间的高亮状态属于
 * 样式问题，不该在这个文件里长出第二个 state 管理方案。
 *
 * 关键的一条：**`onDragOver` 必须 `preventDefault()`**。浏览器对可拖放的
 * 内容默认行为是"导航过去打开它"，不挡住的话用户一松手，整个应用就被
 * 那张图替换掉了 —— 而且是在 WebView 里，等于把应用顶掉，看着像崩溃。
 * 所以这里连"是不是图片"都先不判断，一律先挡住。
 */
export function makeImageDropHandlers(
  api: NoteApi,
  textarea: RefObject<HTMLTextAreaElement | null>,
  setValue: (v: string) => void,
  setActive: (active: boolean) => void,
  onError: (e: unknown) => void
): ImageDropHandlers {
  return {
    onDragOver: (e) => {
      e.preventDefault();
      // 声明"我是拷贝"而不是默认的"移动"：否则从某些文件管理器拖进来时，
      // 松手后源文件会被移走（虽然实际不会，但光标图标会那么显示）。
      // `dataTransfer` 在合成的拖拽事件里可能是 null，顺手挡一下 ——
      // 它要是抛了，后面的 `setActive(true)` 就执行不到，高亮就没了。
      if (e.dataTransfer) e.dataTransfer.dropEffect = "copy";
      setActive(true);
    },

    onDragLeave: (e) => {
      // 拖过子元素时也会触发 dragleave，直接用 relatedTarget 判断是否
      // 真的离开了这个容器，否则高亮会一闪一闪。
      const next = e.relatedTarget as Node | null;
      if (next && e.currentTarget.contains(next)) return;
      setActive(false);
    },

    onDrop: (e) => {
      e.preventDefault();
      setActive(false);
      const el = textarea.current;
      if (!el) return;
      const files = filesFromDrop(e.dataTransfer);
      if (files.length === 0) return;
      // 落点当作光标位置：拖拽本身没有"光标在文字中间"的概念，
      // 直接把焦点交回输入框，插入位置就是它当前的选区。
      el.focus();
      void insertAttachments(api, el, files, setValue).catch(onError);
    },
  };
}
