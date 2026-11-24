//! Start the GUI at login (the helper is started by systemd / the service manager).

fn build() -> Result<auto_launch::AutoLaunch, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    auto_launch::AutoLaunchBuilder::new()
        .set_app_name("StudyForge")
        .set_app_path(&exe.to_string_lossy())
        .set_args(&["--minimized"])
        .build()
        .map_err(|e| e.to_string())
}

pub fn is_enabled() -> bool {
    build().and_then(|a| a.is_enabled().map_err(|e| e.to_string())).unwrap_or(false)
}

pub fn set(enabled: bool) -> Result<(), String> {
    let a = build()?;
    if enabled {
        a.enable().map_err(|e| e.to_string())
    } else if a.is_enabled().unwrap_or(false) {
        a.disable().map_err(|e| e.to_string())
    } else {
        Ok(())
    }
}
