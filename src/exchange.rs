use std::{fmt, str::FromStr};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExchangeKind {
    Lighter,
    Arcus,
    Aster,
    Hyperliquid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveVenue {
    LighterMainnet,
    LighterRobinhood,
    ArcusMainnet,
    ArcusTestnet,
    AsterMainnet,
    HyperliquidMainnet,
    HyperliquidTestnet,
}

impl LiveVenue {
    pub const ALL: [Self; 7] = [
        Self::LighterMainnet,
        Self::LighterRobinhood,
        Self::ArcusMainnet,
        Self::ArcusTestnet,
        Self::AsterMainnet,
        Self::HyperliquidMainnet,
        Self::HyperliquidTestnet,
    ];

    pub const fn exchange(self) -> ExchangeKind {
        match self {
            Self::LighterMainnet | Self::LighterRobinhood => ExchangeKind::Lighter,
            Self::ArcusMainnet | Self::ArcusTestnet => ExchangeKind::Arcus,
            Self::AsterMainnet => ExchangeKind::Aster,
            Self::HyperliquidMainnet | Self::HyperliquidTestnet => ExchangeKind::Hyperliquid,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LighterMainnet => "lighter-mainnet",
            Self::LighterRobinhood => "lighter-robinhood",
            Self::ArcusMainnet => "arcus-mainnet",
            Self::ArcusTestnet => "arcus-testnet",
            Self::AsterMainnet => "aster-mainnet",
            Self::HyperliquidMainnet => "hyperliquid-mainnet",
            Self::HyperliquidTestnet => "hyperliquid-testnet",
        }
    }

    pub const fn config_path(self) -> &'static str {
        match self {
            Self::LighterMainnet => "config/settings.yaml",
            Self::LighterRobinhood => "config/settings.robinhood.yaml",
            Self::ArcusMainnet => "config/settings.arcus.yaml",
            Self::ArcusTestnet => "config/settings.arcus-testnet.yaml",
            Self::AsterMainnet => "config/settings.aster.yaml",
            Self::HyperliquidMainnet => "config/settings.hyperliquid.yaml",
            Self::HyperliquidTestnet => "config/settings.hyperliquid-testnet.yaml",
        }
    }

    pub const fn rest_url(self) -> &'static str {
        match self {
            Self::LighterMainnet => "https://mainnet.zklighter.elliot.ai",
            Self::LighterRobinhood => "https://api.rh.lighter.xyz",
            Self::ArcusMainnet => crate::arcus::MAINNET_REST_URL,
            Self::ArcusTestnet => crate::arcus::TESTNET_REST_URL,
            Self::AsterMainnet => crate::aster::MAINNET_REST_URL,
            Self::HyperliquidMainnet => crate::hyperliquid::MAINNET_REST_URL,
            Self::HyperliquidTestnet => crate::hyperliquid::TESTNET_REST_URL,
        }
    }

    pub const fn websocket_url(self) -> &'static str {
        match self {
            Self::LighterMainnet => "wss://mainnet.zklighter.elliot.ai/stream",
            Self::LighterRobinhood => "wss://api.rh.lighter.xyz/stream",
            Self::ArcusMainnet => crate::arcus::MAINNET_WEBSOCKET_URL,
            Self::ArcusTestnet => crate::arcus::TESTNET_WEBSOCKET_URL,
            Self::AsterMainnet => crate::aster::MAINNET_WS_URL,
            Self::HyperliquidMainnet => crate::hyperliquid::MAINNET_WS_URL,
            Self::HyperliquidTestnet => crate::hyperliquid::TESTNET_WS_URL,
        }
    }

    pub const fn chain_id(self) -> Option<i64> {
        match self {
            Self::LighterMainnet => Some(304),
            Self::LighterRobinhood => Some(466_324),
            Self::ArcusMainnet
            | Self::ArcusTestnet
            | Self::AsterMainnet
            | Self::HyperliquidMainnet
            | Self::HyperliquidTestnet => None,
        }
    }

    pub fn credential_key(self, suffix: &str) -> String {
        let prefix = match self {
            Self::LighterMainnet => "LIGHTER_MAINNET",
            Self::LighterRobinhood => "LIGHTER_ROBINHOOD",
            Self::ArcusMainnet => "ARCUS_MAINNET",
            Self::ArcusTestnet => "ARCUS_TESTNET",
            Self::AsterMainnet => "ASTER_MAINNET",
            Self::HyperliquidMainnet => "HYPERLIQUID_MAINNET",
            Self::HyperliquidTestnet => "HYPERLIQUID_TESTNET",
        };
        format!("{prefix}_{suffix}")
    }
}

impl FromStr for LiveVenue {
    type Err = ParseLiveVenueError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "lighter-mainnet" | "mainnet" => Ok(Self::LighterMainnet),
            "lighter-robinhood" | "robinhood" => Ok(Self::LighterRobinhood),
            "arcus-mainnet" => Ok(Self::ArcusMainnet),
            "arcus-testnet" => Ok(Self::ArcusTestnet),
            "aster-mainnet" => Ok(Self::AsterMainnet),
            "hyperliquid-mainnet" => Ok(Self::HyperliquidMainnet),
            "hyperliquid-testnet" => Ok(Self::HyperliquidTestnet),
            _ => Err(ParseLiveVenueError),
        }
    }
}

impl fmt::Display for LiveVenue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseLiveVenueError;

impl fmt::Display for ParseLiveVenueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(
            "venue must be lighter-mainnet, lighter-robinhood, arcus-mainnet, arcus-testnet, aster-mainnet, hyperliquid-mainnet, or hyperliquid-testnet",
        )
    }
}

impl std::error::Error for ParseLiveVenueError {}
