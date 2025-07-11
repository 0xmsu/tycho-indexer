use std::collections::{HashMap, HashSet};

use alloy::{
    eips::BlockNumberOrTag,
    primitives::Uint,
    rpc::client::{ClientBuilder, RpcClient},
};
use async_trait::async_trait;
use chrono::NaiveDateTime;
use ethers::{
    middleware::Middleware,
    prelude::{BlockId, Http, Provider, H160, H256, U256},
    providers::ProviderError,
};
use futures::future::try_join_all;
use serde::{Deserialize, Serialize};
use tracing::{trace, warn};
use tycho_common::{
    models::{blockchain::Block, contract::AccountDelta, Address, Chain, ChangeType},
    traits::{AccountExtractor, StorageSnapshotRequest},
    Bytes,
};

use crate::{BytesCodec, RPCError};

/// `EVMAccountExtractor` is a struct that implements the `AccountExtractor` trait for Ethereum
/// accounts. It is recommended for nodes that do not support batch requests.
pub struct EVMAccountExtractor {
    provider: Provider<Http>,
    chain: Chain,
}

/// `EVMBatchAccountExtractor` is a struct that implements the `AccountStorageSource` trait for
/// Ethereum accounts. It can only be used with nodes that support batch requests. If you are using
/// a node that does not support batch requests, use `EVMAccountExtractor` instead.
pub struct EVMBatchAccountExtractor {
    provider: RpcClient,
    chain: Chain,
}

#[async_trait]
impl AccountExtractor for EVMAccountExtractor {
    type Error = RPCError;

    async fn get_accounts_at_block(
        &self,
        block: &Block,
        requests: &[StorageSnapshotRequest],
    ) -> Result<HashMap<Bytes, AccountDelta>, Self::Error> {
        let mut updates = HashMap::new();
        let block_id = Some(BlockId::from(block.number));

        // Convert addresses to H160 for easier handling
        let h160_addresses: Vec<H160> = requests
            .iter()
            .map(|request| H160::from_bytes(&request.address))
            .collect();

        // Create futures for balance and code retrieval
        let balance_futures: Vec<_> = h160_addresses
            .iter()
            .map(|&address| {
                self.provider
                    .get_balance(address, block_id)
            })
            .collect();

        let code_futures: Vec<_> = h160_addresses
            .iter()
            .map(|&address| {
                self.provider
                    .get_code(address, block_id)
            })
            .collect();

        // Execute all balance and code requests concurrently
        let (result_balances, result_codes) =
            tokio::join!(try_join_all(balance_futures), try_join_all(code_futures));

        let balances = result_balances?;
        let codes = result_codes?;

        // Process each address with its corresponding balance and code
        for (i, &address) in h160_addresses.iter().enumerate() {
            trace!(contract=?address, block_number=?block.number, block_hash=?block.hash, "Extracting contract code and storage" );

            let balance = Some(balances[i]);
            let code = Some(Bytes::from(codes[i].to_vec()));

            let slots_request = requests
                .get(i)
                .expect("Request should exist");
            if slots_request.slots.is_some() {
                // TODO: Implement this
                warn!("Specific storage slot requests are not supported in EVMAccountExtractor");
            }

            let slots = self
                .get_storage_range(address, H256::from_bytes(&block.hash))
                .await?
                .into_iter()
                .map(|(k, v)| (k.to_bytes(), Some(v.to_bytes())))
                .collect();

            updates.insert(
                Bytes::from(address.to_fixed_bytes()),
                AccountDelta {
                    address: address.to_bytes(),
                    chain: self.chain,
                    slots,
                    balance: balance.map(BytesCodec::to_bytes),
                    code,
                    change: ChangeType::Creation,
                },
            );
        }

        return Ok(updates);
    }
}

impl EVMAccountExtractor {
    pub async fn new(node_url: &str, chain: Chain) -> Result<Self, RPCError>
    where
        Self: Sized,
    {
        let provider = Provider::<Http>::try_from(node_url);
        match provider {
            Ok(p) => Ok(Self { provider: p, chain }),
            Err(e) => Err(RPCError::SetupError(e.to_string())),
        }
    }

    async fn get_storage_range(
        &self,
        address: H160,
        block: H256,
    ) -> Result<HashMap<U256, U256>, RPCError> {
        let mut all_slots = HashMap::new();
        let mut start_key = H256::zero();
        let block = format!("0x{block:x}");
        loop {
            let params = serde_json::json!([
                block, 0, // transaction index, 0 for the state at the end of the block
                address, start_key, 100000 // limit
            ]);

            trace!("Requesting storage range for {:?}, block: {:?}", address, block);
            let result: StorageRange = self
                .provider
                .request("debug_storageRangeAt", params)
                .await?;

            for (_, entry) in result.storage {
                all_slots
                    .insert(U256::from(entry.key.as_bytes()), U256::from(entry.value.as_bytes()));
            }

            if let Some(next_key) = result.next_key {
                start_key = next_key;
            } else {
                break;
            }
        }

        Ok(all_slots)
    }

    pub async fn get_block_data(&self, block_id: i64) -> Result<Block, RPCError> {
        let block = self
            .provider
            .get_block(BlockId::from(u64::try_from(block_id).expect("Invalid block number")))
            .await?
            .expect("Block not found");

        Ok(Block {
            number: block.number.unwrap().as_u64(),
            hash: block.hash.unwrap().to_bytes(),
            parent_hash: block.parent_hash.to_bytes(),
            chain: Chain::Ethereum,
            ts: NaiveDateTime::from_timestamp_opt(block.timestamp.as_u64() as i64, 0)
                .expect("Failed to convert timestamp"),
        })
    }
}

impl EVMBatchAccountExtractor {
    pub async fn new(node_url: &str, chain: Chain) -> Result<Self, RPCError>
    where
        Self: Sized,
    {
        let url = url::Url::parse(node_url)
            .map_err(|_| RPCError::SetupError("Invalid URL".to_string()))?;
        let provider = ClientBuilder::default().http(url);
        Ok(Self { provider, chain })
    }

    async fn batch_fetch_account_code_and_balance(
        &self,
        block: &Block,
        max_batch_size: usize,
        chunk: &[StorageSnapshotRequest],
    ) -> Result<(HashMap<Bytes, Bytes>, HashMap<Bytes, Bytes>), RPCError> {
        let mut batch = self.provider.new_batch();
        let mut code_requests = Vec::with_capacity(max_batch_size);
        let mut balance_requests = Vec::with_capacity(max_batch_size);

        for request in chunk {
            code_requests.push(Box::pin(
                batch
                    .add_call(
                        "eth_getCode",
                        &(&request.address, BlockNumberOrTag::from(block.number)),
                    )
                    .map_err(|e| {
                        RPCError::RequestError(ProviderError::CustomError(format!(
                            "Failed to get code: {e}",
                        )))
                    })?
                    .map_resp(|resp: Bytes| resp.to_vec()),
            ));

            balance_requests.push(Box::pin(
                batch
                    .add_call::<_, Uint<256, 4>>(
                        "eth_getBalance",
                        &(&request.address, BlockNumberOrTag::from(block.number)),
                    )
                    .map_err(|e| {
                        RPCError::RequestError(ProviderError::CustomError(format!(
                            "Failed to get balance: {e}",
                        )))
                    })?,
            ));
        }

        batch.send().await.map_err(|e| {
            RPCError::RequestError(ProviderError::CustomError(format!(
                "Failed to send batch request: {e}",
            )))
        })?;

        let mut codes: HashMap<Bytes, Bytes> = HashMap::with_capacity(max_batch_size);
        let mut balances: HashMap<Bytes, Bytes> = HashMap::with_capacity(max_batch_size);

        for (idx, request) in chunk.iter().enumerate() {
            let address = &request.address;

            let code_result = code_requests[idx]
                .as_mut()
                .await
                .map_err(|e| {
                    RPCError::RequestError(ProviderError::CustomError(format!(
                        "Failed to collect code request data: {e}",
                    )))
                })?;

            codes.insert(address.clone(), code_result.into());

            let balance_result = balance_requests[idx]
                .as_mut()
                .await
                .map_err(|e| {
                    RPCError::RequestError(ProviderError::CustomError(format!(
                        "Failed to collect balance request data: {e}",
                    )))
                })?;

            balances.insert(address.clone(), Bytes::from(balance_result.to_be_bytes::<32>()));
        }
        Ok((codes, balances))
    }

    async fn fetch_account_storage(
        &self,
        block: &Block,
        max_batch_size: usize,
        request: &StorageSnapshotRequest,
    ) -> Result<HashMap<Bytes, Option<Bytes>>, RPCError> {
        let mut storage_requests = Vec::with_capacity(max_batch_size);

        let mut result = HashMap::new();

        match request.slots.clone() {
            Some(slots) => {
                for slot_batch in slots.chunks(max_batch_size) {
                    let mut storage_batch = self.provider.new_batch();

                    for slot in slot_batch {
                        storage_requests.push(Box::pin(
                            storage_batch
                                .add_call(
                                    "eth_getStorageAt",
                                    &(&request.address, slot, BlockNumberOrTag::from(block.number)),
                                )
                                .map_err(|e| {
                                    RPCError::RequestError(ProviderError::CustomError(format!(
                                        "Failed to get storage: {e}",
                                    )))
                                })?
                                .map_resp(|res: Bytes| res.to_vec()),
                        ));
                    }

                    storage_batch
                        .send()
                        .await
                        .map_err(|e| {
                            RPCError::RequestError(ProviderError::CustomError(format!(
                                "Failed to send batch request: {e}",
                            )))
                        })?;

                    for (idx, slot) in slot_batch.iter().enumerate() {
                        let storage_result = storage_requests[idx]
                            .as_mut()
                            .await
                            .map_err(|e| {
                                RPCError::RequestError(ProviderError::CustomError(format!(
                                    "Failed to collect storage request data: {e}",
                                )))
                            })?;

                        let value = if storage_result == [0; 32] {
                            None
                        } else {
                            Some(Bytes::from(storage_result))
                        };

                        result.insert(slot.clone(), value);
                    }
                }
            }
            None => {
                let storage = self
                    .get_storage_range(&request.address, block)
                    .await?;
                for (key, value) in storage {
                    result.insert(key, Some(value));
                }
                return Ok(result);
            }
        }

        Ok(result)
    }

    async fn get_storage_range(
        &self,
        address: &Address,
        block: &Block,
    ) -> Result<HashMap<Bytes, Bytes>, RPCError> {
        warn!("Requesting all storage slots for address: {:?}. This request can consume a lot of data, and the method might not be available on the requested chain / node.", address);

        let mut all_slots = HashMap::new();
        let mut start_key = H256::zero();
        loop {
            trace!("Requesting storage range for {:?}, block: {:?}", address.clone(), block);
            let result: StorageRange = self
                .provider
                .request(
                    "debug_storageRangeAt",
                    &(
                        block.hash.to_string(),
                        0, // transaction index, 0 for the state at the end of the block
                        address,
                        start_key,
                        100000, // limit
                    ),
                )
                .await
                .map_err(|e| {
                    RPCError::RequestError(ProviderError::CustomError(format!(
                        "Failed to get storage: {e}",
                    )))
                })?;

            for (_, entry) in result.storage {
                all_slots
                    .insert(Bytes::from(entry.key.as_bytes()), Bytes::from(entry.value.as_bytes()));
            }

            if let Some(next_key) = result.next_key {
                start_key = next_key;
            } else {
                break;
            }
        }

        Ok(all_slots)
    }
}

#[async_trait]
impl AccountExtractor for EVMBatchAccountExtractor {
    type Error = RPCError;

    async fn get_accounts_at_block(
        &self,
        block: &Block,
        requests: &[StorageSnapshotRequest],
    ) -> Result<HashMap<Address, AccountDelta>, Self::Error> {
        let mut updates = HashMap::new();

        // Remove duplicates to avoid making more requests than necessary.
        let unique_requests: Vec<StorageSnapshotRequest> = requests
            .iter()
            .cloned()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();

        // TODO: Make these configurable and optimize for preventing rate limiting.
        // TODO: Handle rate limiting / individual connection failures & retries

        let max_batch_size = 100;
        let storage_max_batch_size = 10000;
        for chunk in unique_requests.chunks(max_batch_size) {
            // Batch request code and balances of all accounts on the chunk.
            // Worst case scenario = 2 * chunk_size requests
            let metadata_fut =
                self.batch_fetch_account_code_and_balance(block, max_batch_size, chunk);

            let mut storage_futures = Vec::new();
            // Batch requests storage_max_batch_size until
            // Worst case scenario = chunk_size * (MAX_EVM_STORAGE_LIMIT / storage_max_batch_size)
            // requests
            for request in chunk.iter() {
                storage_futures.push(self.fetch_account_storage(
                    block,
                    storage_max_batch_size,
                    request,
                ));
            }

            let (codes, balances) = metadata_fut.await?;
            let storage_results = try_join_all(storage_futures).await?;

            for (idx, request) in chunk.iter().enumerate() {
                let address = &request.address;
                let code = codes.get(address).cloned();
                let balance = balances.get(address).cloned();
                let storage = storage_results
                    .get(idx)
                    .cloned()
                    .ok_or_else(|| {
                        RPCError::UnknownError(format!(
                            "Unable to find storage result. Request: {request:?} at block: {block:?}"
                        ))
                    })?;

                let account_delta = AccountDelta {
                    address: address.clone(),
                    chain: self.chain,
                    slots: storage,
                    balance,
                    code,
                    change: ChangeType::Creation,
                };

                updates.insert(address.clone(), account_delta);
            }
        }

        Ok(updates)
    }
}

#[derive(Serialize, Deserialize, Debug)]
struct StorageEntry {
    key: H256,
    value: H256,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct StorageRange {
    storage: HashMap<H256, StorageEntry>,
    next_key: Option<H256>,
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use tracing_test::traced_test;

    use super::*;

    // Common test constants
    const BALANCER_VAULT_STR: &str = "0xba12222222228d8ba445958a75a0704d566bf2c8";
    const STETH_STR: &str = "0xae7ab96520de3a18e5e111b5eaab095312d7fe84";
    const TEST_BLOCK_HASH: &str =
        "0x7f70ac678819e24c4947a3a95fdab886083892a18ba1a962ebaac31455584042";
    const TEST_BLOCK_NUMBER: u64 = 20378314;

    // Common token addresses for tests
    const TOKEN_ADDRESSES: [&str; 5] = [
        BALANCER_VAULT_STR,
        STETH_STR,
        "0x6b175474e89094c44da98b954eedeac495271d0f", // DAI
        "0x2260fac5e5542a773aa44fbcfedf7c193bc2c599", // WBTC
        "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48", // USDC
    ];

    // Common storage slots for testing
    fn get_test_slots() -> HashMap<Bytes, Bytes> {
        HashMap::from([
            (
                Bytes::from_str("0000000000000000000000000000000000000000000000000000000000000000")
                    .unwrap(),
                Bytes::from_str("0000000000000000000000000000000000000000000000000000000000000001")
                    .unwrap(),
            ),
            (
                Bytes::from_str("0000000000000000000000000000000000000000000000000000000000000003")
                    .unwrap(),
                Bytes::from_str("00000000000000000000006048a8c631fb7e77eca533cf9c29784e482391e700")
                    .unwrap(),
            ),
            (
                Bytes::from_str("00015ea75c6f99b2e8663793de8ab1ce7c52e3295bf307bbf9990d4af56f7035")
                    .unwrap(),
                Bytes::from_str("0000000000000000000000000000000000000000000000000000000000000001")
                    .unwrap(),
            ),
        ])
    }

    // Test fixture setup
    struct TestFixture {
        block: Block,
        node_url: String,
    }

    impl TestFixture {
        async fn new() -> Self {
            let node_url = std::env::var("RPC_URL").expect("RPC_URL must be set for testing");

            let block_hash = H256::from_str(TEST_BLOCK_HASH).expect("valid block hash");

            let block = Block::new(
                TEST_BLOCK_NUMBER,
                Chain::Ethereum,
                block_hash.to_bytes(),
                Default::default(),
                Default::default(),
            );

            Self { block, node_url }
        }

        async fn create_evm_extractor(&self) -> Result<EVMAccountExtractor, RPCError> {
            EVMAccountExtractor::new(&self.node_url, Chain::Ethereum).await
        }

        async fn create_batch_extractor(&self) -> Result<EVMBatchAccountExtractor, RPCError> {
            EVMBatchAccountExtractor::new(&self.node_url, Chain::Ethereum).await
        }

        fn create_address(address_str: &str) -> Address {
            Address::from_str(address_str).expect("valid address")
        }

        fn create_storage_request(
            address_str: &str,
            slots: Option<Vec<Bytes>>,
        ) -> StorageSnapshotRequest {
            StorageSnapshotRequest { address: Self::create_address(address_str), slots }
        }
    }

    #[tokio::test]
    #[ignore = "require RPC connection"]
    async fn test_account_extractor() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = TestFixture::new().await;
        let extractor = fixture.create_evm_extractor().await?;

        let requests = vec![TestFixture::create_storage_request(BALANCER_VAULT_STR, None)];

        let updates = extractor
            .get_accounts_at_block(&fixture.block, &requests)
            .await?;

        assert_eq!(updates.len(), 1);
        let update = updates
            .get(&Bytes::from_str(BALANCER_VAULT_STR).expect("valid address"))
            .expect("update exists");

        assert_eq!(update.slots.len(), 47690);

        Ok(())
    }

    #[traced_test]
    #[tokio::test]
    #[ignore = "require RPC connection"]
    /// Test the contract extractor with a large number of storage slots (stETH is the 9th largest
    /// token by number of holders).
    /// This test takes around 2 mins to run and retreives around 50mb of data
    async fn test_contract_extractor_steth() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = TestFixture::new().await;
        let extractor = fixture.create_evm_extractor().await?;

        let requests = vec![TestFixture::create_storage_request(STETH_STR, None)];

        println!("Getting accounts for block: {TEST_BLOCK_NUMBER:?}");
        let start_time = std::time::Instant::now();
        let updates = extractor
            .get_accounts_at_block(&fixture.block, &requests)
            .await?;
        let duration = start_time.elapsed();
        println!("Time taken to get accounts: {duration:?}");

        assert_eq!(updates.len(), 1);
        let update = updates
            .get(&Bytes::from_str(STETH_STR).expect("valid address"))
            .expect("update exists");

        assert_eq!(update.slots.len(), 789526);

        Ok(())
    }

    #[traced_test]
    #[tokio::test]
    #[ignore = "require RPC connection"]
    async fn test_get_storage_snapshots() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = TestFixture::new().await;
        println!("Using node: {}", fixture.node_url);

        let extractor = fixture.create_batch_extractor().await?;

        let requests = vec![
            TestFixture::create_storage_request(BALANCER_VAULT_STR, Some(vec![])),
            TestFixture::create_storage_request(STETH_STR, Some(vec![])),
        ];

        let start_time = std::time::Instant::now();
        let result = extractor
            .get_accounts_at_block(&fixture.block, &requests)
            .await?;
        let duration = start_time.elapsed();
        println!("Time taken to get storage snapshots: {duration:?}");

        assert_eq!(result.len(), 2);

        // First account check
        let first_address = TestFixture::create_address(BALANCER_VAULT_STR);
        let first_delta = result
            .get(&first_address)
            .expect("first address should exist");
        assert_eq!(first_delta.address, first_address);
        assert_eq!(first_delta.chain, Chain::Ethereum);
        assert!(first_delta.code.is_some());
        assert!(first_delta.balance.is_some());
        println!("Balance: {:?}", first_delta.balance);

        // Second account check
        let second_address = TestFixture::create_address(STETH_STR);
        let second_delta: &AccountDelta = result
            .get(&second_address)
            .expect("second address should exist");
        assert_eq!(second_delta.address, second_address);
        assert_eq!(second_delta.chain, Chain::Ethereum);
        assert!(second_delta.code.is_some());
        assert!(second_delta.balance.is_some());
        println!("Balance: {:?}", second_delta.balance);

        Ok(())
    }

    #[traced_test]
    #[tokio::test]
    #[ignore = "require RPC connection"]
    async fn test_evm_batch_extractor_new() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = TestFixture::new().await;

        // Test with valid URL
        let extractor = EVMBatchAccountExtractor::new(&fixture.node_url, Chain::Ethereum).await?;
        assert_eq!(extractor.chain, Chain::Ethereum);

        // Test with invalid URL
        let result = EVMBatchAccountExtractor::new("invalid-url", Chain::Ethereum).await;
        assert!(result.is_err());

        Ok(())
    }

    #[traced_test]
    #[tokio::test]
    #[ignore = "require RPC connection"]
    async fn test_batch_fetch_account_code_and_balance() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = TestFixture::new().await;
        let extractor = fixture.create_batch_extractor().await?;

        // Test with multiple addresses
        let requests = vec![
            TestFixture::create_storage_request(BALANCER_VAULT_STR, Some(Vec::new())),
            TestFixture::create_storage_request(STETH_STR, Some(Vec::new())),
        ];

        let (codes, balances) = extractor
            .batch_fetch_account_code_and_balance(&fixture.block, 10, &requests)
            .await?;

        // Check that we got code and balance for both addresses
        assert_eq!(codes.len(), 2);
        assert_eq!(balances.len(), 2);

        // Check that the first address has code and balance
        let first_address = TestFixture::create_address(BALANCER_VAULT_STR);
        assert!(codes.contains_key(&first_address));
        assert!(balances.contains_key(&first_address));

        // Check that the second address has code and balance
        let second_address = TestFixture::create_address(STETH_STR);
        assert!(codes.contains_key(&second_address));
        assert!(balances.contains_key(&second_address));

        // Verify code is non-empty for contract addresses
        assert!(!codes
            .get(&first_address)
            .unwrap()
            .is_empty());
        assert!(!codes
            .get(&second_address)
            .unwrap()
            .is_empty());

        Ok(())
    }

    #[traced_test]
    #[tokio::test]
    #[ignore = "require RPC connection"]
    async fn test_fetch_account_storage_without_specific_slots(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let fixture = TestFixture::new().await;
        let extractor = fixture.create_batch_extractor().await?;

        // Create request with specific slots
        let slots = get_test_slots();
        let request = TestFixture::create_storage_request(BALANCER_VAULT_STR, None);

        let storage = extractor
            .fetch_account_storage(&fixture.block, 10, &request)
            .await?;

        // Verify that we got the storage for all requested slots
        assert_eq!(storage.len(), 47690);

        // Check that each slot has a value
        for (key, value) in slots.iter().take(3) {
            println!("slot: {key:?}");
            assert!(storage.contains_key(key));
            assert_eq!(
                storage
                    .get(key)
                    .and_then(|v| v.as_ref()),
                Some(value)
            ); // Storage value exists and matches
        }

        Ok(())
    }

    #[traced_test]
    #[tokio::test]
    #[ignore = "require RPC connection"]
    async fn test_fetch_account_storage_with_specific_slots(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let fixture = TestFixture::new().await;
        let extractor = fixture.create_batch_extractor().await?;

        // Create request with specific slots
        let slots = get_test_slots();
        let slots_request: Vec<Bytes> = slots.keys().cloned().collect();
        let request = TestFixture::create_storage_request(BALANCER_VAULT_STR, Some(slots_request));

        let storage = extractor
            .fetch_account_storage(&fixture.block, 10, &request)
            .await?;

        // Verify that we got the storage for all requested slots
        assert_eq!(storage.len(), 3);

        // Check that each slot has a value
        for (key, value) in slots.iter() {
            println!("slot: {key:?}");
            assert!(storage.contains_key(key));
            assert_eq!(
                storage
                    .get(key)
                    .and_then(|v| v.as_ref()),
                Some(value)
            ); // Storage value exists and matches
        }

        Ok(())
    }

    #[traced_test]
    #[tokio::test]
    #[ignore = "require RPC connection"]
    async fn test_get_storage_snapshots_with_specific_slots(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let fixture = TestFixture::new().await;
        let extractor = fixture.create_batch_extractor().await?;

        // Create request with specific slots
        let slots = get_test_slots();
        let slots_request: Vec<Bytes> = slots.keys().cloned().collect();

        let requests =
            vec![TestFixture::create_storage_request(BALANCER_VAULT_STR, Some(slots_request))];

        let result = extractor
            .get_accounts_at_block(&fixture.block, &requests)
            .await?;

        assert_eq!(result.len(), 1);

        // Check the account delta
        let address = TestFixture::create_address(BALANCER_VAULT_STR);
        let delta = result
            .get(&address)
            .expect("address should exist");

        assert_eq!(delta.address, address);
        assert_eq!(delta.chain, Chain::Ethereum);
        assert!(delta.code.is_some());
        assert!(delta.balance.is_some());

        // Check that storage slots match what we requested
        assert_eq!(delta.slots.len(), 3);
        for (key, value) in slots.iter() {
            assert!(delta.slots.contains_key(key));
            assert_eq!(
                delta
                    .slots
                    .get(key)
                    .and_then(|v| v.as_ref()),
                Some(value)
            );
        }

        Ok(())
    }

    #[traced_test]
    #[tokio::test]
    #[ignore = "require RPC connection"]
    async fn test_get_storage_snapshots_with_empty_slot() -> Result<(), Box<dyn std::error::Error>>
    {
        let fixture = TestFixture::new().await;
        let extractor = fixture.create_batch_extractor().await?;

        // Try to get a slot that was not initialized / is empty
        let slots_request: Vec<Bytes> = vec![Bytes::from_str(
            "0000000000000000000000000000000000000000000000000000000000000002",
        )
        .unwrap()];

        let requests = vec![TestFixture::create_storage_request(
            BALANCER_VAULT_STR,
            Some(slots_request.clone()),
        )];

        let result = extractor
            .get_accounts_at_block(&fixture.block, &requests)
            .await?;

        assert_eq!(result.len(), 1);

        // Check the account delta
        let address = TestFixture::create_address(BALANCER_VAULT_STR);
        let delta = result
            .get(&address)
            .expect("address should exist");

        assert_eq!(delta.address, address);
        assert_eq!(delta.chain, Chain::Ethereum);
        assert!(delta.code.is_some());
        assert!(delta.balance.is_some());

        // Check that storage slots match what we requested
        assert_eq!(delta.slots.len(), 1);
        assert_eq!(
            delta
                .slots
                .get(&slots_request[0])
                .unwrap(),
            &None
        );

        Ok(())
    }

    #[traced_test]
    #[tokio::test]
    #[ignore = "require RPC connection"]
    async fn test_get_storage_snapshots_multiple_accounts() -> Result<(), Box<dyn std::error::Error>>
    {
        let fixture = TestFixture::new().await;
        let extractor = fixture.create_batch_extractor().await?;

        // Create multiple requests with different token addresses
        let requests: Vec<_> = TOKEN_ADDRESSES
            .iter()
            .map(|&addr| {
                TestFixture::create_storage_request(
                    addr,
                    Some(vec![Bytes::from_str(
                        "0000000000000000000000000000000000000000000000000000000000000000",
                    )
                    .unwrap()]),
                )
            })
            .collect();

        let start_time = std::time::Instant::now();
        let result = extractor
            .get_accounts_at_block(&fixture.block, &requests)
            .await?;
        let duration = start_time.elapsed();
        println!(
            "Time taken to get storage snapshots for {} accounts: {:?}",
            requests.len(),
            duration
        );

        assert_eq!(result.len(), TOKEN_ADDRESSES.len());

        // Check each account has the required data
        for addr_str in TOKEN_ADDRESSES.iter() {
            let address = TestFixture::create_address(addr_str);
            let delta = result
                .get(&address)
                .expect("address should exist");

            assert_eq!(delta.address, address);
            assert_eq!(delta.chain, Chain::Ethereum);
            assert!(delta.code.is_some());
            assert!(delta.balance.is_some());
            assert_eq!(delta.slots.len(), 1);

            println!(
                "Address: {}, Code size: {}, Has balance: {}",
                addr_str,
                delta.code.as_ref().unwrap().len(),
                delta.balance.is_some()
            );
        }

        Ok(())
    }
}
