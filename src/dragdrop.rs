//! OLE 拖放：岛作为放置目标接收文件，以及把暂存的文件拖出去。

use std::path::PathBuf;

use windows::Win32::Foundation::{HWND, POINT, POINTL};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CoCreateInstance, DVASPECT_CONTENT, FORMATETC, IDataObject, TYMED_HGLOBAL,
};
use windows::Win32::System::Ole::{
    CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE, IDropTarget, IDropTarget_Impl, ReleaseStgMedium,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::Shell::{CLSID_DragDropHelper, DragQueryFileW, HDROP, IDropTargetHelper};
use windows::core::{Ref, Result};
use windows_core::implement;

/// 放置目标把事件交给 UI 侧处理。
pub trait DropSink {
    /// 有文件拖进窗口 / 离开窗口。
    fn drag_hover(&self, over: bool);
    /// 文件放下。
    fn dropped(&self, paths: Vec<PathBuf>);
    /// 是不是我们自己正在往外拖（自己拖给自己要忽略）。
    fn is_dragging_out(&self) -> bool;
}

#[implement(IDropTarget)]
pub struct DropTarget {
    hwnd: HWND,
    sink: Box<dyn DropSink>,
    helper: Option<IDropTargetHelper>,
    accept: std::cell::Cell<bool>,
}

impl DropTarget {
    pub fn create(hwnd: HWND, sink: impl DropSink + 'static) -> IDropTarget {
        let helper = unsafe { CoCreateInstance(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER).ok() };
        DropTarget { hwnd, sink: Box::new(sink), helper, accept: std::cell::Cell::new(false) }.into()
    }

    fn effect(&self) -> DROPEFFECT {
        if self.accept.get() { DROPEFFECT_COPY } else { DROPEFFECT_NONE }
    }
}

fn hdrop_format() -> FORMATETC {
    FORMATETC { cfFormat: CF_HDROP.0, ptd: std::ptr::null_mut(), dwAspect: DVASPECT_CONTENT.0, lindex: -1, tymed: TYMED_HGLOBAL.0 as u32 }
}

fn has_files(data: &IDataObject) -> bool {
    unsafe { data.QueryGetData(&hdrop_format()).is_ok() }
}

/// 从数据对象里读出 CF_HDROP 文件列表。
pub fn read_files(data: &IDataObject) -> Vec<PathBuf> {
    let mut out = Vec::new();
    unsafe {
        let Ok(mut medium) = data.GetData(&hdrop_format()) else { return out };
        let hdrop = HDROP(medium.u.hGlobal.0);
        let n = DragQueryFileW(hdrop, u32::MAX, None);
        for i in 0..n {
            let len = DragQueryFileW(hdrop, i, None) as usize;
            let mut buf = vec![0u16; len + 1];
            let got = DragQueryFileW(hdrop, i, Some(&mut buf)) as usize;
            out.push(PathBuf::from(String::from_utf16_lossy(&buf[..got])));
        }
        ReleaseStgMedium(&mut medium);
    }
    out
}

fn pt(p: &POINTL) -> POINT {
    POINT { x: p.x, y: p.y }
}

impl IDropTarget_Impl for DropTarget_Impl {
    fn DragEnter(&self, data: Ref<IDataObject>, _keys: MODIFIERKEYS_FLAGS, p: &POINTL, effect: *mut DROPEFFECT) -> Result<()> {
        let ok = !self.sink.is_dragging_out() && data.as_ref().is_some_and(has_files);
        self.accept.set(ok);
        unsafe {
            *effect = self.effect();
            if let (Some(h), Some(d)) = (&self.helper, data.as_ref()) {
                let _ = h.DragEnter(self.hwnd, d, &pt(p), *effect);
            }
        }
        if ok {
            self.sink.drag_hover(true);
        }
        Ok(())
    }

    fn DragOver(&self, _keys: MODIFIERKEYS_FLAGS, p: &POINTL, effect: *mut DROPEFFECT) -> Result<()> {
        unsafe {
            *effect = self.effect();
            if let Some(h) = &self.helper {
                let _ = h.DragOver(&pt(p), *effect);
            }
        }
        Ok(())
    }

    fn DragLeave(&self) -> Result<()> {
        if let Some(h) = &self.helper {
            unsafe {
                let _ = h.DragLeave();
            }
        }
        if self.accept.replace(false) {
            self.sink.drag_hover(false);
        }
        Ok(())
    }

    fn Drop(&self, data: Ref<IDataObject>, _keys: MODIFIERKEYS_FLAGS, p: &POINTL, effect: *mut DROPEFFECT) -> Result<()> {
        unsafe {
            *effect = self.effect();
            if let (Some(h), Some(d)) = (&self.helper, data.as_ref()) {
                let _ = h.Drop(d, &pt(p), *effect);
            }
        }
        if self.accept.replace(false) {
            self.sink.drag_hover(false);
            if let Some(d) = data.as_ref() {
                self.sink.dropped(read_files(d));
            }
        }
        Ok(())
    }
}
