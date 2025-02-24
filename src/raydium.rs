use std::env;

use amm_cli::AmmSwapInfoResult;
use anyhow::{anyhow, Context, Result};
use raydium_amm::state::{AmmInfo, Loadable};
use reqwest::Proxy;
use serde::{Deserialize, Serialize};
use solana_client::{
    nonblocking::rpc_client::RpcClient,
    rpc_filter::{Memcmp, RpcFilterType},
};
use solana_sdk::{
    // native_token::LAMPORTS_PER_SOL,
    instruction::Instruction,
    program_pack::Pack,
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    system_instruction,
};
use spl_associated_token_account::{
    get_associated_token_address, instruction::create_associated_token_account,
};
use spl_token::{amount_to_ui_amount, ui_amount_to_amount};
use spl_token_client::token::TokenError;
use std::{str::FromStr, sync::Arc};

use crate::{
    get_rpc_client_blocking,
    swap::{SwapDirection, SwapInType},
    token, tx,
};
use spl_token::state::Account;

use tracing::{debug, error, info};

pub const AMM_PROGRAM: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";

pub struct Raydium {
    pub client: Arc<RpcClient>,
    pub keypair: Arc<Keypair>,
    pub client_blocking: Option<Arc<solana_client::rpc_client::RpcClient>>,
    pub pool_id: Option<String>,
}

impl Raydium {
    pub fn new(client: Arc<RpcClient>, keypair: Arc<Keypair>) -> Self {
        Self {
            client,
            keypair,
            client_blocking: None,
            pool_id: None,
        }
    }

    pub fn with_blocking_client(
        &mut self,
        client: Arc<solana_client::rpc_client::RpcClient>,
    ) -> &mut Self {
        self.client_blocking = Some(client);
        self
    }

    pub fn with_pool_id(&mut self, pool_id: Option<String>) -> &mut Self {
        self.pool_id = pool_id;
        self
    }

    pub async fn swap(
        &self,
        mint_str: &str,
        amount_in: f64,
        swap_direction: SwapDirection,
        in_type: SwapInType,
        slippage: u64,
        use_jito: bool,
    ) -> Result<Vec<String>> {
        // slippage_bps = 50u64; // 0.5%
        let slippage_bps = slippage * 100;
        let owner = self.keypair.pubkey();
        let mint = Pubkey::from_str(mint_str)
            .map_err(|e| anyhow!("failed to parse mint pubkey: {}", e))?;
        let program_id = spl_token::ID;
        let native_mint = spl_token::native_mint::ID;

        let (amm_pool_id, pool_state) = get_pool_state(
            self.client_blocking.clone().unwrap(),
            self.pool_id.as_deref(),
            Some(mint_str),
        )
        .await?;
        // debug!("pool_state: {:#?}", pool_state);

        let (token_in, token_out, user_input_token, swap_base_in) = match (
            swap_direction.clone(),
            pool_state.coin_vault_mint == native_mint,
        ) {
            (SwapDirection::Buy, true) => (native_mint, mint, pool_state.coin_vault, true),
            (SwapDirection::Buy, false) => (native_mint, mint, pool_state.pc_vault, true),
            (SwapDirection::Sell, true) => (mint, native_mint, pool_state.pc_vault, true),
            (SwapDirection::Sell, false) => (mint, native_mint, pool_state.coin_vault, true),
        };

        debug!("token_in:{token_in}, token_out:{token_out}, user_input_token:{user_input_token}, swap_base_in:{swap_base_in}");

        let in_ata = get_associated_token_address(&owner, &token_in);
        let out_ata = get_associated_token_address(&owner, &token_out);

        let mut create_instruction = None;
        let mut close_instruction = None;

        let (amount_specified, amount_ui_pretty) = match swap_direction {
            SwapDirection::Buy => {
                // Create base ATA if it doesn't exist.
                match token::get_account_info(
                    self.client.clone(),
                    self.keypair.clone(),
                    &token_out,
                    &out_ata,
                )
                .await
                {
                    Ok(_) => debug!("base ata exists. skipping creation.."),
                    Err(TokenError::AccountNotFound) | Err(TokenError::AccountInvalidOwner) => {
                        info!(
                            "base ATA for mint {} does not exist. will be create",
                            token_out
                        );
                        // token::create_associated_token_account(
                        //     self.client.clone(),
                        //     self.keypair.clone(),
                        //     &token_out,
                        //     &owner,
                        // )
                        // .await?;
                        create_instruction = Some(create_associated_token_account(
                            &owner,
                            &owner,
                            &token_out,
                            &program_id,
                        ));
                    }
                    Err(error) => error!("error retrieving out ATA: {}", error),
                }

                (
                    ui_amount_to_amount(amount_in, spl_token::native_mint::DECIMALS),
                    (amount_in, spl_token::native_mint::DECIMALS),
                )
            }
            SwapDirection::Sell => {
                let in_account = token::get_account_info(
                    self.client.clone(),
                    self.keypair.clone(),
                    &token_in,
                    &in_ata,
                )
                .await?;
                let in_mint =
                    token::get_mint_info(self.client.clone(), self.keypair.clone(), &token_in)
                        .await?;
                let amount = match in_type {
                    SwapInType::Qty => ui_amount_to_amount(amount_in, in_mint.base.decimals),
                    SwapInType::Pct => {
                        let amount_in_pct = amount_in.min(1.0);
                        if amount_in_pct == 1.0 {
                            // sell all, close ata
                            info!("sell all. will be close ATA for mint {}", token_in);
                            close_instruction = Some(spl_token::instruction::close_account(
                                &program_id,
                                &in_ata,
                                &owner,
                                &owner,
                                &vec![&owner],
                            )?);
                            in_account.base.amount
                        } else {
                            (amount_in_pct * 100.0) as u64 * in_account.base.amount / 100
                        }
                    }
                };
                (
                    amount,
                    (
                        amount_to_ui_amount(amount, in_mint.base.decimals),
                        in_mint.base.decimals,
                    ),
                )
            }
        };

        let amm_program = Pubkey::from_str(AMM_PROGRAM)?;
        debug!("amm pool id: {amm_pool_id}");
        let client = get_rpc_client_blocking()?;
        let swap_info_result = amm_cli::calculate_swap_info(
            &client,
            amm_program,
            amm_pool_id,
            user_input_token,
            amount_specified,
            slippage_bps,
            swap_base_in,
        )?;
        let other_amount_threshold = swap_info_result.other_amount_threshold;

        info!("swap_info_result: {:#?}", swap_info_result);

        info!(
            "swap: {}, value: {:?} -> {}",
            token_in, amount_ui_pretty, token_out
        );
        // build instructions
        let mut instructions = vec![];
        // sol <-> wsol support
        let mut wsol_account = None;
        if token_in == native_mint || token_out == native_mint {
            // create wsol account
            let seed = &format!("{}", Keypair::new().pubkey())[..32];
            let wsol_pubkey = Pubkey::create_with_seed(&owner, seed, &spl_token::id())?;
            wsol_account = Some(wsol_pubkey);

            // LAMPORTS_PER_SOL / 100 // 0.01 SOL as rent
            // get rent
            let rent = self
                .client
                .get_minimum_balance_for_rent_exemption(Account::LEN)
                .await?;
            // if buy add amount_specified
            let total_amount = if token_in == native_mint {
                rent + amount_specified
            } else {
                rent
            };
            // create tmp wsol account
            instructions.push(system_instruction::create_account_with_seed(
                &owner,
                &wsol_pubkey,
                &owner,
                seed,
                total_amount,
                Account::LEN as u64, // 165, // Token account size
                &spl_token::id(),
            ));

            // initialize account
            instructions.push(spl_token::instruction::initialize_account(
                &spl_token::id(),
                &wsol_pubkey,
                &native_mint,
                &owner,
            )?);
        }

        if let Some(create_instruction) = create_instruction {
            instructions.push(create_instruction);
        }
        if amount_specified > 0 {
            let mut close_wsol_account_instruction = None;
            // replace native mint with tmp wsol account
            let mut final_in_ata = in_ata;
            let mut final_out_ata = out_ata;

            if let Some(wsol_account) = wsol_account {
                match swap_direction {
                    SwapDirection::Buy => {
                        final_in_ata = wsol_account;
                    }
                    SwapDirection::Sell => {
                        final_out_ata = wsol_account;
                    }
                }
                close_wsol_account_instruction = Some(spl_token::instruction::close_account(
                    &program_id,
                    &wsol_account,
                    &owner,
                    &owner,
                    &vec![&owner],
                )?);
            }

            // build swap instruction
            let build_swap_instruction = amm_swap(
                &amm_program,
                swap_info_result,
                &owner,
                &final_in_ata,
                &final_out_ata,
                amount_specified,
                other_amount_threshold,
                swap_base_in,
            )?;
            info!(
                "amount_specified: {}, other_amount_threshold: {}, wsol_account: {:?}",
                amount_specified, other_amount_threshold, wsol_account
            );
            instructions.push(build_swap_instruction);
            // close wsol account
            if let Some(close_wsol_account_instruction) = close_wsol_account_instruction {
                instructions.push(close_wsol_account_instruction);
            }
        }
        if let Some(close_instruction) = close_instruction {
            instructions.push(close_instruction);
        }
        if instructions.len() == 0 {
            return Err(anyhow!("instructions is empty, no tx required"));
        }

        tx::new_signed_and_send(&client, &self.keypair, instructions, use_jito).await
    }
}

pub fn amm_swap(
    amm_program: &Pubkey,
    result: AmmSwapInfoResult,
    user_owner: &Pubkey,
    user_source: &Pubkey,
    user_destination: &Pubkey,
    amount_specified: u64,
    other_amount_threshold: u64,
    swap_base_in: bool,
) -> Result<Instruction> {
    let swap_instruction = if swap_base_in {
        raydium_amm::instruction::swap_base_in(
            &amm_program,
            &result.pool_id,
            &result.amm_authority,
            &result.amm_open_orders,
            &result.amm_coin_vault,
            &result.amm_pc_vault,
            &result.market_program,
            &result.market,
            &result.market_bids,
            &result.market_asks,
            &result.market_event_queue,
            &result.market_coin_vault,
            &result.market_pc_vault,
            &result.market_vault_signer,
            user_source,
            user_destination,
            user_owner,
            amount_specified,
            other_amount_threshold,
        )?
    } else {
        raydium_amm::instruction::swap_base_out(
            &amm_program,
            &result.pool_id,
            &result.amm_authority,
            &result.amm_open_orders,
            &result.amm_coin_vault,
            &result.amm_pc_vault,
            &result.market_program,
            &result.market,
            &result.market_bids,
            &result.market_asks,
            &result.market_event_queue,
            &result.market_coin_vault,
            &result.market_pc_vault,
            &result.market_vault_signer,
            user_source,
            user_destination,
            user_owner,
            other_amount_threshold,
            amount_specified,
        )?
    };

    Ok(swap_instruction)
}

pub async fn get_pool_state(
    rpc_client: Arc<solana_client::rpc_client::RpcClient>,
    pool_id: Option<&str>,
    mint: Option<&str>,
) -> Result<(Pubkey, AmmInfo)> {
    if let Some(pool_id) = pool_id {
        debug!("finding pool state by pool_id: {}", pool_id);
        let amm_pool_id = Pubkey::from_str(pool_id)?;
        let pool_state =
            common::rpc::get_account::<raydium_amm::state::AmmInfo>(&rpc_client, &amm_pool_id)?
                .ok_or(anyhow!("NotFoundPool: pool state not found"))?;
        Ok((amm_pool_id, pool_state))
    } else {
        if let Some(mint) = mint {
            // find pool by mint via rpc
            if let Ok(pool_state) = get_pool_state_by_mint(rpc_client.clone(), mint).await {
                return Ok(pool_state);
            }
            // find pool by mint via raydium api
            let pool_data = get_pool_info(&spl_token::native_mint::ID.to_string(), mint).await;
            if let Ok(pool_data) = pool_data {
                let pool = pool_data
                    .get_pool()
                    .ok_or(anyhow!("NotFoundPool: pool not found in raydium api"))?;
                let amm_pool_id = Pubkey::from_str(&pool.id)?;
                debug!("finding pool state by raydium api: {}", amm_pool_id);
                let pool_state = common::rpc::get_account::<raydium_amm::state::AmmInfo>(
                    &rpc_client,
                    &amm_pool_id,
                )?
                .ok_or(anyhow!("NotFoundPool: pool state not found"))?;
                return Ok((amm_pool_id, pool_state));
            }
            Err(anyhow!("NotFoundPool: pool state not found"))
        } else {
            Err(anyhow!("NotFoundPool: pool state not found"))
        }
    }
}

pub async fn get_pool_state_by_mint(
    rpc_client: Arc<solana_client::rpc_client::RpcClient>,
    mint: &str,
) -> Result<(Pubkey, AmmInfo)> {
    debug!("finding pool state by mint: {}", mint);
    // (pc_mint, coin_mint)
    let pairs = vec![
        // pump pool
        (
            Some(spl_token::native_mint::ID),
            Pubkey::from_str(mint).ok(),
        ),
        // general pool
        (
            Pubkey::from_str(mint).ok(),
            Some(spl_token::native_mint::ID),
        ),
    ];

    let pool_len = core::mem::size_of::<raydium_amm::state::AmmInfo>() as u64;
    let amm_program = Pubkey::from_str(AMM_PROGRAM)?;
    // Find matching AMM pool from mint pairs by filter
    let mut found_pools = None;
    for (coin_mint, pc_mint) in pairs {
        debug!(
            "get_pool_state_by_mint filter: coin_mint: {:?}, pc_mint: {:?}",
            coin_mint, pc_mint
        );
        let filters = match (coin_mint, pc_mint) {
            (None, None) => Some(vec![RpcFilterType::DataSize(pool_len)]),
            (Some(coin_mint), None) => Some(vec![
                RpcFilterType::Memcmp(Memcmp::new_base58_encoded(400, &coin_mint.to_bytes())),
                RpcFilterType::DataSize(pool_len),
            ]),
            (None, Some(pc_mint)) => Some(vec![
                RpcFilterType::Memcmp(Memcmp::new_base58_encoded(432, &pc_mint.to_bytes())),
                RpcFilterType::DataSize(pool_len),
            ]),
            (Some(coin_mint), Some(pc_mint)) => Some(vec![
                RpcFilterType::Memcmp(Memcmp::new_base58_encoded(400, &coin_mint.to_bytes())),
                RpcFilterType::Memcmp(Memcmp::new_base58_encoded(432, &pc_mint.to_bytes())),
                RpcFilterType::DataSize(pool_len),
            ]),
        };
        let pools =
            common::rpc::get_program_accounts_with_filters(&rpc_client, amm_program, filters)
                .unwrap();
        if !pools.is_empty() {
            found_pools = Some(pools);
            break;
        }
    }

    match found_pools {
        Some(pools) => {
            let pool = &pools[0];
            let pool_state = raydium_amm::state::AmmInfo::load_from_bytes(&pools[0].1.data)?;
            Ok((pool.0, pool_state.clone()))
        }
        None => {
            return Err(anyhow!("NotFoundPool: pool state not found"));
        }
    }
}

// get pool info
// https://api-v3.raydium.io/pools/info/mint?mint1=So11111111111111111111111111111111111111112&mint2=EzM2d8JVpzfhV7km3tUsR1U1S4xwkrPnWkM4QFeTpump&poolType=standard&poolSortField=default&sortType=desc&pageSize=10&page=1
pub async fn get_pool_info(mint1: &str, mint2: &str) -> Result<PoolData> {
    let mut client_builder = reqwest::Client::builder();
    if let Ok(http_proxy) = env::var("HTTP_PROXY") {
        let proxy = Proxy::all(http_proxy)?;
        client_builder = client_builder.proxy(proxy);
    }
    let client = client_builder.build()?;

    let result = client
        .get("https://api-v3.raydium.io/pools/info/mint")
        .query(&[
            ("mint1", mint1),
            ("mint2", mint2),
            ("poolType", "standard"),
            ("poolSortField", "default"),
            ("sortType", "desc"),
            ("pageSize", "1"),
            ("page", "1"),
        ])
        .send()
        .await?
        .json::<PoolInfo>()
        .await
        .context("Failed to parse pool info JSON")?;
    Ok(result.data)
}
// get pool info by ids
// https://api-v3.raydium.io/pools/info/ids?ids=3RHg85W1JtKeqFQSxBfd2RX13aBFvvy6gcATkHU657mL
pub async fn get_pool_info_by_id(pool_id: &str) -> Result<PoolData> {
    let mut client_builder = reqwest::Client::builder();
    if let Ok(http_proxy) = env::var("HTTP_PROXY") {
        let proxy = Proxy::all(http_proxy)?;
        client_builder = client_builder.proxy(proxy);
    }
    let client = client_builder.build()?;

    let result = client
        .get("https://api-v3.raydium.io/pools/info/ids")
        .query(&[("ids", pool_id)])
        .send()
        .await?
        .json::<PoolData>()
        .await
        .context("Failed to parse pool info JSON")?;
    Ok(result)
}

#[derive(Debug, Deserialize)]
pub struct PoolInfo {
    pub success: bool,
    pub data: PoolData,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct PoolData {
    // pub count: u32,
    pub data: Vec<Pool>,
}

impl PoolData {
    pub fn get_pool(&self) -> Option<Pool> {
        self.data.first().cloned()
    }
}

#[derive(Debug, Deserialize, Clone, Serialize)]
pub struct Pool {
    pub id: String,
    #[serde(rename = "programId")]
    pub program_id: String,
    #[serde(rename = "mintA")]
    pub mint_a: Mint,
    #[serde(rename = "mintB")]
    pub mint_b: Mint,
    #[serde(rename = "marketId")]
    pub market_id: String,
}

#[derive(Debug, Deserialize, Clone, Serialize)]
pub struct Mint {
    pub address: String,
    pub symbol: String,
    pub name: String,
    pub decimals: u8,
}
