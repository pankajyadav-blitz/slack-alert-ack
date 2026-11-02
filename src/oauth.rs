//! One-shot helper: obtain a Slack *user* token (xoxp-) via OAuth + PKCE.
//! Port of get_user_token.py, as `ack-bot get-user-token`.
//!
//! Why this exists: the app's "Install to Workspace" button only requests the
//! scopes saved under User Token Scopes in the app config. Requesting `user_scope`
//! in the authorize URL grants them without touching that UI. Slack requires PKCE
//! whenever redirect_uri is a non-web URI such as https://localhost/..., hence the
//! code_verifier/code_challenge pair below.
//!
//! Writes SLACK_USER_TOKEN into the credentials file on success. The authorization
//! code is single-use and expires in ~10 minutes, so finish the browser step promptly.

use std::fs;
use std::io::{self, BufRead, Read, Write};
use std::path::PathBuf;
use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::tokens;
use crate::util::die;

const REDIRECT_URI: &str = "https://localhost/slack/callback";

const USER_SCOPES: [&str; 6] = [
    "groups:history",   // private channels -- the whole point
    "groups:read",
    "channels:history", // public channels
    "channels:read",
    "chat:write",
    "reactions:write",
];

fn verifier_file() -> PathBuf {
    let env = tokens::env_path();
    let dir = env.parent().filter(|p| !p.as_os_str().is_empty());
    match dir {
        Some(dir) => dir.join(".pkce_verifier"),
        None => PathBuf::from(".pkce_verifier"),
    }
}

/// base64url with no padding -- what PKCE wants, in the 20 lines it takes, rather
/// than a dependency.
fn b64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity((bytes.len() * 4 + 2) / 3);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        let indices = [
            (n >> 18) & 63,
            (n >> 12) & 63,
            (n >> 6) & 63,
            n & 63,
        ];
        for (i, index) in indices.iter().enumerate() {
            // Emit only the characters the input actually covers, which is what
            // dropping the '=' padding amounts to.
            if i <= chunk.len() {
                out.push(ALPHABET[*index as usize] as char);
            }
        }
    }
    out
}

fn random_bytes(count: usize) -> Vec<u8> {
    let mut buf = vec![0u8; count];
    let mut file = fs::File::open("/dev/urandom")
        .unwrap_or_else(|e| die(format!("cannot read /dev/urandom: {e}")));
    file.read_exact(&mut buf)
        .unwrap_or_else(|e| die(format!("cannot read /dev/urandom: {e}")));
    buf
}

fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Mint a PKCE verifier, stash it, and return the authorize URL.
///
/// The verifier is written to disk because the browser step happens out of band --
/// `--url` and `--exchange` are separate process runs.
fn build_authorize() -> String {
    let verifier = b64url(&random_bytes(64));
    let challenge = b64url(&Sha256::digest(verifier.as_bytes()));
    let path = verifier_file();
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        let _ = fs::create_dir_all(parent);
        tokens::set_mode(parent, 0o700);
    }
    if let Err(e) = fs::write(&path, &verifier) {
        die(format!("cannot write {}: {e}", path.display()));
    }
    tokens::set_mode(&path, 0o600);

    format!(
        "https://slack.com/oauth/v2/authorize?client_id={}&user_scope={}&redirect_uri={}\
         &code_challenge={}&code_challenge_method=S256",
        urlencode(&tokens::client_id()),
        urlencode(&USER_SCOPES.join(",")),
        urlencode(REDIRECT_URI),
        urlencode(&challenge),
    )
}

fn post(params: &[(&str, String)]) -> Value {
    let http = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap_or_else(|e| die(format!("cannot build an http client: {e}")));
    let resp = http
        .post("https://slack.com/api/oauth.v2.access")
        .form(params)
        .send()
        .unwrap_or_else(|e| die(format!("could not reach Slack: {e}")));
    let text = resp
        .text()
        .unwrap_or_else(|e| die(format!("could not read Slack's reply: {e}")));
    serde_json::from_str(&text).unwrap_or_else(|_| {
        let snippet: String = text.chars().take(200).collect();
        die(format!("Slack returned non-JSON: {snippet}"))
    })
}

fn prompt(message: &str) -> String {
    print!("{message}");
    let _ = io::stdout().flush();
    let mut line = String::new();
    match io::stdin().lock().read_line(&mut line) {
        Ok(0) | Err(_) => die("nothing on stdin to read"),
        Ok(_) => line.trim().to_string(),
    }
}

pub fn main(url_only: bool, exchange: Option<String>) -> i32 {
    if url_only {
        println!("{}", build_authorize());
        return 0;
    }

    if let Some(raw) = exchange {
        let path = verifier_file();
        let Ok(verifier) = fs::read_to_string(&path) else {
            eprintln!("no stashed verifier -- run with --url first");
            return 1;
        };
        let raw = if raw.trim().is_empty() {
            prompt("code or redirected URL: ")
        } else {
            raw
        };
        return finish(verifier.trim(), &raw);
    }

    // Interactive single-shot mode.
    let authorize = build_authorize();
    let verifier = fs::read_to_string(verifier_file()).unwrap_or_default();

    println!("\n1. Open this URL in the browser logged into your workspace:\n");
    println!("{authorize}");
    println!("\n2. Click Allow. The browser will fail to load localhost -- that is");
    println!("   expected. Copy the whole address-bar URL (it contains ?code=...).\n");

    let raw = prompt("Paste the redirected URL (or just the code): ");
    finish(verifier.trim(), &raw)
}

fn code_res() -> (&'static Regex, &'static Regex) {
    static IN_URL: OnceLock<Regex> = OnceLock::new();
    static BARE: OnceLock<Regex> = OnceLock::new();
    (
        IN_URL.get_or_init(|| Regex::new(r"[?&]code=([^&\s]+)").unwrap()),
        BARE.get_or_init(|| Regex::new(r"^\d+\.\d+\.[0-9a-f]+$").unwrap()),
    )
}

fn finish(verifier: &str, raw: &str) -> i32 {
    let raw = raw.trim();
    let (in_url, bare) = code_res();
    let code = if let Some(caps) = in_url.captures(raw) {
        caps[1].to_string()
    } else if bare.is_match(raw) {
        raw.to_string() // bare code, pasted without the URL
    } else {
        // Sending this to Slack only ever returns invalid_code, so say what is
        // actually wrong: the redirect carried no authorization code back.
        eprintln!("\nThat has no '?code=' in it -- it is the bare redirect URL.");
        eprintln!("After clicking Allow, the address bar should read:\n");
        eprintln!("  {REDIRECT_URI}?code=514487189412.XXXXXXXXXX.abcdef123...\n");
        eprintln!("If there is no ?code= there, the Allow step did not complete --");
        eprintln!("check whether the consent screen showed an approval or error message.");
        return 1;
    };

    let mut params: Vec<(&str, String)> = vec![
        ("client_id", tokens::client_id()),
        ("code", code),
        ("redirect_uri", REDIRECT_URI.to_string()),
        ("code_verifier", verifier.to_string()),
    ];
    let mut data = post(&params);

    let ok = data.get("ok").and_then(Value::as_bool) == Some(true);
    let error = data.get("error").and_then(Value::as_str).unwrap_or("").to_string();
    if !ok
        && matches!(
            error.as_str(),
            "invalid_client_id" | "bad_client_secret" | "invalid_client" | "client_secret_missing"
        )
    {
        // Confidential client: Slack wants the secret alongside PKCE. Prompt rather
        // than taking it on argv, so it stays out of shell history.
        println!("\nSlack says: {error} -- this app needs its client secret too.");
        println!("Basic Information -> App Credentials -> Client Secret -> Show\n");
        let secret = rpassword::prompt_password("client secret (hidden): ")
            .unwrap_or_else(|e| die(format!("could not read the secret: {e}")));
        params.push(("client_secret", secret.trim().to_string()));
        data = post(&params);
    }

    if data.get("ok").and_then(Value::as_bool) != Some(true) {
        let error = data.get("error").and_then(Value::as_str).unwrap_or("unknown");
        eprintln!("\nFAILED: {error}");
        if error == "invalid_code" {
            eprintln!("The code was already used or expired -- rerun and be quick.");
        }
        return 1;
    }

    let authed = data.get("authed_user").cloned().unwrap_or(Value::Null);
    let token = authed.get("access_token").and_then(Value::as_str);
    let Some(token) = token else {
        eprintln!("\nNo user token in the response. Slack returned only a bot token, which");
        let mut keys: Vec<&str> = data
            .as_object()
            .map(|o| o.keys().map(String::as_str).collect())
            .unwrap_or_default();
        keys.sort_unstable();
        eprintln!("means user_scope was not granted. Keys present: {keys:?}");
        return 1;
    };

    // Rotation-enabled apps hand back the refresh token and lifetime right here, and
    // nowhere else ever again -- drop them and the 12h token can only be replaced by
    // redoing this whole browser flow. See tokens.rs.
    let refresh_token = authed.get("refresh_token").and_then(Value::as_str);
    let expires_in = authed.get("expires_in").and_then(Value::as_i64);
    let path = tokens::env_path();
    if let Err(e) = tokens::save(token, refresh_token, expires_in, path) {
        die(format!("cannot write {}: {e}", path.display()));
    }

    // The refresh call needs it too, and it is only shown in the dashboard.
    if let Some((_, secret)) = params.iter().find(|(k, _)| *k == "client_secret") {
        if let Err(e) = tokens::update_env(
            &[("SLACK_CLIENT_SECRET".to_string(), secret.clone())],
            path,
        ) {
            die(format!("cannot write {}: {e}", path.display()));
        }
    }

    let scopes = authed.get("scope").and_then(Value::as_str).unwrap_or("");
    println!(
        "\nOK -- wrote SLACK_USER_TOKEN to {} (prefix {})",
        path.display(),
        &token[..token.len().min(5)]
    );
    println!("granted user scopes: {scopes}");
    if tokens::rotating(Some(token)) {
        match refresh_token {
            Some(_) => println!(
                "token rotation is on: saved a refresh token too, valid ~{}h per cycle.\n\
                 the bot renews itself from here on -- no repeat of this flow.",
                expires_in.unwrap_or(0) / 3600
            ),
            None => println!(
                "\nWARNING: this is a rotating token but Slack returned no refresh token,\n\
                 so it will stop working in ~12h and you will land back on this flow."
            ),
        }
    }
    println!("\nNext:  ack-bot --list-channels");
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b64url_matches_the_reference_encoding() {
        // Vectors from RFC 4648 with padding stripped, plus the url-safe alphabet.
        assert_eq!(b64url(b""), "");
        assert_eq!(b64url(b"f"), "Zg");
        assert_eq!(b64url(b"fo"), "Zm8");
        assert_eq!(b64url(b"foo"), "Zm9v");
        assert_eq!(b64url(b"foob"), "Zm9vYg");
        assert_eq!(b64url(b"fooba"), "Zm9vYmE");
        assert_eq!(b64url(b"foobar"), "Zm9vYmFy");
        assert_eq!(b64url(&[0xfb, 0xff, 0xfe]), "-__-");
    }

    #[test]
    fn challenge_is_the_sha256_of_the_verifier() {
        // The one PKCE vector from RFC 7636 appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            b64url(&Sha256::digest(verifier.as_bytes())),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn urlencode_escapes_what_a_query_string_cannot_hold() {
        assert_eq!(urlencode("a,b:c/d"), "a%2Cb%3Ac%2Fd");
        assert_eq!(urlencode("plain-value_1.0~"), "plain-value_1.0~");
    }
}
