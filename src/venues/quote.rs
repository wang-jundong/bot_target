use crate::events::{PumpCurveSnapshot, PumpSwapSnapshot};
use solana_sdk::pubkey::Pubkey;

fn ceil_fee(amount: u128, fee_bps: u128) -> u128 {
    if fee_bps == 0 {
        return 0;
    }
    amount.saturating_mul(fee_bps).saturating_add(9_999) / 10_000
}

pub fn slippage_down(amount: u128, slippage_bps: u64) -> u128 {
    let slippage = slippage_bps as f64 / 100.0;
    let cut = (slippage * 10.0).floor().max(0.0) as u128;
    amount.saturating_sub(amount.saturating_mul(cut) / 1_000)
}

fn creator_is_default(creator: &str) -> bool {
    creator == Pubkey::default().to_string()
}

pub fn quote_buy_tokens(spendable: u128, curve: &PumpCurveSnapshot) -> u128 {
    if spendable == 0 || curve.virtual_token_reserves == 0 || curve.virtual_quote_reserves == 0 {
        return 0;
    }
    let creator_fee = if creator_is_default(&curve.creator) { 0 } else { curve.creator_fee_bps };
    let total_fee_bps = curve.protocol_fee_bps.saturating_add(creator_fee);
    let mut net_sol = spendable.saturating_mul(10_000) / total_fee_bps.saturating_add(10_000);
    let fees = ceil_fee(net_sol, curve.protocol_fee_bps) + if creator_fee == 0 { 0 } else { ceil_fee(net_sol, creator_fee) };
    let over = net_sol.saturating_add(fees);
    if over > spendable {
        net_sol = net_sol.saturating_sub(over - spendable);
    }
    if net_sol <= 1 {
        return 0;
    }
    let net = net_sol - 1;
    let tokens = net.saturating_mul(curve.virtual_token_reserves) / curve.virtual_quote_reserves.saturating_add(net);
    tokens.min(curve.real_token_reserves)
}

pub fn quote_sell_sol(token_amount: u128, curve: &PumpCurveSnapshot) -> u128 {
    if token_amount == 0 || curve.virtual_token_reserves == 0 {
        return 0;
    }
    let sol_cost = token_amount.saturating_mul(curve.virtual_quote_reserves) / curve.virtual_token_reserves.saturating_add(token_amount);
    let creator_fee = if creator_is_default(&curve.creator) { 0 } else { curve.creator_fee_bps };
    sol_cost.saturating_sub(ceil_fee(sol_cost, curve.protocol_fee_bps)).saturating_sub(ceil_fee(sol_cost, creator_fee))
}

fn slippage_factor(slippage_bps: u64, up: bool) -> u128 {
    let slippage = slippage_bps as f64 / 100.0;
    let factor = if up { 1.0 + slippage / 100.0 } else { 1.0 - slippage / 100.0 };
    (factor * 1e9).floor().max(0.0) as u128
}

pub fn quote_amm_buy(quote: u128, swap: &PumpSwapSnapshot, slippage_bps: u64) -> (u128, u128) {
    let effective_quote_reserve = swap.quote_reserve as i128 + swap.virtual_quote_reserves;
    let creator_fee = if creator_is_default(&swap.coin_creator) { 0 } else { swap.coin_creator_fee_bps };
    let total_fee_bps = swap.lp_fee_bps + swap.protocol_fee_bps + creator_fee;
    let mut effective_quote = quote.saturating_mul(10_000) / (10_000 + total_fee_bps);
    let fees = ceil_fee(effective_quote, swap.lp_fee_bps)
        + ceil_fee(effective_quote, swap.protocol_fee_bps)
        + if creator_fee == 0 { 0 } else { ceil_fee(effective_quote, creator_fee) };
    if fees > quote {
        effective_quote = effective_quote.saturating_sub(fees - quote);
    }
    let input_amount = if effective_quote == 0 { 0 } else { effective_quote - 1 };
    let base = if effective_quote_reserve <= 0 {
        0
    } else {
        let denominator = effective_quote_reserve as u128 + input_amount;
        if denominator == 0 { 0 } else { swap.base_reserve.saturating_mul(input_amount) / denominator }
    };
    let max_quote = quote.saturating_mul(slippage_factor(slippage_bps, true)) / 1_000_000_000;
    (base, max_quote)
}

pub fn quote_amm_sell(base: u128, swap: &PumpSwapSnapshot, slippage_bps: u64) -> u128 {
    let effective_quote_reserve = swap.quote_reserve as i128 + swap.virtual_quote_reserves;
    if base == 0 || swap.base_reserve == 0 || effective_quote_reserve <= 0 {
        return 0;
    }
    let quote_out = (effective_quote_reserve as u128).saturating_mul(base) / swap.base_reserve.saturating_add(base);
    let creator_fee = if creator_is_default(&swap.coin_creator) { 0 } else { swap.coin_creator_fee_bps };
    let net = quote_out
        .saturating_sub(ceil_fee(quote_out, swap.lp_fee_bps))
        .saturating_sub(ceil_fee(quote_out, swap.protocol_fee_bps))
        .saturating_sub(if creator_fee == 0 { 0 } else { ceil_fee(quote_out, creator_fee) });
    net.saturating_mul(slippage_factor(slippage_bps, false)) / 1_000_000_000
}
