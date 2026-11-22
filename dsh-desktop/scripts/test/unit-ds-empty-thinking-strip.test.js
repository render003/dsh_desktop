'use strict';

// 单测：ds-empty-thinking-strip（官方 DeepSeek Messages 路由空 <thinking> 拦截补丁）
//
// 覆盖：
//   1. transform 在真实 dsh-llm-deepseek 产物上应用成功、幂等（changed → already）与锚点漂移拒改；
//   2. 入站流翻译 translate() 在逐字符/多片流式到达下：
//      - 纯空 `<thinking> </thinking>` / `<think></think>` / 多组空标签在 tool_use 前被彻底丢弃，
//        不占块序号、不破坏 reasoning signature 与 replayState 1:1 索引对齐；
//      - 前导空标签 + 真实正文（`<thinking> </thinking>\nREPORT.md 已收尾完毕`）只剥离前导空标签，
//        真实正文完整保留；
//      - 正常文本、HTML（`<div>`）、非空 `<thinking>真实思考</thinking>`、正文中间提及 `<thinking> </thinking>`
//        均原样零误伤；
//      - 整轮 end_turn 仅含空思考标签时抛 EMPTY_RESPONSE（触发 Harness 自动重试）；
//   3. 出站历史回放 assistant() 对已落盘的污染历史同样剔除纯空思考标签块，保留后续 reasoning signature 索引对齐。

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const {
  transformDsEmptyThinkingStrip,
  MARKER,
  TARGET_REL,
} = require('../patch-ds-empty-thinking-strip');
const { transformDsToolSchemaSanitize } = require('../patch-ds-tool-schema-sanitize');

const DESKTOP_ROOT = path.resolve(__dirname, '..', '..');
const LIVE_TARGET = path.join(DESKTOP_ROOT, 'node_modules', TARGET_REL);

function loadPatchedRuntime() {
  let src = fs.readFileSync(LIVE_TARGET, 'utf8');
  // 确保 ds-tool-schema-sanitize 辅助函数存在（无论测试前是否已跑 patch-deps）
  const rTool = transformDsToolSchemaSanitize(src, LIVE_TARGET);
  if (rTool.status === 'changed') src = rTool.src;
  const rEmpty = transformDsEmptyThinkingStrip(src, LIVE_TARGET);
  if (rEmpty.status === 'changed') src = rEmpty.src;

  const replayStart = src.indexOf('//#region lib/types/replay.js');
  const serializeEnd = src.indexOf('function serialize(options, connection');
  const streamStart = src.indexOf('//#region lib/types/translate.js');
  const streamEnd = src.indexOf('//#region lib/types/adapter.js');
  assert.ok(replayStart >= 0 && serializeEnd > replayStart, '无法定位 replay/assistant 代码区');
  assert.ok(streamStart >= 0 && streamEnd > streamStart, '无法定位 translate 代码区');

  class FakeLlmError extends Error {
    constructor(msg, code) {
      super(msg);
      this.code = code;
    }
  }
  const helperCode = [
    src.slice(replayStart, serializeEnd),
    src.slice(streamStart, streamEnd),
    'module.exports = { assistant, validateReplay, translate, __dshDsStripEmptyThinking, __dshDsIsEmptyThinkingPrefix };',
  ].join('\n');

  const mod = { exports: {} };
  vm.runInNewContext(helperCode, {
    module: mod,
    exports: mod.exports,
    LlmError: FakeLlmError,
    ToolCallId: (s) => s,
    Map,
    Set,
    Array,
    Object,
    Number,
    String,
    JSON,
    Error,
  });
  return { ...mod.exports, FakeLlmError };
}

async function* asAsync(list) {
  for (const item of list) yield item;
}

async function collect(gen) {
  const out = [];
  for await (const c of gen) out.push(c);
  return out;
}

test('transform：真实靶文件幂等且漂移时 fail-closed 返回 anchor-missing', () => {
  assert.ok(fs.existsSync(LIVE_TARGET), `缺少靶文件: ${LIVE_TARGET}`);
  const live = fs.readFileSync(LIVE_TARGET, 'utf8');
  if (live.includes(MARKER)) {
    const r = transformDsEmptyThinkingStrip(live, LIVE_TARGET);
    assert.equal(r.status, 'already');
  } else {
    const r1 = transformDsEmptyThinkingStrip(live, LIVE_TARGET);
    assert.equal(r1.status, 'changed');
    assert.ok(r1.src.includes(MARKER));
    const r2 = transformDsEmptyThinkingStrip(r1.src, LIVE_TARGET);
    assert.equal(r2.status, 'already');
  }
  const broken = transformDsEmptyThinkingStrip('function unrelated() {}', 'fake.js');
  assert.equal(broken.status, 'anchor-missing');
});

test('入站 translate：纯空 <thinking> </thinking> 在 tool_use 前被彻底丢弃，且不破坏 reasoning signature 与 replayState 对齐', async () => {
  const { translate, validateReplay } = loadPatchedRuntime();
  const events = [
    { type: 'message_start', message: { usage: { input_tokens: 10, output_tokens: 5 } } },
    // 真实结构化 thinking 块（带 signature）
    { type: 'content_block_start', index: 0, content_block: { type: 'thinking', thinking: '' } },
    { type: 'content_block_delta', index: 0, delta: { type: 'thinking_delta', thinking: '先检查文件' } },
    { type: 'content_block_delta', index: 0, delta: { type: 'signature_delta', signature: 'sig-abc-123' } },
    { type: 'content_block_stop', index: 0 },
    // 泄漏到 text 通道的空 <thinking> </thinking>（故意拆碎成多片 delta）
    { type: 'content_block_start', index: 1, content_block: { type: 'text', text: '' } },
    { type: 'content_block_delta', index: 1, delta: { type: 'text_delta', text: '<th' } },
    { type: 'content_block_delta', index: 1, delta: { type: 'text_delta', text: 'inking>' } },
    { type: 'content_block_delta', index: 1, delta: { type: 'text_delta', text: ' ' } },
    { type: 'content_block_delta', index: 1, delta: { type: 'text_delta', text: '</thinking>' } },
    { type: 'content_block_stop', index: 1 },
    // 紧接着的 tool_use
    { type: 'content_block_start', index: 2, content_block: { type: 'tool_use', id: 'call_1', name: 'bash', input: {} } },
    { type: 'content_block_delta', index: 2, delta: { type: 'input_json_delta', partial_json: '{"cmd":"ls"}' } },
    { type: 'content_block_stop', index: 2 },
    { type: 'message_delta', delta: { stop_reason: 'tool_use' }, usage: { output_tokens: 20 } },
    { type: 'message_stop' },
  ];

  const chunks = await collect(translate(asAsync(events), 'deepseek-v4.1-flash-1017'));
  const blockEnds = chunks.filter((c) => c.type === 'block-end');
  assert.equal(blockEnds.length, 2, '空 <thinking> 文本块应被完全丢弃，只剩 reasoning 和 tool-call');
  assert.deepEqual(blockEnds.map((c) => [c.index, c.block.type]), [
    [0, 'reasoning'],
    [1, 'tool-call'],
  ]);
  assert.equal(chunks.some((c) => c.type === 'text-delta'), false, '不应发出任何 text-delta');

  const finish = chunks.find((c) => c.type === 'finish');
  assert.equal(finish.replayState.blocks.length, 2);
  assert.deepEqual(JSON.parse(JSON.stringify(finish.replayState.blocks)), [
    { type: 'reasoning', signature: 'sig-abc-123' },
    { type: 'tool-call' },
  ]);

  // 用真实 validateReplay 核验持久化后的消息与 replayState 严格对齐
  const validated = validateReplay(
    {
      source: { kind: 'model', model: 'deepseek-v4.1-flash-1017', replayState: finish.replayState },
      content: blockEnds.map((c) => c.block),
    },
    'deepseek-v4.1-flash-1017',
  );
  assert.deepEqual(
    JSON.parse(JSON.stringify(validated)),
    JSON.parse(JSON.stringify(finish.replayState.blocks)),
  );
});

test('入站 translate：前导空标签 + 真实正文只剥离前导空标签，非空标签/HTML/正文内提及全部原样保留', async () => {
  const { translate } = loadPatchedRuntime();

  const runText = async (deltas) => {
    const events = [
      { type: 'message_start', message: { usage: { input_tokens: 1, output_tokens: 1 } } },
      { type: 'content_block_start', index: 0, content_block: { type: 'text', text: '' } },
      ...deltas.map((text) => ({ type: 'content_block_delta', index: 0, delta: { type: 'text_delta', text } })),
      { type: 'content_block_stop', index: 0 },
      { type: 'message_delta', delta: { stop_reason: 'end_turn' } },
      { type: 'message_stop' },
    ];
    const chunks = await collect(translate(asAsync(events), 'deepseek-v4.1-flash-1017'));
    const end = chunks.find((c) => c.type === 'block-end');
    const streamed = chunks.filter((c) => c.type === 'text-delta').map((c) => c.text).join('');
    return { finalText: end.block.text, streamed };
  };

  // 前导空标签 + 正文
  const r1 = await runText(['<thinking>', ' ', '</thinking>\n', '<think></think>\n', 'REPORT.md 已收尾完毕']);
  assert.equal(r1.finalText, 'REPORT.md 已收尾完毕');
  assert.equal(r1.streamed, 'REPORT.md 已收尾完毕');

  // 非空 <thinking>实内容</thinking> 原样保留
  const r2 = await runText(['<thinking>', '保留内容', '</thinking>']);
  assert.equal(r2.finalText, '<thinking>保留内容</thinking>');
  assert.equal(r2.streamed, '<thinking>保留内容</thinking>');

  // HTML 标签不误伤
  const r3 = await runText(['<', 'div>hello</div>']);
  assert.equal(r3.finalText, '<div>hello</div>');
  assert.equal(r3.streamed, '<div>hello</div>');

  // 正文中间提及 <thinking> </thinking> 不误伤
  const r4 = await runText(['说明：模型会输出 <thinking> </thinking> 标签']);
  assert.equal(r4.finalText, '说明：模型会输出 <thinking> </thinking> 标签');
  assert.equal(r4.streamed, '说明：模型会输出 <thinking> </thinking> 标签');

  // 孤立短前缀 "<" 结束时不丢失
  const r5 = await runText(['<']);
  assert.equal(r5.finalText, '<');
  assert.equal(r5.streamed, '<');
});

test('入站 translate：整轮 end_turn 仅吐出空 <thinking> </thinking> 时抛 EMPTY_RESPONSE 以触发自动重试', async () => {
  const { translate } = loadPatchedRuntime();
  const events = [
    { type: 'message_start', message: { usage: { input_tokens: 1, output_tokens: 1 } } },
    { type: 'content_block_start', index: 0, content_block: { type: 'text', text: '' } },
    { type: 'content_block_delta', index: 0, delta: { type: 'text_delta', text: '<thinking> </thinking>' } },
    { type: 'content_block_stop', index: 0 },
    { type: 'message_delta', delta: { stop_reason: 'end_turn' } },
    { type: 'message_stop' },
  ];
  await assert.rejects(
    () => collect(translate(asAsync(events), 'deepseek-v4.1-flash-1017')),
    (err) => err && err.code === 'EMPTY_RESPONSE',
  );
});

test('出站 assistant() 回放：已污染历史中的空 <thinking> </thinking> 块被剔除，且保留后续 reasoning signature', () => {
  const { assistant } = loadPatchedRuntime();
  const model = 'deepseek-v4.1-flash-1017';
  const msg = {
    source: {
      kind: 'model',
      model,
      replayState: {
        response: { kind: 'deepseek-messages', version: 1, model },
        blocks: [
          { type: 'text' },
          { type: 'reasoning', signature: 'sig-keep-me' },
          { type: 'text' },
          { type: 'tool-call' },
        ],
      },
    },
    content: [
      { type: 'text', text: '<thinking> </thinking>' },
      { type: 'reasoning', text: '思考过程' },
      { type: 'text', text: '<thinking> </thinking>\n真正回复' },
      { type: 'tool-call', id: 'call_2', name: 'read_file', arguments: '{"path":"a.md"}' },
    ],
  };

  let degraded = null;
  const wire = assistant(msg, model, (reason) => { degraded = reason; });
  assert.equal(degraded, null, 'replayState 校验不应降级');
  assert.deepEqual(JSON.parse(JSON.stringify(wire)), [
    { type: 'thinking', thinking: '思考过程', signature: 'sig-keep-me' },
    { type: 'text', text: '真正回复' },
    { type: 'tool_use', id: 'call_2', name: 'read_file', input: { path: 'a.md' } },
  ]);
});
