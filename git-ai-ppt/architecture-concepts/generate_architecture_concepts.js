const sharp = require("sharp");
const fs = require("fs");
const path = require("path");

const OUT = __dirname;
const W = 1600;
const H = 900;
const C = {
  navy: "#213261",
  blue: "#0070C0",
  blue2: "#4F9DD8",
  pale: "#EAF4FB",
  pale2: "#F5F9FC",
  line: "#AFC4D6",
  ink: "#1F2937",
  muted: "#5B6775",
  orange: "#D97706",
  orangeBg: "#FFF7E8",
  green: "#2E8B57",
  greenBg: "#EDF8F1",
  red: "#C73E3A",
  redBg: "#FFF0EF",
  white: "#FFFFFF",
};

function esc(s) {
  return String(s)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
}

function text(x, y, lines, size = 24, color = C.ink, weight = 400, anchor = "start", gap = 1.3) {
  const arr = Array.isArray(lines) ? lines : [lines];
  return `<text x="${x}" y="${y}" text-anchor="${anchor}" font-family="Microsoft YaHei, PingFang SC, Arial" font-size="${size}" font-weight="${weight}" fill="${color}">${
    arr.map((line, i) => `<tspan x="${x}" dy="${i === 0 ? 0 : size * gap}">${esc(line)}</tspan>`).join("")
  }</text>`;
}

function rect(x, y, w, h, fill = C.white, stroke = C.line, r = 18, sw = 2) {
  return `<rect x="${x}" y="${y}" width="${w}" height="${h}" rx="${r}" fill="${fill}" stroke="${stroke}" stroke-width="${sw}"/>`;
}

function line(x1, y1, x2, y2, color = C.blue, sw = 4, dash = "") {
  return `<line x1="${x1}" y1="${y1}" x2="${x2}" y2="${y2}" stroke="${color}" stroke-width="${sw}" ${dash ? `stroke-dasharray="${dash}"` : ""} marker-end="url(#arrow)"/>`;
}

function pill(x, y, w, label, fill = C.navy, color = C.white) {
  return `${rect(x, y, w, 42, fill, fill, 21, 0)}${text(x + w / 2, y + 29, label, 20, color, 700, "middle")}`;
}

function card(x, y, w, h, title, body, options = {}) {
  const fill = options.fill || C.white;
  const stroke = options.stroke || C.line;
  const titleColor = options.titleColor || C.navy;
  const bodyColor = options.bodyColor || C.muted;
  return [
    rect(x, y, w, h, fill, stroke, options.r || 16, options.sw || 2),
    text(x + 18, y + 34, title, options.titleSize || 22, titleColor, 700),
    text(x + 18, y + 67, body, options.bodySize || 17, bodyColor, 400, "start", 1.25),
  ].join("");
}

function base(title, subtitle, variant) {
  return `<svg xmlns="http://www.w3.org/2000/svg" width="${W}" height="${H}" viewBox="0 0 ${W} ${H}">
    <defs>
      <marker id="arrow" markerWidth="10" markerHeight="10" refX="8" refY="3" orient="auto" markerUnits="strokeWidth">
        <path d="M0,0 L0,6 L9,3 z" fill="${C.blue}"/>
      </marker>
      <filter id="shadow" x="-10%" y="-10%" width="120%" height="130%">
        <feDropShadow dx="0" dy="3" stdDeviation="4" flood-color="#7A90A4" flood-opacity="0.18"/>
      </filter>
    </defs>
    <rect width="${W}" height="${H}" fill="${C.white}"/>
    <rect x="0" y="0" width="${W}" height="16" fill="${C.blue}"/>
    ${text(60, 76, title, 34, C.navy, 700)}
    ${text(60, 116, subtitle, 18, C.muted, 400)}
    ${pill(1400, 44, 130, variant, C.pale, C.blue)}
    <line x1="60" y1="140" x2="1540" y2="140" stroke="${C.line}" stroke-width="2"/>
  `;
}

function finish() {
  return `</svg>`;
}

function concept1() {
  let s = base(
    "双链路采集、统一后端确认：从编辑事实到正式统计",
    "两条客户端链路保持各自证据模型；进入后端后统一完成候选归因、目标分支确认与统计物化。",
    "方案 A"
  );
  s += pill(60, 176, 150, "Kilo v5 链路", C.blue, C.white);
  s += pill(60, 414, 150, "Git AI 链路", C.navy, C.white);

  const xs = [245, 430, 615, 800];
  const w = 155;
  const h = 132;
  const kilo = [
    ["编辑事实", ["生成 / 接受", "候选行"]],
    ["本地缓存", ["持久队列", "失败保留"]],
    ["Commit 报告", ["新增行快照", "候选事实"]],
    ["独立接入", ["ai-code-stats", "Token 单独上报"]],
  ];
  const git = [
    ["来源捕获", ["Hook / 插件", "工具调用"]],
    ["checkpoint", ["AI / Human", "/ Unknown"]],
    ["Git Notes", ["commit", "/ rewrite"]],
    ["独立接入", ["metrics/upload", "日累计快照"]],
  ];
  kilo.forEach((d, i) => {
    s += card(xs[i], 176, w, h, d[0], d[1], { fill: C.pale2, stroke: C.blue2, bodySize: 16 });
    if (i < kilo.length - 1) s += line(xs[i] + w + 6, 242, xs[i + 1] - 8, 242, C.blue, 3);
  });
  git.forEach((d, i) => {
    s += card(xs[i], 414, w, h, d[0], d[1], { fill: "#F4F5FA", stroke: "#8E9BC4", bodySize: 16 });
    if (i < git.length - 1) s += line(xs[i] + w + 6, 480, xs[i + 1] - 8, 480, C.blue, 3);
  });

  s += line(962, 242, 1015, 350, C.blue, 4);
  s += line(962, 480, 1015, 372, C.blue, 4);
  const bx = [1005, 1140, 1275, 1410];
  const backend = [
    ["事实收件", ["幂等 / ACK", "raw 可重放"]],
    ["候选归因", ["exact / partial", "生命周期替换"]],
    ["目标确认", ["目标分支", "逐行占位去重"]],
    ["统计物化", ["正式 committed", "日汇总 / 看板"]],
  ];
  backend.forEach((d, i) => {
    s += card(bx[i], 285, 120, 172, d[0], d[1], {
      fill: i === 3 ? C.greenBg : C.pale,
      stroke: i === 3 ? "#76B68F" : C.blue,
      titleColor: i === 3 ? C.green : C.navy,
      titleSize: 18,
      bodySize: 14,
    });
    if (i < backend.length - 1) s += line(bx[i] + 120, 371, bx[i + 1] - 5, 371, C.blue, 3);
  });

  s += rect(60, 616, 1480, 92, C.orangeBg, "#E7B55E", 14, 2);
  s += text(84, 651, "统计边界", 20, C.orange, 700);
  s += text(210, 650, [
    "同一来源内：事件键幂等、候选替换、逐行 claim 去重；只有目标分支确认且逐行证据通过后才形成权威 committed。",
    "跨来源：Kilo v5 与 Git AI 若认领同一最终行，当前不承诺逐行去重；Token 与代码入库归因分开统计。",
  ], 17, C.ink, 400, "start", 1.45);
  s += rect(60, 742, 1480, 96, C.pale2, C.line, 14, 2);
  s += text(84, 777, "推荐落位", 20, C.navy, 700);
  s += text(210, 777, [
    "适合放在“后台适配与统计闭环”章节开头：先给全景，再分别讲后台链路、生命周期、Token 与审计。",
  ], 18, C.muted, 400);
  return s + finish();
}

function concept2() {
  let s = base(
    "用生命周期状态解释：数据怎样从“观察到”变成“正式计入”",
    "系统架构作为上半区入口，状态迁移作为主阅读路径；更适合强调失败重试、替换和最终确认。",
    "方案 B"
  );
  s += card(65, 175, 250, 132, "Kilo v5 事实适配器", ["编辑增量 / 接受事实", "candidate lines / commit report"], { fill: C.pale2, stroke: C.blue });
  s += card(65, 333, 250, 132, "Git AI 事实适配器", ["Hook / checkpoint / Notes", "commit / rewrite / Token 快照"], { fill: "#F4F5FA", stroke: "#8E9BC4" });
  s += line(322, 241, 390, 321, C.blue, 4);
  s += line(322, 399, 390, 335, C.blue, 4);

  const states = [
    ["已采集", ["来源事实", "已形成"]],
    ["本地待发", ["持久化成功", "水位才推进"]],
    ["服务端已收", ["事件幂等", "失败可重放"]],
    ["候选已投影", ["归因匹配", "rewrite 替换"]],
    ["目标分支已确认", ["最终 Commit", "行级 claim"]],
    ["正式已物化", ["日事实", "明细 / 看板"]],
  ];
  const sx = [395, 585, 775, 965, 1155, 1345];
  states.forEach((d, i) => {
    s += card(sx[i], 255, 165, 142, d[0], d[1], {
      fill: i === 5 ? C.greenBg : C.white,
      stroke: i === 5 ? "#76B68F" : C.blue2,
      titleColor: i === 5 ? C.green : C.navy,
      titleSize: 20,
      bodySize: 16,
    });
    if (i < states.length - 1) s += line(sx[i] + 171, 326, sx[i + 1] - 8, 326, C.blue, 4);
  });

  s += rect(395, 446, 1115, 96, C.redBg, "#E29A96", 14, 2);
  s += text(420, 480, "非直线分支", 20, C.red, 700);
  s += text(555, 479, [
    "上传失败 → 留在本地重试；逐条拒绝 → 保留诊断；amend / rebase / reset → 旧候选失效或恢复；未进入目标分支 → 不计入正式入库。",
  ], 17, C.ink, 400);

  s += rect(65, 585, 1445, 150, C.pale, C.line, 14, 2);
  s += text(90, 622, "贯穿生命周期的四类控制", 21, C.navy, 700);
  const controls = [
    ["身份与维度", "用户 / 组织 / 仓库 / 工具 / 模型"],
    ["可靠传输", "本地落盘 / 批量 / 退避 / 部分成功"],
    ["可审计恢复", "原始事实 / receipt / replay / 状态诊断"],
    ["统计边界", "客户端不产生权威 committed；目标分支才定稿"],
  ];
  controls.forEach((d, i) => {
    const x = 90 + i * 355;
    s += card(x, 647, 325, 68, d[0], [d[1]], { fill: C.white, stroke: C.line, titleSize: 18, bodySize: 14, r: 10 });
  });
  s += text(65, 806, "优点：生命周期与异常分支最清楚。代价：两条链路的内部差异被压缩在入口适配器中。", 18, C.muted, 400);
  return s + finish();
}

function concept3() {
  let s = base(
    "按系统分层解释：双客户端、双入口、统一事实与统一消费",
    "更像“总体架构图”：突出系统边界、责任归属和横向治理能力；生命周期通过层间箭头表达。",
    "方案 C"
  );
  const layers = [
    ["01 采集层", "Kilo v5 插件", "Git AI：Hook / 插件 / CLI", "编辑、来源、Token 与 Commit 事实"],
    ["02 本地事实层", "候选行 / Commit 报告 / 持久队列", "checkpoint / Git Notes / rewrite / 快照队列", "断网保留、重启恢复、成功后删除"],
    ["03 接入规范层", "ai-code-stats / ai-token-usage", "metrics/upload", "鉴权、限流、批量、事件幂等、规范化"],
    ["04 确认账本层", "候选归因与新增行快照", "生命周期替换与来源 lineage", "目标仓库扫描、目标分支确认、行级 claim 去重"],
    ["05 统计消费层", "生成 / 接受 / 候选", "Git AI 记录 / Token 日累计", "正式 committed、日汇总、趋势、明细、Commit 审计"],
  ];
  layers.forEach((d, i) => {
    const y = 168 + i * 122;
    const fill = i === 4 ? C.greenBg : i % 2 === 0 ? C.pale2 : C.white;
    s += rect(62, y, 1290, 96, fill, i === 4 ? "#76B68F" : C.line, 12, 2);
    s += text(82, y + 37, d[0], 21, i === 4 ? C.green : C.navy, 700);
    s += rect(245, y + 15, 305, 66, C.white, C.blue2, 10, 2);
    s += text(267, y + 42, d[1], 17, C.ink, 700);
    s += rect(570, y + 15, 330, 66, C.white, "#8E9BC4", 10, 2);
    s += text(592, y + 42, d[2], 17, C.ink, 700);
    s += rect(920, y + 15, 410, 66, C.white, C.line, 10, 2);
    s += text(942, y + 42, d[3], 16, C.muted, 400);
    if (i < layers.length - 1) s += line(1170, y + 102, 1170, y + 116, C.blue, 3);
  });
  s += rect(1380, 168, 160, 584, C.navy, C.navy, 14, 0);
  s += text(1460, 208, "横向治理", 22, C.white, 700, "middle");
  const gov = ["身份映射", "幂等键", "失败重放", "状态诊断", "留存清理", "权限审计"];
  gov.forEach((g, i) => {
    s += rect(1400, 238 + i * 76, 120, 50, "#34477E", "#6C7CA7", 10, 1);
    s += text(1460, 271 + i * 76, g, 17, C.white, 600, "middle");
  });
  s += rect(62, 790, 1478, 58, C.orangeBg, "#E7B55E", 12, 2);
  s += text(82, 826, "边界：两条代码来源链路不是同一协议；后端统一的是确认与统计口径，不等于已实现跨来源逐行去重。", 18, C.orange, 600);
  return s + finish();
}

function concept4() {
  let s = base(
    "以“目标 Commit”为中心：先确认真实入库，再反推来源并形成统计",
    "更强调指标为什么可信：客户端只提供来源证据，目标仓库事实决定何时、以哪个 Commit 正式计入。",
    "方案 D"
  );
  s += card(65, 190, 310, 145, "Kilo v5 来源证据", ["生成 / 接受 / 候选行", "Commit 报告 / 新增行快照"], { fill: C.pale2, stroke: C.blue });
  s += card(65, 395, 310, 145, "Git AI 来源证据", ["Hook / checkpoint / Git Notes", "commit / rewrite / Token 快照"], { fill: "#F4F5FA", stroke: "#8E9BC4" });
  s += line(384, 262, 560, 356, C.blue, 4);
  s += line(384, 468, 560, 374, C.blue, 4);

  s += rect(555, 245, 370, 260, C.pale, C.blue, 24, 3);
  s += text(740, 295, "规范事实账本", 28, C.navy, 700, "middle");
  s += text(590, 340, ["事件幂等与规范化", "候选归因与生命周期替换", "来源 lineage 与行级 claim", "可重放、可诊断、可重建"], 19, C.ink, 400, "start", 1.55);

  s += line(932, 375, 1050, 375, C.blue, 5);
  s += rect(1055, 245, 255, 260, C.orangeBg, "#E7B55E", 24, 3);
  s += text(1182, 296, "目标 Commit", 28, C.orange, 700, "middle");
  s += text(1182, 349, ["仓库扫描", "目标分支可达", "最终新增行", "同批来源确认"], 19, C.ink, 500, "middle", 1.55);
  s += pill(1102, 447, 160, "权威入库锚点", C.orange, C.white);

  s += line(1318, 375, 1380, 375, C.blue, 5);
  s += card(1385, 245, 150, 260, "统计输出", ["AI 贡献率", "入库率", "接受率", "趋势 / 明细", "Commit 审计"], { fill: C.greenBg, stroke: "#76B68F", titleColor: C.green, titleSize: 22, bodySize: 17 });

  s += rect(555, 560, 980, 120, C.white, C.line, 14, 2);
  s += text(580, 596, "反推关系", 20, C.navy, 700);
  s += text(700, 596, [
    "已确认最终行 → 回溯同一批 accepted / generated 来源；本地 commit 只是候选，不能直接当作最终入库。",
    "amend / rebase / reset 只影响未确认候选；已确认事实以后端目标分支真实历史为准。",
  ], 17, C.ink, 400, "start", 1.45);

  s += rect(65, 720, 1470, 90, C.redBg, "#E29A96", 14, 2);
  s += text(90, 754, "适用性判断", 20, C.red, 700);
  s += text(220, 753, [
    "优点：最能解释“为什么统计可信”。代价：系统组件拓扑不如方案 C 完整；适合作为技术结论页，不一定适合作为唯一总体架构页。",
  ], 17, C.ink, 400);
  return s + finish();
}

async function render(name, svg) {
  fs.writeFileSync(path.join(OUT, `${name}.svg`), svg);
  await sharp(Buffer.from(svg)).png().toFile(path.join(OUT, `${name}.png`));
}

async function main() {
  const items = [
    ["01-dual-swimlane", concept1()],
    ["02-lifecycle-state", concept2()],
    ["03-layered-architecture", concept3()],
    ["04-target-commit-hub", concept4()],
  ];
  for (const [name, svg] of items) await render(name, svg);

  const thumbs = await Promise.all(
    items.map(async ([name]) => ({
      input: await sharp(path.join(OUT, `${name}.png`)).resize(760, 428).toBuffer(),
      name,
    }))
  );
  const canvas = sharp({
    create: { width: 1600, height: 980, channels: 4, background: "#EEF3F7" },
  });
  const composites = thumbs.map((t, i) => ({
    input: t.input,
    left: i % 2 === 0 ? 25 : 815,
    top: i < 2 ? 45 : 525,
  }));
  await canvas.composite(composites).png().toFile(path.join(OUT, "00-contact-sheet.png"));
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
