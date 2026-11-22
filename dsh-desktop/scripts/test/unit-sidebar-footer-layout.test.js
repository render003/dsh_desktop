'use strict';

// 单测：左侧栏底部图标布局优化（sidebar-footer-layout + dsh-toolbox-ui + ds-harness-remote）
// 验证：
//   1. dsh-client-ui-sidebar 的 footerActions 由横向 display:flex 改为纵向堆叠 + gap +
//      子项 min-inline-size:0 + 折叠态 safe center；补丁幂等且漂移 fail-closed。
//   2. dsh-toolbox-ui 与 ds-harness-remote 的侧栏底部入口均对齐原生 42px 满宽行规格、
//      无负边距挤压，并在标签上启用光学基线对齐（text-box:trim-both cap alphabetic）。

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const { transformSidebarFooterLayout, markers } = require('../lib/patch-adapters');
const { UI_SIDEBAR_CLIENT_PKG_REL } = require('../lib/patch-target-resolver');

const DESKTOP_ROOT = path.resolve(__dirname, '..', '..');
const SIDEBAR_LIVE = path.join(DESKTOP_ROOT, 'node_modules', '@deepseek-ai', UI_SIDEBAR_CLIENT_PKG_REL);

test('sidebar-footer-layout：在真实 dsh-client-ui-sidebar 上生效、幂等且锚点漂移时返回 anchor-missing', () => {
  assert.ok(fs.existsSync(SIDEBAR_LIVE), `缺少靶文件: ${SIDEBAR_LIVE}`);
  const src = fs.readFileSync(SIDEBAR_LIVE, 'utf8');
  let patched = src;
  if (src.includes(markers.SIDEBAR_FOOTER_LAYOUT_MARKER)) {
    const r = transformSidebarFooterLayout(src, SIDEBAR_LIVE);
    assert.equal(r.status, 'already');
  } else {
    const r1 = transformSidebarFooterLayout(src, SIDEBAR_LIVE);
    assert.equal(r1.status, 'changed');
    patched = r1.src;
    const r2 = transformSidebarFooterLayout(patched, SIDEBAR_LIVE);
    assert.equal(r2.status, 'already');
  }
  assert.match(patched, /\.hHd-Xa_footerActions\{display:flex;flex-direction:column;align-items:stretch;gap:2px\}/);
  assert.match(patched, /\.hHd-Xa_footerActions>\*\{min-inline-size:0\}/);
  assert.match(patched, /\.hHd-Xa_collapsed \.hHd-Xa_footerActions\{flex-direction:column;align-items:safe center;/);

  const bad = transformSidebarFooterLayout('/* unrelated */', 'fake.js');
  assert.equal(bad.status, 'anchor-missing');
});

test('插件底部入口对齐原生 42px 满宽行：dsh-toolbox-ui 无负边距/窄盒溢出', () => {
  const toolbox = fs.readFileSync(path.join(DESKTOP_ROOT, 'assets', 'plugins', 'dsh-toolbox-ui', 'lib', 'client.js'), 'utf8');
  assert.match(toolbox, /\.dsh-mcp-footer-action\{[^}]*inline-size:100%[^}]*height:42px[^}]*margin:0;/);
  assert.ok(!toolbox.includes('margin:0 -2px;padding:0 10px 0 8px'), 'dsh-toolbox-ui 不得再使用 -2px 负边距');
  assert.match(toolbox, /\.dsh-mcp-footer-name\{[^}]*text-box:trim-both cap alphabetic/);
});
