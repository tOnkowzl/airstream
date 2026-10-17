fn main() {
    #[cfg(windows)]
    {
        let mut res = winres::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("ProductName", "Airstream");
        res.set("FileDescription", "Airstream - cast to TV");
        if res.compile().is_ok() {
            // Link the resource object directly: a static lib's .rsrc is
            // dropped by the GNU linker because nothing references it.
            let out = std::env::var("OUT_DIR").unwrap();
            println!("cargo:rustc-link-arg-bins={out}/resource.o");
        }
    }
}
