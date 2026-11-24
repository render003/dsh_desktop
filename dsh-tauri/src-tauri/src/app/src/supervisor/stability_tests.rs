//! supervisor 端到端稳定性回归（沙箱 home + 真实 sidecar）。

use super::*;

fn repo_root() -> Option<std::path::PathBuf> {
    let mut dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for _ in 0..6 {
        if dir.join("dsh-desktop").join("vendor").join("node").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

fn sandbox(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("dsh-tauri-wf-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// 伴随插件入口文件被写坏（用户磁盘坏块/更新中断的真实形态）：
/// boot 链 sync 重新同步应覆盖修复 → 瀑布首层即应就绪。
#[test]
fn broken_companion_file_is_healed_by_sync() {
    let Some(root) = repo_root() else { eprintln!("[skip] 无依赖环境"); return; };
    let _env = crate::logging::ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let home = sandbox("broken");
    std::env::set_var("DSH_HOME", &home);
    std::env::set_var("DSH_TAURI_USERDATA", home.join("ud"));
    // 1) 建档。
    let sv0: Arc<Supervisor> = Arc::new(Supervisor::new(&root));
    let (tx0, rx0) = std::sync::mpsc::channel();
    sv0.run_sidecar_boot(&tx0, 0).expect("基线 boot");
    drop(rx0);
    // 2) 破坏一个伴随插件入口（写语法垃圾）。目标件必须仍在册——原用 dsh-auto-compact，
    // 该件已于 2026-11-08 按用户要求退役（源目录删除），换用仍在册的 dsh-change-review。
    let victim = home.join("profiles").join("web").join("node_modules").join("dsh-change-review");
    assert!(victim.exists(), "伴随插件应已同步：{}", victim.display());
    let entry = victim.join("lib").join("index.js");
    if !entry.exists() {
        for cand in ["index.js", "main.js"] {
            if victim.join(cand).exists() {
                drop(entry);
                let _ = std::fs::write(victim.join(cand), "this is ( not valid javascript !!!");
                break;
            }
        }
    } else {
        std::fs::write(&entry, "this is ( not valid javascript !!!").unwrap();
    }
    // 3) 完整守护瀑布：期望依然 KernelReady（sync 修复坏文件）。
    let sv: Arc<Supervisor> = Arc::new(Supervisor::new(&root));
    let (tx, rx) = std::sync::mpsc::channel();
    sv.spawn_boot(tx, None);
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        let left = deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(1));
        match rx.recv_timeout(left) {
            Ok(SupervisorEvent::BootStep { ok, name, .. }) => assert!(ok, "boot 步骤 {name} 失败"),
            Ok(SupervisorEvent::KernelReady { url, .. }) => {
                assert!(url.starts_with("http://127.0.0.1:"), "{url}");
                sv.shutdown();
                std::env::remove_var("DSH_HOME");
                std::env::remove_var("DSH_TAURI_USERDATA");
                let _ = std::fs::remove_dir_all(&home);
                return; // PASS：坏插件被自愈，dsh 照常打开
            }
            Ok(other) => panic!("非预期事件: {other:?}"),
            Err(_) => panic!("180s 内未就绪（坏插件未被自愈）"),
        }
    }
}

/// 配置类破坏（patch 非法内容 + 可回滚快照在场）：瀑布应回滚后救回。
#[test]
fn corrupted_patch_is_rolled_back_to_lastgood() {
    let Some(root) = repo_root() else { eprintln!("[skip]"); return; };
    let _env = crate::logging::ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let home = sandbox("rollback");
    std::env::set_var("DSH_HOME", &home);
    std::env::set_var("DSH_TAURI_USERDATA", home.join("ud"));
    let sv: Arc<Supervisor> = Arc::new(Supervisor::new(&root));
    // 1) 建档 + 落定 lastgood 快照。
    let (tx0, rx0) = std::sync::mpsc::channel();
    sv.run_sidecar_boot(&tx0, 0).expect("基线 boot");
    drop(rx0);
    let snap = sv.guard_cli_json(&["guard-snapshot", "baseline"])
        .and_then(|v| v.get("id").and_then(|i| i.as_str()).map(String::from))
        .expect("快照");
    let _ = sv.guard_cli_json(&["guard-mark-good", &snap]);
    // 2) 破坏 package.json（bundles 数组换成非法形态——repair 修不了、restore 能回滚）。
    let pkg = home.join("profiles").join("web").join("package.json");
    std::fs::write(&pkg, "{ this is not json !!!").unwrap();
    // 3) 完整瀑布：boot 链 repair 先修 package.json（integration heal 有 manifest 修复），
    //    即便修复失败也有 restore 层兜底——两路最终都应 KernelReady。
    let (tx, rx) = std::sync::mpsc::channel();
    sv.spawn_boot(tx, None);
    let deadline = Instant::now() + Duration::from_secs(240);
    loop {
        let left = deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(1));
        match rx.recv_timeout(left) {
            Ok(SupervisorEvent::BootStep { name, ok, .. }) => {
                let _ = (name, ok); // boot 步骤在自愈中可能告警，最终以就绪判
            }
            Ok(SupervisorEvent::KernelReady { url, .. }) => {
                assert!(url.starts_with("http://127.0.0.1:"));
                sv.shutdown();
                std::env::remove_var("DSH_HOME");
                std::env::remove_var("DSH_TAURI_USERDATA");
                let _ = std::fs::remove_dir_all(&home);
                return; // PASS：配置破坏被自愈，dsh 照常打开
            }
            Ok(SupervisorEvent::CrashLoop { .. }) => panic!("瀑布未能救回配置破坏"),
            Ok(other) => { let _ = other; }
            Err(_) => panic!("240s 内未就绪（配置破坏未被自愈）"),
        }
    }
}

/// V16-G1 迁移收口：插件保护中心交互面（guard:action）读面——
/// guard_status/guard_check/guard_incident_read/guard_resolve_incident 经
/// supervisor guard_cli_json（node sidecar guard-*）读守护瀑布已落盘的
/// 快照/事故/lastGood。写动作仍走守护瀑布自动面，本测试只验读面与轻量解。
#[test]
fn guard_action_read_surface_status_check_incident_resolve() {
    let Some(root) = repo_root() else { eprintln!("[skip] 无依赖环境"); return; };
    let _env = crate::logging::ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let home = sandbox("guard-read");
    std::env::set_var("DSH_HOME", &home);
    std::env::set_var("DSH_TAURI_USERDATA", home.join("ud"));
    let sv: Arc<Supervisor> = Arc::new(Supervisor::new(&root));
    // 建档（sidecar boot 建立 web profile，快照才有文件可拍）。
    let (tx, rx) = std::sync::mpsc::channel();
    sv.run_sidecar_boot(&tx, 0).expect("基线 boot");
    drop(rx);
    // 快照 + mark-good（模拟守护瀑布已落定的 lastGood）。
    let snap = sv.guard_cli_json(&["guard-snapshot", "baseline"])
        .and_then(|v| v.get("id").and_then(|i| i.as_str()).map(String::from))
        .expect("快照 id");
    let _ = sv.guard_cli_json(&["guard-mark-good", &snap]);
    // 事故报告（读面输入；guard-incident 的返回体 file 为嵌套对象，改用 status 取 id）。
    let _ = sv.guard_cli_json(&["guard-incident", "test", "hello"]);

    // ① status：快照/事故/lastGood 三者齐备。
    let status = sv.guard_status().expect("guard-status 应可读");
    assert_eq!(status.get("ok").and_then(|v| v.as_bool()), Some(true), "status ok: {status}");
    let snaps = status.get("snapshots").and_then(|v| v.as_array()).expect("snapshots 数组");
    assert!(snaps.iter().any(|s| s.get("id").and_then(|i| i.as_str()) == Some(snap.as_str())), "快照应在列表: {snaps:?}");
    let lg = status.get("lastGood").and_then(|v| v.get("id")).and_then(|i| i.as_str());
    assert_eq!(lg, Some(snap.as_str()), "lastGood 应指向标记快照");
    let incidents = status.get("incidents").and_then(|v| v.as_array()).expect("incidents 数组");
    assert!(!incidents.is_empty(), "事故应已落盘: {incidents:?}");
    let inc_id = incidents[0].get("id").and_then(|x| x.as_str()).expect("事故 id").to_string();

    // ② check：体检 findings（只读，不执行修复）。
    let check = sv.guard_check().expect("guard-health 应可读");
    assert_eq!(check.get("ok").and_then(|v| v.as_bool()), Some(true), "check ok: {check}");
    assert!(check.get("findings").and_then(|v| v.as_array()).is_some(), "check 应带 findings 数组: {check}");

    // ③ incident：读详情。
    let detail = sv.guard_incident_read(&inc_id).expect("guard-read-incident 应可读");
    assert_eq!(detail.get("ok").and_then(|v| v.as_bool()), Some(true), "incident ok: {detail}");
    assert!(detail.get("content").and_then(|v| v.as_str()).is_some_and(|c| !c.is_empty()), "事故详情非空");

    // ④ resolve-incident：解决后 status 的 incidents 不再含该 id。
    let resolved = sv.guard_resolve_incident(&inc_id).expect("guard-resolve-incident 应可读");
    assert_eq!(resolved.get("ok").and_then(|v| v.as_bool()), Some(true), "resolve ok: {resolved}");
    let status2 = sv.guard_status().expect("guard-status 二次应可读");
    let incidents2 = status2.get("incidents").and_then(|v| v.as_array()).expect("incidents 数组");
    assert!(!incidents2.iter().any(|i| i.get("id").and_then(|x| x.as_str()) == Some(inc_id.as_str())), "已解决事故不得再出现在列表: {incidents2:?}");

    // ⑤ 非法 id 的读/解不得 panic（sidecar 返回 {ok:false}）。
    let bad = sv.guard_incident_read("../../etc/passwd").expect("非法 id 也应返回 JSON");
    assert_eq!(bad.get("ok").and_then(|v| v.as_bool()), Some(false), "非法 id 应 ok:false: {bad}");

    sv.shutdown();
    std::env::remove_var("DSH_HOME");
    std::env::remove_var("DSH_TAURI_USERDATA");
    let _ = std::fs::remove_dir_all(&home);
}
