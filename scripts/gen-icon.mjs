// 用纯 Node（zlib + 手写 PNG chunk）生成一个 1024x1024 的应用图标源图。
//
// 为什么不用现成图片库：这个项目里不需要为一个图标引入 sharp / canvas 这类
// 重型依赖，而 PNG 的编码格式足够简单，手写一遍反而没有供应链和编译风险。
// 生成后交给 `pnpm tauri icon` 派生全套尺寸（含 .ico / .icns）。

import zlib from "node:zlib";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));

const SIZE = 1024;
const SS = 3; // 超采样倍率，用来做抗锯齿
const W = SIZE * SS;

// ---------- PNG 编码 ----------

const CRC_TABLE = (() => {
  const t = new Int32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    t[n] = c;
  }
  return t;
})();

function crc32(buf) {
  let c = -1;
  for (let i = 0; i < buf.length; i++) c = CRC_TABLE[(c ^ buf[i]) & 0xff] ^ (c >>> 8);
  return (c ^ -1) >>> 0;
}

function chunk(type, data) {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length, 0);
  const typeBuf = Buffer.from(type, "ascii");
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(Buffer.concat([typeBuf, data])), 0);
  return Buffer.concat([len, typeBuf, data, crc]);
}

function encodePng(width, height, rgba) {
  const raw = Buffer.alloc((width * 4 + 1) * height);
  for (let y = 0; y < height; y++) {
    const rowStart = y * (width * 4 + 1);
    raw[rowStart] = 0; // filter type 0 (None)
    rgba.copy(raw, rowStart + 1, y * width * 4, (y + 1) * width * 4);
  }
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(width, 0);
  ihdr.writeUInt32BE(height, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 6; // color type RGBA
  ihdr[10] = 0;
  ihdr[11] = 0;
  ihdr[12] = 0;
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk("IHDR", ihdr),
    chunk("IDAT", zlib.deflateSync(raw, { level: 9 })),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

// ---------- 绘图 ----------

// 圆角矩形的有符号距离场
function sdRoundRect(px, py, cx, cy, hw, hh, r) {
  const qx = Math.abs(px - cx) - (hw - r);
  const qy = Math.abs(py - cy) - (hh - r);
  const ax = Math.max(qx, 0);
  const ay = Math.max(qy, 0);
  return Math.hypot(ax, ay) + Math.min(Math.max(qx, qy), 0) - r;
}

function sdCircle(px, py, cx, cy, r) {
  return Math.hypot(px - cx, py - cy) - r;
}

function sdTriangle(px, py, ax, ay, bx, by, cx, cy) {
  const sign = (x1, y1, x2, y2) => (px - x2) * (y1 - y2) - (x1 - x2) * (py - y2);
  const d1 = sign(ax, ay, bx, by);
  const d2 = sign(bx, by, cx, cy);
  const d3 = sign(cx, cy, ax, ay);
  const hasNeg = d1 < 0 || d2 < 0 || d3 < 0;
  const hasPos = d1 > 0 || d2 > 0 || d3 > 0;
  if (!(hasNeg && hasPos)) {
    // 内部或边上：返回负的到最近边距离
    const edge = (x1, y1, x2, y2) => {
      const vx = x2 - x1;
      const vy = y2 - y1;
      const wx = px - x1;
      const wy = py - y1;
      const t = Math.max(0, Math.min(1, (wx * vx + wy * vy) / (vx * vx + vy * vy)));
      return Math.hypot(wx - t * vx, wy - t * vy);
    };
    return -Math.min(edge(ax, ay, bx, by), edge(bx, by, cx, cy), edge(cx, cy, ax, ay));
  }
  const edge = (x1, y1, x2, y2) => {
    const vx = x2 - x1;
    const vy = y2 - y1;
    const wx = px - x1;
    const wy = py - y1;
    const t = Math.max(0, Math.min(1, (wx * vx + wy * vy) / (vx * vx + vy * vy)));
    return Math.hypot(wx - t * vx, wy - t * vy);
  };
  return Math.min(edge(ax, ay, bx, by), edge(bx, by, cx, cy), edge(cx, cy, ax, ay));
}

const S = (v) => v * SIZE * SS; // 归一化坐标 -> 超采样像素坐标

// 图形定义（全部用 0..1 归一化坐标，便于整体缩放）
const bgR = 0.224;
const bubble = { cx: 0.5, cy: 0.445, hw: 0.285, hh: 0.195, r: 0.105 };
const tail = [
  [0.335, 0.60],
  [0.315, 0.795],
  [0.545, 0.625],
];
const dots = [
  [0.365, 0.445, 0.0345],
  [0.5, 0.445, 0.0345],
  [0.635, 0.445, 0.0345],
];

const rgba = Buffer.alloc(W * W * 4);

for (let y = 0; y < W; y++) {
  for (let x = 0; x < W; x++) {
    const u = x / W;
    const v = y / W;
    let r = 0;
    let g = 0;
    let b = 0;
    let a = 0;

    // 背景圆角方块 + 对角渐变
    const dBg = sdRoundRect(u, v, 0.5, 0.5, 0.5, 0.5, bgR);
    if (dBg <= 0) {
      const t = Math.max(0, Math.min(1, (u + v) / 2));
      r = Math.round(109 + (79 - 109) * t);
      g = Math.round(106 + (70 - 106) * t);
      b = Math.round(247 + (229 - 247) * t);
      a = 255;
    }

    // 气泡主体
    const dBubble = sdRoundRect(u, v, bubble.cx, bubble.cy, bubble.hw, bubble.hh, bubble.r);
    const dTail = sdTriangle(u, v, tail[0][0], tail[0][1], tail[1][0], tail[1][1], tail[2][0], tail[2][1]);
    const dShape = Math.min(dBubble, dTail);
    if (dShape <= 0) {
      r = 255;
      g = 255;
      b = 255;
      a = 255;
    }

    // 三个点（用消息气泡内部的镂空点表达“消息”语义）
    let inDot = false;
    for (const [dx, dy, dr] of dots) {
      if (sdCircle(u, v, dx, dy, dr) <= 0) {
        inDot = true;
        break;
      }
    }
    if (inDot) {
      r = 79;
      g = 70;
      b = 229;
      a = 255;
    }

    const i = (y * W + x) * 4;
    rgba[i] = r;
    rgba[i + 1] = g;
    rgba[i + 2] = b;
    rgba[i + 3] = a;
  }
}

// 盒式降采样 -> 抗锯齿
const out = Buffer.alloc(SIZE * SIZE * 4);
const n = SS * SS;
for (let y = 0; y < SIZE; y++) {
  for (let x = 0; x < SIZE; x++) {
    let r = 0;
    let g = 0;
    let b = 0;
    let a = 0;
    for (let sy = 0; sy < SS; sy++) {
      for (let sx = 0; sx < SS; sx++) {
        const i = ((y * SS + sy) * W + (x * SS + sx)) * 4;
        r += rgba[i];
        g += rgba[i + 1];
        b += rgba[i + 2];
        a += rgba[i + 3];
      }
    }
    const o = (y * SIZE + x) * 4;
    out[o] = Math.round(r / n);
    out[o + 1] = Math.round(g / n);
    out[o + 2] = Math.round(b / n);
    out[o + 3] = Math.round(a / n);
  }
}

const outDir = path.join(__dirname, "..", "src-tauri", "icons");
fs.mkdirSync(outDir, { recursive: true });
const outPath = path.join(__dirname, "app-icon.png");
fs.writeFileSync(outPath, encodePng(SIZE, SIZE, out));
console.log(`生成图标源图: ${outPath} (${SIZE}x${SIZE})`);
