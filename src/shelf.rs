//! 文件暂存区：拖进来的文件先放在岛上，之后再拖出去用。
//! 只记录路径，不复制文件；列表保存在 %APPDATA%\WinDynamicIsland\shelf.txt，重启后还在。

use std::path::{Path, PathBuf};

use windows::Win32::Foundation::SIZE;
use windows::Win32::Graphics::Gdi::{DeleteObject, HPALETTE};
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, IDataObject};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{HSTRING, Result, w};

/// 图标边长（像素，BGRA 预乘）。
pub const ICON: u32 = 96;
pub const MAX_ITEMS: usize = 5;

pub struct Item {
    pub id: u64,
    pub path: PathBuf,
    pub name: String,
    pub icon: Option<Vec<u8>>,
}

#[derive(Default)]
pub struct Shelf {
    pub items: Vec<Item>,
    next_id: u64,
}

impl Shelf {
    pub fn load() -> Shelf {
        let mut s = Shelf::default();
        if let Some(f) = store_path()
            && let Ok(text) = std::fs::read_to_string(f) {
                let paths: Vec<PathBuf> = text.lines().filter(|l| !l.is_empty()).map(PathBuf::from).collect();
                s.add(paths, false);
            }
        s
    }

    fn save(&self) {
        let Some(f) = store_path() else { return };
        if let Some(dir) = f.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let text: String = self.items.iter().map(|i| format!("{}\n", i.path.display())).collect();
        let _ = std::fs::write(f, text);
    }

    /// 加入文件（去重；新的排前面；超出上限丢最旧的）。返回新加入的数量。
    pub fn add(&mut self, paths: Vec<PathBuf>, persist: bool) -> usize {
        let mut added = 0;
        for p in paths.into_iter().rev() {
            if !p.exists() {
                continue;
            }
            self.items.retain(|i| i.path != p);
            self.next_id += 1;
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string());
            let icon = icon_for(&p).ok();
            self.items.insert(0, Item { id: self.next_id, path: p, name, icon });
            added += 1;
        }
        self.items.truncate(MAX_ITEMS);
        if persist {
            self.save();
        }
        added
    }

    pub fn remove(&mut self, id: u64) {
        self.items.retain(|i| i.id != id);
        self.save();
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.save();
    }

    /// 文件被移走或删掉后，从暂存区里去掉。
    pub fn prune_missing(&mut self) -> bool {
        let before = self.items.len();
        self.items.retain(|i| i.path.exists());
        let changed = self.items.len() != before;
        if changed {
            self.save();
        }
        changed
    }

    pub fn get(&self, id: u64) -> Option<&Item> {
        self.items.iter().find(|i| i.id == id)
    }
}

fn store_path() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA")?;
    Some(Path::new(&base).join("WinDynamicIsland").join("shelf.txt"))
}

pub fn open(path: &Path) {
    unsafe {
        ShellExecuteW(None, w!("open"), &HSTRING::from(path.as_os_str()), None, None, SW_SHOWNORMAL);
    }
}

/// 用 Shell 生成数据对象：资源管理器、聊天软件、浏览器上传框都认，还自带拖动缩略图。
pub fn data_object(paths: &[&Path]) -> Result<IDataObject> {
    unsafe {
        let mut pidls: Vec<*mut ITEMIDLIST> = Vec::new();
        for p in paths {
            let mut pidl = std::ptr::null_mut();
            if SHParseDisplayName(&HSTRING::from(p.as_os_str()), None, &mut pidl, 0, None).is_ok() {
                pidls.push(pidl);
            }
        }
        let consts: Vec<*const ITEMIDLIST> = pidls.iter().map(|&p| p as *const _).collect();
        let arr = SHCreateShellItemArrayFromIDLists(&consts);
        for p in pidls {
            ILFree(Some(p));
        }
        arr?.BindToHandler(None, &BHID_DataObject)
    }
}

/// 取文件的缩略图（图片、视频）或图标，统一缩放成 ICON×ICON 的预乘 BGRA。
pub fn icon_for(path: &Path) -> Result<Vec<u8>> {
    unsafe {
        let item: IShellItemImageFactory = SHCreateItemFromParsingName(&HSTRING::from(path.as_os_str()), None)?;
        let hbmp = item.GetImage(SIZE { cx: ICON as i32, cy: ICON as i32 }, SIIGBF_RESIZETOFIT)?;
        let wic: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let result = (|| {
            let bmp = wic.CreateBitmapFromHBITMAP(hbmp, HPALETTE::default(), WICBitmapUsePremultipliedAlpha)?;
            let (mut w, mut h) = (0u32, 0u32);
            bmp.GetSize(&mut w, &mut h)?;
            let conv = wic.CreateFormatConverter()?;
            conv.Initialize(&bmp, &GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom)?;
            let mut px = vec![0u8; (w * h * 4) as usize];
            conv.CopyPixels(std::ptr::null(), w * 4, &mut px)?;
            Ok(fit_square(&px, w, h, ICON))
        })();
        let _ = DeleteObject(hbmp.into());
        result
    }
}

/// 居中贴到正方形透明画布上（保持比例，最近邻缩放；GetImage 已经给了接近的尺寸）。
/// 缩略图（JPEG 等）常常整张 alpha 都是 0，这种情况当作不透明处理。
fn fit_square(px: &[u8], w: u32, h: u32, side: u32) -> Vec<u8> {
    let opaque = px.as_chunks::<4>().0.iter().all(|p| p[3] == 0);
    let mut out = vec![0u8; (side * side * 4) as usize];
    if w == 0 || h == 0 {
        return out;
    }
    let k = (side as f32 / w as f32).min(side as f32 / h as f32);
    let (dw, dh) = (((w as f32 * k) as u32).max(1), ((h as f32 * k) as u32).max(1));
    let (ox, oy) = ((side - dw) / 2, (side - dh) / 2);
    for y in 0..dh {
        let sy = ((y as f32 / k) as u32).min(h - 1);
        for x in 0..dw {
            let sx = ((x as f32 / k) as u32).min(w - 1);
            let s = ((sy * w + sx) * 4) as usize;
            let d = (((oy + y) * side + ox + x) * 4) as usize;
            out[d..d + 4].copy_from_slice(&px[s..s + 4]);
            if opaque {
                out[d + 3] = 255;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_square_centers_and_keeps_aspect() {
        // 4×2 红色不透明 → 8×8：上下各留 2 行透明
        let px = [0u8, 0, 255, 255].repeat(8);
        let out = fit_square(&px, 4, 2, 8);
        let alpha = |x: u32, y: u32| out[((y * 8 + x) * 4 + 3) as usize];
        assert_eq!(alpha(4, 0), 0);
        assert_eq!(alpha(4, 4), 255);
        assert_eq!(alpha(4, 7), 0);
    }

    #[test]
    fn fit_square_treats_zero_alpha_as_opaque() {
        let px = [10u8, 20, 30, 0].repeat(4);
        let out = fit_square(&px, 2, 2, 2);
        assert!(out.as_chunks::<4>().0.iter().all(|p| p[3] == 255));
    }

    #[test]
    fn add_dedupes_and_skips_missing() {
        let mut s = Shelf::default();
        let me = std::env::current_exe().unwrap();
        assert_eq!(s.add(vec![me.clone(), me.clone(), PathBuf::from("Z:\\不存在.txt")], false), 2);
        assert_eq!(s.items.len(), 1);
        s.add(vec![me.clone()], false);
        assert_eq!(s.items.len(), 1);
    }
}
