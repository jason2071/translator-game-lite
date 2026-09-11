fn main() {
    slint_build::compile("ui/app.slint").unwrap();

    // Embed the Windows application icon (exe file, taskbar, title bar).
    #[cfg(target_os = "windows")]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/app.ico");
        res.compile().unwrap();
    }
}
