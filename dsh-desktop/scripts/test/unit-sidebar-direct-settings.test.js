'use strict';

// 单测：左侧栏底部直接放置「设置」按钮（sidebar-direct-settings）
// 验证：
//   1. dsh-client-ui-settings-account 的 AccountMenu 移除「更多」弹出菜单及其中的
//      「意见反馈」「登录/退出」项，直接渲染「设置」按钮（点击直接 openSettings()）。
//   2. 补丁幂等且锚点漂移时返回 anchor-missing；按钮携带 aria-label 与装饰图标 aria-hidden。

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const { transformSidebarDirectSettings, markers } = require('../lib/patch-adapters');
const { UI_SETTINGS_ACCOUNT_PKG_REL } = require('../lib/patch-target-resolver');

const DESKTOP_ROOT = path.resolve(__dirname, '..', '..');
const ACCOUNT_LIVE = path.join(DESKTOP_ROOT, 'node_modules', '@deepseek-ai', UI_SETTINGS_ACCOUNT_PKG_REL);

test('sidebar-direct-settings：在真实 dsh-client-ui-settings-account 上生效、幂等且漂移 fail-closed', () => {
  assert.ok(fs.existsSync(ACCOUNT_LIVE), `缺少靶文件: ${ACCOUNT_LIVE}`);
  const src = fs.readFileSync(ACCOUNT_LIVE, 'utf8');
  let patched = src;
  if (src.includes(markers.SIDEBAR_DIRECT_SETTINGS_MARKER)) {
    const r = transformSidebarDirectSettings(src, ACCOUNT_LIVE);
    assert.equal(r.status, 'already');
  } else {
    const r1 = transformSidebarDirectSettings(src, ACCOUNT_LIVE);
    assert.equal(r1.status, 'changed');
    patched = r1.src;
    const r2 = transformSidebarDirectSettings(patched, ACCOUNT_LIVE);
    assert.equal(r2.status, 'already');
  }

  assert.ok(patched.includes(markers.SIDEBAR_DIRECT_SETTINGS_MARKER));
  assert.ok(patched.includes('"aria-label":t(`settings`)'));
  assert.ok(patched.includes('IconSettingsOutlineMedium,{size:16,"aria-hidden":true}'));
  assert.ok(patched.includes('onClick:()=>{openSettings()}'));
  assert.ok(!patched.includes('id:`contact`,label:t(`contactUs`)'), '不应再渲染意见反馈菜单项');
  assert.ok(!patched.includes('id:`signin`,label:t(`signIn`)'), '不应再渲染登录菜单项');
  assert.ok(!patched.includes('children:signedIn?label:t(`more`)'), '不应再渲染「更多」触发文案');

  const bad = transformSidebarDirectSettings('/* unrelated */', 'fake.js');
  assert.equal(bad.status, 'anchor-missing');
});
