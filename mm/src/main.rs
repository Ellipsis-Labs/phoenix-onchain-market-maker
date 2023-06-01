use anchor_lang::InstructionData;
use anchor_lang::ToAccountMetas;
use anyhow::anyhow;
use clap::Parser;
use phoenix::program::get_seat_address;
use phoenix::program::get_vault_address;
use phoenix::program::MarketHeader;
use phoenix_onchain_mm::OrderParams;
use phoenix_onchain_mm::PriceImprovementBehavior;
use phoenix_onchain_mm::StrategyParams;
use solana_cli_config::{Config, ConfigInput, CONFIG_FILE};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::read_keypair_file;
use solana_sdk::signer::keypair::Keypair;
use solana_sdk::signer::Signer;
use spl_associated_token_account::get_associated_token_address;
use std::str::FromStr;

pub fn get_network(network_str: &str) -> &str {
    match network_str {
        "devnet" | "dev" | "d" => "https://api.devnet.solana.com",
        "mainnet" | "main" | "m" | "mainnet-beta" => "https://api.mainnet-beta.solana.com",
        "localnet" | "localhost" | "l" | "local" => "http://localhost:8899",
        _ => network_str,
    }
}

pub fn get_payer_keypair_from_path(path: &str) -> anyhow::Result<Keypair> {
    read_keypair_file(&*shellexpand::tilde(path)).map_err(|e| anyhow!(e.to_string()))
}

#[derive(Parser, Debug)]
#[clap(version, about)]
struct Arguments {
    /// Optionally include your keypair path. Defaults to your Solana CLI config file.
    #[clap(global = true, short, long)]
    keypair_path: Option<String>,
    /// Optionally include your RPC endpoint. Use "local", "dev", "main" for default endpoints. Defaults to your Solana CLI config file.
    #[clap(global = true, short, long)]
    url: Option<String>,
    /// Optionally include a commitment level. Defaults to your Solana CLI config file.
    #[clap(global = true, short, long)]
    commitment: Option<String>,
    /// Market pubkey to provide on
    market: Pubkey,
    // The ticker is used to pull the price from the Coinbase API, and therefore should conform to the Coinbase ticker format.
    /// Note that for all USDC quoted markets, the price feed should use "USD" instead of "USDC".
    #[clap(short, long, default_value = "SOL-USD")]
    ticker: String,
    #[clap(long, default_value = "2000")]
    quote_refresh_frequency_in_ms: u64,
    #[clap(long, default_value = "3")]
    quote_edge_in_bps: u64,
    #[clap(long, default_value = "100000000")]
    quote_size: u64,
    #[clap(long, default_value = "join")]
    price_improvement_behavior: String,
    #[clap(long, default_value = "true")]
    post_only: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FaucetMetadata {
    pub spec_pubkey: Pubkey,
    pub faucet_pubkey: Pubkey,
    pub difficulty: u8,
    pub amount: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Arguments::parse();
    let config = match CONFIG_FILE.as_ref() {
        Some(config_file) => Config::load(config_file).unwrap_or_else(|_| {
            println!("Failed to load config file: {}", config_file);
            Config::default()
        }),
        None => Config::default(),
    };
    let commitment =
        ConfigInput::compute_commitment_config("", &cli.commitment.unwrap_or(config.commitment)).1;
    let payer = get_payer_keypair_from_path(&cli.keypair_path.unwrap_or(config.keypair_path))?;
    let network_url = &get_network(&cli.url.unwrap_or(config.json_rpc_url)).to_string();
    let client = RpcClient::new_with_commitment(network_url.to_string(), commitment);

    let sdk = phoenix_sdk::sdk_client::SDKClient::new(&payer, network_url).await?;

    let Arguments {
        market,
        ticker,
        quote_edge_in_bps,
        quote_size,
        quote_refresh_frequency_in_ms,
        price_improvement_behavior,
        post_only,
        ..
    } = cli;

    let maker_setup_instructions = sdk.get_maker_setup_instructions_for_market(&market).await?;
    sdk.client
        .sign_send_instructions(maker_setup_instructions, vec![])
        .await
        .unwrap();

    let strategy_key = Pubkey::find_program_address(
        &[b"phoenix", payer.pubkey().as_ref(), market.as_ref()],
        &phoenix_onchain_mm::id(),
    )
    .0;

    let mut create = false;
    match client.get_account(&strategy_key).await {
        Ok(acc) => {
            if acc.data.is_empty() {
                create = true;
            }
        }
        Err(_) => {
            create = true;
        }
    }

    let price_improvement = match price_improvement_behavior.as_str() {
        "Join" | "join" => PriceImprovementBehavior::Join,
        "Dime" | "dime" => PriceImprovementBehavior::Dime,
        "Ignore" | "ignore" => PriceImprovementBehavior::Ignore,
        _ => PriceImprovementBehavior::Join,
    };

    let params = StrategyParams {
        quote_edge_in_bps: Some(quote_edge_in_bps),
        quote_size_in_quote_atoms: Some(quote_size),
        price_improvement_behavior: Some(price_improvement),
        post_only: Some(post_only),
    };
    if create {
        let initialize_data = phoenix_onchain_mm::instruction::Initialize { params };
        let initialize_accounts = phoenix_onchain_mm::accounts::Initialize {
            phoenix_strategy: strategy_key,
            market,
            user: payer.pubkey(),
            system_program: solana_sdk::system_program::id(),
        };

        let ix = Instruction {
            program_id: phoenix_onchain_mm::id(),
            accounts: initialize_accounts.to_account_metas(None),
            data: initialize_data.data(),
        };

        let transaction = solana_sdk::transaction::Transaction::new_signed_with_payer(
            &[ix],
            Some(&payer.pubkey()),
            &[&payer],
            client.get_latest_blockhash().await?,
        );
        let txid = client.send_and_confirm_transaction(&transaction).await?;
        println!("Creating strategy account: {}", txid);
    }

    let data = client.get_account_data(&market).await?;
    let header =
        bytemuck::try_from_bytes::<MarketHeader>(&data[..std::mem::size_of::<MarketHeader>()])
            .map_err(|_| anyhow::Error::msg("Failed to parse Phoenix market header"))?;

    println!("Quote Params: {:#?}", params);

    loop {
        let fair_price = {
            let response = reqwest::get(format!(
                "https://api.coinbase.com/v2/prices/{}/spot",
                ticker
            ))
            .await?
            .json::<serde_json::Value>()
            .await?;

            f64::from_str(response["data"]["amount"].as_str().unwrap())?
        };

        println!("Fair price: {}", fair_price);

        let args = phoenix_onchain_mm::instruction::UpdateQuotes {
            params: OrderParams {
                fair_price_in_quote_atoms_per_raw_base_unit: (fair_price * 1e6) as u64,
                strategy_params: params,
            },
        };

        let accounts = phoenix_onchain_mm::accounts::UpdateQuotes {
            phoenix_strategy: strategy_key,
            market,
            user: payer.pubkey(),
            phoenix_program: phoenix::id(),
            log_authority: phoenix::phoenix_log_authority::id(),
            seat: get_seat_address(&market, &payer.pubkey()).0,
            quote_account: get_associated_token_address(
                &payer.pubkey(),
                &header.quote_params.mint_key,
            ),
            base_account: get_associated_token_address(
                &payer.pubkey(),
                &header.base_params.mint_key,
            ),
            quote_vault: get_vault_address(&market, &header.quote_params.mint_key).0,
            base_vault: get_vault_address(&market, &header.base_params.mint_key).0,
            token_program: spl_token::id(),
        };

        let ix = Instruction {
            program_id: phoenix_onchain_mm::id(),
            accounts: accounts.to_account_metas(None),
            data: args.data(),
        };

        let transaction = solana_sdk::transaction::Transaction::new_signed_with_payer(
            &[ix],
            Some(&payer.pubkey()),
            &[&payer],
            client.get_latest_blockhash().await?,
        );
        if client
            .send_and_confirm_transaction(&transaction)
            .await
            .and_then(|sig| {
                println!("Updating quotes: {}", sig);
                Ok(())
            })
            .is_err()
        {
            println!("Failed to update quotes");
        };

        tokio::time::sleep(std::time::Duration::from_millis(
            quote_refresh_frequency_in_ms,
        ))
        .await;
    }
}
