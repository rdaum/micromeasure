use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=CUDA_HOME");
    println!("cargo:rerun-if-env-changed=CUDA_PATH");

    if std::env::var_os("CARGO_FEATURE_CUDA").is_none() {
        return;
    }

    for lib_dir in cuda_lib_dirs() {
        if lib_dir.exists() {
            println!("cargo:rustc-link-search=native={}", lib_dir.display());
        }
    }
}

fn cuda_lib_dirs() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(cuda_home) = std::env::var_os("CUDA_HOME") {
        roots.push(PathBuf::from(cuda_home));
    }
    if let Some(cuda_path) = std::env::var_os("CUDA_PATH") {
        roots.push(PathBuf::from(cuda_path));
    }
    roots.push(PathBuf::from("/usr/local/cuda"));
    roots.push(PathBuf::from("/usr/local/cuda-13"));
    roots.push(PathBuf::from("/usr/local/cuda-13.0"));

    let mut dirs = Vec::new();
    for root in roots {
        push_cuda_lib_dirs(&mut dirs, &root);
    }
    dirs
}

fn push_cuda_lib_dirs(dirs: &mut Vec<PathBuf>, root: &Path) {
    for relative in [
        "lib64",
        "lib",
        "targets/x86_64-linux/lib",
        "targets/sbsa-linux/lib",
        "targets/aarch64-linux/lib",
    ] {
        let dir = root.join(relative);
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
}
