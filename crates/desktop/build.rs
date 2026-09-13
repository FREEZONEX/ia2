fn main() {
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=assets/ia2.ico");
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("assets/ia2.ico");
        resource.set("ProductName", "IA2");
        resource.set("FileDescription", "IA2 industrial automation IDE");
        resource.set("OriginalFilename", "IA2.exe");
        resource.compile().expect("compile IA2 Windows resources");
    }
}
