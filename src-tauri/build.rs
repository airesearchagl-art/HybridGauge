fn main() {
    // Tauri v2 の WindowsAttributes を使ってマニフェストを埋め込む。
    // winres と併用すると RT_MANIFEST が重複して CVT1100 になるため winres は使わない。
    #[allow(unused_mut)]
    let mut attrs = tauri_build::Attributes::new();

    #[cfg(windows)]
    {
        let windows_attrs = tauri_build::WindowsAttributes::new()
            .app_manifest(include_str!("windows/main.manifest"));
        attrs = attrs.windows_attributes(windows_attrs);
    }

    tauri_build::try_build(attrs).expect("failed to run tauri-build");
}
