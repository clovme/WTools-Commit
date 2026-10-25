// commit.exe —— WTools 主仓库与子模块批量拉取 / 提交工具
//
// 用法：
//   commit pull   拉取主仓库并初始化/同步全部子模块
//   commit push   逐个提交并推送子模块，再提交推送主仓库（无需提交信息）
//
// 约定：
//   * 以 exe 自身所在目录作为仓库根目录（可放在 WTools 根目录直接运行）
//   * 不按远程地址猜测分支：优先当前所在分支；detached HEAD 时依次尝试
//     origin/HEAD 指向的远程默认分支、main / master 中实际存在的那一个
//   * 提交信息自动生成：git add 后取暂存区 name-status 摘要截取；取不到时固定为 "^Q^"
//   * push 会自动把落后于远程的子模块安全快进到最新，主仓库随之记录新指针；
//     仅当本地与远程分叉、无法快进时才中止；本地有未提交改动时 pull 会提示先 push

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, process};

// ---------- Windows 控制台 ----------

#[cfg(windows)]
unsafe extern "system" {
    fn SetConsoleOutputCP(code_page_id: u32) -> i32;
    fn SetConsoleCP(code_page_id: u32) -> i32;
}

#[cfg(windows)]
fn setup_console() {
    unsafe {
        SetConsoleOutputCP(65001);
        SetConsoleCP(65001);
    }
}

#[cfg(not(windows))]
fn setup_console() {}

// ---------- 输出辅助 ----------

fn die(msg: &str) -> ! {
    eprintln!("[x] {msg}");
    eprintln!("    操作已中止，未对仓库做任何推送。");
    process::exit(1);
}

fn warn(msg: &str) {
    eprintln!("[!] {msg}");
}

// ---------- git 封装 ----------

fn git_command(repo: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(repo)
        .arg("-c")
        .arg("core.quotepath=false")
        // 强制 HTTP/1.1：部分网络/代理下 HTTP/2 长连接会在传输数百 MB 后被
        // 中途重置（schannel: server closed abruptly / curl 56），HTTP/1.1 更稳。
        .arg("-c")
        .arg("http.version=HTTP/1.1")
        // 网络近乎卡死（连续 60 秒平均速度低于 1KB/s）时主动失败，
        // 避免 clone/fetch/push 无限挂起；正常慢速下载不受影响。
        .arg("-c")
        .arg("http.lowSpeedLimit=1000")
        .arg("-c")
        .arg("http.lowSpeedTime=60")
        .args(args);
    cmd
}

/// 执行 git 并捕获 stdout（用于查询类命令）。返回 (是否成功, 去空白后的 stdout)。
fn git_capture(repo: &Path, args: &[&str]) -> (bool, String) {
    match git_command(repo, args).output() {
        Ok(output) => (
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
        ),
        Err(e) => (false, e.to_string()),
    }
}

/// 执行 git，stdout/stderr 直接继承到当前控制台（用于 clone / fetch / push 等需要展示进度的命令）。
fn git_run(repo: &Path, args: &[&str]) -> bool {
    match git_command(repo, args).status() {
        Ok(status) => status.success(),
        Err(e) => {
            warn(&format!("无法启动 git：{e}"));
            false
        }
    }
}

/// 运行 `git submodule update --init`，弱网下优先浅克隆、失败再全量兜底。
/// 大子模块的完整历史常达数百 MB，弱网传输时易在中途被重置（RPC failed /
/// early EOF / curl 56）；`--depth 1` 只取最新提交，数据量小、成功率高。
/// 主仓库锁定的就是最新提交时浅克隆即可；若锁定的是旧提交（不在最新历史中），
/// 浅克隆会失败，再自动退化为完整克隆。extra 用于追加范围参数（如 push 单个
/// 子模块时的 `-- <path>`）。重跑时已完成的子模块会被跳过。
fn submodule_update_init(root: &Path, extra: &[&str]) -> bool {
    const BASE: [&str; 5] =
        ["submodule", "update", "--init", "--recursive", "--progress"];

    // 1) 浅克隆（--depth 1），最多两次以应对偶发网络抖动。
    for attempt in 1..=2 {
        let mut args = BASE.to_vec();
        args.extend_from_slice(&["--depth", "1"]);
        args.extend_from_slice(extra);
        if git_run(root, &args) {
            return true;
        }
        if attempt < 2 {
            println!("\n[!] 浅克隆可能因网络中断未完成，正在重试（第 1 次）...\n");
        }
    }

    // 2) 浅克隆失败：可能锁定的是旧提交，退化为完整克隆再试。
    println!("\n[!] 浅克隆未能完成（主仓库锁定的可能是较早提交），改用完整克隆...\n");
    for attempt in 1..=2 {
        let mut args = BASE.to_vec();
        args.extend_from_slice(extra);
        if git_run(root, &args) {
            return true;
        }
        if attempt < 2 {
            println!("\n[!] 完整克隆可能因网络中断未完成，正在重试（第 1 次）...\n");
        }
    }
    false
}

fn origin_url(repo: &Path) -> String {
    let (ok, url) = git_capture(repo, &["remote", "get-url", "origin"]);
    if !ok || url.is_empty() {
        die(&format!(
            "在 {} 中无法获取 origin 远程地址，确认这是一个 git 仓库。",
            repo.display()
        ));
    }
    url
}

/// 查询远程默认分支：读取 origin/HEAD 符号引用，返回如 "main" / "master"。
/// clone 出来的仓库通常已带有该引用；调用方可先用 `git remote set-head origin --auto`
/// 校正。读取失败时返回 None。
fn remote_default_branch(repo: &Path) -> Option<String> {
    let (ok, out) = git_capture(
        repo,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    );
    if ok {
        if let Some(name) = out.strip_prefix("origin/") {
            let name = name.trim();
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// 让本地 origin/HEAD 与远程实际默认分支对齐。失败不致命（仅影响 detached HEAD
/// 情况下的回退探测），静默执行。
fn refresh_remote_head(repo: &Path) {
    git_capture(repo, &["remote", "set-head", "origin", "--auto"]);
}

/// 候选主干分支名：现代仓库多用 main，旧仓库多用 master。
const COMMON_MAIN_BRANCHES: &[&str] = &["main", "master"];

/// 探测 main / master 中实际存在的那个分支（每个仓库只有一个分支时使用）。
/// 先看远程跟踪引用 origin/<name>（fetch 后即最新），再看本地分支 <name>。
/// 例：origin/master 存在而 origin/main 不存在时，返回 "master"。
fn existing_main_branch(repo: &Path) -> Option<String> {
    for name in COMMON_MAIN_BRANCHES {
        if rev_parse(repo, &format!("origin/{name}")).is_some() {
            return Some((*name).to_string());
        }
    }
    for name in COMMON_MAIN_BRANCHES {
        if rev_parse(repo, &format!("refs/heads/{name}")).is_some() {
            return Some((*name).to_string());
        }
    }
    None
}

/// 确定要操作的分支：
/// 1. 当前所在分支（最优先，不打断用户工作状态）；
/// 2. detached HEAD 时读 origin/HEAD 指向的远程默认分支（clone 时通常自带）；
/// 3. origin/HEAD 缺失时，按 main / master 谁存在用谁；
/// 4. 最后再 `git remote set-head origin --auto` 向远程校正一次。
fn resolve_branch(repo: &Path) -> Option<String> {
    current_branch(repo)
        .or_else(|| remote_default_branch(repo))
        .or_else(|| existing_main_branch(repo))
        .or_else(|| {
            refresh_remote_head(repo);
            remote_default_branch(repo)
        })
}

fn has_changes(repo: &Path) -> bool {
    let (ok, out) = git_capture(repo, &["status", "--porcelain"]);
    ok && !out.is_empty()
}

/// 取不到暂存摘要时使用的固定提交信息。
const DEFAULT_COMMIT_MSG: &str = "^Q^";

/// 生成提交信息：调用方需先完成 `git add -A`，本函数读取暂存区
/// name-status 摘要（如 "M commit.exe; A Background"）并截断；
/// 暂存区为空或命令失败时回退到固定信息 "^Q^"。
fn staged_message(repo: &Path) -> String {
    const LIMIT: usize = 10;
    let (ok, out) = git_capture(repo, &["diff", "--cached", "--name-status"]);
    let mut entries: Vec<String> = Vec::new();
    if ok {
        for line in out.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            // 形如 "M\tpath"、"A\tpath"、"R100\told\tnew"，压成单行。
            entries.push(line.split_whitespace().collect::<Vec<_>>().join(" "));
        }
    }
    if entries.is_empty() {
        return DEFAULT_COMMIT_MSG.to_string();
    }
    let total = entries.len();
    entries.truncate(LIMIT);
    let mut msg = entries.join("; ");
    if total > LIMIT {
        msg.push_str(&format!("; ...共 {total} 项"));
    }
    msg
}

fn current_branch(repo: &Path) -> Option<String> {
    let (ok, out) = git_capture(repo, &["symbolic-ref", "--short", "HEAD"]);
    if ok {
        Some(out)
    } else {
        None
    }
}

fn rev_parse(repo: &Path, spec: &str) -> Option<String> {
    let (ok, out) = git_capture(repo, &["rev-parse", spec]);
    if ok {
        Some(out)
    } else {
        None
    }
}

/// 计算 <left>..<right> 范围内右侧领先的提交数。
fn ahead_count(repo: &Path, left: &str, right: &str) -> i64 {
    let range = format!("{left}..{right}");
    let (ok, out) = git_capture(repo, &["rev-list", "--count", &range]);
    if ok {
        out.parse().unwrap_or(0)
    } else {
        0
    }
}

// ---------- 子模块 ----------

struct Submodule {
    path: String,
}

/// 解析 .gitmodules，按出现顺序收集子模块 path。
fn parse_submodules(root: &Path) -> Vec<Submodule> {
    let file = root.join(".gitmodules");
    let content = match fs::read_to_string(&file) {
        Ok(text) => text,
        Err(e) => die(&format!("读取 {} 失败：{e}", file.display())),
    };

    let mut result: Vec<String> = Vec::new();
    let mut path: Option<String> = None;

    let flush = |path: &mut Option<String>, result: &mut Vec<String>| {
        if let Some(p) = path.take() {
            result.push(p);
        }
    };

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            flush(&mut path, &mut result);
        } else if let Some((key, value)) = trimmed.split_once('=') {
            if key.trim() == "path" {
                path = Some(value.trim().to_string());
            }
        }
    }
    flush(&mut path, &mut result);

    if result.is_empty() {
        die(".gitmodules 中没有解析到任何子模块。");
    }
    result.into_iter().map(|path| Submodule { path }).collect()
}

/// 子模块已初始化的标志：其目录下存在 .git 文件（或目录）。
fn is_initialized(root: &Path, sub_path: &str) -> bool {
    root.join(sub_path).join(".git").exists()
}

/// 确保子模块附着在跟踪分支上（初始化后通常处于 detached HEAD），返回分支名。
///
/// 已附着在某分支上时直接使用当前分支；detached HEAD 时跟随远程默认分支
/// （origin/HEAD，或 main / master 中实际存在的那一个）。这里不判断远程是否
/// 领先——远程领先时的自动拉齐统一交给 fast_forward_to_origin，避免在
/// “子模块已推送、主仓库指针尚未更新”的常见场景下直接中止。
fn attach_to_branch(root: &Path, sub: &Submodule) -> String {
    let sub_repo = root.join(&sub.path);

    // 已附着在分支上：不猜测、不切换，直接使用当前分支。
    if let Some(b) = current_branch(&sub_repo) {
        return b;
    }

    // detached HEAD：跟随远程默认分支。
    let branch = resolve_branch(&sub_repo).unwrap_or_else(|| {
        die(&format!(
            "子模块 {} 处于 detached HEAD，且无法确定其远程默认分支（origin/HEAD 不可用）。",
            sub.path
        ))
    });
    println!("  切换到分支 {branch}");
    if !git_run(&sub_repo, &["checkout", &branch]) {
        die(&format!(
            "子模块 {} 无法切换到 {branch}，可能存在本地冲突改动。",
            sub.path
        ));
    }
    branch
}

/// 将子模块当前分支快进到 origin/<branch>，成功返回 true。
///
/// 已最新时是无害的 no-op；本地纯落后（远程领先）时安全快进，使子模块工作区
/// 到达远程最新，主仓库随后即可记录到新的子模块指针。浅克隆可能因历史不足而
/// 判断不了能否快进，此时先加深历史再重试。只有本地与远程真正分叉（各有独立
/// 提交、无法线性快进）时才返回 false，由调用方中止并提示人工处理。
fn fast_forward_to_origin(repo: &Path, branch: &str) -> bool {
    let remote = format!("origin/{branch}");
    if git_run(repo, &["merge", "--ff-only", &remote]) {
        return true;
    }

    let (_, shallow) = git_capture(repo, &["rev-parse", "--is-shallow-repository"]);
    if shallow.trim() == "true" {
        warn("快进失败，可能是浅克隆历史不足，正在加深历史后重试...");
        if git_run(repo, &["fetch", "--progress", "--deepen", "100", "origin"])
            && git_run(repo, &["merge", "--ff-only", &remote])
        {
            return true;
        }
    }
    false
}

// ---------- 仓库定位 ----------

/// 以 exe 所在目录为仓库根；开发调试时回退到当前工作目录。
fn repo_root() -> PathBuf {
    if let Ok(exe) = env::current_exe() {
        if let Some(dir) = exe.parent() {
            if dir.join(".git").exists() {
                return dir.to_path_buf();
            }
        }
    }
    match env::current_dir() {
        Ok(dir) if dir.join(".git").exists() => dir,
        _ => die("未找到 git 仓库：请将 commit.exe 放在 WTools 根目录（含 .git 的目录）。"),
    }
}

// ---------- push ----------

fn cmd_push(root: &Path) {
    println!("================ WTools 推送 ================");
    let main_url = origin_url(root);
    // 不按远程地址猜测分支：当前所在分支即操作分支；detached HEAD 时
    // 回退到 origin/HEAD 指向的远程默认分支。
    let main_branch = resolve_branch(root).unwrap_or_else(|| {
        die("无法确定主仓库分支：当前为 detached HEAD，且 origin/HEAD 指向的远程默认分支不可用。")
    });
    println!("仓库 : {}", root.display());
    println!("远程 : {main_url}");
    println!("分支 : {main_branch}");
    println!("============================================");

    // 主仓库前置检查
    println!("[主仓库] 检查远程更新...");
    if !git_run(root, &["fetch", "--progress", "origin"]) {
        die("主仓库 git fetch 失败，请检查网络或远程地址。");
    }
    // detached HEAD 时附着到远程默认分支；已在分支上则直接使用，不做切换。
    if current_branch(root).is_none() {
        println!("主仓库处于 detached HEAD，切换到分支 {main_branch}");
        if !git_run(root, &["checkout", &main_branch]) {
            die(&format!("主仓库无法切换到分支 {main_branch}。"));
        }
    }
    if ahead_count(root, "HEAD", &format!("origin/{main_branch}")) > 0 {
        die("远程主仓库存在本地没有的新提交，请先执行 commit pull 后再推送。");
    }

    let submodules = parse_submodules(root);
    let mut changed: Vec<String> = Vec::new();

    for sub in &submodules {
        println!("\n[子模块] {}", sub.path);

        if !is_initialized(root, &sub.path) {
            println!("  尚未初始化，开始 clone（首次较慢，下方会显示下载进度）...");
            if !submodule_update_init(root, &["--", sub.path.as_str()]) {
                die(&format!(
                    "子模块 {} 初始化失败（已重试仍失败），请检查网络后重试。",
                    sub.path
                ));
            }
        }

        let sub_repo = root.join(&sub.path);
        println!("  拉取远程引用...");
        if !git_run(&sub_repo, &["fetch", "--progress", "origin"]) {
            die(&format!("子模块 {} git fetch 失败。", sub.path));
        }
        println!("  远程引用已更新。");

        // 附着到跟踪分支（首次初始化后处于 detached HEAD）。
        let branch = attach_to_branch(root, sub);

        // 先提交本地工作区改动（若有），随后再统一拉齐远程，避免在旧代码上提交。
        let mut local_committed = false;
        if has_changes(&sub_repo) {
            println!("  发现改动，提交中...");
            if !git_run(&sub_repo, &["add", "-A"]) {
                die(&format!("子模块 {} git add 失败。", sub.path));
            }
            let msg = staged_message(&sub_repo);
            println!("  提交信息：{msg}");
            if !git_run(&sub_repo, &["commit", "-m", &msg]) {
                die(&format!("子模块 {} git commit 失败。", sub.path));
            }
            local_committed = true;
        }

        // 远程领先时自动安全快进到最新（远程新提交会通过主仓库更新指针一并纳入）；
        // 仅当本地与远程分叉、无法线性快进时才中止。
        if !fast_forward_to_origin(&sub_repo, &branch) {
            die(&format!(
                "子模块 {} 本地与 origin/{branch} 已分叉，无法自动快进，请先手动合并/变基后再 commit push。",
                sub.path
            ));
        }

        if local_committed {
            println!("  推送到 origin/{branch}...");
            if !git_run(&sub_repo, &["push", "--progress", "origin", &branch]) {
                die(&format!("子模块 {} push 失败。", sub.path));
            }
            changed.push(sub.path.clone());
            println!("  [OK] {} 已推送", sub.path);
        } else {
            println!("  已与 origin/{branch} 对齐。");
        }
    }

    // 主仓库提交（子模块指针变更 / commit.exe / 根目录文件等）
    println!("\n[主仓库] 检查待提交内容...");
    if has_changes(root) {
        if !git_run(root, &["add", "-A"]) {
            die("主仓库 git add 失败。");
        }
        let msg = staged_message(root);
        println!("  提交信息：{msg}");
        if !git_run(root, &["commit", "-m", &msg]) {
            die("主仓库 git commit 失败。");
        }
        if !git_run(root, &["push", "--progress", "origin", &main_branch]) {
            die("主仓库 push 失败。");
        }
        println!("\n[OK] 全部推送完成。");
    } else if !changed.is_empty() {
        warn("子模块已推送，但主仓库未检测到指针变更，主仓库无需提交。");
        println!("[OK] 子模块推送完成。");
    } else {
        println!("主仓库与全部子模块均无改动，已是最新。");
    }
}

// ---------- pull ----------

fn cmd_pull(root: &Path) {
    println!("================ WTools 拉取 ================");
    let main_url = origin_url(root);
    // 不按远程地址猜测分支：当前所在分支即操作分支；detached HEAD 时
    // 回退到 origin/HEAD 指向的远程默认分支。
    let main_branch = resolve_branch(root).unwrap_or_else(|| {
        die("无法确定主仓库分支：当前为 detached HEAD，且 origin/HEAD 指向的远程默认分支不可用。")
    });
    println!("仓库 : {}", root.display());
    println!("远程 : {main_url}");
    println!("分支 : {main_branch}");
    println!("============================================");

    // 本地改动检查，避免拉取产生冲突
    if has_changes(root) {
        die("主仓库存在未提交改动，请先执行 commit push 后再拉取。");
    }
    let submodules = parse_submodules(root);
    for sub in &submodules {
        if is_initialized(root, &sub.path) {
            let sub_repo = root.join(&sub.path);
            if has_changes(&sub_repo) {
                die(&format!(
                    "子模块 {} 存在未提交改动，请先执行 commit push 后再拉取。",
                    sub.path
                ));
            }
        }
    }

    // 主仓库快进拉取
    println!("[主仓库] 拉取远程更新...");
    if !git_run(root, &["fetch", "--progress", "origin"]) {
        die("主仓库 git fetch 失败，请检查网络或远程地址。");
    }
    // detached HEAD 时附着到远程默认分支再拉取；已在分支上则直接使用。
    if current_branch(root).is_none() {
        println!("主仓库处于 detached HEAD，切换到分支 {main_branch}");
        if !git_run(root, &["checkout", &main_branch]) {
            die(&format!("主仓库无法切换到分支 {main_branch}。"));
        }
    }
    if !git_run(root, &["merge", "--ff-only", &format!("origin/{main_branch}")]) {
        die("主仓库无法快进合并（本地与远程可能已分叉），请先处理本地提交或手动排查。");
    }

    // 初始化并同步全部子模块到主仓库锁定的提交
    println!("\n[子模块] 初始化 / 同步全部子模块（首次会 clone，较慢，下方显示下载进度）...");
    if !submodule_update_init(root, &[]) {
        die("子模块同步失败（已重试仍失败），请检查网络后重新执行 commit pull。");
    }

    // 同步后子模块处于 detached HEAD（锁定提交），需要附着回跟踪分支；
    // 若本地分支已存在但停在旧提交，还要快进对齐到 origin/<branch>，
    // 否则子模块文件不会真正更新到最新。
    for sub in &submodules {
        let sub_repo = root.join(&sub.path);
        // 同步后子模块处于 detached HEAD：跟随 origin/HEAD 指向的远程默认分支。
        let branch = match resolve_branch(&sub_repo) {
            Some(b) => b,
            None => {
                warn(&format!(
                    "子模块 {} 无法确定远程默认分支（origin/HEAD 不可用），保持 detached HEAD。",
                    sub.path
                ));
                continue;
            }
        };
        let head = rev_parse(&sub_repo, "HEAD");
        let tip = rev_parse(&sub_repo, &format!("origin/{branch}"));
        if head.is_none() || tip.is_none() {
            warn(&format!("子模块 {} 无法解析提交，保持现状。", sub.path));
            continue;
        }
        if head != tip {
            warn(&format!(
                "子模块 {} 锁定提交与 origin/{branch} 不一致，保持 detached HEAD，请手动确认。",
                sub.path
            ));
            continue;
        }
        if !git_run(&sub_repo, &["checkout", &branch]) {
            warn(&format!(
                "子模块 {} 未能切换到 {branch}，保持 detached HEAD。",
                sub.path
            ));
            continue;
        }
        if ahead_count(&sub_repo, &branch, &format!("origin/{branch}")) > 0
            && !git_run(
                &sub_repo,
                &["merge", "--ff-only", &format!("origin/{branch}")],
            )
        {
            warn(&format!(
                "子模块 {} 本地分支无法快进到 origin/{branch}，请手动确认。",
                sub.path
            ));
        }
    }

    println!("\n[OK] 主仓库与全部子模块拉取完成。");
}

// ---------- 入口 ----------

fn print_usage() {
    println!(
        "WTools 主仓库 / 子模块批量工具\n\n\
         用法:\n  \
         commit pull   拉取主仓库并初始化/同步全部子模块\n  \
         commit push   逐个提交并推送子模块，再提交并推送主仓库\n\n\
         说明:\n  \
         - 请将 commit.exe 放在 WTools 根目录（与 .gitmodules 同级）\n  \
         - 分支策略：使用当前所在分支；detached HEAD 时跟随 origin/HEAD，或在 main / master 中选择实际存在的分支\n  \
         - push 无需提交信息：自动取 add 后的暂存摘要，取不到时固定为 ^Q^"
    );
}

fn main() {
    setup_console();

    let args: Vec<String> = env::args().collect();
    let root = repo_root();

    match args.get(1).map(|s| s.as_str()) {
        Some("push") => cmd_push(&root),
        Some("pull") => cmd_pull(&root),
        _ => {
            print_usage();
            process::exit(1);
        }
    }
}
