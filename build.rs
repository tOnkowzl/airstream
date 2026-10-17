fn main() {
    #[cfg(windows)]
    {
        let mut res = winres::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("ProductName", "Airstream");
        res.set("FileDescription", "Airstream - cast to TV");
        let _ = res.compile();
    }
}
