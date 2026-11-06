//! 把岛仔图标与版本信息作为资源编进 exe。
//!
//! 官方发布用 Windows SDK 的 rc.exe 生成 .res 交给 MSVC 链接器；
//! 没有 Windows SDK 的本机构建（Rust GNU 工具链等）则回退到 MinGW 的
//! windres 生成 COFF 资源对象，保证图标和版本信息不会缺失。
//! 两者都不存在时只打印警告，不影响程序功能（托盘图标有内嵌 ICO 回退）。

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// 在 PATH 中查找可执行文件。
fn on_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

fn find_rc() -> Option<PathBuf> {
    // 明确指定时优先使用，便于非标准安装目录。
    if let Some(value) = env::var_os("RC") {
        let explicit = PathBuf::from(value);
        if explicit.is_file() {
            return Some(explicit);
        }
    }
    if let Some(found) = on_path("rc.exe") {
        return Some(found);
    }
    let roots = [
        r"C:\Program Files (x86)\Windows Kits\10\bin",
        r"C:\Program Files\Windows Kits\10\bin",
    ];
    let mut found: Vec<PathBuf> = Vec::new();
    for root in roots {
        let Ok(entries) = fs::read_dir(root) else { continue };
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let rc = dir.join("x64").join("rc.exe");
            if rc.exists() {
                found.push(rc);
            }
        }
    }
    found.sort();
    found.pop()
}

fn find_windres() -> Option<PathBuf> {
    if let Some(value) = env::var_os("WINDRES") {
        let explicit = PathBuf::from(value);
        if explicit.is_file() {
            return Some(explicit);
        }
    }
    if let Some(found) = on_path("windres.exe") {
        return Some(found);
    }
    // 便携版 MinGW 可能只把 gcc 放进了 PATH，windres 与它同目录。
    if let Some(gcc) = on_path("gcc.exe") {
        if let Some(dir) = gcc.parent() {
            let candidate = dir.join("windres.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    for root in [
        r"C:\msys64\mingw64\bin",
        r"C:\msys2\mingw64\bin",
        r"C:\mingw64\bin",
    ] {
        let candidate = Path::new(root).join("windres.exe");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn main() {
    println!("cargo:rerun-if-changed=app.rc");
    println!("cargo:rerun-if-changed=assets/daozai.ico");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=RC");
    println!("cargo:rerun-if-env-changed=WINDRES");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    if let Some(rc) = find_rc() {
        let out = out_dir.join("daozai.res");
        let status = Command::new(&rc)
            .arg("/nologo")
            .arg("/fo")
            .arg(&out)
            .arg("app.rc")
            .status();
        if status.map(|s| s.success()).unwrap_or(false) {
            println!("cargo:rustc-link-arg={}", out.display());
        } else {
            println!("cargo:warning=rc.exe 编译 app.rc 失败");
        }
        return;
    }

    if let Some(windres) = find_windres() {
        let out = out_dir.join("daozai-res.o");
        let status = Command::new(&windres)
            .arg("--input-format=rc")
            .arg("--output-format=coff")
            .arg("-o")
            .arg(&out)
            .arg("app.rc")
            .status();
        if status.map(|s| s.success()).unwrap_or(false) {
            println!("cargo:rustc-link-arg={}", out.display());
        } else {
            println!("cargo:warning=windres 编译 app.rc 失败");
        }
        return;
    }

    println!("cargo:warning=未找到 rc.exe 或 windres，跳过图标与版本资源");
}
