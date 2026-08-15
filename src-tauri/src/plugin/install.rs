//! Copying a plugin into an install directory.
//!
//! Lives in the library rather than beside the desktop command because the
//! *worker* is the machine that most needs it: until this existed, installing a
//! plugin on a worker meant `cp -r`, nothing afterwards compared that copy
//! against its source, and a rebuilt worker binary could sit next to a plugin
//! from a year earlier — the exact configuration that made a remote job hang at
//! 64 images while the desktop ran the same plugin fine.
//!
//! Installing is idempotent: the destination directory is replaced wholesale,
//! so "update" is just installing again over the top. There is no registry of
//! where a plugin came from, deliberately — the source is a path the operator
//! typed, and remembering it would invite a background updater that reaches
//! back out to a location that may no longer exist.
//!
//! Two things it does that a copy does not:
//!
//! - **Skips virtualenvs and caches** (`.venv`, `venv`, `__pycache__`). They are
//!   large, platform-specific, and full of absolute paths that break on the
//!   destination machine.
//! - **Rewrites a venv-relative interpreter to an absolute one.** The bundled ML
//!   taggers declare `{plugin_dir}/../.venv/bin/python`, a sibling of the plugin
//!   directory in the *install* root. Copying such a plugin on its own produces
//!   one that cannot spawn, so if the source has a venv beside it, its
//!   interpreter path is written into the installed manifest.

use std::path::Path;

use super::manifest::PluginManifest;
use super::{check_api_version, PluginInfo, RequestDelivery};

/// Install a plugin directory (or a bare `.py` script) into `dest_dir`.
///
/// A directory must carry a `manifest.json`; a script gets a generated one that
/// declares no `api_version`, because nothing here can promise an arbitrary
/// script streams its results.
pub fn install_from_path(source: &Path, dest_dir: &Path) -> Result<PluginInfo, String> {
    if !source.exists() {
        return Err(format!("Path does not exist: {}", source.display()));
    }
    std::fs::create_dir_all(dest_dir).map_err(|e| e.to_string())?;

    if source.is_dir() {
        install_directory(source, dest_dir)
    } else if source.extension().and_then(|e| e.to_str()) == Some("py") {
        install_script(source, dest_dir)
    } else {
        Err("Path must be a plugin directory (with manifest.json) or a .py file".to_string())
    }
}

fn install_directory(source: &Path, dest_dir: &Path) -> Result<PluginInfo, String> {
    let manifest_path = source.join("manifest.json");
    if !manifest_path.exists() {
        return Err("Plugin directory must contain a manifest.json".to_string());
    }
    let manifest = PluginManifest::load(&manifest_path).map_err(|e| e.to_string())?;
    // Refuse a plugin built for a newer host now rather than at the first job.
    // `UpFront` is the permissive check: a plugin the *worker* must refuse is
    // still worth installing, since the same directory may serve a desktop.
    check_api_version(&manifest, RequestDelivery::UpFront)?;

    // The manifest `name`, not the source directory name — `find_plugin` and
    // every job resolve by manifest name, and the two legitimately differ
    // (`example-auto-tagger/` declares `example-image-tagger`).
    let target = dest_dir.join(&manifest.name);
    copy_dir_filtered(source, &target, &|name: &str| {
        name == ".venv" || name == "venv" || name == "__pycache__"
    })
    .map_err(|e| e.to_string())?;
    rewrite_manifest_python(&target.join("manifest.json"), source)?;

    Ok(PluginInfo::from(&manifest))
}

fn install_script(source: &Path, dest_dir: &Path) -> Result<PluginInfo, String> {
    let stem = source
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("plugin");
    let plugin_name = stem.replace('_', "-");
    let target = dest_dir.join(&plugin_name);
    std::fs::create_dir_all(&target).map_err(|e| e.to_string())?;

    let file_name = source
        .file_name()
        .ok_or_else(|| "source has no file name".to_string())?;
    std::fs::copy(source, target.join(file_name)).map_err(|e| e.to_string())?;

    let source_dir = source.parent().unwrap_or(Path::new("."));
    let req_source = source_dir.join("requirements.txt");
    if req_source.exists() {
        let _ = std::fs::copy(&req_source, target.join("requirements.txt"));
    }

    let python_command = detect_venv_python(source_dir).unwrap_or_else(|| "python3".to_string());
    let script_name = file_name.to_string_lossy().to_string();
    let description = format!("Auto-installed plugin from {}", script_name);
    // No `api_version`: see the module doc. The generated manifest is honest
    // about what we know, and its author can raise it once the script streams.
    let manifest_json = serde_json::json!({
        "name": plugin_name,
        "display_name": plugin_name,
        "version": "1.0.0",
        "description": description,
        "execution": {
            "type": "cli",
            "command": python_command,
            "args": [format!("{{plugin_dir}}/{}", script_name)],
        },
        "capabilities": ["read_image"],
        "tag_prefix": plugin_name,
    });
    let manifest_str = serde_json::to_string_pretty(&manifest_json).map_err(|e| e.to_string())?;
    std::fs::write(target.join("manifest.json"), &manifest_str).map_err(|e| e.to_string())?;

    Ok(PluginInfo {
        name: plugin_name.clone(),
        display_name: plugin_name.clone(),
        version: "1.0.0".to_string(),
        api_version: 0,
        description,
        tag_prefix: plugin_name,
    })
}

/// Look for a Python venv in `dir` and return the absolute path to its
/// interpreter. NOTE: Don't canonicalize — Python venvs work via symlink
/// location, so resolving the link finds the system interpreter and loses the
/// environment.
pub fn detect_venv_python(dir: &Path) -> Option<String> {
    let abs_dir = if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(dir)
    };
    for venv_name in &[".venv", "venv"] {
        let python = abs_dir.join(venv_name).join("bin").join("python");
        if python.exists() {
            return Some(python.display().to_string());
        }
    }
    None
}

fn copy_dir_filtered(src: &Path, dst: &Path, skip: &dyn Fn(&str) -> bool) -> std::io::Result<()> {
    if dst.exists() {
        std::fs::remove_dir_all(dst)?;
    }
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if skip(&name_str) {
            continue;
        }
        let file_type = entry.file_type()?;
        let dest_path = dst.join(&name);
        if file_type.is_dir() {
            copy_dir_filtered(&entry.path(), &dest_path, skip)?;
        } else if file_type.is_symlink() {
            let link_target = std::fs::read_link(entry.path())?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(&link_target, &dest_path)?;
        } else {
            std::fs::copy(entry.path(), &dest_path)?;
        }
    }
    Ok(())
}

fn rewrite_manifest_python(manifest_path: &Path, source_dir: &Path) -> Result<(), String> {
    let text = std::fs::read_to_string(manifest_path).map_err(|e| e.to_string())?;
    let mut doc: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;

    let needs_rewrite = doc
        .pointer("/execution/command")
        .and_then(|v| v.as_str())
        .map(|cmd| cmd.contains(".venv") || cmd.contains("venv"))
        .unwrap_or(false);

    if needs_rewrite
        && let Some(abs_python) = detect_venv_python(source_dir)
    {
        if let Some(exec) = doc.get_mut("execution").and_then(|e| e.as_object_mut()) {
            exec.insert("command".to_string(), serde_json::Value::String(abs_python));
        }
        let out = serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?;
        std::fs::write(manifest_path, out).map_err(|e| e.to_string())?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("lv-install-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_plugin(dir: &Path, name: &str, api_version: Option<u32>) {
        std::fs::create_dir_all(dir).unwrap();
        let api = api_version
            .map(|v| format!("\"api_version\": {v},"))
            .unwrap_or_default();
        std::fs::write(
            dir.join("manifest.json"),
            format!(
                r#"{{"name": "{name}", "display_name": "{name}", "version": "2.1.0", {api}
                    "description": "", "author": null, "license": null,
                    "execution": {{ "type": "cli", "command": "python3", "args": ["{{plugin_dir}}/t.py"] }},
                    "capabilities": ["read_image"], "tag_prefix": "{name}", "ui": null}}"#
            ),
        )
        .unwrap();
        std::fs::write(dir.join("t.py"), "print()").unwrap();
    }

    /// Installing the same source twice must land on the current copy, not on
    /// a merge of both — "update" is the same verb as "install".
    #[test]
    fn reinstalling_replaces_the_previous_copy() {
        let root = temp_dir("replace");
        let source = root.join("src");
        let dest = root.join("plugins");
        write_plugin(&source, "t", Some(1));
        std::fs::write(source.join("stale.py"), "old").unwrap();
        install_from_path(&source, &dest).unwrap();
        assert!(dest.join("t/stale.py").exists());

        std::fs::remove_file(source.join("stale.py")).unwrap();
        let info = install_from_path(&source, &dest).unwrap();
        assert_eq!(info.version, "2.1.0");
        assert_eq!(info.api_version, 1);
        assert!(!dest.join("t/stale.py").exists(), "stale file survived");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The install directory is keyed on the manifest name because that is
    /// what every job and `find_plugin` look up.
    #[test]
    fn the_target_directory_is_the_manifest_name() {
        let root = temp_dir("naming");
        let source = root.join("example-auto-tagger");
        let dest = root.join("plugins");
        write_plugin(&source, "example-image-tagger", Some(1));
        install_from_path(&source, &dest).unwrap();
        assert!(dest.join("example-image-tagger/manifest.json").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_plugin_from_a_newer_host_is_refused_at_install() {
        let root = temp_dir("future");
        let source = root.join("src");
        let dest = root.join("plugins");
        write_plugin(&source, "t", Some(super::super::PLUGIN_API_VERSION + 1));
        let err = install_from_path(&source, &dest).unwrap_err();
        assert!(err.contains("update LightView"), "{err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A copied venv is broken on the destination machine and enormous, so it
    /// is skipped — and the interpreter path is absolutised in its place.
    #[test]
    fn virtualenvs_are_skipped_and_the_interpreter_absolutised() {
        let root = temp_dir("venv");
        let source = root.join("src");
        let dest = root.join("plugins");
        write_plugin(&source, "t", Some(1));
        std::fs::create_dir_all(source.join(".venv/bin")).unwrap();
        std::fs::write(source.join(".venv/bin/python"), "#!/bin/sh").unwrap();
        std::fs::write(source.join("__pycache__.marker"), "").unwrap();
        // Point the manifest at the sibling venv, as the ML taggers do.
        let m = source.join("manifest.json");
        let text = std::fs::read_to_string(&m)
            .unwrap()
            .replace("\"python3\"", "\"{plugin_dir}/../.venv/bin/python\"");
        std::fs::write(&m, text).unwrap();

        install_from_path(&source, &dest).unwrap();
        assert!(!dest.join("t/.venv").exists(), "venv was copied");
        let installed = std::fs::read_to_string(dest.join("t/manifest.json")).unwrap();
        assert!(
            installed.contains(".venv/bin/python") && !installed.contains("{plugin_dir}/../"),
            "interpreter not absolutised: {installed}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
