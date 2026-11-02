//! The Android side: the activity, and calls into its Java over JNI
//! (`android/java`).

use std::sync::OnceLock;

use android_activity::AndroidApp;
use jni::JNIEnv;
use jni::errors::Error;
use jni::objects::JObject;

static APP: OnceLock<AndroidApp> = OnceLock::new();

pub fn set_app(app: AndroidApp) {
    let _ = APP.set(app);
}

/// Runs `f` with this thread's JNI environment and the activity, in a local
/// frame: threads stay attached, so references made in `f` would otherwise
/// pile up until the table overflows. A Java exception comes back as None.
pub fn with_activity<T>(f: impl FnOnce(&mut JNIEnv, &JObject) -> Result<T, Error>) -> Option<T> {
    let app = APP.get()?;
    // SAFETY: the process's VM and the app's activity, both alive while the
    // app runs.
    let vm = unsafe { jni::JavaVM::from_raw(app.vm_as_ptr().cast()) }.ok()?;
    let mut env = vm.attach_current_thread_permanently().ok()?;
    let activity = unsafe { JObject::from_raw(app.activity_as_ptr().cast()) };
    let got = env.with_local_frame(16, |env| f(env, &activity));
    if got.is_err() {
        // A Java exception left pending would abort the next JNI call.
        let _ = env.exception_clear();
    }
    got.ok()
}

/// Closes the app and opens it again: radio settings take effect at start.
pub fn restart() {
    with_activity(|env, a| env.call_method(a, "restart", "()V", &[]).map(|_| ()));
}
