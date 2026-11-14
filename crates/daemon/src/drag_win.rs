//! Windows: pick up a file drag carried over from the other computer, so you can drop it into any folder here.
//!
//! When the pointer comes over while you are dragging files on the other computer, that computer sends
//! `DragStart` (what is being dragged) and starts copying the files into a private staging folder here. This
//! starts a real Windows drag of those files right where the pointer arrived: carry it to an Explorer window,
//! the desktop, or any app that accepts files, and let go. If you let go before the copy is finished, the drop
//! simply waits for it. If you let go where nothing accepts files, they are kept in your received-files folder.
use crate::{inject_win, shared, xfer};
use kvm_protocol::{InputEvent, MouseButton};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use windows::core::{implement, w, HRESULT};
use windows::Win32::Foundation::*;
use windows::Win32::System::Com::{IDataObject, DVASPECT_CONTENT, FORMATETC, STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::{IDropSource, IDropSource_Impl, OleInitialize, CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::Shell::{SHCreateDataObject, SHDoDragDrop, DROPFILES};
use windows::Win32::UI::WindowsAndMessaging::*;

struct Drag {
    id: u32,
    /// None while the files are still arriving; Some(ok) when the transfer is over.
    ready: Option<bool>,
}

static DRAG: Mutex<Option<Drag>> = Mutex::new(None);

fn root() -> PathBuf {
    std::env::temp_dir().join("SmoothKvm-drag")
}

/// Where the files of drag `id` are put while they are being dragged.
pub fn staging(id: u32) -> PathBuf {
    root().join(format!("{id:08x}"))
}

fn state(id: u32) -> Option<Option<bool>> {
    DRAG.lock().ok().and_then(|g| g.as_ref().filter(|d| d.id == id).map(|d| d.ready))
}

/// The files of drag `id` have all arrived (or the transfer failed).
pub fn ready(id: u32, ok: bool) {
    if let Ok(mut g) = DRAG.lock() {
        if let Some(d) = g.as_mut().filter(|d| d.id == id) {
            d.ready = Some(ok);
        }
    }
    // the drag loop re-checks on input: give it a tiny nudge in case you already let go
    inject_win::inject(&InputEvent::MouseMoveRel { dx: 1, dy: 0 });
    inject_win::inject(&InputEvent::MouseMoveRel { dx: -1, dy: 0 });
}

/// The other computer is carrying a drag of `names` (top-level files/folders) over: pick it up here.
pub fn begin(names: Vec<String>, id: u32) {
    // staging folders of old drags (dropped long ago) are cleaned up as new ones start
    if let Ok(rd) = std::fs::read_dir(root()) {
        for e in rd.flatten() {
            let old = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).map(|d| d > Duration::from_secs(24 * 3600)).unwrap_or(false);
            if old {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
    if let Ok(mut g) = DRAG.lock() {
        *g = Some(Drag { id, ready: None });
    }
    std::thread::spawn(move || {
        if let Err(e) = unsafe { run(&names, id) } {
            shared::warn(&format!("Could not continue the drag here: {e}"));
        }
    });
}

/// Build the CF_HDROP file list ("these files are being dragged") for the data object.
unsafe fn set_files(data: &IDataObject, paths: &[PathBuf]) -> windows::core::Result<()> {
    let mut wide: Vec<u16> = Vec::new();
    for p in paths {
        wide.extend(p.as_os_str().to_string_lossy().encode_utf16());
        wide.push(0);
    }
    wide.push(0);
    let head = std::mem::size_of::<DROPFILES>();
    let h = GlobalAlloc(GMEM_MOVEABLE, head + wide.len() * 2)?;
    let mem = GlobalLock(h) as *mut u8;
    if mem.is_null() {
        return Err(windows::core::Error::from_win32());
    }
    let df = mem as *mut DROPFILES;
    (*df).pFiles = head as u32;
    (*df).fWide = TRUE;
    std::ptr::copy_nonoverlapping(wide.as_ptr() as *const u8, mem.add(head), wide.len() * 2);
    let _ = GlobalUnlock(h);
    let fmt = FORMATETC { cfFormat: CF_HDROP.0, ptd: std::ptr::null_mut(), dwAspect: DVASPECT_CONTENT.0, lindex: -1, tymed: TYMED_HGLOBAL.0 as u32 };
    let medium = STGMEDIUM { tymed: TYMED_HGLOBAL.0 as u32, u: STGMEDIUM_0 { hGlobal: h }, pUnkForRelease: std::mem::ManuallyDrop::new(None) };
    data.SetData(&fmt, &medium, TRUE)
}

/// What actually arrived (top-level items of the staging folder).
fn arrived(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir).map(|rd| rd.flatten().map(|e| e.path()).filter(|p| !p.to_string_lossy().ends_with(".part")).collect()).unwrap_or_default()
}

#[implement(IDropSource)]
struct DropSource {
    id: u32,
    data: IDataObject,
    dir: PathBuf,
    started: Instant,
}

impl IDropSource_Impl for DropSource_Impl {
    fn QueryContinueDrag(&self, fescapepressed: BOOL, grfkeystate: MODIFIERKEYS_FLAGS) -> HRESULT {
        if fescapepressed.as_bool() {
            return DRAGDROP_S_CANCEL;
        }
        if grfkeystate.0 & MK_LBUTTON.0 != 0 {
            return S_OK; // still carrying it
        }
        // let go: drop now if the files are here, otherwise wait for them
        match state(self.id) {
            Some(Some(true)) => {
                let files = arrived(&self.dir);
                if files.is_empty() {
                    return DRAGDROP_S_CANCEL;
                }
                let _ = unsafe { set_files(&self.data, &files) };
                DRAGDROP_S_DROP
            }
            Some(Some(false)) | None => DRAGDROP_S_CANCEL,
            Some(None) if self.started.elapsed() > Duration::from_secs(600) => DRAGDROP_S_CANCEL,
            Some(None) => S_OK,
        }
    }

    fn GiveFeedback(&self, _dweffect: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

unsafe fn pump() {
    let mut msg = MSG::default();
    while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

unsafe fn run(names: &[String], id: u32) -> Result<(), String> {
    let e = |e: windows::core::Error| e.message().to_string();
    OleInitialize(None).map_err(e)?;
    let dir = staging(id);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    // the drag starts with the names that are coming (Explorer only reads the files when you drop)
    let paths: Vec<PathBuf> = names.iter().filter_map(|n| xfer::safe_rel(n)).filter_map(|p| p.into_iter().next()).map(|n| dir.join(n)).collect();
    if paths.is_empty() {
        return Err("nothing to drag".into());
    }
    // A tiny, invisible window under the pointer, so the button press below lands on SmoothKvm and not on
    // whatever happens to be at the screen edge.
    let mut p = POINT::default();
    let _ = GetCursorPos(&mut p);
    let hwnd = CreateWindowExW(WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE, w!("STATIC"), w!(""), WS_POPUP, p.x - 8, p.y - 8, 16, 16, None, None, None, None).map_err(e)?;
    let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 1, LWA_ALPHA);
    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    pump();
    // your finger is still on the button over on the other computer: hold it down here too
    inject_win::inject(&InputEvent::MouseButton { button: MouseButton::Left, pressed: true });
    std::thread::sleep(Duration::from_millis(30));
    pump();
    let data: IDataObject = SHCreateDataObject(None, None, None).map_err(e)?;
    set_files(&data, &paths).map_err(e)?;
    let source: IDropSource = DropSource { id, data: data.clone(), dir: dir.clone(), started: Instant::now() }.into();
    // the shell's version of DoDragDrop also shows the usual picture of what is being dragged
    let effect = SHDoDragDrop(hwnd, &data, &source, DROPEFFECT_COPY).unwrap_or(DROPEFFECT_NONE);
    let _ = DestroyWindow(hwnd);
    let dropped = effect != DROPEFFECT_NONE;
    if dropped {
        shared::info(&format!("Dropped {} item(s) from the other computer", paths.len()));
        return Ok(());
    }
    // Let go where nothing takes files (or cancelled with Esc): keep them in the received-files folder.
    let deadline = Instant::now() + Duration::from_secs(600);
    while state(id) == Some(None) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
    }
    if state(id) == Some(Some(true)) {
        let inbox = xfer::inbox();
        let _ = std::fs::create_dir_all(&inbox);
        let mut kept = 0;
        for f in arrived(&dir) {
            let name = f.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            let dest = inbox.join(xfer::unique_name(&inbox, &name));
            if std::fs::rename(&f, &dest).is_ok() {
                kept += 1;
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
        shared::notify(&format!("Not dropped anywhere: {kept} item(s) kept in {}", inbox.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};

    /// The file list a drop target (Explorer) reads from the drag is exactly the files being carried.
    #[test]
    fn data_object_lists_the_files() {
        unsafe {
            let _ = OleInitialize(None);
            let dir = std::env::temp_dir().join(format!("skvm-dragtest-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let files = vec![dir.join("Part 1.step"), dir.join("Drawings")];
            let data: IDataObject = SHCreateDataObject(None, None, None).unwrap();
            set_files(&data, &files).unwrap();
            let fmt = FORMATETC { cfFormat: CF_HDROP.0, ptd: std::ptr::null_mut(), dwAspect: DVASPECT_CONTENT.0, lindex: -1, tymed: TYMED_HGLOBAL.0 as u32 };
            let medium = data.GetData(&fmt).expect("CF_HDROP is offered");
            let h = HDROP(medium.u.hGlobal.0);
            let n = DragQueryFileW(h, u32::MAX, None);
            assert_eq!(n, 2);
            for (i, want) in files.iter().enumerate() {
                let len = DragQueryFileW(h, i as u32, None) as usize;
                let mut buf = vec![0u16; len + 1];
                DragQueryFileW(h, i as u32, Some(&mut buf));
                assert_eq!(String::from_utf16_lossy(&buf[..len]), want.to_string_lossy());
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
