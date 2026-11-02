//! The Android app's native library: `libionhf_android.so`, loaded by
//! NativeActivity.

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: android_activity::AndroidApp) {
    ionhf::android_main(app);
}
