use std::{convert::TryFrom, str::FromStr, sync::Arc, time::Duration};

use dotenv::dotenv;
use ethers::{
    abi::{parse_abi, Token},
    prelude::{abigen, BaseContract, SignerMiddleware},
    providers::{Http, Middleware, Provider},
    signers::{LocalWallet, Signer},
    types::{Address, Bytes, Chain, U256},
    utils::Anvil,
};

abigen!(Liquidations, "abis/Liquidations.json");

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenv().ok();

    let rpc_node_url = std::env::var("ALCHEMY_POLYGON_RPC_URL")?;

    let wallet = std::env::var("PRIVATE_KEY")?
        .parse::<LocalWallet>()?
        .with_chain_id(137_u64);
    let provider = Provider::<Http>::try_from(rpc_node_url)?;
    let provider = Arc::new(provider);

    // 4. instantiate the client with the wallet
    let client = Arc::new(SignerMiddleware::new(provider.clone(), wallet));

    let price = provider.clone().get_gas_price().await?;
    let price = U256::from(20_650_000);

    let _deploy_txn = Liquidations::deploy(
        client,
        "0x794a61358D6845594F94dc1DB02A252b5b4814aD"
            .parse::<Address>()
            .unwrap(),
    )
    .unwrap();

    let gas = provider
        .clone()
        .estimate_gas(&_deploy_txn.deployer.tx, None)
        .await?;

    let resp = _deploy_txn.gas(price).send().await?;

    // let mut t = _deploy_txn.clone();
    // t.gas();

    // .gas(price)
    // .send()
    // .await
    // .unwrap();

    // let mut tx = deploy_call.clone();

    // .send()
    // .await
    // .unwrap();

    Ok(())
}