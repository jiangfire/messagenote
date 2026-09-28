/**
 * 离线捕获队列。
 *
 * ## 为什么只有网页端需要它
 *
 * 桌面端本来就有一份本地库，断网时照写不误，联网后由同步引擎推上去。
 * 浏览器里没有本地库 —— 所以"地铁里想记一笔"这件事在网页端原本是**做不到**的，
 * 而那恰恰是这个产品最核心的场景。
 *
 * 这里刻意**不**去浏览器里重建整个本地优先（sqlite-wasm + OPFS）：
 * 浏览和检索可以要求联网，真正必须离线的只有**捕获**。一个小队列就够。
 *
 * ## 重放为什么是安全的
 *
 * 每条排队项都带一个**客户端生成的 id**，服务端那边是幂等的
 * （见 `CreateMessageRequest::id`）。所以重放多少次都只落一条 ——
 * 这件事必须成立，因为这里的重放天然会重试：网络抖一下、页面关掉再打开、
 * 用户手动点一下，都会再跑一遍。
 *
 * 没有那个幂等键的话，这个模块**整体是不成立的**：
 * 一次重试就是一条重复记录。
 */

const DB_NAME = "messagenote";
const DB_VERSION = 1;
const STORE = "outbox";

export interface PendingCapture {
  /** 幂等键。重放时原样带上，服务端据此去重。 */
  id: string;
  body: string;
  /** 入队时刻，用来按顺序重放 —— 顺序反了，时间线上看起来就是乱的。 */
  createdAt: number;
}

function openDb(): Promise<IDBDatabase> {
  return new Promise((resolve, reject) => {
    const req = indexedDB.open(DB_NAME, DB_VERSION);
    req.onupgradeneeded = () => {
      const db = req.result;
      if (!db.objectStoreNames.contains(STORE)) {
        db.createObjectStore(STORE, { keyPath: "id" });
      }
    };
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}

/** 把一次 DB 操作包成 Promise，并保证连接用完就关。 */
async function withStore<T>(
  mode: IDBTransactionMode,
  run: (store: IDBObjectStore) => IDBRequest
): Promise<T> {
  const db = await openDb();
  try {
    return await new Promise<T>((resolve, reject) => {
      const tx = db.transaction(STORE, mode);
      const req = run(tx.objectStore(STORE));
      req.onsuccess = () => resolve(req.result as T);
      req.onerror = () => reject(req.error);
    });
  } finally {
    // **必须关。** 不关的话版本升级会被自己挡住，而且长开着的连接
    // 在标签页之间会互相干扰。
    db.close();
  }
}

export function enqueue(item: PendingCapture): Promise<void> {
  return withStore<void>("readwrite", (s) => s.put(item)).then(() => undefined);
}

/** 按入队顺序取出全部待发送项。 */
export async function pending(): Promise<PendingCapture[]> {
  const all = await withStore<PendingCapture[]>("readonly", (s) => s.getAll());
  // getAll 按主键（id，也就是随机 UUID）排序 —— 那是**乱序**。
  // 必须自己按入队时间排，否则重放之后时间线是错乱的。
  return all.sort((a, b) => a.createdAt - b.createdAt);
}

export function drop(id: string): Promise<void> {
  return withStore<void>("readwrite", (s) => s.delete(id)).then(() => undefined);
}

export async function count(): Promise<number> {
  return withStore<number>("readonly", (s) => s.count());
}

/** 重放的结果，用来告诉界面"刚才补发了几条"。 */
export interface ReplayResult {
  sent: number;
  /** 还压在队列里的条数（包括这次没发成功的那条和它后面的）。 */
  remaining: number;
}

/**
 * 把队列里的记录按顺序补发出去。
 *
 * `send` 是真正发一条的函数（带上**入队时那个 id**）。
 *
 * **第一条失败就停。** 不跳过它去发后面的：队列是按时间排的，跳过会让
 * 时间线错乱；而且失败通常意味着"还没恢复联网"，后面那些也一样发不出去，
 * 白试一遍。
 */
export async function replay(
  send: (item: PendingCapture) => Promise<void>
): Promise<ReplayResult> {
  const items = await pending();
  let sent = 0;

  for (const item of items) {
    try {
      await send(item);
    } catch {
      // 还没恢复联网，或者服务端出错了。留在队列里，下次再来。
      break;
    }
    // **发出去了才删。** 反过来的话，进程在中间被杀掉就是一条永久丢失的记录 ——
    // 用户以为记下了，而它哪儿都不在。
    await drop(item.id);
    sent += 1;
  }

  return { sent, remaining: await count() };
}
