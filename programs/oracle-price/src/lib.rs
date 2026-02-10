use anchor_lang::prelude::*;
use anchor_lang::solana_program::{
    instruction::{AccountMeta, Instruction},
    program::invoke_signed,
};

declare_id!("Liquidbdbaw2nFfqbjGGggc4vr4m48sfLzkAoQ7sw4n");

// ============================================================================
// Constants
// ============================================================================

/// percolator-prog program ID
const PERCOLATOR_PROG_ID: Pubkey =
    pubkey!("DP2EbA2v6rmkmNieZpnjumXosuXQ93r9jyb9eSzzkf1x");

/// Pyth Receiver Program (owner of PriceUpdateV2 accounts)
const PYTH_RECEIVER: Pubkey =
    pubkey!("rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ");

/// Pyth Push Oracle Program (alternative owner)
const PYTH_PUSH_ORACLE: Pubkey =
    pubkey!("pythWSnswVUd12oZpeFP8e9CVaEqJg25g1Vtc2biRsT");

/// SOL/USD Pyth feed ID (first 32 bytes at offset 42 in PriceUpdateV2)
const SOL_USD_FEED_ID: [u8; 32] = [
    0xef, 0x0d, 0x8b, 0x6f, 0xda, 0x2c, 0xeb, 0xa4,
    0x1d, 0xa1, 0x5d, 0x40, 0x95, 0xd1, 0xda, 0x39,
    0x2a, 0x0d, 0x2f, 0x8e, 0xd0, 0xc6, 0xc7, 0xbc,
    0x0f, 0x4c, 0xfa, 0xc8, 0xc2, 0x80, 0xb5, 0x6d,
];

/// SPL Token Program ID
const SPL_TOKEN_PROGRAM: Pubkey =
    pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");

/// SPL Token-2022 (Token Extensions) Program ID
const SPL_TOKEN_2022_PROGRAM: Pubkey =
    pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");

// Account data offsets
const SPL_AMOUNT_OFFSET: usize = 64;
const POOL_BASE_TOKEN_OFFSET: usize = 139;
const POOL_QUOTE_TOKEN_OFFSET: usize = 171;
const PYTH_FEED_ID_OFFSET: usize = 41;
const PYTH_PRICE_OFFSET: usize = 73;
const PYTH_CONF_OFFSET: usize = 81;
const PYTH_EXPO_OFFSET: usize = 89;
const PYTH_PUBLISH_TIME_OFFSET: usize = 93;

/// Max Pyth staleness in seconds
const MAX_PYTH_STALENESS: i64 = 60;

/// Max Pyth confidence ratio (5% = conf * 20 < price)
const MAX_CONF_RATIO_X20: u64 = 20;

/// TWAP ring buffer size
const TWAP_SIZE: usize = 10;

// ============================================================================
// Program
// ============================================================================

#[program]
pub mod oracle_price {
    use super::*;

    /// Initialize a PriceOracle PDA for a specific market.
    /// Admin-only, called once per market.
    pub fn initialize_oracle(ctx: Context<InitializeOracle>) -> Result<()> {
        let oracle = &mut ctx.accounts.oracle_account;
        oracle.slab = ctx.accounts.slab.key();
        oracle.pool = ctx.accounts.pool.key();
        oracle.bump = ctx.bumps.oracle_account;
        oracle.admin = ctx.accounts.admin.key();
        // All other fields default to 0
        Ok(())
    }

    /// Compute price and write to PDA without CPI.
    /// Used for testing before authority transfer.
    pub fn compute_price(ctx: Context<ComputePrice>) -> Result<()> {
        let oracle = &mut ctx.accounts.oracle_account;
        let clock = Clock::get()?;

        // 1. Slot rate limit
        require!(oracle.last_slot < clock.slot, OracleError::SlotAlreadyPushed);

        // 2. Warm-up guard: first 10 pushes admin-only
        if oracle.history_count < TWAP_SIZE as u8 {
            require!(
                ctx.accounts.caller.key() == oracle.admin,
                OracleError::WarmupAdminOnly
            );
        }

        // 3-7. Calculate price
        let (price_e6, token_sol_e9, sol_usd_e6, base_reserve, quote_reserve) =
            calculate_price(
                &ctx.accounts.pool,
                &ctx.accounts.pool_base_token,
                &ctx.accounts.pool_quote_token,
                &ctx.accounts.pyth_price_feed,
                &clock,
                oracle.pool,
            )?;

        // 8. Update TWAP + write PDA
        update_oracle_state(oracle, price_e6, token_sol_e9, sol_usd_e6, base_reserve, quote_reserve, &clock);

        Ok(())
    }

    /// Compute price, write to PDA, and CPI PushOraclePrice to percolator-prog slab.
    pub fn push_price(ctx: Context<PushPrice>) -> Result<()> {
        let oracle = &mut ctx.accounts.oracle_account;
        let clock = Clock::get()?;

        // 1. Slot rate limit
        require!(oracle.last_slot < clock.slot, OracleError::SlotAlreadyPushed);

        // 2. Warm-up guard: first 10 pushes admin-only
        if oracle.history_count < TWAP_SIZE as u8 {
            require!(
                ctx.accounts.caller.key() == oracle.admin,
                OracleError::WarmupAdminOnly
            );
        }

        // 3. Validate percolator_program
        require!(
            ctx.accounts.percolator_program.key() == PERCOLATOR_PROG_ID,
            OracleError::InvalidPercolatorProgram
        );

        // 4. Validate slab owned by percolator-prog
        require!(
            *ctx.accounts.slab.owner == PERCOLATOR_PROG_ID,
            OracleError::InvalidSlabOwner
        );

        // 5-9. Calculate price
        let (price_e6, token_sol_e9, sol_usd_e6, base_reserve, quote_reserve) =
            calculate_price(
                &ctx.accounts.pool,
                &ctx.accounts.pool_base_token,
                &ctx.accounts.pool_quote_token,
                &ctx.accounts.pyth_price_feed,
                &clock,
                oracle.pool,
            )?;

        // 10. Update TWAP + write PDA
        update_oracle_state(oracle, price_e6, token_sol_e9, sol_usd_e6, base_reserve, quote_reserve, &clock);

        // 11. CPI: PushOraclePrice(twap_e6, timestamp) → percolator-prog
        let twap = oracle.twap_e6;
        let timestamp = clock.unix_timestamp;
        let bump = oracle.bump;
        let slab_key = ctx.accounts.slab.key();

        // Build instruction data: [tag=17] + [price_e6: u64 LE] + [timestamp: i64 LE]
        let mut ix_data = Vec::with_capacity(17);
        ix_data.push(17u8);
        ix_data.extend_from_slice(&twap.to_le_bytes());
        ix_data.extend_from_slice(&timestamp.to_le_bytes());

        let ix = Instruction {
            program_id: PERCOLATOR_PROG_ID,
            accounts: vec![
                AccountMeta::new_readonly(ctx.accounts.oracle_account.key(), true),
                AccountMeta::new(slab_key, false),
            ],
            data: ix_data,
        };

        let seeds: &[&[u8]] = &[b"oracle", slab_key.as_ref(), &[bump]];
        invoke_signed(
            &ix,
            &[
                ctx.accounts.oracle_account.to_account_info(),
                ctx.accounts.slab.to_account_info(),
            ],
            &[seeds],
        )?;

        Ok(())
    }
}

// ============================================================================
// Shared logic
// ============================================================================

/// Read a u64 LE from account data at the given offset.
fn read_u64_le(data: &[u8], offset: usize) -> Result<u64> {
    require!(data.len() >= offset + 8, OracleError::AccountDataTooSmall);
    Ok(u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap()))
}

/// Read an i64 LE from account data at the given offset.
fn read_i64_le(data: &[u8], offset: usize) -> Result<i64> {
    require!(data.len() >= offset + 8, OracleError::AccountDataTooSmall);
    Ok(i64::from_le_bytes(data[offset..offset + 8].try_into().unwrap()))
}

/// Read an i32 LE from account data at the given offset.
fn read_i32_le(data: &[u8], offset: usize) -> Result<i32> {
    require!(data.len() >= offset + 4, OracleError::AccountDataTooSmall);
    Ok(i32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()))
}

/// Read a Pubkey (32 bytes) from account data at the given offset.
fn read_pubkey(data: &[u8], offset: usize) -> Result<Pubkey> {
    require!(data.len() >= offset + 32, OracleError::AccountDataTooSmall);
    Ok(Pubkey::new_from_array(data[offset..offset + 32].try_into().unwrap()))
}

/// Calculate TOKEN/USD price from PumpSwap pool reserves and Pyth SOL/USD.
/// Returns (price_e6, token_sol_e9, sol_usd_e6, base_reserve, quote_reserve).
fn calculate_price<'info>(
    pool: &AccountInfo<'info>,
    pool_base_token: &AccountInfo<'info>,
    pool_quote_token: &AccountInfo<'info>,
    pyth_price_feed: &AccountInfo<'info>,
    clock: &Clock,
    expected_pool: Pubkey,
) -> Result<(u64, u64, u64, u64, u64)> {
    // --- Pool validation ---
    require!(pool.key() == expected_pool, OracleError::PoolMismatch);
    let pool_data = pool.try_borrow_data()?;
    require!(pool_data.len() >= 203, OracleError::AccountDataTooSmall);

    // Validate pool → token account mapping
    let expected_base = read_pubkey(&pool_data, POOL_BASE_TOKEN_OFFSET)?;
    let expected_quote = read_pubkey(&pool_data, POOL_QUOTE_TOKEN_OFFSET)?;
    require!(pool_base_token.key() == expected_base, OracleError::PoolTokenMismatch);
    require!(pool_quote_token.key() == expected_quote, OracleError::PoolTokenMismatch);

    // --- SPL Token account validation (accept both Token and Token-2022) ---
    require!(
        *pool_base_token.owner == SPL_TOKEN_PROGRAM || *pool_base_token.owner == SPL_TOKEN_2022_PROGRAM,
        OracleError::InvalidTokenOwner
    );
    require!(
        *pool_quote_token.owner == SPL_TOKEN_PROGRAM || *pool_quote_token.owner == SPL_TOKEN_2022_PROGRAM,
        OracleError::InvalidTokenOwner
    );

    // Read reserves
    let base_data = pool_base_token.try_borrow_data()?;
    let quote_data = pool_quote_token.try_borrow_data()?;
    let base_reserve = read_u64_le(&base_data, SPL_AMOUNT_OFFSET)?;
    let quote_reserve = read_u64_le(&quote_data, SPL_AMOUNT_OFFSET)?;
    require!(base_reserve > 0, OracleError::ZeroReserve);
    require!(quote_reserve > 0, OracleError::ZeroReserve);

    // --- Pyth validation ---
    let pyth_owner = pyth_price_feed.owner;
    require!(
        *pyth_owner == PYTH_RECEIVER || *pyth_owner == PYTH_PUSH_ORACLE,
        OracleError::InvalidPythOwner
    );

    let pyth_data = pyth_price_feed.try_borrow_data()?;
    require!(pyth_data.len() >= 102, OracleError::AccountDataTooSmall);

    // Verify feed ID
    let feed_id: &[u8] = &pyth_data[PYTH_FEED_ID_OFFSET..PYTH_FEED_ID_OFFSET + 32];
    require!(feed_id == SOL_USD_FEED_ID, OracleError::InvalidPythFeedId);

    // Read Pyth price components
    let pyth_price_raw = read_i64_le(&pyth_data, PYTH_PRICE_OFFSET)?;
    let pyth_conf = read_u64_le(&pyth_data, PYTH_CONF_OFFSET)?;
    let pyth_expo = read_i32_le(&pyth_data, PYTH_EXPO_OFFSET)?;
    let pyth_publish_time = read_i64_le(&pyth_data, PYTH_PUBLISH_TIME_OFFSET)?;

    require!(pyth_price_raw > 0, OracleError::InvalidPythPrice);

    // Staleness check
    let age = clock.unix_timestamp.saturating_sub(pyth_publish_time);
    require!(age >= 0 && age <= MAX_PYTH_STALENESS, OracleError::PythStale);

    // Confidence check: conf * 20 < price (i.e. conf/price < 5%)
    let pyth_price = pyth_price_raw as u64;
    require!(
        pyth_conf.saturating_mul(MAX_CONF_RATIO_X20) < pyth_price,
        OracleError::PythConfidenceTooWide
    );

    // --- Price calculation (u128 integer math) ---
    // TOKEN/SOL (e9) = quote_reserve * 1e9 / base_reserve
    // (PumpFun tokens: 6 decimals, wSOL: 9 decimals → natural ratio gives TOKEN/SOL in lamports/unit)
    // We want e9 precision: token_sol_e9 = quote * 1_000_000_000 / base (but this is for display)
    let token_sol_e9 = ((quote_reserve as u128) * 1_000_000_000u128 / (base_reserve as u128)) as u64;

    // SOL/USD from Pyth: pyth_price * 10^expo gives USD price
    // sol_usd_e6 = pyth_price * 10^(6 + expo)
    let sol_usd_expo_sum: i32 = 6 + pyth_expo;
    let sol_usd_e6: u64 = if sol_usd_expo_sum >= 0 {
        (pyth_price as u128 * 10u128.pow(sol_usd_expo_sum as u32)) as u64
    } else {
        (pyth_price as u128 / 10u128.pow((-sol_usd_expo_sum) as u32)) as u64
    };

    // TOKEN/USD * 1e6:
    // price_e6 = quote_reserve * pyth_price * 10^(3 + expo) / base_reserve
    // Derivation: TOKEN/SOL = quote/base (adjusting 9-6=3 decimal difference)
    //             TOKEN/USD = TOKEN/SOL * SOL/USD
    //             price_e6  = (quote/base) * 10^(9-6) * pyth_price * 10^expo * 1e6
    //                       = quote * pyth_price * 10^(3+expo) / base     [when 3+expo < 0]
    //                       = quote * pyth_price / (base * 10^(-(3+expo)))
    let expo_sum: i32 = 3 + pyth_expo;
    let (num, den) = if expo_sum >= 0 {
        (
            (quote_reserve as u128) * (pyth_price as u128) * 10u128.pow(expo_sum as u32),
            base_reserve as u128,
        )
    } else {
        (
            (quote_reserve as u128) * (pyth_price as u128),
            (base_reserve as u128) * 10u128.pow((-expo_sum) as u32),
        )
    };
    require!(den > 0, OracleError::ZeroReserve);
    let price_e6 = (num / den) as u64;
    require!(price_e6 > 0, OracleError::ZeroPrice);

    Ok((price_e6, token_sol_e9, sol_usd_e6, base_reserve, quote_reserve))
}

/// Update oracle PDA state: TWAP ring buffer + fields.
fn update_oracle_state(
    oracle: &mut Account<PriceOracle>,
    price_e6: u64,
    token_sol_e9: u64,
    sol_usd_e6: u64,
    base_reserve: u64,
    quote_reserve: u64,
    clock: &Clock,
) {
    // Update spot fields
    oracle.price_e6 = price_e6;
    oracle.token_sol_e9 = token_sol_e9;
    oracle.sol_usd_e6 = sol_usd_e6;
    oracle.base_reserve = base_reserve;
    oracle.quote_reserve = quote_reserve;
    oracle.last_update = clock.unix_timestamp;
    oracle.last_slot = clock.slot;

    // TWAP ring buffer update
    let idx = oracle.history_idx as usize;
    oracle.price_history[idx] = price_e6;
    oracle.history_idx = ((idx + 1) % TWAP_SIZE) as u8;
    if oracle.history_count < TWAP_SIZE as u8 {
        oracle.history_count += 1;
    }

    // Calculate TWAP from filled portion
    let count = oracle.history_count as usize;
    let sum: u128 = oracle.price_history[..count].iter().map(|&p| p as u128).sum();
    oracle.twap_e6 = (sum / count as u128) as u64;
}

// ============================================================================
// Accounts
// ============================================================================

#[derive(Accounts)]
pub struct InitializeOracle<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,

    #[account(
        init,
        payer = admin,
        space = 8 + PriceOracle::INIT_SPACE,
        seeds = [b"oracle", slab.key().as_ref()],
        bump,
    )]
    pub oracle_account: Account<'info, PriceOracle>,

    /// CHECK: percolator-prog slab account — validated by owner check
    #[account(
        constraint = *slab.owner == PERCOLATOR_PROG_ID @ OracleError::InvalidSlabOwner
    )]
    pub slab: AccountInfo<'info>,

    /// CHECK: PumpSwap pool account — stored in PDA, validated on push
    pub pool: AccountInfo<'info>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct ComputePrice<'info> {
    #[account(mut)]
    pub caller: Signer<'info>,

    #[account(
        mut,
        seeds = [b"oracle", oracle_account.slab.as_ref()],
        bump = oracle_account.bump,
    )]
    pub oracle_account: Account<'info, PriceOracle>,

    /// CHECK: PumpSwap pool — validated against oracle_account.pool
    pub pool: AccountInfo<'info>,

    /// CHECK: Pool base token account — validated against pool data
    pub pool_base_token: AccountInfo<'info>,

    /// CHECK: Pool quote token account — validated against pool data
    pub pool_quote_token: AccountInfo<'info>,

    /// CHECK: Pyth SOL/USD price feed — validated by owner + feed_id
    pub pyth_price_feed: AccountInfo<'info>,
}

#[derive(Accounts)]
pub struct PushPrice<'info> {
    #[account(mut)]
    pub caller: Signer<'info>,

    #[account(
        mut,
        seeds = [b"oracle", oracle_account.slab.as_ref()],
        bump = oracle_account.bump,
    )]
    pub oracle_account: Account<'info, PriceOracle>,

    /// CHECK: PumpSwap pool — validated against oracle_account.pool
    pub pool: AccountInfo<'info>,

    /// CHECK: Pool base token account — validated against pool data
    pub pool_base_token: AccountInfo<'info>,

    /// CHECK: Pool quote token account — validated against pool data
    pub pool_quote_token: AccountInfo<'info>,

    /// CHECK: Pyth SOL/USD price feed — validated by owner + feed_id
    pub pyth_price_feed: AccountInfo<'info>,

    /// CHECK: percolator-prog slab — validated by owner check, writable for CPI
    #[account(
        mut,
        constraint = *slab.owner == PERCOLATOR_PROG_ID @ OracleError::InvalidSlabOwner
    )]
    pub slab: AccountInfo<'info>,

    /// CHECK: percolator-prog program — validated by ID check
    #[account(
        constraint = percolator_program.key() == PERCOLATOR_PROG_ID @ OracleError::InvalidPercolatorProgram
    )]
    pub percolator_program: AccountInfo<'info>,
}

// ============================================================================
// State
// ============================================================================

#[account]
#[derive(InitSpace)]
pub struct PriceOracle {
    pub slab: Pubkey,              // 32 — target market slab
    pub pool: Pubkey,              // 32 — PumpSwap pool address
    pub price_e6: u64,             // 8  — current spot TOKEN/USD (*1e6)
    pub token_sol_e9: u64,         // 8  — TOKEN/SOL (*1e9)
    pub sol_usd_e6: u64,           // 8  — SOL/USD (*1e6, from Pyth)
    pub last_update: i64,          // 8  — last update unix timestamp
    pub base_reserve: u64,         // 8  — pool base token reserve
    pub quote_reserve: u64,        // 8  — pool quote (wSOL) reserve
    pub last_slot: u64,            // 8  — last push slot (rate limit)
    pub price_history: [u64; 10],  // 80 — TWAP ring buffer (last 10 spot prices)
    pub history_idx: u8,           // 1  — ring buffer write index (0-9)
    pub history_count: u8,         // 1  — filled count (0-10)
    pub twap_e6: u64,              // 8  — TWAP of price_history (pushed to slab)
    pub bump: u8,                  // 1  — PDA bump seed
    pub admin: Pubkey,             // 32 — emergency admin
}
// Total: 8 (discriminator) + 32+32+8+8+8+8+8+8+8+80+1+1+8+1+32 = 251 bytes

// ============================================================================
// Errors
// ============================================================================

#[error_code]
pub enum OracleError {
    #[msg("Price already pushed this slot")]
    SlotAlreadyPushed,
    #[msg("Only admin can push during warm-up period (first 10 pushes)")]
    WarmupAdminOnly,
    #[msg("Pool address does not match oracle PDA")]
    PoolMismatch,
    #[msg("Pool token accounts do not match pool data")]
    PoolTokenMismatch,
    #[msg("Token account not owned by SPL Token Program")]
    InvalidTokenOwner,
    #[msg("Pyth account not owned by Pyth Receiver or Push Oracle")]
    InvalidPythOwner,
    #[msg("Pyth feed ID does not match SOL/USD")]
    InvalidPythFeedId,
    #[msg("Pyth price is not positive")]
    InvalidPythPrice,
    #[msg("Pyth price is stale (>60s)")]
    PythStale,
    #[msg("Pyth confidence interval too wide (>5%)")]
    PythConfidenceTooWide,
    #[msg("Pool reserve is zero")]
    ZeroReserve,
    #[msg("Calculated price is zero")]
    ZeroPrice,
    #[msg("Account data too small for expected read")]
    AccountDataTooSmall,
    #[msg("Slab not owned by percolator-prog")]
    InvalidSlabOwner,
    #[msg("Invalid percolator program ID")]
    InvalidPercolatorProgram,
}
