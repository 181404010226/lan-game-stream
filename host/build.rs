fn main() {
    println!("cargo:rerun-if-changed=native/nvenc.cpp");
    println!("cargo:rerun-if-changed=native/nvEncodeAPI.h");
    cc::Build::new()
        .cpp(true)
        .file("native/nvenc.cpp")
        .flag_if_supported("/std:c++17")
        .compile("lan_nvenc");
}
