use ethers::abi::{ParamType, Token as AbiToken};
use ethers::types::{Bytes, H160, U256};
use peanut_internship_rust::{
    Address, BlockId, ChainClient, MAINNET_CHAIN_ID, Token, TokenAmount, TransactionRequest,
    UniswapV2Pair,
};

fn weth() -> Token {
    Token {
        address: Address::new("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(),
        symbol: "WETH".into(),
        decimals: 18,
    }
}

fn usdc() -> Token {
    Token {
        address: Address::new("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(),
        symbol: "USDC".into(),
        decimals: 6,
    }
}

#[tokio::test]
async fn amm_matches_uniswap_router_get_amounts_out_on_chain() {
    let Ok(rpc_url) = std::env::var("MAINNET_RPC_URL") else {
        // Optional integration test: skip when mainnet RPC is not configured.
        return;
    };

    let client = ChainClient::new(vec![rpc_url], 20, 1);

    let pair_address = Address::new("0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc").unwrap();
    let pair = UniswapV2Pair::from_chain(pair_address, &client)
        .await
        .expect("failed to load pair from chain");

    let amount_in: u128 = 2_000 * 10u128.pow(6);
    let local_out = pair
        .get_amount_out(amount_in, &usdc())
        .expect("local formula failed");

    let router = Address::new("0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D").unwrap();
    let selector = hex::decode("d06ca61f").unwrap();
    let path = vec![
        AbiToken::Address(H160::from_slice(usdc().address.as_eth_address().as_bytes())),
        AbiToken::Address(H160::from_slice(weth().address.as_eth_address().as_bytes())),
    ];

    let mut calldata = selector;
    calldata.extend_from_slice(&ethers::abi::encode(&[
        AbiToken::Uint(U256::from(amount_in)),
        AbiToken::Array(path),
    ]));

    let req = TransactionRequest {
        to: router,
        value: TokenAmount::eth(0u64),
        data: Bytes::from(calldata),
        nonce: None,
        gas_limit: None,
        max_fee_per_gas: None,
        max_priority_fee: None,
        chain_id: MAINNET_CHAIN_ID,
    };

    let raw = client
        .call(&req, BlockId::Latest)
        .await
        .expect("router getAmountsOut call failed");

    let decoded = ethers::abi::decode(&[ParamType::Array(Box::new(ParamType::Uint(256)))], &raw)
        .expect("failed to decode router output");
    let amounts = decoded[0]
        .clone()
        .into_array()
        .expect("amounts should be array");
    let router_out = amounts
        .last()
        .and_then(|token| token.clone().into_uint())
        .expect("router output array missing last amount")
        .as_u128();

    assert_eq!(local_out, router_out);
}
