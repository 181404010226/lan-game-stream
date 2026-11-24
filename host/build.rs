fn main() {
    println!("cargo:rerun-if-changed=native/nvenc.cpp");
    println!("cargo:rerun-if-changed=native/nvEncodeAPI.h");
    println!("cargo:rerun-if-changed=native/game_texture.cpp");
    println!("cargo:rerun-if-changed=../render-bridge/frame_link.h");
    cc::Build::new()
        .cpp(true)
        .file("native/nvenc.cpp")
        .file("native/game_texture.cpp")
        .flag_if_supported("/std:c++17")
        .compile("lan_nvenc");
}
