//! 把文本放进系统剪贴板。
//!
//! 优先用平台自带命令（Windows `clip` / PowerShell `Set-Clipboard`、macOS `pbcopy`、
//! Linux 的 wl-copy/xclip/xsel）；都不可用时退回 OSC 52（Windows Terminal / iTerm2 等支持，
//! 终端自己负责写剪贴板）。面板不引入额外的剪贴板 crate。

use std::io::Write;
use std::process::{Command, Stdio};

/// 复制一段文本。失败时返回可展示给用户的原因。
pub fn copy(text: &str) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        if windows_copy(text).is_ok() {
            return Ok(());
        }
    }

    #[cfg(target_os = "macos")]
    {
        if pipe_to("pbcopy", &[], text).is_ok() {
            return Ok(());
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let attempts: [(&str, &[&str]); 4] = [
            // Termux 手机上优先用 termux-api（需要装 termux-api 包）。
            ("termux-clipboard-set", &[]),
            ("wl-copy", &[]),
            ("xclip", &["-selection", "clipboard"]),
            ("xsel", &["--clipboard", "--input"]),
        ];
        for (program, args) in attempts {
            if pipe_to(program, args, text).is_ok() {
                return Ok(());
            }
        }
    }

    osc52(text)
}

/// Windows：ASCII 走 `clip.exe`（快），含中文 / emoji 时走 PowerShell 的 `Set-Clipboard`
/// （`clip.exe` 按控制台代码页读 stdin，非 ASCII 会乱码）。
#[cfg(target_os = "windows")]
fn windows_copy(text: &str) -> Result<(), String> {
    if text.is_ascii() && pipe_to("clip", &[], text).is_ok() {
        return Ok(());
    }
    // 以 base64（纯 ASCII）喂给 PowerShell，绕开 stdin 的代码页问题；到那边再按 UTF-8 解。
    let payload = base64_encode(text.as_bytes());
    let script = "$b64=[Console]::In.ReadToEnd(); $t=[Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($b64)); Set-Clipboard -Value $t";
    if pipe_to(
        "powershell",
        &["-NoProfile", "-NonInteractive", "-Command", script],
        &payload,
    )
    .is_ok()
    {
        return Ok(());
    }
    pipe_to("clip", &[], text)
}

/// 把文本写到子进程 stdin，等它正常退出。
fn pipe_to(program: &str, args: &[&str], text: &str) -> Result<(), String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("起不了 {program}：{error}"))?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin
            .write_all(text.as_bytes())
            .map_err(|error| format!("写不进 {program}：{error}"))?;
    }
    let status = child
        .wait()
        .map_err(|error| format!("等不到 {program}：{error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} 退出码 {status}"))
    }
}

/// 终端剪贴板转义：`ESC ] 52 ; c ; <base64> BEL`。
fn osc52(text: &str) -> Result<(), String> {
    let encoded = base64_encode(text.as_bytes());
    let mut stdout = std::io::stdout();
    write!(stdout, "\x1b]52;c;{encoded}\x07").map_err(|error| format!("写不了终端：{error}"))?;
    stdout
        .flush()
        .map_err(|error| format!("刷不了终端：{error}"))?;
    Ok(())
}

fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let a = chunk[0] as u32;
        let b = chunk.get(1).copied().unwrap_or(0) as u32;
        let c = chunk.get(2).copied().unwrap_or(0) as u32;
        let bits = (a << 16) | (b << 8) | c;
        out.push(TABLE[((bits >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((bits >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((bits >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(bits & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
        assert_eq!(base64_encode("中文".as_bytes()), "5Lit5paH");
    }
}
