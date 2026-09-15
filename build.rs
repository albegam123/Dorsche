fn main() {
    cxx_build::bridge("src/bridge.rs")
        .file("src/dorsche_core.cc")
        .include("include")
        .flag_if_supported("-std=c++20")
        .flag_if_supported("-Wall")
        .flag_if_supported("-Wextra")
        .compile("dorsche-core");

    println!("cargo:rerun-if-changed=src/bridge.rs");
    println!("cargo:rerun-if-changed=src/dorsche_core.cc");
    println!("cargo:rerun-if-changed=include/dorsche_core.h");
}
