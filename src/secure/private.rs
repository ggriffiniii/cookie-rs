use secure::ring::aead::{seal_in_place, open_in_place, Algorithm, AES_256_GCM};
use secure::ring::aead::{OpeningKey, SealingKey};
use secure::ring::rand::SystemRandom;

use secure::rustc_serialize::base64::{ToBase64, FromBase64, STANDARD};

use {Cookie, CookieJar};

// Keep these in sync, and keep the key len synced with the `private` docs.
static ALGO: &'static Algorithm = &AES_256_GCM;
const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;

/// Extends `CookieJar` with a `private` method to retrieve a private child jar.
pub trait Private<'a, 'k> {
    /// Returns a `PrivateJar` with `self` as its parent jar using the key `key`
    /// to sign/encrypt and verify/decrypt cookies added/retrieved from the
    /// child jar. The key must be exactly 32 bytes. For security, the key
    /// _must_ be cryptographically random.
    ///
    /// Any modifications to the child jar will be reflected on the parent jar,
    /// and any retrievals from the child jar will be made from the parent jar.
    ///
    /// This trait is only available when the `secure` feature is enabled.
    ///
    /// # Panics
    ///
    /// Panics if `key` is not exactly 32 bytes long.
    ///
    /// # Example
    ///
    /// ```rust
    /// use cookie::{Cookie, CookieJar, Private};
    ///
    /// // We use a bogus key for demonstration purposes.
    /// let key: Vec<_> = (0..32).collect();
    ///
    /// // Add a private (signed + encrypted) cookie.
    /// let mut jar = CookieJar::new();
    /// jar.private(&key).add(Cookie::new("private", "text"));
    ///
    /// // The cookie's contents are encrypted.
    /// assert_ne!(jar.get("private").unwrap().value(), "text");
    ///
    /// // They can be decrypted and verified through the child jar.
    /// assert_eq!(jar.private(&key).get("private").unwrap().value(), "text");
    ///
    /// // A tampered with cookie does not validate but still exists.
    /// let mut cookie = jar.get("private").unwrap().clone();
    /// jar.add(Cookie::new("private", cookie.value().to_string() + "!"));
    /// assert!(jar.private(&key).get("private").is_none());
    /// assert!(jar.get("private").is_some());
    /// ```
    fn private(&'a mut self, &'k [u8]) -> PrivateJar<'a, 'k>;
}

impl<'a, 'k> Private<'a, 'k> for CookieJar {
    fn private(&'a mut self, key: &'k [u8]) -> PrivateJar<'a, 'k> {
        if key.len() != KEY_LEN {
            panic!("bad key length: expected {} bytes, found {}", KEY_LEN, key.len());
        }

        PrivateJar { parent: self, key: key }
    }
}

/// A child cookie jar that provides authenticated encryption for its cookies.
///
/// A _private_ child jar signs and encrypts all the cookies added to it and
/// verifies and decrypts cookies retrieved from it. Any cookies stored in a
/// `PrivateJar` are simultaneously assured confidentiality, integrity, and
/// authenticity. In other words, clients cannot discover nor tamper with the
/// contents of a cookie, nor can they fabricate cookie data.
///
/// This type is only available when the `secure` feature is enabled.
pub struct PrivateJar<'a, 'k> {
    parent: &'a mut CookieJar,
    key: &'k [u8]
}

impl<'a, 'k> PrivateJar<'a, 'k> {
    /// Given a sealed value `str` where the nonce is prepended to the original
    /// value and then both are Base64 encoded, verifies and decrypts the sealed
    /// value and returns it. If there's a problem, returns an `Err` with a
    /// string describing the issue.
    fn unseal(&self, value: &str) -> Result<String, &'static str> {
        let mut data = value.from_base64().map_err(|_| "bad base64 value")?;
        if data.len() <= NONCE_LEN {
            return Err("length of decoded data is <= NONCE_LEN");
        }

        let key = OpeningKey::new(ALGO, self.key).expect("opening key");
        let (nonce, sealed) = data.split_at_mut(NONCE_LEN);
        let out_len = open_in_place(&key, nonce, 0, sealed, &[])
            .map_err(|_| "invalid key/nonce/value: bad seal")?;

        ::std::str::from_utf8(&sealed[..out_len])
            .map(|s| s.to_string())
            .map_err(|_| "bad unsealed utf8")
    }

    /// Returns a reference to the `Cookie` inside this jar with the name `name`
    /// and authenticates and decrypts the cookie's value, returning a `Cookie`
    /// with the decrypted value. If the cookie cannot be found, or the cookie
    /// fails to authenticate or decrypt, `None` is returned.
    ///
    /// # Example
    ///
    /// ```rust
    /// use cookie::{CookieJar, Cookie, Private};
    ///
    /// # let key: Vec<_> = (0..32).collect();
    /// let mut jar = CookieJar::new();
    /// let mut private_jar = jar.private(&key);
    /// assert!(private_jar.get("name").is_none());
    ///
    /// private_jar.add(Cookie::new("name", "value"));
    /// assert_eq!(private_jar.get("name").unwrap().value(), "value");
    /// ```
    pub fn get(&self, name: &str) -> Option<Cookie<'static>> {
        if let Some(cookie_ref) = self.parent.get(name) {
            let mut cookie = cookie_ref.clone();
            if let Ok(value) = self.unseal(cookie.value()) {
                cookie.set_value(value);
                return Some(cookie);
            }
        }

        None
    }

    /// Adds `cookie` to the parent jar. The cookie's value is encrypted with
    /// authenticated encryption assuring confidentiality, integrity, and
    /// authenticity.
    ///
    /// # Example
    ///
    /// ```rust
    /// use cookie::{CookieJar, Cookie, Private};
    ///
    /// # let key: Vec<_> = (0..32).collect();
    /// let mut jar = CookieJar::new();
    /// jar.private(&key).add(Cookie::new("name", "value"));
    ///
    /// assert_ne!(jar.get("name").unwrap().value(), "value");
    /// assert_eq!(jar.private(&key).get("name").unwrap().value(), "value");
    /// ```
    pub fn add(&mut self, mut cookie: Cookie<'static>) {
        let mut data;
        let output_len = {
            // Create the `SealingKey` structure.
            let key = SealingKey::new(ALGO, self.key).expect("sealing key creation");

            // Create a vec to hold the [nonce | cookie value | overhead].
            let overhead = ALGO.max_overhead_len();
            let cookie_val = cookie.value().as_bytes();
            data = vec![0; NONCE_LEN + cookie_val.len() + overhead];

            // Randomly generate the nonce, then copy the cookie value as input.
            let (nonce, in_out) = data.split_at_mut(NONCE_LEN);
            SystemRandom::new().fill(nonce).expect("couldn't random fill nonce");
            in_out[..cookie_val.len()].copy_from_slice(cookie_val);

            // Perform the actual sealing operation and get the output length.
            seal_in_place(&key, nonce, in_out, overhead, &[]).expect("in-place seal")
        };

        // Base64 encode the nonce and encrypted value.
        let sealed_value = data[..(NONCE_LEN + output_len)].to_base64(STANDARD);
        cookie.set_value(sealed_value);

        // Add the sealed cookie to the parent.
        self.parent.add(cookie);
    }

    /// Removes `cookie` from the parent jar.
    ///
    /// For correct removal, the passed in `cookie` must contain the same `path`
    /// and `domain` as the cookie that was initially set.
    ///
    /// See [CookieJar::remove](struct.CookieJar.html#method.remove) for more
    /// details.
    ///
    /// # Example
    ///
    /// ```rust
    /// use cookie::{CookieJar, Cookie, Private};
    ///
    /// # let key: Vec<_> = (0..32).collect();
    /// let mut jar = CookieJar::new();
    /// let mut private_jar = jar.private(&key);
    ///
    /// private_jar.add(Cookie::new("name", "value"));
    /// assert!(private_jar.get("name").is_some());
    ///
    /// private_jar.remove(Cookie::named("name"));
    /// assert!(private_jar.get("name").is_none());
    /// ```
    pub fn remove(&mut self, cookie: Cookie<'static>) {
        self.parent.remove(cookie);
    }
}

#[cfg(test)]
mod test {
    use super::Private;
    use {CookieJar, Cookie};

    #[test]
    fn simple() {
        let key: Vec<u8> = (0..super::KEY_LEN as u8).collect();
        let mut jar = CookieJar::new();
        assert_simple_behaviour!(jar, jar.private(&key));
    }

    #[test]
    fn private() {
        let key: Vec<u8> = (0..super::KEY_LEN as u8).collect();
        let mut jar = CookieJar::new();
        assert_secure_behaviour!(jar, jar.private(&key));
    }
}
