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
  '// ' + MARKER + ': 拦截官方 DeepSeek Messages 正文通道泄漏的纯空 <thinking>/<think> 与 DSML 控制标签，',
  '// 兼顾入站流式过滤与出站历史回放清理，防长会话上下文自激复读并保持 replayState 对齐。',
  'const __DSH_EMPTY_THINK_LEADING_RE = /^\\s*(?:(?:<(?:thinking|think)>\\s*<\\/(?:thinking|think)>|<\\uff5c+DSML\\uff5c+\\s*parameter\\s+name="thinking"?\\s*>\\s*<\\/(?:thinking|think)>|<\\/?\\uff5c+DSML\\uff5c+\\s*(?:parameter|invoke|function_calls|calls)\\b[^>]*>|<\\/(?:thinking|think)>)\\s*)+/;',
  'const __DSH_DS_DSML_CLOSE_TRAILING_RE = /\\s*(?:<\\/\\uff5c+DSML\\uff5c+\\s*(?:parameter|invoke|function_calls|calls)\\s*>\\s*)+$/;',
  'const __DSH_DS_DSML_CLOSE_INLINE_RE = /(`?)<\\/\\uff5c+DSML\\uff5c+\\s*(?:parameter|invoke|function_calls|calls)\\s*>/g;',
  'function __dshDsStripEmptyThinking(text) {',
  '\tif (typeof text !== "string" || text.length === 0) return text;',
  '\tif (!text.includes("<")) return text;',
  '\tlet out = text.replace(__DSH_EMPTY_THINK_LEADING_RE, "");',
  '\tif (out.includes("\\uff5c")) {',
  '\t\tout = out.replace(__DSH_DS_DSML_CLOSE_TRAILING_RE, (m, off, str) => off > 0 && str[off - 1] === "`" ? m : "");',
  '\t\tout = out.replace(__DSH_DS_DSML_CLOSE_INLINE_RE, (m, q) => q ? m : "");',
  '\t}',
  '\treturn out;',
  '}',
  'function __dshDsIsEmptyThinkingPrefix(text) {',
  '\tif (typeof text !== "string" || text.length === 0) return true;',
  '\tlet rest = text.replace(__DSH_EMPTY_THINK_LEADING_RE, "");',
  '\trest = rest.replace(/^\\s+/, "");',
  '\tif (rest.length === 0) return true;',
  '\tif (rest === "<" || rest === "</") return true;',
  '\tif ((rest.startsWith("<\\uff5c") || rest.startsWith("</\\uff5c")) && !rest.includes(">") && rest.length <= 96) return true;',
  '\tfor (const tag of ["thinking", "think"]) {',
  '\t\tconst open = "<" + tag + ">";',
  '\t\tconst close = "</" + tag + ">";',
  '\t\tif (close.startsWith(rest)) return true;',
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
  'function __dshDsFilterStreamDelta(st, delta, flush) {',
  '\tlet s = st.tail + (delta || "");',
  '\tst.tail = "";',
  '\tif (s.length === 0) return "";',
  '\tif (!flush && !s.includes("<") && !st.wsHold) {',
  '\t\tst.prevChar = s[s.length - 1];',
  '\t\treturn s;',
  '\t}',
  '\tlet stripped = false;',
  '\tif (s.includes("\\uff5c")) {',
  '\t\ts = s.replace(__DSH_DS_DSML_CLOSE_INLINE_RE, (m, q, off) => {',
  '\t\t\tconst prev = q || (off === 0 ? st.prevChar : "");',
  '\t\t\tif (prev === "`") return m;',
  '\t\t\tstripped = true;',
  '\t\t\treturn "";',
  '\t\t});',
  '\t}',
  '\tlet hold = "";',
  '\tif (!flush) {',
  '\t\tconst idx = s.lastIndexOf("<");',
  '\t\tif (idx !== -1) {',
  '\t\t\tconst tail = s.slice(idx);',
  '\t\t\tconst prevBeforeLt = idx > 0 ? s[idx - 1] : st.prevChar;',
  '\t\t\tif (prevBeforeLt !== "`" && !tail.includes(">") && tail.length <= 96 && (tail === "<" || tail === "</" || tail.startsWith("</\\uff5c"))) {',
  '\t\t\t\thold = tail;',
  '\t\t\t\ts = s.slice(0, idx);',
  '\t\t\t}',
  '\t\t}',
  '\t}',
  '\tif (stripped || st.wsHold) {',
  '\t\tif (s.length > 0 && /^\\s+$/.test(s)) {',
  '\t\t\tif (flush) {',
  '\t\t\t\tst.wsHold = false;',
  '\t\t\t\treturn "";',
  '\t\t\t}',
  '\t\t\tst.wsHold = true;',
  '\t\t\tst.tail = s + hold;',
  '\t\t\treturn "";',
  '\t\t}',
  '\t\tif (s.length === 0) {',
  '\t\t\tst.wsHold = !flush && (stripped || st.wsHold);',
  '\t\t\tst.tail = hold;',
  '\t\t\treturn "";',
  '\t\t}',
  '\t\tst.wsHold = false;',
  '\t}',
  '\tst.tail = hold;',
  '\tif (s.length > 0) st.prevChar = s[s.length - 1];',
  '\treturn s;',
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
  '\tlet __hasSubstantive = false;',
  '\tlet __droppedEmptyText = 0;',
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
  '\t\t\t\t\tblock._dsmlState = { tail: "", prevChar: "", wsHold: false };',
  '\t\t\t\t\tconst __lead = block.content.text.replace(__DSH_EMPTY_THINK_LEADING_RE, "");',
  '\t\t\t\t\tconst __out = __dshDsFilterStreamDelta(block._dsmlState, __lead, false);',
  '\t\t\t\t\tblock.content.text = __lead;',
  '\t\t\t\t\tblock.index = __emittedReplays.length;',
  '\t\t\t\t\t__emittedReplays.push(block.replay);',
  '\t\t\t\t\tyield {',
  '\t\t\t\t\t\ttype: "block-start",',
  '\t\t\t\t\t\tindex: block.index,',
  '\t\t\t\t\t\tblockType: "text"',
  '\t\t\t\t\t};',
  '\t\t\t\t\tif (__out) {',
  '\t\t\t\t\t\tif (__out.trim().length > 0) __hasSubstantive = true;',
  '\t\t\t\t\t\tyield {',
  '\t\t\t\t\t\t\ttype: "text-delta",',
  '\t\t\t\t\t\t\tindex: block.index,',
  '\t\t\t\t\t\t\ttext: __out',
  '\t\t\t\t\t\t};',
  '\t\t\t\t\t}',
  '\t\t\t\t}',
  '\t\t\t} else {',
  '\t\t\t\tblock.index = __emittedReplays.length;',
  '\t\t\t\t__emittedReplays.push(block.replay);',
  '\t\t\t\tif (block.content.type === "tool-call") __hasSubstantive = true;',
  '\t\t\t\tyield {',
  '\t\t\t\t\ttype: "block-start",',
  '\t\t\t\t\tindex: block.index,',
  '\t\t\t\t\tblockType: block.content.type',
  '\t\t\t\t};',
  '\t\t\t\tif (block.content.type === "reasoning") {',
  '\t\t\t\t\tif (block.content.text) {',
  '\t\t\t\t\t\tif (block.content.text.trim().length > 0) __hasSubstantive = true;',
  '\t\t\t\t\t\tyield {',
  '\t\t\t\t\t\t\ttype: "reasoning-delta",',
  '\t\t\t\t\t\t\tindex: block.index,',
  '\t\t\t\t\t\t\ttext: block.content.text',
  '\t\t\t\t\t\t};',
  '\t\t\t\t\t}',
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
  '\t\t\t\t\t\tblock._dsmlState = { tail: "", prevChar: "", wsHold: false };',
  '\t\t\t\t\t\tconst __lead = block._thinkBuf.replace(__DSH_EMPTY_THINK_LEADING_RE, "");',
  '\t\t\t\t\t\tconst __out = __dshDsFilterStreamDelta(block._dsmlState, __lead, false);',
  '\t\t\t\t\t\tblock.content.text = __lead;',
  '\t\t\t\t\t\tblock.index = __emittedReplays.length;',
  '\t\t\t\t\t\t__emittedReplays.push(block.replay);',
  '\t\t\t\t\t\tyield {',
  '\t\t\t\t\t\t\ttype: "block-start",',
  '\t\t\t\t\t\t\tindex: block.index,',
  '\t\t\t\t\t\t\tblockType: "text"',
  '\t\t\t\t\t\t};',
  '\t\t\t\t\t\tif (__out) {',
  '\t\t\t\t\t\t\tif (__out.trim().length > 0) __hasSubstantive = true;',
  '\t\t\t\t\t\t\tyield {',
  '\t\t\t\t\t\t\t\ttype: "text-delta",',
  '\t\t\t\t\t\t\t\tindex: block.index,',
  '\t\t\t\t\t\t\t\ttext: __out',
  '\t\t\t\t\t\t\t};',
  '\t\t\t\t\t\t}',
  '\t\t\t\t\t}',
  '\t\t\t\t} else if (chunk !== void 0) {',
  '\t\t\t\t\tif (block.content.type === "text" && chunk.type === "text-delta") {',
  '\t\t\t\t\t\tif (!block._dsmlState) block._dsmlState = { tail: "", prevChar: "", wsHold: false };',
  '\t\t\t\t\t\tconst __out = __dshDsFilterStreamDelta(block._dsmlState, chunk.text, false);',
  '\t\t\t\t\t\tif (__out) {',
  '\t\t\t\t\t\t\tif (__out.trim().length > 0) __hasSubstantive = true;',
  '\t\t\t\t\t\t\tyield { ...chunk, text: __out };',
  '\t\t\t\t\t\t}',
  '\t\t\t\t\t} else {',
  '\t\t\t\t\t\tif (chunk.type === "reasoning-delta" && typeof chunk.text === "string" && chunk.text.trim().length > 0) __hasSubstantive = true;',
  '\t\t\t\t\t\tyield chunk;',
  '\t\t\t\t\t}',
  '\t\t\t\t}',
  '\t\t\t} else {',
  '\t\t\t\tblock.closed = true;',
  '\t\t\t\tif (block._thinkPending) {',
  '\t\t\t\t\tblock._thinkPending = false;',
  '\t\t\t\t\tconst __cleaned = __dshDsStripEmptyThinking(block._thinkBuf);',
  '\t\t\t\t\tif (__cleaned.length > 0 || block._thinkBuf === __cleaned) {',
  '\t\t\t\t\t\tblock.content.text = __cleaned;',
  '\t\t\t\t\t\tif (__cleaned.trim().length > 0) __hasSubstantive = true;',
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
  '\t\t\t\t\t} else __droppedEmptyText += 1;',
  '\t\t\t\t} else {',
  '\t\t\t\t\tif (block.content.type === "text") {',
  '\t\t\t\t\t\tconst __flushed = block._dsmlState ? __dshDsFilterStreamDelta(block._dsmlState, "", true) : "";',
  '\t\t\t\t\t\tif (__flushed) {',
  '\t\t\t\t\t\t\tif (__flushed.trim().length > 0) __hasSubstantive = true;',
  '\t\t\t\t\t\t\tyield {',
  '\t\t\t\t\t\t\t\ttype: "text-delta",',
  '\t\t\t\t\t\t\t\tindex: block.index,',
  '\t\t\t\t\t\t\t\ttext: __flushed',
  '\t\t\t\t\t\t\t};',
  '\t\t\t\t\t\t}',
  '\t\t\t\t\t\tblock.content.text = __dshDsStripEmptyThinking(block.content.text);',
  '\t\t\t\t\t\tif (block.content.text.trim().length > 0) __hasSubstantive = true;',
  '\t\t\t\t\t} else if (block.content.type === "tool-call" && block.json.length > 0) {',
  '\t\t\t\t\t\tblock.content.arguments = block.json;',
  '\t\t\t\t\t}',
  '\t\t\t\t\tyield {',
  '\t\t\t\t\t\ttype: "block-end",',
  '\t\t\t\t\t\tindex: block.index,',
  '\t\t\t\t\t\tblock: { ...block.content }',
  '\t\t\t\t\t};',
  '\t\t\t\t}',
  '\t\t\t}',
  '\t\t}',
].join('\n');

const EMPTY_STOP_REPLACEMENT = '\t\t\tif ((__emittedReplays.length === 0 || (!__hasSubstantive && __droppedEmptyText > 0)) && reason.kind === "stop") throw new LlmError("DeepSeek Messages returned no content", "EMPTY_RESPONSE");';
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
