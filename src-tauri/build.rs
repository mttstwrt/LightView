fn main() {
    // intel_tex_2 contains ISPC-compiled C++ objects that require libstdc++.
    println!("cargo:rustc-link-lib=stdc++");
    tauri_build::build()
}
