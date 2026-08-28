//! `cangling-update hostinfo` — write `info.md` next to the binary.

use anyhow::{Context, Result};
use crate::db;
use crate::models::Project;
use crate::paths::AppPaths;
use std::path::{Path, PathBuf};
use std::process::Command;

const UNIT_PATH: &str = "/etc/systemd/system/cangling-update.service";

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Software {
    pub name: String,
    pub version: String,
    pub path: String,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct DiskMount {
    pub mount: String,
    pub filesystem: String,
    pub total: u64,
    pub used: u64,
    pub avail: u64,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct MemInfo {
    pub total: u64,
    pub used: u64,
    pub avail: u64,
    pub swap_total: u64,
    pub swap_used: u64,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct CpuInfo {
    pub arch: String,
    pub model: String,
    pub logical_cpus: u32,
    pub sockets: Option<u32>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct GpuInfo {
    pub name: String,
    pub arch: String,
    pub count: u32,
    pub source: String,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct HostSnapshot {
    pub generated_at: String,
    pub hostname: String,
    pub primary_ip: String,
    pub ips: Vec<String>,
    pub listen: Option<(String, u16)>,
    pub software: Vec<Software>,
    pub projects: Vec<Project>,
    pub disks: Vec<DiskMount>,
    pub mem: MemInfo,
    pub cpu: CpuInfo,
    pub gpus: Vec<GpuInfo>,
    pub exe_dir: String,
    pub config_dir: String,
}

pub fn run(paths: &AppPaths, output: Option<PathBuf>) -> Result<()> {
    let snap = collect(paths)?;
    let md = render_markdown(&snap);
    let dest = output.unwrap_or_else(|| paths.exe_dir.join("info.md"));
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create {}", parent.display()))?;
    }
    std::fs::write(&dest, md).with_context(|| format!("write {}", dest.display()))?;
    println!("{}", dest.display());
    eprintln!("已写入 {}", dest.display());
    if look_path("glow").is_some() {
        eprintln!("查看：glow {}", dest.display());
    }
    Ok(())
}

pub fn collect(paths: &AppPaths) -> Result<HostSnapshot> {
    let projects = match db::open(&paths.db_path) {
        Ok(conn) => db::list_projects(&conn).unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    Ok(collect_with_projects(paths, projects))
}

pub fn collect_with_projects(paths: &AppPaths, projects: Vec<Project>) -> HostSnapshot {
    HostSnapshot {
        generated_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S %z").to_string(),
        hostname: hostname(),
        primary_ip: primary_ip(),
        ips: list_ips(),
        listen: listen_from_unit(),
        software: collect_software(paths),
        projects,
        disks: collect_disks(),
        mem: collect_mem(),
        cpu: collect_cpu(),
        gpus: collect_gpus(),
        exe_dir: paths.exe_dir.display().to_string(),
        config_dir: paths.config_dir.display().to_string(),
    }
}

/// `color` query: missing/`1`/`true`/`always` → ANSI; `0`/`false`/`never` → 纯文本。
pub fn want_color(q: Option<&str>) -> bool {
    match q.map(|s| s.trim().to_ascii_lowercase()) {
        None => true,
        Some(s) if s.is_empty() => true,
        Some(s) => !matches!(s.as_str(), "0" | "false" | "no" | "never" | "off"),
    }
}

struct Paint {
    on: bool,
}

impl Paint {
    fn wrap(&self, code: &str, s: &str) -> String {
        if self.on {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
    fn title(&self, s: &str) -> String {
        self.wrap("1;96", s)
    }
    fn head(&self, s: &str) -> String {
        self.wrap("1;36", s)
    }
    fn key(&self, s: &str) -> String {
        self.wrap("2", s)
    }
    fn val(&self, s: &str) -> String {
        self.wrap("32", s)
    }
    fn warn(&self, s: &str) -> String {
        self.wrap("33", s)
    }
    fn bad(&self, s: &str) -> String {
        self.wrap("31", s)
    }
    fn dim(&self, s: &str) -> String {
        self.wrap("2", s)
    }
    fn rule(&self) -> String {
        self.wrap("2", "────────────────────────────────────────")
    }
}

/// Terminal document for `curl http://host/hostinfo` / `cat`.
pub fn render_ansi(s: &HostSnapshot, color: bool) -> String {
    let p = Paint { on: color };
    let mut out = String::new();
    out.push('\n');
    out.push_str(&format!("  {}\n", p.title("主机信息")));
    out.push_str(&format!("  {}\n\n", p.dim(&format!("生成 {}", s.generated_at))));

    out.push_str(&format!("  {}\n  {}\n", p.head("主机"), p.rule()));
    kv(&mut out, &p, "主机名", &s.hostname);
    kv(&mut out, &p, "主 IP", &dash(&s.primary_ip));
    let ips = if s.ips.is_empty() {
        "—".into()
    } else {
        s.ips.join(", ")
    };
    kv(&mut out, &p, "本机地址", &ips);
    kv(&mut out, &p, "程序目录", &s.exe_dir);
    kv(&mut out, &p, "数据目录", &s.config_dir);
    if let Some((bind, port)) = &s.listen {
        kv(&mut out, &p, "服务监听", &format!("{bind}:{port}"));
    }
    out.push('\n');

    out.push_str(&format!("  {}\n  {}\n", p.head("已装软件"), p.rule()));
    for sw in &s.software {
        let ver = if sw.version.contains("未安装") {
            p.warn(&sw.version)
        } else {
            p.val(&sw.version)
        };
        out.push_str(&format!(
            "  {}  {}  {}\n",
            p.key(&pad_display(&sw.name, 18)),
            ver,
            p.dim(&sw.path)
        ));
    }
    out.push('\n');

    out.push_str(&format!("  {}\n  {}\n", p.head("项目"), p.rule()));
    if s.projects.is_empty() {
        out.push_str(&format!("  {}\n\n", p.dim("暂无登记项目。")));
    } else {
        for proj in &s.projects {
            let ver = match proj.current_version_no {
                Some(n) => format!("v{n}"),
                None => "—".into(),
            };
            out.push_str(&format!(
                "  {}  {}  {}\n",
                p.val(&pad_display(&proj.name, 16)),
                p.key(&ver),
                proj.directory
            ));
            if !proj.description.is_empty() {
                out.push_str(&format!("    {}\n", p.dim(&proj.description)));
            }
        }
        out.push('\n');
    }

    out.push_str(&format!("  {}\n  {}\n", p.head("磁盘"), p.rule()));
    if s.disks.is_empty() {
        out.push_str(&format!("  {}\n\n", p.dim("未能读取磁盘用量。")));
    } else {
        for d in &s.disks {
            let ratio = if d.total == 0 {
                0.0
            } else {
                d.used as f64 / d.total as f64
            };
            let pct = format!("{:.0}%", ratio * 100.0);
            let bar = usage_bar(ratio, 12);
            let bar_c = if ratio >= 0.9 {
                p.bad(&bar)
            } else if ratio >= 0.7 {
                p.warn(&bar)
            } else {
                p.val(&bar)
            };
            out.push_str(&format!(
                "  {}  {}  {}  {} / {}\n",
                p.key(&pad_display(&d.mount, 12)),
                bar_c,
                p.val(&pad_display(&pct, 4)),
                human_bytes(d.avail),
                human_bytes(d.total)
            ));
        }
        out.push('\n');
    }

    out.push_str(&format!("  {}\n  {}\n", p.head("内存"), p.rule()));
    if s.mem.total == 0 {
        out.push_str(&format!("  {}\n\n", p.dim("未能读取内存用量。")));
    } else {
        ansi_mem_row(
            &mut out,
            &p,
            "物理内存",
            s.mem.used,
            s.mem.avail,
            s.mem.total,
        );
        if s.mem.swap_total == 0 {
            kv(&mut out, &p, "交换分区", "未配置");
        } else {
            let swap_avail = s.mem.swap_total.saturating_sub(s.mem.swap_used);
            ansi_mem_row(
                &mut out,
                &p,
                "交换分区",
                s.mem.swap_used,
                swap_avail,
                s.mem.swap_total,
            );
        }
        out.push('\n');
    }

    out.push_str(&format!("  {}\n  {}\n", p.head("CPU"), p.rule()));
    kv(&mut out, &p, "架构", &dash(&s.cpu.arch));
    kv(&mut out, &p, "型号", &dash(&s.cpu.model));
    kv(&mut out, &p, "逻辑核数", &s.cpu.logical_cpus.to_string());
    if let Some(n) = s.cpu.sockets {
        kv(&mut out, &p, "插槽数", &n.to_string());
    }
    out.push('\n');

    out.push_str(&format!("  {}\n  {}\n", p.head("GPU"), p.rule()));
    if s.gpus.is_empty() {
        out.push_str(&format!("  {}\n", p.dim("未检测到 GPU / NPU。")));
    } else {
        for g in &s.gpus {
            out.push_str(&format!(
                "  {}  {}  ×{}  {}\n",
                p.val(&g.name),
                p.key(&dash(&g.arch)),
                g.count,
                p.dim(&g.source)
            ));
        }
    }
    out.push('\n');
    out
}

fn ansi_mem_row(out: &mut String, p: &Paint, label: &str, used: u64, avail: u64, total: u64) {
    let ratio = if total == 0 {
        0.0
    } else {
        used as f64 / total as f64
    };
    let pct = format!("{:.0}%", ratio * 100.0);
    let bar = usage_bar(ratio, 12);
    let bar_c = if ratio >= 0.9 {
        p.bad(&bar)
    } else if ratio >= 0.7 {
        p.warn(&bar)
    } else {
        p.val(&bar)
    };
    out.push_str(&format!(
        "  {}  {}  {}  {} / {}  可用 {}\n",
        p.key(&pad_display(label, 8)),
        bar_c,
        p.val(&pad_display(&pct, 4)),
        human_bytes(used),
        human_bytes(total),
        human_bytes(avail)
    ));
}

fn kv(out: &mut String, p: &Paint, k: &str, v: &str) {
    out.push_str(&format!("  {}  {}\n", p.key(&pad_display(k, 10)), p.val(v)));
}

fn display_width(s: &str) -> usize {
    s.chars()
        .map(|c| {
            let u = c as u32;
            if (0x2E80..=0x9FFF).contains(&u)
                || (0xF900..=0xFAFF).contains(&u)
                || (0xFF00..=0xFF60).contains(&u)
                || (0x3400..=0x4DBF).contains(&u)
                || u >= 0x20000
            {
                2
            } else {
                1
            }
        })
        .sum()
}

fn pad_display(s: &str, width: usize) -> String {
    let w = display_width(s);
    if w >= width {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(width - w))
    }
}

fn usage_bar(ratio: f64, width: usize) -> String {
    let ratio = ratio.clamp(0.0, 1.0);
    let filled = ((ratio * width as f64).round() as usize).min(width);
    format!(
        "[{}{}]",
        "█".repeat(filled),
        "░".repeat(width.saturating_sub(filled))
    )
}

pub fn render_markdown(s: &HostSnapshot) -> String {
    let mut out = String::new();
    out.push_str("# 主机信息\n\n");
    out.push_str(&format!("生成时间：{}\n\n", s.generated_at));

    out.push_str("## 主机\n\n");
    out.push_str("| 项 | 值 |\n|---|---|\n");
    row(&mut out, "主机名", &s.hostname);
    row(&mut out, "主 IP", &dash(&s.primary_ip));
    let ips = if s.ips.is_empty() {
        "—".into()
    } else {
        s.ips.join(", ")
    };
    row(&mut out, "本机地址", &ips);
    row(&mut out, "程序目录", &s.exe_dir);
    row(&mut out, "数据目录", &s.config_dir);
    if let Some((bind, port)) = &s.listen {
        row(&mut out, "服务监听", &format!("{bind}:{port}"));
    }
    out.push('\n');

    out.push_str("## 已装软件\n\n");
    out.push_str("| 软件 | 版本 | 位置 |\n|---|---|---|\n");
    for sw in &s.software {
        out.push_str(&format!(
            "| {} | {} | {} |\n",
            cell(&sw.name),
            cell(&sw.version),
            cell(&sw.path)
        ));
    }
    out.push('\n');

    out.push_str("## 项目\n\n");
    if s.projects.is_empty() {
        out.push_str("暂无登记项目。\n\n");
    } else {
        out.push_str("| 名称 | 目录 | 当前版本 | 说明 |\n|---|---|---|---|\n");
        for p in &s.projects {
            let ver = match p.current_version_no {
                Some(n) => format!("v{n}"),
                None => "—".into(),
            };
            out.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                cell(&p.name),
                cell(&p.directory),
                cell(&ver),
                cell(&p.description)
            ));
        }
        out.push('\n');
    }

    out.push_str("## 磁盘\n\n");
    if s.disks.is_empty() {
        out.push_str("未能读取磁盘用量。\n\n");
    } else {
        out.push_str("| 挂载点 | 文件系统 | 已用 | 可用 | 总计 | 使用率 |\n|---|---|---|---|---|---|\n");
        for d in &s.disks {
            let pct = if d.total == 0 {
                "—".into()
            } else {
                format!("{:.0}%", (d.used as f64 / d.total as f64) * 100.0)
            };
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} |\n",
                cell(&d.mount),
                cell(&d.filesystem),
                human_bytes(d.used),
                human_bytes(d.avail),
                human_bytes(d.total),
                pct
            ));
        }
        out.push('\n');
    }

    out.push_str("## 内存\n\n");
    if s.mem.total == 0 {
        out.push_str("未能读取内存用量。\n\n");
    } else {
        out.push_str("| 项 | 已用 | 可用 | 总计 | 使用率 |\n|---|---|---|---|---|\n");
        let pct = format!("{:.0}%", (s.mem.used as f64 / s.mem.total as f64) * 100.0);
        out.push_str(&format!(
            "| 物理内存 | {} | {} | {} | {} |\n",
            human_bytes(s.mem.used),
            human_bytes(s.mem.avail),
            human_bytes(s.mem.total),
            pct
        ));
        if s.mem.swap_total == 0 {
            out.push_str("| 交换分区 | — | — | 未配置 | — |\n");
        } else {
            let swap_avail = s.mem.swap_total.saturating_sub(s.mem.swap_used);
            let spct = format!(
                "{:.0}%",
                (s.mem.swap_used as f64 / s.mem.swap_total as f64) * 100.0
            );
            out.push_str(&format!(
                "| 交换分区 | {} | {} | {} | {} |\n",
                human_bytes(s.mem.swap_used),
                human_bytes(swap_avail),
                human_bytes(s.mem.swap_total),
                spct
            ));
        }
        out.push('\n');
    }

    out.push_str("## CPU\n\n");
    out.push_str("| 项 | 值 |\n|---|---|\n");
    row(&mut out, "架构", &dash(&s.cpu.arch));
    row(&mut out, "型号", &dash(&s.cpu.model));
    row(&mut out, "逻辑核数", &s.cpu.logical_cpus.to_string());
    if let Some(n) = s.cpu.sockets {
        row(&mut out, "插槽数", &n.to_string());
    }
    out.push('\n');

    out.push_str("## GPU\n\n");
    if s.gpus.is_empty() {
        out.push_str("未检测到 GPU / NPU。\n");
    } else {
        out.push_str("| 名称 | 架构 | 数量 | 来源 |\n|---|---|---|---|\n");
        for g in &s.gpus {
            out.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                cell(&g.name),
                cell(&dash(&g.arch)),
                g.count,
                cell(&g.source)
            ));
        }
    }
    out.push('\n');
    out
}

fn row(out: &mut String, k: &str, v: &str) {
    out.push_str(&format!("| {} | {} |\n", cell(k), cell(v)));
}

fn cell(s: &str) -> String {
    s.replace('|', "\\|")
        .replace('\n', " ")
        .replace('\r', "")
}

fn dash(s: &str) -> String {
    if s.trim().is_empty() {
        "—".into()
    } else {
        s.to_string()
    }
}

pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} {}", UNITS[0])
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

fn collect_software(paths: &AppPaths) -> Vec<Software> {
    let mut list = Vec::new();
    let exe = std::env::current_exe()
        .ok()
        .unwrap_or_else(|| paths.exe_dir.join("cangling-update"));
    let mut self_ver = format!("v{}", env!("CARGO_PKG_VERSION"));
    if let Some((bind, port)) = listen_from_unit() {
        self_ver.push_str(&format!("  监听 {bind}:{port}"));
    }
    list.push(Software {
        name: "cangling-update".into(),
        version: self_ver,
        path: exe.display().to_string(),
    });

    probe(
        &mut list,
        "docker",
        &["docker"],
        &[&["version", "--format", "{{.Client.Version}}"], &["version"]],
    );
    push_compose(&mut list);
    probe(&mut list, "k3s", &["k3s"], &[&["--version"]]);
    probe(
        &mut list,
        "kubectl",
        &["kubectl"],
        &[&["version", "--client", "--short"], &["version", "--client"]],
    );
    probe(
        &mut list,
        "k9s",
        &["k9s"],
        &[&["version", "--short"], &["version"]],
    );
    probe(&mut list, "glow", &["glow"], &[&["--version"]]);
    probe(&mut list, "git", &["git"], &[&["--version"]]);
    probe(&mut list, "containerd", &["containerd"], &[&["--version"]]);
    list
}

fn push_compose(list: &mut Vec<Software>) {
    if let Some(path) = look_path("docker") {
        if let Some(version) = first_ok_version(
            "docker",
            &[&["compose", "version", "--short"], &["compose", "version"]],
        ) {
            list.push(Software {
                name: "docker compose".into(),
                version,
                path: path.display().to_string(),
            });
            return;
        }
    }
    if let Some(path) = look_path("docker-compose") {
        let version = first_ok_version(
            "docker-compose",
            &[&["version", "--short"], &["version"], &["--version"]],
        )
        .unwrap_or_else(|| "已安装（版本未知）".into());
        list.push(Software {
            name: "docker-compose".into(),
            version,
            path: path.display().to_string(),
        });
        return;
    }
    list.push(Software {
        name: "docker compose".into(),
        version: if look_path("docker").is_some() {
            "未检测到插件".into()
        } else {
            "未安装".into()
        },
        path: look_path("docker")
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "—".into()),
    });
}

fn probe(list: &mut Vec<Software>, name: &str, bins: &[&str], attempts: &[&[&str]]) {
    for bin in bins {
        if let Some(path) = look_path(bin) {
            let version = first_ok_version(bin, attempts)
                .unwrap_or_else(|| "已安装（版本未知）".into());
            list.push(Software {
                name: name.into(),
                version,
                path: path.display().to_string(),
            });
            return;
        }
    }
    list.push(Software {
        name: name.into(),
        version: "未安装".into(),
        path: "—".into(),
    });
}

fn first_ok_version(bin: &str, attempts: &[&[&str]]) -> Option<String> {
    for args in attempts {
        if let Some(v) = cmd_out(bin, args).and_then(|s| short_version(&s)) {
            return Some(v);
        }
    }
    None
}

/// Turn command output into a one-line version. Rejects CLI help / error dumps.
fn short_version(raw: &str) -> Option<String> {
    if looks_like_cli_help(raw) {
        return None;
    }
    let line = first_line(raw);
    if line.is_empty() || looks_like_cli_help(&line) {
        return None;
    }
    if let Some(tok) = line
        .split(|c: char| matches!(c, ',' | ' ' | '\t'))
        .map(str::trim)
        .find(|t| is_version_token(t))
    {
        return Some(tok.to_string());
    }
    if line.chars().count() <= 80 {
        Some(line)
    } else {
        None
    }
}

fn is_version_token(t: &str) -> bool {
    let t = t.trim().trim_start_matches(['v', 'V']);
    t.chars().next().is_some_and(|c| c.is_ascii_digit()) && t.contains('.')
}

fn looks_like_cli_help(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    lower.contains("unknown flag")
        || lower.contains("unknown shorthand")
        || lower.contains("is not a docker command")
        || lower.contains("see 'docker")
        || lower.contains("see \"docker")
        || lower.contains("usage:  docker")
        || lower.contains("usage: docker")
        || lower.contains("a self-sufficient runtime")
}

fn look_path(name: &str) -> Option<PathBuf> {
    let p = Path::new(name);
    if p.is_absolute() {
        return p.is_file().then(|| p.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(name);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

fn cmd_out(bin: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(bin).args(args).output().ok()?;
    let mut s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        s = String::from_utf8_lossy(&out.stderr).trim().to_string();
    }
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn first_line(s: &str) -> String {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or(s)
        .to_string()
}

fn hostname() -> String {
    cmd_out("hostname", &[])
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok().map(|s| s.trim().into()))
        .unwrap_or_else(|| "unknown".into())
}

pub fn primary_ip() -> String {
    if let Some(s) = cmd_out("ip", &["-4", "route", "get", "1.1.1.1"]) {
        let parts: Vec<&str> = s.split_whitespace().collect();
        if let Some(i) = parts.iter().position(|p| *p == "src") {
            if let Some(ip) = parts.get(i + 1) {
                return (*ip).to_string();
            }
        }
    }
    list_ips().into_iter().next().unwrap_or_default()
}

fn list_ips() -> Vec<String> {
    let mut ips = Vec::new();
    if let Some(raw) = cmd_out("hostname", &["-I"]) {
        for ip in raw.split_whitespace() {
            if !skip_ip(ip) && !ips.iter().any(|x| x == ip) {
                ips.push(ip.to_string());
            }
        }
    }
    if ips.is_empty() {
        if let Some(raw) = cmd_out("ip", &["-4", "-o", "addr", "show"]) {
            for ip in parse_ip_addr(&raw) {
                if !skip_ip(&ip) && !ips.contains(&ip) {
                    ips.push(ip);
                }
            }
        }
    }
    ips
}

pub fn parse_ip_addr(raw: &str) -> Vec<String> {
    let mut ips = Vec::new();
    for line in raw.lines() {
        // 2: enp3s0    inet 10.141.8.61/24 ...
        let parts: Vec<&str> = line.split_whitespace().collect();
        if let Some(i) = parts.iter().position(|p| *p == "inet") {
            if let Some(cidr) = parts.get(i + 1) {
                let ip = cidr.split('/').next().unwrap_or(cidr);
                if !ip.is_empty() {
                    ips.push(ip.to_string());
                }
            }
        }
    }
    ips
}

fn skip_ip(ip: &str) -> bool {
    ip == "127.0.0.1"
        || ip.starts_with("127.")
        || ip.starts_with("169.254.")
        || ip == "::1"
}

pub fn parse_df(raw: &str) -> Vec<DiskMount> {
    let mut out = Vec::new();
    for (i, line) in raw.lines().enumerate() {
        if i == 0 {
            continue;
        }
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 6 {
            continue;
        }
        let filesystem = cols[0];
        let mount = cols[cols.len() - 1];
        if filesystem.starts_with("tmpfs")
            || filesystem == "devtmpfs"
            || filesystem == "overlay"
            || mount.starts_with("/run")
            || mount.starts_with("/sys")
            || mount.starts_with("/dev")
            || mount.starts_with("/proc")
        {
            continue;
        }
        let Ok(total) = cols[1].parse::<u64>() else {
            continue;
        };
        let Ok(used) = cols[2].parse::<u64>() else {
            continue;
        };
        let Ok(avail) = cols[3].parse::<u64>() else {
            continue;
        };
        out.push(DiskMount {
            mount: mount.into(),
            filesystem: filesystem.into(),
            total,
            used,
            avail,
        });
    }
    out
}

fn collect_disks() -> Vec<DiskMount> {
    cmd_out("df", &["-P", "-B1"])
        .map(|s| parse_df(&s))
        .unwrap_or_default()
}

fn parse_meminfo_kb(v: &str) -> Option<u64> {
    v.split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()
        .map(|n| n.saturating_mul(1024))
}

pub fn parse_meminfo(raw: &str) -> MemInfo {
    let mut total = 0u64;
    let mut avail = 0u64;
    let mut free = 0u64;
    let mut buffers = 0u64;
    let mut cached = 0u64;
    let mut swap_total = 0u64;
    let mut swap_free = 0u64;
    for line in raw.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let Some(bytes) = parse_meminfo_kb(v) else {
            continue;
        };
        match k.trim() {
            "MemTotal" => total = bytes,
            "MemAvailable" => avail = bytes,
            "MemFree" => free = bytes,
            "Buffers" => buffers = bytes,
            "Cached" => cached = bytes,
            "SwapTotal" => swap_total = bytes,
            "SwapFree" => swap_free = bytes,
            _ => {}
        }
    }
    if avail == 0 && total > 0 {
        avail = free.saturating_add(buffers).saturating_add(cached).min(total);
    }
    if avail > total {
        avail = total;
    }
    MemInfo {
        total,
        used: total.saturating_sub(avail),
        avail,
        swap_total,
        swap_used: swap_total.saturating_sub(swap_free).min(swap_total),
    }
}

fn collect_mem() -> MemInfo {
    std::fs::read_to_string("/proc/meminfo")
        .map(|s| parse_meminfo(&s))
        .unwrap_or_default()
}

pub fn parse_cpuinfo(raw: &str, fallback_arch: &str, fallback_cpus: u32) -> CpuInfo {
    let mut model = String::new();
    let mut arch = fallback_arch.to_string();
    let mut processors = 0u32;
    let mut physical_ids = std::collections::BTreeSet::new();
    for line in raw.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let k = k.trim();
        let v = v.trim();
        match k {
            "processor" => processors += 1,
            "model name" | "Hardware" if model.is_empty() => model = v.into(),
            "CPU part" if model.is_empty() => model = format!("CPU part {v}"),
            "architecture" | "CPU architecture" => arch = v.into(),
            "physical id" => {
                if let Ok(n) = v.parse::<u32>() {
                    physical_ids.insert(n);
                }
            }
            _ => {}
        }
    }
    if model.is_empty() {
        if let Some(line) = raw.lines().find(|l| l.to_lowercase().contains("implementer")) {
            model = line.trim().into();
        }
    }
    let logical = if processors == 0 {
        fallback_cpus
    } else {
        processors
    };
    CpuInfo {
        arch,
        model,
        logical_cpus: logical,
        sockets: if physical_ids.is_empty() {
            None
        } else {
            Some(physical_ids.len() as u32)
        },
    }
}

fn collect_cpu() -> CpuInfo {
    let fallback_cpus = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1);
    let arch = cmd_out("uname", &["-m"]).unwrap_or_else(|| std::env::consts::ARCH.into());
    let raw = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let mut cpu = parse_cpuinfo(&raw, &arch, fallback_cpus);
    if cpu.arch.is_empty() {
        cpu.arch = arch;
    }
    if cpu.model.is_empty() {
        if let Some(s) = cmd_out("lscpu", &[]) {
            for line in s.lines() {
                if let Some(v) = line.strip_prefix("Model name:") {
                    cpu.model = v.trim().into();
                    break;
                }
            }
        }
    }
    cpu
}

pub fn parse_nvidia_smi_l(raw: &str) -> Vec<GpuInfo> {
    let mut gpus = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // GPU 0: NVIDIA A100-SXM4-40GB (UUID: ...)
        let name = line
            .split_once(':')
            .map(|(_, rest)| rest.split('(').next().unwrap_or(rest).trim())
            .unwrap_or(line);
        gpus.push(GpuInfo {
            name: name.into(),
            arch: "NVIDIA".into(),
            count: 1,
            source: "nvidia-smi".into(),
        });
    }
    coalesce_gpus(gpus)
}

pub fn parse_lspci_display(raw: &str) -> Vec<GpuInfo> {
    let mut gpus = Vec::new();
    for line in raw.lines() {
        let lower = line.to_lowercase();
        if !(lower.contains("vga") || lower.contains("3d controller") || lower.contains("display")) {
            continue;
        }
        let name = line.split(": ").nth(1).unwrap_or(line).trim();
        let arch = if lower.contains("nvidia") {
            "NVIDIA"
        } else if lower.contains("amd") || lower.contains("ati") {
            "AMD"
        } else if lower.contains("intel") {
            "Intel"
        } else if lower.contains("mali") {
            "Mali"
        } else if lower.contains("llvmpipe") {
            "Software"
        } else {
            "PCI"
        };
        gpus.push(GpuInfo {
            name: name.into(),
            arch: arch.into(),
            count: 1,
            source: "lspci".into(),
        });
    }
    coalesce_gpus(gpus)
}

fn coalesce_gpus(gpus: Vec<GpuInfo>) -> Vec<GpuInfo> {
    let mut map: Vec<GpuInfo> = Vec::new();
    for g in gpus {
        if let Some(e) = map.iter_mut().find(|e| e.name == g.name && e.arch == g.arch) {
            e.count += g.count;
        } else {
            map.push(g);
        }
    }
    map
}

fn collect_gpus() -> Vec<GpuInfo> {
    if let Some(raw) = cmd_out("nvidia-smi", &["-L"]) {
        let g = parse_nvidia_smi_l(&raw);
        if !g.is_empty() {
            return g;
        }
    }
    if Path::new("/dev").read_dir().ok().into_iter().flatten().flatten().any(|e| {
        e.file_name().to_string_lossy().starts_with("davinci")
    }) {
        let n = std::fs::read_dir("/dev")
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("davinci"))
            .count() as u32;
        if n > 0 {
            return vec![GpuInfo {
                name: "Huawei Ascend NPU".into(),
                arch: "Ascend".into(),
                count: n,
                source: "/dev/davinci*".into(),
            }];
        }
    }
    if let Some(raw) = cmd_out("lspci", &[]) {
        let g = parse_lspci_display(&raw);
        if !g.is_empty() {
            return g;
        }
    }
    drm_gpus()
}

fn drm_gpus() -> Vec<GpuInfo> {
    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("card") || name.contains('-') {
            continue;
        }
        let uevent = e.path().join("device/uevent");
        let driver = std::fs::read_to_string(&uevent)
            .ok()
            .and_then(|s| {
                s.lines()
                    .find_map(|l| l.strip_prefix("DRIVER=").map(|v| v.to_string()))
            })
            .unwrap_or_else(|| name.to_string());
        if driver == "virtio_gpu" || driver.contains("bochs") || driver.contains("qxl") {
            names.push(GpuInfo {
                name: driver.clone(),
                arch: "Virtual".into(),
                count: 1,
                source: format!("drm/{name}"),
            });
        } else {
            names.push(GpuInfo {
                name: driver.clone(),
                arch: driver,
                count: 1,
                source: format!("drm/{name}"),
            });
        }
    }
    coalesce_gpus(names)
}

fn listen_from_unit() -> Option<(String, u16)> {
    let content = std::fs::read_to_string(UNIT_PATH).ok()?;
    for line in content.lines() {
        let line = line.trim();
        if let Some(exec) = line.strip_prefix("ExecStart=") {
            return Some(parse_exec_listen(exec));
        }
    }
    None
}

pub fn parse_exec_listen(exec: &str) -> (String, u16) {
    let mut bind = "0.0.0.0".to_string();
    let mut port = 5400u16;
    let parts: Vec<&str> = exec.split_whitespace().collect();
    let mut i = 0;
    while i < parts.len() {
        let p = parts[i].trim_matches('"');
        if p == "--bind" {
            if let Some(v) = parts.get(i + 1) {
                bind = v.trim_matches('"').to_string();
                i += 2;
                continue;
            }
        } else if let Some(v) = p.strip_prefix("--bind=") {
            bind = v.trim_matches('"').to_string();
        } else if p == "--port" {
            if let Some(v) = parts.get(i + 1) {
                if let Ok(n) = v.trim_matches('"').parse() {
                    port = n;
                }
                i += 2;
                continue;
            }
        } else if let Some(v) = p.strip_prefix("--port=") {
            if let Ok(n) = v.trim_matches('"').parse() {
                port = n;
            }
        }
        i += 1;
    }
    (bind, port)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_project() -> Project {
        Project {
            id: "p1".into(),
            name: "cis".into(),
            description: "业务".into(),
            directory: "/data/cis".into(),
            created_at: String::new(),
            updated_at: String::new(),
            current_version_no: Some(3),
            current_version_id: Some("v3".into()),
            version_count: 3,
        }
    }

    #[test]
    fn renders_projects_and_disk() {
        let snap = HostSnapshot {
            generated_at: "2026-08-21 14:00:00 +0800".into(),
            hostname: "hn".into(),
            primary_ip: "10.141.8.61".into(),
            ips: vec!["10.141.8.61".into()],
            listen: Some(("0.0.0.0".into(), 80)),
            software: vec![Software {
                name: "cangling-update".into(),
                version: "v0.1.29  监听 0.0.0.0:80".into(),
                path: "/root/update/cangling-update".into(),
            }],
            projects: vec![sample_project()],
            disks: vec![DiskMount {
                mount: "/".into(),
                filesystem: "/dev/mapper/klas-root".into(),
                total: 4 * 1024 * 1024 * 1024 * 1024,
                used: 37 * 1024 * 1024 * 1024,
                avail: 4 * 1024 * 1024 * 1024 * 1024 - 37 * 1024 * 1024 * 1024,
            }],
            mem: MemInfo {
                total: 32 * 1024 * 1024 * 1024,
                used: 12 * 1024 * 1024 * 1024,
                avail: 20 * 1024 * 1024 * 1024,
                swap_total: 8 * 1024 * 1024 * 1024,
                swap_used: 0,
            },
            cpu: CpuInfo {
                arch: "aarch64".into(),
                model: "Kunpeng-920".into(),
                logical_cpus: 64,
                sockets: Some(2),
            },
            gpus: vec![],
            exe_dir: "/root/update".into(),
            config_dir: "/root/update/config".into(),
        };
        let md = render_markdown(&snap);
        assert!(md.contains("# 主机信息"));
        assert!(md.contains("## 内存"));
        assert!(md.contains("物理内存"));
        assert!(md.contains("交换分区"));
        assert!(md.contains("10.141.8.61"));
        assert!(md.contains("| cis | /data/cis | v3 | 业务 |"));
        assert!(md.contains("Kunpeng-920"));
        assert!(md.contains("64"));
        assert!(md.contains("未检测到 GPU"));
        assert!(md.contains("/root/update/cangling-update"));
        assert!(md.contains("0.0.0.0:80"));
        assert!(md.contains("GiB") || md.contains("TiB"));
    }

    #[test]
    fn empty_projects_note() {
        let md = render_markdown(&HostSnapshot {
            generated_at: "t".into(),
            hostname: "x".into(),
            ..HostSnapshot::default()
        });
        assert!(md.contains("暂无登记项目"));
    }

    #[test]
    fn parse_df_skips_tmpfs() {
        let raw = "\
Filesystem     1024-blocks Used Available Capacity Mounted on
/dev/sda2      4294967296 1000 4294966296 1% /
tmpfs          1024 0 1024 0% /dev/shm
overlay        100 1 99 1% /var/lib/docker/overlay2/abc
/dev/sda1      1048576 200000 848576 19% /boot
";
        let disks = parse_df(raw);
        assert_eq!(disks.len(), 2);
        assert_eq!(disks[0].mount, "/");
        assert_eq!(disks[0].total, 4294967296);
        assert_eq!(disks[1].mount, "/boot");
    }

    #[test]
    fn parse_aarch64_cpuinfo() {
        let raw = "\
processor\t: 0
model name\t: Kunpeng-920
CPU architecture: 8
processor\t: 1
model name\t: Kunpeng-920
physical id\t: 0
processor\t: 2
physical id\t: 1
";
        let cpu = parse_cpuinfo(raw, "aarch64", 1);
        assert_eq!(cpu.logical_cpus, 3);
        assert_eq!(cpu.model, "Kunpeng-920");
        assert_eq!(cpu.sockets, Some(2));
        assert_eq!(cpu.arch, "8");
    }

    #[test]
    fn parse_gpus() {
        let nv = parse_nvidia_smi_l(
            "GPU 0: NVIDIA A100-SXM4-40GB (UUID: GPU-aaa)\nGPU 1: NVIDIA A100-SXM4-40GB (UUID: GPU-bbb)\n",
        );
        assert_eq!(nv.len(), 1);
        assert_eq!(nv[0].count, 2);
        assert_eq!(nv[0].arch, "NVIDIA");

        let pci = parse_lspci_display(
            "00:02.0 VGA compatible controller: Device 1234:1111\n01:00.0 3D controller: NVIDIA Corporation GA102\n",
        );
        assert!(pci.iter().any(|g| g.arch == "NVIDIA"));
    }

    #[test]
    fn parse_exec_port() {
        assert_eq!(
            parse_exec_listen("/root/update/cangling-update --bind 0.0.0.0 --port 80"),
            ("0.0.0.0".into(), 80)
        );
    }

    fn sample_snap() -> HostSnapshot {
        HostSnapshot {
            generated_at: "2026-08-21 14:00:00 +0800".into(),
            hostname: "hn".into(),
            primary_ip: "10.141.8.61".into(),
            ips: vec!["10.141.8.61".into()],
            listen: Some(("0.0.0.0".into(), 80)),
            software: vec![Software {
                name: "git".into(),
                version: "未安装".into(),
                path: "—".into(),
            }],
            projects: vec![],
            disks: vec![DiskMount {
                mount: "/".into(),
                filesystem: "xfs".into(),
                total: 100,
                used: 80,
                avail: 20,
            }],
            mem: MemInfo {
                total: 16 * 1024 * 1024 * 1024,
                used: 8 * 1024 * 1024 * 1024,
                avail: 8 * 1024 * 1024 * 1024,
                swap_total: 0,
                swap_used: 0,
            },
            cpu: CpuInfo {
                arch: "aarch64".into(),
                model: "Kunpeng-920".into(),
                logical_cpus: 64,
                sockets: Some(2),
            },
            gpus: vec![],
            exe_dir: "/root/update".into(),
            config_dir: "/root/update/config".into(),
        }
    }

    #[test]
    fn parse_meminfo_uses_available() {
        let raw = "\
MemTotal:       32768000 kB
MemFree:         1024000 kB
MemAvailable:   20480000 kB
Buffers:          512000 kB
Cached:          4096000 kB
SwapTotal:       8192000 kB
SwapFree:        6144000 kB
";
        let m = parse_meminfo(raw);
        assert_eq!(m.total, 32768000 * 1024);
        assert_eq!(m.avail, 20480000 * 1024);
        assert_eq!(m.used, (32768000 - 20480000) * 1024);
        assert_eq!(m.swap_total, 8192000 * 1024);
        assert_eq!(m.swap_used, (8192000 - 6144000) * 1024);
    }

    #[test]
    fn parse_meminfo_falls_back_without_available() {
        let raw = "\
MemTotal:       1000 kB
MemFree:         100 kB
Buffers:          50 kB
Cached:          150 kB
SwapTotal:         0 kB
SwapFree:          0 kB
";
        let m = parse_meminfo(raw);
        assert_eq!(m.total, 1000 * 1024);
        assert_eq!(m.avail, 300 * 1024);
        assert_eq!(m.used, 700 * 1024);
        assert_eq!(m.swap_total, 0);
        assert_eq!(m.swap_used, 0);
    }

    #[test]
    fn ansi_has_color_and_plain_does_not() {
        let snap = sample_snap();
        let color = render_ansi(&snap, true);
        let plain = render_ansi(&snap, false);
        assert!(color.contains("\x1b["));
        assert!(!plain.contains("\x1b["));
        assert!(plain.contains("主机信息"));
        assert!(plain.contains("10.141.8.61"));
        assert!(plain.contains("Kunpeng-920"));
        assert!(plain.contains("暂无登记项目"));
        assert!(color.contains("█") || color.contains("░"));
    }

    #[test]
    fn color_query() {
        assert!(want_color(None));
        assert!(want_color(Some("1")));
        assert!(want_color(Some("always")));
        assert!(!want_color(Some("0")));
        assert!(!want_color(Some("never")));
        assert!(!want_color(Some("false")));
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1536), "1.5 KiB");
    }

    #[test]
    fn parse_ip_lines() {
        let raw = "2: enp3s0    inet 10.141.8.61/24 brd 10.141.8.255 scope global\n8: docker0 inet 172.17.0.1/16\n";
        let ips = parse_ip_addr(raw);
        assert_eq!(ips, vec!["10.141.8.61", "172.17.0.1"]);
    }

    #[test]
    fn short_version_parses_compose_and_rejects_docker_help() {
        assert_eq!(short_version("v2.29.7\n").as_deref(), Some("v2.29.7"));
        assert_eq!(
            short_version("Docker Compose version v2.24.5\n").as_deref(),
            Some("v2.24.5")
        );
        assert_eq!(
            short_version("docker-compose version 1.29.2, build 5becea4c\n").as_deref(),
            Some("1.29.2")
        );
        let help = "\
unknown flag: --short
See 'docker --help'.

Usage:  docker [OPTIONS] COMMAND

A self-sufficient runtime for containers

Common Commands:
  run         Create and run a new container from an image
  exec        Execute a command in a running container
";
        assert_eq!(short_version(help), None);
        assert_eq!(short_version("unknown flag: --short"), None);
        assert_eq!(
            short_version("docker: 'compose' is not a docker command.\nSee 'docker --help'\n"),
            None
        );
    }
}
