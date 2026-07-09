use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=CUDA_HOME");
    println!("cargo:rerun-if-env-changed=CUDA_PATH");

    let cuda_enabled = std::env::var_os("CARGO_FEATURE_CUDA").is_some();
    let gpu_counters_enabled = std::env::var_os("CARGO_FEATURE_GPU_COUNTERS").is_some();

    if !cuda_enabled {
        return;
    }

    for lib_dir in cuda_lib_dirs() {
        if lib_dir.exists() {
            println!("cargo:rustc-link-search=native={}", lib_dir.display());
        }
    }

    if gpu_counters_enabled {
        build_gpu_counters();
        println!("cargo:rustc-link-lib=static=micromeasure_gpu_counters");
        println!("cargo:rustc-link-lib=dylib=stdc++");
        println!("cargo:rustc-link-lib=dylib=cuda");
        println!("cargo:rustc-link-lib=dylib=cupti");
        println!("cargo:rerun-if-changed=native/gpu_counters.cpp");
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
        "extras/CUPTI/lib64",
        "extras/CUPTI/lib",
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

fn build_gpu_counters() {
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR is set by Cargo"));
    let object = out_dir.join("gpu_counters.o");
    let archive = out_dir.join("libmicromeasure_gpu_counters.a");

    let include_dir = cuda_include_dirs()
        .into_iter()
        .find(|dir| dir.join("cupti_profiler_host.h").exists())
        .expect("CUPTI profiler headers not found; set CUDA_HOME or CUDA_PATH");

    let status = std::process::Command::new("g++")
        .args(["-std=c++17", "-O2", "-I"])
        .arg(&include_dir)
        .args(["-c", "native/gpu_counters.cpp", "-o"])
        .arg(&object)
        .status()
        .expect("failed to run g++ for GPU counters");
    assert!(status.success(), "g++ failed to build GPU counters");

    let status = std::process::Command::new("ar")
        .arg("crs")
        .arg(&archive)
        .arg(&object)
        .status()
        .expect("failed to run ar for GPU counters");
    assert!(status.success(), "ar failed to archive GPU counters");

    println!("cargo:rustc-link-search=native={}", out_dir.display());
}

fn cuda_include_dirs() -> Vec<PathBuf> {
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
        for relative in [
            "include",
            "targets/x86_64-linux/include",
            "targets/sbsa-linux/include",
            "targets/aarch64-linux/include",
        ] {
            let dir = root.join(relative);
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
    }
    dirs
}
