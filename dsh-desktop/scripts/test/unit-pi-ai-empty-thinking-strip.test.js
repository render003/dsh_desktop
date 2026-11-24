'use strict';

// 单测：patch-pi-ai-empty-thinking-strip.js
// 验证：
//   1. 真实 @deepseek-ai/dsh-llm-pi-ai/lib/index.js 上变换命中、幂等、可解析、漂移 fail-closed；
//   2. 入站流 toStreamChunks：
//      - 独立纯空 `<thinking> </thinking>` 块紧跟 toolCall 时整块丢弃，后续块 index 连续重排
//        且 replayState.blocks 与发出块 1:1 对齐；
//      - `<thinking> </thinking>\n\n真实正文` 跨分片到达时剥离前导空标签，正文零延迟透传；
//      - 整轮 stop 仅产出空思考标签时抛 EMPTY_RESPONSE 触发重试；
//      - 非空 `<thinking>真实思考</thinking>` 与普通文本原样保留。
//   3. 出站历史回放 foreignAssistant / replayedAssistant：
//      - 老会话里落盘的纯空 `<thinking> </thinking>` 文本块从出站 content 省略（不触发 invalidReplay）；
//      - 带前导空思考标签的历史正文剥离前导标签。

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const {
  transformPiAiEmptyThinkingStrip,
  MARKER,
  TARGET_REL,
} = require('../patch-pi-ai-empty-thinking-strip');

const DESKTOP_ROOT = path.resolve(__dirname, '..', '..');
const PIAI_LIVE = path.join(DESKTOP_ROOT, 'node_modules', TARGET_REL);

/** 从已打补丁的源码中提取 helper + foreignAssistant + replayedAssistant + toStreamChunks 在沙箱里真跑。 */
function loadPatchedRuntime(patchedSrc) {
  const startHelper = patchedSrc.indexOf('const __DSH_PI_EMPTY_THINK_LEADING_RE');
  const endForeign = patchedSrc.indexOf('/** Recombine durable Harness content');
  const startReplayed = patchedSrc.indexOf('function replayedAssistant(');
  const endReplayed = patchedSrc.indexOf('/**\n* Convert one durable Harness assistant message');
  const startStream = patchedSrc.indexOf('async function* toStreamChunks(');
  const endStream = patchedSrc.indexOf('throw new LlmError("pi-ai event stream ended without done/error", "STREAM_CLOSED");\n}') +
    'throw new LlmError("pi-ai event stream ended without done/error", "STREAM_CLOSED");\n}'.length;

  assert.ok(startHelper > 0 && endForeign > startHelper, '未能定位 helper/foreignAssistant');
  assert.ok(startReplayed > 0 && endReplayed > startReplayed, '未能定位 replayedAssistant');
  assert.ok(startStream > 0 && endStream > startStream, '未能定位 toStreamChunks');

  class LlmError extends Error {
    constructor(msg, code) { super(msg); this.code = code; }
  }
  const code = [
    patchedSrc.slice(startHelper, endForeign),
    patchedSrc.slice(startReplayed, endReplayed),
    patchedSrc.slice(startStream, endStream),
    '({ __dshPiStripEmptyThinking, __dshPiIsEmptyThinkingPrefix, foreignAssistant, replayedAssistant, toStreamChunks })',
  ].join('\n');

  const sandbox = {
    LlmError,
    __dshPiRestoreToolName: (n) => n,
    parseArguments: (raw) => JSON.parse(raw || '{}'),
    emptyPiUsage: () => ({ input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } }),
    readReplayState: (s) => s,
    invalidReplay: (msg) => { throw new Error('invalidReplay: ' + msg); },
    brandString: (s) => s,
    mapUsage: (u) => u,
    mapStopReason: (m) => ({ kind: m.stopReason === 'toolUse' ? 'tool-calls' : 'stop' }),
    toPiReplayState: (m) => ({
      response: { api: m.api || 'openai-responses', provider: m.provider || 'sub2api', model: m.model || 'deepseek-v4.1-flash-1017', stopReason: m.stopReason || 'stop' },
      blocks: m.content.map((b) => ({ type: b.type === 'thinking' ? 'reasoning' : b.type === 'toolCall' ? 'tool-call' : 'text' })),
    }),
  };
  return vm.runInNewContext(code, sandbox);
}

test('pi-ai-empty-thinking-strip：真靶命中、幂等、漂移 fail-closed', () => {
  assert.ok(fs.existsSync(PIAI_LIVE), `缺少靶文件: ${PIAI_LIVE}`);
  const src = fs.readFileSync(PIAI_LIVE, 'utf8');
  let patched = src;
  if (src.includes(MARKER)) {
    assert.equal(transformPiAiEmptyThinkingStrip(src, PIAI_LIVE).status, 'already');
  } else {
    const r1 = transformPiAiEmptyThinkingStrip(src, PIAI_LIVE);
    assert.equal(r1.status, 'changed');
    patched = r1.src;
    assert.equal(transformPiAiEmptyThinkingStrip(patched, PIAI_LIVE).status, 'already');
  }
  assert.equal(transformPiAiEmptyThinkingStrip('/* unrelated */', 'fake.js').status, 'anchor-missing');
});

test('入站流 toStreamChunks：丢弃纯空 <thinking> 块、重排索引并保持 replayState 与 replayedAssistant 1:1 对齐', async () => {
  const src = fs.readFileSync(PIAI_LIVE, 'utf8');
  const patched = src.includes(MARKER) ? src : transformPiAiEmptyThinkingStrip(src, PIAI_LIVE).src;
  const rt = loadPatchedRuntime(patched);

  const wireMsg = {
    api: 'openai-responses',
    provider: 'sub2api',
    model: 'deepseek-v4.1-flash-1017',
    stopReason: 'toolUse',
    usage: { input: 10, output: 5 },
    content: [
      { type: 'thinking', thinking: ' ' },
      { type: 'text', text: '<thinking> </thinking>' },
      { type: 'toolCall', id: 'call_1|item_1', name: 'edit', arguments: { a: 1 } },
    ],
  };

  async function* events() {
    yield { type: 'start' };
    yield { type: 'thinking_start', contentIndex: 0 };
    yield { type: 'thinking_delta', contentIndex: 0, delta: ' ' };
    yield { type: 'thinking_end', contentIndex: 0, content: ' ' };
    yield { type: 'text_start', contentIndex: 1 };
    yield { type: 'text_delta', contentIndex: 1, delta: '<think' };
    yield { type: 'text_delta', contentIndex: 1, delta: 'ing> ' };
    yield { type: 'text_delta', contentIndex: 1, delta: '</thinking>' };
    yield { type: 'text_end', contentIndex: 1, content: '<thinking> </thinking>' };
    yield { type: 'toolcall_start', contentIndex: 2, partial: wireMsg };
    yield { type: 'toolcall_delta', contentIndex: 2, delta: '{"a":1}' };
    yield { type: 'toolcall_end', contentIndex: 2, toolCall: wireMsg.content[2] };
    yield { type: 'done', message: wireMsg };
  }

  const out = [];
  for await (const chunk of rt.toStreamChunks(events(), 128000)) out.push(chunk);

  const ends = out.filter((c) => c.type === 'block-end');
  assert.equal(ends.length, 2, '空 <thinking> 文本块应被完全丢弃');
  assert.equal(ends[0].index, 0);
  assert.equal(ends[0].block.type, 'reasoning');
  assert.equal(ends[1].index, 1, 'tool-call 索引应重排为连续的 1');
  assert.equal(ends[1].block.type, 'tool-call');

  const finish = out.find((c) => c.type === 'finish');
  assert.equal(finish.replayState.blocks.length, 2, 'replayState 应同步过滤掉已丢弃的空思考块');
  assert.deepEqual(finish.replayState.blocks.map((b) => b.type), ['reasoning', 'tool-call']);

  // 端到端回放验证：replayedAssistant 接受过滤后的 message + replayState 不得报 invalidReplay。
  const replayed = rt.replayedAssistant(
    { content: ends.map((e) => e.block) },
    { provider: 'sub2api', model: 'deepseek-v4.1-flash-1017' },
    finish.replayState,
  );
  assert.equal(replayed.content.length, 2);
});

test('入站流 toStreamChunks：剥离前导空 <thinking> 保留后续正文；整轮纯空思考抛 EMPTY_RESPONSE；非空保留', async () => {
  const src = fs.readFileSync(PIAI_LIVE, 'utf8');
  const patched = src.includes(MARKER) ? src : transformPiAiEmptyThinkingStrip(src, PIAI_LIVE).src;
  const rt = loadPatchedRuntime(patched);

  // 1) 前导空标签 + 真实正文（对应实机 LINE 8155 / 8221）
  async function* mixedEvents() {
    yield { type: 'text_start', contentIndex: 0 };
    yield { type: 'text_delta', contentIndex: 0, delta: '<thinking> ' };
    yield { type: 'text_delta', contentIndex: 0, delta: '</thinking>\n\n渲染侧' };
    yield { type: 'text_delta', contentIndex: 0, delta: '计数' };
    yield { type: 'text_end', contentIndex: 0, content: '<thinking> </thinking>\n\n渲染侧计数' };
    yield {
      type: 'done',
      message: { stopReason: 'stop', usage: {}, content: [{ type: 'text', text: '<thinking> </thinking>\n\n渲染侧计数' }] },
    };
  }
  const mixed = [];
  for await (const c of rt.toStreamChunks(mixedEvents(), 128000)) mixed.push(c);
  assert.equal(mixed.filter((c) => c.type === 'text-delta').map((c) => c.text).join(''), '渲染侧计数');
  assert.equal(mixed.find((c) => c.type === 'block-end').block.text, '渲染侧计数');

  // 2) 整轮 stop 只有空思考标签 → EMPTY_RESPONSE
  async function* onlyEmptyEvents() {
    yield { type: 'text_start', contentIndex: 0 };
    yield { type: 'text_delta', contentIndex: 0, delta: '<thinking> </thinking>' };
    yield { type: 'text_end', contentIndex: 0, content: '<thinking> </thinking>' };
    yield {
      type: 'done',
      message: { stopReason: 'stop', usage: {}, content: [{ type: 'text', text: '<thinking> </thinking>' }] },
    };
  }
  await assert.rejects(async () => {
    for await (const _ of rt.toStreamChunks(onlyEmptyEvents(), 128000)) { /* consume */ }
  }, (err) => err && err.code === 'EMPTY_RESPONSE');

  // 3) 非空 <thinking>真实内容</thinking> 原样保留
  async function* nonEmptyEvents() {
    yield { type: 'text_start', contentIndex: 0 };
    yield { type: 'text_delta', contentIndex: 0, delta: '<thinking>重要</thinking>' };
    yield { type: 'text_end', contentIndex: 0, content: '<thinking>重要</thinking>' };
    yield {
      type: 'done',
      message: { stopReason: 'stop', usage: {}, content: [{ type: 'text', text: '<thinking>重要</thinking>' }] },
    };
  }
  const kept = [];
  for await (const c of rt.toStreamChunks(nonEmptyEvents(), 128000)) kept.push(c);
  assert.equal(kept.find((c) => c.type === 'block-end').block.text, '<thinking>重要</thinking>');
});

test('出站回放 foreignAssistant / replayedAssistant：清洗已污染历史中的空思考块且不破坏 replayState 校验', () => {
  const src = fs.readFileSync(PIAI_LIVE, 'utf8');
  const patched = src.includes(MARKER) ? src : transformPiAiEmptyThinkingStrip(src, PIAI_LIVE).src;
  const rt = loadPatchedRuntime(patched);

  const pollutedMsg = {
    source: { provider: 'sub2api', model: 'deepseek-v4.1-flash-1017' },
    content: [
      { type: 'text', text: '<thinking> </thinking>' },
      { type: 'text', text: '<thinking> </thinking>\n\n真实回复' },
      { type: 'tool-call', id: 'call_1', name: 'edit', arguments: '{}' },
    ],
  };

  const foreign = rt.foreignAssistant(pollutedMsg);
  assert.equal(foreign.content.length, 2, 'foreignAssistant 应省略纯空思考块');
  assert.equal(foreign.content[0].text, '真实回复');
  assert.equal(foreign.content[1].type, 'toolCall');

  const rawState = {
    response: { api: 'openai-responses', provider: 'sub2api', model: 'deepseek-v4.1-flash-1017', stopReason: 'toolUse' },
    blocks: [{ type: 'text' }, { type: 'text' }, { type: 'tool-call' }],
  };
  const replayed = rt.replayedAssistant(pollutedMsg, pollutedMsg.source, rawState);
  assert.equal(replayed.content.length, 2, 'replayedAssistant 校验原长度通过后应省略纯空思考块');
  assert.equal(replayed.content[0].text, '真实回复');
  assert.equal(replayed.content[1].type, 'toolCall');
});

test('DSML 控制标签全场景：纯 DSML + 空白 reasoning 抛 EMPTY_RESPONSE；尾随 DSML 零闪烁剥离；反引号示例原样保留', async () => {
  const src = fs.readFileSync(PIAI_LIVE, 'utf8');
  const patched = src.includes(MARKER) ? src : transformPiAiEmptyThinkingStrip(src, PIAI_LIVE).src;
  const rt = loadPatchedRuntime(patched);

  // 1) 实机 line 8341 精确复现：reasoning(" ") + text("</｜｜DSML｜｜ invoke>\n</｜｜DSML｜｜ calls>") + stop → EMPTY_RESPONSE
  async function* dsmlOnlyWithBlankReasoning() {
    yield { type: 'thinking_start', contentIndex: 0 };
    yield { type: 'thinking_delta', contentIndex: 0, delta: ' ' };
    yield { type: 'thinking_end', contentIndex: 0, content: ' ' };
    yield { type: 'text_start', contentIndex: 1 };
    for (const delta of ['</｜｜DSML｜｜', ' invoke', '>\n', '</｜｜DSML｜｜', ' calls', '>']) {
      yield { type: 'text_delta', contentIndex: 1, delta };
    }
    yield { type: 'text_end', contentIndex: 1, content: '</｜｜DSML｜｜ invoke>\n</｜｜DSML｜｜ calls>' };
    yield {
      type: 'done',
      message: {
        stopReason: 'stop',
        usage: {},
        content: [
          { type: 'thinking', thinking: ' ' },
          { type: 'text', text: '</｜｜DSML｜｜ invoke>\n</｜｜DSML｜｜ calls>' },
        ],
      },
    };
  }
  await assert.rejects(async () => {
    for await (const _ of rt.toStreamChunks(dsmlOnlyWithBlankReasoning(), 128000)) { /* consume */ }
  }, (err) => err && err.code === 'EMPTY_RESPONSE');

  // 2) 实机尾随 DSML 闭合标签（正文后紧跟 </｜｜DSML｜｜ parameter>\n</｜｜DSML｜｜ invoke>\n</｜｜DSML｜｜ calls>）
  const rawTail = 'Run both in one command.</｜｜DSML｜｜ parameter>\n</｜｜DSML｜｜ invoke>\n</｜｜DSML｜｜ calls>';
  async function* trailingDsmlEvents() {
    yield { type: 'text_start', contentIndex: 0 };
    for (const delta of ['Run both in one command.', '</｜｜DSML｜｜', ' parameter', '>\n', '</｜｜DSML｜｜', ' invoke', '>\n', '</｜｜DSML｜｜', ' calls', '>']) {
      yield { type: 'text_delta', contentIndex: 0, delta };
    }
    yield { type: 'text_end', contentIndex: 0, content: rawTail };
    yield { type: 'done', message: { stopReason: 'stop', usage: {}, content: [{ type: 'text', text: rawTail }] } };
  }
  const tailChunks = [];
  for await (const c of rt.toStreamChunks(trailingDsmlEvents(), 128000)) tailChunks.push(c);
  assert.equal(tailChunks.filter((c) => c.type === 'text-delta').map((c) => c.text).join(''), 'Run both in one command.');
  assert.equal(tailChunks.find((c) => c.type === 'block-end').block.text, 'Run both in one command.');

  // 3) 空 thinking + <｜DSML｜ parameter name="thinking"> </thinking> 组合后跟 toolCall → 整块丢弃
  const mixedDsmlEmpty = '<thinking> </thinking>\n\n<｜DSML｜ parameter name="thinking"> </thinking>\n\n';
  async function* dsmlParamEmptyBeforeTool() {
    yield { type: 'text_start', contentIndex: 0 };
    yield { type: 'text_delta', contentIndex: 0, delta: '<thinking> </thinking>\n\n<｜DSML｜' };
    yield { type: 'text_delta', contentIndex: 0, delta: ' parameter name="thinking"> </' };
    yield { type: 'text_delta', contentIndex: 0, delta: 'thinking>\n\n' };
    yield { type: 'text_end', contentIndex: 0, content: mixedDsmlEmpty };
    yield { type: 'toolcall_start', contentIndex: 1, partial: { content: [{ type: 'text', text: mixedDsmlEmpty }, { type: 'toolCall', id: 'c1', name: 'pwsh' }] } };
    yield { type: 'toolcall_end', contentIndex: 1, toolCall: { id: 'c1', name: 'pwsh', arguments: {} } };
    yield { type: 'done', message: { stopReason: 'toolUse', usage: {}, content: [{ type: 'text', text: mixedDsmlEmpty }, { type: 'toolCall', id: 'c1', name: 'pwsh', arguments: {} }] } };
  }
  const beforeTool = [];
  for await (const c of rt.toStreamChunks(dsmlParamEmptyBeforeTool(), 128000)) beforeTool.push(c);
  assert.equal(beforeTool.filter((c) => c.type === 'text-delta').length, 0);
  assert.equal(beforeTool.find((c) => c.type === 'block-end').index, 0);
  assert.equal(beforeTool.find((c) => c.type === 'finish').replayState.blocks.length, 1);

  // 4) 反引号包裹的 DSML 示例原样保留（不误伤技术讨论）
  const quoted = '示例：`</｜｜DSML｜｜ invoke>` 文本';
  async function* quotedDsmlEvents() {
    yield { type: 'text_start', contentIndex: 0 };
    yield { type: 'text_delta', contentIndex: 0, delta: '示例：`' };
    yield { type: 'text_delta', contentIndex: 0, delta: '</｜｜DSML｜｜' };
    yield { type: 'text_delta', contentIndex: 0, delta: ' invoke>` 文本' };
    yield { type: 'text_end', contentIndex: 0, content: quoted };
    yield { type: 'done', message: { stopReason: 'stop', usage: {}, content: [{ type: 'text', text: quoted }] } };
  }
  const keptQuoted = [];
  for await (const c of rt.toStreamChunks(quotedDsmlEvents(), 128000)) keptQuoted.push(c);
  assert.equal(keptQuoted.filter((c) => c.type === 'text-delta').map((c) => c.text).join(''), quoted);
  assert.equal(keptQuoted.find((c) => c.type === 'block-end').block.text, quoted);

  // 5) 出站回放清洗已落盘的 DSML 文本块与尾随 DSML
  const dsmlPolluted = {
    source: { provider: 'sub2api', model: 'deepseek-v4.1-flash-1017' },
    content: [
      { type: 'text', text: '</｜｜DSML｜｜ invoke>\n</｜｜DSML｜｜ calls>' },
      { type: 'text', text: rawTail },
    ],
  };
  const out = rt.foreignAssistant(dsmlPolluted);
  assert.equal(out.content.length, 1);
  assert.equal(out.content[0].text, 'Run both in one command.');
});
