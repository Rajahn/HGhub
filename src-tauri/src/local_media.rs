//! 把已经生成的视频放进用户能找到的位置。
//!
//! Android 10 及以上写入 MediaStore 的 Downloads/HGhub，不申请存储权限。
//! 更旧的系统退回公共下载目录；失败时退回应用专属目录，并在提示里给出路径。

use std::path::{Path, PathBuf};

pub struct Published {
    pub location: String,
    pub token: String,
}

pub fn publish(src: &Path, name: &str, mime: &str) -> Result<Published, String> {
    #[cfg(target_os = "android")]
    {
        android::publish(src, name, mime)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = mime;
        let dir = desktop_dir();
        std::fs::create_dir_all(&dir).map_err(|_| "无法创建下载目录")?;
        let dest = unique_path(&dir, name);
        std::fs::copy(src, &dest).map_err(|_| "无法写入下载目录")?;
        let text = dest.display().to_string();
        Ok(Published { location: text.clone(), token: text })
    }
}

/// 只在用户删除离线视频时调用。删不掉系统副本时，应用内文件仍会删掉。
pub fn remove(token: &str) -> Result<(), String> {
    if token.is_empty() {
        return Ok(());
    }
    #[cfg(target_os = "android")]
    {
        android::remove(token)
    }
    #[cfg(not(target_os = "android"))]
    {
        if !token.starts_with("content:") {
            let _ = std::fs::remove_file(token);
        }
        Ok(())
    }
}

pub fn open(token: &str, mime: &str) -> Result<(), String> {
    if token.is_empty() {
        return Err("没有可打开的文件".into());
    }
    #[cfg(target_os = "android")]
    {
        android::open(token, mime)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = mime;
        let status = std::process::Command::new("xdg-open").arg(token).status();
        match status {
            Ok(code) if code.success() => Ok(()),
            _ => Err(format!("请到文件管理器打开 {token}")),
        }
    }
}

fn desktop_dir() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join("Downloads").join("HGhub");
    }
    std::env::temp_dir().join("HGhub")
}

fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let path = Path::new(name);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("video");
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("mp4");
    for n in 2..100 {
        let next = dir.join(format!("{stem}-{n}.{ext}"));
        if !next.exists() {
            return next;
        }
    }
    dir.join(format!("{stem}-{}.{}", std::process::id(), ext))
}

#[cfg(target_os = "android")]
mod android {
    use super::Published;
    use jni::objects::{JObject, JString, JValue};
    use jni::JNIEnv;
    use std::path::Path;

    pub fn publish(src: &Path, name: &str, mime: &str) -> Result<Published, String> {
        let path = src.to_str().ok_or("临时文件路径无效")?.to_string();
        with_env(|env, context| {
            let sdk = env
                .get_static_field("android/os/Build$VERSION", "SDK_INT", "I")
                .map_err(|e| e.to_string())?
                .i()
                .map_err(|e| e.to_string())?;
            if sdk >= 29 {
                media_store(env, context, &path, name, mime)
            } else {
                legacy_download(env, &path, name).or_else(|_| app_file(env, context, &path, name))
            }
        })
    }

    pub fn remove(token: &str) -> Result<(), String> {
        if token.starts_with("content:") {
            return with_env(|env, context| {
                let resolver = env
                    .call_method(context, "getContentResolver", "()Landroid/content/ContentResolver;", &[])
                    .map_err(|e| e.to_string())?
                    .l()
                    .map_err(|e| e.to_string())?;
                let raw = env.new_string(token).map_err(|e| e.to_string())?;
                let uri = env
                    .call_static_method("android/net/Uri", "parse", "(Ljava/lang/String;)Landroid/net/Uri;", &[JValue::Object(&raw)])
                    .map_err(|e| e.to_string())?
                    .l()
                    .map_err(|e| e.to_string())?;
                env.call_method(&resolver, "delete", "(Landroid/net/Uri;Ljava/lang/String;[Ljava/lang/String;)I", &[JValue::Object(&uri), JValue::Object(&JObject::null()), JValue::Object(&JObject::null())])
                    .map_err(|_| "无法删除系统下载目录中的副本".to_string())?;
                Ok(())
            });
        }
        let _ = std::fs::remove_file(token);
        Ok(())
    }

    pub fn open(token: &str, mime: &str) -> Result<(), String> {
        with_env(|env, context| {
            let action = env.new_string("android.intent.action.VIEW").map_err(|e| e.to_string())?;
            let intent = env
                .new_object("android/content/Intent", "(Ljava/lang/String;)V", &[JValue::Object(&action)])
                .map_err(|e| e.to_string())?;
            let mime = env.new_string(mime).map_err(|e| e.to_string())?;
            if token.starts_with("content:") {
                let raw = env.new_string(token).map_err(|e| e.to_string())?;
                let uri = env
                    .call_static_method("android/net/Uri", "parse", "(Ljava/lang/String;)Landroid/net/Uri;", &[JValue::Object(&raw)])
                    .map_err(|e| e.to_string())?
                    .l()
                    .map_err(|e| e.to_string())?;
                env.call_method(&intent, "setDataAndType", "(Landroid/net/Uri;Ljava/lang/String;)Landroid/content/Intent;", &[JValue::Object(&uri), JValue::Object(&mime)])
                    .map_err(|e| e.to_string())?;
                env.call_method(&intent, "addFlags", "(I)Landroid/content/Intent;", &[JValue::Int(0x1)])
                    .map_err(|e| e.to_string())?;
            } else {
                let raw = env.new_string(format!("file://{token}")).map_err(|e| e.to_string())?;
                let uri = env
                    .call_static_method("android/net/Uri", "parse", "(Ljava/lang/String;)Landroid/net/Uri;", &[JValue::Object(&raw)])
                    .map_err(|e| e.to_string())?
                    .l()
                    .map_err(|e| e.to_string())?;
                env.call_method(&intent, "setDataAndType", "(Landroid/net/Uri;Ljava/lang/String;)Landroid/content/Intent;", &[JValue::Object(&uri), JValue::Object(&mime)])
                    .map_err(|e| e.to_string())?;
            }
            // NEW_TASK 让应用 Context 在后台线程也能拉起播放器。
            env.call_method(&intent, "addFlags", "(I)Landroid/content/Intent;", &[JValue::Int(0x10000001)])
                .map_err(|e| e.to_string())?;
            let app = env
                .call_method(context, "getApplicationContext", "()Landroid/content/Context;", &[])
                .map_err(|e| e.to_string())?
                .l()
                .map_err(|e| e.to_string())?;
            env.call_method(&app, "startActivity", "(Landroid/content/Intent;)V", &[JValue::Object(&intent)])
                .map_err(|_| "没有可打开该视频的播放器".to_string())?;
            Ok(())
        })
    }

    fn media_store(env: &mut JNIEnv, context: &JObject, src: &str, name: &str, mime: &str) -> Result<Published, String> {
        let resolver = env
            .call_method(context, "getContentResolver", "()Landroid/content/ContentResolver;", &[])
            .map_err(|e| e.to_string())?
            .l()
            .map_err(|e| e.to_string())?;
        let volume = env.new_string("external_primary").map_err(|e| e.to_string())?;
        let collection = env
            .call_static_method("android/provider/MediaStore$Downloads", "getContentUri", "(Ljava/lang/String;)Landroid/net/Uri;", &[JValue::Object(&volume)])
            .map_err(|e| e.to_string())?
            .l()
            .map_err(|e| e.to_string())?;
        let values = content_values(env, &[
            ("_display_name", name),
            ("mime_type", mime),
            ("relative_path", "Download/HGhub"),
        ])?;
        put_int(env, &values, "is_pending", 1)?;
        let item = env
            .call_method(&resolver, "insert", "(Landroid/net/Uri;Landroid/content/ContentValues;)Landroid/net/Uri;", &[JValue::Object(&collection), JValue::Object(&values)])
            .map_err(|e| e.to_string())?
            .l()
            .map_err(|e| e.to_string())?;
        if item.is_null() {
            return Err("系统没有接受下载文件".into());
        }
        if let Err(err) = copy_into(env, &resolver, &item, src) {
            let _ = env.call_method(&resolver, "delete", "(Landroid/net/Uri;Ljava/lang/String;[Ljava/lang/String;)I", &[JValue::Object(&item), JValue::Object(&JObject::null()), JValue::Object(&JObject::null())]);
            return Err(err);
        }
        let done = content_values(env, &[])?;
        put_int(env, &done, "is_pending", 0)?;
        env.call_method(&resolver, "update", "(Landroid/net/Uri;Landroid/content/ContentValues;Ljava/lang/String;[Ljava/lang/String;)I", &[JValue::Object(&item), JValue::Object(&done), JValue::Object(&JObject::null()), JValue::Object(&JObject::null())])
            .map_err(|e| e.to_string())?;
        let token = jstring(env, &item)?;
        Ok(Published { location: format!("下载/HGhub/{name}"), token })
    }

    fn legacy_download(env: &mut JNIEnv, src: &str, name: &str) -> Result<Published, String> {
        let kind = env.new_string("Download").map_err(|e| e.to_string())?;
        let root = env
            .call_static_method("android/os/Environment", "getExternalStoragePublicDirectory", "(Ljava/lang/String;)Ljava/io/File;", &[JValue::Object(&kind)])
            .map_err(|e| e.to_string())?
            .l()
            .map_err(|e| e.to_string())?;
        let sub = env.new_string("HGhub").map_err(|e| e.to_string())?;
        let dir = env.new_object("java/io/File", "(Ljava/io/File;Ljava/lang/String;)V", &[JValue::Object(&root), JValue::Object(&sub)]).map_err(|e| e.to_string())?;
        env.call_method(&dir, "mkdirs", "()Z", &[]).map_err(|e| e.to_string())?;
        let leaf = env.new_string(name).map_err(|e| e.to_string())?;
        let dest = env.new_object("java/io/File", "(Ljava/io/File;Ljava/lang/String;)V", &[JValue::Object(&dir), JValue::Object(&leaf)]).map_err(|e| e.to_string())?;
        let dest_path = jstring_method(env, &dest, "getAbsolutePath")?;
        copy_paths(env, src, &dest_path)?;
        Ok(Published { location: format!("下载/HGhub/{name}"), token: dest_path })
    }

    fn app_file(env: &mut JNIEnv, context: &JObject, src: &str, name: &str) -> Result<Published, String> {
        let dir = env
            .call_method(context, "getExternalFilesDir", "(Ljava/lang/String;)Ljava/io/File;", &[JValue::Object(&JObject::null())])
            .map_err(|e| e.to_string())?
            .l()
            .map_err(|e| e.to_string())?;
        let sub = env.new_string("HGhub").map_err(|e| e.to_string())?;
        let folder = env.new_object("java/io/File", "(Ljava/io/File;Ljava/lang/String;)V", &[JValue::Object(&dir), JValue::Object(&sub)]).map_err(|e| e.to_string())?;
        env.call_method(&folder, "mkdirs", "()Z", &[]).map_err(|e| e.to_string())?;
        let leaf = env.new_string(name).map_err(|e| e.to_string())?;
        let dest = env.new_object("java/io/File", "(Ljava/io/File;Ljava/lang/String;)V", &[JValue::Object(&folder), JValue::Object(&leaf)]).map_err(|e| e.to_string())?;
        let dest_path = jstring_method(env, &dest, "getAbsolutePath")?;
        copy_paths(env, src, &dest_path)?;
        Ok(Published { location: dest_path.clone(), token: dest_path })
    }

    fn copy_into(env: &mut JNIEnv, resolver: &JObject, item: &JObject, src: &str) -> Result<(), String> {
        let output = env
            .call_method(resolver, "openOutputStream", "(Landroid/net/Uri;)Ljava/io/OutputStream;", &[JValue::Object(item)])
            .map_err(|e| e.to_string())?
            .l()
            .map_err(|e| e.to_string())?;
        if output.is_null() {
            return Err("系统没有打开下载文件".into());
        }
        let input_path = env.new_string(src).map_err(|e| e.to_string())?;
        let input = env.new_object("java/io/FileInputStream", "(Ljava/lang/String;)V", &[JValue::Object(&input_path)]).map_err(|e| e.to_string())?;
        let buffer = env.new_byte_array(64 * 1024).map_err(|e| e.to_string())?;
        loop {
            let n = env.call_method(&input, "read", "([B)I", &[JValue::Object(&buffer)]).map_err(|e| e.to_string())?.i().map_err(|e| e.to_string())?;
            if n < 0 {
                break;
            }
            env.call_method(&output, "write", "([BII)V", &[JValue::Object(&buffer), JValue::Int(0), JValue::Int(n)]).map_err(|e| e.to_string())?;
        }
        env.call_method(&output, "flush", "()V", &[]).map_err(|e| e.to_string())?;
        env.call_method(&output, "close", "()V", &[]).map_err(|e| e.to_string())?;
        env.call_method(&input, "close", "()V", &[]).map_err(|e| e.to_string())?;
        Ok(())
    }

    fn copy_paths(env: &mut JNIEnv, src: &str, dest: &str) -> Result<(), String> {
        let src = env.new_string(src).map_err(|e| e.to_string())?;
        let dest = env.new_string(dest).map_err(|e| e.to_string())?;
        let input = env.new_object("java/io/FileInputStream", "(Ljava/lang/String;)V", &[JValue::Object(&src)]).map_err(|e| e.to_string())?;
        let output = env.new_object("java/io/FileOutputStream", "(Ljava/lang/String;)V", &[JValue::Object(&dest)]).map_err(|e| e.to_string())?;
        let buffer = env.new_byte_array(64 * 1024).map_err(|e| e.to_string())?;
        loop {
            let n = env.call_method(&input, "read", "([B)I", &[JValue::Object(&buffer)]).map_err(|e| e.to_string())?.i().map_err(|e| e.to_string())?;
            if n < 0 {
                break;
            }
            env.call_method(&output, "write", "([BII)V", &[JValue::Object(&buffer), JValue::Int(0), JValue::Int(n)]).map_err(|e| e.to_string())?;
        }
        env.call_method(&output, "close", "()V", &[]).map_err(|e| e.to_string())?;
        env.call_method(&input, "close", "()V", &[]).map_err(|e| e.to_string())?;
        Ok(())
    }

    fn content_values<'local>(env: &mut JNIEnv<'local>, pairs: &[(&str, &str)]) -> Result<JObject<'local>, String> {
        let values = env.new_object("android/content/ContentValues", "()V", &[]).map_err(|e| e.to_string())?;
        for (key, value) in pairs {
            let key = env.new_string(*key).map_err(|e| e.to_string())?;
            let value = env.new_string(*value).map_err(|e| e.to_string())?;
            env.call_method(&values, "put", "(Ljava/lang/String;Ljava/lang/String;)V", &[JValue::Object(&key), JValue::Object(&value)]).map_err(|e| e.to_string())?;
        }
        Ok(values)
    }

    fn put_int(env: &mut JNIEnv, values: &JObject, key: &str, value: i32) -> Result<(), String> {
        let key = env.new_string(key).map_err(|e| e.to_string())?;
        let integer = env
            .call_static_method("java/lang/Integer", "valueOf", "(I)Ljava/lang/Integer;", &[JValue::Int(value)])
            .map_err(|e| e.to_string())?
            .l()
            .map_err(|e| e.to_string())?;
        env.call_method(values, "put", "(Ljava/lang/String;Ljava/lang/Integer;)V", &[JValue::Object(&key), JValue::Object(&integer)]).map_err(|e| e.to_string())?;
        Ok(())
    }

    fn jstring(env: &mut JNIEnv, obj: &JObject) -> Result<String, String> {
        jstring_method(env, obj, "toString")
    }

    fn jstring_method(env: &mut JNIEnv, obj: &JObject, name: &str) -> Result<String, String> {
        let value = env.call_method(obj, name, "()Ljava/lang/String;", &[]).map_err(|e| e.to_string())?.l().map_err(|e| e.to_string())?;
        let value = JString::from(value);
        let text = env.get_string(&value).map_err(|e| e.to_string())?;
        Ok(text.to_string_lossy().into_owned())
    }

    fn with_env<T>(f: impl FnOnce(&mut JNIEnv, &JObject) -> Result<T, String>) -> Result<T, String> {
        let ctx = ndk_context::android_context();
        let vm = unsafe { jni::JavaVM::from_raw(ctx.vm() as *mut jni::sys::JavaVM) }.map_err(|e| e.to_string())?;
        let mut env = vm.attach_current_thread().map_err(|e| e.to_string())?;
        let context = std::mem::ManuallyDrop::new(unsafe { JObject::from_raw(ctx.context() as jni::sys::jobject) });
        f(&mut env, &context)
    }
}
