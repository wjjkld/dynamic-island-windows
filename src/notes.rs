//! 便签：随手记下的想法，每条一行，存在 %APPDATA%\WinDynamicIsland\notes.txt。
//! 新的在最上面，条数不限（岛上一次显示几条，滚轮翻）。

use std::path::PathBuf;

use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::CF_UNICODETEXT;

/// 一条便签最多多少个字（岛上一行放不下时显示省略号，复制出来是完整的）。
pub const MAX_CHARS: usize = 200;

#[derive(Default)]
pub struct Notes {
    pub items: Vec<String>,
    path: Option<PathBuf>,
}

impl Notes {
    pub fn load() -> Notes {
        let path = path();
        let text = path.as_ref().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
        Notes { items: parse(&text), path }
    }

    /// 加一条（放最上面）。空白时不加，返回是否加了。
    pub fn add(&mut self, text: &str) -> bool {
        let t = clean(text);
        if t.is_empty() {
            return false;
        }
        self.items.insert(0, t);
        self.save();
        true
    }

    pub fn remove(&mut self, i: usize) {
        if i < self.items.len() {
            self.items.remove(i);
            self.save();
        }
    }

    fn save(&self) {
        let Some(p) = &self.path else { return };
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(p, self.items.join("\n") + "\n");
    }
}

fn path() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("APPDATA")?).join("WinDynamicIsland").join("notes.txt"))
}

fn parse(text: &str) -> Vec<String> {
    text.lines().map(clean).filter(|l| !l.is_empty()).collect()
}

/// 去掉首尾空白和换行，截到 MAX_CHARS 个字。
fn clean(s: &str) -> String {
    let one_line = s.split(['\r', '\n']).map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" ");
    one_line.chars().take(MAX_CHARS).collect()
}

/// 把文字放进剪贴板。
pub fn copy(text: &str) -> bool {
    let wide: Vec<u16> = text.encode_utf16().chain([0]).collect();
    unsafe {
        if OpenClipboard(None).is_err() {
            return false;
        }
        let _ = EmptyClipboard();
        let ok = (|| {
            let mem = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2).ok()?;
            let dst = GlobalLock(mem) as *mut u16;
            if dst.is_null() {
                let _ = GlobalFree(Some(mem));
                return None;
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
            let _ = GlobalUnlock(mem);
            // 成功后内存归系统管
            if SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(mem.0))).is_err() {
                let _ = GlobalFree(Some(HGLOBAL(mem.0)));
                return None;
            }
            Some(())
        })()
        .is_some();
        let _ = CloseClipboard();
        ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_clean() {
        assert_eq!(parse("  a  \n\n b\r\n"), ["a", "b"]);
        assert_eq!(clean("第一行\n第二行"), "第一行 第二行");
        assert_eq!(clean(&"字".repeat(300)).chars().count(), MAX_CHARS);
        assert_eq!(parse(&"x\n".repeat(30)).len(), 30);
    }

    #[test]
    fn add_remove_without_file() {
        let mut n = Notes::default();
        assert!(!n.add("   "));
        assert!(n.add("买牛奶"));
        assert!(n.add("回邮件"));
        assert_eq!(n.items, ["回邮件", "买牛奶"]);
        n.remove(0);
        assert_eq!(n.items, ["买牛奶"]);
        for i in 0..20 {
            n.add(&i.to_string());
        }
        assert_eq!(n.items.len(), 21);
    }
}
