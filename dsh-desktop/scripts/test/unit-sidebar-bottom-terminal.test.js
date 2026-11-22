'use strict';

// 单测：底部面板增加终端功能（sidebar-bottom-terminal + dsh-better-sidebar bottomOnly 守卫）
// 验证：
//   1. dsh-client-ui-sidebar-terminal 在 betterSidebar 就绪时向底部工作台注册 bottomOnly
//      的 `terminal` tab，并将底部终端纳入 webTerminals.retainTabs 同步；幂等且漂移 fail-closed。
//   2. dsh-better-sidebar 的原生右侧栏同步循环跳过 `descriptor.bottomOnly === true`，避免重复注册。

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const { transformSidebarBottomTerminal, markers } = require('../lib/patch-adapters');
const { UI_SIDEBAR_TERMINAL_PKG_REL } = require('../lib/patch-target-resolver');

const DESKTOP_ROOT = path.resolve(__dirname, '..', '..');
const TERMINAL_LIVE = path.join(DESKTOP_ROOT, 'node_modules', '@deepseek-ai', UI_SIDEBAR_TERMINAL_PKG_REL);

test('sidebar-bottom-terminal：在真实 dsh-client-ui-sidebar-terminal 上生效、幂等且漂移 fail-closed', () => {
  assert.ok(fs.existsSync(TERMINAL_LIVE), `缺少靶文件: ${TERMINAL_LIVE}`);
  const src = fs.readFileSync(TERMINAL_LIVE, 'utf8');
  let patched = src;
  if (src.includes(markers.SIDEBAR_BOTTOM_TERMINAL_MARKER)) {
    const r = transformSidebarBottomTerminal(src, TERMINAL_LIVE);
    assert.equal(r.status, 'already');
  } else {
    const r1 = transformSidebarBottomTerminal(src, TERMINAL_LIVE);
    assert.equal(r1.status, 'changed');
    patched = r1.src;
    const r2 = transformSidebarBottomTerminal(patched, TERMINAL_LIVE);
    assert.equal(r2.status, 'already');
  }

  assert.ok(patched.includes(markers.SIDEBAR_BOTTOM_TERMINAL_MARKER));
  assert.ok(patched.includes('...bottomTabs.values()'));
  assert.ok(patched.includes('ctx.inject([`betterSidebar`]'));
  assert.ok(patched.includes('bottomOnly:true'));
  assert.ok(patched.includes('LazyTerminalBody'));

  const bad = transformSidebarBottomTerminal('/* unrelated */', 'fake.js');
  assert.equal(bad.status, 'anchor-missing');
});

test('dsh-better-sidebar 原生右侧栏跳过 bottomOnly 桌面标签，避免与原生终端重复', () => {
  for (const rel of ['src/client/native/index.ts', 'lib/client.js', 'lib/client-registry.js']) {
    const text = fs.readFileSync(path.join(DESKTOP_ROOT, 'assets', 'plugins', 'dsh-better-sidebar', rel), 'utf8');
    assert.ok(text.includes('descriptor.bottomOnly === true'), `${rel} 应过滤 bottomOnly 描述符`);
  }
});
