//! Claude Code accesses this item through Apple's security tool. A native
//! write from tokeman rebinds the item's partition to the tokeman binary,
//! which makes Claude's next read ask for the login Keychain password.

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;

const SECURITY: &str = "/usr/bin/security";
const SERVICE: &str = "Claude Code-credentials";

pub(super) enum Item {
    Document(Value),
    Missing,
    NeedsInteraction,
}

pub(super) fn read(account: &str) -> Result<Item> {
    let output = Command::new(SECURITY)
        .args(["find-generic-password", "-a", account, "-s", SERVICE, "-w"])
        .output()
        .context("could not run macOS security to read Claude's Keychain credential")?;
    match output.status.code() {
        Some(0) => parse_document(&output.stdout).map(Item::Document),
        // security returns the low byte of the OSStatus.
        Some(44) => Ok(Item::Missing),          // errSecItemNotFound
        Some(36) => Ok(Item::NeedsInteraction), // errSecInteractionNotAllowed
        _ => bail!(
            "could not read Claude's Keychain credential ({})",
            output.status
        ),
    }
}

fn parse_document(output: &[u8]) -> Result<Value> {
    let bytes = output.trim_ascii();
    // security -w hex-encodes the entire value if any byte is nonprintable
    // (including newlines and bytes in a UTF-8 string).
    if !bytes.is_empty() && bytes.iter().all(u8::is_ascii_hexdigit) {
        let hex = std::str::from_utf8(bytes)?;
        ensure!(
            hex.len().is_multiple_of(2),
            "incomplete hex in Claude's Keychain credential"
        );
        let decoded = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16))
            .collect::<Result<Vec<_>, _>>()?;
        return serde_json::from_slice(&decoded)
            .context("Claude Keychain credential is not valid JSON");
    }
    serde_json::from_slice(bytes).context("Claude Keychain credential is not valid JSON")
}

/// security -i has its own tokenizer. Inside single quotes it recognizes
/// backslash escapes; JSON serialization already escapes line breaks and NUL.
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

pub(super) fn write_command(account: &str, document: &Value) -> Result<String> {
    ensure!(
        !account.is_empty() && !account.contains(['\n', '\r', '\0']),
        "invalid macOS Keychain account"
    );
    let account = quote(account);
    let data = quote(&serde_json::to_string(document)?);
    let command = format!("add-generic-password -U -a {account} -s '{SERVICE}' -w {data}\n");
    // The interactive reader has a 4096-byte buffer including its trailing
    // NUL. Leave room for the newline so the entire update is one command.
    ensure!(
        command.len() < 4096,
        "Claude's Keychain credential is too large for a private update through macOS security"
    );
    Ok(command)
}

pub(super) fn write(account: &str, document: &Value) -> Result<()> {
    let command = write_command(account, document)?;
    let mut child = Command::new(SECURITY)
        .arg("-i")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("could not run macOS security to update Claude's Keychain credential")?;
    let sent = child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(command.as_bytes());
    let status = child.wait().context("could not wait for macOS security")?;
    sent.context("could not send Claude's Keychain credential to macOS security")?;
    ensure!(
        status.success(),
        "could not update Claude's Keychain credential ({status})"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_plain_and_hex_encoded_json() {
        let doc = json!({"label": "café"});
        let hex: String = doc
            .to_string()
            .bytes()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(parse_document(hex.as_bytes()).unwrap(), doc);
        assert_eq!(
            parse_document(b"{\"mcpOAuth\":{}}\n").unwrap(),
            json!({"mcpOAuth":{}})
        );
        assert!(parse_document(b"7b0").is_err());
    }

    #[test]
    fn writes_one_complete_command_or_refuses_before_writing() {
        let value = json!({"quote": "it's a \"test\"\n"});
        let command = write_command("O'Brien", &value).unwrap();
        assert_eq!(command.lines().count(), 1);
        assert!(command.contains(r#"-a 'O\'Brien'"#));
        assert!(command.contains(r#"it\'s a \\"test\\"\\n"#));
        assert!(write_command("bad\naccount", &value).is_err());
        let overhead = write_command("ember", &json!({"x": ""})).unwrap().len();
        assert!(write_command("ember", &json!({"x": "x".repeat(4095 - overhead)})).is_ok());
        assert!(write_command("ember", &json!({"x": "x".repeat(4096 - overhead)})).is_err());
    }
}
