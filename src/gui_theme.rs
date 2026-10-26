//! Lightweight, paint-only styling for the native GUI. Input, accessibility and
//! activation remain the responsibility of the original Win32 controls.

use std::{cell::Cell, rc::Rc};

use native_windows_gui as nwg;
use windows::Win32::{
    Foundation::{COLORREF, HWND, LPARAM, POINT, RECT, WPARAM},
    Graphics::Gdi::{
        ClientToScreen, CreateSolidBrush, DeleteObject, DrawFocusRect, DrawTextW, ExcludeClipRect,
        FillRect, FrameRect, GetStockObject, GetSysColor, GetWindowDC, RedrawWindow, ReleaseDC,
        RestoreDC, SaveDC, SelectObject, SetBkColor, SetBkMode, SetDCBrushColor, SetTextColor,
        COLOR_GRAYTEXT, DC_BRUSH, DT_CENTER, DT_HIDEPREFIX, DT_SINGLELINE, DT_VCENTER, HBRUSH, HDC,
        HGDIOBJ, RDW_ALLCHILDREN, RDW_ERASE, RDW_FRAME, RDW_INVALIDATE, TRANSPARENT,
    },
    UI::{
        Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW},
        Controls::{
            CDDS_PREPAINT, CDIS_DISABLED, CDIS_FOCUS, CDIS_HOT, CDIS_SELECTED, CDRF_SKIPDEFAULT,
            NMCUSTOMDRAW, NMHDR, NM_CUSTOMDRAW,
        },
        Input::KeyboardAndMouse::IsWindowEnabled,
        WindowsAndMessaging::{
            GetClassNameW, GetClientRect, GetParent, GetWindowRect, GetWindowTextLengthW,
            GetWindowTextW, IsWindow, SendMessageW, SystemParametersInfoW, SPI_GETHIGHCONTRAST,
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, UISF_HIDEACCEL, UISF_HIDEFOCUS, WM_CTLCOLORBTN,
            WM_CTLCOLORSTATIC, WM_ERASEBKGND, WM_GETFONT, WM_NCPAINT, WM_NOTIFY, WM_QUERYUISTATE,
            WM_SETTINGCHANGE, WM_SYSCOLORCHANGE, WM_THEMECHANGED,
        },
    },
};

const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(r as u32 | ((g as u32) << 8) | ((b as u32) << 16))
}

const BACKGROUND: COLORREF = rgb(243, 245, 248);
const SURFACE: COLORREF = rgb(255, 255, 255);
const TEXT: COLORREF = rgb(31, 41, 55);
const MUTED: COLORREF = rgb(94, 105, 120);
const BLUE: COLORREF = rgb(29, 78, 216);
// NWG reserves IDs <= 0xffff. A fixed ID also rejects double installation.
const HANDLER_ID: usize = 0x4d4d_5448;

struct Brush(HBRUSH);

impl Brush {
    fn new(color: COLORREF) -> Result<Self, nwg::NwgError> {
        // SAFETY: CreateSolidBrush takes a value and returns an owned GDI handle.
        let brush = unsafe { CreateSolidBrush(color) };
        if brush.0.is_null() {
            Err(nwg::NwgError::resource_create("GUI theme brush"))
        } else {
            Ok(Self(brush))
        }
    }
}

impl Drop for Brush {
    fn drop(&mut self) {
        // SAFETY: These brushes are owned, never selected into a DC, and are
        // released only after the last event callback using them is unbound.
        unsafe {
            let _ = DeleteObject(self.0);
        }
    }
}

struct Palette {
    background: Brush,
    surface: Brush,
    native: Cell<bool>,
    window: HWND,
    cards: Vec<HWND>,
    primary: HWND,
    secondary: Vec<HWND>,
    muted: Vec<HWND>,
    headings: Vec<HWND>,
}

impl Palette {
    fn background_for(&self, parent: HWND) -> (&Brush, COLORREF) {
        if self.cards.contains(&parent) {
            (&self.surface, SURFACE)
        } else {
            (&self.background, BACKGROUND)
        }
    }
}

/// Retain on the GUI thread until the UI closes. Drop **before** the window and
/// controls (e.g. declare `RefCell<Option<GuiTheme>>` before the HWND fields).
/// Does not change fonts, window titles, control styles or keyboard/click handling.
pub struct GuiTheme {
    handlers: Vec<nwg::RawEventHandler>,
    palette: Rc<Palette>,
}

impl GuiTheme {
    /// Install after all controls exist. `cards` are native Frame containers;
    /// `primary` and `secondary` are ordinary text push buttons. Headings retain
    /// their existing font; only their foreground/background are styled here.
    /// Other controls retain native input, selection, disabled and focus behavior.
    pub fn install(
        window: &nwg::ControlHandle,
        cards: &[&nwg::ControlHandle],
        primary: &nwg::ControlHandle,
        secondary: &[&nwg::ControlHandle],
        muted_labels: &[&nwg::ControlHandle],
        headings: &[&nwg::ControlHandle],
    ) -> Result<Self, nwg::NwgError> {
        let palette = Rc::new(Palette {
            background: Brush::new(BACKGROUND)?,
            surface: Brush::new(SURFACE)?,
            native: Cell::new(high_contrast()),
            window: hwnd(window)?,
            cards: handles(cards)?,
            primary: hwnd(primary)?,
            secondary: handles(secondary)?,
            muted: handles(muted_labels)?,
            headings: handles(headings)?,
        });
        let mut theme = Self {
            handlers: Vec::new(),
            palette,
        };
        let mut parents = vec![theme.palette.window];
        parents.extend(theme.palette.cards.iter().copied());
        // Notifications are sent to the direct parent, not necessarily the root.
        for child in std::iter::once(&theme.palette.primary)
            .chain(theme.palette.secondary.iter())
            .chain(theme.palette.muted.iter())
            .chain(theme.palette.headings.iter())
        {
            // SAFETY: All handles were validated above, on the GUI thread.
            if let Ok(parent) = unsafe { GetParent(*child) } {
                if !parents.contains(&parent) {
                    parents.push(parent);
                }
            }
        }
        parents.sort_unstable_by_key(|h| h.0 as usize);
        parents.dedup();
        for parent in parents {
            let palette = Rc::clone(&theme.palette);
            let handle = nwg::ControlHandle::Hwnd(parent.0.cast());
            theme.handlers.push(nwg::bind_raw_event_handler(
                &handle,
                HANDLER_ID,
                move |raw, msg, w, l| paint_parent(&palette, HWND(raw.cast()), msg, w, l),
            )?);
        }
        // NWG vertically centers Labels using a non-client margin. Give that
        // margin the same color as the client area without replacing text paint.
        let mut labels = theme.palette.muted.clone();
        labels.extend(theme.palette.headings.iter().copied());
        labels.sort_unstable_by_key(|h| h.0 as usize);
        labels.dedup();
        for label in labels {
            let palette = Rc::clone(&theme.palette);
            let handle = nwg::ControlHandle::Hwnd(label.0.cast());
            theme.handlers.push(nwg::bind_raw_event_handler(
                &handle,
                HANDLER_ID,
                move |raw, msg, _, _| {
                    if msg == WM_NCPAINT && !palette.native.get() {
                        paint_label_margin(&palette, HWND(raw.cast()))
                    } else {
                        None
                    }
                },
            )?);
        }
        redraw(theme.palette.window);
        Ok(theme)
    }
}

impl Drop for GuiTheme {
    fn drop(&mut self) {
        for handler in self.handlers.drain(..).rev() {
            let _ = nwg::unbind_raw_event_handler(&handler);
        }
        redraw(self.palette.window);
    }
}

fn hwnd(handle: &nwg::ControlHandle) -> Result<HWND, nwg::NwgError> {
    let raw = handle
        .hwnd()
        .ok_or_else(|| nwg::NwgError::events_binding("Theme requires an HWND"))?;
    let h = HWND(raw.cast());
    // SAFETY: IsWindow accepts an arbitrary handle and does not dereference it.
    if unsafe { IsWindow(h).as_bool() } {
        Ok(h)
    } else {
        Err(nwg::NwgError::events_binding("Theme requires a live HWND"))
    }
}

fn handles(values: &[&nwg::ControlHandle]) -> Result<Vec<HWND>, nwg::NwgError> {
    values.iter().map(|h| hwnd(h)).collect()
}

fn high_contrast() -> bool {
    let mut info = HIGHCONTRASTW {
        cbSize: std::mem::size_of::<HIGHCONTRASTW>() as u32,
        ..Default::default()
    };
    // SAFETY: The writable buffer has the exact size expected by this SPI action.
    // If querying accessibility fails, prefer native rendering over fixed colors.
    unsafe {
        SystemParametersInfoW(
            SPI_GETHIGHCONTRAST,
            info.cbSize,
            Some((&mut info as *mut HIGHCONTRASTW).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .is_err()
            || info.dwFlags.contains(HCF_HIGHCONTRASTON)
    }
}

fn redraw(window: HWND) {
    // SAFETY: No borrowed buffers or immediate repaint; invalidation is queued.
    unsafe {
        if IsWindow(window).as_bool() {
            let _ = RedrawWindow(
                window,
                None,
                None,
                RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN | RDW_FRAME,
            );
        }
    }
}

fn paint_parent(p: &Palette, parent: HWND, msg: u32, w: usize, l: isize) -> Option<isize> {
    if matches!(msg, WM_SETTINGCHANGE | WM_SYSCOLORCHANGE | WM_THEMECHANGED) {
        p.native.set(high_contrast());
        redraw(p.window);
        return None;
    }
    if p.native.get() {
        return None;
    }
    // SAFETY: This function is only called by NWG's synchronous window callback.
    // Message parameters are interpreted only for the matching Win32 message;
    // notification structures and DCs are borrowed for this call, never retained.
    unsafe {
        match msg {
            WM_ERASEBKGND if parent == p.window || p.cards.contains(&parent) => {
                let mut rect = RECT::default();
                if GetClientRect(parent, &mut rect).is_err() {
                    return None;
                }
                let brush = p.background_for(parent).0;
                (FillRect(HDC(w as _), &rect, brush.0) != 0).then_some(1)
            }
            WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => {
                let child = HWND(l as _);
                // Checkboxes, radio buttons and read-only edits also use STATIC.
                // Disabled edits keep the native disabled background.
                if msg == WM_CTLCOLORSTATIC {
                    let mut class = [0u16; 32];
                    let len = GetClassNameW(child, &mut class) as usize;
                    let class = String::from_utf16_lossy(&class[..len]);
                    if !class.eq_ignore_ascii_case("Static")
                        && !class.eq_ignore_ascii_case("Button")
                        && !(class.eq_ignore_ascii_case("Edit") && IsWindowEnabled(child).as_bool())
                    {
                        return None;
                    }
                }
                let dc = HDC(w as _);
                let (brush, bg) = p.background_for(parent);
                let color = if !IsWindowEnabled(child).as_bool() {
                    COLORREF(GetSysColor(COLOR_GRAYTEXT))
                } else if p.muted.contains(&child) {
                    MUTED
                } else {
                    TEXT
                };
                SetTextColor(dc, color);
                SetBkColor(dc, bg);
                SetBkMode(dc, TRANSPARENT);
                Some(brush.0 .0 as isize)
            }
            WM_NOTIFY if l != 0 => {
                let header = &*(l as *const NMHDR);
                if header.code != NM_CUSTOMDRAW
                    || (header.hwndFrom != p.primary && !p.secondary.contains(&header.hwndFrom))
                {
                    return None;
                }
                let draw = &*(l as *const NMCUSTOMDRAW);
                if draw.dwDrawStage != CDDS_PREPAINT {
                    return None;
                }
                paint_button(draw, header.hwndFrom == p.primary)
            }
            _ => None,
        }
    }
}

fn paint_button(draw: &NMCUSTOMDRAW, primary: bool) -> Option<isize> {
    // SAFETY: The custom-draw DC/rectangle are live for this notification. SaveDC
    // and RestoreDC keep every color/font/brush change local to this paint only.
    unsafe {
        let dc = draw.hdc;
        let saved = SaveDC(dc);
        if saved == 0 {
            return None;
        }
        let disabled = draw.uItemState.contains(CDIS_DISABLED)
            || !IsWindowEnabled(draw.hdr.hwndFrom).as_bool();
        let pressed = draw.uItemState.contains(CDIS_SELECTED);
        let hot = draw.uItemState.contains(CDIS_HOT);
        let bg = if disabled {
            rgb(229, 233, 239)
        } else if primary && pressed {
            rgb(30, 58, 138)
        } else if primary && hot {
            rgb(30, 64, 175)
        } else if primary {
            BLUE
        } else if pressed {
            rgb(226, 232, 240)
        } else if hot {
            rgb(241, 245, 249)
        } else {
            SURFACE
        };
        let fg = if disabled {
            MUTED
        } else if primary {
            SURFACE
        } else {
            TEXT
        };
        let brush = HBRUSH(GetStockObject(DC_BRUSH).0);
        SetDCBrushColor(dc, bg);
        FillRect(dc, &draw.rc, brush);
        SetDCBrushColor(
            dc,
            if primary && !disabled {
                bg
            } else {
                rgb(190, 199, 212)
            },
        );
        FrameRect(dc, &draw.rc, brush);

        let font = SendMessageW(draw.hdr.hwndFrom, WM_GETFONT, WPARAM(0), LPARAM(0));
        if font.0 != 0 {
            SelectObject(dc, HGDIOBJ(font.0 as _));
        }
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, fg);
        let ui = SendMessageW(draw.hdr.hwndFrom, WM_QUERYUISTATE, WPARAM(0), LPARAM(0)).0 as u32;
        let len = GetWindowTextLengthW(draw.hdr.hwndFrom).max(0) as usize;
        let mut text = vec![0u16; len + 1];
        let copied = GetWindowTextW(draw.hdr.hwndFrom, &mut text).max(0) as usize;
        text.truncate(copied);
        let mut rect = draw.rc;
        rect.left += 6;
        rect.right -= 6;
        if pressed && !disabled {
            rect.top += 1;
            rect.bottom += 1;
        }
        let mut flags = DT_CENTER | DT_VCENTER | DT_SINGLELINE;
        if ui & UISF_HIDEACCEL != 0 {
            flags |= DT_HIDEPREFIX;
        }
        DrawTextW(dc, &mut text, &mut rect, flags);
        if !disabled && draw.uItemState.contains(CDIS_FOCUS) && ui & UISF_HIDEFOCUS == 0 {
            let focus = RECT {
                left: draw.rc.left + 4,
                top: draw.rc.top + 4,
                right: draw.rc.right - 4,
                bottom: draw.rc.bottom - 4,
            };
            SetTextColor(dc, fg);
            SetBkColor(dc, bg);
            let _ = DrawFocusRect(dc, &focus);
        }
        let _ = RestoreDC(dc, saved);
        Some(CDRF_SKIPDEFAULT as isize)
    }
}

fn paint_label_margin(p: &Palette, label: HWND) -> Option<isize> {
    // SAFETY: Only paints the label's non-client margin. The DC is acquired and
    // released here, with its client rectangle excluded so native text is intact.
    unsafe {
        let parent = GetParent(label).ok()?;
        let mut window = RECT::default();
        let mut client = RECT::default();
        GetWindowRect(label, &mut window).ok()?;
        GetClientRect(label, &mut client).ok()?;
        let mut origin = POINT::default();
        if !ClientToScreen(label, &mut origin).as_bool() {
            return None;
        }
        let dc = GetWindowDC(label);
        if dc.0.is_null() {
            return None;
        }
        let x = origin.x - window.left;
        let y = origin.y - window.top;
        ExcludeClipRect(dc, x, y, x + client.right, y + client.bottom);
        let rect = RECT {
            left: 0,
            top: 0,
            right: window.right - window.left,
            bottom: window.bottom - window.top,
        };
        FillRect(dc, &rect, p.background_for(parent).0 .0);
        ReleaseDC(label, dc);
        Some(0)
    }
}
