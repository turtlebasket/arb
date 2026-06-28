//! Chain + exchange selection shared across all CLI subcommands.

use std::fmt;

/// Supported chains. Tron is TVM (not EVM) — present for selection but its
/// streaming client is not implemented in this iteration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chain {
    Base,
    Bsc,
    Tron,
}

impl Chain {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "base" => Ok(Chain::Base),
            "bsc" | "bnb" => Ok(Chain::Bsc),
            "tron" => Ok(Chain::Tron),
            other => Err(format!("unknown chain '{other}' (expected base|bsc|tron)")),
        }
    }

    pub fn chain_id(&self) -> u64 {
        match self {
            Chain::Base => 8453,
            Chain::Bsc => 56,
            Chain::Tron => 728126428, // Tron mainnet id used by EVM-compat tooling
        }
    }

    /// Native gas token symbol.
    pub fn native_symbol(&self) -> &'static str {
        match self {
            Chain::Base => "ETH",
            Chain::Bsc => "BNB",
            Chain::Tron => "TRX",
        }
    }

    /// Env var the WS RPC URL is read from (user supplies private endpoints).
    pub fn rpc_env_var(&self) -> &'static str {
        match self {
            Chain::Base => "BASE_WSS_URL",
            Chain::Bsc => "BSC_WSS_URL",
            Chain::Tron => "TRON_WSS_URL",
        }
    }

    pub fn is_evm(&self) -> bool {
        !matches!(self, Chain::Tron)
    }
}

impl fmt::Display for Chain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Chain::Base => "base",
            Chain::Bsc => "bsc",
            Chain::Tron => "tron",
        };
        f.write_str(s)
    }
}

/// Selectable exchanges, each mapping to an AMM math family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exchange {
    UniswapV2,
    UniswapV3,
    PancakeV2,
    PancakeV3,
    Aerodrome,          // Solidly volatile + stable
    AerodromeSlipstream, // UniV3-style concentrated liquidity
    SunSwap,
    Curve,
    BalancerV2,
}

impl Exchange {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().replace(['-', '_'], "").as_str() {
            "uniswapv2" | "univ2" => Ok(Exchange::UniswapV2),
            "uniswapv3" | "univ3" => Ok(Exchange::UniswapV3),
            "pancakev2" | "pancakeswapv2" => Ok(Exchange::PancakeV2),
            "pancakev3" | "pancakeswapv3" => Ok(Exchange::PancakeV3),
            "aerodrome" | "aero" => Ok(Exchange::Aerodrome),
            "aerodromeslipstream" | "slipstream" => Ok(Exchange::AerodromeSlipstream),
            "sunswap" | "sun" => Ok(Exchange::SunSwap),
            "curve" => Ok(Exchange::Curve),
            "balancer" | "balancerv2" => Ok(Exchange::BalancerV2),
            other => Err(format!("unknown exchange '{other}'")),
        }
    }

    /// Whether this exchange is deployed on the given chain.
    pub fn supported_on(&self, chain: Chain) -> bool {
        match (self, chain) {
            (Exchange::UniswapV2 | Exchange::UniswapV3, Chain::Base) => true,
            (Exchange::Aerodrome | Exchange::AerodromeSlipstream, Chain::Base) => true,
            (Exchange::BalancerV2 | Exchange::Curve, Chain::Base) => true,
            (Exchange::PancakeV2 | Exchange::PancakeV3, Chain::Bsc) => true,
            (Exchange::UniswapV2 | Exchange::UniswapV3, Chain::Bsc) => true,
            (Exchange::Curve | Exchange::BalancerV2, Chain::Bsc) => true,
            (Exchange::SunSwap, Chain::Tron) => true,
            _ => false,
        }
    }
}

impl fmt::Display for Exchange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

/// A validated chain + exchange selection.
#[derive(Debug, Clone)]
pub struct Selection {
    pub chain: Chain,
    pub exchanges: Vec<Exchange>,
}

impl Selection {
    /// Build from raw CLI strings, validating exchange/chain compatibility.
    pub fn build(chain: &str, exchanges: &[String]) -> Result<Self, String> {
        let chain = Chain::parse(chain)?;
        if exchanges.is_empty() {
            return Err("at least one --exchange is required".to_string());
        }
        let mut parsed = Vec::with_capacity(exchanges.len());
        for ex in exchanges {
            let e = Exchange::parse(ex)?;
            if !e.supported_on(chain) {
                return Err(format!("{e} is not available on {chain}"));
            }
            parsed.push(e);
        }
        Ok(Selection {
            chain,
            exchanges: parsed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_chains() {
        assert_eq!(Chain::parse("base").unwrap(), Chain::Base);
        assert_eq!(Chain::parse("BSC").unwrap(), Chain::Bsc);
        assert_eq!(Chain::parse("bnb").unwrap(), Chain::Bsc);
        assert!(Chain::parse("solana").is_err());
    }

    #[test]
    fn parses_exchanges_with_aliases() {
        assert_eq!(Exchange::parse("uni-v3").unwrap(), Exchange::UniswapV3);
        assert_eq!(Exchange::parse("pancakeV2").unwrap(), Exchange::PancakeV2);
        assert_eq!(Exchange::parse("slipstream").unwrap(), Exchange::AerodromeSlipstream);
    }

    #[test]
    fn validates_chain_exchange_compat() {
        // Aerodrome only on Base.
        assert!(Selection::build("base", &["aerodrome".into()]).is_ok());
        assert!(Selection::build("bsc", &["aerodrome".into()]).is_err());
        // Pancake only on BSC.
        assert!(Selection::build("bsc", &["pancakev3".into()]).is_ok());
        assert!(Selection::build("base", &["pancakev3".into()]).is_err());
        // SunSwap only on Tron.
        assert!(Selection::build("tron", &["sunswap".into()]).is_ok());
    }

    #[test]
    fn requires_at_least_one_exchange() {
        assert!(Selection::build("base", &[]).is_err());
    }
}
