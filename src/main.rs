// commit.exe —— WTools 主仓库与子模块批量拉取 / 提交工具
//
// 用法：
//   commit pull           拉取主仓库并初始化/同步全部子模块
//   commit push           逐个提交并推送子模块，再提交推送主仓库（无需提交信息）
//   commit login [用户]   切换/登录 Git 账号（默认取仓库 URL 中的账号名）
//   commit logout [用户]  注销 Git 凭据（不指定用户则注销当前主机的全部 Git 凭据）
//
// 账号策略（零配置，不修改 .gitconfig）：
//   * 账号名从主仓库 origin 与 .gitmodules 各子模块 URL 的路径中自动提取
//     （如 https://github.com/win-tools/xxx.git → win-tools），天然支持按 URL
//     归属识别账号，无需任何配置文件
//   * 直接操作 Windows 凭据管理器中的 git 凭据（GCM 存储），与命令行、小乌龟、
//     IDE 等所有 Git 客户端共用；不读写任何 .gitconfig
//   * 凭据账号与目标账号不一致时，删除旧凭据并调用
//     `git credential-manager github login --browser` 唤起浏览器登录
//   * pull 时若未登录：公开仓库匿名拉取，私有仓库跳过并在最后汇总；
//     push 必须登录，无有效凭据直接中止
//   * 同一主机下的子模块分属多个账号时，单凭据模式无法同时满足，明确报错
//
// 其他约定：
//   * 以 exe 自身所在目录作为仓库根目录（可放在 WTools 根目录直接运行）
//   * 不按远程地址猜测分支：优先当前所在分支；detached HEAD 时依次尝试
//     origin/HEAD 指向的远程默认分支、main / master 中实际存在的那一个
//   * 提交信息自动生成：git add 后取暂存区 name-status 摘要截取；取不到时固定为 "^Q^"
//   * push 会自动把落后于远程的子模块安全快进到最新，主仓库随之记录新指针；
//     仅当本地与远程分叉、无法快进时才中止；本地有未提交改动时 pull 会提示先 push

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;
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

/// 认证相关的命令模式：
/// * Normal：正常使用凭据管理器（可能弹窗）。
/// * NoInteractive：保留凭据管理器，但禁止弹窗/终端提示，用于查询当前生效账号。
/// * Anonymous：在 NoInteractive 基础上清空 credential helper，完全匿名，
///   用于探测公开仓库，既不读取账号也不弹窗。
#[derive(Clone, Copy, PartialEq, Eq)]
enum AuthMode {
    Normal,
    NoInteractive,
    Anonymous,
}

fn git_command(repo: &Path, args: &[&str]) -> Command {
    git_command_opts(repo, args, AuthMode::Normal)
}

/// 构造 git 命令，认证行为由 mode 控制。
fn git_command_opts(repo: &Path, args: &[&str], mode: AuthMode) -> Command {
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
        .arg("http.lowSpeedTime=60");
    if mode != AuthMode::Normal {
        cmd.env("GIT_TERMINAL_PROMPT", "0");
        cmd.env("GCM_INTERACTIVE", "never");
    }
    if mode == AuthMode::Anonymous {
        // 空值会清空此前继承的所有 credential.helper，git 便无凭据可用。
        cmd.arg("-c").arg("credential.helper=");
    }
    cmd.args(args);
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
    git_run_opts(repo, args, AuthMode::Normal)
}

fn git_run_opts(repo: &Path, args: &[&str], mode: AuthMode) -> bool {
    match git_command_opts(repo, args, mode).status() {
        Ok(status) => status.success(),
        Err(e) => {
            warn(&format!("无法启动 git：{e}"));
            false
        }
    }
}

/// 返回某路径所属 git 仓库的根（toplevel）；不在仓库内时返回 None。
fn git_toplevel_of(path: &Path) -> Option<PathBuf> {
    let dir = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()?.to_path_buf()
    };
    let (ok, out) = git_capture(&dir, &["rev-parse", "--show-toplevel"]);
    if !ok || out.is_empty() {
        return None;
    }
    out.lines().next().map(PathBuf::from)
}

/// 运行 `git submodule update --init`，弱网下优先浅克隆、失败再全量兜底。
/// 大子模块的完整历史常达数百 MB，弱网传输时易在中途被重置（RPC failed /
/// early EOF / curl 56）；`--depth 1` 只取最新提交，数据量小、成功率高。
/// 主仓库锁定的就是最新提交时浅克隆即可；若锁定的是旧提交（不在最新历史中），
/// 浅克隆会失败，再自动退化为完整克隆。extra 用于追加范围参数（如
/// 单个子模块时的 `-- <path>`）。Anonymous 模式用于未登录时的匿名拉取。
/// 重跑时已完成的子模块会被跳过。mode 为 Anonymous 时用于未登录的匿名拉取。
fn submodule_update_init(root: &Path, extra: &[&str], mode: AuthMode) -> bool {
    const BASE: [&str; 5] = ["submodule", "update", "--init", "--recursive", "--progress"];

    // 1) 浅克隆（--depth 1），最多两次以应对偶发网络抖动。
    for attempt in 1..=2 {
        let mut args = BASE.to_vec();
        args.extend_from_slice(&["--depth", "1"]);
        args.extend_from_slice(extra);
        if git_run_opts(root, &args, mode) {
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
        if git_run_opts(root, &args, mode) {
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
    if ok && let Some(name) = out.strip_prefix("origin/") {
        let name = name.trim();
        if !name.is_empty() {
            return Some(name.to_string());
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
    if ok { Some(out) } else { None }
}

fn rev_parse(repo: &Path, spec: &str) -> Option<String> {
    let (ok, out) = git_capture(repo, &["rev-parse", spec]);
    if ok { Some(out) } else { None }
}

/// 计算 <left>..<right> 范围内右侧领先的提交数。
fn ahead_count(repo: &Path, left: &str, right: &str) -> i64 {
    let range = format!("{left}..{right}");
    let (ok, out) = git_capture(repo, &["rev-list", "--count", &range]);
    if ok { out.parse().unwrap_or(0) } else { 0 }
}

// ---------- 远程 URL 解析 ----------

/// 从远程 URL 解析 (主机, 账号/owner)，主机统一小写。
/// 支持：
///   https://github.com/win-tools/repo.git
///   http://gitlab.example.com:8080/team/repo.git
///   https://user:token@github.com/owner/repo.git
///   ssh://git@github.com/owner/repo.git
///   git@github.com:owner/repo.git
fn parse_remote(url: &str) -> Option<(String, String)> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }

    // 标准 scheme 形式：[scheme://][userinfo@]host[:port]/owner/repo
    if let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .or_else(|| url.strip_prefix("ssh://"))
        .or_else(|| url.strip_prefix("git://"))
    {
        let rest = rest.strip_prefix("//").unwrap_or(rest);
        let (authority, path) = rest.split_once('/')?;
        // 去掉 userinfo 与端口。
        let host = authority.rsplit('@').next().unwrap_or(authority);
        let host = host.split(':').next().unwrap_or(host);
        let owner = path
            .trim_start_matches('/')
            .split('/')
            .find(|s| !s.is_empty())?;
        if host.is_empty() || owner.is_empty() {
            return None;
        }
        return Some((host.to_lowercase(), owner.to_string()));
    }

    // scp-like 形式：user@host:path
    if let Some(at) = url.find('@') {
        let after = &url[at + 1..];
        let (host_port, path) = after.split_once(':')?;
        let host = host_port.split(':').next().unwrap_or(host_port);
        let owner = path
            .trim_start_matches('/')
            .split('/')
            .find(|s| !s.is_empty())?;
        if host.is_empty() || owner.is_empty() {
            return None;
        }
        return Some((host.to_lowercase(), owner.to_string()));
    }

    None
}

// ---------- Windows 凭据管理 ----------

#[cfg(windows)]
mod cred {
    use std::ptr;

    const CRED_TYPE_GENERIC: u32 = 1;
    const CRED_ENUMERATE_ALL_CREDENTIALS: u32 = 0x1;

    #[repr(C)]
    struct FileTime {
        _low: u32,
        _high: u32,
    }

    /// 对应 Windows CREDENTIALW 结构，只需读取到 user_name，其余按原始布局占位。
    #[repr(C)]
    struct CredentialW {
        _flags: u32,
        _type: u32,
        target_name: *mut u16,
        _comment: *mut u16,
        _last_written: FileTime,
        _blob_size: u32,
        _blob: *mut u8,
        _persist: u32,
        _attr_count: u32,
        _attrs: *mut core::ffi::c_void,
        _target_alias: *mut u16,
        user_name: *mut u16,
    }

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn CredEnumerateW(
            filter: *const u16,
            flags: u32,
            count: *mut u32,
            credentials: *mut *mut *mut CredentialW,
        ) -> i32;
        fn CredFree(buffer: *mut core::ffi::c_void);
        fn CredDeleteW(target: *const u16, cred_type: u32, flags: u32) -> i32;
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(core::iter::once(0u16)).collect()
    }

    unsafe fn read_wide(p: *const u16) -> String {
        unsafe {
            if p.is_null() {
                return String::new();
            }
            let mut len = 0usize;
            while *p.add(len) != 0 {
                len += 1;
            }
            String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
        }
    }

    #[derive(Debug)]
    pub(super) struct CredItem {
        /// 逻辑目标名（已剥离 `LegacyGeneric:target=` 前缀），用于展示与匹配。
        pub(super) target: String,
        /// 枚举返回的原始目标名，作为删除时的兜底。
        pub(super) raw_target: String,
        pub(super) user: String,
    }

    /// 凭据管理器中普通凭据的目标名前缀，枚举时会出现、删除时部分系统需要去掉。
    const LEGACY_PREFIX: &str = "LegacyGeneric:target=";

    /// 判断凭据目标名是否属于 GCM 为指定主机存储的 git 凭据。
    /// GCM 目标名形如 `git:https://github.com`（默认账号）与
    /// `git:https://user@github.com`（命名账号），因此统一按主机名匹配。
    fn target_is_git_host(target: &str, host: &str) -> bool {
        let Some(rest) = target
            .strip_prefix("git:https://")
            .or_else(|| target.strip_prefix("git:http://"))
        else {
            return false;
        };
        let authority = rest.split('/').next().unwrap_or(rest);
        let cred_host = authority.rsplit('@').next().unwrap_or(authority);
        // 带端口时要求完全一致。
        cred_host.eq_ignore_ascii_case(host)
    }

    /// 枚举当前 Windows 用户下、GCM 为该主机保存的全部 git 凭据。
    pub(super) fn list(host: &str) -> Vec<CredItem> {
        let mut out = Vec::new();
        unsafe {
            let mut count = 0u32;
            let mut credentials: *mut *mut CredentialW = ptr::null_mut();
            let ok = CredEnumerateW(
                ptr::null(),
                CRED_ENUMERATE_ALL_CREDENTIALS,
                &mut count,
                &mut credentials,
            );
            if ok == 0 {
                return out;
            }
            for i in 0..count as usize {
                let item = *credentials.add(i);
                if item.is_null() {
                    continue;
                }
                let raw_target = read_wide((*item).target_name);
                let user = read_wide((*item).user_name);
                let target = raw_target
                    .strip_prefix(LEGACY_PREFIX)
                    .unwrap_or(&raw_target)
                    .to_string();
                if target_is_git_host(&target, host) {
                    out.push(CredItem {
                        target,
                        raw_target,
                        user,
                    });
                }
            }
            CredFree(credentials as *mut core::ffi::c_void);
        }
        out
    }

    /// 删除指定目标名的凭据。不同系统对前缀要求不一致，依次尝试
    /// 逻辑名、带前缀名与枚举原始名，任一成功即可。
    pub(super) fn delete(target: &str) -> bool {
        let logical = target.strip_prefix(LEGACY_PREFIX).unwrap_or(target);
        let candidates = [
            logical.to_string(),
            format!("{LEGACY_PREFIX}{logical}"),
            target.to_string(),
        ];
        unsafe {
            candidates.iter().any(|name| {
                let w = wide(name);
                CredDeleteW(w.as_ptr(), CRED_TYPE_GENERIC, 0) != 0
            })
        }
    }

    #[cfg(test)]
    mod cred_tests {
        use super::*;

        #[test]
        fn git_host_matching() {
            assert!(target_is_git_host("git:https://github.com", "github.com"));
            assert!(target_is_git_host(
                "git:https://win-tools@github.com",
                "github.com"
            ));
            assert!(target_is_git_host(
                "git:http://localhost:8080",
                "localhost:8080"
            ));
            assert!(!target_is_git_host("git:https://gitlab.com", "github.com"));
            assert!(!target_is_git_host(
                "MicrosoftAccount:target=SSO_POP_Device",
                "github.com"
            ));
        }
    }
}

#[cfg(not(windows))]
mod cred {
    #[derive(Debug)]
    pub(super) struct CredItem {
        pub(super) target: String,
        pub(super) raw_target: String,
        pub(super) user: String,
    }

    pub(super) fn list(_host: &str) -> Vec<CredItem> {
        Vec::new()
    }

    pub(super) fn delete(_target: &str) -> bool {
        false
    }
}

/// 触发 GCM 的浏览器登录（GitHub 专用）。stdout/stderr 透传到控制台，
/// 用户在浏览器中完成 OAuth 授权。
fn gcm_login(repo: &Path, host: &str, user: &str) -> bool {
    if !host.eq_ignore_ascii_case("github.com") {
        die(&format!(
            "账号 {user} 位于主机 {host}，自动登录目前仅支持 github.com；\
             请先手动完成该主机的 Git 登录（任意 git 操作触发登录窗口）后重试。"
        ));
    }
    println!("即将打开浏览器登录 GitHub 账号 {user}，请在浏览器中完成授权...");
    git_run(
        repo,
        &[
            "credential-manager",
            "github",
            "login",
            "--username",
            user,
            "--browser",
        ],
    )
}

/// 收集主仓库与全部子模块 URL 中出现的 (主机, 账号)，去重后返回。
fn collect_identities(root: &Path, subs: &[Submodule]) -> Vec<(String, String)> {
    let mut result: Vec<(String, String)> = Vec::new();
    let mut push = |url: &str, where_: &str| match parse_remote(url) {
        Some(id) => {
            if !result.iter().any(|x| x == &id) {
                result.push(id);
            }
        }
        None => warn(&format!("{where_} 的远程地址无法解析账号：{url}")),
    };
    push(&origin_url(root), "主仓库");
    for sub in subs {
        push(&sub.url, &format!("子模块 {}", sub.path));
    }
    result
}

/// 同一主机下若出现多个不同账号，单凭据模式无法同时满足，直接中止。
fn assert_single_owner_per_host(identities: &[(String, String)]) {
    let hosts: Vec<String> = identities
        .iter()
        .map(|(h, _)| h.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    for host in hosts {
        let owners: HashSet<&str> = identities
            .iter()
            .filter(|(h, _)| h == &host)
            .map(|(_, o)| o.as_str())
            .collect();
        if owners.len() > 1 {
            let mut list: Vec<&str> = owners.into_iter().collect();
            list.sort();
            die(&format!(
                "主机 {host} 下的仓库分属多个账号（{}），同一时刻凭据管理器只能为一个\
                 账号提供默认凭据，无法批量操作；请将它们拆到不同账号下分别执行。",
                list.join("、")
            ));
        }
    }
}

/// 查询 GCM 当前对该主机的无用户名请求实际返回的账号（即“当前生效账号”）。
/// 禁止交互：不弹窗、不触发浏览器登录；无可用凭据时返回 None。
fn effective_account(root: &Path, host: &str) -> Option<String> {
    let mut child = git_command_opts(root, &["credential", "fill"], AuthMode::NoInteractive)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    {
        let stdin = child.stdin.as_mut()?;
        let _ = stdin.write_all(format!("protocol=https\nhost={host}\n\n").as_bytes());
    }
    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .find_map(|line| line.strip_prefix("username=").map(|s| s.trim().to_string()))
}

/// 注销该主机下所有不属于 keep 的 Git 凭据。
/// github 优先使用 GCM 自带的 logout（清理最彻底），CredDeleteW 作为兜底。
fn remove_other_accounts(root: &Path, host: &str, keep: &str) {
    let others: Vec<cred::CredItem> = cred::list(host)
        .into_iter()
        .filter(|i| !i.user.eq_ignore_ascii_case(keep))
        .collect();
    for item in others {
        println!("  注销其他账号凭据：{}（用户：{}）", item.target, item.user);
        let mut removed = false;
        if host.eq_ignore_ascii_case("github.com")
            && git_run(
                root,
                &["credential-manager", "github", "logout", item.user.as_str()],
            )
        {
            removed = true;
        }
        if !removed && !cred::delete(&item.raw_target) {
            die(&format!(
                "无法删除凭据 {}（用户：{}）。请打开 Windows 凭据管理器手动删除该条\
                 「普通凭据」后重试。",
                item.target, item.user
            ));
        }
    }
}

/// 确保该主机的当前生效账号就是 wanted。
///   * 已生效（含目标账号已存在、注销其他账号后自动生效）→ 直接通过；
///   * 目标账号从未登录 → 调 GCM 唤起浏览器授权；
///   * 登录失败/取消 → 返回 false（pull 可降级匿名，push 必须中止）。
fn ensure_credential(root: &Path, host: &str, wanted: &str) -> bool {
    println!("[凭据] 检查 {host} 的 Git 凭据（目标账号：{wanted}）...");

    let is_wanted = |u: Option<String>| u.is_some_and(|u| u.eq_ignore_ascii_case(wanted));

    // 已生效：顺手清掉其他账号，保证唯一，避免 GCM 在多账号间弹选择窗。
    if is_wanted(effective_account(root, host)) {
        println!("  当前生效账号已是 {wanted}。");
        remove_other_accounts(root, host, wanted);
        return true;
    }

    // 当前生效的是别的账号（或为空）：先注销其他账号。
    remove_other_accounts(root, host, wanted);

    // 目标账号此前登录过（命名凭据仍在）时，注销其他账号后 GCM 会自动改用它，
    // 无需再次浏览器授权。
    if is_wanted(effective_account(root, host)) {
        println!("  [OK] 已切换到账号 {wanted}（复用已有凭据）。");
        return true;
    }

    println!("  未发现账号 {wanted} 的凭据，需要登录。");
    if !gcm_login(root, host, wanted) {
        warn("Git 登录未完成（失败或被取消）。");
        return false;
    }
    if is_wanted(effective_account(root, host)) {
        println!("  [OK] 账号 {wanted} 的凭据已就绪。");
        true
    } else {
        warn("登录命令已结束，但未能确认目标账号凭据，请稍后重试或手动登录。");
        false
    }
}

/// 逐主机确保凭据；全部就绪返回 true，任一失败返回 false。
fn ensure_credentials(root: &Path, identities: &[(String, String)]) -> bool {
    let mut ok = true;
    for (host, owner) in identities {
        if !ensure_credential(root, host, owner) {
            ok = false;
        }
    }
    ok
}

// ---------- 匿名访问探测 ----------

#[derive(PartialEq, Eq)]
enum Access {
    /// 公开仓库，匿名可访问。
    Public,
    /// 需要认证（私有仓库，匿名被拒绝）。
    Private,
    /// 网络错误或无法判断。
    Unreachable,
}

/// 匿名探测远程仓库的可访问性：清空 credential helper、禁用一切交互后
/// 执行 ls-remote。公开仓库成功；私有仓库返回认证错误；其余视为网络故障。
fn probe_access(repo: &Path, url: &str) -> Access {
    match git_command_opts(repo, &["ls-remote", "--heads", url], AuthMode::Anonymous).output() {
        Ok(output) if output.status.success() => Access::Public,
        Ok(output) => {
            let err = String::from_utf8_lossy(&output.stderr).to_lowercase();
            const NET_KEYS: &[&str] = &[
                "unable to access",
                "could not resolve host",
                "failed to connect",
                "connection timed out",
                "connection was reset",
                "network is unreachable",
                "operation timed out",
                "early eof",
                "rpc failed",
            ];
            const AUTH_KEYS: &[&str] = &[
                "could not read username",
                "authentication failed",
                "terminal prompts disabled",
                "invalid username or password",
                "permission denied",
                "access denied",
                "401",
                "403",
            ];
            if NET_KEYS.iter().any(|k| err.contains(k)) {
                Access::Unreachable
            } else if AUTH_KEYS.iter().any(|k| err.contains(k)) {
                Access::Private
            } else {
                // 无法归类时按网络/未知处理，避免把可拉取的仓库误判为私有。
                Access::Unreachable
            }
        }
        Err(_) => Access::Unreachable,
    }
}

// ---------- 子模块 ----------

struct Submodule {
    path: String,
    url: String,
}

/// 解析 .gitmodules，按出现顺序收集子模块 path 与 url。
fn parse_submodules(root: &Path) -> Vec<Submodule> {
    let file = root.join(".gitmodules");
    let content = match fs::read_to_string(&file) {
        Ok(text) => text,
        // 没有 .gitmodules 即没有子模块（pack/unpack 也可用于普通单仓库）。
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => die(&format!("读取 {} 失败：{e}", file.display())),
    };

    struct Raw {
        path: Option<String>,
        url: Option<String>,
    }
    let mut result: Vec<Submodule> = Vec::new();
    let mut cur = Raw {
        path: None,
        url: None,
    };

    let flush = |cur: &mut Raw, result: &mut Vec<Submodule>| {
        if let Some(path) = cur.path.take() {
            let url = cur.url.take().unwrap_or_default();
            result.push(Submodule { path, url });
        } else {
            cur.url = None;
        }
    };

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            flush(&mut cur, &mut result);
        } else if let Some((key, value)) = trimmed.split_once('=') {
            match key.trim() {
                "path" => cur.path = Some(value.trim().to_string()),
                "url" => cur.url = Some(value.trim().to_string()),
                _ => {}
            }
        }
    }
    flush(&mut cur, &mut result);

    if result.is_empty() {
        die(".gitmodules 中没有解析到任何子模块。");
    }
    result
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
    if let Ok(exe) = env::current_exe()
        && let Some(dir) = exe.parent()
        && dir.join(".git").exists()
    {
        return dir.to_path_buf();
    }
    match env::current_dir() {
        Ok(dir) if dir.join(".git").exists() => dir,
        _ => die("未找到 git 仓库：请将 commit.exe 放在 WTools 根目录（含 .git 的目录）。"),
    }
}

/// 打印仓库头部信息并返回 (origin URL, 分支名)。
fn print_header(root: &Path, title: &str) -> (String, String) {
    let url = origin_url(root);
    let branch = resolve_branch(root).unwrap_or_else(|| {
        die("无法确定主仓库分支：当前为 detached HEAD，且 origin/HEAD 指向的远程默认分支不可用。")
    });
    println!("================ {title} ================");
    println!("仓库 : {}", root.display());
    println!("远程 : {url}");
    println!("分支 : {branch}");
    println!("============================================");
    (url, branch)
}

/// detached HEAD 时附着到指定分支。
fn checkout_main_if_detached(root: &Path, branch: &str) {
    if current_branch(root).is_none() {
        println!("主仓库处于 detached HEAD，切换到分支 {branch}");
        if !git_run(root, &["checkout", branch]) {
            die(&format!("主仓库无法切换到分支 {branch}。"));
        }
    }
}

// ---------- push ----------

fn cmd_push(root: &Path) {
    let (_main_url, main_branch) = print_header(root, "WTools 推送");

    // 账号自检：push 必须登录，且同一主机只能有一个账号。
    let submodules = parse_submodules(root);
    let identities = collect_identities(root, &submodules);
    if identities.is_empty() {
        die("未能从主仓库或子模块 URL 中解析出任何账号。");
    }
    assert_single_owner_per_host(&identities);
    if !ensure_credentials(root, &identities) {
        die("Git 账号未登录，无法推送。请先执行 commit login（或按提示完成浏览器登录）后再 push。");
    }

    // 分卷过期检查：源文件比对应分卷新时提醒重新 pack（仅警告，不阻断）。
    warn_stale_packs(root, &submodules);

    // 主仓库前置检查
    println!("[主仓库] 检查远程更新...");
    if !git_run(root, &["fetch", "--progress", "origin"]) {
        die("主仓库 git fetch 失败，请检查网络或远程地址。");
    }
    checkout_main_if_detached(root, &main_branch);
    if ahead_count(root, "HEAD", &format!("origin/{main_branch}")) > 0 {
        die("远程主仓库存在本地没有的新提交，请先执行 commit pull 后再推送。");
    }

    let mut changed: Vec<String> = Vec::new();

    for sub in &submodules {
        println!("\n[子模块] {}", sub.path);

        if !is_initialized(root, &sub.path) {
            println!("  尚未初始化，开始 clone（首次较慢，下方会显示下载进度）...");
            if !submodule_update_init(root, &["--", sub.path.as_str()], AuthMode::Normal) {
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

/// pull 前的本地改动检查，避免拉取产生冲突。
fn check_local_changes_before_pull(root: &Path, subs: &[Submodule]) {
    if has_changes(root) {
        die("主仓库存在未提交改动，请先执行 commit push 后再拉取。");
    }
    for sub in subs {
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
}

/// fetch 主仓库并快进到 origin/<branch>。Anonymous 模式用于未登录时的匿名拉取。
fn fetch_and_ff_main(root: &Path, branch: &str, mode: AuthMode) {
    println!("[主仓库] 拉取远程更新...");
    if !git_run_opts(root, &["fetch", "--progress", "origin"], mode) {
        die("主仓库 git fetch 失败，请检查网络或远程地址。");
    }
    checkout_main_if_detached(root, branch);
    if !git_run_opts(
        root,
        &["merge", "--ff-only", &format!("origin/{branch}")],
        mode,
    ) {
        die("主仓库无法快进合并（本地与远程可能已分叉），请先处理本地提交或手动排查。");
    }
}

/// 子模块同步后附着回跟踪分支（仅处理 allow 中的 path）。
fn attach_submodules_after_pull(root: &Path, subs: &[Submodule], allow: &HashSet<String>) {
    for sub in subs {
        if !allow.contains(&sub.path) {
            continue;
        }
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
}

fn cmd_pull(root: &Path) {
    let (main_url, main_branch) = print_header(root, "WTools 拉取");

    let submodules = parse_submodules(root);
    check_local_changes_before_pull(root, &submodules);

    // 账号自检：失败时降级为匿名模式（公开仓库照拉，私有仓库跳过）。
    let identities = collect_identities(root, &submodules);
    if identities.is_empty() {
        die("未能从主仓库或子模块 URL 中解析出任何账号。");
    }
    assert_single_owner_per_host(&identities);
    let logged_in = ensure_credentials(root, &identities);

    if !logged_in {
        warn("未完成 Git 登录，将以匿名模式拉取：公开仓库正常拉取，私有仓库跳过。");
        cmd_pull_anonymous(root, &main_url, &main_branch, &submodules);
        return;
    }

    // 主仓库快进拉取
    fetch_and_ff_main(root, &main_branch, AuthMode::Normal);

    // 初始化并同步全部子模块到主仓库锁定的提交
    println!("\n[子模块] 初始化 / 同步全部子模块（首次会 clone，较慢，下方显示下载进度）...");
    if !submodule_update_init(root, &[], AuthMode::Normal) {
        die("子模块同步失败（已重试仍失败），请检查网络后重新执行 commit pull。");
    }

    let allow: HashSet<String> = submodules.iter().map(|s| s.path.clone()).collect();
    attach_submodules_after_pull(root, &submodules, &allow);

    println!("\n[OK] 主仓库与全部子模块拉取完成。");
}

/// 未登录降级：主仓库与每个子模块先匿名探测，公开的拉取，私有的跳过。
fn cmd_pull_anonymous(root: &Path, main_url: &str, main_branch: &str, subs: &[Submodule]) {
    // 主仓库匿名探测：主仓库不可达则整个流程无法继续。
    print!("[主仓库] 匿名探测可访问性... ");
    match probe_access(root, main_url) {
        Access::Public => {
            println!("公开仓库");
            fetch_and_ff_main(root, main_branch, AuthMode::Anonymous);
        }
        Access::Private => {
            die("主仓库为私有仓库，匿名模式无法访问。请先执行 commit login 后再 commit pull。");
        }
        Access::Unreachable => {
            die("主仓库匿名访问失败（网络故障或地址不可达），请检查网络后重试。");
        }
    }

    let mut public: HashSet<String> = HashSet::new();
    let mut skipped_private: Vec<String> = Vec::new();
    let mut skipped_failed: Vec<String> = Vec::new();

    println!("\n[子模块] 匿名探测各子模块可访问性...");
    for sub in subs {
        print!("  {} ... ", sub.path);
        match probe_access(root, &sub.url) {
            Access::Public => {
                println!("公开");
                // 无论是否已初始化都要同步：已初始化的子模块也要快进到主仓库锁定的提交。
                if submodule_update_init(root, &["--", sub.path.as_str()], AuthMode::Anonymous) {
                    public.insert(sub.path.clone());
                } else {
                    warn(&format!("子模块 {} 拉取失败（已重试），跳过。", sub.path));
                    skipped_failed.push(sub.path.clone());
                }
            }
            Access::Private => {
                println!("私有，跳过");
                skipped_private.push(sub.path.clone());
            }
            Access::Unreachable => {
                println!("网络/未知故障，跳过");
                skipped_failed.push(sub.path.clone());
            }
        }
    }

    attach_submodules_after_pull(root, subs, &public);

    println!("\n[OK] 匿名拉取完成：{} 个公开子模块已同步。", public.len());
    if !skipped_private.is_empty() {
        warn(&format!(
            "以下 {} 个私有子模块已跳过（登录后执行 commit pull 可拉取）：{}",
            skipped_private.len(),
            skipped_private.join("、")
        ));
    }
    if !skipped_failed.is_empty() {
        warn(&format!(
            "以下 {} 个子模块因网络/失败已跳过，请检查网络后重试：{}",
            skipped_failed.len(),
            skipped_failed.join("、")
        ));
    }
}

// ---------- login / logout ----------

fn cmd_login(root: &Path, user: Option<&str>) {
    let subs = parse_submodules(root);
    let identities = collect_identities(root, &subs);
    if identities.is_empty() {
        die("未能从主仓库或子模块 URL 中解析出任何账号。");
    }
    assert_single_owner_per_host(&identities);

    for (host, owner) in &identities {
        let target = user.unwrap_or(owner);
        if let Some(u) = user
            && u != owner
        {
            warn(&format!(
                "仓库 URL 中的账号是 {owner}，你指定登录的是 {u}，按指定账号执行。"
            ));
        }
        println!("================ 登录 {host} ================");
        // 与 pull/push 相同的确保逻辑：注销其他账号 → 复用已有凭据 / 浏览器授权。
        if !ensure_credential(root, host, target) {
            die("登录失败或被取消。");
        }
    }
}

fn cmd_logout(root: &Path, user: Option<&str>) {
    let subs = parse_submodules(root);
    let identities = collect_identities(root, &subs);
    if identities.is_empty() {
        die("未能从主仓库或子模块 URL 中解析出主机信息。");
    }
    let hosts: Vec<String> = identities
        .iter()
        .map(|(h, _)| h.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();

    for host in hosts {
        println!("================ 注销 {host} ================");
        let items = cred::list(&host);
        let targets: Vec<cred::CredItem> = items
            .into_iter()
            .filter(|i| user.map(|u| i.user.eq_ignore_ascii_case(u)).unwrap_or(true))
            .collect();
        if targets.is_empty() {
            println!("  没有匹配的 Git 凭据，无需注销。");
            continue;
        }

        if host.eq_ignore_ascii_case("github.com") {
            // GCM 按账号注销（一次清掉该账号的默认+命名凭据），去重后逐个执行。
            let mut accounts: Vec<String> = Vec::new();
            for item in &targets {
                if !accounts.iter().any(|u| u.eq_ignore_ascii_case(&item.user)) {
                    accounts.push(item.user.clone());
                }
            }
            for account in accounts {
                if git_run(root, &["credential-manager", "github", "logout", &account]) {
                    println!("  [OK] 已注销账号：{account}");
                } else {
                    // 兜底：直接删除凭据管理器中的条目。
                    let leftover = cred::list(&host).into_iter().find(|i| {
                        i.user.eq_ignore_ascii_case(&account)
                            && targets.iter().any(|t| t.raw_target == i.raw_target)
                    });
                    if let Some(item) = leftover
                        && cred::delete(&item.raw_target)
                    {
                        println!("  [OK] 已注销账号：{account}（凭据管理器删除）");
                        continue;
                    }
                    die(&format!(
                        "无法注销账号 {account}，请在 Windows 凭据管理器中手动删除。"
                    ));
                }
            }
        } else {
            for item in &targets {
                if cred::delete(&item.raw_target) {
                    println!("  [OK] 已注销：{}（用户：{}）", item.target, item.user);
                } else {
                    die(&format!(
                        "无法删除凭据 {}（用户：{}），请在 Windows 凭据管理器中手动删除。",
                        item.target, item.user
                    ));
                }
            }
        }
    }
}

// ---------- 7z 分卷打包 / 解包 ----------
//
// 大文件绕过 GitHub 单文件 100MB 限制的工作流（替代有配额限制的 Git LFS）：
//   * commit pack <文件/目录> [等级]
//       7z 分卷压缩（默认 90MB/卷），分卷放在源文件旁边，命名为
//       `源名[pack].7z.001`、`.002`……；归档内保留相对所属仓库根的路径。
//       随后把源文件写入该仓库 .gitignore 的自动管理块；已跟踪的源文件
//       执行 git rm --cached（保留本地文件）。之后正常 commit push 即可。
//   * commit unpack [--force]
//       扫描主仓库与全部子模块的 `*[pack].7z.001` 主卷，按归档内路径还原。
//
// 不维护任何额外清单：分卷自描述（路径、CRC），未压缩的源文件与分卷
// 用“源文件是否比分卷新”判断是否需要重新 pack。

const PACK_MAIN_SUFFIX: &str = "[pack].7z.001";
const PACK_ARCHIVE_SUFFIX: &str = "[pack].7z";
const PACK_VOLUME_SIZE: &str = "90m";

const IGNORE_BEGIN: &str = "# >>> commit pack (managed) >>>";
const IGNORE_END: &str = "# <<< commit pack (managed) <<<";

/// 由源文件/目录名推导归档基础名：Motrix.exe → Motrix.exe[pack].7z
fn pack_archive_name(source_name: &str) -> String {
    format!("{source_name}{PACK_ARCHIVE_SUFFIX}")
}

/// 由主卷名反推源名：Motrix.exe[pack].7z.001 → Motrix.exe
fn source_name_from_main_volume(volume_name: &str) -> Option<String> {
    volume_name
        .strip_suffix(PACK_MAIN_SUFFIX)
        .map(|s| s.to_string())
}

/// 匹配某源文件全部分卷的文件名前缀：Motrix.exe → Motrix.exe[pack].7z.
fn volume_prefix(source_name: &str) -> String {
    format!("{source_name}[pack].7z.")
}

/// 判断某文件名是否为指定源文件的分卷（前缀匹配且后缀为纯数字）。
fn is_volume_of(source_name: &str, file_name: &str) -> bool {
    match file_name.strip_prefix(&volume_prefix(source_name)) {
        Some(suffix) => !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()),
        None => false,
    }
}

/// 查找 7z.exe：根目录固定位置优先，其次根目录浅层搜索、PATH、常见安装路径。
fn find_7z(root: &Path) -> Option<PathBuf> {
    for fixed in [
        root.join("7z.exe"),
        root.join("7z").join("7z.exe"),
        root.join("System").join("7-Zip").join("7z.exe"),
    ] {
        if fixed.is_file() {
            return Some(fixed);
        }
    }
    if let Some(found) = shallow_find_file(root, "7z.exe", 3) {
        return Some(found);
    }
    if let Some(path_var) = env::var_os("PATH") {
        for dir in env::split_paths(&path_var) {
            let candidate = dir.join("7z.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    for var in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Ok(base) = env::var(var) {
            let candidate = PathBuf::from(base).join("7-Zip").join("7z.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// 在 root 下浅层递归查找指定文件名（不进入 .git），找到第一个即返回。
fn shallow_find_file(root: &Path, want: &str, max_depth: usize) -> Option<PathBuf> {
    fn walk(dir: &Path, want: &str, depth: usize) -> Option<PathBuf> {
        if depth == 0 {
            return None;
        }
        let mut subdirs = Vec::new();
        for entry in fs::read_dir(dir).ok()?.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            if name == ".git" {
                continue;
            }
            if path.is_file() && name == want {
                return Some(path);
            }
            if path.is_dir() {
                subdirs.push(path);
            }
        }
        for sub in subdirs {
            if let Some(found) = walk(&sub, want, depth - 1) {
                return Some(found);
            }
        }
        None
    }
    walk(root, want, max_depth)
}

fn run_7z(sevenz: &Path, cwd: &Path, args: &[String]) -> bool {
    match Command::new(sevenz).current_dir(cwd).args(args).status() {
        Ok(status) => status.success(),
        Err(e) => {
            warn(&format!("无法启动 7z：{e}"));
            false
        }
    }
}

fn capture_7z(sevenz: &Path, cwd: &Path, args: &[String]) -> Option<String> {
    let output = Command::new(sevenz)
        .current_dir(cwd)
        .args(args)
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        None
    }
}

/// 路径规范化为正斜杠（.gitignore 与 7z 归档内路径统一使用 /）。
fn forward_slash(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// 去掉 Windows canonicalize 产生的 `\\?\`（及 UNC 的 `\\?\UNC\`）前缀。
fn clean_verbatim(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path
    }
}

/// 规范化路径：canonicalize 并去掉 `\\?\` 前缀；失败则返回原路径。
fn canonical_path(path: &Path) -> PathBuf {
    path.canonicalize()
        .map(clean_verbatim)
        .unwrap_or_else(|_| path.to_path_buf())
}

/// 列出某源文件对应的所有分卷（按文件名前缀 `源名[pack].7z.NNN`）。
fn existing_volumes(source: &Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    let (Some(parent), Some(name)) = (source.parent(), source.file_name()) else {
        return result;
    };
    let source_name = name.to_string_lossy().to_string();
    let Ok(entries) = fs::read_dir(parent) else {
        return result;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name().to_string_lossy().to_string();
        if is_volume_of(&source_name, &file_name) {
            result.push(entry.path());
        }
    }
    result.sort();
    result
}

fn remove_old_volumes(source: &Path) {
    for volume in existing_volumes(source) {
        let _ = fs::remove_file(volume);
    }
}

/// 幂等更新仓库 .gitignore 的 pack 管理块。
fn ensure_ignore_block(repo: &Path, rule: &str) {
    let file = repo.join(".gitignore");
    let content = fs::read_to_string(&file).unwrap_or_default();
    if content.lines().any(|line| line.trim() == rule) {
        return;
    }

    let new_content = if content.contains(IGNORE_BEGIN) {
        let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
        let insert_at = lines
            .iter()
            .position(|l| l.trim() == IGNORE_END)
            .unwrap_or(lines.len());
        lines.insert(insert_at, rule.to_string());
        lines.join("\n") + "\n"
    } else {
        let mut text = content;
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(IGNORE_BEGIN);
        text.push('\n');
        text.push_str(rule);
        text.push('\n');
        text.push_str(IGNORE_END);
        text.push('\n');
        text
    };

    if let Err(e) = fs::write(&file, new_content) {
        die(&format!("写入 {} 失败：{e}", file.display()));
    }
}

/// 目标是否已被 git 跟踪。
fn is_tracked(repo: &Path, rel: &str) -> bool {
    let (ok, out) = git_capture(repo, &["ls-files", "--error-unmatch", "--", rel]);
    ok && !out.is_empty()
}

fn cmd_pack(root: &Path, target_arg: &str, level_arg: &str) {
    println!("================ WTools 打包 ================");
    let sevenz = find_7z(root).unwrap_or_else(|| {
        die("未找到 7z.exe：请把 7z.exe 放到 WTools 根目录，或安装 7-Zip，或加入 PATH。")
    });
    println!("7z   : {}", sevenz.display());

    let raw = PathBuf::from(target_arg);
    let target = if raw.is_absolute() {
        raw
    } else {
        root.join(&raw)
    };
    if !target.exists() {
        die(&format!("目标不存在：{}", target.display()));
    }
    if target
        .file_name()
        .map(|n| n.to_string_lossy().contains("[pack].7z"))
        .unwrap_or(false)
    {
        die("不能对分卷自身执行 pack。");
    }

    // 归属仓库（主仓库或某个子模块）。
    let target_c = canonical_path(&target);
    let repo = git_toplevel_of(&target_c).unwrap_or_else(|| {
        die(&format!("目标不在任何 git 仓库内：{}", target.display()));
    });
    let repo_c = canonical_path(&repo);
    let root_c = canonical_path(root);
    if !repo_c.starts_with(&root_c) {
        die(&format!(
            "目标所属仓库 {} 不在 WTools 仓库内，拒绝操作。",
            repo_c.display()
        ));
    }
    let rel = target_c.strip_prefix(&repo_c).unwrap().to_path_buf();
    let rel_str = forward_slash(&rel);
    let name = target_c.file_name().unwrap().to_string_lossy().to_string();

    let level: u8 = match level_arg {
        "store" => 1,
        "fast" => 3,
        "normal" => 5,
        "best" => 9,
        other => die(&format!(
            "未知压缩等级 {other}（可选：store/fast/normal/best）。"
        )),
    };
    let level_name = match level {
        1 => "仅存储（已是压缩格式）",
        3 => "快速",
        5 => "标准",
        _ => "极限",
    };

    println!("目标 : {rel_str}");
    println!("仓库 : {}", repo_c.display());
    println!("等级 : {level_name}（mx={level}），分卷 {PACK_VOLUME_SIZE}");
    println!("============================================");

    // 清理旧分卷，避免残留导致解压到错误版本。
    remove_old_volumes(&target_c);

    // 归档放在源文件旁边，基础名 `源名[pack].7z`，7z 自动追加 .001/.002。
    let archive_rel = rel.with_file_name(pack_archive_name(&name));
    let archive_str = forward_slash(&archive_rel);
    let args = vec![
        "a".to_string(),
        "-t7z".to_string(),
        "-m0=lzma2".to_string(),
        format!("-mx={level}"),
        format!("-v{PACK_VOLUME_SIZE}"),
        "-mmt=on".to_string(),
        "-y".to_string(),
        archive_str,
        rel_str.clone(),
    ];
    println!("正在压缩...");
    if !run_7z(&sevenz, &repo_c, &args) {
        die("7z 压缩失败，请检查上方输出。");
    }

    // 源文件加入忽略（已跟踪则先取消跟踪，保留本地文件）。
    let rule = format!("/{rel_str}");
    ensure_ignore_block(&repo_c, &rule);
    if is_tracked(&repo_c, &rel_str) {
        println!("源文件已被跟踪，执行 git rm --cached（本地文件保留）...");
        let mut rm_args = vec!["rm", "--cached", "--quiet"];
        if target_c.is_dir() {
            rm_args.push("-r"); // 目录需要递归取消跟踪
        }
        rm_args.push("--");
        rm_args.push(&rel_str);
        if !git_run(&repo_c, &rm_args) {
            die(&format!(
                "git rm --cached {rel_str} 失败，请手动取消跟踪后再提交。"
            ));
        }
    }

    let volumes = existing_volumes(&target_c);
    let total: u64 = volumes
        .iter()
        .filter_map(|v| fs::metadata(v).ok().map(|m| m.len()))
        .sum();
    println!(
        "\n[OK] 打包完成：{} 个分卷，共 {} MB",
        volumes.len(),
        total / 1024 / 1024
    );
    for v in &volumes {
        println!("  {}", v.display());
    }
    println!("源文件已加入 .gitignore；现在执行 commit push 即可提交分卷。");
}

/// 递归收集指定目录下的 pack 主卷；prune 中的目录（.git 之外的排除项，
/// 如主仓库视角下的各子模块）整目录跳过。
fn scan_pack_mains(dir: &Path, prune: &HashSet<PathBuf>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    fn walk(dir: &Path, prune: &HashSet<PathBuf>, out: &mut Vec<PathBuf>) {
        let canon = canonical_path(dir);
        if prune.contains(&canon) {
            return;
        }
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        let mut subdirs = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let file_name = entry.file_name();
            if file_name == ".git" {
                continue;
            }
            if path.is_file() && file_name.to_string_lossy().ends_with(PACK_MAIN_SUFFIX) {
                out.push(path);
            } else if path.is_dir() {
                subdirs.push(path);
            }
        }
        for sub in subdirs {
            walk(&sub, prune, out);
        }
    }
    walk(dir, prune, &mut out);
    out.sort();
    out
}

/// 列出 7z 归档内的文件路径（相对归档工作目录）。
fn list_archive_entries(sevenz: &Path, cwd: &Path, archive: &Path) -> Vec<String> {
    let args = vec![
        "l".to_string(),
        "-slt".to_string(),
        archive.to_string_lossy().to_string(),
    ];
    let Some(text) = capture_7z(sevenz, cwd, &args) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| line.strip_prefix("Path = "))
        .map(str::trim)
        .filter(|p| !p.is_empty() && !p.contains(PACK_ARCHIVE_SUFFIX))
        .map(|p| forward_slash(Path::new(p)))
        .collect()
}

fn cmd_unpack(root: &Path, force: bool) {
    println!("================ WTools 解包 ================");
    let sevenz = find_7z(root).unwrap_or_else(|| {
        die("未找到 7z.exe：请把 7z.exe 放到 WTools 根目录，或安装 7-Zip，或加入 PATH。")
    });

    let submodules = parse_submodules(root);
    let mut prune = HashSet::new();
    for sub in &submodules {
        if is_initialized(root, &sub.path) {
            prune.insert(canonical_path(&root.join(&sub.path)));
        }
    }

    // 主仓库（排除子模块）+ 各子模块。
    let mut mains = scan_pack_mains(root, &prune);
    for sub in &submodules {
        let p = root.join(&sub.path);
        if is_initialized(root, &sub.path) {
            mains.extend(scan_pack_mains(&p, &HashSet::new()));
        }
    }
    mains.sort();
    mains.dedup();

    if mains.is_empty() {
        println!("未发现任何 [pack] 分卷，无需解包。");
        return;
    }

    let mut restored = 0usize;
    let mut skipped = 0usize;
    for main in &mains {
        let repo = git_toplevel_of(main).unwrap_or_else(|| root.to_path_buf());
        let entries = list_archive_entries(&sevenz, &repo, main);
        if !force && !entries.is_empty() && entries.iter().all(|e| repo.join(e).exists()) {
            let rc = canonical_path(root);
            let display = main
                .strip_prefix(&rc)
                .map(forward_slash)
                .unwrap_or_else(|_| main.display().to_string());
            println!("  已存在，跳过：{display}");
            skipped += 1;
            continue;
        }
        println!("  解压：{}", main.display());
        let mode = if force { "-aoa" } else { "-aos" };
        let out_arg = format!("-o{}", repo.display());
        let args = vec![
            "x".to_string(),
            mode.to_string(),
            "-y".to_string(),
            main.to_string_lossy().to_string(),
            out_arg,
        ];
        if !run_7z(&sevenz, &repo, &args) {
            die(&format!(
                "解压失败：{}（分卷可能不完整或损坏）。",
                main.display()
            ));
        }
        restored += 1;
    }

    println!("\n[OK] 解包完成：解压 {restored} 个，跳过 {skipped} 个已存在的。");
    if !force && skipped > 0 {
        println!("如需强制覆盖已存在的文件，执行 commit unpack --force。");
    }
}

fn fs_mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

/// 目录内是否存在修改时间晚于 than 的文件（忽略 .git）。
fn dir_has_newer_file(dir: &Path, than: SystemTime) -> bool {
    fn walk(dir: &Path, than: SystemTime) -> bool {
        let Ok(entries) = fs::read_dir(dir) else {
            return false;
        };
        let mut subdirs = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.file_name() == Some(std::ffi::OsStr::new(".git")) {
                continue;
            }
            if path.is_file() {
                if let Some(t) = fs_mtime(&path)
                    && t > than
                {
                    return true;
                }
            } else if path.is_dir() {
                subdirs.push(path);
            }
        }
        subdirs.into_iter().any(|s| walk(&s, than))
    }
    walk(dir, than)
}

/// push 前检查分卷是否已过期（源文件比分卷新），仅警告不阻断。
fn warn_stale_packs(root: &Path, submodules: &[Submodule]) {
    let mut prune = HashSet::new();
    for sub in submodules {
        if is_initialized(root, &sub.path) {
            prune.insert(canonical_path(&root.join(&sub.path)));
        }
    }
    let mut mains = scan_pack_mains(root, &prune);
    for sub in submodules {
        if is_initialized(root, &sub.path) {
            mains.extend(scan_pack_mains(&root.join(&sub.path), &HashSet::new()));
        }
    }

    for main in mains {
        let Some(file_name) = main.file_name().map(|n| n.to_string_lossy().to_string()) else {
            continue;
        };
        let Some(source_name) = source_name_from_main_volume(&file_name) else {
            continue;
        };
        let source = main.with_file_name(source_name);
        let Some(volume_time) = fs_mtime(&main) else {
            continue;
        };
        let stale = if source.is_file() {
            fs_mtime(&source).is_some_and(|t| t > volume_time)
        } else if source.is_dir() {
            dir_has_newer_file(&source, volume_time)
        } else {
            false
        };
        if stale {
            warn(&format!(
                "分卷 {} 的源文件已更新，分卷过期；请先执行 commit pack 重新打包再推送。",
                forward_slash(&source)
            ));
        }
    }
}

// ---------- 入口 ----------

fn print_usage() {
    println!(
        "WTools 主仓库 / 子模块批量工具\n\n\
         用法:\n  \
         commit pull           拉取主仓库并初始化/同步全部子模块\n  \
         commit push           逐个提交并推送子模块，再提交并推送主仓库\n  \
         commit pack <路径>    把大文件/目录分卷压缩为 90MB 的 7z 分卷（自动忽略源文件）\n  \
         commit unpack         扫描并还原全部 [pack] 分卷（新克隆后执行）\n  \
         commit login [用户]   切换/登录 Git 账号（默认取仓库 URL 中的账号）\n  \
         commit logout [用户]  注销 Git 凭据（不指定用户则注销全部）\n\n\
         说明:\n  \
         - 请将 commit.exe 放在 WTools 根目录（与 .gitmodules 同级）\n  \
         - 账号从远程 URL 自动识别（如 github.com/win-tools/... → win-tools）\n  \
         - 凭据保存在 Windows 凭据管理器，对命令行 / 小乌龟 / IDE 同时生效，\
         不修改 .gitconfig；账号不一致时会自动注销旧凭据并唤起浏览器登录\n  \
         - pull 未登录时仅拉取公开仓库，私有仓库跳过并汇总；push 必须登录\n  \
         - pack 等级：store（仅存储）/fast（快速）/normal（标准）/best（极限），\
         不填则按类型自动选择；分卷命名为 源名[pack].7z.001，与源文件同目录\n  \
         - push 时若源文件比对应分卷新，会提示重新 pack（不阻断提交）\n  \
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
        Some("pack") => match args.get(2) {
            Some(target) => cmd_pack(&root, target, "best"),
            None => {
                print_usage();
                process::exit(1);
            }
        },
        Some("unpack") => cmd_unpack(&root, true),
        Some("login") => cmd_login(&root, args.get(2).map(|s| s.as_str())),
        Some("logout") => cmd_logout(&root, args.get(2).map(|s| s.as_str())),
        _ => {
            print_usage();
            process::exit(1);
        }
    }
}

// ---------- 单元测试 ----------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_https_remote() {
        assert_eq!(
            parse_remote("https://github.com/win-tools/Software-Commonly.git"),
            Some(("github.com".to_string(), "win-tools".to_string()))
        );
        assert_eq!(
            parse_remote("https://github.com/win-tools/WTools-All"),
            Some(("github.com".to_string(), "win-tools".to_string()))
        );
        assert_eq!(
            parse_remote("http://gitlab.example.com:8080/team/repo.git"),
            Some(("gitlab.example.com".to_string(), "team".to_string()))
        );
        assert_eq!(
            parse_remote("https://user:token@github.com/owner/repo.git"),
            Some(("github.com".to_string(), "owner".to_string()))
        );
        assert_eq!(
            parse_remote("https://github.com/owner/repo/"),
            Some(("github.com".to_string(), "owner".to_string()))
        );
    }

    #[test]
    fn parse_ssh_remote() {
        assert_eq!(
            parse_remote("git@github.com:win-tools/data.git"),
            Some(("github.com".to_string(), "win-tools".to_string()))
        );
        assert_eq!(
            parse_remote("ssh://git@github.com/win-tools/data.git"),
            Some(("github.com".to_string(), "win-tools".to_string()))
        );
        assert_eq!(
            parse_remote("git@gitlab.example.com:team/repo.git"),
            Some(("gitlab.example.com".to_string(), "team".to_string()))
        );
    }

    #[test]
    fn parse_bad_remote() {
        assert_eq!(parse_remote(""), None);
        assert_eq!(parse_remote("not-a-url"), None);
        assert_eq!(parse_remote("https://github.com"), None);
    }

    #[test]
    fn pack_volume_naming_roundtrip() {
        // 文件（含多扩展名）
        assert_eq!(pack_archive_name("Motrix.exe"), "Motrix.exe[pack].7z");
        assert_eq!(
            source_name_from_main_volume("Motrix.exe[pack].7z.001"),
            Some("Motrix.exe".to_string())
        );
        // 目录
        assert_eq!(pack_archive_name("DownKyi"), "DownKyi[pack].7z");
        assert_eq!(
            source_name_from_main_volume("DownKyi[pack].7z.001"),
            Some("DownKyi".to_string())
        );
        // 带空格与中文（主卷固定为 .001）
        assert_eq!(
            source_name_from_main_volume("我的 文件.bin[pack].7z.001"),
            Some("我的 文件.bin".to_string())
        );
        // 非主卷无法反推
        assert_eq!(source_name_from_main_volume("Motrix.exe"), None);
        assert_eq!(source_name_from_main_volume("Motrix.exe[pack].7z"), None);
        assert_eq!(
            source_name_from_main_volume("Motrix.exe[pack].7z.002"),
            None
        );
    }

    #[test]
    fn volume_prefix_matches_only_volumes() {
        assert_eq!(volume_prefix("Motrix.exe"), "Motrix.exe[pack].7z.");
        // 数字后缀才是分卷
        assert!(is_volume_of("Motrix.exe", "Motrix.exe[pack].7z.001"));
        assert!(is_volume_of("Motrix.exe", "Motrix.exe[pack].7z.099"));
        // 非数字后缀、无后缀、别的源文件都不能误匹配
        assert!(!is_volume_of("Motrix.exe", "Motrix.exe[pack].7z.bak"));
        assert!(!is_volume_of("Motrix.exe", "Motrix.exe[pack].7z"));
        assert!(!is_volume_of("Motrix.exe", "Other.exe[pack].7z.001"));
    }

    /// 环境相关的 FFI 冒烟测试：枚举本机凭据，验证目标名解析正确。
    /// 默认跳过（依赖本机已安装凭据），用 --include-ignored 手动运行。
    #[cfg(windows)]
    #[test]
    #[ignore]
    fn list_github_credentials_smoke() {
        let items = cred::list("github.com");
        eprintln!("github credentials = {items:?}");
        for item in &items {
            assert!(!item.target.starts_with("LegacyGeneric"));
            assert!(item.target.contains("github.com"));
            assert!(!item.user.is_empty());
        }
    }
}
