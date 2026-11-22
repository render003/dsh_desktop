'use strict';

// 单测：移除反馈功能（remove-feedback-ui）
// 验证：dsh-client-ui-message-feedback 的 apply(ctx) 提前返回（不提供 feedbackUi，
// 顶栏「···」菜单里的「反馈」项随之消失），补丁幂等且漂移 fail-closed。

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const { transformRemoveFeedbackUi, markers } = require('../lib/patch-adapters');
const { UI_MESSAGE_FEEDBACK_PKG_REL } = require('../lib/patch-target-resolver');

const DESKTOP_ROOT = path.resolve(__dirname, '..', '..');
const FEEDBACK_LIVE = path.join(DESKTOP_ROOT, 'node_modules', '@deepseek-ai', UI_MESSAGE_FEEDBACK_PKG_REL);

test('remove-feedback-ui：在真实 dsh-client-ui-message-feedback 上生效、幂等且漂移 fail-closed', () => {
  assert.ok(fs.existsSync(FEEDBACK_LIVE), `缺少靶文件: ${FEEDBACK_LIVE}`);
  const src = fs.readFileSync(FEEDBACK_LIVE, 'utf8');
  let patched = src;
  if (src.includes(markers.REMOVE_FEEDBACK_UI_MARKER)) {
    const r = transformRemoveFeedbackUi(src, FEEDBACK_LIVE);
    assert.equal(r.status, 'already');
  } else {
    const r1 = transformRemoveFeedbackUi(src, FEEDBACK_LIVE);
    assert.equal(r1.status, 'changed');
    patched = r1.src;
    const r2 = transformRemoveFeedbackUi(patched, FEEDBACK_LIVE);
    assert.equal(r2.status, 'already');
  }

  assert.ok(patched.includes('function apply(ctx){/* ' + markers.REMOVE_FEEDBACK_UI_MARKER + ' */return;'));

  const bad = transformRemoveFeedbackUi('/* unrelated */', 'fake.js');
  assert.equal(bad.status, 'anchor-missing');
});
