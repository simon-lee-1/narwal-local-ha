//! Qualified legacy AES/base64/NUL codec with strict JSON and bounded messages.
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};
use std::{ffi::c_void, fmt};
#[path = "public_key.rs"]
mod public_key;
pub(crate) const MESSAGE_CAP: usize = 8 * 1024 * 1024;
const CAP: usize = MESSAGE_CAP;
#[link(name = "libcrypto.so.3", kind = "dylib", modifiers = "+verbatim")]
unsafe extern "C" {
    fn EVP_CIPHER_CTX_new() -> *mut c_void;
    fn EVP_CIPHER_CTX_free(ctx: *mut c_void);
    fn EVP_aes_128_ecb() -> *const c_void;
    fn EVP_DecryptInit_ex(
        ctx: *mut c_void,
        cipher: *const c_void,
        engine: *mut c_void,
        key: *const u8,
        iv: *const u8,
    ) -> i32;
    fn EVP_CIPHER_CTX_set_padding(ctx: *mut c_void, padding: i32) -> i32;
    fn EVP_DecryptUpdate(
        ctx: *mut c_void,
        out: *mut u8,
        out_len: *mut i32,
        input: *const u8,
        len: i32,
    ) -> i32;
    fn EVP_DecryptFinal_ex(ctx: *mut c_void, out: *mut u8, out_len: *mut i32) -> i32;
}
struct Context(*mut c_void);
impl Drop for Context {
    fn drop(&mut self) {
        unsafe { EVP_CIPHER_CTX_free(self.0) }
    }
}
fn decrypt_with_key(cipher: &[u8], key: &[u8; 16]) -> Option<Vec<u8>> {
    if cipher.is_empty() || cipher.len() > CAP || !cipher.len().is_multiple_of(16) {
        return None;
    }
    unsafe {
        let ptr = EVP_CIPHER_CTX_new();
        if ptr.is_null() {
            return None;
        }
        let ctx = Context(ptr);
        let alg = EVP_aes_128_ecb();
        if alg.is_null()
            || EVP_DecryptInit_ex(
                ctx.0,
                alg,
                std::ptr::null_mut(),
                key.as_ptr(),
                std::ptr::null(),
            ) != 1
            || EVP_CIPHER_CTX_set_padding(ctx.0, 0) != 1
        {
            return None;
        }
        let mut out = vec![0u8; cipher.len() + 16];
        let (mut n, mut tail) = (0, 0);
        if EVP_DecryptUpdate(
            ctx.0,
            out.as_mut_ptr(),
            &mut n,
            cipher.as_ptr(),
            cipher.len() as i32,
        ) != 1
            || n < 0
            || n as usize > cipher.len()
        {
            return None;
        }
        if EVP_DecryptFinal_ex(ctx.0, out.as_mut_ptr().add(n as usize), &mut tail) != 1
            || tail < 0
            || (n as usize) + (tail as usize) != cipher.len()
        {
            return None;
        }
        out.truncate(cipher.len());
        Some(out)
    }
}
struct Strict(Value);
impl<'de> Deserialize<'de> for Strict {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Strict;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("strict JSON")
            }
            fn visit_unit<E: de::Error>(self) -> Result<Strict, E> {
                Ok(Strict(Value::Null))
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Strict, E> {
                Ok(Strict(Value::Bool(v)))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Strict, E> {
                Ok(Strict(Value::Number(v.into())))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Strict, E> {
                Ok(Strict(Value::Number(v.into())))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Strict, E> {
                Number::from_f64(v)
                    .map(|n| Strict(Value::Number(n)))
                    .ok_or_else(|| E::custom("invalid_number"))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Strict, E> {
                Ok(Strict(Value::String(v.into())))
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<Strict, E> {
                Ok(Strict(Value::String(v)))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Strict, A::Error> {
                let mut v = vec![];
                while let Some(Strict(x)) = a.next_element()? {
                    v.push(x)
                }
                Ok(Strict(Value::Array(v)))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Strict, A::Error> {
                let mut v = Map::new();
                while let Some(k) = a.next_key::<String>()? {
                    if v.contains_key(&k) {
                        return Err(de::Error::custom("duplicate_key"));
                    }
                    let Strict(x) = a.next_value()?;
                    v.insert(k, x);
                }
                Ok(Strict(Value::Object(v)))
            }
        }
        d.deserialize_any(V)
    }
}
fn parse(raw: &[u8]) -> Option<Value> {
    serde_json::from_slice::<Strict>(raw).ok().map(|s| s.0)
}

/// Decode a canonical encrypted object or a strict plain announcement object.
/// The wire-message budget includes base64 overhead; no session history is kept.
pub(crate) fn decode_object(raw: &[u8]) -> Option<Value> {
    if raw.is_empty() || raw.len() > MESSAGE_CAP {
        return None;
    }
    if (raw.first() == Some(&b'{') || raw.first().is_some_and(u8::is_ascii_whitespace))
        && let Some(value) = parse(raw).filter(Value::is_object)
    {
        return Some(value);
    }
    let cipher = STANDARD.decode(raw).ok()?;
    if STANDARD.encode(&cipher).as_bytes() != raw {
        return None;
    }
    let mut decoded = decrypt_with_key(&cipher, &public_key::KEY)?;
    let zeros = decoded.iter().rev().take_while(|b| **b == 0).count();
    if zeros > 15 {
        return None;
    }
    decoded.truncate(decoded.len() - zeros);
    parse(&decoded).filter(Value::is_object)
}
unsafe extern "C" {
    fn EVP_EncryptInit_ex(
        ctx: *mut c_void,
        cipher: *const c_void,
        engine: *mut c_void,
        key: *const u8,
        iv: *const u8,
    ) -> i32;
    fn EVP_EncryptUpdate(
        ctx: *mut c_void,
        out: *mut u8,
        out_len: *mut i32,
        input: *const u8,
        len: i32,
    ) -> i32;
    fn EVP_EncryptFinal_ex(ctx: *mut c_void, out: *mut u8, out_len: *mut i32) -> i32;
}
fn encrypt_with_key(raw: &[u8], key: &[u8; 16]) -> Option<Vec<u8>> {
    if raw.is_empty() || raw.len() > CAP || !raw.len().is_multiple_of(16) {
        return None;
    }
    unsafe {
        let ptr = EVP_CIPHER_CTX_new();
        if ptr.is_null() {
            return None;
        }
        let ctx = Context(ptr);
        let alg = EVP_aes_128_ecb();
        if alg.is_null()
            || EVP_EncryptInit_ex(
                ctx.0,
                alg,
                std::ptr::null_mut(),
                key.as_ptr(),
                std::ptr::null(),
            ) != 1
            || EVP_CIPHER_CTX_set_padding(ctx.0, 0) != 1
        {
            return None;
        }
        let mut out = vec![0; raw.len() + 16];
        let (mut n, mut tail) = (0, 0);
        if EVP_EncryptUpdate(
            ctx.0,
            out.as_mut_ptr(),
            &mut n,
            raw.as_ptr(),
            raw.len() as i32,
        ) != 1
            || n < 0
            || n as usize > raw.len()
        {
            return None;
        }
        if EVP_EncryptFinal_ex(ctx.0, out.as_mut_ptr().add(n as usize), &mut tail) != 1
            || tail < 0
            || n as usize + tail as usize != raw.len()
        {
            return None;
        }
        out.truncate(raw.len());
        Some(out)
    }
}
pub(crate) fn encode_object(v: &serde_json::Value) -> Option<Vec<u8>> {
    if !v.is_object() {
        return None;
    }
    let mut raw = serde_json::to_vec(v).ok()?;
    let pad = (16 - raw.len() % 16) % 16;
    if raw.len() + pad > CAP {
        return None;
    }
    raw.resize(raw.len() + pad, 0);
    let encoded = STANDARD
        .encode(encrypt_with_key(&raw, &public_key::KEY)?)
        .into_bytes();
    (encoded.len() <= MESSAGE_CAP).then_some(encoded)
}
