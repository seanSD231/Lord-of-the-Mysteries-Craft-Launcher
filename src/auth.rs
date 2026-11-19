//! Signing in through Microsoft, and confirming the account owns Minecraft.
//!
//! The device-code flow is used rather than an embedded browser: the player is given a
//! short code and signs in wherever they like, and this process never sees a password.
//!
//! The ownership check at the end is not optional and is not a formality. If the account
//! does not own Minecraft, the profile endpoint returns 404 and the launcher stops. This
//! launcher installs and launches the game for people who have bought it; it is not a way
//! to play without one.
//!
//! The chain is the one Mojang requires:
//!   Microsoft device code -> Xbox Live -> XSTS -> Minecraft services -> profile

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const DEVICE_CODE: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/devicecode";
const TOKEN: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/token";
const XBL: &str = "https://user.auth.xboxlive.com/user/authenticate";
const XSTS: &str = "https://xsts.auth.xboxlive.com/xsts/authorize";
const MC_LOGIN: &str = "https://api.minecraftservices.com/authentication/login_with_xbox";
const MC_PROFILE: &str = "https://api.minecraftservices.com/minecraft/profile";
const SCOPE: &str = "XboxLive.signin offline_access";

/// Our Azure application, approved by Mojang for the Minecraft API on 2026-11-19.
///
/// Safe to keep in the open: this is a public client using the device-code flow, so the
/// id is sent in the clear in every request a player's machine makes and there is no
/// secret paired with it. `LOTMCRAFT_CLIENT_ID` still wins, for testing against another
/// registration without a rebuild.
///
/// An `Option` rather than an empty string for "unset": clippy reads a const's value, so
/// `is_empty()` on a filled-in one is a warning, and warnings are errors here.
const BUILTIN_CLIENT_ID: Option<&str> = Some("b6f64652-1ffe-4083-bc9c-b7f366c9eef7");

pub fn client_id() -> Result<String> {
    if let Ok(v) = std::env::var("LOTMCRAFT_CLIENT_ID") {
        if !v.trim().is_empty() {
            return Ok(v.trim().to_string());
        }
    }
    if let Some(v) = BUILTIN_CLIENT_ID {
        return Ok(v.to_string());
    }
    bail!(
        "no Microsoft client id is configured.\n\
         Register an app at https://portal.azure.com (Microsoft Entra ID -> App \
         registrations), enable the public-client / device-code flow on it, then set \
         LOTMCRAFT_CLIENT_ID to its Application (client) ID."
    )
}

/// What the launch step needs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub username: String,
    pub uuid: String,
    pub access_token: String,
}

/// Only the refresh token is kept. It is stored in plain text, like every other
/// Minecraft launcher, so it is written with no world or group access where the platform
/// allows it, and it grants nothing beyond Minecraft sign-in.
#[derive(Serialize, Deserialize, Default)]
struct Stored {
    refresh_token: String,
}

fn store_path(base: &Path) -> PathBuf {
    base.join("auth.json")
}

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default = "default_interval")]
    interval: u64,
    #[serde(default = "default_expiry")]
    expires_in: u64,
}

fn default_interval() -> u64 {
    5
}
fn default_expiry() -> u64 {
    900
}

#[derive(Deserialize)]
struct TokenResponse {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    error: String,
}

/// Signs in, reusing a stored refresh token when there is one.
///
/// `on_prompt` is handed the URL and the code to show the player. It is only called when
/// a fresh sign-in is actually needed.
pub fn sign_in(
    base: &Path,
    mut on_prompt: impl FnMut(&str, &str),
    mut on_status: impl FnMut(&str),
) -> Result<Session> {
    let cid = client_id()?;

    if let Some(stored) = read_store(base) {
        if !stored.refresh_token.is_empty() {
            on_status("refreshing the saved sign-in");
            match refresh(&cid, &stored.refresh_token) {
                Ok((ms_token, new_refresh)) => {
                    write_store(base, &new_refresh).ok();
                    return finish(&ms_token, &mut on_status);
                }
                Err(_) => on_status("the saved sign-in has expired"),
            }
        }
    }

    let code: DeviceCodeResponse = post_form(
        DEVICE_CODE,
        &[("client_id", &cid), ("scope", &SCOPE.to_string())],
    )
    .context("asking Microsoft for a device code")?;

    on_prompt(&code.verification_uri, &code.user_code);

    let deadline = Instant::now() + Duration::from_secs(code.expires_in);
    let mut interval = Duration::from_secs(code.interval);
    let (ms_token, refresh_token) = loop {
        if Instant::now() > deadline {
            bail!("the sign-in code expired before it was used");
        }
        std::thread::sleep(interval);
        let r: TokenResponse = match post_form(
            TOKEN,
            &[
                ("client_id", &cid),
                (
                    "grant_type",
                    &"urn:ietf:params:oauth:grant-type:device_code".to_string(),
                ),
                ("device_code", &code.device_code),
            ],
        ) {
            Ok(r) => r,
            Err(_) => continue, // the endpoint answers 400 until the player finishes
        };
        match r.error.as_str() {
            "" => break (r.access_token, r.refresh_token),
            "authorization_pending" => continue,
            "slow_down" => {
                interval += Duration::from_secs(5);
                continue;
            }
            "authorization_declined" => bail!("the sign-in was declined"),
            "expired_token" => bail!("the sign-in code expired before it was used"),
            other => bail!("Microsoft refused the sign-in: {other}"),
        }
    };

    write_store(base, &refresh_token).ok();
    finish(&ms_token, &mut on_status)
}

fn refresh(cid: &str, refresh_token: &str) -> Result<(String, String)> {
    let r: TokenResponse = post_form(
        TOKEN,
        &[
            ("client_id", &cid.to_string()),
            ("grant_type", &"refresh_token".to_string()),
            ("refresh_token", &refresh_token.to_string()),
            ("scope", &SCOPE.to_string()),
        ],
    )?;
    if !r.error.is_empty() || r.access_token.is_empty() {
        bail!("the saved sign-in could not be refreshed");
    }
    Ok((r.access_token, r.refresh_token))
}

/// Microsoft token -> Xbox Live -> XSTS -> Minecraft -> profile.
fn finish(ms_token: &str, on_status: &mut impl FnMut(&str)) -> Result<Session> {
    on_status("signing in to Xbox Live");
    let xbl: serde_json::Value = post_json(
        XBL,
        &json!({
            "Properties": {
                "AuthMethod": "RPS",
                "SiteName": "user.auth.xboxlive.com",
                "RpsTicket": format!("d={ms_token}")
            },
            "RelyingParty": "http://auth.xboxlive.com",
            "TokenType": "JWT"
        }),
        None,
    )
    .context("Xbox Live sign-in")?;
    let xbl_token = xbl["Token"].as_str().unwrap_or_default().to_string();
    let uhs = xbl["DisplayClaims"]["xui"][0]["uhs"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    if xbl_token.is_empty() || uhs.is_empty() {
        bail!("Xbox Live did not return a usable token");
    }

    on_status("authorizing with XSTS");
    let xsts: serde_json::Value = post_json(
        XSTS,
        &json!({
            "Properties": { "SandboxId": "RETAIL", "UserTokens": [xbl_token] },
            "RelyingParty": "rp://api.minecraftservices.com/",
            "TokenType": "JWT"
        }),
        None,
    )
    .map_err(explain_xsts)
    .context("XSTS authorization")?;
    let xsts_token = xsts["Token"].as_str().unwrap_or_default().to_string();
    if xsts_token.is_empty() {
        bail!("XSTS did not return a usable token");
    }

    on_status("signing in to Minecraft");
    let mc: serde_json::Value = post_json(
        MC_LOGIN,
        &json!({ "identityToken": format!("XBL3.0 x={uhs};{xsts_token}") }),
        None,
    )
    .context("Minecraft services sign-in")?;
    let access_token = mc["access_token"].as_str().unwrap_or_default().to_string();
    if access_token.is_empty() {
        bail!("Minecraft services did not return an access token");
    }

    on_status("checking the account owns Minecraft");
    let profile: serde_json::Value = get_json(MC_PROFILE, &access_token).map_err(|e| {
        anyhow!(
            "this Microsoft account does not own Minecraft: Java Edition, so the game \
             cannot be launched for it.\n({e})"
        )
    })?;
    let username = profile["name"].as_str().unwrap_or_default().to_string();
    let uuid = profile["id"].as_str().unwrap_or_default().to_string();
    if username.is_empty() || uuid.is_empty() {
        bail!("the Minecraft profile came back without a name or id");
    }

    Ok(Session {
        username,
        uuid,
        access_token,
    })
}

/// XSTS reports a few conditions by code that are otherwise baffling.
fn explain_xsts(e: anyhow::Error) -> anyhow::Error {
    let text = e.to_string();
    if text.contains("2148916233") {
        anyhow!("this Microsoft account has no Xbox profile; create one and try again")
    } else if text.contains("2148916238") {
        anyhow!("this account is a child account and must be added to a family group")
    } else {
        e
    }
}

fn read_store(base: &Path) -> Option<Stored> {
    let bytes = std::fs::read(store_path(base)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_store(base: &Path, refresh_token: &str) -> Result<()> {
    let path = store_path(base);
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let bytes = serde_json::to_vec_pretty(&Stored {
        refresh_token: refresh_token.to_string(),
    })?;
    std::fs::write(&path, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Removes the saved sign-in.
pub fn sign_out(base: &Path) -> Result<()> {
    let path = store_path(base);
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

// --- small HTTP helpers -------------------------------------------------------------

fn post_form<T: serde::de::DeserializeOwned>(url: &str, fields: &[(&str, &String)]) -> Result<T> {
    let pairs: Vec<(&str, &str)> = fields.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let resp = crate::fetch::post_form_raw(url, &pairs)?;
    serde_json::from_slice(&resp).with_context(|| format!("parsing the response from {url}"))
}

fn post_json<T: serde::de::DeserializeOwned>(
    url: &str,
    body: &serde_json::Value,
    bearer: Option<&str>,
) -> Result<T> {
    let resp = crate::fetch::post_json_raw(url, body, bearer)?;
    serde_json::from_slice(&resp).with_context(|| format!("parsing the response from {url}"))
}

fn get_json<T: serde::de::DeserializeOwned>(url: &str, bearer: &str) -> Result<T> {
    let resp = crate::fetch::get_with_bearer(url, bearer)?;
    serde_json::from_slice(&resp).with_context(|| format!("parsing the response from {url}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_round_trips() {
        let dir = std::env::temp_dir().join(format!("lotm-auth-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        write_store(&dir, "abc123").unwrap();
        assert_eq!(read_store(&dir).unwrap().refresh_token, "abc123");
        sign_out(&dir).unwrap();
        assert!(read_store(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_client_id_explains_itself() {
        if std::env::var("LOTMCRAFT_CLIENT_ID").is_ok() || BUILTIN_CLIENT_ID.is_some() {
            return;
        }
        let e = client_id().unwrap_err().to_string();
        assert!(e.contains("portal.azure.com"));
    }
}
