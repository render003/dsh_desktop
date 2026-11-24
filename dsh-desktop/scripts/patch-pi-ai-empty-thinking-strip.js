'use strict';

// dsh-llm-pi-ai 空思考标签拦截补丁（自定义 / OpenAI / Responses / Anthropic 等 pi-ai 路由）。
//
// 背景：此前 ds-empty-thinking-strip 仅覆盖官方 @deepseek-ai/dsh-llm-deepseek 路由；
// 但自定义网关（如 sub2api / OpenAI Responses / 第三方兼容端点）均经由
// @deepseek-ai/dsh-llm-pi-ai 转发。deepseek-v4.1-flash 等模型在这些路由下同样会
// 把空 `<thinking> </thinking>` / `<think> </think>` 泄入普通 `type === "text"` 块
// （单独一块或带 `\n\n` 正文前缀），导致前端打断工具折叠并在长会话回放中自激复读。
//
// 治理（双端闭环，与 ds-empty-thinking-strip 同构）：
//   1. 入站流翻译 `toStreamChunks()`：
//      - 对 `text_start`/`text_delta`/`text_end` 仅在处于「空思考标签前缀」时暂存；
//      - 一旦出现真实正文立即剥离前导空标签并按零延迟透传（后续 delta 直出）；
//      - 若整块闭合后只剩纯空 `<thinking>/<think>` 标签则彻底丢弃（不发 block-start/
//        text-delta/block-end），重排后续块 index 保持 0..N-1 连续，并在 `done` 时同步
//        过滤 `event.message.content` 使 `replayState.blocks` 与发出块严格 1:1 对齐；
//      - 若整轮 `stop` 因丢弃空思考块而无任何剩余输出，按 `EMPTY_RESPONSE` 触发重试。
//   2. 出站历史回放 `foreignAssistant()` 与 `replayedAssistant()`：
//      - 对既有已污染会话里落盘的 `text` 块剥离前导空思考标签；剥后为空则从发给模型的
//        `content` 中省略（`replayedAssistant` 先完成原长度与类型校验再 `flatMap` 省略，
//        不破坏既有 `replayState` 的签名对齐），立刻阻断老会话的复读正反馈。
//
// 保守原则：仅剔除位于块首的纯空 `<thinking>\s*</thinking>` / `<think>\s*</think>`；
// 正常正文、非空 `<thinking>实内容</thinking>` 及正文中间提及的标签一律原样保留。

const fs = require('node:fs');
const path = require('node:path');
const { writeFileAtomic } = require('./lib/patch-io');

const TARGET_REL = path.join('@deepseek-ai', 'dsh-llm-pi-ai', 'lib', 'index.js');
const MARKER = 'dsh-desktop patch (pi-ai empty thinking strip)';

const FOREIGN_FN_ANCHOR = 'function foreignAssistant(message) {';
const FOREIGN_TEXT_ANCHOR = [
  '\t\tcase "text":',
  '\t\t\tcontent.push({',
  '\t\t\t\ttype: "text",',
  '\t\t\t\ttext: block.text',
  '\t\t\t});',
  '\t\t\tbreak;',
].join('\n');

const REPLAYED_TEXT_ANCHOR = [
  '\t\tcontent: message.content.map((block, index) => {',
  '\t\t\tconst replay = state.blocks[index];',
  '\t\t\tif (replay === void 0 || replay.type !== block.type) return invalidReplay(`block ${index} does not match assistant content`);',
  '\t\t\tswitch (block.type) {',
  '\t\t\t\tcase "text": return {',
  '\t\t\t\t\ttype: "text",',
  '\t\t\t\t\ttext: block.text,',
  '\t\t\t\t\t...replay.type === "text" && replay.textSignature !== void 0 ? { textSignature: replay.textSignature } : {}',
  '\t\t\t\t};',
].join('\n');

const STREAM_ANCHOR = [
  'async function* toStreamChunks(events, contextWindow, callerSignal, requestedModel, prefixDigest) {',
  '\tconst toolIds = /* @__PURE__ */ new Map();',
  '\tlet argumentsError;',
  '\tfor await (const event of events) switch (event.type) {',
  '\t\tcase "start": break;',
  '\t\tcase "text_start":',
  '\t\t\tyield {',
  '\t\t\t\ttype: "block-start",',
  '\t\t\t\tindex: event.contentIndex,',
  '\t\t\t\tblockType: "text"',
  '\t\t\t};',
  '\t\t\tbreak;',
  '\t\tcase "text_delta":',
  '\t\t\tyield {',
  '\t\t\t\ttype: "text-delta",',
  '\t\t\t\tindex: event.contentIndex,',
  '\t\t\t\ttext: event.delta',
  '\t\t\t};',
  '\t\t\tbreak;',
  '\t\tcase "text_end":',
  '\t\t\tyield {',
  '\t\t\t\ttype: "block-end",',
  '\t\t\t\tindex: event.contentIndex,',
  '\t\t\t\tblock: {',
  '\t\t\t\t\ttype: "text",',
  '\t\t\t\t\ttext: event.content',
  '\t\t\t\t}',
  '\t\t\t};',
  '\t\t\tbreak;',
  '\t\tcase "thinking_start":',
  '\t\t\tyield {',
  '\t\t\t\ttype: "block-start",',
  '\t\t\t\tindex: event.contentIndex,',
  '\t\t\t\tblockType: "reasoning"',
  '\t\t\t};',
  '\t\t\tbreak;',
  '\t\tcase "thinking_delta":',
  '\t\t\tyield {',
  '\t\t\t\ttype: "reasoning-delta",',
  '\t\t\t\tindex: event.contentIndex,',
  '\t\t\t\ttext: event.delta',
  '\t\t\t};',
  '\t\t\tbreak;',
  '\t\tcase "thinking_end":',
  '\t\t\tyield {',
  '\t\t\t\ttype: "block-end",',
  '\t\t\t\tindex: event.contentIndex,',
  '\t\t\t\tblock: {',
  '\t\t\t\t\ttype: "reasoning",',
  '\t\t\t\t\ttext: event.content',
  '\t\t\t\t}',
  '\t\t\t};',
  '\t\t\tbreak;',
  '\t\tcase "toolcall_start": {',
  '\t\t\tconst partial = event.partial.content[event.contentIndex];',
  '\t\t\tconst id = partial?.type === "toolCall" ? partial.id : "";',
  '\t\t\tconst name = partial?.type === "toolCall" ? partial.name : "";',
  '\t\t\ttoolIds.set(event.contentIndex, {',
  '\t\t\t\tid,',
  '\t\t\t\tname',
  '\t\t\t});',
  '\t\t\tyield {',
  '\t\t\t\ttype: "block-start",',
  '\t\t\t\tindex: event.contentIndex,',
  '\t\t\t\tblockType: "tool-call"',
  '\t\t\t};',
  '\t\t\tbreak;',
  '\t\t}',
  '\t\tcase "toolcall_delta": {',
  '\t\t\tconst known = toolIds.get(event.contentIndex);',
  '\t\t\tyield {',
  '\t\t\t\ttype: "tool-call-delta",',
  '\t\t\t\tindex: event.contentIndex,',
  '\t\t\t\tid: brandString(known?.id ?? ""),',
  '\t\t\t\t...known?.name !== void 0 && known.name.length > 0 ? { name: known.name } : {},',
  '\t\t\t\targumentsDelta: event.delta',
  '\t\t\t};',
  '\t\t\tbreak;',
  '\t\t}',
  '\t\tcase "toolcall_end":',
  '\t\t\tif (event.toolCall.argumentsError !== void 0) {',
  '\t\t\t\targumentsError ??= `Tool call "${event.toolCall.name}": ${event.toolCall.argumentsError}`;',
  '\t\t\t\tbreak;',
  '\t\t\t}',
  '\t\t\tyield {',
  '\t\t\t\ttype: "block-end",',
  '\t\t\t\tindex: event.contentIndex,',
  '\t\t\t\tblock: {',
  '\t\t\t\t\ttype: "tool-call",',
  '\t\t\t\t\tid: brandString(event.toolCall.id),',
  '\t\t\t\t\tname: event.toolCall.name,',
  '\t\t\t\t\targuments: JSON.stringify(event.toolCall.arguments)',
  '\t\t\t\t}',
  '\t\t\t};',
  '\t\t\tbreak;',
  '\t\tcase "done":',
  '\t\t\tyield {',
  '\t\t\t\ttype: "usage",',
  '\t\t\t\tusage: mapUsage(event.message.usage)',
  '\t\t\t};',
  '\t\t\tif (argumentsError !== void 0 && event.message.stopReason !== "length") throw new LlmError(argumentsError, "MALFORMED_RESPONSE");',
  '\t\t\tyield {',
  '\t\t\t\ttype: "finish",',
  '\t\t\t\treason: mapStopReason(event.message, contextWindow),',
  '\t\t\t\treplayState: toPiReplayState(event.message, requestedModel, prefixDigest)',
  '\t\t\t};',
  '\t\t\treturn;',
].join('\n');

const HELPER = [
  '// ' + MARKER + ': 拦截 pi-ai 路由（含 sub2api / OpenAI Responses / 兼容端点）正文通道泄漏的',
  '// 纯空 <thinking>/<think> 与 DeepSeek DSML 控制标签（如 </｜｜DSML｜｜ invoke> / calls / parameter），',
  '// 兼顾入站流式过滤与出站历史回放清理，并保持 replayState 对齐。',
  'const __DSH_PI_EMPTY_THINK_LEADING_RE = /^\\s*(?:(?:<(?:thinking|think)>\\s*<\\/(?:thinking|think)>|<\\uff5c+DSML\\uff5c+\\s*parameter\\s+name="thinking"?\\s*>\\s*<\\/(?:thinking|think)>|<\\/?\\uff5c+DSML\\uff5c+\\s*(?:parameter|invoke|function_calls|calls)\\b[^>]*>|<\\/(?:thinking|think)>)\\s*)+/;',
  'const __DSH_PI_DSML_CLOSE_TRAILING_RE = /\\s*(?:<\\/\\uff5c+DSML\\uff5c+\\s*(?:parameter|invoke|function_calls|calls)\\s*>\\s*)+$/;',
  'const __DSH_PI_DSML_CLOSE_INLINE_RE = /(`?)<\\/\\uff5c+DSML\\uff5c+\\s*(?:parameter|invoke|function_calls|calls)\\s*>/g;',
  'function __dshPiStripEmptyThinking(text) {',
  '\tif (typeof text !== "string" || text.length === 0) return text;',
  '\tif (!text.includes("<")) return text;',
  '\tlet out = text.replace(__DSH_PI_EMPTY_THINK_LEADING_RE, "");',
  '\tif (out.includes("\\uff5c")) {',
  '\t\tout = out.replace(__DSH_PI_DSML_CLOSE_TRAILING_RE, (m, off, str) => off > 0 && str[off - 1] === "`" ? m : "");',
  '\t\tout = out.replace(__DSH_PI_DSML_CLOSE_INLINE_RE, (m, q) => q ? m : "");',
  '\t}',
  '\treturn out;',
  '}',
  'function __dshPiIsEmptyThinkingPrefix(text) {',
  '\tif (typeof text !== "string" || text.length === 0) return true;',
  '\tlet rest = text.replace(__DSH_PI_EMPTY_THINK_LEADING_RE, "");',
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
  'function __dshPiFilterStreamDelta(st, delta, flush) {',
  '\tlet s = st.tail + (delta || "");',
  '\tst.tail = "";',
  '\tif (s.length === 0) return "";',
  '\tif (!flush && !s.includes("<") && !st.wsHold) {',
  '\t\tst.prevChar = s[s.length - 1];',
  '\t\treturn s;',
  '\t}',
  '\tlet stripped = false;',
  '\tif (s.includes("\\uff5c")) {',
  '\t\ts = s.replace(__DSH_PI_DSML_CLOSE_INLINE_RE, (m, q, off) => {',
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

const FOREIGN_TEXT_REPLACEMENT = [
  '\t\tcase "text": {',
  '\t\t\tconst __t = __dshPiStripEmptyThinking(block.text);',
  '\t\t\tif (__t.length > 0 || block.text === __t) content.push({',
  '\t\t\t\ttype: "text",',
  '\t\t\t\ttext: __t',
  '\t\t\t});',
  '\t\t\tbreak;',
  '\t\t}',
].join('\n');

const REPLAYED_TEXT_REPLACEMENT = [
  '\t\tcontent: message.content.flatMap((block, index) => {',
  '\t\t\tconst replay = state.blocks[index];',
  '\t\t\tif (replay === void 0 || replay.type !== block.type) return invalidReplay(`block ${index} does not match assistant content`);',
  '\t\t\tswitch (block.type) {',
  '\t\t\t\tcase "text": {',
  '\t\t\t\t\tconst __t = __dshPiStripEmptyThinking(block.text);',
  '\t\t\t\t\tif (__t.length === 0 && block.text !== __t) return [];',
  '\t\t\t\t\treturn [{',
  '\t\t\t\t\t\ttype: "text",',
  '\t\t\t\t\t\ttext: __t,',
  '\t\t\t\t\t\t...replay.type === "text" && replay.textSignature !== void 0 ? { textSignature: replay.textSignature } : {}',
  '\t\t\t\t\t}];',
  '\t\t\t\t}',
].join('\n');

const STREAM_REPLACEMENT = [
  'async function* toStreamChunks(events, contextWindow, callerSignal, requestedModel, prefixDigest) {',
  '\tconst toolIds = /* @__PURE__ */ new Map();',
  '\tconst __indexMap = /* @__PURE__ */ new Map();',
  '\tconst __pendingText = /* @__PURE__ */ new Map();',
  '\tconst __streamStates = /* @__PURE__ */ new Map();',
  '\tconst __droppedWire = /* @__PURE__ */ new Set();',
  '\tlet __nextIdx = 0;',
  '\tlet __hasSubstantive = false;',
  '\tconst __allocIdx = (wireIndex) => {',
  '\t\tlet idx = __indexMap.get(wireIndex);',
  '\t\tif (idx === void 0) {',
  '\t\t\tidx = __nextIdx++;',
  '\t\t\t__indexMap.set(wireIndex, idx);',
  '\t\t}',
  '\t\treturn idx;',
  '\t};',
  '\tlet argumentsError;',
  '\tfor await (const event of events) switch (event.type) {',
  '\t\tcase "start": break;',
  '\t\tcase "text_start":',
  '\t\t\t__pendingText.set(event.contentIndex, { buf: "" });',
  '\t\t\tbreak;',
  '\t\tcase "text_delta": {',
  '\t\t\tconst __p = __pendingText.get(event.contentIndex);',
  '\t\t\tif (__p !== void 0) {',
  '\t\t\t\t__p.buf += event.delta;',
  '\t\t\t\tif (!__dshPiIsEmptyThinkingPrefix(__p.buf)) {',
  '\t\t\t\t\t__pendingText.delete(event.contentIndex);',
  '\t\t\t\t\tconst __st = { tail: "", prevChar: "", wsHold: false };',
  '\t\t\t\t\t__streamStates.set(event.contentIndex, __st);',
  '\t\t\t\t\tconst __lead = __p.buf.replace(__DSH_PI_EMPTY_THINK_LEADING_RE, "");',
  '\t\t\t\t\tconst __cleaned = __dshPiFilterStreamDelta(__st, __lead, false);',
  '\t\t\t\t\tconst __idx = __allocIdx(event.contentIndex);',
  '\t\t\t\t\tyield {',
  '\t\t\t\t\t\ttype: "block-start",',
  '\t\t\t\t\t\tindex: __idx,',
  '\t\t\t\t\t\tblockType: "text"',
  '\t\t\t\t\t};',
  '\t\t\t\t\tif (__cleaned) {',
  '\t\t\t\t\t\tif (__cleaned.trim().length > 0) __hasSubstantive = true;',
  '\t\t\t\t\t\tyield {',
  '\t\t\t\t\t\t\ttype: "text-delta",',
  '\t\t\t\t\t\t\tindex: __idx,',
  '\t\t\t\t\t\t\ttext: __cleaned',
  '\t\t\t\t\t\t};',
  '\t\t\t\t\t}',
  '\t\t\t\t}',
  '\t\t\t} else {',
  '\t\t\t\tlet __st = __streamStates.get(event.contentIndex);',
  '\t\t\t\tif (__st === void 0) {',
  '\t\t\t\t\t__st = { tail: "", prevChar: "", wsHold: false };',
  '\t\t\t\t\t__streamStates.set(event.contentIndex, __st);',
  '\t\t\t\t}',
  '\t\t\t\tconst __out = __dshPiFilterStreamDelta(__st, event.delta, false);',
  '\t\t\t\tif (__out) {',
  '\t\t\t\t\tif (__out.trim().length > 0) __hasSubstantive = true;',
  '\t\t\t\t\tyield {',
  '\t\t\t\t\t\ttype: "text-delta",',
  '\t\t\t\t\t\tindex: __allocIdx(event.contentIndex),',
  '\t\t\t\t\t\ttext: __out',
  '\t\t\t\t\t};',
  '\t\t\t\t}',
  '\t\t\t}',
  '\t\t\tbreak;',
  '\t\t}',
  '\t\tcase "text_end": {',
  '\t\t\tconst __raw = typeof event.content === "string" ? event.content : "";',
  '\t\t\tconst __cleaned = __dshPiStripEmptyThinking(__raw);',
  '\t\t\tif (__pendingText.has(event.contentIndex)) {',
  '\t\t\t\t__pendingText.delete(event.contentIndex);',
  '\t\t\t\tif (__cleaned.length === 0 && __raw !== __cleaned) {',
  '\t\t\t\t\t__droppedWire.add(event.contentIndex);',
  '\t\t\t\t\tbreak;',
  '\t\t\t\t}',
  '\t\t\t\tconst __idx = __allocIdx(event.contentIndex);',
  '\t\t\t\tyield {',
  '\t\t\t\t\ttype: "block-start",',
  '\t\t\t\t\tindex: __idx,',
  '\t\t\t\t\tblockType: "text"',
  '\t\t\t\t};',
  '\t\t\t\tif (__cleaned) {',
  '\t\t\t\t\tif (__cleaned.trim().length > 0) __hasSubstantive = true;',
  '\t\t\t\t\tyield {',
  '\t\t\t\t\t\ttype: "text-delta",',
  '\t\t\t\t\t\tindex: __idx,',
  '\t\t\t\t\t\ttext: __cleaned',
  '\t\t\t\t\t};',
  '\t\t\t\t}',
  '\t\t\t\tyield {',
  '\t\t\t\t\ttype: "block-end",',
  '\t\t\t\t\tindex: __idx,',
  '\t\t\t\t\tblock: {',
  '\t\t\t\t\t\ttype: "text",',
  '\t\t\t\t\t\ttext: __cleaned',
  '\t\t\t\t\t}',
  '\t\t\t\t};',
  '\t\t\t\tbreak;',
  '\t\t\t}',
  '\t\t\tconst __st = __streamStates.get(event.contentIndex);',
  '\t\t\t__streamStates.delete(event.contentIndex);',
  '\t\t\tconst __idx = __allocIdx(event.contentIndex);',
  '\t\t\tconst __flushed = __st !== void 0 ? __dshPiFilterStreamDelta(__st, "", true) : "";',
  '\t\t\tif (__flushed) {',
  '\t\t\t\tif (__flushed.trim().length > 0) __hasSubstantive = true;',
  '\t\t\t\tyield {',
  '\t\t\t\t\ttype: "text-delta",',
  '\t\t\t\t\tindex: __idx,',
  '\t\t\t\t\ttext: __flushed',
  '\t\t\t\t};',
  '\t\t\t}',
  '\t\t\tif (__cleaned.trim().length > 0) __hasSubstantive = true;',
  '\t\t\tyield {',
  '\t\t\t\ttype: "block-end",',
  '\t\t\t\tindex: __idx,',
  '\t\t\t\tblock: {',
  '\t\t\t\t\ttype: "text",',
  '\t\t\t\t\ttext: __cleaned',
  '\t\t\t\t}',
  '\t\t\t};',
  '\t\t\tbreak;',
  '\t\t}',
  '\t\tcase "thinking_start":',
  '\t\t\tyield {',
  '\t\t\t\ttype: "block-start",',
  '\t\t\t\tindex: __allocIdx(event.contentIndex),',
  '\t\t\t\tblockType: "reasoning"',
  '\t\t\t};',
  '\t\t\tbreak;',
  '\t\tcase "thinking_delta":',
  '\t\t\tif (typeof event.delta === "string" && event.delta.trim().length > 0) __hasSubstantive = true;',
  '\t\t\tyield {',
  '\t\t\t\ttype: "reasoning-delta",',
  '\t\t\t\tindex: __allocIdx(event.contentIndex),',
  '\t\t\t\ttext: event.delta',
  '\t\t\t};',
  '\t\t\tbreak;',
  '\t\tcase "thinking_end":',
  '\t\t\tif (typeof event.content === "string" && event.content.trim().length > 0) __hasSubstantive = true;',
  '\t\t\tyield {',
  '\t\t\t\ttype: "block-end",',
  '\t\t\t\tindex: __allocIdx(event.contentIndex),',
  '\t\t\t\tblock: {',
  '\t\t\t\t\ttype: "reasoning",',
  '\t\t\t\t\ttext: event.content',
  '\t\t\t\t}',
  '\t\t\t};',
  '\t\t\tbreak;',
  '\t\tcase "toolcall_start": {',
  '\t\t\t__hasSubstantive = true;',
  '\t\t\tconst partial = event.partial.content[event.contentIndex];',
  '\t\t\tconst id = partial?.type === "toolCall" ? partial.id : "";',
  '\t\t\tconst name = partial?.type === "toolCall" ? partial.name : "";',
  '\t\t\ttoolIds.set(event.contentIndex, {',
  '\t\t\t\tid,',
  '\t\t\t\tname',
  '\t\t\t});',
  '\t\t\tyield {',
  '\t\t\t\ttype: "block-start",',
  '\t\t\t\tindex: __allocIdx(event.contentIndex),',
  '\t\t\t\tblockType: "tool-call"',
  '\t\t\t};',
  '\t\t\tbreak;',
  '\t\t}',
  '\t\tcase "toolcall_delta": {',
  '\t\t\tconst known = toolIds.get(event.contentIndex);',
  '\t\t\tyield {',
  '\t\t\t\ttype: "tool-call-delta",',
  '\t\t\t\tindex: __allocIdx(event.contentIndex),',
  '\t\t\t\tid: brandString(known?.id ?? ""),',
  '\t\t\t\t...known?.name !== void 0 && known.name.length > 0 ? { name: known.name } : {},',
  '\t\t\t\targumentsDelta: event.delta',
  '\t\t\t};',
  '\t\t\tbreak;',
  '\t\t}',
  '\t\tcase "toolcall_end":',
  '\t\t\t__hasSubstantive = true;',
  '\t\t\tif (event.toolCall.argumentsError !== void 0) {',
  '\t\t\t\targumentsError ??= `Tool call "${event.toolCall.name}": ${event.toolCall.argumentsError}`;',
  '\t\t\t\tbreak;',
  '\t\t\t}',
  '\t\t\tyield {',
  '\t\t\t\ttype: "block-end",',
  '\t\t\t\tindex: __allocIdx(event.contentIndex),',
  '\t\t\t\tblock: {',
  '\t\t\t\t\ttype: "tool-call",',
  '\t\t\t\t\tid: brandString(event.toolCall.id),',
  '\t\t\t\t\tname: event.toolCall.name,',
  '\t\t\t\t\targuments: JSON.stringify(event.toolCall.arguments)',
  '\t\t\t\t}',
  '\t\t\t};',
  '\t\t\tbreak;',
  '\t\tcase "done": {',
  '\t\t\tyield {',
  '\t\t\t\ttype: "usage",',
  '\t\t\t\tusage: mapUsage(event.message.usage)',
  '\t\t\t};',
  '\t\t\tif (argumentsError !== void 0 && event.message.stopReason !== "length") throw new LlmError(argumentsError, "MALFORMED_RESPONSE");',
  '\t\t\tconst __cleanMsg = __droppedWire.size > 0 ? { ...event.message, content: event.message.content.filter((_, __i) => !__droppedWire.has(__i)) } : event.message;',
  '\t\t\tif (!__hasSubstantive && event.message.stopReason === "stop" && __droppedWire.size > 0) throw new LlmError("pi-ai returned only empty thinking/DSML tags", "EMPTY_RESPONSE");',
  '\t\t\tyield {',
  '\t\t\t\ttype: "finish",',
  '\t\t\t\treason: mapStopReason(__cleanMsg, contextWindow),',
  '\t\t\t\treplayState: toPiReplayState(__cleanMsg, requestedModel, prefixDigest)',
  '\t\t\t};',
  '\t\t\treturn;',
  '\t\t}',
].join('\n');

const PAIRS = [
  [FOREIGN_TEXT_ANCHOR, FOREIGN_TEXT_REPLACEMENT],
  [REPLAYED_TEXT_ANCHOR, REPLAYED_TEXT_REPLACEMENT],
  [STREAM_ANCHOR, STREAM_REPLACEMENT],
];

const hits = (src, needle) => src.split(needle).length - 1;

function transformPiAiEmptyThinkingStrip(src, file) {
  if (src.includes(MARKER)) return { status: 'already' };
  const miss = [FOREIGN_FN_ANCHOR, ...PAIRS.map(([from]) => from)]
    .map((n) => ({ n, hits: hits(src, n) }))
    .filter((r) => r.hits !== 1);
  if (miss.length > 0) {
    return {
      status: 'anchor-missing',
      detail: 'pi-ai 空思考标签拦截锚点非唯一命中（' +
        miss.map((r) => JSON.stringify(r.n.trim().slice(0, 40)) + '=' + r.hits).join(' / ') +
        '，版本可能已变化），跳过 ' + (file || '<unknown>'),
    };
  }
  let out = src.replace(FOREIGN_FN_ANCHOR, () => HELPER + FOREIGN_FN_ANCHOR);
  for (const [from, to] of PAIRS) out = out.replace(from, () => to);
  return { status: 'changed', src: out };
}

function patchPiAiEmptyThinkingStrip(nmRoot, log = () => {}, stats) {
  const file = path.join(nmRoot, TARGET_REL);
  if (!fs.existsSync(file)) return 0;
  let src;
  try {
    src = fs.readFileSync(file, 'utf8');
  } catch (err) {
    log('pi-ai 空思考标签拦截补丁: 读取失败 ' + file + ': ' + err.message);
    if (stats) stats.failed += 1;
    return 0;
  }
  const result = transformPiAiEmptyThinkingStrip(src, file);
  if (result.status === 'already') {
    log('pi-ai 空思考标签拦截补丁: 已应用，跳过 ' + file);
    return 0;
  }
  if (result.status === 'anchor-missing') {
    log('pi-ai 空思考标签拦截补丁: ' + result.detail);
    if (stats) stats.anchorMissing += 1;
    return 0;
  }
  try {
    writeFileAtomic(file, result.src);
    log('pi-ai 空思考标签拦截补丁: 已注入入站流式过滤与出站回放清理 ' + file);
    return 1;
  } catch (err) {
    log('pi-ai 空思考标签拦截补丁: 写入失败 ' + file + ': ' + err.message);
    if (stats) stats.failed += 1;
  }
  return 0;
}

module.exports = {
  patchPiAiEmptyThinkingStrip,
  transformPiAiEmptyThinkingStrip,
  MARKER,
  TARGET_REL,
};

if (require.main === module) {
  const root = process.argv[2] ? path.resolve(process.argv[2]) : path.resolve(__dirname, '..', 'node_modules');
  const n = patchPiAiEmptyThinkingStrip(root, (m) => console.log(m));
  console.log(n > 0 ? 'patched ' + n + ' file(s)' : 'nothing to patch');
}
