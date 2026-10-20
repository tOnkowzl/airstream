fn main() {
    #[cfg(windows)]
    {
        let mut res = winres::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("ProductName", "Airstream");
        res.set("FileDescription", "Airstream - cast to TV");
        // MSVC links winres' resource.lib itself; resource.o is GNU-only.
        let gnu = std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu");
        if res.compile().is_ok() && gnu {
            // Link the resource object directly: a static lib's .rsrc is
            // dropped by the GNU linker because nothing references it.
            let out = std::env::var("OUT_DIR").unwrap();
            println!("cargo:rustc-link-arg-bins={out}/resource.o");
        }
    }
}
