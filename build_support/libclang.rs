use std::env;
use std::path::PathBuf;
use std::process::Command;

/// Configure bindgen to use the Homebrew LLVM installation on macOS.
///
/// Homebrew's Apple-Silicon prefix is not one of clang-sys' built-in search
/// paths. Selecting it before bindgen runs also avoids the Command Line Tools
/// libclang, whose `@rpath` install name cannot be resolved by Cargo's build
/// script binaries. The package-manager operation is idempotent: an existing
/// `llvm` formula is left untouched.
pub fn ensure() {
    println!("cargo:rerun-if-changed=../../build_support/libclang.rs");
    println!("cargo:rerun-if-env-changed=LIBCLANG_PATH");

    if let Some(path) = env::var_os("LIBCLANG_PATH") {
        let path = PathBuf::from(path);
        if !cfg!(target_os = "macos") || path.is_file() || path.join("libclang.dylib").is_file() {
            return;
        }
        panic!(
            "LIBCLANG_PATH does not contain libclang.dylib: {}",
            path.display()
        );
    }

    if !cfg!(target_os = "macos") {
        return;
    }

    let brew = match Command::new("brew").arg("--version").output() {
        Ok(output) if output.status.success() => "brew",
        _ => panic!(
            "libclang is required for bindgen; install Homebrew and LLVM (brew install llvm), or set LIBCLANG_PATH"
        ),
    };

    let llvm_prefix = Command::new(brew)
        .args(["--prefix", "llvm"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()));

    let llvm_prefix = llvm_prefix.or_else(|| {
        let status = Command::new(brew).args(["install", "llvm"]).status().ok()?;
        if !status.success() {
            return None;
        }
        let output = Command::new(brew).args(["--prefix", "llvm"]).output().ok()?;
        output
            .status
            .success()
            .then(|| PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()))
    });

    let lib_dir = llvm_prefix.map(|prefix| prefix.join("lib"));
    let Some(lib_dir) = lib_dir.filter(|path| path.join("libclang.dylib").is_file()) else {
        panic!(
            "Homebrew LLVM is missing libclang.dylib; reinstall it with `brew reinstall llvm`, or set LIBCLANG_PATH"
        );
    };

    // Build scripts are single-threaded here, before bindgen invokes clang.
    // Rust 2024 makes process-wide environment mutation explicit.
    unsafe { env::set_var("LIBCLANG_PATH", &lib_dir) };
    println!("cargo:warning=using Homebrew libclang from {}", lib_dir.display());
}
