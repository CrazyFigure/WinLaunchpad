//! 构建脚本：调用 Windows SDK 的 rc.exe 把图标与版本信息编入可执行文件。
//! 找不到 rc.exe 时跳过资源（不影响功能，只是 exe 没有自定义图标）。

use std::{env, fs, path::PathBuf, process::Command};

fn find_rc() -> Option<PathBuf> {
    let roots = [
        r"C:\Program Files (x86)\Windows Kits\10\bin",
        r"C:\Program Files\Windows Kits\10\bin",
    ];
    let mut best: Option<(u32, u32, u32, PathBuf)> = None;
    for root in roots {
        let Ok(entries) = fs::read_dir(root) else { continue };
        for e in entries.flatten() {
            let ver = e.file_name().to_string_lossy().to_string();
            let mut parts = [0u32; 3];
            for (i, p) in ver.split('.').take(3).enumerate() {
                parts[i] = p.parse().unwrap_or(0);
            }
            let rc = e.path().join("x64").join("rc.exe");
            if rc.exists() && best.as_ref().map(|b| parts[0] > b.0 || (parts[0] == b.0 && (parts[1] > b.1 || (parts[1] == b.1 && parts[2] > b.2)))).unwrap_or(true) {
                best = Some((parts[0], parts[1], parts[2], rc));
            }
        }
    }
    best.map(|b| b.3)
}

fn main() {
    println!("cargo:rerun-if-changed=assets/app.ico");
    println!("cargo:rerun-if-changed=assets/app.rc");
    println!("cargo:rerun-if-changed=assets/app.manifest");
    let Some(rc) = find_rc() else {
        println!("cargo:warning=rc.exe not found, embedding no resources");
        return;
    };
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("app.res");
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let status = Command::new(&rc)
        .arg("/nologo")
        .arg(format!("/fo{}", out.display()))
        .arg("/i")
        .arg(manifest.join("assets").as_os_str())
        .arg(manifest.join("assets").join("app.rc").as_os_str())
        .status();
    match status {
        Ok(s) if s.success() => {
            println!("cargo:rustc-link-arg-bin=winlaunchpad={}", out.display());
        }
        _ => println!("cargo:warning=rc.exe failed, embedding no resources"),
    }
}
