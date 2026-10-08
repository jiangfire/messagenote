/**
 * 极简 CDP 客户端 —— 用 Node 内置的 WebSocket 驱动 headless Edge。
 *
 * 存在的理由：网页端从没在真实浏览器里跑过。构建通过、类型检查通过、
 * 数据层用 Node 直接调过 —— 但那些都证明不了"React 挂载了、事件接上了、
 * 页面真的能用"。这个能。
 */
import { setTimeout as sleep } from "node:timers/promises";

export { sleep };

const CDP = "http://127.0.0.1:9222";

export async function waitForDevTools(timeoutMs = 30000) {
  const t0 = Date.now();
  while (Date.now() - t0 < timeoutMs) {
    try {
      const r = await fetch(`${CDP}/json/version`);
      if (r.ok) return r.json();
    } catch {
      /* 还没起来 */
    }
    await sleep(200);
  }
  throw new Error("DevTools 端口一直没起来 —— Edge 启动失败？");
}

export async function newTarget(url) {
  const r = await fetch(`${CDP}/json/new?${encodeURIComponent(url)}`, { method: "PUT" });
  if (!r.ok) throw new Error(`开新标签失败：HTTP ${r.status}`);
  return r.json();
}

class Session {
  constructor(ws) {
    this.ws = ws;
    this.seq = 0;
    this.pending = new Map();
    this.consoleErrors = [];
    /** 页面弹过的 confirm/prompt/alert 文本。测试可以据此断言"对话框说的是不是真话" */
    this.dialogs = [];

    /** CDP 事件订阅（见 `on`）。 */
    this.listeners = new Map();

    ws.addEventListener("message", (e) => {
      const msg = JSON.parse(e.data);
      if (msg.id && this.pending.has(msg.id)) {
        const { resolve, reject } = this.pending.get(msg.id);
        this.pending.delete(msg.id);
        if (msg.error) reject(new Error(`CDP 报错：${JSON.stringify(msg.error)}`));
        else resolve(msg.result);
        return;
      }
      // **先给订阅者**（如 Network.* 的监听），再走下面那几类内置收集 ——
      // 顺序反了的话，"刷新时到底有没有网络请求"这类诊断就永远拿不到数据。
      const subs = this.listeners.get(msg.method);
      if (subs) {
        for (const fn of [...subs]) {
          try {
            fn(msg.params);
          } catch {
            // 监听器自己的异常不该让整轮测试崩掉
          }
        }
      }
      // 页面里的报错必须收集起来 —— "看起来渲染了"不等于"没出错"
      if (msg.method === "Runtime.exceptionThrown") {
        this.consoleErrors.push(
          msg.params?.exceptionDetails?.exception?.description ??
            JSON.stringify(msg.params?.exceptionDetails)
        );
      }
      if (msg.method === "Runtime.consoleAPICalled" && msg.params?.type === "error") {
        this.consoleErrors.push(
          (msg.params.args ?? []).map((a) => a.value ?? a.description).join(" ")
        );
      }
      // headless 下 confirm() 默认**自动返回 false**，不接管的话
      // 所有"确认删除"的分支都不会执行，测试会以"什么都没发生"收场。
      if (msg.method === "Page.javascriptDialogOpening") {
        this.dialogs.push(msg.params?.message ?? "");
        this.send("Page.handleJavaScriptDialog", { accept: true }).catch(() => {});
      }
    });
  }

  send(method, params = {}) {
    const id = ++this.seq;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.ws.send(JSON.stringify({ id, method, params }));
    });
  }

  /**
   * 订阅某个 CDP 事件。返回取消订阅的函数。
   *
   * 什么时候需要它：**断言失败但看不出原因**的时候。比如「离线刷新拿不到壳」
   * 这类问题，从界面上看只有一句 ERR_INTERNET_DISCONNECTED ——
   * 到底是 SW 没接管导航、还是接管了但缓存是空的、还是导航根本没发出去，
   * 只有把 Network.* 的原始事件打出来才分得清。
   *
   * 猜是猜不出来的，这类问题已经猜错过好几轮了。
   */
  on(method, fn) {
    if (!this.listeners.has(method)) this.listeners.set(method, new Set());
    this.listeners.get(method).add(fn);
    return () => this.off(method, fn);
  }

  off(method, fn) {
    this.listeners.get(method)?.delete(fn);
  }
}

export async function openSession(wsUrl) {
  const ws = new WebSocket(wsUrl);
  await new Promise((res, rej) => {
    ws.addEventListener("open", res, { once: true });
    ws.addEventListener("error", () => rej(new Error("连不上 CDP WebSocket")), { once: true });
  });
  const s = new Session(ws);
  await s.send("Page.enable");
  await s.send("Runtime.enable");
  return s;
}

/** 在页面里求值，返回可序列化的结果。页面抛异常会变成这里的异常。 */
export async function evaluate(s, expression) {
  const r = await s.send("Runtime.evaluate", {
    expression,
    awaitPromise: true,
    returnByValue: true,
  });
  if (r.exceptionDetails) {
    throw new Error(
      `页面里抛异常：${r.exceptionDetails.exception?.description ?? JSON.stringify(r.exceptionDetails)}`
    );
  }
  return r.result.value;
}

export async function waitFor(s, expression, label, timeoutMs = 20000) {
  const t0 = Date.now();
  let last;
  while (Date.now() - t0 < timeoutMs) {
    try {
      last = await evaluate(s, expression);
      if (last) return last;
    } catch (e) {
      last = e.message;
    }
    await sleep(150);
  }
  const msg = `等不到「${label}」（最后的值：${JSON.stringify(last)}）`;
  // **记进全局失败数再抛**，而不是只抛。
  //
  // 抛异常会中止整轮，于是后面几十条断言**根本没跑** ——
  // 而汇总那个"0 条失败"看起来是绿的。实际发生过的：一条 waitFor 超时导致
  // 后面 20 条断言被跳过，报告却显示全过。抛之前先把失败记上，
  // 这样至少退出码和失败数是真的。
  if (typeof globalThis.__e2eFailed === "number") globalThis.__e2eFailed++;
  throw new Error(msg);
}

/**
 * 往 React 受控输入框里填值。
 *
 * 直接 `el.value = x` **不行** —— React 在 value 上装了 setter 拦截，
 * 必须用原型上的原生 setter 写，再手动派发 input 事件，否则 onChange 不触发，
 * 界面看起来填上了、状态其实是空的。
 */
export async function fill(s, selector, value) {
  await evaluate(
    s,
    `(() => {
       const el = document.querySelector(${JSON.stringify(selector)});
       if (!el) throw new Error("找不到元素：" + ${JSON.stringify(selector)});
       const proto = el.tagName === "TEXTAREA"
         ? HTMLTextAreaElement.prototype
         : HTMLInputElement.prototype;
       Object.getOwnPropertyDescriptor(proto, "value").set.call(el, ${JSON.stringify(value)});
       el.dispatchEvent(new Event("input", { bubbles: true }));
       return true;
     })()`
  );
}

export async function click(s, selector) {
  await evaluate(
    s,
    `(() => {
       const el = document.querySelector(${JSON.stringify(selector)});
       if (!el) throw new Error("找不到元素：" + ${JSON.stringify(selector)});
       el.click();
       return true;
     })()`
  );
}

/**
 * 真的"点"一下：mousedown → mouseup → click 依次派发。
 *
 * `click()` 只派发最后那一个 click，测不出监听在 mousedown 上的行为 ——
 * 而"点菜单外面就收起"正是这么实现的（mousedown 而不是 click，
 * 因为 click 在某些情况下会被元素内部的处理吞掉）。
 */
export async function realClick(s, selector) {
  await evaluate(
    s,
    `(() => {
       const el = document.querySelector(${JSON.stringify(selector)});
       // 错误消息里也必须 stringify：selector 带引号（属性选择器都会）时，
       // 裸内插会把这行变成非法 JS —— 报出来的是 SyntaxError，
       // 把真正想说的「找不到元素」吞掉了。
       if (!el) throw new Error("找不到元素：" + ${JSON.stringify(selector)});
       for (const type of ["mousedown", "mouseup", "click"]) {
         el.dispatchEvent(new MouseEvent(type, { bubbles: true, cancelable: true }));
       }
       return true;
     })()`
  );
}

/** 把 React 的受控输入当作"用户敲进去"来用：填值 + 派发 Enter。 */
export async function pressEnter(s, selector) {
  await evaluate(
    s,
    `(() => {
       const el = document.querySelector(${JSON.stringify(selector)});
       if (!el) throw new Error("找不到元素：" + ${JSON.stringify(selector)});
       el.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
       return true;
     })()`
  );
}

/**
 * 选 <select> 的一个选项。
 *
 * 同样要走原生 setter + change 事件：React 在 value 上装了拦截，
 * 直接赋值不会触发 onChange，界面看起来变了、状态其实没动。
 */
export async function selectOption(s, selector, value) {
  await evaluate(
    s,
    `(() => {
       const el = document.querySelector(${JSON.stringify(selector)});
       if (!el) throw new Error("找不到元素：" + ${JSON.stringify(selector)});
       Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, "value").set.call(
         el, ${JSON.stringify(value)}
       );
       el.dispatchEvent(new Event("change", { bubbles: true }));
       return true;
     })()`
  );
}

export async function screenshot(s, path) {
  const r = await s.send("Page.captureScreenshot", { format: "png" });
  const { writeFile, mkdir } = await import("node:fs/promises");
  const { dirname } = await import("node:path");
  // writeFile 不会自动建父目录 —— 不建的话第一次跑就 ENOENT
  await mkdir(dirname(path), { recursive: true });
  await writeFile(path, Buffer.from(r.data, "base64"));
  return path;
}

/** 页面可见文本，用来断言"用户看得到什么"。 */
export function visibleText(s) {
  return evaluate(s, "document.body.innerText");
}
