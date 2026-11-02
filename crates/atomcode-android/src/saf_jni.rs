//! The Android half of the SAF world: [`SafBackend`] over the `SafTree` object.
//!
//! `atomcode_capabilities::saf` owns everything that is not platform-specific —
//! containment, path composition, error mapping — and is tested on the build
//! machine. What is left is exactly what needs Android: talking to documents.
//!
//! Calls are synchronous JNI calls made from async code. That is deliberate: SAF
//! reads and directory listings are local and short, and a callback into the
//! runtime from Kotlin would need a channel per operation and make a wrong answer
//! indistinguishable from a slow one. If a provider ever turns out to block for
//! real, the fix belongs here — spawn the call onto a blocking lane — not in the
//! world above.
//!
//! Results arrive as JSON strings because "the document is gone" is an ordinary
//! answer, not an exception: `{"error":"not_found"}` maps to
//! [`SafError::NotFound`], `denied` to a refusal, and anything else to `Io`.
//!
//! Every call is written as separate statements rather than nested expressions:
//! `JNIEnv` is borrowed mutably by each call, so `f(env, g(env))` does not compile
//! and the result of one call has to be bound before the next borrow.

use std::sync::OnceLock;

use async_trait::async_trait;
use atomcode_capabilities::saf::{SafBackend, SafEntry, SafError, SafStat};
use jni::objects::{GlobalRef, JObject, JValue};
use jni::{JNIEnv, JavaVM};
use serde_json::Value;

/// The `SafTree` class, captured on the Java thread that installs us.
///
/// Held from the start for the reason `jni_api::POWER` is: an attached native
/// thread has no application class loader, so resolving the class on demand from
/// a runtime worker would fail even though it is plainly there.
static TREE: OnceLock<(JavaVM, GlobalRef)> = OnceLock::new();

struct JniBackend;

/// Attach this thread and run `body`, since an async worker is not a Java thread.
fn with_env<T>(body: impl FnOnce(&mut JNIEnv<'_>) -> Result<T, SafError>) -> Result<T, SafError> {
    let Some((vm, _)) = TREE.get() else {
        return Err(SafError::Denied("no folder has been granted".into()));
    };
    let mut env = vm
        .attach_current_thread_as_daemon()
        .map_err(|failure| SafError::Io(format!("jni attach: {failure}")))?;
    body(&mut env)
}

/// Call one `String -> String` method on `SafTree` and give back its text.
fn call_text(env: &mut JNIEnv<'_>, method: &str, rel: &str) -> Result<String, SafError> {
    let Some((_, class)) = TREE.get() else {
        return Err(SafError::Denied("no folder has been granted".into()));
    };
    let argument = env
        .new_string(rel)
        .map_err(|failure| SafError::Io(format!("{method}: {failure}")))?;
    let argument = JObject::from(argument);
    let value = env
        .call_static_method(
            class,
            method,
            "(Ljava/lang/String;)Ljava/lang/String;",
            &[JValue::Object(&argument)],
        )
        .map_err(|failure| SafError::Io(format!("{method}: {failure}")))?;
    let object = value
        .l()
        .map_err(|failure| SafError::Io(format!("{method}: {failure}")))?;
    if object.is_null() {
        return Err(SafError::NotFound);
    }
    let object = JObject::from(object);
    let text = env
        .get_string((&object).into())
        .map_err(|failure| SafError::Io(format!("{method}: {failure}")))?;
    Ok(text.into())
}

/// Turn `{"error":…}` into this seam's vocabulary, and the literal `"null"` into
/// "the document is not there".
fn classify(payload: &str) -> Result<Option<String>, SafError> {
    if payload == "null" {
        return Ok(None);
    }
    if payload.trim_start().starts_with('{') {
        if let Ok(Value::Object(object)) = serde_json::from_str::<Value>(payload) {
            if let Some(Value::String(kind)) = object.get("error") {
                let message = object
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or_default();
                return Err(match kind.as_str() {
                    "not_found" => SafError::NotFound,
                    "denied" => SafError::Denied("the platform refused access".into()),
                    other => SafError::Io(if message.is_empty() {
                        other.to_string()
                    } else {
                        format!("{other}: {message}")
                    }),
                });
            }
        }
    }
    Ok(Some(payload.to_string()))
}

/// `"ok"` is success; anything else is a failure with something to say.
fn expect_ok(payload: &str, what: &str) -> Result<(), SafError> {
    match classify(payload)? {
        Some(text) if text == "ok" => Ok(()),
        Some(other) => Err(SafError::Io(format!("{what}: {other}"))),
        None => Err(SafError::Io(format!("{what}: empty result"))),
    }
}

#[async_trait]
impl SafBackend for JniBackend {
    async fn list(&self, path: &str) -> Result<Vec<SafEntry>, SafError> {
        with_env(|env| {
            let payload = call_text(env, "list", path)?;
            let payload = classify(&payload)?.ok_or(SafError::NotFound)?;
            let parsed: Value = serde_json::from_str(&payload)
                .map_err(|failure| SafError::Io(format!("bad list payload: {failure}")))?;
            let entries = parsed
                .get("entries")
                .and_then(|e| e.as_array())
                .ok_or_else(|| SafError::Io("list payload has no entries".into()))?;
            Ok(entries
                .iter()
                .filter_map(|entry| {
                    Some(SafEntry {
                        name: entry.get("name")?.as_str()?.to_string(),
                        is_dir: entry.get("isDir")?.as_bool()?,
                    })
                })
                .collect())
        })
    }

    async fn read(&self, path: &str) -> Result<Vec<u8>, SafError> {
        with_env(|env| {
            let Some((_, class)) = TREE.get() else {
                return Err(SafError::Denied("no folder has been granted".into()));
            };
            let argument = env
                .new_string(path)
                .map_err(|failure| SafError::Io(format!("read: {failure}")))?;
            let argument = JObject::from(argument);
            let value = env
                .call_static_method(
                    class,
                    "read",
                    "(Ljava/lang/String;)[B",
                    &[JValue::Object(&argument)],
                )
                .map_err(|failure| SafError::Io(format!("read: {failure}")))?;
            let array = value
                .l()
                .map_err(|failure| SafError::Io(format!("read: {failure}")))?;
            if array.is_null() {
                return Err(SafError::NotFound);
            }
            let array = JObject::from(array);
            let array = jni::objects::JByteArray::from(array);
            env.convert_byte_array(array)
                .map_err(|failure| SafError::Io(format!("read: {failure}")))
        })
    }

    async fn write(&self, path: &str, bytes: &[u8]) -> Result<(), SafError> {
        with_env(|env| {
            let Some((_, class)) = TREE.get() else {
                return Err(SafError::Denied("no folder has been granted".into()));
            };
            let argument = env
                .new_string(path)
                .map_err(|failure| SafError::Io(format!("write: {failure}")))?;
            let argument = JObject::from(argument);
            let data = env
                .byte_array_from_slice(bytes)
                .map_err(|failure| SafError::Io(format!("write: {failure}")))?;
            let data = JObject::from(data);
            let value = env
                .call_static_method(
                    class,
                    "write",
                    "(Ljava/lang/String;[B)Ljava/lang/String;",
                    &[JValue::Object(&argument), JValue::Object(&data)],
                )
                .map_err(|failure| SafError::Io(format!("write: {failure}")))?;
            let object = value
                .l()
                .map_err(|failure| SafError::Io(format!("write: {failure}")))?;
            if object.is_null() {
                return Err(SafError::Io("write: no result".into()));
            }
            let object = JObject::from(object);
            let text = env
                .get_string((&object).into())
                .map_err(|failure| SafError::Io(format!("write: {failure}")))?;
            let text: String = text.into();
            expect_ok(&text, "write")
        })
    }

    async fn create_dir(&self, path: &str) -> Result<(), SafError> {
        with_env(|env| {
            let payload = call_text(env, "createDir", path)?;
            expect_ok(&payload, "createDir")
        })
    }

    async fn stat(&self, path: &str) -> Result<Option<SafStat>, SafError> {
        with_env(|env| {
            let payload = call_text(env, "stat", path)?;
            let Some(payload) = classify(&payload)? else {
                return Ok(None);
            };
            let parsed: Value = serde_json::from_str(&payload)
                .map_err(|failure| SafError::Io(format!("bad stat payload: {failure}")))?;
            Ok(Some(SafStat {
                is_dir: parsed
                    .get("isDir")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                len: parsed.get("len").and_then(|v| v.as_u64()).unwrap_or(0),
            }))
        })
    }
}

/// Called from Kotlin once a folder has been granted, and on every start when a
/// previous grant is restored. Idempotent: the class ref is captured on the first
/// call and the backend is installed once.
pub fn install(env: &mut JNIEnv<'_>, _class: jni::objects::JClass<'_>) -> bool {
    if TREE.get().is_none() {
        let Ok(vm) = env.get_java_vm() else {
            tracing::warn!("SAF: could not get the JVM");
            return false;
        };
        // The JNI signature of a method declared in a Kotlin `object` receives the
        // SINGLETON, not its class — `NativeAgent` is such an object, so the ref
        // handed in here is an instance and cannot be used as a class for
        // `call_static_method`. The class this backend actually calls is `SafTree`,
        // and looking it up is safe right here because this runs on a Java thread
        // that has the application class loader.
        let Ok(tree_class) = env.find_class("com/atomcode/app/SafTree") else {
            tracing::warn!("SAF: could not resolve the SafTree class");
            return false;
        };
        let Ok(global) = env.new_global_ref(tree_class) else {
            tracing::warn!("SAF: could not hold the SafTree class");
            return false;
        };
        let _ = TREE.set((vm, global));
    }
    atomcode_capabilities::saf::install_backend(std::sync::Arc::new(JniBackend));
    tracing::info!(
        installed = atomcode_capabilities::saf::installed_backend().is_some(),
        "SAF: backend installed"
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_platform_refusal_is_told_apart_from_a_failure() {
        assert_eq!(classify("not json"), Ok(Some("not json".into())));
        assert_eq!(classify("null"), Ok(None));
        assert!(matches!(
            classify(r#"{"error":"not_found","message":"gone"}"#),
            Err(SafError::NotFound)
        ));
        assert!(matches!(
            classify(r#"{"error":"denied","message":"no"}"#),
            Err(SafError::Denied(_))
        ));
        assert!(matches!(
            classify(r#"{"error":"io","message":"disk"}"#),
            Err(SafError::Io(_))
        ));
    }

    #[test]
    fn only_ok_counts_as_success() {
        assert!(expect_ok("ok", "write").is_ok());
        assert!(expect_ok("something else", "write").is_err());
        assert!(expect_ok(r#"{"error":"io","message":"x"}"#, "write").is_err());
    }

    #[test]
    fn a_listing_is_read_out_of_its_json() {
        let payload = r#"{"entries":[{"name":"a.txt","isDir":false},{"name":"sub","isDir":true}]}"#;
        let parsed: Value = serde_json::from_str(payload).unwrap();
        let entries = parsed.get("entries").and_then(|e| e.as_array()).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].get("name").and_then(|v| v.as_str()), Some("sub"));
        assert_eq!(
            entries[1].get("isDir").and_then(|v| v.as_bool()),
            Some(true)
        );
    }
}
