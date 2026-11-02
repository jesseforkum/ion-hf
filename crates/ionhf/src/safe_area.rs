//! How much of the screen's top and bottom the system covers, in points:
//! none on the desktop; on Android the status bar, the gesture bar and the
//! on-screen keyboard, which all draw over the app's window.

use eframe::egui;

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
    crate::android::with_activity(|env, activity| {
        let window = env
            .call_method(activity, "getWindow", "()Landroid/view/Window;", &[])?
            .l()?;
        let decor = env
            .call_method(&window, "getDecorView", "()Landroid/view/View;", &[])?
            .l()?;
        let insets = env
            .call_method(&decor, "getRootWindowInsets", "()Landroid/view/WindowInsets;", &[])?
            .l()?;
        if insets.is_null() {
            return Err(jni::errors::Error::NullPtr("root window insets"));
        }
        let top = env.call_method(&insets, "getSystemWindowInsetTop", "()I", &[])?.i()?;
        let bottom = env
            .call_method(&insets, "getSystemWindowInsetBottom", "()I", &[])?
            .i()?;
        Ok((top, bottom))
    })
}

#[cfg(not(target_os = "android"))]
pub fn insets(_ctx: &egui::Context) -> (f32, f32) {
    (0.0, 0.0)
}
