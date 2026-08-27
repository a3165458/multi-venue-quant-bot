use anyhow::{bail, Context, Result};
use multi_venue_quant_bot::exchange::LiveVenue;
use std::path::{Path, PathBuf};
use std::str::FromStr;

const LEGACY_CREDENTIAL_KEYS: [&str; 3] = [
    "LIGHTER_SECRET_KEY",
    "LIGHTER_ACCOUNT_INDEX",
    "LIGHTER_API_KEY_INDEX",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CredentialProfile {
    Mainnet,
    Robinhood,
}

impl CredentialProfile {
    pub(crate) fn network_name(self) -> &'static str {
        match self {
            Self::Mainnet => "mainnet",
            Self::Robinhood => "robinhood",
        }
    }

    pub(crate) fn env_key(self, suffix: &str) -> String {
        format!(
            "LIGHTER_{}_{}",
            self.network_name().to_ascii_uppercase(),
            suffix
        )
    }

    pub(crate) fn env_path(self) -> PathBuf {
        PathBuf::from(".env")
    }
}

pub(crate) struct Credentials {
    pub(crate) secret_key: String,
    pub(crate) account_index: String,
    pub(crate) api_key_index: String,
}

pub(crate) struct ArcusCredentials {
    pub(crate) api_key: String,
    pub(crate) signing_key: String,
    pub(crate) address: String,
    pub(crate) account_index: u8,
}

#[allow(dead_code)]
pub(crate) struct AsterCredentials {
    pub(crate) signer_address: String,
    pub(crate) signer_private_key: String,
}

#[allow(dead_code)]
pub(crate) struct HyperliquidCredentials {
    /// Master account queried for balances, orders, and fills.
    pub(crate) account_address: String,
    /// Private key of the account or of an approved API/agent wallet.
    pub(crate) signer_private_key: String,
}

pub(crate) fn profile_for_chain_id(chain_id: i64) -> Result<CredentialProfile> {
    match chain_id {
        304 => Ok(CredentialProfile::Mainnet),
        466324 => Ok(CredentialProfile::Robinhood),
        other => bail!("unsupported Lighter chain_id {other}; cannot select a credential profile"),
    }
}

#[allow(dead_code)]
pub(crate) fn profile_for_network(network: &str) -> Result<CredentialProfile> {
    match network {
        "mainnet" => Ok(CredentialProfile::Mainnet),
        "robinhood" => Ok(CredentialProfile::Robinhood),
        other => bail!("unsupported Lighter network {other}"),
    }
}

#[cfg(test)]
pub(crate) fn credential_env_path(chain_id: i64) -> PathBuf {
    profile_for_chain_id(chain_id)
        .map(CredentialProfile::env_path)
        .unwrap_or_else(|_| PathBuf::from(".env.unsupported"))
}

#[allow(dead_code)]
pub(crate) fn credential_env_path_for_network(network: &str) -> Result<PathBuf> {
    Ok(profile_for_network(network)?.env_path())
}

pub(crate) fn dotenv_value(contents: &str, key: &str) -> Option<String> {
    contents.lines().find_map(|line| {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            return None;
        }
        let (candidate, value) = trimmed.split_once('=')?;
        (candidate.trim() == key).then(|| value.trim().trim_matches(['\'', '"']).to_string())
    })
}

pub(crate) fn read_env_value(path: impl AsRef<Path>, key: &str) -> Option<String> {
    let contents = std::fs::read_to_string(path).ok()?;
    dotenv_value(&contents, key)
}

pub(crate) fn selected_network() -> String {
    selected_venue().as_str().to_string()
}

pub(crate) fn selected_venue() -> LiveVenue {
    std::env::var("TRADING_VENUE")
        .ok()
        .or_else(|| read_env_value(".env", "TRADING_VENUE"))
        .and_then(|value| LiveVenue::from_str(&value).ok())
        .or_else(|| {
            std::env::var("LIGHTER_NETWORK")
                .ok()
                .or_else(|| read_env_value(".env", "LIGHTER_NETWORK"))
                .and_then(|value| LiveVenue::from_str(&value).ok())
        })
        .unwrap_or(LiveVenue::LighterMainnet)
}

/// Load non-credential settings from `.env`. Credential-looking keys are deliberately
/// skipped so a legacy `.env` can never override a network-specific profile.
pub(crate) fn load_shared_env() -> Result<()> {
    let Ok(contents) = std::fs::read_to_string(".env") else {
        return Ok(());
    };
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, raw_value)) = trimmed.split_once('=') else {
            bail!("invalid entry in .env: expected KEY=VALUE");
        };
        let key = key.trim().trim_start_matches("export ").trim().to_string();
        let value = raw_value.trim().trim_matches(['\'', '"']).to_string();
        if !LEGACY_CREDENTIAL_KEYS.contains(&key.as_str()) && std::env::var_os(&key).is_none() {
            std::env::set_var(key, value);
        }
    }
    Ok(())
}

pub(crate) fn load_credentials(profile: CredentialProfile) -> Result<(Credentials, PathBuf)> {
    let path = profile.env_path();
    let contents = std::fs::read_to_string(&path).unwrap_or_default();
    let value = |suffix: &str| -> Result<String> {
        let key = profile.env_key(suffix);
        dotenv_value(&contents, &key)
            .or_else(|| std::env::var(&key).ok())
            .filter(|value| !value.trim().is_empty())
            .with_context(|| format!("{key} is empty or missing in {}", path.display()))
    };
    Ok((
        Credentials {
            secret_key: value("SECRET_KEY")?,
            account_index: value("ACCOUNT_INDEX")?,
            api_key_index: value("API_KEY_INDEX")?,
        },
        path,
    ))
}

pub(crate) fn load_arcus_credentials(venue: LiveVenue) -> Result<(ArcusCredentials, PathBuf)> {
    if venue.exchange() != multi_venue_quant_bot::exchange::ExchangeKind::Arcus {
        bail!("{venue} is not an Arcus venue");
    }
    let path = PathBuf::from(".env");
    let contents = std::fs::read_to_string(&path).unwrap_or_default();
    let value = |suffix: &str| -> Result<String> {
        let key = venue.credential_key(suffix);
        dotenv_value(&contents, &key)
            .or_else(|| std::env::var(&key).ok())
            .filter(|value| !value.trim().is_empty())
            .with_context(|| format!("{key} is empty or missing in {}", path.display()))
    };
    let account_index = value("ACCOUNT_INDEX")?
        .parse::<u8>()
        .context("Arcus account index must be an integer from 0 to 9")?;
    if account_index > 9 {
        bail!("Arcus account index must be from 0 to 9");
    }
    Ok((
        ArcusCredentials {
            api_key: value("API_KEY")?,
            signing_key: value("SIGNING_KEY")?,
            address: value("ADDRESS")?,
            account_index,
        },
        path,
    ))
}

#[allow(dead_code)]
pub(crate) fn load_aster_credentials(venue: LiveVenue) -> Result<(AsterCredentials, PathBuf)> {
    let path = PathBuf::from(".env");
    let contents = std::fs::read_to_string(&path).unwrap_or_default();
    let value = |suffix: &str| -> Result<String> {
        let key = venue.credential_key(suffix);
        dotenv_value(&contents, &key)
            .or_else(|| std::env::var(&key).ok())
            .filter(|value| !value.trim().is_empty())
            .with_context(|| format!("{key} is empty or missing in {}", path.display()))
    };
    Ok((build_aster_credentials(venue, value)?, path))
}

fn build_aster_credentials(
    venue: LiveVenue,
    mut value: impl FnMut(&str) -> Result<String>,
) -> Result<AsterCredentials> {
    if venue != LiveVenue::AsterMainnet {
        bail!("{venue} is not an Aster venue");
    }
    Ok(AsterCredentials {
        signer_address: value("SIGNER_ADDRESS")?,
        signer_private_key: value("SIGNER_PRIVATE_KEY")?,
    })
}

#[allow(dead_code)]
pub(crate) fn load_hyperliquid_credentials(
    venue: LiveVenue,
) -> Result<(HyperliquidCredentials, PathBuf)> {
    let path = PathBuf::from(".env");
    let contents = std::fs::read_to_string(&path).unwrap_or_default();
    let value = |suffix: &str| -> Result<String> {
        let key = venue.credential_key(suffix);
        dotenv_value(&contents, &key)
            .or_else(|| std::env::var(&key).ok())
            .filter(|value| !value.trim().is_empty())
            .with_context(|| format!("{key} is empty or missing in {}", path.display()))
    };
    Ok((build_hyperliquid_credentials(venue, value)?, path))
}

fn build_hyperliquid_credentials(
    venue: LiveVenue,
    mut value: impl FnMut(&str) -> Result<String>,
) -> Result<HyperliquidCredentials> {
    if venue.exchange() != multi_venue_quant_bot::exchange::ExchangeKind::Hyperliquid {
        bail!("{venue} is not a Hyperliquid venue");
    }
    Ok(HyperliquidCredentials {
        account_address: value("ACCOUNT_ADDRESS")?,
        signer_private_key: value("SIGNER_PRIVATE_KEY")?,
    })
}

#[cfg(test)]
fn hyperliquid_credentials_from_contents(
    venue: LiveVenue,
    contents: &str,
) -> Result<HyperliquidCredentials> {
    let value = |suffix: &str| -> Result<String> {
        let key = venue.credential_key(suffix);
        dotenv_value(contents, &key)
            .filter(|value| !value.trim().is_empty())
            .with_context(|| format!("{key} is empty or missing"))
    };
    build_hyperliquid_credentials(venue, value)
}

#[cfg(test)]
fn aster_credentials_from_contents(venue: LiveVenue, contents: &str) -> Result<AsterCredentials> {
    let value = |suffix: &str| -> Result<String> {
        let key = venue.credential_key(suffix);
        dotenv_value(contents, &key)
            .filter(|value| !value.trim().is_empty())
            .with_context(|| format!("{key} is empty or missing"))
    };
    build_aster_credentials(venue, value)
}

#[cfg(test)]
pub(crate) fn credential_env_template(label: &str) -> String {
    format!(
        "# {label}; Lighter API keys, not wallet L1 keys\n\
LIGHTER_NETWORK=mainnet\n\
LIGHTER_MAINNET_SECRET_KEY=\n\
LIGHTER_MAINNET_ACCOUNT_INDEX=\n\
LIGHTER_MAINNET_API_KEY_INDEX=\n\
LIGHTER_ROBINHOOD_SECRET_KEY=\n\
LIGHTER_ROBINHOOD_ACCOUNT_INDEX=\n\
LIGHTER_ROBINHOOD_API_KEY_INDEX=\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn chain_ids_map_to_namespaced_keys_in_one_env_file() {
        assert_eq!(credential_env_path(304), PathBuf::from(".env"));
        assert_eq!(credential_env_path(466324), PathBuf::from(".env"));
        assert_eq!(
            profile_for_chain_id(304).unwrap().env_key("SECRET_KEY"),
            "LIGHTER_MAINNET_SECRET_KEY"
        );
        assert_eq!(
            profile_for_chain_id(466324).unwrap().env_key("SECRET_KEY"),
            "LIGHTER_ROBINHOOD_SECRET_KEY"
        );
    }

    #[test]
    fn unknown_chain_ids_are_rejected() {
        let err = profile_for_chain_id(999).unwrap_err();
        assert!(err.to_string().contains("unsupported Lighter chain_id"));
    }

    #[test]
    fn network_names_map_to_the_shared_env_file() {
        assert_eq!(
            credential_env_path_for_network("mainnet").unwrap(),
            PathBuf::from(".env")
        );
        assert_eq!(
            credential_env_path_for_network("robinhood").unwrap(),
            PathBuf::from(".env")
        );
        assert!(credential_env_path_for_network("testnet").is_err());
    }

    #[test]
    fn dotenv_value_parser_ignores_comments_and_similar_keys() {
        let input =
            "# LIGHTER_NETWORK=mainnet\nLIGHTER_NETWORK = robinhood\nOTHER_LIGHTER_NETWORK=bad\n";
        assert_eq!(
            dotenv_value(input, "LIGHTER_NETWORK"),
            Some("robinhood".to_string())
        );
        assert_eq!(dotenv_value(input, "MISSING"), None);
    }

    #[test]
    fn profile_template_contains_both_namespaced_credentials() {
        let template = credential_env_template("Robinhood Chain");
        assert!(template.contains("LIGHTER_MAINNET_SECRET_KEY="));
        assert!(template.contains("LIGHTER_ROBINHOOD_SECRET_KEY="));
        assert!(template.contains("LIGHTER_NETWORK="));
    }

    #[test]
    fn aster_credentials_are_namespaced_and_fail_closed() {
        let contents = "ASTER_MAINNET_SIGNER_ADDRESS=0x1111111111111111111111111111111111111111\n\
ASTER_MAINNET_SIGNER_PRIVATE_KEY=0x2222222222222222222222222222222222222222222222222222222222222222\n";
        let credentials =
            aster_credentials_from_contents(LiveVenue::AsterMainnet, contents).unwrap();
        assert_eq!(
            credentials.signer_address,
            "0x1111111111111111111111111111111111111111"
        );
        let missing = contents.replace("ASTER_MAINNET_SIGNER_PRIVATE_KEY", "ASTER_OTHER_KEY");
        assert!(aster_credentials_from_contents(LiveVenue::AsterMainnet, &missing).is_err());
        assert!(aster_credentials_from_contents(LiveVenue::ArcusMainnet, contents).is_err());
    }

    #[test]
    fn hyperliquid_credentials_are_namespaced_and_fail_closed() {
        let contents = "HYPERLIQUID_MAINNET_ACCOUNT_ADDRESS=0x3333333333333333333333333333333333333333\n\
HYPERLIQUID_MAINNET_SIGNER_PRIVATE_KEY=0x4444444444444444444444444444444444444444444444444444444444444444\n";
        let credentials =
            hyperliquid_credentials_from_contents(LiveVenue::HyperliquidMainnet, contents).unwrap();
        assert_eq!(
            credentials.account_address,
            "0x3333333333333333333333333333333333333333"
        );
        // Testnet keys are a separate namespace: mainnet secrets never leak over.
        assert!(
            hyperliquid_credentials_from_contents(LiveVenue::HyperliquidTestnet, contents).is_err()
        );
        let missing = contents.replace(
            "HYPERLIQUID_MAINNET_SIGNER_PRIVATE_KEY",
            "HYPERLIQUID_OTHER",
        );
        assert!(
            hyperliquid_credentials_from_contents(LiveVenue::HyperliquidMainnet, &missing).is_err()
        );
        assert!(hyperliquid_credentials_from_contents(LiveVenue::AsterMainnet, contents).is_err());
    }
}
