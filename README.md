# Oracle Price Program

**Program ID:** `Liquidbdbaw2nFfqbjGGggc4vr4m48sfLzkAoQ7sw4n`

## Overview

Oracle Price is a fully on-chain price oracle for the [MemeLiquid](https://memeliquid.io) perpetuals platform. Instead of relying on an off-chain server to compute and sign prices, this program reads PumpSwap AMM pool reserves and Pyth Network's SOL/USD feed directly on Solana, calculates the TOKEN/USD price entirely on-chain using u128 integer arithmetic, and pushes it to the perpetuals engine via Cross-Program Invocation (CPI).

To protect against single-block price manipulation, the program maintains a TWAP (Time-Weighted Average Price) using a 10-sample ring buffer. Each time a price is pushed, the spot price is recorded in the buffer and the average of all samples is computed. Only this TWAP average is sent to the perpetuals engine — so even if someone manipulates the AMM pool in a single block, the impact on the oracle price is limited to 1/10th of the manipulation. Combined with a one-push-per-slot rate limit, Pyth staleness checks (60s max), confidence interval validation (5% max), and a 10% price cap circuit breaker on the perpetuals side, the system provides multiple layers of defense against oracle manipulation.

The oracle authority is a PDA (Program Derived Address) owned by this program — no human wallet can push prices directly. Anyone can call `push_price` permissionlessly (after the initial 10-push warm-up period), and the program will compute the correct price from on-chain data. This makes the price feed fully trustless and eliminates single points of failure.

## How It Works

```
PumpSwap AMM Pool          Pyth Network
(TOKEN/SOL reserves)       (SOL/USD feed)
        |                        |
        v                        v
  +---------------------------------+
  |     oracle-price program        |
  |                                 |
  |  1. Read pool base/quote        |
  |  2. Read Pyth SOL/USD           |
  |  3. Validate all inputs         |
  |  4. Calculate TOKEN/USD (u128)  |
  |  5. Update TWAP ring buffer     |
  |  6. CPI → PushOraclePrice       |
  +---------------------------------+
                  |
                  v
          percolator-prog
          (perpetuals slab)
```

### Price Calculation

```
TOKEN/SOL = quote_reserve / base_reserve  (adjusted for decimal difference)
TOKEN/USD = TOKEN/SOL * SOL/USD (from Pyth)
```

All arithmetic uses `u128` to avoid overflow. The final `price_e6` is TOKEN/USD scaled by 10^6.

### TWAP

A ring buffer of the last 10 spot prices is maintained in the PDA. On each push, the buffer average (TWAP) is calculated and sent to percolator-prog. This smooths out single-block price manipulation.

## Instructions

### `initialize_oracle`

Creates a `PriceOracle` PDA for a specific market. Admin-only, called once per market.

**Accounts:**
| # | Account | Type | Description |
|---|---------|------|-------------|
| 0 | `admin` | Signer, Mut | Admin keypair (pays rent) |
| 1 | `oracle_account` | PDA, Mut | `seeds = ["oracle", slab]` |
| 2 | `slab` | Readonly | percolator-prog market slab |
| 3 | `pool` | Readonly | PumpSwap AMM pool |
| 4 | `system_program` | Program | System Program |

### `compute_price`

Calculates price and updates the PDA **without** CPI. Used for testing and TWAP warm-up before authority transfer.

**Accounts:**
| # | Account | Type | Description |
|---|---------|------|-------------|
| 0 | `caller` | Signer, Mut | Anyone (admin-only during warm-up) |
| 1 | `oracle_account` | PDA, Mut | Oracle PDA |
| 2 | `pool` | Readonly | PumpSwap AMM pool |
| 3 | `pool_base_token` | Readonly | Pool's base token account |
| 4 | `pool_quote_token` | Readonly | Pool's quote token account |
| 5 | `pyth_price_feed` | Readonly | Pyth SOL/USD price feed |

### `push_price`

Calculates price, updates the PDA, and CPI pushes the TWAP to percolator-prog. Permissionless after warm-up (first 10 pushes are admin-only).

**Accounts:**
| # | Account | Type | Description |
|---|---------|------|-------------|
| 0 | `caller` | Signer, Mut | Anyone (admin-only during warm-up) |
| 1 | `oracle_account` | PDA, Mut | Oracle PDA (signs CPI) |
| 2 | `pool` | Readonly | PumpSwap AMM pool |
| 3 | `pool_base_token` | Readonly | Pool's base token account |
| 4 | `pool_quote_token` | Readonly | Pool's quote token account |
| 5 | `pyth_price_feed` | Readonly | Pyth SOL/USD price feed |
| 6 | `slab` | Mut | percolator-prog market slab |
| 7 | `percolator_program` | Readonly | percolator-prog program |

## PriceOracle PDA Layout

Seeds: `["oracle", slab_pubkey]`

| Field | Type | Size | Description |
|-------|------|------|-------------|
| `slab` | Pubkey | 32 | Target market slab |
| `pool` | Pubkey | 32 | PumpSwap pool address |
| `price_e6` | u64 | 8 | Current spot TOKEN/USD (x10^6) |
| `token_sol_e9` | u64 | 8 | TOKEN/SOL (x10^9) |
| `sol_usd_e6` | u64 | 8 | SOL/USD from Pyth (x10^6) |
| `last_update` | i64 | 8 | Unix timestamp of last update |
| `base_reserve` | u64 | 8 | Pool base token reserve |
| `quote_reserve` | u64 | 8 | Pool quote (wSOL) reserve |
| `last_slot` | u64 | 8 | Last push slot (rate limit) |
| `price_history` | [u64; 10] | 80 | TWAP ring buffer |
| `history_idx` | u8 | 1 | Ring buffer write index |
| `history_count` | u8 | 1 | Filled count (0-10) |
| `twap_e6` | u64 | 8 | TWAP average (pushed to slab) |
| `bump` | u8 | 1 | PDA bump seed |
| `admin` | Pubkey | 32 | Emergency admin |

Total: 251 bytes (+ 8 byte Anchor discriminator = 259 bytes)

## Security

- **Slot rate limit:** One push per Solana slot prevents spam.
- **Warm-up guard:** First 10 pushes are admin-only, preventing TWAP manipulation during buffer fill.
- **Pyth validation:** Feed ID, owner program, staleness (60s max), and confidence (5% max ratio) are all checked.
- **Pool validation:** Pool address matches PDA, token accounts match pool data, SPL Token/Token-2022 ownership verified.
- **Integer math:** All price calculations use `u128` to prevent overflow.
- **PDA authority:** The oracle PDA signs CPI calls, so only this program can push prices to the slab.

## Deployed Markets

| Market | Slab | Pool | Oracle PDA |
|--------|------|------|------------|
| LIQUID/SOL | `GxhEAi8ZfrQxS7MLokCZQSGQKJXYjpp3gZzSP8X47Df8` | `Hv7KcoMceKmk4AQdTRHq2XwdEMCLabGsR99RwiMdGcha` | `8iW2s5yMVL9u8Tj4fBmq88gmAMspxEHAhshwjSHQNBDJ` |
| Buttcoin/SOL | `2UfcqQ4oftBBdb6kyve1FMqyZBontpEHgkY3LZkkugTe` | `FFcYgSSgWHforA9rXXkA48p8YFoz8TSW85Jpo3CQHDyS` | `FL7tP92yTMqCCMnSiHQLE52Rx449n9ifnR9pszsgNnik` |

## Dependencies

| Crate | Version |
|-------|---------|
| `anchor-lang` | 0.30.1 |

## Build

```bash
# Requires Solana CLI with cargo-build-sbf
cargo-build-sbf --manifest-path programs/oracle-price/Cargo.toml
```

## License

Proprietary. All rights reserved.
