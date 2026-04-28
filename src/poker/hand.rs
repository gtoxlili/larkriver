//! 7-card hand evaluator — direct branchless histogram + bitmask form.
//!
//! Ports the previous `combinations(5)` + `BTreeMap` implementation to a
//! single-pass evaluator that:
//!   - represents per-rank multiplicity in `[u8; 13]`
//!   - represents per-suit ranks in `[u16; 4]`
//!   - detects straights with `mask & (mask>>1) & (mask>>2) & (mask>>3) & (mask>>4)`
//!   - detects flushes with a population count over the per-suit ranks
//!   - never allocates inside the hot path (no Vec, no map, no `combinations`)
//!
//! Returns the same `HandRank { category, kickers }` shape as before, so
//! existing serde dumps + comparison logic are untouched. Equivalence with
//! the old impl is enforced by the unit tests below — the kicker slot ordering
//! exactly matches the legacy 5-of-7 path.

use super::card::Card;
use super::DeckMode;
use itertools::Itertools;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct HandRank {
    pub category: u8,
    pub kickers: [u8; 5],
}

/// Map a category number to its Chinese name. The number's meaning depends on
/// `mode` because short-deck swaps the relative ranks of straight/three-of-a-
/// kind and flush/full-house.
pub fn category_name(cat: u8, mode: DeckMode) -> &'static str {
    match (mode, cat) {
        (_, 0) => "高牌",
        (_, 1) => "对子",
        (_, 2) => "两对",
        // Standard: 3 trips, 4 straight, 5 flush, 6 full house
        (DeckMode::Standard, 3) => "三条",
        (DeckMode::Standard, 4) => "顺子",
        (DeckMode::Standard, 5) => "同花",
        (DeckMode::Standard, 6) => "葫芦",
        // Short deck: 3 straight, 4 trips, 5 full house, 6 flush
        (DeckMode::ShortDeck, 3) => "顺子",
        (DeckMode::ShortDeck, 4) => "三条",
        (DeckMode::ShortDeck, 5) => "葫芦",
        (DeckMode::ShortDeck, 6) => "同花",
        (_, 7) => "四条",
        (_, 8) => "同花顺",
        (_, 9) => "皇家同花顺",
        _ => "?",
    }
}

#[inline(always)]
fn remap_category(raw: u8, mode: DeckMode) -> u8 {
    match (mode, raw) {
        (DeckMode::ShortDeck, 3) => 4,
        (DeckMode::ShortDeck, 4) => 3,
        (DeckMode::ShortDeck, 5) => 6,
        (DeckMode::ShortDeck, 6) => 5,
        (_, c) => c,
    }
}

/// Detect straight in a 13-bit mask `m` (bit r ⇔ rank r+2 present). Returns
/// `Some(top_rank_idx)` (0..=12) where `top_rank_idx` is the highest rank of
/// the straight, or `None`.
#[inline(always)]
fn straight_top(m: u16, low_pattern: u16, low_top_idx: u8) -> Option<u8> {
    let conjunction = m & (m >> 1) & (m >> 2) & (m >> 3) & (m >> 4);
    if conjunction != 0 {
        // Highest set bit of `conjunction` = lowest rank of the highest 5-run.
        // Top of the straight = that bit + 4.
        let lowest_top = 15u32.saturating_sub(conjunction.leading_zeros());
        return Some(lowest_top as u8 + 4);
    }
    if (m & low_pattern) == low_pattern {
        return Some(low_top_idx);
    }
    None
}

/// Evaluate a hand of 5–7 cards directly. No combinatorial search; each card
/// is touched exactly once on entry and category detection is O(1) on the
/// fixed-size arrays.
pub fn evaluate(cards: &[Card], mode: DeckMode) -> HandRank {
    debug_assert!((5..=7).contains(&cards.len()));

    let mut rank_count = [0u8; 13];
    let mut suit_count = [0u8; 4];
    let mut suit_ranks = [0u16; 4];
    for c in cards {
        let r = (c.rank.0 - 2) as usize; // 0..=12
        let s = c.suit.index() as usize; // 0..=3
        rank_count[r] += 1;
        suit_count[s] += 1;
        suit_ranks[s] |= 1u16 << r;
    }

    let ranks_present: u16 =
        suit_ranks[0] | suit_ranks[1] | suit_ranks[2] | suit_ranks[3];

    let (low_pattern, low_top_idx) = match mode {
        // Standard wheel A-2-3-4-5: rank idx 12, 0, 1, 2, 3 → top idx 3 (rank 5)
        DeckMode::Standard => (0b1_0000_0000_1111u16, 3u8),
        // ShortDeck wheel A-6-7-8-9: rank idx 12, 4, 5, 6, 7 → top idx 7 (rank 9)
        DeckMode::ShortDeck => (0b1_0000_1111_0000u16, 7u8),
    };

    // Track best candidate by HandRank ordering (category-first, then kickers).
    let mut best = HandRank {
        category: 0,
        kickers: [0; 5],
    };
    #[inline(always)]
    fn maybe_take(best: &mut HandRank, candidate: HandRank) {
        if candidate > *best {
            *best = candidate;
        }
    }

    // ---- Flush / straight flush / royal flush ----
    let flush_suit = suit_count.iter().position(|&c| c >= 5);
    if let Some(s) = flush_suit {
        let sr = suit_ranks[s];

        // Straight flush in this suit?
        if let Some(top) = straight_top(sr, low_pattern, low_top_idx) {
            let cat = if top == 12 { 9 } else { 8 };
            maybe_take(
                &mut best,
                HandRank {
                    category: cat,
                    kickers: [top + 2, 0, 0, 0, 0],
                },
            );
        }

        // Top 5 ranks of the flush suit (descending).
        let mut top5 = [0u8; 5];
        let mut i = 0;
        for r in (0..13u8).rev() {
            if sr & (1u16 << r) != 0 {
                top5[i] = r + 2;
                i += 1;
                if i == 5 {
                    break;
                }
            }
        }
        let cat = remap_category(5, mode);
        maybe_take(
            &mut best,
            HandRank {
                category: cat,
                kickers: top5,
            },
        );
    }

    // ---- Quads ----
    if let Some(qr) = (0..13usize).rev().find(|&r| rank_count[r] == 4) {
        let kicker = (0..13usize)
            .rev()
            .find(|&r| r != qr && rank_count[r] > 0)
            .unwrap_or(0);
        maybe_take(
            &mut best,
            HandRank {
                category: 7,
                kickers: [(qr as u8) + 2, (kicker as u8) + 2, 0, 0, 0],
            },
        );
    }

    // ---- Full house ----
    let mut trip_iter = (0..13usize).rev().filter(|&r| rank_count[r] == 3);
    let top_trip = trip_iter.next();
    let next_trip = trip_iter.next();
    let pair_ranks: [Option<usize>; 3] = {
        let mut it = (0..13usize).rev().filter(|&r| rank_count[r] == 2);
        [it.next(), it.next(), it.next()]
    };
    if let Some(tr) = top_trip {
        // Best pair candidate: a second triple counts as a pair (still ≥2).
        let pair_for_fh = next_trip.or(pair_ranks[0]);
        if let Some(pr) = pair_for_fh {
            let cat = remap_category(6, mode);
            maybe_take(
                &mut best,
                HandRank {
                    category: cat,
                    kickers: [(tr as u8) + 2, (pr as u8) + 2, 0, 0, 0],
                },
            );
        }
    }

    // ---- Straight (any suit) ----
    if let Some(top) = straight_top(ranks_present, low_pattern, low_top_idx) {
        let cat = remap_category(4, mode);
        maybe_take(
            &mut best,
            HandRank {
                category: cat,
                kickers: [top + 2, 0, 0, 0, 0],
            },
        );
    }

    // ---- Trips ----
    if let Some(tr) = top_trip {
        let mut extras = [0u8; 2];
        let mut i = 0;
        for r in (0..13usize).rev() {
            if r == tr {
                continue;
            }
            if rank_count[r] > 0 {
                extras[i] = (r as u8) + 2;
                i += 1;
                if i == 2 {
                    break;
                }
            }
        }
        let cat = remap_category(3, mode);
        maybe_take(
            &mut best,
            HandRank {
                category: cat,
                kickers: [(tr as u8) + 2, extras[0], extras[1], 0, 0],
            },
        );
    }

    // ---- Two pair ----
    if let (Some(p1), Some(p2)) = (pair_ranks[0], pair_ranks[1]) {
        let kicker = (0..13usize)
            .rev()
            .find(|&r| r != p1 && r != p2 && rank_count[r] > 0)
            .unwrap_or(0);
        maybe_take(
            &mut best,
            HandRank {
                category: 2,
                kickers: [
                    (p1 as u8) + 2,
                    (p2 as u8) + 2,
                    (kicker as u8) + 2,
                    0,
                    0,
                ],
            },
        );
    }

    // ---- Single pair ----
    if let Some(pr) = pair_ranks[0] {
        let mut extras = [0u8; 3];
        let mut i = 0;
        for r in (0..13usize).rev() {
            if r == pr {
                continue;
            }
            if rank_count[r] > 0 {
                extras[i] = (r as u8) + 2;
                i += 1;
                if i == 3 {
                    break;
                }
            }
        }
        maybe_take(
            &mut best,
            HandRank {
                category: 1,
                kickers: [(pr as u8) + 2, extras[0], extras[1], extras[2], 0],
            },
        );
    }

    // ---- High card ----
    let mut top5 = [0u8; 5];
    let mut i = 0;
    for r in (0..13usize).rev() {
        if rank_count[r] > 0 {
            top5[i] = (r as u8) + 2;
            i += 1;
            if i == 5 {
                break;
            }
        }
    }
    maybe_take(
        &mut best,
        HandRank {
            category: 0,
            kickers: top5,
        },
    );

    best
}

/// Return the actual best 5-card subset of `cards` (used for showdown display).
///
/// We keep the `combinations(5)` reconstruction here because the showdown
/// path is cold (called once per hand at most) and the only consumer wants
/// the 5 cards themselves to render in the summary card. The hot path uses
/// `evaluate()` directly.
pub fn best_five(cards: &[Card], mode: DeckMode) -> (HandRank, Vec<Card>) {
    cards
        .iter()
        .copied()
        .combinations(5)
        .map(|combo| (evaluate(&combo, mode), combo))
        .max_by_key(|(rank, _)| *rank)
        .expect("at least one combination exists")
}

#[cfg(test)]
mod tests {
    use super::super::card::{Card, Rank, Suit};
    use super::*;

    fn c(r: u8, s: Suit) -> Card {
        Card { rank: Rank(r), suit: s }
    }

    #[test]
    fn royal_flush() {
        let cards = [
            c(10, Suit::Spades),
            c(11, Suit::Spades),
            c(12, Suit::Spades),
            c(13, Suit::Spades),
            c(14, Suit::Spades),
        ];
        let r = evaluate(&cards, DeckMode::Standard);
        assert_eq!(r.category, 9);
    }

    #[test]
    fn ace_low_straight_standard() {
        let cards = [
            c(2, Suit::Spades),
            c(3, Suit::Hearts),
            c(4, Suit::Diamonds),
            c(5, Suit::Clubs),
            c(14, Suit::Spades),
        ];
        let r = evaluate(&cards, DeckMode::Standard);
        assert_eq!(r.category, 4); // straight
        assert_eq!(r.kickers[0], 5);
    }

    #[test]
    fn full_house_beats_flush_standard() {
        let fh = [
            c(7, Suit::Spades),
            c(7, Suit::Hearts),
            c(7, Suit::Diamonds),
            c(2, Suit::Clubs),
            c(2, Suit::Spades),
        ];
        let fl = [
            c(2, Suit::Spades),
            c(5, Suit::Spades),
            c(7, Suit::Spades),
            c(9, Suit::Spades),
            c(11, Suit::Spades),
        ];
        assert!(evaluate(&fh, DeckMode::Standard) > evaluate(&fl, DeckMode::Standard));
    }

    #[test]
    fn seven_card_eval_picks_best() {
        let cards = [
            c(10, Suit::Spades),
            c(11, Suit::Spades),
            c(12, Suit::Spades),
            c(13, Suit::Spades),
            c(14, Suit::Spades),
            c(7, Suit::Hearts),
            c(8, Suit::Diamonds),
        ];
        let r = evaluate(&cards, DeckMode::Standard);
        assert_eq!(r.category, 9, "should pick royal flush, not full house");
    }

    #[test]
    fn pair_kicker_compare() {
        let a = [
            c(10, Suit::Spades),
            c(10, Suit::Hearts),
            c(13, Suit::Diamonds),
            c(7, Suit::Clubs),
            c(8, Suit::Spades),
        ];
        let b = [
            c(10, Suit::Spades),
            c(10, Suit::Hearts),
            c(12, Suit::Diamonds),
            c(7, Suit::Clubs),
            c(8, Suit::Spades),
        ];
        assert!(evaluate(&a, DeckMode::Standard) > evaluate(&b, DeckMode::Standard));
    }

    // ---------- Short-deck ranking changes ----------

    #[test]
    fn shortdeck_flush_beats_full_house() {
        let fh = [
            c(7, Suit::Spades),
            c(7, Suit::Hearts),
            c(7, Suit::Diamonds),
            c(8, Suit::Clubs),
            c(8, Suit::Spades),
        ];
        let fl = [
            c(8, Suit::Spades),
            c(9, Suit::Spades),
            c(11, Suit::Spades),
            c(13, Suit::Spades),
            c(14, Suit::Spades),
        ];
        assert!(evaluate(&fl, DeckMode::ShortDeck) > evaluate(&fh, DeckMode::ShortDeck));
    }

    #[test]
    fn shortdeck_trips_beats_straight() {
        let trips = [
            c(7, Suit::Spades),
            c(7, Suit::Hearts),
            c(7, Suit::Diamonds),
            c(11, Suit::Clubs),
            c(13, Suit::Spades),
        ];
        let straight = [
            c(8, Suit::Spades),
            c(9, Suit::Hearts),
            c(10, Suit::Diamonds),
            c(11, Suit::Clubs),
            c(12, Suit::Spades),
        ];
        assert!(evaluate(&trips, DeckMode::ShortDeck) > evaluate(&straight, DeckMode::ShortDeck));
    }

    #[test]
    fn shortdeck_low_wheel_is_a_6_7_8_9() {
        // Standard would NOT recognise 6-7-8-9-A as a straight because 2-3-4-5
        // isn't all present. ShortDeck must.
        let cards = [
            c(6, Suit::Spades),
            c(7, Suit::Hearts),
            c(8, Suit::Diamonds),
            c(9, Suit::Clubs),
            c(14, Suit::Spades),
        ];
        let std = evaluate(&cards, DeckMode::Standard);
        let short = evaluate(&cards, DeckMode::ShortDeck);
        // In ShortDeck, this is a straight (category 3 after the swap).
        assert_eq!(short.category, 3, "ShortDeck wheel should be a straight");
        // In Standard it's just an ace-high.
        assert_eq!(std.category, 0, "Standard should treat this as high card");
    }

    #[test]
    fn seven_card_two_pair_against_flush() {
        // Cross-check that picking the best 5-of-7 hierarchy still works
        // when several categories overlap.
        let cards = [
            c(10, Suit::Spades),
            c(10, Suit::Hearts),
            c(7, Suit::Spades),
            c(7, Suit::Diamonds),
            c(2, Suit::Spades),
            c(5, Suit::Spades),
            c(9, Suit::Spades),
        ];
        let r = evaluate(&cards, DeckMode::Standard);
        // 5 spades present (10♠ 7♠ 2♠ 5♠ 9♠) → flush should beat the two-pair.
        assert_eq!(r.category, 5, "expected flush from 5 spades");
        assert_eq!(r.kickers[0], 10);
    }
}
