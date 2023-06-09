use dotenv::dotenv;
use ethers::prelude::k256::ecdsa::SigningKey;
use ethers::prelude::{abigen, SignerMiddleware};
use ethers::providers::Ipc;
use ethers::signers::{LocalWallet, Signer, Wallet};
use ethers::types::{BigEndianHash, BlockNumber, H256, H64};
use ethers::types::{Transaction, TxHash, U64};
use ethers::utils::{hex, rlp};
use ethers::{
    providers::{Middleware, Provider},
    types::{Address, Bytes, GethDebugTracingOptions, TransactionRequest, U256},
    utils,
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use std::{sync::Arc, time::Instant};
use tsuki::constants::protocol::UniswapV2;
use tsuki::constants::token::ERC20Token;
use tsuki::tx_pool::TxPool;
use tsuki::uniswapV2::UniswapV2Client;
use tsuki::utils::batch::common::BatchRequest;
use tsuki::utils::batch::BatchProvider;
use tsuki::utils::block::{self, Block, PartialHeader};
use tsuki::utils::transaction::{
    build_typed_transaction, EIP1559Transaction, EIP2930Transaction, EthTransactionRequest,
    TypedTransaction,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TraceConfig {
    pub disable_storage: bool,
    pub disable_stack: bool,
    pub enable_memory: bool,
    pub enable_return_data: bool,
    pub tracer: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tracer_config: Option<TracerConfig>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TracerConfig {
    pub only_top_call: bool,
    pub with_log: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Res {
    pub result: BlockTraceResult,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct BlockTraceResult {
    pub from: Address,
    pub gas: U256,
    pub gas_used: U256,
    pub input: Bytes,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Bytes>,
    pub to: Address,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<U256>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calls: Option<Vec<BlockTraceResult>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TxpoolEntry {
    pub hash: H256,
    pub gas_price: U256,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TxpoolContent {
    pub pending: HashMap<Address, HashMap<U256, TxpoolEntry>>,
    pub queued: HashMap<Address, HashMap<U256, TxpoolEntry>>,
}

abigen!(
    ERC20,
    r#"[
        approve(address spender, uint256 amount) external returns (bool)
    ]"#,
);

fn gen_txn(
    txn: ethers::types::transaction::eip2718::TypedTransaction,
    to: Address,
    signer_client: SignerMiddleware<Arc<Provider<Ipc>>, Wallet<SigningKey>>,
    gas_price: U256,
    nonce: U256,
) -> TypedTransaction {
    let txn = txn.as_eip1559_ref().unwrap();

    let txn_req: EthTransactionRequest = tsuki::utils::transaction::EthTransactionRequest {
        from: Some(signer_client.address()),
        to: Some(to),
        gas_price: None,
        max_fee_per_gas: Some(gas_price),
        max_priority_fee_per_gas: Some(gas_price),
        gas: Some(500_000.into()),
        value: Some(0.into()),
        data: txn.data.clone(),
        nonce: Some(nonce),
        access_list: None,
        transaction_type: None,
    };

    let ttr = txn_req.into_typed_request().unwrap();
    let mut ethers_ttr: ethers::types::transaction::eip2718::TypedTransaction = ttr.clone().into();
    ethers_ttr.set_from(signer_client.address());
    ethers_ttr.set_chain_id(137);
    let signature = signer_client.signer().sign_transaction_sync(&ethers_ttr);
    return build_typed_transaction(ttr, signature);
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let provider_ipc = Provider::connect_ipc("/home/user/.bor/data/bor.ipc").await?;
    let provider_ipc = Arc::new(provider_ipc);
    let batch_provider_ipc = BatchProvider::connect_ipc("/home/user/.bor/data/bor.ipc").await?;
    let txpool = TxPool::init(provider_ipc.clone(), 1000);
    let txpool = Arc::new(txpool);
    tokio::spawn(txpool.clone().stream_mempool());
    tokio::time::sleep(Duration::from_secs(2)).await;
    let transactions = txpool.get_mempool().await;
    let mut batch = BatchRequest::new();
    for txn in &transactions {
        batch
            .add_request("eth_getTransactionCount", (txn.from, "latest"))
            .unwrap();
    }
    let mut i = 0;
    let mut responses = batch_provider_ipc.execute_batch(&mut batch).await?;
    while let Some(Ok(num)) = responses.next_response::<U256>() {
        println!("{:?}:{}", transactions[i].from, num);
        i += 1;
    }
    Ok(())
}

async fn old_main() -> Result<(), Box<dyn std::error::Error>> {
    dotenv().ok();
    let provider_ipc = Provider::connect_ipc("/home/user/.bor/data/bor.ipc").await?;
    let provider_ipc = Arc::new(provider_ipc);

    let wallet = std::env::var("PRIVATE_KEY")
        .unwrap()
        .parse::<LocalWallet>()
        .unwrap()
        .with_chain_id(137u64);
    let signer_client = SignerMiddleware::new(provider_ipc.clone(), wallet);

    // generate one transaction, see what happens
    let uniswap_client = UniswapV2Client::new(provider_ipc.clone());

    let nonce = signer_client
        .get_transaction_count(signer_client.address(), None)
        .await?;
    let gas_price = provider_ipc.get_gas_price().await?;

    let token_contract = ERC20::new(ERC20Token::USDC.get_address(), provider_ipc.clone());
    let approve_tx = token_contract.approve(
        UniswapV2::SUSHISWAP.get_router_address(),
        U256::from(1_000_000),
    );

    println!("{:?}", approve_tx.tx);

    let swap_tx = uniswap_client.get_swapExactTokensForTokens_txn(
        UniswapV2::SUSHISWAP,
        tsuki::constants::token::ERC20Token::USDC,
        tsuki::constants::token::ERC20Token::USDT,
        U256::from(1_000_000),
    );

    let approve_tx = gen_txn(
        approve_tx.tx,
        ERC20Token::USDC.get_address(),
        signer_client.clone(),
        gas_price,
        nonce + 1,
    );
    let swap_tx = gen_txn(
        swap_tx.tx,
        UniswapV2::SUSHISWAP.get_router_address(),
        signer_client,
        gas_price,
        nonce,
    );

    let block_number = provider_ipc.get_block_number().await?.as_u64();
    let block_number = utils::serialize(&block_number);

    let bytes = provider_ipc
        .request::<_, Bytes>("debug_getBlockRlp", [block_number])
        .await?;

    let block: Block = rlp::decode(&bytes)?;
    let mut txns = block.transactions;
    txns.push(swap_tx);
    txns.push(approve_tx);
    let sim_block: Block = Block::new(block.header.into(), txns, block.ommers);

    let sim_block_rlp = rlp::encode(&sim_block);
    let sim_block_rlp = ["0x", &hex::encode(sim_block_rlp)].join("");
    let sim_block_rlp = utils::serialize(&sim_block_rlp);

    let config = TraceConfig {
        disable_storage: true,
        disable_stack: true,
        enable_memory: false,
        enable_return_data: false,
        tracer: "callTracer".to_string(),
        tracer_config: Some(TracerConfig {
            only_top_call: true,
            with_log: false,
        }),
    };
    let config = utils::serialize(&config);

    let now = Instant::now();
    let result = provider_ipc
        .request::<_, Vec<Res>>("debug_traceBlock", [sim_block_rlp, config])
        .await?;
    println!("Time elapsed: {}ms", now.elapsed().as_millis());

    println!("Number in result: {:?}", result.len());
    println!("{:?}", result[result.len() - 2]);
    println!("{:?}", result[result.len() - 1]);
    Ok(())
}

async fn txpool() -> Result<(), Box<dyn std::error::Error>> {
    let provider_ipc = Provider::connect_ipc("/home/user/.bor/data/bor.ipc").await?;
    let provider_ipc = Arc::new(provider_ipc);
    let txpool = TxPool::init(provider_ipc.clone(), 1000);
    let txpool = Arc::new(txpool);
    txpool.stream_mempool().await;
    Ok(())
}

#[tokio::main]
async fn txpool_content() -> Result<(), Box<dyn std::error::Error>> {
    let provider_ipc = Provider::connect_ipc("/home/user/.bor/data/bor.ipc").await?;
    let provider_ipc = Arc::new(provider_ipc);

    let mut block_stream = provider_ipc.subscribe_blocks().await.unwrap();
    let start_block_num = provider_ipc.get_block_number().await?;
    let mut pending_txn_hashs = HashSet::<H256>::new();
    let mut gas_prices = Vec::<U256>::new();
    let mut mapping: HashMap<H256, U256> = HashMap::new();

    while let Some(block) = block_stream.next().await {
        if block.number.unwrap() == start_block_num + 2 {
            // pull mempool transactions
            let content = provider_ipc
                .request::<_, TxpoolContent>("txpool_content", ())
                .await?;
            let pending = content.pending;
            for (_address, nonce_map) in pending {
                for (_nonce, entry) in nonce_map {
                    pending_txn_hashs.insert(entry.hash);
                    gas_prices.push(entry.gas_price);
                    mapping.insert(entry.hash, entry.gas_price);
                }
            }
        } else if block.number.unwrap() == start_block_num + 3 {
            let mut local_gas_prices = Vec::<U256>::new();
            let block = provider_ipc
                .get_block(block.number.unwrap())
                .await?
                .unwrap();
            let txns = block.transactions;
            let num_txns = txns.len();
            let mut num_txns_in_mempool = 0;
            for txn_hash in txns {
                if pending_txn_hashs.contains(&txn_hash) {
                    num_txns_in_mempool += 1;
                    local_gas_prices.push(mapping[&txn_hash]);
                }
            }
            println!(
                "{}/{} transactions from mempool were in mined blocked.",
                num_txns_in_mempool, num_txns
            );
            gas_prices.sort();
            gas_prices.reverse();
            println!("--------------");
            println!("Local gas prices: {:?}", local_gas_prices);
            println!("______________");
            println!(
                "Mempool gas prices: {:?}",
                gas_prices[0..local_gas_prices.len()].to_vec()
            );

            break;
        }
    }
    Ok(())
}

async fn debug_traceBlock() -> Result<(), Box<dyn std::error::Error>> {
    let provider_ipc = Provider::connect_ipc("/home/user/.bor/data/bor.ipc").await?;
    let provider_ipc = Arc::new(provider_ipc);

    let block_number = provider_ipc.get_block_number().await?.as_u64();
    let block_number = utils::serialize(&block_number);

    let bytes = provider_ipc
        .request::<_, Bytes>("debug_getBlockRlp", [block_number])
        .await?;

    let block: Block = rlp::decode(&bytes)?;
    println!("Number of txns: {:?}", block.transactions.len());
    // let block_rlp = rlp::encode(&block);
    let block_rlp = ["0x", &hex::encode(bytes)].join("");
    // println!("{:?}", block_rlp);
    let block_rlp = utils::serialize(&block_rlp);

    let config = TraceConfig {
        disable_storage: true,
        disable_stack: true,
        enable_memory: false,
        enable_return_data: false,
        tracer: "callTracer".to_string(),
        tracer_config: Some(TracerConfig {
            only_top_call: true,
            with_log: false,
        }),
    };
    let config = utils::serialize(&config);

    let result = provider_ipc
        .request::<_, Vec<Res>>("debug_traceBlock", [block_rlp, config])
        .await?;

    println!("Number in result: {:?}", result.len());
    println!("{:?}", result);

    Ok(())
}

async fn debug_traceBlockByNumber() -> Result<(), Box<dyn std::error::Error>> {
    let provider_ipc = Provider::connect_ipc("/home/user/.bor/data/bor.ipc").await?;
    let provider_ipc = Arc::new(provider_ipc);

    let block_number = provider_ipc.get_block_number().await?;
    let config = TraceConfig {
        disable_storage: true,
        disable_stack: true,
        enable_memory: false,
        enable_return_data: false,
        tracer: "callTracer".to_string(),
        tracer_config: Some(TracerConfig {
            only_top_call: true,
            with_log: false,
        }),
    };
    let mut results = vec![];
    let now = Instant::now();
    for i in 0..4 {
        let block_number = utils::serialize(&(block_number - i));
        let config = utils::serialize(&config);
        results.push(provider_ipc.request::<_, Vec<BlockTraceResult>>(
            "debug_traceBlockByNumber",
            [block_number, config],
        ));
    }
    for result in results {
        let _res = result.await?;
    }
    println!("TIME ELAPSED: {:?}ms", now.elapsed().as_millis());
    Ok(())
}
