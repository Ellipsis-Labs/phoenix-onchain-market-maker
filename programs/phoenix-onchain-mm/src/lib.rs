use anchor_lang::{
    __private::bytemuck::{self},
    prelude::*,
    solana_program::program::invoke,
};
use phoenix::{
    program::MarketHeader,
    quantities::{Ticks, WrapperU64},
    state::{
        markets::{FIFOOrderId, FIFORestingOrder, Market},
        OrderPacket, Side,
    },
};

declare_id!("MM1BW8uAmQ1zXP8mi8izfGQfjB1ASZhh93Tteo9LUfW");

#[derive(Clone)]
pub struct PhoenixV1;

impl anchor_lang::Id for PhoenixV1 {
    fn id() -> Pubkey {
        phoenix::id()
    }
}
pub const PHOENIX_MARKET_DISCRIMINANT: u64 = 8167313896524341111;

fn load_header(info: &AccountInfo) -> Result<MarketHeader> {
    require!(
        info.owner == &phoenix::id(),
        StrategyError::InvalidPhoenixProgram
    );
    let data = info.data.borrow();
    let header =
        bytemuck::try_from_bytes::<MarketHeader>(&data[..std::mem::size_of::<MarketHeader>()])
            .map_err(|_| {
                msg!("Failed to parse Phoenix market header");
                StrategyError::FailedToDeserializePhoenixMarket
            })?;
    require!(
        header.discriminant == PHOENIX_MARKET_DISCRIMINANT,
        StrategyError::InvalidPhoenixProgram,
    );
    Ok(*header)
}

fn get_best_bid_and_ask(
    market: &dyn Market<Pubkey, FIFOOrderId, FIFORestingOrder, OrderPacket>,
    trader_index: u64,
) -> (u64, u64) {
    let best_bid = market
        .get_book(Side::Bid)
        .iter()
        .find(|(_, o)| o.trader_index != trader_index)
        .map(|(o, _)| o.price_in_ticks.as_u64())
        .unwrap_or_else(|| 1);
    let best_ask = market
        .get_book(Side::Ask)
        .iter()
        .find(|(_, o)| o.trader_index != trader_index)
        .map(|(o, _)| o.price_in_ticks.as_u64())
        .unwrap_or_else(|| u64::MAX);
    (best_bid, best_ask)
}

fn get_bid_price(
    fair_price_in_quote_atoms_per_raw_base_unit: u64,
    header: &MarketHeader,
    edge_in_bps: u64,
) -> u64 {
    let fair_price_in_ticks = fair_price_in_quote_atoms_per_raw_base_unit
        * header.raw_base_units_per_base_unit as u64
        / header.get_tick_size_in_quote_atoms_per_base_unit().as_u64();
    let edge_in_ticks = edge_in_bps * fair_price_in_ticks / 10_000;
    fair_price_in_ticks - edge_in_ticks
}

fn get_ask_price(
    fair_price_in_quote_atoms_per_raw_base_unit: u64,
    header: &MarketHeader,
    edge_in_bps: u64,
) -> u64 {
    let fair_price_in_ticks = fair_price_in_quote_atoms_per_raw_base_unit
        * header.raw_base_units_per_base_unit as u64
        / header.get_tick_size_in_quote_atoms_per_base_unit().as_u64();
    let edge_in_ticks = edge_in_bps * fair_price_in_ticks / 10_000;
    fair_price_in_ticks + edge_in_ticks
}

#[derive(Debug, AnchorSerialize, AnchorDeserialize, Clone, Copy)]
pub enum PriceImprovementBehavior {
    Join,
    Dime,
    Ignore,
}

impl PriceImprovementBehavior {
    pub fn to_u8(&self) -> u8 {
        match self {
            PriceImprovementBehavior::Join => 0,
            PriceImprovementBehavior::Dime => 1,
            PriceImprovementBehavior::Ignore => 2,
        }
    }

    pub fn from_u8(byte: u8) -> Self {
        match byte {
            0 => PriceImprovementBehavior::Join,
            1 => PriceImprovementBehavior::Dime,
            2 => PriceImprovementBehavior::Ignore,
            _ => panic!("Invalid PriceImprovementBehavior"),
        }
    }
}

#[account(zero_copy)]
pub struct PhoenixStrategyState {
    pub trader: Pubkey,
    pub market: Pubkey,
    // Order parameters
    pub bid_order_sequence_number: u64,
    pub bid_price_in_ticks: u64,
    pub initial_bid_size_in_base_lots: u64,
    pub ask_order_sequence_number: u64,
    pub ask_price_in_ticks: u64,
    pub initial_ask_size_in_base_lots: u64,
    pub last_update_slot: u64,
    pub last_update_unix_timestamp: i64,
    // Strategy parameters
    /// Number of basis points betweeen quoted price and fair price
    pub quote_edge_in_bps: u64,
    /// Order notional size in quote atoms
    pub quote_size_in_quote_atoms: u64,
    /// If set to true, the orders will never cross the spread
    pub post_only: bool,
    /// Determines whether/how to improve BBO
    pub price_improvement_behavior: u8,
    padding: [u8; 6],
}

#[derive(Debug, AnchorDeserialize, AnchorSerialize, Clone, Copy)]
pub struct OrderParams {
    pub fair_price_in_quote_atoms_per_raw_base_unit: u64,
    pub strategy_params: StrategyParams,
}

#[derive(Debug, AnchorDeserialize, AnchorSerialize, Clone, Copy)]
pub struct StrategyParams {
    pub quote_edge_in_bps: Option<u64>,
    pub quote_size_in_quote_atoms: Option<u64>,
    pub price_improvement_behavior: Option<PriceImprovementBehavior>,
    pub post_only: bool,
}

#[program]
pub mod phoenix_onchain_mm {
    use phoenix::{
        program::{
            new_order::{CondensedOrder, MultipleOrderPacket},
            CancelMultipleOrdersByIdParams, CancelOrderParams,
        },
        quantities::BaseLots,
        state::Side,
    };

    use super::*;

    pub fn initialize(ctx: Context<Initialize>, params: StrategyParams) -> Result<()> {
        require!(
            params.quote_edge_in_bps.is_some()
                && params.quote_size_in_quote_atoms.is_some()
                && params.price_improvement_behavior.is_some(),
            StrategyError::InvalidStrategyParams
        );
        require!(
            params.quote_edge_in_bps.unwrap() > 0,
            StrategyError::EdgeMustBeNonZero
        );
        load_header(&ctx.accounts.market)?;
        let clock = Clock::get()?;
        msg!("Initializing Phoenix Strategy with params: {:?}", params);
        let mut phoenix_strategy = ctx.accounts.phoenix_strategy.load_init()?;
        *phoenix_strategy = PhoenixStrategyState {
            trader: *ctx.accounts.user.key,
            market: *ctx.accounts.market.key,
            bid_order_sequence_number: 0,
            bid_price_in_ticks: 0,
            initial_bid_size_in_base_lots: 0,
            ask_order_sequence_number: 0,
            ask_price_in_ticks: 0,
            initial_ask_size_in_base_lots: 0,
            last_update_slot: clock.slot,
            last_update_unix_timestamp: clock.unix_timestamp,
            quote_edge_in_bps: params.quote_edge_in_bps.unwrap(),
            quote_size_in_quote_atoms: params.quote_size_in_quote_atoms.unwrap(),
            post_only: params.post_only,
            price_improvement_behavior: params.price_improvement_behavior.unwrap().to_u8(),
            padding: [0; 6],
        };
        Ok(())
    }

    pub fn update_quotes(ctx: Context<UpdateQuotes>, params: OrderParams) -> Result<()> {
        let UpdateQuotes {
            phoenix_strategy,
            user,
            phoenix_program,
            log_authority,
            market: market_account,
            seat,
            quote_account,
            base_account,
            quote_vault,
            base_vault,
            token_program,
        } = ctx.accounts;

        let mut phoenix_strategy = phoenix_strategy.load_mut()?;

        // Update timestamps
        let clock = Clock::get()?;
        phoenix_strategy.last_update_slot = clock.slot;
        phoenix_strategy.last_update_unix_timestamp = clock.unix_timestamp;

        // Update the strategy parameters
        if let Some(edge) = params.strategy_params.quote_edge_in_bps {
            if edge > 0 {
                phoenix_strategy.quote_edge_in_bps = edge;
            }
        }
        if let Some(size) = params.strategy_params.quote_size_in_quote_atoms {
            phoenix_strategy.quote_size_in_quote_atoms = size;
        }
        phoenix_strategy.post_only = params.strategy_params.post_only;
        if let Some(price_improvement_behavior) = params.strategy_params.price_improvement_behavior
        {
            phoenix_strategy.price_improvement_behavior = price_improvement_behavior.to_u8();
        }

        // Load market
        let header = load_header(market_account)?;
        let market_data = market_account.data.borrow();
        let (_, market_bytes) = market_data.split_at(std::mem::size_of::<MarketHeader>());
        let market = phoenix::program::load_with_dispatch(&header.market_size_params, market_bytes)
            .map_err(|_| {
                msg!("Failed to deserialize market");
                StrategyError::FailedToDeserializePhoenixMarket
            })?
            .inner;

        let trader_index = market.get_trader_index(&user.key()).unwrap_or(u32::MAX) as u64;

        let size_in_quote_lots =
            phoenix_strategy.quote_size_in_quote_atoms * header.get_quote_lot_size().as_u64();

        let mut bid_price_in_ticks = get_bid_price(
            params.fair_price_in_quote_atoms_per_raw_base_unit,
            &header,
            phoenix_strategy.quote_edge_in_bps,
        );

        let mut ask_price_in_ticks = get_ask_price(
            params.fair_price_in_quote_atoms_per_raw_base_unit,
            &header,
            phoenix_strategy.quote_edge_in_bps,
        );

        // Returns the best bid and ask prices that are not placed by the trader
        let (best_bid, best_ask) = get_best_bid_and_ask(market, trader_index);

        msg!("Current market: {} @ {}", best_bid, best_ask);

        let price_improvement_behavior =
            PriceImprovementBehavior::from_u8(phoenix_strategy.price_improvement_behavior);

        match price_improvement_behavior {
            PriceImprovementBehavior::Join => {
                ask_price_in_ticks = ask_price_in_ticks.max(best_ask);
                bid_price_in_ticks = bid_price_in_ticks.min(best_bid);
            }
            PriceImprovementBehavior::Dime => {
                // If price_improvement_behavior is set to Dime, we will never price improve by more than 1 tick
                ask_price_in_ticks = ask_price_in_ticks.max(best_ask - 1);
                bid_price_in_ticks = bid_price_in_ticks.min(best_bid + 1);
            }
            _ => {}
        }

        let bid_size_in_base_lots =
            size_in_quote_lots / (bid_price_in_ticks * market.get_tick_size().as_u64());

        let ask_size_in_base_lots =
            size_in_quote_lots / (ask_price_in_ticks * market.get_tick_size().as_u64());

        msg!(
            "Our market: {} {} @ {} {}",
            bid_size_in_base_lots,
            bid_price_in_ticks,
            ask_price_in_ticks,
            ask_size_in_base_lots
        );

        let mut changed_bid = true;
        let mut changed_ask = true;
        let orders_to_cancel = [
            (
                Side::Bid,
                bid_price_in_ticks,
                FIFOOrderId::new_from_untyped(
                    phoenix_strategy.bid_price_in_ticks,
                    phoenix_strategy.bid_order_sequence_number,
                ),
                phoenix_strategy.initial_bid_size_in_base_lots,
            ),
            (
                Side::Ask,
                ask_price_in_ticks,
                FIFOOrderId::new_from_untyped(
                    phoenix_strategy.ask_price_in_ticks,
                    phoenix_strategy.ask_order_sequence_number,
                ),
                phoenix_strategy.initial_ask_size_in_base_lots,
            ),
        ]
        .iter()
        .filter_map(|(side, price, order_id, initial_size)| {
            if let Some(resting_order) = market.get_book(*side).get(order_id) {
                // The order is 100% identical, do not cancel it
                if resting_order.num_base_lots == *initial_size
                    && order_id.price_in_ticks.as_u64() == *price
                {
                    match side {
                        Side::Bid => changed_bid = false,
                        Side::Ask => changed_ask = false,
                    }
                    return None;
                }
                return Some(*order_id);
            }
            None
        })
        .collect::<Vec<FIFOOrderId>>();

        let mut order_sequence_number = market.get_sequence_number();

        // Drop reference prior to invoking
        drop(market_data);

        // Cancel the old orders
        if !orders_to_cancel.is_empty() {
            invoke(
                &phoenix::program::create_cancel_multiple_orders_by_id_with_free_funds_instruction(
                    &market_account.key(),
                    &user.key(),
                    &CancelMultipleOrdersByIdParams {
                        orders: orders_to_cancel
                            .iter()
                            .map(|o_id| CancelOrderParams {
                                order_sequence_number: o_id.order_sequence_number,
                                price_in_ticks: o_id.price_in_ticks.as_u64(),
                                side: Side::from_order_sequence_number(o_id.order_sequence_number),
                            })
                            .collect::<Vec<_>>(),
                    },
                ),
                &[
                    phoenix_program.to_account_info(),
                    log_authority.to_account_info(),
                    user.to_account_info(),
                    market_account.to_account_info(),
                ],
            )?;
        }

        let client_order_id = u128::from_le_bytes(user.key().to_bytes()[..16].try_into().unwrap());
        if !changed_ask && !changed_bid {
            msg!("No orders to change");
            return Ok(());
        }
        if phoenix_strategy.post_only
            || !matches!(price_improvement_behavior, PriceImprovementBehavior::Join)
        {
            invoke(
                &phoenix::program::create_new_multiple_order_instruction_with_custom_token_accounts(
                    &market_account.key(),
                    &user.key(),
                    &base_account.key(),
                    &quote_account.key(),
                    &header.base_params.mint_key,
                    &header.quote_params.mint_key,
                    &MultipleOrderPacket::new(
                        if changed_bid {
                            vec![CondensedOrder::new_default(
                                bid_price_in_ticks,
                                bid_size_in_base_lots,
                            )]
                        } else {
                            vec![]
                        },
                        if changed_ask {
                            vec![CondensedOrder::new_default(
                                ask_price_in_ticks,
                                ask_size_in_base_lots,
                            )]
                        } else {
                            vec![]
                        },
                        Some(client_order_id),
                        false,
                    ),
                ),
                &[
                    phoenix_program.to_account_info(),
                    log_authority.to_account_info(),
                    user.to_account_info(),
                    market_account.to_account_info(),
                    seat.to_account_info(),
                    quote_account.to_account_info(),
                    base_account.to_account_info(),
                    quote_vault.to_account_info(),
                    base_vault.to_account_info(),
                    token_program.to_account_info(),
                ],
            )?;
        } else {
            if changed_bid {
                invoke(
                    &phoenix::program::create_new_order_instruction_with_custom_token_accounts(
                        &market_account.key(),
                        &user.key(),
                        &base_account.key(),
                        &quote_account.key(),
                        &header.base_params.mint_key,
                        &header.quote_params.mint_key,
                        &OrderPacket::Limit {
                            side: Side::Bid,
                            price_in_ticks: Ticks::new(bid_price_in_ticks),
                            num_base_lots: BaseLots::new(bid_size_in_base_lots),
                            self_trade_behavior: phoenix::state::SelfTradeBehavior::CancelProvide,
                            match_limit: None,
                            client_order_id,
                            use_only_deposited_funds: false,
                            last_valid_slot: None,
                            last_valid_unix_timestamp_in_seconds: None,
                        },
                    ),
                    &[
                        phoenix_program.to_account_info(),
                        log_authority.to_account_info(),
                        user.to_account_info(),
                        market_account.to_account_info(),
                        seat.to_account_info(),
                        quote_account.to_account_info(),
                        base_account.to_account_info(),
                        quote_vault.to_account_info(),
                        base_vault.to_account_info(),
                        token_program.to_account_info(),
                    ],
                )?;
            }
            if changed_ask {
                invoke(
                    &phoenix::program::create_new_order_instruction_with_custom_token_accounts(
                        &market_account.key(),
                        &user.key(),
                        &base_account.key(),
                        &quote_account.key(),
                        &header.base_params.mint_key,
                        &header.quote_params.mint_key,
                        &OrderPacket::Limit {
                            side: Side::Ask,
                            price_in_ticks: Ticks::new(ask_price_in_ticks),
                            num_base_lots: BaseLots::new(ask_size_in_base_lots),
                            self_trade_behavior: phoenix::state::SelfTradeBehavior::CancelProvide,
                            match_limit: None,
                            client_order_id,
                            use_only_deposited_funds: false,
                            last_valid_slot: None,
                            last_valid_unix_timestamp_in_seconds: None,
                        },
                    ),
                    &[
                        phoenix_program.to_account_info(),
                        log_authority.to_account_info(),
                        user.to_account_info(),
                        market_account.to_account_info(),
                        seat.to_account_info(),
                        quote_account.to_account_info(),
                        base_account.to_account_info(),
                        quote_vault.to_account_info(),
                        base_vault.to_account_info(),
                        token_program.to_account_info(),
                    ],
                )?;
            }
        }

        let market_data = market_account.data.borrow();
        let (_, market_bytes) = market_data.split_at(std::mem::size_of::<MarketHeader>());
        let market = phoenix::program::load_with_dispatch(&header.market_size_params, market_bytes)
            .map_err(|_| {
                msg!("Failed to deserialize market");
                StrategyError::FailedToDeserializePhoenixMarket
            })?
            .inner;

        if changed_bid {
            // Reverse the bits of the order_sequence_number for bids
            let bid_order_id =
                FIFOOrderId::new_from_untyped(bid_price_in_ticks, !order_sequence_number);
            market
                .get_book(Side::Bid)
                .get(&bid_order_id)
                .map(|order| {
                    msg!("Placed bid order");
                    phoenix_strategy.bid_price_in_ticks = bid_price_in_ticks;
                    phoenix_strategy.bid_order_sequence_number = !order_sequence_number;
                    phoenix_strategy.initial_bid_size_in_base_lots = order.num_base_lots.as_u64();
                    order_sequence_number += 1;
                })
                .unwrap_or_else(|| {
                    msg!("Bid order not found");
                });
        }
        if changed_ask {
            let ask_order_id =
                FIFOOrderId::new_from_untyped(ask_price_in_ticks, order_sequence_number);
            market
                .get_book(Side::Ask)
                .get(&ask_order_id)
                .map(|order| {
                    msg!("Placed ask order");
                    phoenix_strategy.ask_price_in_ticks = ask_price_in_ticks;
                    phoenix_strategy.ask_order_sequence_number = order_sequence_number;
                    phoenix_strategy.initial_ask_size_in_base_lots = order.num_base_lots.as_u64();
                })
                .unwrap_or_else(|| {
                    msg!("Ask order not found");
                });
        }

        Ok(())
    }
}

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(
        init,
        seeds=[b"phoenix".as_ref(), user.key.as_ref(), market.key.as_ref()],
        bump,
        payer = user,
        space = 8 + std::mem::size_of::<PhoenixStrategyState>(),
    )]
    pub phoenix_strategy: AccountLoader<'info, PhoenixStrategyState>,
    #[account(mut)]
    pub user: Signer<'info>,
    /// CHECK: Checked in instruction
    pub market: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct UpdateQuotes<'info> {
    #[account(
        mut,
        seeds=[b"phoenix".as_ref(), user.key.as_ref(), market.key.as_ref()],
        bump,
    )]
    pub phoenix_strategy: AccountLoader<'info, PhoenixStrategyState>,
    pub user: Signer<'info>,
    pub phoenix_program: Program<'info, PhoenixV1>,
    /// CHECK: Checked in CPI
    pub log_authority: UncheckedAccount<'info>,
    /// CHECK: Checked in CPI
    #[account(mut)]
    pub market: UncheckedAccount<'info>,
    /// CHECK: Checked in instruction and CPI
    pub seat: UncheckedAccount<'info>,
    /// CHECK: Checked in CPI
    #[account(mut)]
    pub quote_account: UncheckedAccount<'info>,
    /// CHECK: Checked in CPI
    #[account(mut)]
    pub base_account: UncheckedAccount<'info>,
    /// CHECK: Checked in CPI
    #[account(mut)]
    pub quote_vault: UncheckedAccount<'info>,
    /// CHECK: Checked in CPI
    #[account(mut)]
    pub base_vault: UncheckedAccount<'info>,
    /// CHECK: Checked in CPI
    pub token_program: UncheckedAccount<'info>,
}

// An enum for custom error codes
#[error_code]
pub enum StrategyError {
    InvalidStrategyParams,
    EdgeMustBeNonZero,
    InvalidPhoenixProgram,
    FailedToDeserializePhoenixMarket,
}
