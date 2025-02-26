use std::{env, str::FromStr, sync::Arc};

use axum::{
    debug_handler,
    extract::{Path, State},
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use serde_json::json;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{pubkey::Pubkey, signature::Keypair, signer::Signer};
use tracing::{error, info, warn};

use crate::{
    constants::Symbol,
    get_rpc_client, get_rpc_client_blocking,
    helper::{api_error, api_ok},
    pump::{get_pump_info, Pump, PumpInfo},
    raydium::{get_pool_info, Raydium},
    swap::{self, SwapDirection, SwapInType},
    token,
};

#[derive(Clone)]
pub struct AppState {
    pub client: Arc<RpcClient>,
    pub client_blocking: Arc<solana_client::rpc_client::RpcClient>,
    pub wallet: Arc<Keypair>,
}

#[derive(Debug, Deserialize)]
pub struct CreateSwap {
    mint: String,
    direction: SwapDirection,
    amount_in: f64,
    in_type: Option<SwapInType>,
    slippage: Option<u64>,
    jito: Option<bool>,
}

#[debug_handler]
pub async fn swap(
    State(state): State<AppState>,
    Json(input): Json<CreateSwap>,
) -> impl IntoResponse {
    let slippage = match input.slippage {
        Some(v) => v,
        None => {
            let slippage = env::var("SLIPPAGE").unwrap_or("5".to_string());
            let slippage = slippage.parse::<u64>().unwrap_or(5);
            slippage
        }
    };

    info!("{:?}, slippage: {}", input, slippage);

    let result = swap::swap(
        state,
        input.mint.as_str(),
        input.amount_in,
        input.direction.clone(),
        input.in_type.unwrap_or(SwapInType::Qty),
        slippage,
        input.jito.unwrap_or(false),
    )
    .await;
    match result {
        Ok(txs) => api_ok(txs),
        Err(err) => {
            warn!("swap err: {:#?}", err);
            api_error(&err.to_string())
        }
    }
}

#[debug_handler]
pub async fn get_pool(
    State(state): State<AppState>,
    Path(pool_id): Path<String>,
) -> impl IntoResponse {
    let client = match get_rpc_client() {
        Ok(client) => client,
        Err(err) => {
            return api_error(&format!("failed to get rpc client: {err}"));
        }
    };
    let client_blocking = match get_rpc_client_blocking() {
        Ok(client) => client,
        Err(err) => {
            return api_error(&format!("failed to get rpc client: {err}"));
        }
    };
    let wallet = state.wallet;
    let mut swapx = Raydium::new(client, wallet);
    swapx.with_blocking_client(client_blocking);
    match swapx.get_pool(pool_id.as_str()).await {
        Ok(data) => api_ok(json!({
            "base": data.0,
            "quote": data.1,
            "price": data.2,
            "usd_price": data.3,
            "sol_price": data.4,
        })),
        Err(err) => {
            warn!("get pool err: {:#?}", err);
            api_error(&err.to_string())
        }
    }
}

#[debug_handler]
pub async fn get_pool_by_token_address(
    State(_state): State<AppState>,
    Path(token_address): Path<String>,
) -> impl IntoResponse {
    let pool_data = get_pool_info(&token_address, Symbol::WSOL_TOKEN).await;
    info!("get_pool_by_token_address: {:#?}", pool_data);
    match pool_data {
        Ok(data) => api_ok(json!(data)),
        Err(err) => {
            warn!("get swap pool by token address err: {:#?}", err);
            api_error(&err.to_string())
        }
    }
}

#[debug_handler]
pub async fn get_raydium_token_price(
    State(state): State<AppState>,
    Path(token_address): Path<String>,
) -> impl IntoResponse {
    let pool_data = get_pool_info(&token_address, Symbol::WSOL_TOKEN).await;
    info!("get_pool_by_token_address: {:#?}", pool_data);
    match pool_data {
        Ok(data) => {
            match data.get_pool() {
                Some(pool) => {
                    let mut swapx = Raydium::new(state.client.clone(), state.wallet.clone());
                    swapx.with_blocking_client(state.client_blocking.clone());
                    let price = swapx.get_pool_price(Some(&pool.id), None).await;
                    match price {
                        Ok(raydium_info) => api_ok(json!(raydium_info)),
                        Err(err) => {
                            error!("get pool price err: {:#?}", err);
                            api_error(&err.to_string())
                        }
                    }
                }
                None => {
                    // warn!("get pool err: {:#?}", err);
                    // api_error(&err.to_string())
                    api_error("pool not found")
                }
            }
        }
        Err(err) => {
            warn!("get swap pool by token address err: {:#?}", err);
            api_error(&err.to_string())
        }
    }
}

#[debug_handler]
pub async fn get_pump_token_price(
    State(state): State<AppState>,
    Path(token_address): Path<String>,
) -> impl IntoResponse {
    let mut swapx = Pump::new(state.client.clone(), state.wallet.clone());
    swapx.with_blocking_client(state.client_blocking.clone());
    match swapx.get_pump_price(&token_address).await {
        Ok(data) => api_ok(json!({
            "base_amount": data.0,
            "quote_amount": data.1,
            "price": data.2,
        })),
        Err(err) => {
            warn!("get pump token {token_address} price err: {:#?}", err);
            api_error(&err.to_string())
        }
    }
}

pub async fn get_coin_info(wallet: Arc<Keypair>, mint: &String) -> Result<PumpInfo, String> {
    let client = match get_rpc_client() {
        Ok(client) => client,
        Err(err) => {
            return Err(format!("failed to get rpc client: {err}"));
        }
    };
    let client_blocking = match get_rpc_client_blocking() {
        Ok(client) => client,
        Err(err) => {
            return Err(format!("failed to get rpc client: {err}"));
        }
    };
    // query from pump.fun
    let mut pump_info = match get_pump_info(client_blocking.clone(), &mint).await {
        Ok(info) => info,
        Err(err) => {
            return Err(err.to_string());
        }
    };
    if pump_info.complete {
        let mut swapx = Raydium::new(client, wallet);
        swapx.with_blocking_client(client_blocking);
        match swapx.get_pool_price(None, Some(mint.as_str())).await {
            Ok(raydium_info) => {
                pump_info.raydium_info = Some(raydium_info);
            }
            Err(err) => {
                warn!("get raydium pool price err: {:#?}", err);
            }
        }
    }
    Ok(pump_info)
}

pub async fn coins(State(state): State<AppState>, Path(mint): Path<String>) -> impl IntoResponse {
    match get_coin_info(state.wallet, &mint).await {
        Ok(pump_info) => {
            return api_ok(pump_info);
        }
        Err(err_msg) => {
            return api_error(&err_msg);
        }
    }
}

#[debug_handler]
pub async fn token_accounts(State(state): State<AppState>) -> impl IntoResponse {
    let client = match get_rpc_client() {
        Ok(client) => client,
        Err(err) => {
            return api_error(&format!("failed to get rpc client: {err}"));
        }
    };
    let wallet = state.wallet;

    let token_accounts = token::token_accounts(&client, &wallet.pubkey()).await;

    match token_accounts {
        Ok(token_accounts) => api_ok(token_accounts),
        Err(err) => {
            warn!("get token_accounts err: {:#?}", err);
            api_error(&err.to_string())
        }
    }
}

#[debug_handler]
pub async fn token_account(
    State(state): State<AppState>,
    Path(mint): Path<String>,
) -> impl IntoResponse {
    let client = match get_rpc_client() {
        Ok(client) => client,
        Err(err) => {
            return api_error(&format!("failed to get rpc client: {err}"));
        }
    };
    let wallet = state.wallet;

    let mint = if let Ok(mint) = Pubkey::from_str(mint.as_str()) {
        mint
    } else {
        return api_error("invalid mint pubkey");
    };

    let token_account = token::token_account(&client, &wallet.pubkey(), mint).await;

    match token_account {
        Ok(token_account) => api_ok(token_account),
        Err(err) => {
            warn!("get token_account err: {:#?}", err);
            api_error(&err.to_string())
        }
    }
}
