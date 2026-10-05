//! Android Keystore wrapping for chatter-core's at-rest secrets.
//!
//! Implements `chatter_core::system::sealed_secret::KeyWrap` over the Kotlin
//! `com.chatter.client.KeyStoreHelper` (AES-256-GCM, StrongBox when present,
//! TEE otherwise). Installed in `android_main` before `app_init`, because the
//! database passphrase is unsealed while the client is built.
//!
//! The class is resolved through the Application's classloader rather than
//! `FindClass`: this runs on the native thread, whose `FindClass` sees only the
//! system classloader, and before `MainActivity` is guaranteed to have
//! registered itself.

use chatter_core::system::sealed_secret::{self, KeyWrap};
use jni::JNIEnv;
use jni::objects::{JByteArray, JClass, JObject, JValue};

use super::activity::{clear_exception, jni_env};

const HELPER_CLASS: &str = "com.chatter.client.KeyStoreHelper";

struct AndroidKeystore;

impl KeyWrap for AndroidKeystore {
    fn seal(&self, alias: &str, plaintext: &[u8]) -> Result<Vec<u8>, String> {
        call("seal", alias, plaintext)
    }

    fn unseal(&self, alias: &str, blob: &[u8]) -> Result<Vec<u8>, String> {
        call("unseal", alias, blob)
    }
}

/// Register the Keystore as chatter-core's key wrapper. Call once at startup.
pub fn init() {
    sealed_secret::install_key_wrap(Box::new(AndroidKeystore));
}

fn call(method: &str, alias: &str, input: &[u8]) -> Result<Vec<u8>, String> {
    let mut env = jni_env()?;
    let result = invoke(&mut env, method, alias, input);
    if result.is_err() {
        clear_exception(&mut env);
    }
    result.map_err(|e| format!("KeyStoreHelper.{method}: {e}"))
}

/// One JNI round trip inside its own local frame, so the attached native
/// thread does not accumulate local references.
fn invoke(
    env: &mut JNIEnv<'static>,
    method: &str,
    alias: &str,
    input: &[u8],
) -> jni::errors::Result<Vec<u8>> {
    env.with_local_frame(16, |env| -> jni::errors::Result<Vec<u8>> {
        let ctx = ndk_context::android_context().context();
        // SAFETY: ndk_context holds a global ref to the Application for the
        // process lifetime; we only borrow it.
        let app = unsafe { JObject::from_raw(ctx.cast()) };
        let loader = env
            .call_method(&app, "getClassLoader", "()Ljava/lang/ClassLoader;", &[])
            .and_then(|v| v.l())?;
        let name = env.new_string(HELPER_CLASS)?;
        let class: JClass = env
            .call_method(
                &loader,
                "loadClass",
                "(Ljava/lang/String;)Ljava/lang/Class;",
                &[JValue::Object(&name)],
            )
            .and_then(|v| v.l())?
            .into();
        let j_alias = env.new_string(alias)?;
        let j_input = env.byte_array_from_slice(input)?;
        let out: JByteArray = env
            .call_static_method(
                &class,
                method,
                "(Ljava/lang/String;[B)[B",
                &[JValue::Object(&j_alias), JValue::Object(&j_input)],
            )
            .and_then(|v| v.l())?
            .into();
        let bytes = env.convert_byte_array(&out)?;
        // The input/output Java arrays may hold the secret; overwrite them
        // before they are left to the GC (best effort — ART may have copied).
        let zeros_in = vec![0i8; input.len()];
        let _ = env.set_byte_array_region(&j_input, 0, &zeros_in);
        let zeros_out = vec![0i8; bytes.len()];
        let _ = env.set_byte_array_region(&out, 0, &zeros_out);
        Ok(bytes)
    })
}
