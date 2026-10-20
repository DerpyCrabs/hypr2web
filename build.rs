fn main() {
    println!("cargo:rerun-if-changed=src/encoder.c");
    let mut build = cc::Build::new();
    for name in ["libavcodec", "libavutil", "libswscale"] {
        build.includes(
            pkg_config::probe_library(name)
                .expect("FFmpeg development libraries required")
                .include_paths,
        );
    }
    build.file("src/encoder.c").compile("hypr2web_nvenc");
}
