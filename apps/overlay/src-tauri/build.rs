/// tauri-build's default manifest (Common-Controls v6) plus PerMonitorV2 DPI
/// awareness. tao sets PerMonitorV2 at runtime anyway; the manifest makes it
/// hold from the first frame (M6.8).
const MANIFEST: &str = r#"<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <dependency>
    <dependentAssembly>
      <assemblyIdentity
        type="win32"
        name="Microsoft.Windows.Common-Controls"
        version="6.0.0.0"
        processorArchitecture="*"
        publicKeyToken="6595b64144ccf1df"
        language="*"
      />
    </dependentAssembly>
  </dependency>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
    </windowsSettings>
  </application>
</assembly>
"#;

/// `tauri_build` validates every declared `bundle.resources` path *here*, in
/// the build script, so a checked-in resource entry would make a plain
/// `cargo check` depend on a release artefact of another workspace. The daemon
/// is therefore declared at bundle time by scripts/build-release.{ps1,sh}.
fn main() {
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .windows_attributes(tauri_build::WindowsAttributes::new().app_manifest(MANIFEST)),
    )
    .expect("tauri build");
}
