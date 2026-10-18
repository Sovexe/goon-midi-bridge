fn main() -> std::io::Result<()> {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    winresource::WindowsResource::new()
        .set_icon("assets/icon.ico")
        .compile()
}
