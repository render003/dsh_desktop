//! TA1 属性测试（手写 xorshift 生成器，零新依赖）：对日志脱敏纯函数
//! 契约做 10^4 量级随机对照 + 不变量 + 脏输入不 panic。
//!
//! 覆盖（经 lib.rs 的 cfg(test) 门访问私有 mod 内 pub 契约）：
//! - `logging::scrub_secrets`
//! （原 `commands::updater_client` 三组属性测试已随客户端自更新链裁撤移除，2026-11-22。）

/// xorshift64*：手写确定性 PRNG（零依赖）。
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1) // 种子非零
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n.max(1)
    }
}

// ---------------------------------------------------------------------------
// scrub_secrets：随机注入 × 不变量
// ---------------------------------------------------------------------------

#[test]
fn ta1_scrub_secrets_injected_tokens_removed_10k() {
    let u = crate::logging::scrub_secrets;
    let mut rng = Rng::new(0x5EC1E7);
    let words = ["kernel", "boot", "ok", "ws://127.0.0.1", "port=9", "error:", "hint", "中文日志", "path C:\\x", "2026-08-22T00:00:00Z"];
    let alnum = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let rands = |rng: &mut Rng, n: usize| -> String {
        (0..n).map(|_| alnum[rng.below(alnum.len() as u64) as usize] as char).collect()
    };
    for i in 0..10_000 {
        let word = words[rng.below(words.len() as u64) as usize];
        let prelen = rng.below(12) as usize;
        let prefix = format!("{word} {}", rands(&mut rng, prelen));
        let suflen = rng.below(12) as usize;
        let suffix = format!("{} tail", rands(&mut rng, suflen));
        let toklen = 20 + rng.below(20) as usize; // ≥20 位
        let token = rands(&mut rng, toklen);
        let (line, marker) = match i % 4 {
            0 => (format!("{prefix} sk-{token} {suffix}"), format!("sk-{token}")),
            1 => (format!("{prefix} Bearer {token} {suffix}"), token.clone()),
            2 => (format!("{prefix} Authorization: {token} {suffix}"), token.clone()),
            _ => (format!("{prefix} api_key={token} {suffix}"), token.clone()),
        };
        let out = u(&line);
        assert!(!out.contains(&marker), "原 token 残留: {out:?}（含 {marker:?}）");
        assert!(out.contains("***"), "脱敏必须留痕: {out:?}");
    }
}

#[test]
fn ta1_scrub_secrets_benign_lines_unchanged_10k() {
    let u = crate::logging::scrub_secrets;
    let mut rng = Rng::new(0xBE012);
    // 纯小写字母 + 空格 + 少量符号：形态上不可能命中 sk-/Bearer/Authorization。
    let chars = b"abcdefghijklmnopqrstuvwxyz 0123456789 .:/\\=+[]()#%";
    for _ in 0..10_000 {
        let len = 1 + rng.below(80) as usize;
        let line: String = (0..len).map(|_| chars[rng.below(chars.len() as u64) as usize] as char).collect();
        if line.contains("bearer") || line.contains("authorization") || line.contains("api_key") {
            continue; // 生成器撞形（大小写不敏感命中）→ 排除
        }
        assert_eq!(u(&line), line, "普通日志行被误改: {line:?}");
    }
}

#[test]
fn ta1_scrub_secrets_dirty_never_panics() {
    let u = crate::logging::scrub_secrets;
    let mut rng = Rng::new(0xC0A15);
    for _ in 0..3_000 {
        let len = rng.below(64) as usize;
        let bytes: Vec<u8> = (0..len).map(|_| (rng.below(256)) as u8).collect();
        let line = String::from_utf8_lossy(&bytes).into_owned();
        let _ = u(&line);
    }
    // 边界：空串。
    assert_eq!(u(""), "");
}

/// 【TA1 缺陷锚点——已修复，断言已反转（2026-08-22）】
/// Authorization 值（scheme + 空格 + 凭据复合）改为整行余段打码，
/// Bearer/Basic 裸凭据不再残留。
#[test]
fn ta1_scrub_secrets_known_gap_authorization_bearer_value_survives() {
    let u = crate::logging::scrub_secrets;
    let token = "TfQNQcCsqGnu2hm2a3rdEAtoyag"; // 28 位，非 sk- 前缀
    let out = u(&format!("Authorization: Bearer {token}"));
    assert!(!out.contains(token), "Bearer 裸 token 必须被整行打码（已修复）");
    assert!(out.contains("***"), "打码标记在位");
}
