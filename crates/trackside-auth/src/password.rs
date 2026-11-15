//! Argon2id hashing and verification on the blocking pool, limited by a semaphore so a burst
//! of logins cannot take every CPU.

use std::sync::Arc;

use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use argon2::{Algorithm, Argon2, Params, Version};
use tokio::sync::{OnceCell, Semaphore};
use zeroize::Zeroizing;

use crate::AuthError;
use crate::token::fill_random;

/// Accepted login password lengths, in bytes: anything but empty, up to a cap that keeps
/// hashing cheap.
pub const PASSWORD_BYTES: std::ops::RangeInclusive<usize> = 1..=1024;

#[derive(Debug)]
pub(crate) struct Hasher {
    params: Params,
    permits: Arc<Semaphore>,
    /// A hash of a random password with the current parameters, verified against when the
    /// user does not exist so that both cases take the same time.
    dummy: OnceCell<String>,
}

impl Hasher {
    /// `memory_bytes` and `iterations` come from `auth.argon2_*`; out-of-range values fall
    /// back to the OWASP minimum (19 MiB, 2 passes, 1 lane).
    pub(crate) fn new(memory_bytes: u64, iterations: u32) -> Self {
        let kib = u32::try_from(memory_bytes / 1024).unwrap_or(u32::MAX);
        let params = Params::new(kib, iterations, 1, None).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "argon2 settings out of range; using 19 MiB and 2 passes");
            Params::DEFAULT
        });
        let lanes = std::thread::available_parallelism().map_or(2, std::num::NonZero::get);
        Self {
            params,
            permits: Arc::new(Semaphore::new((lanes / 2).clamp(1, 4))),
            dummy: OnceCell::new(),
        }
    }

    fn argon(params: Params) -> Argon2<'static> {
        Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
    }

    async fn run<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, AuthError> {
        let permit = Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .map_err(|_| AuthError::Io(std::io::Error::other("password hasher closed")))?;
        tokio::task::spawn_blocking(move || {
            let out = work();
            drop(permit);
            out
        })
        .await
        .map_err(|e| AuthError::Io(std::io::Error::other(format!("password hashing: {e}"))))
    }

    /// The PHC string for `password` with a fresh 16-byte salt.
    pub(crate) async fn hash(&self, password: Zeroizing<String>) -> Result<String, AuthError> {
        let mut salt = [0u8; 16];
        fill_random(&mut salt)?;
        let params = self.params.clone();
        self.run(move || {
            Self::argon(params)
                .hash_password_with_salt(password.as_bytes(), &salt)
                .map(|hash| hash.to_string())
                .map_err(|e| AuthError::Invalid(format!("cannot hash password: {e}")))
        })
        .await?
    }

    /// Whether `password` matches `phc`. With no hash, verifies against the dummy hash and
    /// answers false, taking as long as a real check.
    pub(crate) async fn verify(
        &self,
        password: Zeroizing<String>,
        phc: Option<String>,
    ) -> Result<bool, AuthError> {
        let real = phc.is_some();
        let phc = match phc {
            Some(phc) => phc,
            None => self.dummy().await?.clone(),
        };
        let params = self.params.clone();
        let matched = self
            .run(move || {
                match Self::argon(params).verify_password(password.as_bytes(), phc.as_str()) {
                    Ok(()) => true,
                    Err(argon2::password_hash::Error::PasswordInvalid) => false,
                    Err(e) => {
                        tracing::error!(error = %e, "stored password hash is unreadable");
                        false
                    }
                }
            })
            .await?;
        Ok(real && matched)
    }

    async fn dummy(&self) -> Result<&String, AuthError> {
        self.dummy
            .get_or_try_init(|| async {
                let mut random = Zeroizing::new([0u8; 24]);
                fill_random(random.as_mut_slice())?;
                self.hash(Zeroizing::new(crate::token::hex(random.as_slice())))
                    .await
            })
            .await
    }
}

/// Checks the length rules for a new password.
pub(crate) fn validate_new_password(password: &str) -> Result<(), AuthError> {
    if PASSWORD_BYTES.contains(&password.len()) {
        Ok(())
    } else {
        Err(AuthError::Invalid(format!(
            "passwords are {} to {} bytes long",
            PASSWORD_BYTES.start(),
            PASSWORD_BYTES.end()
        )))
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    fn small() -> Hasher {
        Hasher::new(1 << 20, 1)
    }

    #[tokio::test]
    async fn hash_then_verify_accepts_only_the_password() -> Result<(), AuthError> {
        let hasher = small();
        let phc = hasher
            .hash(Zeroizing::new(String::from("correct horse")))
            .await?;
        assert!(phc.starts_with("$argon2id$v=19$m=1024,t=1,p=1$"));
        assert!(
            hasher
                .verify(
                    Zeroizing::new(String::from("correct horse")),
                    Some(phc.clone())
                )
                .await?
        );
        assert!(
            !hasher
                .verify(Zeroizing::new(String::from("correct horsE")), Some(phc))
                .await?
        );
        Ok(())
    }

    #[test]
    fn default_settings_give_owasp_parameters() {
        let hasher = Hasher::new(19 << 20, 2);
        assert_eq!(
            (
                hasher.params.m_cost(),
                hasher.params.t_cost(),
                hasher.params.p_cost()
            ),
            (19_456, 2, 1)
        );
    }

    #[test]
    fn out_of_range_settings_fall_back_to_defaults() {
        let hasher = Hasher::new(1024, 0);
        assert_eq!(hasher.params.m_cost(), Params::DEFAULT_M_COST);
    }

    #[tokio::test]
    async fn verifying_without_a_hash_is_false() -> Result<(), AuthError> {
        assert!(
            !small()
                .verify(Zeroizing::new(String::from("anything")), None)
                .await?
        );
        Ok(())
    }

    #[tokio::test]
    async fn garbage_hash_is_a_mismatch_not_an_error() -> Result<(), AuthError> {
        assert!(
            !small()
                .verify(
                    Zeroizing::new(String::from("anything")),
                    Some(String::from("$md5$nope"))
                )
                .await?
        );
        Ok(())
    }

    #[tokio::test]
    async fn unknown_user_check_takes_as_long_as_a_wrong_password() -> Result<(), AuthError> {
        let hasher = Hasher::new(4 << 20, 2);
        let phc = hasher
            .hash(Zeroizing::new(String::from("the password")))
            .await?;
        hasher
            .verify(Zeroizing::new(String::from("warm up")), None)
            .await?;
        let mut known = Vec::new();
        let mut unknown = Vec::new();
        for _ in 0..7 {
            let start = Instant::now();
            hasher
                .verify(Zeroizing::new(String::from("wrong")), Some(phc.clone()))
                .await?;
            known.push(start.elapsed());
            let start = Instant::now();
            hasher
                .verify(Zeroizing::new(String::from("wrong")), None)
                .await?;
            unknown.push(start.elapsed());
        }
        known.sort();
        unknown.sort();
        let (known, unknown) = (known[3].as_secs_f64(), unknown[3].as_secs_f64());
        let ratio = known / unknown;
        assert!(
            (0.9..=1.1).contains(&ratio),
            "median wrong-password check {known:.4}s, unknown-user check {unknown:.4}s"
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hashing_does_not_block_the_runtime() -> Result<(), AuthError> {
        let hasher = Arc::new(Hasher::new(16 << 20, 3));
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let hashing = {
            let hasher = Arc::clone(&hasher);
            tokio::spawn(async move { hasher.hash(Zeroizing::new(String::from("pw"))).await })
        };
        tokio::spawn(async move {
            let _ = tx.send(Instant::now());
        });
        let started = Instant::now();
        let ticked = tokio::time::timeout(Duration::from_secs(5), &mut rx)
            .await
            .map_err(|_| AuthError::Unauthenticated)?
            .map_err(|_| AuthError::Unauthenticated)?;
        assert!(
            !hashing.is_finished() || ticked.duration_since(started) < Duration::from_millis(50)
        );
        hashing
            .await
            .map_err(|e| AuthError::Io(std::io::Error::other(e)))??;
        Ok(())
    }

    #[test]
    fn password_length_rules() {
        assert!(validate_new_password("").is_err());
        assert!(validate_new_password("1").is_ok());
        assert!(validate_new_password(&"x".repeat(1025)).is_err());
        assert!(validate_new_password(&"x".repeat(1025)).is_err());
    }
}
