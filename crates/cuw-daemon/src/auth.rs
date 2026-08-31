//! The localhost bearer token (M2.4). Generated once at first run, stored in the
//! data dir, required on every route. This is the localhost gate — not a Claude
//! token — so it is safe to hold in the overlay's webview (plan §5).

use std::path::Path;

use rand::RngCore;

const FILE: &str = "bearer.token";

/// Read the bearer token, generating and persisting a fresh 256-bit one on first
/// run. Written `0600` on unix; on Windows it lands under `%APPDATA%\…\cuw`,
/// which inherits the user-profile ACL — there is no `0600` equivalent applied
/// here (plan §5).
pub fn load_or_create(data_dir: &Path) -> anyhow::Result<String> {
    let path = data_dir.join(FILE);
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let token = existing.trim();
        if !token.is_empty() {
            return Ok(token.to_string());
        }
    }

    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();

    std::fs::write(&path, &token)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(token)
}
