//! Google OAuth 2.0 and Google Drive spreadsheet sync.

use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

pub const DEFAULT_SPREADSHEET_ID: &str = "1mrj9GLGbLypEscPTVUVOvhT-i2pV4RwqODnbUhjYDHg";
const REDIRECT_PORT: u16 = 8085;
const SCOPE: &str = "https://www.googleapis.com/auth/drive.readonly";

fn config_dir() -> PathBuf {
    let mut p = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    p.push("timepipe");
    let _ = fs::create_dir_all(&p);
    p
}

fn client_secret_path() -> PathBuf {
    config_dir().join("client_secret.json")
}

fn token_path() -> PathBuf {
    config_dir().join("token.json")
}

#[derive(Deserialize)]
struct InstalledApp {
    client_id: String,
    client_secret: String,
}

#[derive(Deserialize)]
struct SecretFile {
    installed: Option<InstalledApp>,
    web: Option<InstalledApp>,
}

#[derive(Serialize, Deserialize, Clone)]
struct StoredToken {
    access_token: String,
    refresh_token: Option<String>,
    #[serde(default)]
    token_type: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    #[serde(default)]
    token_type: String,
}

fn load_client_credentials() -> Result<(String, String), String> {
    let p = client_secret_path();
    let content = fs::read_to_string(&p).map_err(|_| {
        format!(
            "Missing credentials file: {}\nPlease place your downloaded OAuth JSON file there.",
            p.display()
        )
    })?;
    let secret: SecretFile = serde_json::from_str(&content)
        .map_err(|e| format!("Failed to parse client_secret.json: {e}"))?;

    let app = secret
        .installed
        .or(secret.web)
        .ok_or_else(|| "Invalid client_secret.json: neither 'installed' nor 'web' key found.".to_string())?;

    Ok((app.client_id, app.client_secret))
}

fn load_stored_token() -> Option<StoredToken> {
    let p = token_path();
    fs::read_to_string(p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
}

fn save_token(token: &StoredToken) -> Result<(), String> {
    let p = token_path();
    let s = serde_json::to_string_pretty(token).map_err(|e| e.to_string())?;
    fs::write(p, s).map_err(|e| e.to_string())
}

/// Runs a temporary HTTP server on localhost to capture OAuth 2.0 authorization code.
fn login_flow(client_id: &str, client_secret: &str) -> Result<StoredToken, String> {
    let redirect_uri = format!("http://localhost:{REDIRECT_PORT}");
    let auth_url = format!(
        "https://accounts.google.com/o/oauth2/v2/auth?client_id={}&redirect_uri={}&response_type=code&scope={}&access_type=offline&prompt=consent",
        urlencoding::encode(client_id),
        urlencoding::encode(&redirect_uri),
        urlencoding::encode(SCOPE)
    );

    println!("\nOpening browser for Google authorization...");
    println!("If the browser does not open automatically, visit:\n{}\n", auth_url);
    let _ = webbrowser::open(&auth_url);

    let server = tiny_http::Server::http(format!("127.0.0.1:{REDIRECT_PORT}"))
        .map_err(|e| format!("Failed to start local server on port {REDIRECT_PORT}: {e}"))?;

    println!("Waiting for authorization in browser...");

    let mut auth_code = None;
    for request in server.incoming_requests() {
        let url = request.url().to_string();
        if let Some(pos) = url.find("code=") {
            let code_part = &url[pos + 5..];
            let code = code_part.split('&').next().unwrap_or("");
            auth_code = Some(urlencoding::decode(code).map_err(|e| e.to_string())?);

            let response = tiny_http::Response::from_string(
                "<html><body><h2>Authorization successful!</h2><p>You can close this tab and return to timepipe.</p></body></html>",
            )
            .with_header(tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap());
            let _ = request.respond(response);
            break;
        } else {
            let response = tiny_http::Response::from_string("Waiting for Google authentication...");
            let _ = request.respond(response);
        }
    }

    let code = auth_code.ok_or_else(|| "Failed to receive authorization code from Google.".to_string())?;

    // Exchange code for tokens
    let token_resp: TokenResponse = ureq::post("https://oauth2.googleapis.com/token")
        .send_form(&[
            ("code", &code),
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("redirect_uri", &redirect_uri),
            ("grant_type", "authorization_code"),
        ])
        .map_err(|e| format!("Token exchange error: {e}"))?
        .into_json()
        .map_err(|e| format!("Failed to parse token response: {e}"))?;

    let stored = StoredToken {
        access_token: token_resp.access_token,
        refresh_token: token_resp.refresh_token,
        token_type: token_resp.token_type,
    };
    save_token(&stored)?;
    Ok(stored)
}

fn refresh_access_token(client_id: &str, client_secret: &str, refresh_token: &str) -> Result<StoredToken, String> {
    let token_resp: TokenResponse = ureq::post("https://oauth2.googleapis.com/token")
        .send_form(&[
            ("refresh_token", refresh_token),
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("grant_type", "refresh_token"),
        ])
        .map_err(|e| format!("Token refresh error: {e}"))?
        .into_json()
        .map_err(|e| format!("Failed to parse refreshed token response: {e}"))?;

    let stored = StoredToken {
        access_token: token_resp.access_token,
        refresh_token: Some(refresh_token.to_string()),
        token_type: token_resp.token_type,
    };
    save_token(&stored)?;
    Ok(stored)
}

pub fn get_valid_token() -> Result<String, String> {
    let (client_id, client_secret) = load_client_credentials()?;

    if let Some(token) = load_stored_token() {
        // Try testing current access token
        let test = ureq::get("https://www.googleapis.com/drive/v3/about?fields=user")
            .set("Authorization", &format!("Bearer {}", token.access_token))
            .call();

        if test.is_ok() {
            return Ok(token.access_token);
        }

        // Token expired, try refresh if available
        if let Some(ref rf) = token.refresh_token {
            if let Ok(refreshed) = refresh_access_token(&client_id, &client_secret, rf) {
                return Ok(refreshed.access_token);
            }
        }
    }

    // Need new login
    let new_token = login_flow(&client_id, &client_secret)?;
    Ok(new_token.access_token)
}

/// Downloads the Google Sheets document as XLSX and replaces `target_file`.
pub fn sync_spreadsheet(file_id: &str, target_file: &Path) -> Result<String, String> {
    let token = get_valid_token()?;
    let url = format!(
        "https://docs.google.com/spreadsheets/d/{}/export?format=xlsx",
        file_id
    );

    let resp = match ureq::get(&url)
        .set("Authorization", &format!("Bearer {}", token))
        .call()
    {
        Ok(r) => r,
        Err(e) => {
            // If docs.google.com failed, fallback to drive api
            let drive_url = format!(
                "https://www.googleapis.com/drive/v3/files/{}/export?mimeType=application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                file_id
            );
            ureq::get(&drive_url)
                .set("Authorization", &format!("Bearer {}", token))
                .call()
                .map_err(|e2| format!("Download error: {e} | fallback: {e2}"))?
        }
    };

    let mut bytes = Vec::new();
    resp.into_reader()
        .read_to_end(&mut bytes)
        .map_err(|e| format!("Failed reading response: {e}"))?;

    // Validate that it's a valid ZIP/XLSX (starts with PK\x03\x04)
    if bytes.len() < 4 || &bytes[..4] != b"PK\x03\x04" {
        let preview = String::from_utf8_lossy(&bytes[..bytes.len().min(300)]);
        return Err(format!("Not an XLSX file (response preview: {})", preview.trim()));
    }

    // Write atomically so a failed write never corrupts the existing sheet
    let tmp = target_file.with_extension("xlsx.tmp");
    fs::write(&tmp, &bytes).map_err(|e| format!("Failed to write XLSX to {}: {e}", tmp.display()))?;
    fs::rename(&tmp, target_file).map_err(|e| format!("Failed to replace {}: {e}", target_file.display()))?;

    Ok(format!("Synced successfully ({:.1} MB)", bytes.len() as f64 / (1024.0 * 1024.0)))
}

mod urlencoding {
    pub fn encode(s: &str) -> String {
        let mut out = String::new();
        for b in s.bytes() {
            if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' || b == b'~' {
                out.push(b as char);
            } else {
                out.push_str(&format!("%{:02X}", b));
            }
        }
        out
    }

    pub fn decode(s: &str) -> Result<String, std::string::FromUtf8Error> {
        let mut bytes = Vec::new();
        let mut chars = s.bytes();
        while let Some(b) = chars.next() {
            if b == b'%' {
                let h1 = chars.next().unwrap_or(0);
                let h2 = chars.next().unwrap_or(0);
                let hex_str = [h1, h2];
                if let Ok(v) = u8::from_str_radix(std::str::from_utf8(&hex_str).unwrap_or("00"), 16) {
                    bytes.push(v);
                }
            } else if b == b'+' {
                bytes.push(b' ');
            } else {
                bytes.push(b);
            }
        }
        String::from_utf8(bytes)
    }
}
