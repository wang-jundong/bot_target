//! Trade-print tape for v071 volume and age.
//!
//! Volume is buy SOL plus sell SOL over `target_volume_s` up to and including
//! the buy that opened the cycle. A 0 SOL print is left out. An alt-quote print
//! keeps its quote atoms; the runtime turns those atoms into SOL with the
//! Jupiter rate before the band is applied. A zero-size print is kept so that
//! buy can still end the window.
//!
//! Age matches sibling project `1`: seconds from the pump `create` /
//! `create_v2` we saw on the firehose to the cycle-open buy. Launch times are
//! sticky `Instant` stamps — no RPC. A missing create makes age non-finite
//! (unknown). A launch older than the keep horizon stays aged-out instead of
//! becoming a new birth.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;

use crate::events::{CompiledIx, ParsedTargetTransaction};
use crate::venues::decoder;
use crate::venues::ids::{
    PUMP_CREATE_DISCRIMINATOR, PUMP_CREATE_MINT_ACCOUNT_INDEX, PUMP_CREATE_V2_DISCRIMINATOR,
    PUMP_PROGRAM, is_sol_quote,
};

const MAX_MINTS: usize = 8_192;
const MAX_SAMPLES_PER_MINT: usize = 512;
const MAX_LAUNCHES: usize = 65_536;

#[derive(Debug, Clone)]
struct Sample {
    at: Instant,
    /// SOL lamports when `quote` is `None`. Quote atoms when `quote` is set.
    atoms: u64,
    quote: Option<Pubkey>,
    /// A non-SOL print whose quote mint could not be read.
    unpriced: bool,
    sig: Signature,
}

#[derive(Debug, Default)]
struct Launches {
    by_mint: HashMap<Pubkey, Instant>,
    by_time: BTreeMap<(Instant, u64), Pubkey>,
    seq: u64,
    /// Mints whose create fell off the horizon. A later stamp must not
    /// become a new launch, or an old mint looks young enough to buy.
    aged_out: HashSet<Pubkey>,
    aged_order: VecDeque<Pubkey>,
}

impl Launches {
    fn remember(&mut self, mint: Pubkey, at: Instant, keep: Duration) {
        self.expire(keep);
        if self.aged_out.contains(&mint) || self.by_mint.contains_key(&mint) {
            return;
        }
        self.seq = self.seq.wrapping_add(1);
        self.by_mint.insert(mint, at);
        self.by_time.insert((at, self.seq), mint);
        self.expire(keep);
    }

    fn expire(&mut self, keep: Duration) {
        let Some(horizon) = Instant::now().checked_sub(keep) else {
            return;
        };
        while let Some(first) = self.by_time.keys().next().copied() {
            let too_old = first.0 < horizon;
            let over = self.by_mint.len() > MAX_LAUNCHES;
            if !too_old && !over {
                break;
            }
            let Some(mint) = self.by_time.remove(&first) else {
                break;
            };
            self.by_mint.remove(&mint);
            self.mark_aged(mint);
        }
    }

    fn mark_aged(&mut self, mint: Pubkey) {
        if self.aged_out.insert(mint) {
            self.aged_order.push_back(mint);
        }
        while self.aged_order.len() > MAX_LAUNCHES {
            if let Some(old) = self.aged_order.pop_front() {
                self.aged_out.remove(&old);
            }
        }
    }

    /// Seconds from create to `open_at`. Non-finite when create was never seen
    /// or the stamp aged out of the keep window.
    fn age_at(&self, mint: &Pubkey, open_at: Instant) -> f64 {
        if self.aged_out.contains(mint) {
            return f64::NAN;
        }
        match self.by_mint.get(mint) {
            Some(at) => open_at.saturating_duration_since(*at).as_secs_f64(),
            None => f64::NAN,
        }
    }
}

#[derive(Debug)]
pub struct V071Tape {
    inner: Mutex<HashMap<Pubkey, VecDeque<Sample>>>,
    launches: Mutex<Launches>,
    launch_keep: Duration,
}

impl V071Tape {
    pub fn new(launch_keep: Duration) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            launches: Mutex::new(Launches::default()),
            launch_keep,
        }
    }

    pub fn push(&self, mint: Pubkey, sol: f64, sig: Signature, keep: Duration) {
        self.push_at(mint, sol, sig, Instant::now(), keep);
    }

    pub fn push_at(&self, mint: Pubkey, sol: f64, sig: Signature, at: Instant, keep: Duration) {
        if sol <= 0.0 || !sol.is_finite() {
            return;
        }
        let atoms = (sol * 1_000_000_000.0).round() as u64;
        self.push_sample(mint, atoms, None, false, sig, at, keep);
    }

    fn push_sample(
        &self,
        mint: Pubkey,
        atoms: u64,
        quote: Option<Pubkey>,
        unpriced: bool,
        sig: Signature,
        at: Instant,
        keep: Duration,
    ) {
        let mut map = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        if map.len() >= MAX_MINTS && !map.contains_key(&mint) {
            evict_coldest(&mut map);
        }
        let stale = {
            let queue = map.entry(mint).or_default();
            if queue.iter().any(|sample| sample.sig == sig) {
                return;
            }
            queue.push_back(Sample {
                at,
                atoms,
                quote,
                unpriced,
                sig,
            });
            while queue.len() > MAX_SAMPLES_PER_MINT {
                queue.pop_front();
            }
            let cutoff = Instant::now()
                .checked_sub(keep)
                .unwrap_or_else(Instant::now);
            while queue.front().is_some_and(|sample| sample.at < cutoff) {
                queue.pop_front();
            }
            queue.is_empty()
        };
        if stale {
            map.remove(&mint);
        }
    }

    /// Firehose: first pump create for this mint. Sticky — later creates ignored.
    pub fn record_launch(&self, mint: Pubkey) {
        self.record_launch_at(mint, Instant::now());
    }

    pub fn record_launch_at(&self, mint: Pubkey, at: Instant) {
        let mut launches = self.launches.lock().unwrap_or_else(|err| err.into_inner());
        launches.remember(mint, at, self.launch_keep);
    }

    /// Quote legs inside the volume window, whether a leg could not be priced,
    /// and age in seconds at the cycle-opening buy.
    ///
    /// Each leg is `(quote_mint, atoms)`. `None` is native SOL lamports. The
    /// window includes that buy and every sized print in the lookback before
    /// it. A zero-size print still ends the window. Age is non-finite when the
    /// pump create was never seen on the firehose (or aged out), or when an
    /// `open_sig` was given but that buy is not on the tape yet — never fall
    /// back to `Instant::now()`, or age drifts through the seek window and a
    /// too-young cycle can pass `min_target_age_s`. Volume is unpriced when
    /// this buy's signature is missing, so later prints are not counted in
    /// its place.
    pub fn open_window(
        &self,
        mint: &Pubkey,
        lookback: Duration,
        open_sig: Option<&Signature>,
    ) -> (Vec<(Option<Pubkey>, u64)>, bool, f64) {
        let map = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        let queue = map.get(mint);
        let found = queue.and_then(|queue| {
            open_sig.and_then(|sig| queue.iter().find(|sample| sample.sig == *sig))
        });
        let missing_open = open_sig.is_some() && found.is_none();
        // Age must freeze at the open buy. A missing open sig used to fall
        // back to `now`, so retries during seek inflated age past the min.
        let open_at = found.map(|sample| sample.at);
        let mut seen = HashSet::new();
        let mut legs: Vec<(Option<Pubkey>, u64)> = Vec::new();
        let mut unpriced = missing_open;
        if let Some(open_at) = open_at {
            let cutoff = open_at.checked_sub(lookback).unwrap_or(open_at);
            if let Some(queue) = queue {
                for sample in queue {
                    if sample.at < cutoff || sample.at > open_at {
                        continue;
                    }
                    if !seen.insert(sample.sig) {
                        continue;
                    }
                    if sample.unpriced {
                        unpriced = true;
                        continue;
                    }
                    if sample.atoms == 0 {
                        continue;
                    }
                    if let Some((_, atoms)) =
                        legs.iter_mut().find(|(quote, _)| *quote == sample.quote)
                    {
                        *atoms = atoms.saturating_add(sample.atoms);
                    } else {
                        legs.push((sample.quote, sample.atoms));
                    }
                }
            }
        }
        drop(map);
        let age = match open_at {
            Some(at) => {
                let launches = self.launches.lock().unwrap_or_else(|err| err.into_inner());
                launches.age_at(mint, at)
            }
            // No open buy on the tape (or none asked for). Unknown — do not
            // age from wall clock or a too-young coin clears the min in seek.
            None => f64::NAN,
        };
        (legs, unpriced, age)
    }
}

/// Scan outer + inner ixs for pump `create` / `create_v2`. Returns the mint.
pub fn pump_create_mint(tx: &ParsedTargetTransaction) -> Option<Pubkey> {
    if tx.failed {
        return None;
    }
    for ix in tx.instructions.iter().chain(tx.inner_instructions.iter()) {
        if let Some(mint) = create_mint_from_ix(tx, ix) {
            return Some(mint);
        }
    }
    None
}

fn create_mint_from_ix(tx: &ParsedTargetTransaction, ix: &CompiledIx) -> Option<Pubkey> {
    let program = tx.account_keys.get(ix.program_id_index)?.parse::<Pubkey>().ok()?;
    if program != *PUMP_PROGRAM {
        return None;
    }
    if ix.data.len() < 8 {
        return None;
    }
    let disc: [u8; 8] = ix.data[..8].try_into().ok()?;
    if disc != PUMP_CREATE_DISCRIMINATOR && disc != PUMP_CREATE_V2_DISCRIMINATOR {
        return None;
    }
    let mint_i = *ix.accounts.get(PUMP_CREATE_MINT_ACCOUNT_INDEX)?;
    tx.account_keys.get(mint_i)?.parse().ok()
}

fn evict_coldest(map: &mut HashMap<Pubkey, VecDeque<Sample>>) {
    let Some(dead) = map
        .iter()
        .min_by_key(|(_, queue)| {
            queue
                .back()
                .map(|sample| sample.at)
                .unwrap_or_else(Instant::now)
        })
        .map(|(mint, _)| *mint)
    else {
        return;
    };
    map.remove(&dead);
}

pub fn observe(tx: &ParsedTargetTransaction, tape: &V071Tape, keep: Duration) {
    if let Some(mint) = pump_create_mint(tx) {
        tape.record_launch(mint);
    }
    if tx.failed {
        return;
    }
    let Ok(signature) = tx.signature.parse::<Signature>() else {
        return;
    };
    let mut by_mint: HashMap<Pubkey, (u64, Option<Pubkey>, bool)> = HashMap::new();
    for trade in decoder::decode(tx) {
        let Ok(mint) = trade.descriptor.mint.parse::<Pubkey>() else {
            continue;
        };
        let atoms = trade.event.sol_amount;
        let quote_mint = trade.descriptor.quote_mint.as_deref().or_else(|| {
            trade
                .event
                .curve
                .as_ref()
                .and_then(|curve| curve.quote_mint.as_deref())
        });
        let (quote, bad) = if atoms == 0 || is_sol_quote(quote_mint) {
            (None, false)
        } else {
            match quote_mint.and_then(|quote| quote.parse().ok()) {
                Some(quote) => (Some(quote), false),
                None => (None, true),
            }
        };
        match by_mint.get_mut(&mint) {
            None => {
                by_mint.insert(mint, (atoms, quote, bad));
            }
            Some(entry) => {
                if atoms == 0 {
                    continue;
                }
                if entry.2 || bad || (entry.0 > 0 && entry.1 != quote) {
                    entry.2 = true;
                } else {
                    entry.1 = quote;
                    entry.0 = entry.0.saturating_add(atoms);
                }
            }
        }
    }
    let at = Instant::now();
    for (mint, (atoms, quote, bad)) in by_mint {
        tape.push_sample(mint, atoms, quote, bad, signature, at, keep);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(n: u8) -> Signature {
        Signature::from([n; 64])
    }

    #[test]
    fn volume_includes_his_open_and_the_lookback_only() {
        let book = V071Tape::new(Duration::from_secs(600));
        let mint = Pubkey::new_from_array([1u8; 32]);
        let keep = Duration::from_secs(60);
        let open_at = Instant::now();
        book.record_launch_at(mint, open_at - Duration::from_secs(30));
        book.push_at(mint, 9.0, sig(1), open_at - Duration::from_secs(8), keep);
        book.push_at(mint, 2.0, sig(2), open_at - Duration::from_secs(3), keep);
        book.push_at(mint, 1.0, sig(3), open_at, keep);
        book.push_at(mint, 4.0, sig(4), open_at + Duration::from_secs(1), keep);
        let (legs, unpriced, age) = book.open_window(&mint, Duration::from_secs(5), Some(&sig(3)));
        assert!(!unpriced);
        let volume: f64 = legs.iter().map(|(_, atoms)| *atoms as f64 / 1e9).sum();
        assert!((volume - 3.0).abs() < 1e-9, "vol={volume}");
        assert!((age - 30.0).abs() < 0.05, "age={age}");
    }

    #[test]
    fn missing_create_makes_age_unknown() {
        let book = V071Tape::new(Duration::from_secs(600));
        let mint = Pubkey::new_from_array([2u8; 32]);
        let (legs, unpriced, age) = book.open_window(&mint, Duration::from_secs(5), None);
        assert!(legs.is_empty());
        assert!(!unpriced);
        assert!(!age.is_finite(), "age={age}");
    }

    #[test]
    fn alt_quote_keeps_atoms_for_a_later_sol_rate() {
        let book = V071Tape::new(Duration::from_secs(600));
        let mint = Pubkey::new_from_array([3u8; 32]);
        let quote = Pubkey::new_from_array([9u8; 32]);
        let keep = Duration::from_secs(60);
        let open_at = Instant::now();
        book.push_sample(
            mint,
            50_000_000,
            Some(quote),
            false,
            sig(1),
            open_at - Duration::from_secs(1),
            keep,
        );
        book.push_sample(mint, 150_000_000, Some(quote), false, sig(2), open_at, keep);
        let (legs, unpriced, _) = book.open_window(&mint, Duration::from_secs(5), Some(&sig(2)));
        assert!(!unpriced);
        assert_eq!(legs, vec![(Some(quote), 200_000_000)]);
    }

    #[test]
    fn zero_size_buy_ends_the_window() {
        let book = V071Tape::new(Duration::from_secs(600));
        let mint = Pubkey::new_from_array([4u8; 32]);
        let keep = Duration::from_secs(60);
        let open_at = Instant::now();
        book.push_sample(
            mint,
            2_000_000_000,
            None,
            false,
            sig(2),
            open_at - Duration::from_secs(1),
            keep,
        );
        book.push_sample(mint, 0, None, false, sig(3), open_at, keep);
        book.push_sample(
            mint,
            4_000_000_000,
            None,
            false,
            sig(4),
            open_at + Duration::from_secs(1),
            keep,
        );
        let (legs, unpriced, _) = book.open_window(&mint, Duration::from_secs(5), Some(&sig(3)));
        assert!(!unpriced);
        let volume: f64 = legs.iter().map(|(_, atoms)| *atoms as f64 / 1e9).sum();
        assert!((volume - 2.0).abs() < 1e-9, "vol={volume}");
    }

    #[test]
    fn missing_open_does_not_count_later_prints() {
        let book = V071Tape::new(Duration::from_secs(600));
        let mint = Pubkey::new_from_array([5u8; 32]);
        let keep = Duration::from_secs(60);
        let open_at = Instant::now();
        book.push_at(mint, 4.0, sig(4), open_at + Duration::from_secs(1), keep);
        let (legs, unpriced, _) = book.open_window(&mint, Duration::from_secs(5), Some(&sig(9)));
        assert!(unpriced);
        assert!(legs.is_empty());
    }

    #[test]
    fn missing_open_does_not_let_age_drift_with_wall_clock() {
        // Regression: gate buy age was 19s (min 30) but Instant::now() fallback
        // on a missing open sig let seek retries age past the floor and buy.
        let book = V071Tape::new(Duration::from_secs(600));
        let mint = Pubkey::new_from_array([8u8; 32]);
        let create_at = Instant::now() - Duration::from_secs(19);
        book.record_launch_at(mint, create_at);
        let (_, unpriced, age) =
            book.open_window(&mint, Duration::from_secs(5), Some(&sig(9)));
        assert!(unpriced);
        assert!(!age.is_finite(), "age must stay unknown, got {age}");
    }

    #[test]
    fn a_full_tape_drops_the_quiet_mint() {
        let book = V071Tape::new(Duration::from_secs(600));
        let keep = Duration::from_secs(60);
        let now = Instant::now();
        for n in 0..MAX_MINTS {
            book.push_at(
                mint_at(n as u32),
                1.0,
                sig_at(n as u32),
                now - Duration::from_secs(30),
                keep,
            );
        }
        let hot = mint_at(u32::MAX - 1);
        let fresh = mint_at(u32::MAX - 2);
        book.push_at(hot, 1.0, sig(1), now, keep);
        book.push_at(fresh, 2.0, sig(2), now, keep);
        let (hot_legs, hot_unpriced, _) =
            book.open_window(&hot, Duration::from_secs(60), Some(&sig(1)));
        let (fresh_legs, fresh_unpriced, _) =
            book.open_window(&fresh, Duration::from_secs(60), Some(&sig(2)));
        assert!(!hot_unpriced && !fresh_unpriced);
        assert!(!hot_legs.is_empty());
        assert!(!fresh_legs.is_empty());
    }

    #[test]
    fn an_old_launch_does_not_start_again() {
        let book = V071Tape::new(Duration::from_secs(10));
        let mint = Pubkey::new_from_array([6u8; 32]);
        book.record_launch_at(mint, Instant::now() - Duration::from_secs(30));
        let (_, _, age) = book.open_window(&mint, Duration::from_secs(5), None);
        assert!(!age.is_finite(), "age={age}");
        book.record_launch_at(mint, Instant::now());
        let (_, _, age) = book.open_window(&mint, Duration::from_secs(5), None);
        assert!(!age.is_finite(), "age={age}");
    }

    #[test]
    fn create_discriminator_picks_the_mint_account() {
        let mint = Pubkey::new_from_array([7u8; 32]);
        let tx = ParsedTargetTransaction {
            signature: Signature::from([1u8; 64]).to_string(),
            slot: 1,
            transaction_index: Some(0),
            timestamp_ms: 0,
            account_keys: vec![
                Pubkey::new_from_array([8u8; 32]).to_string(),
                PUMP_PROGRAM.to_string(),
                mint.to_string(),
            ],
            program_ids: vec![PUMP_PROGRAM.to_string()],
            logs: vec![],
            instructions: vec![CompiledIx {
                program_id_index: 1,
                accounts: vec![2],
                data: PUMP_CREATE_V2_DISCRIMINATOR.to_vec(),
            }],
            inner_instructions: vec![],
            pre_token_balances: vec![],
            post_token_balances: vec![],
            pre_balances: vec![],
            post_balances: vec![],
            failed: false,
        };
        assert_eq!(pump_create_mint(&tx), Some(mint));
    }

    fn mint_at(n: u32) -> Pubkey {
        let mut bytes = [0u8; 32];
        bytes[..4].copy_from_slice(&n.to_le_bytes());
        Pubkey::new_from_array(bytes)
    }

    fn sig_at(n: u32) -> Signature {
        let mut bytes = [0u8; 64];
        bytes[..4].copy_from_slice(&n.to_le_bytes());
        Signature::from(bytes)
    }
}
