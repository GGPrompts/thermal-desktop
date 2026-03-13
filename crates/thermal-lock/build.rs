fn main() {
    // Link against libpam for PAM authentication FFI
    println!("cargo:rustc-link-lib=pam");
}
