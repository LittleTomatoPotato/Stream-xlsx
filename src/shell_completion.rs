use clap::CommandFactory;
use clap_complete::Shell;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::Args;

/// 自动检测当前用户的 shell
fn detect_shell() -> anyhow::Result<Shell> {
    #[cfg(windows)]
    {
        // Windows 默认 PowerShell，也可以通过进程名判断
        if std::env::var("PSModulePath").is_ok()
            || std::env::var("PSExecutionPolicyPreference").is_ok()
        {
            return Ok(Shell::PowerShell);
        }
        // 回退到 PowerShell
        return Ok(Shell::PowerShell);
    }

    #[cfg(not(windows))]
    {
        let shell_path = std::env::var("SHELL").map_err(|_| {
            anyhow::anyhow!("无法检测 shell：$SHELL 环境变量未设置，请手动指定 --shell")
        })?;
        let shell_name = Path::new(&shell_path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("");

        match shell_name {
            "bash" => Ok(Shell::Bash),
            "zsh" => Ok(Shell::Zsh),
            "fish" => Ok(Shell::Fish),
            "elvish" => Ok(Shell::Elvish),
            _ => Err(anyhow::anyhow!(
                "无法识别 shell '{}', 请手动指定 --shell",
                shell_name
            )),
        }
    }
}

fn home_dir() -> anyhow::Result<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var("USERPROFILE")
            .or_else(|_| {
                std::env::var("HOMEDRIVE")
                    .map(|d| d + &std::env::var("HOMEPATH").unwrap_or_default())
            })
            .map(PathBuf::from)
            .map_err(|_| anyhow::anyhow!("无法获取用户主目录"))
    }
    #[cfg(not(windows))]
    {
        std::env::var("HOME")
            .map(PathBuf::from)
            .map_err(|_| anyhow::anyhow!("无法获取用户主目录"))
    }
}

/// 补全脚本的存放目录和 rc 文件路径
struct ShellConfig {
    /// 补全脚本写入路径
    completion_path: PathBuf,
    /// 需要修改的 shell rc 文件
    rc_path: Option<PathBuf>,
    /// 追加到 rc 文件的 source 语句
    source_line: String,
    /// 提示用户执行的重载命令
    reload_hint: String,
}

impl ShellConfig {
    fn for_shell(shell: Shell, home: &Path) -> Self {
        match shell {
            Shell::Bash => {
                let completion_dir = home.join(".local/share/sxlsx/completions");
                let completion_path = completion_dir.join("sxlsx.bash");
                let rc_path = home.join(".bashrc");
                let source_line = format!(
                    r#"# sxlsx shell completion
[ -f "{}" ] && source "{}""#,
                    completion_path.display(),
                    completion_path.display()
                );
                Self {
                    completion_path,
                    rc_path: Some(rc_path),
                    source_line,
                    reload_hint: "source ~/.bashrc".into(),
                }
            }
            Shell::Zsh => {
                let completion_dir = home.join(".local/share/sxlsx/completions");
                let completion_path = completion_dir.join("_sxlsx");
                let rc_path = home.join(".zshrc");
                let source_line = format!(
                    r#"# sxlsx shell completion
fpath+=("{}")
autoload -Uz compinit && compinit"#,
                    completion_dir.display()
                );
                Self {
                    completion_path,
                    rc_path: Some(rc_path),
                    source_line,
                    reload_hint: "source ~/.zshrc".into(),
                }
            }
            Shell::Fish => {
                let completion_dir = home.join(".config/fish/completions");
                let completion_path = completion_dir.join("sxlsx.fish");
                // fish 自动加载 completions 目录，不需要修改 config.fish
                Self {
                    completion_path,
                    rc_path: None,
                    source_line: String::new(),
                    reload_hint: "无需额外操作，fish 会自动加载".into(),
                }
            }
            Shell::PowerShell => {
                #[cfg(windows)]
                let completion_dir = home.join("Documents/PowerShell/Completions");
                #[cfg(not(windows))]
                let completion_dir = home.join(".local/share/powershell/Completions");

                let completion_path = completion_dir.join("sxlsx.ps1");
                let rc_path = powershell_profile_path(home);
                let source_line = format!(
                    r#"# sxlsx shell completion
. "{}""#,
                    completion_path.display()
                );
                Self {
                    completion_path,
                    rc_path,
                    source_line,
                    reload_hint: ". $PROFILE".into(),
                }
            }
            Shell::Elvish => {
                let completion_dir = home.join(".local/share/sxlsx/completions");
                let completion_path = completion_dir.join("sxlsx.elv");
                let rc_path = home.join(".elvish/rc.elv");
                let source_line = format!(
                    r#"# sxlsx shell completion
eval (cat "{}")"#,
                    completion_path.display()
                );
                Self {
                    completion_path,
                    rc_path: Some(rc_path),
                    source_line,
                    reload_hint: "重启 elvish".into(),
                }
            }
            _ => unreachable!(),
        }
    }
}

#[cfg(windows)]
fn powershell_profile_path(home: &Path) -> Option<PathBuf> {
    // 尝试常见路径
    let candidates = [
        home.join("Documents/PowerShell/Microsoft.PowerShell_profile.ps1"),
        home.join("Documents/WindowsPowerShell/Microsoft.PowerShell_profile.ps1"),
    ];
    for p in &candidates {
        if p.exists() {
            return Some(p.clone());
        }
    }
    // 默认创建第一个
    Some(candidates[0].clone())
}

#[cfg(not(windows))]
fn powershell_profile_path(_home: &Path) -> Option<PathBuf> {
    // Linux/macOS 上 PowerShell 配置文件通常在 ~/.config/powershell/
    None
}

/// 生成补全脚本内容
fn generate_completion(shell: Shell) -> Vec<u8> {
    let mut cmd = Args::command();
    let name = cmd.get_name().to_string();
    let mut buf = Vec::new();
    clap_complete::generate(shell, &mut cmd, name, &mut buf);
    buf
}

/// 检查 rc 文件中是否已包含 source 语句
fn already_sourced(rc_path: &Path, marker: &str) -> bool {
    fs::read_to_string(rc_path)
        .map(|content| content.contains(marker))
        .unwrap_or(false)
}

/// 主入口
pub fn install(shell: Option<Shell>) -> anyhow::Result<()> {
    let shell = match shell {
        Some(s) => s,
        None => detect_shell()?,
    };

    let home = home_dir()?;
    let config = ShellConfig::for_shell(shell, &home);

    // 1. 生成并写入补全脚本
    let completion_script = generate_completion(shell);
    if let Some(parent) = config.completion_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&config.completion_path, completion_script)?;
    println!("✓ 补全脚本已写入: {}", config.completion_path.display());

    // 2. 如有需要，在 rc 文件中追加 source 语句
    if let Some(rc_path) = &config.rc_path {
        if !rc_path.exists() {
            if let Some(parent) = rc_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(rc_path, "")?;
        }

        let marker = "sxlsx shell completion";
        if !already_sourced(rc_path, marker) {
            let mut file = fs::OpenOptions::new().append(true).open(rc_path)?;
            writeln!(file)?;
            writeln!(file, "{}", config.source_line)?;
            println!("✓ 已在 {} 中追加补全加载语句", rc_path.display());
        } else {
            println!("○ {} 中已存在补全加载语句，跳过", rc_path.display());
        }
    }

    // 3. 提示重载
    println!();
    println!("补全安装完成！请执行以下命令使其生效：");
    println!("  {}", config.reload_hint);
    println!("或重新打开终端。");

    Ok(())
}

/// 仅输出到 stdout（老行为）
pub fn print_to_stdout(shell: Shell) {
    let mut cmd = Args::command();
    let name = cmd.get_name().to_string();
    clap_complete::generate(shell, &mut cmd, name, &mut std::io::stdout());
}
