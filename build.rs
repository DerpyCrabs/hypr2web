use std::{env, fs, path::PathBuf, process::Command};
fn main() {
    for path in ["src/encoder.c", "src/convert.cu"] {
        println!("cargo:rerun-if-changed={path}");
    }
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let cuda = PathBuf::from(env::var("CUDA_HOME").unwrap_or_else(|_| "/opt/cuda".into()));
    println!("cargo:rerun-if-env-changed=CUDA_HOME");
    let status = Command::new(cuda.join("bin/nvcc"))
        .args(["--ptx", "-O3", "-arch=compute_75", "src/convert.cu", "-o"])
        .arg(out.join("convert.ptx"))
        .status()
        .expect("CUDA toolkit (nvcc) required");
    assert!(status.success(), "CUDA kernel compilation failed");
    let ptx = fs::read(out.join("convert.ptx")).unwrap();
    let data = ptx
        .iter()
        .chain(std::iter::once(&0))
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(",");
    fs::write(
        out.join("convert_ptx.h"),
        format!("static const unsigned char convert_ptx[] = {{{data}}};"),
    )
    .unwrap();
    let mut build = cc::Build::new();
    for name in ["libavcodec", "libavutil", "libswscale"] {
        build.includes(
            pkg_config::probe_library(name)
                .expect("FFmpeg development libraries required")
                .include_paths,
        );
    }
    build
        .include(cuda.join("include"))
        .include(out)
        .file("src/encoder.c")
        .compile("hypr2web_nvenc");
    println!("cargo:rustc-link-lib=cuda");
}
