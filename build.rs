fn main() {
    // Embed Windows resources: icon and application manifest (auto-elevate to admin)
    #[cfg(windows)]
    {
        let mut res = winres::WindowsResource::new();
        res.set_icon("icon.ico");
        res.set_manifest_file("thermalstats.manifest");
        res.compile().expect("Failed to compile Windows resources");
    }
}
