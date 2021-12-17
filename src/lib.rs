mod asset;
mod oracle;
mod utils;

use crate::asset::*;
use crate::oracle::*;
use crate::utils::*;

use near_sdk::borsh::{self, BorshDeserialize, BorshSerialize};
use near_sdk::collections::UnorderedMap;
use near_sdk::json_types::ValidAccountId;
use near_sdk::serde::{Deserialize, Serialize};
use near_sdk::{
    assert_one_yocto, env, ext_contract, near_bindgen, AccountId, Balance, BorshStorageKey,
    Duration, Gas, PanicOnDefault, Promise, Timestamp,
};

near_sdk::setup_alloc!();

const NO_DEPOSIT: Balance = 0;

const TGAS: Gas = 10u64.pow(12);
const GAS_FOR_PROMISE: Gas = 10 * TGAS;

const NEAR_CLAIM: Balance = 30 * 10u128.pow(23);
const NEAR_CLAIM_DURATION: Duration = 24 * 60 * 60 * 10u64.pow(9);

pub type DurationSec = u32;

#[derive(BorshSerialize, BorshStorageKey)]
enum StorageKey {
    Oracles,
    Assets,
}

#[near_bindgen]
#[derive(BorshDeserialize, BorshSerialize, PanicOnDefault)]
pub struct Contract {
    pub oracles: UnorderedMap<AccountId, VOracle>,

    pub assets: UnorderedMap<AssetId, VAsset>,

    pub recency_duration_sec: DurationSec,
}

#[derive(Serialize, Deserialize)]
#[serde(crate = "near_sdk::serde")]
pub struct PriceData {
    #[serde(with = "u64_dec_format")]
    pub timestamp: Timestamp,
    pub recency_duration_sec: DurationSec,

    pub prices: Vec<AssetOptionalPrice>,
}

#[ext_contract(ext_price_receiver)]
pub trait ExtPriceReceiver {
    fn oracle_on_call(&mut self, sender_id: AccountId, data: PriceData, msg: String);
}

#[near_bindgen]
impl Contract {
    #[init]
    pub fn new(recency_duration_sec: DurationSec) -> Self {
        Self {
            oracles: UnorderedMap::new(StorageKey::Oracles),
            assets: UnorderedMap::new(StorageKey::Assets),
            recency_duration_sec,
        }
    }

    #[private]
    pub fn set_recency_duration_sec(&mut self, recency_duration_sec: DurationSec) {
        self.recency_duration_sec = recency_duration_sec;
    }

    #[private]
    pub fn add_oracle(&mut self, account_id: ValidAccountId) {
        assert!(self.internal_get_oracle(account_id.as_ref()).is_none());
        self.internal_set_oracle(account_id.as_ref(), Oracle::new());
    }

    #[private]
    pub fn remove_oracle(&mut self, account_id: ValidAccountId) {
        assert!(self.oracles.remove(account_id.as_ref()).is_some());
    }

    /// Remove price data from removed oracle.
    pub fn clean_oracle_data(&mut self, account_id: ValidAccountId, asset_ids: Vec<AssetId>) {
        assert!(self.internal_get_oracle(account_id.as_ref()).is_none());
        for asset_id in asset_ids {
            let mut asset = self.internal_get_asset(&asset_id).expect("Unknown asset");
            if asset.remove_report(account_id.as_ref()) {
                self.internal_set_asset(&asset_id, asset);
            }
        }
    }

    #[private]
    pub fn add_asset(&mut self, asset_id: AssetId) {
        assert!(self.internal_get_asset(&asset_id).is_none());
        self.internal_set_asset(&asset_id, Asset::new());
    }

    pub fn get_oracle(&self, account_id: ValidAccountId) -> Option<Oracle> {
        self.internal_get_oracle(account_id.as_ref())
    }

    pub fn get_oracles(
        &self,
        from_index: Option<u64>,
        limit: Option<u64>,
    ) -> Vec<(AccountId, Oracle)> {
        unordered_map_pagination(&self.oracles, from_index, limit)
    }

    pub fn get_assets(
        &self,
        from_index: Option<u64>,
        limit: Option<u64>,
    ) -> Vec<(AccountId, Asset)> {
        unordered_map_pagination(&self.assets, from_index, limit)
    }

    pub fn get_asset(&self, asset_id: AssetId) -> Option<Asset> {
        self.internal_get_asset(&asset_id)
    }

    pub fn get_price_data(&self, asset_ids: Vec<AssetId>) -> PriceData {
        let timestamp = env::block_timestamp();
        let timestamp_cut = timestamp.saturating_sub(to_nano(self.recency_duration_sec));
        let min_num_recent_reports = std::cmp::max(1, (self.oracles.len() + 1) / 2) as usize;

        PriceData {
            timestamp,
            recency_duration_sec: self.recency_duration_sec,
            prices: asset_ids
                .into_iter()
                .map(|asset_id| {
                    let asset = self.internal_get_asset(&asset_id);
                    AssetOptionalPrice {
                        asset_id,
                        price: asset.and_then(|asset| {
                            asset.median_price(timestamp_cut, min_num_recent_reports)
                        }),
                    }
                })
                .collect(),
        }
    }

    /// Returns price data for a given oracle ID and given list of asset IDs.
    pub fn get_oracle_price_data(
        &self,
        account_id: ValidAccountId,
        asset_ids: Vec<AssetId>,
    ) -> PriceData {
        let timestamp = env::block_timestamp();
        let timestamp_cut = timestamp.saturating_sub(to_nano(self.recency_duration_sec));

        let oracle_id: AccountId = account_id.into();
        PriceData {
            timestamp,
            recency_duration_sec: self.recency_duration_sec,
            prices: asset_ids
                .into_iter()
                .map(|asset_id| {
                    let asset = self.internal_get_asset(&asset_id);
                    AssetOptionalPrice {
                        asset_id,
                        price: asset.and_then(|asset| {
                            asset
                                .reports
                                .into_iter()
                                .find(|report| report.oracle_id == oracle_id)
                                .filter(|report| report.timestamp >= timestamp_cut)
                                .map(|report| report.price)
                        }),
                    }
                })
                .collect(),
        }
    }

    pub fn report_prices(&mut self, prices: Vec<AssetPrice>) {
        assert!(!prices.is_empty());
        let oracle_id = env::predecessor_account_id();
        let timestamp = env::block_timestamp();

        // Oracle stats
        let mut oracle = self.internal_get_oracle(&oracle_id).expect("Not an oracle");
        oracle.last_report = timestamp;
        oracle.price_reports += prices.len() as u64;

        if oracle.last_near_claim + NEAR_CLAIM_DURATION <= timestamp {
            oracle.last_near_claim = timestamp;
            Promise::new(oracle_id.clone()).transfer(NEAR_CLAIM);
        }

        self.internal_set_oracle(&oracle_id, oracle);

        // Updating prices
        for AssetPrice { asset_id, price } in prices {
            price.assert_valid();
            let mut asset = self.internal_get_asset(&asset_id).expect("Unknown asset");
            asset.remove_report(&oracle_id);
            asset.add_report(Report {
                oracle_id: oracle_id.clone(),
                timestamp,
                price,
            });
            self.internal_set_asset(&asset_id, asset);
        }
    }

    pub fn report_prices_triple(&mut self, prices: Vec<AssetPrices>) {
        assert!(!prices.is_empty());
        let max_percentage:u128 = 40; //my
        let min_percentage:u128 = 28; //my

        let oracle_id = env::predecessor_account_id();
        let timestamp = env::block_timestamp();

        // Oracle stats
        let mut oracle = self.internal_get_oracle(&oracle_id).expect("Not an oracle");
        oracle.last_report = timestamp;
        oracle.price_reports += prices.len() as u64;

        if oracle.last_near_claim + NEAR_CLAIM_DURATION <= timestamp {
            oracle.last_near_claim = timestamp;
            Promise::new(oracle_id.clone()).transfer(NEAR_CLAIM);
        }

        self.internal_set_oracle(&oracle_id, oracle);

        // Updating prices
        for AssetPrices { asset_id, price_b, price_cm, price_cg} in prices {
            price_b.assert_valid();
            price_cm.assert_valid();
            price_cg.assert_valid();
            let mut asset = self.internal_get_asset(&asset_id).expect("Unknown asset");
            let mut middle_price:u128 = (price_b.get_balance() + price_cg.get_balance() + price_cm.get_balance())/3;
            let firtst_price_percentage:u128 = (price_b.get_balance() * 100 as u128)/middle_price;
            let second_price_percentage:u128 = (price_cm.get_balance() * 100 as u128)/middle_price;
            let third_price_percentage:u128 = (price_cg.get_balance() * 100 as u128)/middle_price;
            if firtst_price_percentage > max_percentage {
                if second_price_percentage > max_percentage {
                    middle_price = (price_b.get_balance() + price_cm.get_balance())/2;
                }else if third_price_percentage > max_percentage{
                    middle_price = (price_b.get_balance() + price_cg.get_balance())/2;
                } else{
                    middle_price = (price_cg.get_balance() + price_cm.get_balance())/2;
                }
            }else if firtst_price_percentage < min_percentage {
                if second_price_percentage < min_percentage{
                    middle_price = (price_b.get_balance() + price_cm.get_balance())/2;
                }else if third_price_percentage < min_percentage{
                    middle_price = (price_b.get_balance() + price_cg.get_balance())/2;
                } else{
                    middle_price = (price_cg.get_balance() + price_cm.get_balance())/2;
                }
            }

            asset.remove_report(&oracle_id);
            asset.add_report(Report {
                oracle_id: oracle_id.clone(),
                timestamp,
                price: Price{
                    multiplier: middle_price,
                    decimals: price_b.get_decimals()
                },
            });
            self.internal_set_asset(&asset_id, asset);
        }
    }

    #[payable]
    pub fn oracle_call(
        &mut self,
        receiver_id: ValidAccountId,
        asset_ids: Vec<AssetId>,
        msg: String,
    ) -> Promise {
        self.assert_well_paid();

        let sender_id = env::predecessor_account_id();
        let price_data = self.get_price_data(asset_ids);
        let remaining_gas = env::prepaid_gas() - env::used_gas();
        assert!(remaining_gas >= GAS_FOR_PROMISE);

        ext_price_receiver::oracle_on_call(
            sender_id,
            price_data,
            msg,
            receiver_id.as_ref(),
            NO_DEPOSIT,
            remaining_gas - GAS_FOR_PROMISE,
        )
    }
}

impl Contract {
    pub fn assert_well_paid(&self) {
        assert_one_yocto();
    }
}

// near call price.rkonoval.testnet report_prices_triple '{"prices":[{"asset_id":"RUS", "price_b":{"multiplier":"2", "decimals":7}, "price_cm":{"multiplier":"6", "decimals":7}, "price_cg":{"multiplier":"4", "decimals":7}}]}' --accountId sergei24.testnet
//47.123.338.425.891.180.000.000
//340.282.366.920.938.586.008.062.602.462.446.642.046