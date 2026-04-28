//! Monte-Carlo equity (showdown win rate) for the actor's own hole cards.
//!
//! Given the hero's hole cards, the revealed community cards, and the count
//! of opponents still in the hand, simulate `iterations` random complete deals
//! and return the hero's win probability. Ties score as half-wins
//! (split-pot approximation).
//!
//! ## 2026 hot-path overhauls
//! - **Bitmask used-card lookup** — replaces the old `HashSet<Card>` with a
//!   `u64` bit-set indexed by `Card::packed()` (0..52). The used-set check is
//!   one bit-test instead of a hash + probe.
//! - **Per-iteration partial Fisher-Yates** — only shuffles the prefix we
//!   actually consume (`need_total` slots), not the entire 50-card residual.
//! - **`fastrand` thread-local RNG** — single-instruction `usize` draw, vs
//!   `rand`'s thread-local-with-OS-seed reseed loop.
//! - **`rayon` parallelism** — iterations are independent, so we fan out
//!   across all CPU cores and sum the scores. Linear scaling on multi-core.
//! - **`SmallVec` scratch buffers** — the 5-card community + 7-card eval
//!   buffers live entirely on the stack.
//!
//! Cost is now bounded mostly by `evaluate()` itself, which is histogram /
//! bitmask based and never allocates.

use rayon::prelude::*;
use smallvec::SmallVec;

use super::card::Card;
use super::hand::{evaluate, HandRank};
use super::DeckMode;

/// Build the residual deck (cards NOT already in `used_mask`) as a Vec of
/// packed-u8 indices. Order is deterministic: sorted ascending, which the
/// per-iteration Fisher-Yates then breaks.
fn residual_pool(mode: DeckMode, used_mask: u64) -> Vec<u8> {
    let low = match mode {
        DeckMode::Standard => 2u8,
        DeckMode::ShortDeck => 6u8,
    };
    let mut out = Vec::with_capacity((15 - low as usize) * 4);
    for r in low..=14u8 {
        for s in 0..4u8 {
            let p = (r - 2) * 4 + s;
            if used_mask & (1u64 << p) == 0 {
                out.push(p);
            }
        }
    }
    out
}

pub fn equity(
    hole: &[Card],
    community: &[Card],
    n_opponents: usize,
    iterations: u32,
    mode: DeckMode,
) -> f64 {
    if hole.len() != 2 || n_opponents == 0 || iterations == 0 {
        return 0.0;
    }
    let need_community = 5usize.saturating_sub(community.len());
    let need_total = 2 * n_opponents + need_community;

    // Used-card bitmask: one bit per packed card index.
    let mut used: u64 = 0;
    for c in hole.iter().chain(community.iter()) {
        used |= 1u64 << c.packed();
    }
    let pool: Vec<u8> = residual_pool(mode, used);
    if pool.len() < need_total {
        return 0.0;
    }

    // Snapshot inputs for the parallel closure.
    let hero_hole: [u8; 2] = [hole[0].packed(), hole[1].packed()];
    let community_packed: SmallVec<u8, 5> =
        community.iter().map(|c| c.packed()).collect();

    // Parallel Monte-Carlo: each worker keeps a thread-local copy of the
    // pool to perform partial Fisher-Yates without contention.
    let total_score: f64 = (0..iterations)
        .into_par_iter()
        .map_init(
            || pool.clone(),
            |pool, _| simulate_one(
                pool,
                need_total,
                need_community,
                n_opponents,
                hero_hole,
                &community_packed,
                mode,
            ),
        )
        .sum();

    total_score / iterations as f64
}

#[inline]
fn simulate_one(
    pool: &mut [u8],
    need_total: usize,
    need_community: usize,
    n_opponents: usize,
    hero_hole: [u8; 2],
    community_packed: &[u8],
    mode: DeckMode,
) -> f64 {
    // Partial Fisher-Yates: only randomise the first `need_total` slots.
    // Each pick swaps in one fresh random tail card; that's all we need to
    // expose to the iteration's deal logic.
    let n = pool.len();
    let last = need_total.min(n.saturating_sub(1));
    for i in 0..last {
        let j = fastrand::usize(i..n);
        pool.swap(i, j);
    }

    // Build the 5-card community used by everyone in this iteration.
    let mut full_community: SmallVec<Card, 5> = SmallVec::new();
    for &p in community_packed {
        full_community.push(Card::from_packed(p));
    }
    for j in 0..need_community {
        full_community.push(Card::from_packed(pool[2 * n_opponents + j]));
    }

    // Hero: hole + community
    let mut seven: SmallVec<Card, 7> = SmallVec::new();
    seven.push(Card::from_packed(hero_hole[0]));
    seven.push(Card::from_packed(hero_hole[1]));
    seven.extend_from_slice(&full_community);
    let hero_rank = evaluate(&seven, mode);

    // Best opponent
    let mut max_opp: Option<HandRank> = None;
    for i in 0..n_opponents {
        seven.clear();
        seven.push(Card::from_packed(pool[2 * i]));
        seven.push(Card::from_packed(pool[2 * i + 1]));
        seven.extend_from_slice(&full_community);
        let r = evaluate(&seven, mode);
        max_opp = match max_opp {
            Some(m) if m >= r => Some(m),
            _ => Some(r),
        };
    }
    let max_opp = max_opp.expect("n_opponents > 0 enforced by caller");

    if hero_rank > max_opp {
        1.0
    } else if hero_rank == max_opp {
        0.5
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::super::card::{Card, Rank, Suit};
    use super::*;

    fn c(r: u8, s: Suit) -> Card {
        Card { rank: Rank(r), suit: s }
    }

    #[test]
    fn pocket_aces_dominates_preflop() {
        let aa = vec![c(14, Suit::Spades), c(14, Suit::Hearts)];
        let eq_heads_up = equity(&aa, &[], 1, 1000, DeckMode::Standard);
        // Pocket aces head-up vs random hand is ~85% — give wide bounds for the
        // sample variance at 1k iters.
        assert!(eq_heads_up > 0.78, "expected AA HU equity > 0.78, got {eq_heads_up}");
        assert!(eq_heads_up < 0.92);
    }

    #[test]
    fn made_flush_on_river_is_near_certain() {
        let hole = vec![c(14, Suit::Spades), c(13, Suit::Spades)];
        let board = vec![
            c(2, Suit::Spades),
            c(7, Suit::Spades),
            c(9, Suit::Spades),
            c(11, Suit::Hearts),
            c(4, Suit::Diamonds),
        ];
        let eq = equity(&hole, &board, 1, 500, DeckMode::Standard);
        // Ace-high flush on the river — opponent can only beat us with a
        // straight flush or higher. Should be > 0.95.
        assert!(eq > 0.95, "expected near-lock equity, got {eq}");
    }

    #[test]
    fn empty_inputs_dont_panic() {
        assert_eq!(equity(&[], &[], 1, 100, DeckMode::Standard), 0.0);
        let aa = vec![c(14, Suit::Spades), c(14, Suit::Hearts)];
        assert_eq!(equity(&aa, &[], 0, 100, DeckMode::Standard), 0.0);
    }

    #[test]
    fn shortdeck_aa_preflop_lower_than_standard() {
        // In short deck (36-card), AA pre-flop has noticeably lower equity
        // because opponents are more likely to flop sets / straights.
        let aa = vec![c(14, Suit::Spades), c(14, Suit::Hearts)];
        let eq_std = equity(&aa, &[], 4, 1000, DeckMode::Standard);
        let eq_short = equity(&aa, &[], 4, 1000, DeckMode::ShortDeck);
        assert!(
            eq_short < eq_std,
            "short-deck AA equity should be lower: std={eq_std} short={eq_short}"
        );
    }
}
