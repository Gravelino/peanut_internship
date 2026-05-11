use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::path::Path;

use rust_decimal::Decimal;
use serde::Deserialize;

use crate::core::types::{Address, split_pair_symbols as split_core_pair_symbols};

#[derive(Debug, Clone, Deserialize)]
pub struct AddressBookEntry {
    pub base: String,
    pub base_decimals: u8,
    pub quote: String,
    pub quote_decimals: u8,
    #[serde(default)]
    pub pool: Option<String>,
    #[serde(default = "default_pool_type")]
    pub pool_type: String,
    #[serde(default, alias = "fee", alias = "fee_tier", alias = "fee_bps")]
    pub v3_fee: Option<u32>,
    #[serde(default)]
    pub v3_path: Option<Vec<String>>,
    #[serde(default, alias = "fees", alias = "fee_path", alias = "v3_fee_path")]
    pub v3_fees: Option<Vec<u32>>,
    #[serde(default)]
    pub quoter: Option<String>,
    #[serde(default = "default_quoter_type")]
    pub quoter_type: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressBookPoolKind {
    V2,
    V3,
}

impl AddressBookPoolKind {
    pub fn parse(pair: &str, raw: &str) -> Result<Self, Box<dyn std::error::Error>> {
        match raw.to_ascii_lowercase().as_str() {
            "v2" => Ok(Self::V2),
            "v3" => Ok(Self::V3),
            other => Err(format!(
                "pair '{pair}' has unsupported pool_type '{other}', expected 'v2' or 'v3'"
            )
            .into()),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::V2 => "v2",
            Self::V3 => "v3",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ParsedAddressBookPair {
    pub pair: String,
    pub base_symbol: String,
    pub quote_symbol: String,
    pub base: Address,
    pub base_decimals: u8,
    pub quote: Address,
    pub quote_decimals: u8,
    pub pool: Option<Address>,
    pub pool_kind: AddressBookPoolKind,
    pub v3_fee: Option<u32>,
    pub v3_path: Option<Vec<Address>>,
    pub v3_fees: Option<Vec<u32>>,
    pub quoter: Option<Address>,
    pub quoter_type: String,
}

#[derive(Debug, Clone)]
pub struct AddressBookTokenConfig {
    pub address: Address,
    pub decimals: u8,
}

pub type RawAddressBook = BTreeMap<String, AddressBookEntry>;

pub fn default_pool_type() -> String {
    "v2".to_string()
}

pub fn default_quoter_type() -> String {
    "quoter_v2".to_string()
}

pub fn load_raw_address_book(
    path: impl AsRef<Path>,
) -> Result<RawAddressBook, Box<dyn std::error::Error>> {
    Ok(serde_json::from_reader(File::open(path)?)?)
}

pub fn load_parsed_address_book(
    path: impl AsRef<Path>,
) -> Result<Vec<ParsedAddressBookPair>, Box<dyn std::error::Error>> {
    parse_address_book(load_raw_address_book(path)?)
}

pub fn parse_address_book(
    raw: RawAddressBook,
) -> Result<Vec<ParsedAddressBookPair>, Box<dyn std::error::Error>> {
    let mut out = Vec::new();
    for (pair, entry) in raw {
        let (base_symbol, quote_symbol) = split_pair_symbols(&pair)?;
        out.push(ParsedAddressBookPair {
            pair: pair.clone(),
            base_symbol: base_symbol.to_ascii_uppercase(),
            quote_symbol: quote_symbol.to_ascii_uppercase(),
            base: Address::new(&entry.base)?,
            base_decimals: entry.base_decimals,
            quote: Address::new(&entry.quote)?,
            quote_decimals: entry.quote_decimals,
            pool: entry.pool.as_deref().map(Address::new).transpose()?,
            pool_kind: AddressBookPoolKind::parse(&pair, &entry.pool_type)?,
            v3_fee: entry.v3_fee,
            v3_path: parse_optional_v3_path(entry.v3_path)?,
            v3_fees: entry.v3_fees,
            quoter: entry.quoter.as_deref().map(Address::new).transpose()?,
            quoter_type: entry.quoter_type,
        });
    }
    Ok(out)
}

pub fn split_pair_symbols(pair: &str) -> Result<(&str, &str), Box<dyn std::error::Error>> {
    split_core_pair_symbols(pair).map_err(|error| error.into())
}

pub fn parse_optional_v3_path(
    raw: Option<Vec<String>>,
) -> Result<Option<Vec<Address>>, Box<dyn std::error::Error>> {
    raw.map(|path| {
        path.into_iter()
            .map(|address| Address::new(&address).map_err(|e| e.into()))
            .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()
    })
    .transpose()
}

pub fn selected_pairs<'a>(
    pairs: &'a [ParsedAddressBookPair],
    selected: &[String],
) -> Result<Vec<&'a ParsedAddressBookPair>, Box<dyn std::error::Error>> {
    selected
        .iter()
        .map(|wanted| {
            pairs
                .iter()
                .find(|pair| &pair.pair == wanted)
                .ok_or_else(|| {
                    format!("live execution pair '{wanted}' missing from --dex-address-book").into()
                })
        })
        .collect()
}

pub fn validate_selected_pool_compatibility(
    pairs: &[&ParsedAddressBookPair],
) -> Result<(), Box<dyn std::error::Error>> {
    for pair in pairs {
        if pair.pool.is_none() {
            return Err(format!(
                "live execution pair '{}' has no pool in --dex-address-book",
                pair.pair
            )
            .into());
        }
        validate_v3_route(pair)?;
    }
    Ok(())
}

pub fn validate_v3_route(pair: &ParsedAddressBookPair) -> Result<(), Box<dyn std::error::Error>> {
    if pair.pool_kind != AddressBookPoolKind::V3 {
        return Ok(());
    }
    match (&pair.v3_path, &pair.v3_fees) {
        (Some(path), Some(fees)) => {
            if path.len() < 2 {
                return Err(format!(
                    "live execution pair '{}' has v3_path with fewer than 2 tokens",
                    pair.pair
                )
                .into());
            }
            if fees.len() + 1 != path.len() {
                return Err(format!(
                    "live execution pair '{}' has v3_fees length {}, expected {} for v3_path length {}",
                    pair.pair,
                    fees.len(),
                    path.len().saturating_sub(1),
                    path.len()
                )
                .into());
            }
            let starts_base_ends_quote =
                path.first() == Some(&pair.base) && path.last() == Some(&pair.quote);
            let starts_quote_ends_base =
                path.first() == Some(&pair.quote) && path.last() == Some(&pair.base);
            if !starts_base_ends_quote && !starts_quote_ends_base {
                return Err(format!(
                    "live execution pair '{}' v3_path endpoints must match pair base/quote token addresses",
                    pair.pair
                )
                .into());
            }
        }
        (None, None) => {}
        _ => {
            return Err(format!(
                "live execution pair '{}' uses V3 route but must provide both v3_path and v3_fees",
                pair.pair
            )
            .into());
        }
    }
    Ok(())
}

pub fn validate_cex_pair_symbols(pairs: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    for pair in pairs {
        let (base, quote) = split_pair_symbols(pair)?;
        for asset in [base, quote] {
            if !asset.chars().all(|c| c.is_ascii_alphanumeric()) {
                return Err(format!(
                    "live execution pair '{pair}' contains CEX-incompatible asset symbol '{asset}'; Binance symbol mapping only supports alphanumeric asset symbols"
                )
                .into());
            }
        }
    }
    Ok(())
}

pub fn validate_asset_symbol_uniqueness(
    pairs: &[&ParsedAddressBookPair],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut by_symbol: HashMap<String, (String, String)> = HashMap::new();
    for pair in pairs {
        for (symbol, address) in [
            (&pair.base_symbol, &pair.base),
            (&pair.quote_symbol, &pair.quote),
        ] {
            let key = symbol.to_ascii_uppercase();
            let lower = address.lower();
            if let Some((seen_address, seen_pair)) = by_symbol.get(&key) {
                if seen_address != &lower {
                    return Err(format!(
                        "live execution asset symbol '{key}' maps to multiple token addresses across selected pairs: {seen_address} in {seen_pair}, {lower} in {}; use distinct symbols such as USDC and USDC_E",
                        pair.pair
                    )
                    .into());
                }
            } else {
                by_symbol.insert(key, (lower, pair.pair.clone()));
            }
        }
    }
    Ok(())
}

pub fn validate_arbitrum_known_token_symbols(
    pairs: &[&ParsedAddressBookPair],
    native_usdc: &Address,
) -> Result<(), Box<dyn std::error::Error>> {
    for pair in pairs {
        for (symbol, address) in [
            (&pair.base_symbol, &pair.base),
            (&pair.quote_symbol, &pair.quote),
        ] {
            if symbol.eq_ignore_ascii_case("USDC") && address != native_usdc {
                return Err(format!(
                    "live execution pair '{}' uses symbol USDC for token {}; on Arbitrum live CEX↔DEX accounting requires native USDC {}; use a distinct symbol such as USDC_E for bridged USDC.e",
                    pair.pair, address, native_usdc
                )
                .into());
            }
        }
    }
    Ok(())
}

pub fn selected_pool_kinds(pairs: &[&ParsedAddressBookPair]) -> (bool, bool) {
    let mut has_v2 = false;
    let mut has_v3 = false;
    for pair in pairs {
        match pair.pool_kind {
            AddressBookPoolKind::V2 => has_v2 = true,
            AddressBookPoolKind::V3 => has_v3 = true,
        }
    }
    (has_v2, has_v3)
}

pub fn unique_tokens(pairs: &[ParsedAddressBookPair]) -> Vec<(String, Address, u8)> {
    let mut tokens = BTreeMap::new();
    for pair in pairs {
        tokens.entry(pair.base.lower()).or_insert_with(|| {
            (
                pair.base_symbol.clone(),
                pair.base.clone(),
                pair.base_decimals,
            )
        });
        tokens.entry(pair.quote.lower()).or_insert_with(|| {
            (
                pair.quote_symbol.clone(),
                pair.quote.clone(),
                pair.quote_decimals,
            )
        });
    }
    tokens.into_values().collect()
}

pub fn rebalance_token_book(
    pairs: &[ParsedAddressBookPair],
) -> HashMap<String, AddressBookTokenConfig> {
    let mut out = HashMap::new();
    for pair in pairs {
        out.entry(pair.base_symbol.to_ascii_uppercase())
            .or_insert(AddressBookTokenConfig {
                address: pair.base.clone(),
                decimals: pair.base_decimals,
            });
        out.entry(pair.quote_symbol.to_ascii_uppercase())
            .or_insert(AddressBookTokenConfig {
                address: pair.quote.clone(),
                decimals: pair.quote_decimals,
            });
    }
    out
}

pub fn dex_pool_fee_bps_map(pairs: &[ParsedAddressBookPair]) -> HashMap<String, Decimal> {
    let mut out = HashMap::new();
    for pair in pairs {
        let fee_bps = match pair.pool_kind {
            AddressBookPoolKind::V2 => Some(Decimal::from(30)),
            AddressBookPoolKind::V3 => pair
                .v3_fees
                .as_ref()
                .map(|fees| fees.iter().map(|fee| Decimal::from(*fee)).sum::<Decimal>())
                .or_else(|| pair.v3_fee.map(Decimal::from))
                .map(|fee_hundredths_bps| fee_hundredths_bps / Decimal::from(100)),
        };
        if let Some(fee_bps) = fee_bps {
            out.insert(pair.pair.clone(), fee_bps);
        }
    }
    out
}
