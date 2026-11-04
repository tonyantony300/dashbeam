#[cfg(test)]
thread_local! {
    static CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn record_call() {
    CALLS.with(|c| c.set(c.get() + 1));
}

/// Per-thread so parallel tests cannot disturb each other's counts.
#[cfg(test)]
pub fn reset_call_count_for_tests() {
    CALLS.with(|c| c.set(0));
}

#[cfg(test)]
pub fn call_count_for_tests() -> usize {
    CALLS.with(|c| c.get())
}

#[cfg(not(target_os = "android"))]
use anyhow::Context;

#[cfg(not(target_os = "android"))]
const SERVICE: &str = "alt-sendme";
#[cfg(not(target_os = "android"))]
const USER: &str = "iroh-secret";

/// Android has no real OS keychain backend in `keyring` (mock only). Identity
/// persistence uses `identity.key` exclusively — skip keychain I/O here.
#[cfg(target_os = "android")]
pub fn load_secret_hex() -> anyhow::Result<Option<String>> {
    #[cfg(test)]
    record_call();
    Ok(None)
}

#[cfg(not(target_os = "android"))]
pub fn load_secret_hex() -> anyhow::Result<Option<String>> {
    #[cfg(test)]
    record_call();
    let entry = keyring::Entry::new(SERVICE, USER).context("keyring entry")?;
    match entry.get_password() {
        Ok(value) => Ok(Some(value)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(err) => Err(err.into()),
    }
}

#[cfg(target_os = "android")]
pub fn save_secret_hex(_hex: &str) -> anyhow::Result<()> {
    #[cfg(test)]
    record_call();
    Ok(())
}

#[cfg(not(target_os = "android"))]
pub fn save_secret_hex(hex: &str) -> anyhow::Result<()> {
    #[cfg(test)]
    record_call();
    let entry = keyring::Entry::new(SERVICE, USER).context("keyring entry")?;
    entry
        .set_password(hex)
        .context("failed to save secret to keyring")
}

#[cfg(target_os = "android")]
pub fn delete_secret() -> anyhow::Result<()> {
    #[cfg(test)]
    record_call();
    Ok(())
}

#[cfg(not(target_os = "android"))]
pub fn delete_secret() -> anyhow::Result<()> {
    #[cfg(test)]
    record_call();
    let entry = keyring::Entry::new(SERVICE, USER).context("keyring entry")?;
    match entry.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(err) => Err(err.into()),
    }
}
