//! Android entry point. The system's NativeActivity loads this library and the activity glue
//! calls `android_main` on a thread of its own.

#[cfg(target_os = "android")]
#[no_mangle]
fn android_main(app: tssm::AndroidApp) {
    tssm::android_main(app);
}
