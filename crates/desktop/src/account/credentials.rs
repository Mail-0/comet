use anyhow::Result;
use async_trait::async_trait;
use keiki_api::StoredCredentials;

pub const KEY: &str = "keiki://oauth";

#[async_trait]
pub trait CredentialStore: Send + Sync {
    async fn load(&self) -> Result<Option<StoredCredentials>>;
    async fn save(&self, credentials: &StoredCredentials) -> Result<()>;
    async fn delete(&self) -> Result<()>;
}

pub struct SecureStore;

#[async_trait]
impl CredentialStore for SecureStore {
    async fn load(&self) -> Result<Option<StoredCredentials>> {
        let payload = platform::load().await?;
        payload
            .map(|bytes| serde_json::from_slice(&bytes).map_err(Into::into))
            .transpose()
    }

    async fn save(&self, credentials: &StoredCredentials) -> Result<()> {
        platform::save(serde_json::to_vec(credentials)?).await
    }

    async fn delete(&self) -> Result<()> {
        platform::delete().await
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use anyhow::{Context, ensure};
    use core_foundation::{
        base::{CFType, CFTypeRef, OSStatus, TCFType},
        boolean::CFBoolean,
        data::CFData,
        dictionary::{CFDictionary, CFDictionaryRef},
        string::{CFString, CFStringRef},
    };

    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        static kSecClass: CFStringRef;
        static kSecClassInternetPassword: CFStringRef;
        static kSecAttrServer: CFStringRef;
        static kSecAttrAccount: CFStringRef;
        static kSecValueData: CFStringRef;
        static kSecReturnData: CFStringRef;
        fn SecItemAdd(attributes: CFDictionaryRef, result: *mut CFTypeRef) -> OSStatus;
        fn SecItemUpdate(query: CFDictionaryRef, attributes: CFDictionaryRef) -> OSStatus;
        fn SecItemDelete(query: CFDictionaryRef) -> OSStatus;
        fn SecItemCopyMatching(query: CFDictionaryRef, result: *mut CFTypeRef) -> OSStatus;
    }
    const NOT_FOUND: OSStatus = -25300;

    unsafe fn scope() -> Vec<(CFString, CFType)> {
        vec![
            (
                unsafe { CFString::wrap_under_get_rule(kSecClass) },
                unsafe { CFString::wrap_under_get_rule(kSecClassInternetPassword) }.as_CFType(),
            ),
            (
                unsafe { CFString::wrap_under_get_rule(kSecAttrServer) },
                CFString::from(KEY).as_CFType(),
            ),
        ]
    }

    unsafe fn query() -> CFDictionary<CFString, CFType> {
        CFDictionary::from_CFType_pairs(&unsafe { scope() })
    }

    pub async fn load() -> Result<Option<Vec<u8>>> {
        tokio::task::spawn_blocking(|| unsafe {
            let mut pairs = scope();
            pairs.push((
                CFString::wrap_under_get_rule(kSecReturnData),
                CFBoolean::true_value().as_CFType(),
            ));
            let query = CFDictionary::from_CFType_pairs(&pairs);
            let mut result = std::ptr::null();
            let status = SecItemCopyMatching(query.as_concrete_TypeRef(), &mut result);
            if status == NOT_FOUND {
                return Ok(None);
            }
            ensure!(status == 0, "Keychain credential read failed ({status})");
            let data = CFType::wrap_under_create_rule(result)
                .downcast::<CFData>()
                .context("Keychain credential is not data")?;
            Ok(Some(data.bytes().to_vec()))
        })
        .await?
    }

    pub async fn save(payload: Vec<u8>) -> Result<()> {
        tokio::task::spawn_blocking(move || unsafe {
            let query = query();
            let attributes = vec![
                (
                    CFString::wrap_under_get_rule(kSecAttrAccount),
                    CFString::from("OAuth").as_CFType(),
                ),
                (
                    CFString::wrap_under_get_rule(kSecValueData),
                    CFData::from_buffer(&payload).as_CFType(),
                ),
            ];
            let mut status = SecItemUpdate(
                query.as_concrete_TypeRef(),
                CFDictionary::from_CFType_pairs(&attributes).as_concrete_TypeRef(),
            );
            if status == NOT_FOUND {
                let mut pairs = scope();
                pairs.extend(attributes);
                status = SecItemAdd(
                    CFDictionary::from_CFType_pairs(&pairs).as_concrete_TypeRef(),
                    std::ptr::null_mut(),
                );
            }
            ensure!(status == 0, "Keychain credential write failed ({status})");
            Ok(())
        })
        .await?
    }

    pub async fn delete() -> Result<()> {
        tokio::task::spawn_blocking(|| unsafe {
            let status = SecItemDelete(query().as_concrete_TypeRef());
            ensure!(
                status == 0 || status == NOT_FOUND,
                "Keychain credential deletion failed ({status})"
            );
            Ok(())
        })
        .await?
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    const LABEL: &str = "zed-github-account";

    pub async fn load() -> Result<Option<Vec<u8>>> {
        let keyring = oo7::Keyring::new().await?;
        keyring.unlock().await?;
        for item in keyring.search_items(&[("url", KEY)]).await? {
            if item.label().await? == LABEL {
                item.unlock().await?;
                return Ok(Some(item.secret().await?.to_vec()));
            }
        }
        Ok(None)
    }

    pub async fn save(payload: Vec<u8>) -> Result<()> {
        let keyring = oo7::Keyring::new().await?;
        keyring.unlock().await?;
        keyring
            .create_item(LABEL, &[("url", KEY), ("username", "OAuth")], payload, true)
            .await?;
        Ok(())
    }

    pub async fn delete() -> Result<()> {
        let keyring = oo7::Keyring::new().await?;
        keyring.unlock().await?;
        for item in keyring.search_items(&[("url", KEY)]).await? {
            if item.label().await? == LABEL {
                item.delete().await?;
            }
        }
        Ok(())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod platform {
    use super::*;
    pub async fn load() -> Result<Option<Vec<u8>>> {
        anyhow::bail!("Secure credentials unsupported on this platform")
    }
    pub async fn save(_: Vec<u8>) -> Result<()> {
        anyhow::bail!("Secure credentials unsupported on this platform")
    }
    pub async fn delete() -> Result<()> {
        anyhow::bail!("Secure credentials unsupported on this platform")
    }
}
