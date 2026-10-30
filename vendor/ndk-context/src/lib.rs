//! Provides a stable api to rust crates for interfacing with the Android platform. It is
//! initialized by the runtime, usually [__ndk-glue__](https://crates.io/crates/ndk-glue),
//! but could also be initialized by Java or Kotlin code when embedding in an existing Android
//! project.
//!
//! ```no_run
//! let ctx = ndk_context::android_context();
//! let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }?;
//! let env = vm.attach_current_thread();
//! let class_ctx = env.find_class("android/content/Context")?;
//! let audio_service = env.get_static_field(class_ctx, "AUDIO_SERVICE", "Ljava/lang/String;")?;
//! let audio_manager = env
//!     .call_method(
//!         ctx.context() as jni::sys::jobject,
//!         "getSystemService",
//!         "(Ljava/lang/String;)Ljava/lang/Object;",
//!         &[audio_service],
//!     )?
//!     .l()?;
//! ```
use std::ffi::c_void;

// A Service may initialize the process before GameActivity's native glue.
// Keep the Application ref installed by the first owner and allow the glue's
// later initialization with the same VM. A Mutex also avoids unsynchronised
// reads while Service and Activity start on different threads.
static ANDROID_CONTEXT: std::sync::Mutex<Option<(usize, usize)>> = std::sync::Mutex::new(None);

/// [`AndroidContext`] provides the pointers required to interface with the jni on Android
/// platforms.
#[derive(Clone, Copy, Debug)]
pub struct AndroidContext {
    java_vm: *mut c_void,
    context_jobject: *mut c_void,
}

impl AndroidContext {
    /// A handle to the `JavaVM` object.
    ///
    /// Usage with [__jni__](https://crates.io/crates/jni) crate:
    /// ```no_run
    /// let ctx = ndk_context::android_context();
    /// let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }?;
    /// let env = vm.attach_current_thread();
    /// ```
    pub fn vm(self) -> *mut c_void {
        self.java_vm
    }

    /// A handle to an [android.content.Context](https://developer.android.com/reference/android/content/Context).
    /// In most cases this will be a ptr to an `Activity`, but this isn't guaranteed.
    ///
    /// Usage with [__jni__](https://crates.io/crates/jni) crate:
    /// ```no_run
    /// let ctx = ndk_context::android_context();
    /// let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }?;
    /// let env = vm.attach_current_thread();
    /// let class_ctx = env.find_class("android/content/Context")?;
    /// let audio_service = env.get_static_field(class_ctx, "AUDIO_SERVICE", "Ljava/lang/String;")?;
    /// let audio_manager = env
    ///     .call_method(
    ///         ctx.context() as jni::sys::jobject,
    ///         "getSystemService",
    ///         "(Ljava/lang/String;)Ljava/lang/Object;",
    ///         &[audio_service],
    ///     )?
    ///     .l()?;
    /// ```
    pub fn context(self) -> *mut c_void {
        self.context_jobject
    }
}

/// Main entry point to this crate. Returns an [`AndroidContext`].
pub fn android_context() -> AndroidContext {
    let (java_vm, context_jobject) = ANDROID_CONTEXT
        .lock()
        .unwrap()
        .expect("android context was not initialized");
    AndroidContext {
        java_vm: java_vm as *mut c_void,
        context_jobject: context_jobject as *mut c_void,
    }
}

/// Whether a Service or the native Activity glue installed the process VM.
pub fn is_initialized() -> bool {
    ANDROID_CONTEXT.lock().unwrap().is_some()
}

/// Initializes the [`AndroidContext`]. [`AndroidContext`] is initialized by [__ndk-glue__](https://crates.io/crates/ndk-glue)
/// before `main` is called.
///
/// # Safety
///
/// The pointers must be valid and this function must be called exactly once before `main` is
/// called.
pub unsafe fn initialize_android_context(java_vm: *mut c_void, context_jobject: *mut c_void) {
    let mut context = ANDROID_CONTEXT.lock().unwrap();
    match *context {
        None => *context = Some((java_vm as usize, context_jobject as usize)),
        Some((vm, _)) => assert_eq!(vm, java_vm as usize, "different Android VM"),
    }
}

/// Removes the [`AndroidContext`]. It is released by [__ndk-glue__](https://crates.io/crates/ndk-glue)
/// when the activity is finished and destroyed.
///
/// # Safety
///
/// This function must only be called after [`initialize_android_context()`],
/// when the activity is subsequently destroyed according to Android.
pub unsafe fn release_android_context() {
    // This process retains the first Application global ref for headless work.
    // Activity teardown must not invalidate it while the Service is alive.
    assert!(ANDROID_CONTEXT.lock().unwrap().is_some());
}
