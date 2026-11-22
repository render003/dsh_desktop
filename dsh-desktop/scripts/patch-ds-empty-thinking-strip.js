'use strict';

// dsh-llm-deepseek 空思考标签拦截补丁（官方 DeepSeek Messages 路由）。
//
// 现象：deepseek-v4.1-flash 等模型在连续工具调用中，偶发把空的 `<thinking> </thinking>`
// （或 `<think> </think>`）泄入普通 `type === "text"` 正文块，而非结构化 `thinking` 块。
// DSH 原样透传后，不仅打断前端工具调用折叠，更会被 `assistant()` 忠实回放进历史上下文，
// 在长会话里触发强烈的上下文自激复读（每调一次工具就先吐一遍 `<thinking> </thinking>`）。
//
// 治理（双端闭环）：
//   1. 入站流翻译 `translate()`：仅当 `text` 块处于「空思考标签前缀」时暂存，一旦出现
//      真实正文立刻剥离前导空标签并按零延迟透传；若整块闭合后只剩空 `<thinking>/<think>`
//      标签则彻底丢弃（不发 `block-start/text-delta/block-end`），并同步维护
//      `__emittedReplays` 使 `replayState.blocks` 与实际发出块保持严格 1:1 对齐；
//      若整轮 `end_turn` 只产出空思考标签，按 `EMPTY_RESPONSE` 走 Harness 内置自动重试。
//   2. 出站历史回放 `assistant()`：对既有已污染会话里落盘的 `text` 块同样剥离前导空思考
//      标签，剥后为空则同上游空文本约定直接跳过（不影响 `replay[index].signature` 对齐），
//      立刻阻断老会话的复读正反馈。
//
// 保守原则：仅剔除位于块首的纯空 `<thinking>\s*</thinking>` / `<think>\s*</think>`；
// 正常正文、非空 `<thinking>实内容</thinking>`、以及正文中间提及的标签一律原样保留。
//
// 用法：node scripts/patch-ds-empty-thinking-strip.js [<node_modules 根>]

const fs = require('node:fs');
const path = require('node:path');
const { writeFileAtomic } = require('./lib/patch-io');

const TARGET_REL = path.join('@deepseek-ai', 'dsh-llm-deepseek', 'lib', 'index.js');
const MARKER = 'dsh-desktop patch (ds empty thinking strip)';

const ASSISTANT_FN_ANCHOR = 'function assistant(message, model, onReplayDegrade) {';
const REPLAY_TEXT_ANCHOR = [
  '\t\t\tcase "text": return block.text.length === 0 ? [] : {',
  '\t\t\t\ttype: "text",',
  '\t\t\t\ttext: block.text',
  '\t\t\t};',
].join('\n');

const TRANSLATE_HEAD_ANCHOR = [
  'async function* translate(events, model) {',
  '\tconst blocks = /* @__PURE__ */ new Map();',
].join('\n');

const TRANSLATE_BLOCKS_ANCHOR = [
  '\t\tif (event.type === "content_block_start") {',
  '\t\t\tconst wireIndex = indexOf(event);',
  '\t\t\tif (blocks.has(wireIndex) || reason !== void 0) return malformed("block starts after settlement or repeats an index");',
  '\t\t\tconst block = startBlock(event, blocks.size);',
  '\t\t\tblocks.set(wireIndex, block);',
  '\t\t\tyield {',
  '\t\t\t\ttype: "block-start",',
  '\t\t\t\tindex: block.index,',
  '\t\t\t\tblockType: block.content.type',
  '\t\t\t};',
  '\t\t\tif (block.content.type === "text" || block.content.type === "reasoning") {',
  '\t\t\t\tif (block.content.text) yield {',
  '\t\t\t\t\ttype: block.content.type === "text" ? "text-delta" : "reasoning-delta",',
  '\t\t\t\t\tindex: block.index,',
  '\t\t\t\t\ttext: block.content.text',
  '\t\t\t\t};',
  '\t\t\t} else yield {',
  '\t\t\t\ttype: "tool-call-delta",',
  '\t\t\t\tindex: block.index,',
  '\t\t\t\tid: block.content.id,',
  '\t\t\t\tname: block.content.name,',
  '\t\t\t\targumentsDelta: ""',
  '\t\t\t};',
  '\t\t} else if (event.type === "content_block_delta" || event.type === "content_block_stop") {',
  '\t\t\tconst block = blocks.get(indexOf(event));',
  '\t\t\tif (block === void 0 || block.closed) return malformed("delta/stop without an open block");',
  '\t\t\tif (event.type === "content_block_delta") {',
  '\t\t\t\tconst chunk = deltaChunk(block, event.delta);',
  '\t\t\t\tif (chunk !== void 0) yield chunk;',
  '\t\t\t} else {',
  '\t\t\t\tblock.closed = true;',
  '\t\t\t\tif (block.content.type === "tool-call" && block.json.length > 0) block.content.arguments = block.json;',
  '\t\t\t\tyield {',
  '\t\t\t\t\ttype: "block-end",',
  '\t\t\t\t\tindex: block.index,',
  '\t\t\t\t\tblock: { ...block.content }',
  '\t\t\t\t};',
  '\t\t\t}',
  '\t\t}',
].join('\n');

const EMPTY_STOP_ANCHOR = '\t\t\tif (blocks.size === 0 && reason.kind === "stop") throw new LlmError("DeepSeek Messages returned no content", "EMPTY_RESPONSE");';
const REPLAY_STATE_ANCHOR = '\t\t\t\treplayState: replayState(model, [...blocks.values()].map((block) => block.replay))';

const HELPER = [
  '// ' + MARKER + ': 拦截官方 DeepSeek Messages 正文通道泄漏的纯空 <thinking>/<think> 标签，',
  '// 兼顾入站流式过滤与出站历史回放清理，防长会话上下文自激复读并保持 replayState 对齐。',
  'const __DSH_EMPTY_THINK_LEADING_RE = /^\\s*(?:<(?:thinking|think)>\\s*<\\/(?:thinking|think)>\\s*)+/;',
  'function __dshDsStripEmptyThinking(text) {',
  '\tif (typeof text !== "string" || text.length === 0) return text;',
  '\tif (!text.includes("<")) return text;',
  '\treturn text.replace(__DSH_EMPTY_THINK_LEADING_RE, "");',
  '}',
  'function __dshDsIsEmptyThinkingPrefix(text) {',
  '\tif (typeof text !== "string" || text.length === 0) return true;',
  '\tlet rest = text.replace(__DSH_EMPTY_THINK_LEADING_RE, "");',
  '\trest = rest.replace(/^\\s+/, "");',
  '\tif (rest.length === 0) return true;',
  '\tfor (const tag of ["thinking", "think"]) {',
  '\t\tconst open = "<" + tag + ">";',
  '\t\tconst close = "</" + tag + ">";',
  '\t\tif (rest.length < open.length) {',
  '\t\t\tif (open.startsWith(rest)) return true;',
  '\t\t\tcontinue;',
  '\t\t}',
  '\t\tif (!rest.startsWith(open)) continue;',
  '\t\tconst afterOpen = rest.slice(open.length).replace(/^\\s+/, "");',
  '\t\tif (afterOpen.length === 0) return true;',
  '\t\tif (close.startsWith(afterOpen)) return true;',
  '\t}',
  '\treturn false;',
  '}',
  '',
].join('\n');

const REPLAY_TEXT_REPLACEMENT = [
  '\t\t\tcase "text": {',
  '\t\t\t\tconst __t = __dshDsStripEmptyThinking(block.text);',
  '\t\t\t\treturn __t.length === 0 ? [] : {',
  '\t\t\t\t\ttype: "text",',
  '\t\t\t\t\ttext: __t',
  '\t\t\t\t};',
  '\t\t\t}',
].join('\n');

const TRANSLATE_HEAD_REPLACEMENT = [
  'async function* translate(events, model) {',
  '\tconst blocks = /* @__PURE__ */ new Map();',
  '\tconst __emittedReplays = [];',
].join('\n');

const TRANSLATE_BLOCKS_REPLACEMENT = [
  '\t\tif (event.type === "content_block_start") {',
  '\t\t\tconst wireIndex = indexOf(event);',
  '\t\t\tif (blocks.has(wireIndex) || reason !== void 0) return malformed("block starts after settlement or repeats an index");',
  '\t\t\tconst block = startBlock(event, __emittedReplays.length);',
  '\t\t\tblocks.set(wireIndex, block);',
  '\t\t\tif (block.content.type === "text") {',
  '\t\t\t\tif (__dshDsIsEmptyThinkingPrefix(block.content.text)) {',
  '\t\t\t\t\tblock._thinkPending = true;',
  '\t\t\t\t\tblock._thinkBuf = block.content.text;',
  '\t\t\t\t} else {',
  '\t\t\t\t\tblock.content.text = __dshDsStripEmptyThinking(block.content.text);',
  '\t\t\t\t\tblock.index = __emittedReplays.length;',
  '\t\t\t\t\t__emittedReplays.push(block.replay);',
  '\t\t\t\t\tyield {',
  '\t\t\t\t\t\ttype: "block-start",',
  '\t\t\t\t\t\tindex: block.index,',
  '\t\t\t\t\t\tblockType: "text"',
  '\t\t\t\t\t};',
  '\t\t\t\t\tif (block.content.text) yield {',
  '\t\t\t\t\t\ttype: "text-delta",',
  '\t\t\t\t\t\tindex: block.index,',
  '\t\t\t\t\t\ttext: block.content.text',
  '\t\t\t\t\t};',
  '\t\t\t\t}',
  '\t\t\t} else {',
  '\t\t\t\tblock.index = __emittedReplays.length;',
  '\t\t\t\t__emittedReplays.push(block.replay);',
  '\t\t\t\tyield {',
  '\t\t\t\t\ttype: "block-start",',
  '\t\t\t\t\tindex: block.index,',
  '\t\t\t\t\tblockType: block.content.type',
  '\t\t\t\t};',
  '\t\t\t\tif (block.content.type === "reasoning") {',
  '\t\t\t\t\tif (block.content.text) yield {',
  '\t\t\t\t\t\ttype: "reasoning-delta",',
  '\t\t\t\t\t\tindex: block.index,',
  '\t\t\t\t\t\ttext: block.content.text',
  '\t\t\t\t\t};',
  '\t\t\t\t} else yield {',
  '\t\t\t\t\ttype: "tool-call-delta",',
  '\t\t\t\t\tindex: block.index,',
  '\t\t\t\t\tid: block.content.id,',
  '\t\t\t\t\tname: block.content.name,',
  '\t\t\t\t\targumentsDelta: ""',
  '\t\t\t\t};',
  '\t\t\t}',
  '\t\t} else if (event.type === "content_block_delta" || event.type === "content_block_stop") {',
  '\t\t\tconst block = blocks.get(indexOf(event));',
  '\t\t\tif (block === void 0 || block.closed) return malformed("delta/stop without an open block");',
  '\t\t\tif (event.type === "content_block_delta") {',
  '\t\t\t\tconst chunk = deltaChunk(block, event.delta);',
  '\t\t\t\tif (block._thinkPending) {',
  '\t\t\t\t\tblock._thinkBuf += chunk ? chunk.text : "";',
  '\t\t\t\t\tif (!__dshDsIsEmptyThinkingPrefix(block._thinkBuf)) {',
  '\t\t\t\t\t\tblock._thinkPending = false;',
  '\t\t\t\t\t\tconst __cleaned = __dshDsStripEmptyThinking(block._thinkBuf);',
  '\t\t\t\t\t\tblock.content.text = __cleaned;',
  '\t\t\t\t\t\tblock.index = __emittedReplays.length;',
  '\t\t\t\t\t\t__emittedReplays.push(block.replay);',
  '\t\t\t\t\t\tyield {',
  '\t\t\t\t\t\t\ttype: "block-start",',
  '\t\t\t\t\t\t\tindex: block.index,',
  '\t\t\t\t\t\t\tblockType: "text"',
  '\t\t\t\t\t\t};',
  '\t\t\t\t\t\tif (__cleaned) yield {',
  '\t\t\t\t\t\t\ttype: "text-delta",',
  '\t\t\t\t\t\t\tindex: block.index,',
  '\t\t\t\t\t\t\ttext: __cleaned',
  '\t\t\t\t\t\t};',
  '\t\t\t\t\t}',
  '\t\t\t\t} else if (chunk !== void 0) yield chunk;',
  '\t\t\t} else {',
  '\t\t\t\tblock.closed = true;',
  '\t\t\t\tif (block._thinkPending) {',
  '\t\t\t\t\tblock._thinkPending = false;',
  '\t\t\t\t\tconst __cleaned = __dshDsStripEmptyThinking(block._thinkBuf);',
  '\t\t\t\t\tif (__cleaned.length > 0 || block._thinkBuf === __cleaned) {',
  '\t\t\t\t\t\tblock.content.text = __cleaned;',
  '\t\t\t\t\t\tblock.index = __emittedReplays.length;',
  '\t\t\t\t\t\t__emittedReplays.push(block.replay);',
  '\t\t\t\t\t\tyield {',
  '\t\t\t\t\t\t\ttype: "block-start",',
  '\t\t\t\t\t\t\tindex: block.index,',
  '\t\t\t\t\t\t\tblockType: "text"',
  '\t\t\t\t\t\t};',
  '\t\t\t\t\t\tif (__cleaned) yield {',
  '\t\t\t\t\t\t\ttype: "text-delta",',
  '\t\t\t\t\t\t\tindex: block.index,',
  '\t\t\t\t\t\t\ttext: __cleaned',
  '\t\t\t\t\t\t};',
  '\t\t\t\t\t\tyield {',
  '\t\t\t\t\t\t\ttype: "block-end",',
  '\t\t\t\t\t\t\tindex: block.index,',
  '\t\t\t\t\t\t\tblock: { ...block.content }',
  '\t\t\t\t\t\t};',
  '\t\t\t\t\t}',
  '\t\t\t\t} else {',
  '\t\t\t\t\tif (block.content.type === "tool-call" && block.json.length > 0) block.content.arguments = block.json;',
  '\t\t\t\t\tyield {',
  '\t\t\t\t\t\ttype: "block-end",',
  '\t\t\t\t\t\tindex: block.index,',
  '\t\t\t\t\t\tblock: { ...block.content }',
  '\t\t\t\t\t};',
  '\t\t\t\t}',
  '\t\t\t}',
  '\t\t}',
].join('\n');

const EMPTY_STOP_REPLACEMENT = '\t\t\tif (__emittedReplays.length === 0 && reason.kind === "stop") throw new LlmError("DeepSeek Messages returned no content", "EMPTY_RESPONSE");';
const REPLAY_STATE_REPLACEMENT = '\t\t\t\treplayState: replayState(model, __emittedReplays)';

const PAIRS = [
  [REPLAY_TEXT_ANCHOR, REPLAY_TEXT_REPLACEMENT],
  [TRANSLATE_HEAD_ANCHOR, TRANSLATE_HEAD_REPLACEMENT],
  [TRANSLATE_BLOCKS_ANCHOR, TRANSLATE_BLOCKS_REPLACEMENT],
  [EMPTY_STOP_ANCHOR, EMPTY_STOP_REPLACEMENT],
  [REPLAY_STATE_ANCHOR, REPLAY_STATE_REPLACEMENT],
];

const hits = (src, needle) => src.split(needle).length - 1;

function transformDsEmptyThinkingStrip(src, file) {
  if (src.includes(MARKER)) return { status: 'already' };
  const miss = [ASSISTANT_FN_ANCHOR, ...PAIRS.map(([from]) => from)]
    .map((n) => ({ n, hits: hits(src, n) }))
    .filter((r) => r.hits !== 1);
  if (miss.length > 0) {
    return {
      status: 'anchor-missing',
      detail: '空思考标签拦截锚点非唯一命中（' +
        miss.map((r) => JSON.stringify(r.n.trim().slice(0, 40)) + '=' + r.hits).join(' / ') +
        '，版本可能已变化），跳过 ' + (file || '<unknown>'),
    };
  }
  let out = src.replace(ASSISTANT_FN_ANCHOR, () => HELPER + ASSISTANT_FN_ANCHOR);
  for (const [from, to] of PAIRS) out = out.replace(from, () => to);
  return { status: 'changed', src: out };
}

function patchDsEmptyThinkingStrip(nmRoot, log = () => {}, stats) {
  const file = path.join(nmRoot, TARGET_REL);
  if (!fs.existsSync(file)) return 0;
  let src;
  try {
    src = fs.readFileSync(file, 'utf8');
  } catch (err) {
    log('ds 空思考标签拦截补丁: 读取失败 ' + file + ': ' + err.message);
    if (stats) stats.failed += 1;
    return 0;
  }
  const result = transformDsEmptyThinkingStrip(src, file);
  if (result.status === 'already') {
    log('ds 空思考标签拦截补丁: 已应用，跳过 ' + file);
    return 0;
  }
  if (result.status === 'anchor-missing') {
    log('ds 空思考标签拦截补丁: ' + result.detail);
    if (stats) stats.anchorMissing += 1;
    return 0;
  }
  try {
    writeFileAtomic(file, result.src);
    log('ds 空思考标签拦截补丁: 已注入入站流式过滤与出站回放清理 ' + file);
    return 1;
  } catch (err) {
    log('ds 空思考标签拦截补丁: 写入失败 ' + file + ': ' + err.message);
    if (stats) stats.failed += 1;
  }
  return 0;
}

module.exports = {
  patchDsEmptyThinkingStrip,
  transformDsEmptyThinkingStrip,
  MARKER,
  TARGET_REL,
};

if (require.main === module) {
  const root = process.argv[2] ? path.resolve(process.argv[2]) : path.resolve(__dirname, '..', 'node_modules');
  const n = patchDsEmptyThinkingStrip(root, (m) => console.log(m));
  console.log(n > 0 ? 'patched ' + n + ' file(s)' : 'nothing to patch');
}
