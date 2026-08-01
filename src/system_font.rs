//! The `extern "C"` shims installed via `fz_install_load_system_font_funcs`.
//!
//! Each shim translates MuPDF's raw callback arguments into safe types and
//! dispatches to the [`FontLoader`](crate::FontLoader) chain (the registered
//! user loader first, then the built-in loaders). On non-Android, non-wasm
//! targets, `SystemFontLoader` is the built-in loader backed by `font-kit` for
//! fonts installed on the system.

use std::ffi::CStr;
use std::os::raw::{c_char, c_int};
use std::ptr;

use mupdf_sys::*;

use crate::font_loader::{self, FontHints};
use crate::{CjkFontOrdering, Font};

/// Hand a `Font` over to MuPDF: return a pointer carrying one owned
/// reference for the caller, releasing our own when `font` drops.
fn font_into_mupdf(ctx: *mut fz_context, font: Font) -> *mut fz_font {
    // SAFETY: `ctx` and `font.inner` are valid; font reference counting in
    // MuPDF is thread-safe across cloned contexts.
    unsafe { fz_keep_font(ctx, font.inner) };
    font.inner
}

/// Font lookups run arbitrary user `FontLoader` code inside an `extern "C"`
/// callback, where unwinding would abort the process. Treat a panic as a
/// failed lookup instead so MuPDF can fall back to its built-in handling.
fn catch_panic(f: impl FnOnce() -> Option<Font>) -> Option<Font> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or(None)
}

pub(crate) unsafe extern "C" fn load_system_font(
    ctx: *mut fz_context,
    name: *const c_char,
    bold: c_int,
    italic: c_int,
    needs_exact_metrics: c_int,
) -> *mut fz_font {
    if name.is_null() {
        return ptr::null_mut();
    }

    let Ok(name) = unsafe { CStr::from_ptr(name) }.to_str() else {
        return ptr::null_mut();
    };

    let hints = FontHints {
        bold: bold != 0,
        italic: italic != 0,
        serif: false,
        needs_exact_metrics: needs_exact_metrics != 0,
    };
    match catch_panic(|| font_loader::dispatch(|loader| loader.load_font(name, hints))) {
        Some(font) => font_into_mupdf(ctx, font),
        None => ptr::null_mut(),
    }
}

pub(crate) unsafe extern "C" fn load_system_cjk_font(
    ctx: *mut fz_context,
    name: *const c_char,
    ordering: c_int,
    serif: c_int,
) -> *mut fz_font {
    let name = if name.is_null() {
        ""
    } else {
        // SAFETY: MuPDF passes a valid NUL-terminated string.
        unsafe { CStr::from_ptr(name) }.to_str().unwrap_or("")
    };

    let ordering = CjkFontOrdering::try_from(ordering).ok();
    match catch_panic(|| font_loader::dispatch_cjk(name, ordering, serif != 0)) {
        Some(font) => font_into_mupdf(ctx, font),
        None => ptr::null_mut(),
    }
}

pub(crate) unsafe extern "C" fn load_system_fallback_font(
    ctx: *mut fz_context,
    script: c_int,
    language: c_int,
    serif: c_int,
    bold: c_int,
    italic: c_int,
) -> *mut fz_font {
    let hints = FontHints {
        bold: bold != 0,
        italic: italic != 0,
        serif: serif != 0,
        needs_exact_metrics: false,
    };
    match catch_panic(|| {
        font_loader::dispatch(|loader| {
            loader.load_fallback_font(script as u32, language as u32, hints)
        })
    }) {
        Some(font) => font_into_mupdf(ctx, font),
        None => ptr::null_mut(),
    }
}

/// Installs this crate's font-resolution hooks (the registered
/// [`FontLoader`](crate::FontLoader), then the built-in `system-fonts` /
/// `bundled-fonts-*` loaders) onto an arbitrary `fz_context`.
///
/// This crate's own [`crate::Context`] does this automatically for the
/// `fz_context` it creates internally.
///
/// # This is not sufficient on its own for an independently created `fz_context`
///
/// The hook implementations construct their `fz_font` objects using this
/// crate's *own* context (see [`crate::new_context`]'s docs for why), not the
/// `ctx` MuPDF happens to pass into the hook at call time. If `ctx` belongs to
/// a different context group than this crate's (e.g. a bare, independent
/// `fz_new_context()` call rather than a clone of this crate's shared
/// context), the `fz_font` objects handed back are **not valid for use with
/// `ctx`** even though installation "succeeds" and MuPDF will call these
/// hooks without complaint -- using them typically corrupts memory, crashes,
/// or deadlocks rather than failing loudly.
///
/// For any `fz_context` your own code creates, prefer obtaining it via
/// [`crate::new_context`] (a clone of this crate's shared context group) in
/// the first place, which needs no separate hook installation step and is
/// safe to mix with objects from this crate. Only reach for this function if
/// you specifically need hooks on a context you know is already a clone of
/// this crate's shared context group (in which case it is a no-op, since
/// clones already have them) or you have otherwise verified the context
/// groups are compatible.
///
/// # Safety
/// `ctx` must be a valid, non-null `fz_context` pointer, and must not be
/// dropped/freed while still in use by MuPDF.
pub unsafe fn install_system_font_funcs(ctx: *mut fz_context) {
    unsafe {
        fz_install_load_system_font_funcs(
            ctx,
            Some(load_system_font),
            Some(load_system_cjk_font),
            Some(load_system_fallback_font),
        );
    }
}

/// Looks up fonts installed on the system via `font-kit`.
// `font-kit` is only a dependency on non-Android, non-wasm targets.
#[cfg(all(
    feature = "system-fonts",
    not(target_arch = "wasm32"),
    not(target_os = "android")
))]
pub(crate) struct SystemFontLoader;

#[cfg(all(
    feature = "system-fonts",
    not(target_arch = "wasm32"),
    not(target_os = "android")
))]
impl font_loader::FontLoader for SystemFontLoader {
    fn load_font(&self, name: &str, hints: FontHints) -> Option<Font> {
        use font_kit::family_name::FamilyName;
        use font_kit::handle::Handle;
        use font_kit::properties::{Properties, Style, Weight};
        use font_kit::source::SystemSource;

        let mut name = name;
        let font_source = SystemSource::new();
        let handle = match font_source.select_by_postscript_name(name) {
            Ok(handle) => handle,
            Err(_) => {
                for suffix in &["MT", "PS", "IdentityH"] {
                    if name.ends_with(suffix) {
                        name = name.strip_suffix(suffix).unwrap_or(name);
                    }
                }
                let mut properties = Properties::new();
                let properties = properties
                    .weight(if hints.bold {
                        Weight::BOLD
                    } else {
                        Weight::NORMAL
                    })
                    .style(if hints.italic {
                        Style::Italic
                    } else {
                        Style::Normal
                    });
                font_source
                    .select_best_match(&[FamilyName::Title(name.to_string())], properties)
                    .ok()?
            }
        };

        let font_index = match handle {
            Handle::Path { font_index, .. } => font_index,
            Handle::Memory { font_index, .. } => font_index,
        };
        let loaded = handle.load().ok()?;
        let font_data = loaded.copy_font_data()?;
        let font =
            Font::from_bytes_with_index(&loaded.family_name(), font_index as i32, &font_data)
                .ok()?;

        if hints.needs_exact_metrics
            && ((hints.bold && !font.is_bold()) || (hints.italic && !font.is_italic()))
        {
            return None;
        }
        Some(font)
    }

    #[cfg(windows)]
    fn load_cjk_font(&self, _name: &str, ordering: CjkFontOrdering, serif: bool) -> Option<Font> {
        let names: &[&str] = if serif {
            match ordering {
                CjkFontOrdering::AdobeCns => &["MingLiU"],
                CjkFontOrdering::AdobeGb => &["SimSun"],
                CjkFontOrdering::AdobeJapan => &["MS-Mincho"],
                CjkFontOrdering::AdobeKorea => &["Batang"],
            }
        } else {
            match ordering {
                CjkFontOrdering::AdobeCns => &["DFKaiShu-SB-Estd-BF"],
                CjkFontOrdering::AdobeGb => &["KaiTi", "KaiTi_GB2312"],
                CjkFontOrdering::AdobeJapan => &["MS-Gothic"],
                CjkFontOrdering::AdobeKorea => &["Gulim"],
            }
        };
        names
            .iter()
            .find_map(|name| self.load_font(name, FontHints::default()))
    }
}

#[cfg(all(test, feature = "bundled-fonts-droid", feature = "bundled-fonts-noto"))]
mod tests {
    use std::ffi::{CStr, CString};

    use super::*;

    #[test]
    fn bundled_cjk_hook_prefers_explicit_font_name() {
        let name = CString::new("Noto Sans").unwrap();
        let ctx = crate::context();
        // SAFETY: `ctx` is the process-global MuPDF context and `name` is a valid C string.
        let font = unsafe { load_system_cjk_font(ctx, name.as_ptr(), FZ_ADOBE_JAPAN as c_int, 0) };
        assert!(!font.is_null());

        // SAFETY: `font` is non-null and owned by this test until it is dropped below.
        let actual = unsafe { CStr::from_ptr(fz_font_name(ctx, font)) }
            .to_str()
            .unwrap()
            .to_owned();
        // SAFETY: `font` was returned with an owned reference from the system CJK hook.
        unsafe { fz_drop_font(ctx, font) };

        assert_eq!(actual, "Noto Sans");
    }
}
