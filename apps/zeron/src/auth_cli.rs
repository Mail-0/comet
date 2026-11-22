//! Keiki account commands. Credentials stay in the OS keyring.

use zeron_engine::{AuthState, Engine, EngineConfig, InstanceLock};

#[derive(Debug, PartialEq, Eq)]
struct AccountStatus {
    auth: String,
    healthy: bool,
}

fn account_status(auth: &AuthState) -> AccountStatus {
    match auth {
        AuthState::SignedOut => AccountStatus {
            auth: "signed out — run `zeron login`".into(),
            healthy: false,
        },
        AuthState::NeedsOrganization { user } => AccountStatus {
            auth: format!(
                "signed in as {}; select a Keiki organization with `zeron login`",
                user.email
            ),
            healthy: false,
        },
        AuthState::SignedIn { user, org_id } => AccountStatus {
            auth: format!(
                "signed in as {}{}",
                user.email,
                org_id
                    .as_ref()
                    .map(|org| format!(" (organization {org})"))
                    .unwrap_or_default()
            ),
            healthy: true,
        },
    }
}

pub async fn login(config: EngineConfig) -> anyhow::Result<()> {
    std::fs::create_dir_all(&config.data_dir)?;
    let _lock = engine_lock(&config, "sign in")?;
    let auth = Engine::build_auth(&config).await;
    let _ = auth.access_token().await;
    if let AuthState::SignedIn { user, .. } = auth.state() {
        println!(
            "Already signed in to Keiki as {}. Run `zeron logout` to switch accounts.",
            user.email
        );
        return Ok(());
    }
    zeron_engine::terminal_sign_in(&auth).await?;
    println!("{}", account_status(&auth.state()).auth);
    Ok(())
}

pub async fn logout(config: EngineConfig) -> anyhow::Result<()> {
    std::fs::create_dir_all(&config.data_dir)?;
    let _lock = engine_lock(&config, "sign out")?;
    let auth = Engine::build_auth(&config).await;
    auth.sign_out().await?;
    println!(
        "Signed out of Keiki. Credentials were removed from the OS keyring; local data is unchanged."
    );
    Ok(())
}

pub async fn status(config: EngineConfig) -> anyhow::Result<()> {
    let auth = Engine::build_auth(&config).await;
    if let Err(zeron_rpc::TokenError::TemporarilyUnavailable(error)) = auth.access_token().await {
        println!("Keiki could not validate the saved credentials: {error}");
    }
    let account = account_status(&auth.state());
    println!("Data dir: {}", config.data_dir.display());
    println!("Keiki:    {}", auth.api_url());
    println!("Mode:     local only");
    println!("Auth:     {}", account.auth);
    match InstanceLock::holder(&config.data_dir) {
        Some(pid) => println!("Engine:   running (pid {pid})"),
        None => println!("Engine:   not running"),
    }
    if !account.healthy {
        anyhow::bail!("Keiki account setup is incomplete");
    }
    Ok(())
}

fn engine_lock(config: &EngineConfig, verb: &str) -> anyhow::Result<InstanceLock> {
    InstanceLock::acquire(&config.data_dir).map_err(|error| anyhow::anyhow!(
        "{error}\nCannot {verb} while an engine is running — use its account settings, or stop the engine first."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_engine::AuthUser;

    fn user() -> AuthUser {
        AuthUser {
            id: "user@example.test".into(),
            email: "user@example.test".into(),
            name: None,
        }
    }

    #[test]
    fn a_signed_out_account_needs_login() {
        let status = account_status(&AuthState::SignedOut);
        assert!(!status.healthy);
        assert!(status.auth.contains("zeron login"));
    }

    #[test]
    fn an_account_without_an_organization_needs_selection() {
        let status = account_status(&AuthState::NeedsOrganization { user: user() });
        assert!(!status.healthy);
        assert!(status.auth.contains("select a Keiki organization"));
    }

    #[test]
    fn a_signed_in_account_reports_its_organization() {
        let status = account_status(&AuthState::SignedIn {
            user: user(),
            org_id: Some("org-1".into()),
        });
        assert!(status.healthy);
        assert!(status.auth.contains("organization org-1"));
    }
}
