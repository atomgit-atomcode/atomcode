//! The JNI surface: three functions Kotlin calls, and nothing else.
//!
//! Kept deliberately dumb — every interesting decision lives in [`crate::host`].
//! These functions translate strings, hold the one process-wide server, and turn
//! a failure into a message Kotlin can log instead of a panic across FFI.

use std::sync::Mutex;

use jni::objects::{GlobalRef, JClass, JObject, JString, JValue};
use jni::sys::jstring;
use jni::{JNIEnv, JavaVM};

use crate::host::{AgentServer, HostOptions};

/// One server per process. A `WebView` reload or an `Activity` recreation must
/// find the same runtime, not a second one — the session log and the live hub
/// both assume a single owner.
static SERVER: Mutex<Option<AgentServer>> = Mutex::new(None);

/// Why the last `nativeStart` failed, for the Kotlin side to log. Cleared on a
/// successful start.
static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);

/// The JVM and Kotlin's power hook, captured on the Java thread that starts the
/// server.
///
/// The class reference has to be held from the start: an attached native thread
/// has no application class loader, so resolving `com/atomcode/app/AgentPower`
/// on demand from a runtime worker would fail even though the class is there.
static POWER: std::sync::OnceLock<(JavaVM, GlobalRef)> = std::sync::OnceLock::new();

/// Tell Kotlin that a turn started or the last one ended, so it can hold the CPU
/// awake for exactly as long as work is running.
///
/// This is the fix for a measured failure: the wake lock used to be driven by a
/// 5 s poller, and once the lock was released the sleeping CPU never ran the
/// poller again — a turn that began just after that point stalled and its SSE
/// stream died. The runtime knows the truth, so it says so.
pub fn notify_turn_active(active: bool) {
    let Some((vm, class)) = POWER.get() else {
        return;
    };
    let Ok(mut env) = vm.attach_current_thread_as_daemon() else {
        return;
    };
    let _ = env.call_static_method(class, "turnActive", "(Z)V", &[JValue::Bool(active as u8)]);
}

fn lock<T>(slot: &'static Mutex<T>) -> std::sync::MutexGuard<'static, T> {
    // A panic in a previous JNI call must not turn every later call into a
    // poisoned-mutex abort: the data is a single Option/String and stays valid.
    slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn set_error(message: impl Into<String>) {
    *lock(&LAST_ERROR) = Some(message.into());
}

fn to_java(env: &mut JNIEnv<'_>, value: &str) -> jstring {
    match env.new_string(value) {
        Ok(s) => s.into_raw(),
        Err(error) => {
            set_error(format!("could not build a Java string: {error}"));
            std::ptr::null_mut()
        }
    }
}

/// Read a Java string argument. Returns `None` on failure (after recording why).
fn read(env: &mut JNIEnv<'_>, value: &JString<'_>, what: &str) -> Option<String> {
    match env.get_string(value) {
        Ok(s) => Some(s.into()),
        Err(error) => {
            set_error(format!("could not read {what} from Java: {error}"));
            None
        }
    }
}

/// Read a `String?`, where `null` is a meaningful answer rather than an error.
///
/// The helper APK is optional, so Kotlin sends `null` when it is not installed.
/// `JString` is not nullable in the binding, hence the raw cast; the local
/// reference still belongs to the caller, so it must not be dropped here.
fn read_optional(env: &mut JNIEnv<'_>, value: &JObject<'_>, what: &str) -> Option<String> {
    if value.is_null() {
        return None;
    }
    let string = unsafe { JString::from_raw(value.as_raw()) };
    let out = read(env, &string, what);
    std::mem::forget(string);
    out
}

/// Initialise logcat. `level` follows `android.util.Log` levels (4 = info).
/// Safe to call more than once; only the first call installs the logger.
#[no_mangle]
pub extern "system" fn Java_com_atomcode_app_NativeAgent_nativeInit(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    level: jni::sys::jint,
) {
    let level = match level {
        2 => log::LevelFilter::Warn,
        3 => log::LevelFilter::Info,
        4 => log::LevelFilter::Debug,
        5 => log::LevelFilter::Trace,
        _ => log::LevelFilter::Error,
    };
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(level)
            .with_tag("atomcode"),
    );
}

/// Start (or return) the in-process agent server.
///
/// `native_lib_dir` is the APK's `nativeLibraryDir`; the bundled git was
/// installed there (see `scripts/build-android-git.sh`) and the host puts its
/// shims on `PATH`.
///
/// Returns the webui URL — `http://127.0.0.1:<port>/?token=<token>` — or `null`
/// on failure, in which case `nativeLastError` says why.
#[no_mangle]
pub extern "system" fn Java_com_atomcode_app_NativeAgent_nativeStart(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    data_dir: JString<'_>,
    workspace: JString<'_>,
    native_lib_dir: JString<'_>,
    helper_lib_dir: JObject<'_>,
) -> jstring {
    let Some(data_dir) = read(&mut env, &data_dir, "dataDir") else {
        return std::ptr::null_mut();
    };
    let Some(workspace) = read(&mut env, &workspace, "workspace") else {
        return std::ptr::null_mut();
    };
    let Some(native_lib_dir) = read(&mut env, &native_lib_dir, "nativeLibraryDir") else {
        return std::ptr::null_mut();
    };
    // Optional: `null` means no provider APK is installed, which is normal.
    //
    // The value is the providers' `nativeLibraryDir`s joined by `\n` — Kotlin keeps
    // the list of provider packages, and one string keeps this signature (and the
    // JNI boundary) from growing a parameter per provider.
    let helper_lib_dirs: Vec<std::path::PathBuf> =
        read_optional(&mut env, &helper_lib_dir, "helperLibraryDirs")
            .map(|joined| {
                joined
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .map(std::path::PathBuf::from)
                    .collect()
            })
            .unwrap_or_default();

    let mut slot = lock(&SERVER);
    if let Some(server) = slot.as_ref() {
        if server.is_alive() {
            // A reload/rotation: hand back the same URL rather than starting a
            // second runtime on a second port.
            return to_java(&mut env, &server.url());
        }
        // The cached server outlived its own task — `/shutdown`, or `run_server`
        // returned an error. Handing its URL back would leave the app pointing at
        // a port nobody listens on, and no later start would ever fix it because
        // the cache says "already running". Drop it (its Drop sends the shutdown
        // signal) and start a fresh one below.
        tracing::warn!(
            port = server.port(),
            "cached agent server is not listening; starting a new one"
        );
    }
    *slot = None;

    // Cache the JVM + the Kotlin power hook while we are still on a Java thread.
    if POWER.get().is_none() {
        let vm = env.get_java_vm();
        let class = env.find_class("com/atomcode/app/AgentPower");
        if let (Ok(vm), Ok(class)) = (vm, class) {
            if let Ok(class) = env.new_global_ref(&class) {
                let _ = POWER.set((vm, class));
            }
        }
    }

    let mut options = HostOptions::new(data_dir, workspace).with_native_lib_dir(native_lib_dir);
    if !helper_lib_dirs.is_empty() {
        options = options.with_helper_lib_dirs(helper_lib_dirs);
    }

    match AgentServer::start(options) {
        Ok(server) => {
            let url = server.url();
            *slot = Some(server);
            *lock(&LAST_ERROR) = None;
            to_java(&mut env, &url)
        }
        Err(error) => {
            set_error(format!("agent server failed to start: {error:#}"));
            std::ptr::null_mut()
        }
    }
}

/// Stop the server and release the runtime. Idempotent.
#[no_mangle]
pub extern "system" fn Java_com_atomcode_app_NativeAgent_nativeStop(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
) {
    *lock(&SERVER) = None;
}

/// Whether this build can sign requests for the AtomGit gateway.
///
/// `false` means the hosted gateway (`llm-api.atomgit.com`) will reject every
/// request with `ATOMCODE_SIG_MISSING`, so a turn against it cannot succeed.
/// The app shows this instead of letting the person discover it as a provider
/// error mid-conversation.
#[no_mangle]
pub extern "system" fn Java_com_atomcode_app_NativeAgent_nativeGatewaySigningAvailable(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
) -> jni::sys::jboolean {
    crate::host::gateway_signer_available() as jni::sys::jboolean
}

/// The last start failure, or `null`.
#[no_mangle]
pub extern "system" fn Java_com_atomcode_app_NativeAgent_nativeLastError(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
) -> jstring {
    match lock(&LAST_ERROR).clone() {
        Some(message) => to_java(&mut env, &message),
        None => std::ptr::null_mut(),
    }
}
