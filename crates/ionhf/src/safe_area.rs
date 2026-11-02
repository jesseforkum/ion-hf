//! How much of the screen's top and bottom the system covers, in points:
//! none on the desktop; on Android the status bar, the gesture bar and the
//! on-screen keyboard, which all draw over the app's window.

use eframe::egui;

#[cfg(target_os = "android")]
static APP: std::sync::OnceLock<android_activity::AndroidApp> = std::sync::OnceLock::new();

#[cfg(target_os = "android")]
pub fn set_app(app: android_activity::AndroidApp) {
    let _ = APP.set(app);
}

/// Top and bottom insets, points.
#[cfg(target_os = "android")]
pub fn insets(ctx: &egui::Context) -> (f32, f32) {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    // Through JNI, so asked twice a second, not every frame.
    static LAST: Mutex<Option<(Instant, (i32, i32))>> = Mutex::new(None);
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let px = match *last {
        Some((at, px)) if at.elapsed() < Duration::from_millis(500) => px,
        _ => {
            let px = system_insets_px().unwrap_or((0, 0));
            *last = Some((Instant::now(), px));
            px
        }
    };
    let ppp = ctx.pixels_per_point();
    (px.0 as f32 / ppp, px.1 as f32 / ppp)
}

/// The decor view's system window insets, top and bottom, pixels.
#[cfg(target_os = "android")]
fn system_insets_px() -> Option<(i32, i32)> {
    use jni::errors::Error;
    use jni::objects::JObject;
    let app = APP.get()?;
    // SAFETY: the process's VM and the app's activity, both alive while
    // the app runs.
    let vm = unsafe { jni::JavaVM::from_raw(app.vm_as_ptr().cast()) }.ok()?;
    let mut env = vm.attach_current_thread_permanently().ok()?;
    let activity = unsafe { JObject::from_raw(app.activity_as_ptr().cast()) };
    // In a local frame: this thread stays attached, so references made here
    // would otherwise pile up until the table overflows.
    let got = env.with_local_frame(8, |env| -> Result<(i32, i32), Error> {
        let window = env
            .call_method(&activity, "getWindow", "()Landroid/view/Window;", &[])?
            .l()?;
        let decor = env
            .call_method(&window, "getDecorView", "()Landroid/view/View;", &[])?
            .l()?;
        let insets = env
            .call_method(&decor, "getRootWindowInsets", "()Landroid/view/WindowInsets;", &[])?
            .l()?;
        if insets.is_null() {
            return Err(Error::NullPtr("root window insets"));
        }
        let top = env.call_method(&insets, "getSystemWindowInsetTop", "()I", &[])?.i()?;
        let bottom = env
            .call_method(&insets, "getSystemWindowInsetBottom", "()I", &[])?
            .i()?;
        Ok((top, bottom))
    });
    if got.is_err() {
        // A Java exception left pending would abort the next JNI call.
        let _ = env.exception_clear();
    }
    got.ok()
}

#[cfg(not(target_os = "android"))]
pub fn insets(_ctx: &egui::Context) -> (f32, f32) {
    (0.0, 0.0)
}
