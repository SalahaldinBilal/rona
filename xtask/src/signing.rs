use std::io::Cursor;
use std::path::Path;

use minisign::{KeyPair, SecretKeyBox};

use crate::write;

pub const PUBLIC_KEY_PATH: &str = "assets/update-key.pub";

pub fn keygen(root: &Path, password: &str) -> Result<(), String> {
    if password.is_empty() {
        return Err("the password can't be empty".into());
    }
    let (public, secret) = generate(password.to_string())?;
    write(&root.join(PUBLIC_KEY_PATH), &public)?;
    println!("Wrote the public key to {PUBLIC_KEY_PATH}, commit it.\n");
    println!("Add these as repository secrets for the release workflow:");
    println!("  RONA_SIGN_PASSWORD: the password you used");
    println!("  RONA_SIGN_KEY: everything between the lines below\n");
    println!("-----\n{secret}-----");
    println!("\nKeep the secret key somewhere safe too. Without it, installed copies can't be updated.");
    Ok(())
}

/// The public and the password-encrypted secret key, both in minisign's text format.
fn generate(password: String) -> Result<(String, String), String> {
    let pair = KeyPair::generate_encrypted_keypair(Some(password)).map_err(|err| err.to_string())?;
    let public = pair.pk.to_box().map_err(|err| err.to_string())?.to_string();
    let secret = pair.sk.to_box(None).map_err(|err| err.to_string())?.to_string();
    Ok((public, secret))
}

/// A minisign signature of `data`, in the text format the app verifies.
pub fn sign(data: &[u8], secret_key: &str, password: &str) -> Result<String, String> {
    let secret = SecretKeyBox::from_string(secret_key)
        .and_then(|key| key.into_secret_key(Some(password.to_string())))
        .map_err(|err| format!("could not read the signing key: {err}"))?;
    let signature = minisign::sign(None, &secret, Cursor::new(data), None, None).map_err(|err| err.to_string())?;
    Ok(signature.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // The app verifies with minisign-verify, so a signature from the minisign crate has to pass there.
    #[test]
    fn signatures_verify_with_the_apps_verifier() {
        let (public, secret) = generate("test password".into()).unwrap();
        let data = b"installer bytes".to_vec();
        let signature = sign(&data, &secret, "test password").unwrap();

        let key = minisign_verify::PublicKey::decode(&public).unwrap();
        let decoded = minisign_verify::Signature::decode(&signature).unwrap();
        assert!(key.verify(&data, &decoded, false).is_ok());

        let mut tampered = data.clone();
        tampered[0] ^= 1;
        assert!(key.verify(&tampered, &decoded, false).is_err());
    }

    #[test]
    fn a_wrong_password_is_refused() {
        let (_, secret) = generate("right".into()).unwrap();
        assert!(sign(b"data", &secret, "wrong").is_err());
    }
}
