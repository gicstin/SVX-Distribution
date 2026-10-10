use crate::error::{Error, Result};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub const CONTEXT: &[u8] = b"SVX-SIGNED-METADATA-V1\n";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SignatureEntry {
    pub keyid: String,
    pub sig: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Envelope {
    pub signed: Value,
    pub signatures: Vec<SignatureEntry>,
}

pub fn canonical(v: &Value) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    write_canonical(v, &mut out)?;
    Ok(out)
}

fn write_canonical(v: &Value, out: &mut Vec<u8>) -> Result<()> {
    match v {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
        Value::Number(n) => {
            if n.is_f64() {
                return Err(Error::Metadata("floating point numbers are not allowed in signed metadata".into()));
            }
            out.extend_from_slice(n.to_string().as_bytes());
        }
        Value::String(s) => out.extend_from_slice(serde_json::to_string(s).expect("string").as_bytes()),
        Value::Array(a) => {
            out.push(b'[');
            for (i, item) in a.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_canonical(item, out)?;
            }
            out.push(b']');
        }
        Value::Object(m) => {
            let sorted: BTreeMap<&String, &Value> = m.iter().collect();
            out.push(b'{');
            for (i, (k, item)) in sorted.into_iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                out.extend_from_slice(serde_json::to_string(k).expect("string").as_bytes());
                out.push(b':');
                write_canonical(item, out)?;
            }
            out.push(b'}');
        }
    }
    Ok(())
}

pub fn message(signed: &Value) -> Result<Vec<u8>> {
    let mut m = CONTEXT.to_vec();
    m.extend(canonical(signed)?);
    Ok(m)
}

pub fn key_id(key: &VerifyingKey) -> String {
    hex::encode(Sha256::digest(key.as_bytes()))
}

pub fn parse_public(hex_key: &str) -> Result<VerifyingKey> {
    let bytes = hex::decode(hex_key).map_err(|_| Error::Metadata("public key is not hex".into()))?;
    let arr: [u8; 32] = bytes.try_into().map_err(|_| Error::Metadata("public key must be 32 bytes".into()))?;
    VerifyingKey::from_bytes(&arr).map_err(|_| Error::Metadata("invalid ed25519 public key".into()))
}

pub fn parse_secret(hex_seed: &str) -> Result<SigningKey> {
    let bytes = hex::decode(hex_seed.trim()).map_err(|_| Error::Metadata("secret key is not hex".into()))?;
    let arr: [u8; 32] = bytes.try_into().map_err(|_| Error::Metadata("secret key must be 32 bytes".into()))?;
    Ok(SigningKey::from_bytes(&arr))
}

pub fn generate_secret() -> SigningKey {
    let mut seed = [0u8; 32];
    getrandom::getrandom(&mut seed).expect("system random source");
    SigningKey::from_bytes(&seed)
}

pub fn sign_value(signed: &Value, keys: &[&SigningKey]) -> Result<Envelope> {
    let msg = message(signed)?;
    let mut signatures = Vec::new();
    let mut seen = BTreeSet::new();
    for k in keys {
        let id = key_id(&k.verifying_key());
        if seen.insert(id.clone()) {
            signatures.push(SignatureEntry { keyid: id, sig: B64.encode(k.sign(&msg).to_bytes()) });
        }
    }
    Ok(Envelope { signed: signed.clone(), signatures })
}

pub fn envelope_bytes(env: &Envelope) -> Vec<u8> {
    let mut v = serde_json::to_vec_pretty(env).expect("serialize envelope");
    v.push(b'\n');
    v
}

pub fn parse_envelope(raw: &[u8]) -> Result<Envelope> {
    serde_json::from_slice(raw).map_err(|e| Error::Metadata(format!("not a signed metadata file: {e}")))
}

pub fn count_valid(env: &Envelope, keys: &BTreeMap<String, VerifyingKey>, allowed: &[String]) -> Result<usize> {
    let msg = message(&env.signed)?;
    let mut ok = BTreeSet::new();
    for s in &env.signatures {
        if !allowed.contains(&s.keyid) || ok.contains(&s.keyid) {
            continue;
        }
        let Some(key) = keys.get(&s.keyid) else { continue };
        let Ok(raw) = B64.decode(&s.sig) else { continue };
        let Ok(arr) = <[u8; 64]>::try_from(raw.as_slice()) else { continue };
        if key.verify_strict(&msg, &Signature::from_bytes(&arr)).is_ok() {
            ok.insert(s.keyid.clone());
        }
    }
    Ok(ok.len())
}

pub fn verify_threshold(env: &Envelope, keys: &BTreeMap<String, VerifyingKey>, allowed: &[String], threshold: u32, what: &str) -> Result<()> {
    if threshold == 0 {
        return Err(Error::Signature(format!("{what}: threshold must be at least 1")));
    }
    let n = count_valid(env, keys, allowed)?;
    if n < threshold as usize {
        return Err(Error::Signature(format!("{what}: {n} valid signature(s), {threshold} required")));
    }
    Ok(())
}
