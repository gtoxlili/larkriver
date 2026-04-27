use super::card::Card;
use super::DeckMode;
use itertools::Itertools;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
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

fn evaluate_5(cards: &[Card], mode: DeckMode) -> HandRank {
    debug_assert_eq!(cards.len(), 5);
    let mut ranks: Vec<u8> = cards.iter().map(|c| c.rank.0).collect();
    ranks.sort_unstable_by(|a, b| b.cmp(a));

    let suit0 = cards[0].suit;
    let is_flush = cards.iter().all(|c| c.suit == suit0);

    let mut sorted_asc = ranks.clone();
    sorted_asc.sort_unstable();
    sorted_asc.dedup();

    let is_straight_normal =
        sorted_asc.len() == 5 && (sorted_asc[4] - sorted_asc[0] == 4);
    // Low-ace wheel differs by mode: 2-3-4-5-A in Standard, 6-7-8-9-A in
    // ShortDeck (since 2-5 don't exist there).
    let low_wheel = match mode {
        DeckMode::Standard => [2u8, 3, 4, 5, 14],
        DeckMode::ShortDeck => [6u8, 7, 8, 9, 14],
    };
    let is_straight_low_ace = sorted_asc == low_wheel;
    let is_straight = is_straight_normal || is_straight_low_ace;
    let straight_high = if is_straight_low_ace {
        // Wheel high card is the 5 (Standard) or the 9 (ShortDeck).
        low_wheel[3]
    } else if is_straight_normal {
        sorted_asc[4]
    } else {
        0
    };

    let mut counts: BTreeMap<u8, u8> = BTreeMap::new();
    for r in &ranks {
        *counts.entry(*r).or_insert(0) += 1;
    }
    let mut count_pairs: Vec<(u8, u8)> = counts.into_iter().map(|(r, c)| (c, r)).collect();
    count_pairs.sort_unstable_by(|a, b| (b.0, b.1).cmp(&(a.0, a.1)));

    // Use the standard category numbering first; remap below for ShortDeck.
    //   0 high · 1 pair · 2 two pair · 3 trips · 4 straight ·
    //   5 flush · 6 full house · 7 quads · 8 straight flush · 9 royal flush
    let raw_category;
    let kickers: [u8; 5];

    if is_flush && is_straight {
        raw_category = if straight_high == 14 { 9 } else { 8 };
        kickers = [straight_high, 0, 0, 0, 0];
    } else if count_pairs[0].0 == 4 {
        raw_category = 7;
        let four = count_pairs[0].1;
        let kicker = ranks.iter().find(|&&r| r != four).copied().unwrap_or(0);
        kickers = [four, kicker, 0, 0, 0];
    } else if count_pairs[0].0 == 3
        && count_pairs.get(1).map_or(false, |x| x.0 >= 2)
    {
        raw_category = 6;
        kickers = [count_pairs[0].1, count_pairs[1].1, 0, 0, 0];
    } else if is_flush {
        raw_category = 5;
        kickers = [ranks[0], ranks[1], ranks[2], ranks[3], ranks[4]];
    } else if is_straight {
        raw_category = 4;
        kickers = [straight_high, 0, 0, 0, 0];
    } else if count_pairs[0].0 == 3 {
        raw_category = 3;
        let three = count_pairs[0].1;
        let mut others: Vec<u8> = ranks.iter().filter(|&&r| r != three).copied().collect();
        others.sort_unstable_by(|a, b| b.cmp(a));
        kickers = [
            three,
            *others.first().unwrap_or(&0),
            *others.get(1).unwrap_or(&0),
            0,
            0,
        ];
    } else if count_pairs[0].0 == 2 && count_pairs.get(1).map_or(false, |x| x.0 == 2) {
        raw_category = 2;
        let high = count_pairs[0].1.max(count_pairs[1].1);
        let low = count_pairs[0].1.min(count_pairs[1].1);
        let kicker = ranks
            .iter()
            .find(|&&r| r != high && r != low)
            .copied()
            .unwrap_or(0);
        kickers = [high, low, kicker, 0, 0];
    } else if count_pairs[0].0 == 2 {
        raw_category = 1;
        let pair = count_pairs[0].1;
        let mut others: Vec<u8> = ranks.iter().filter(|&&r| r != pair).copied().collect();
        others.sort_unstable_by(|a, b| b.cmp(a));
        kickers = [
            pair,
            *others.first().unwrap_or(&0),
            *others.get(1).unwrap_or(&0),
            *others.get(2).unwrap_or(&0),
            0,
        ];
    } else {
        raw_category = 0;
        kickers = [ranks[0], ranks[1], ranks[2], ranks[3], ranks[4]];
    }

    // ShortDeck swaps:
    //   trips (3) ⇄ straight (4)   — trips now beats straight
    //   flush (5) ⇄ full house (6) — flush now beats full house
    let category = match (mode, raw_category) {
        (DeckMode::ShortDeck, 3) => 4,
        (DeckMode::ShortDeck, 4) => 3,
        (DeckMode::ShortDeck, 5) => 6,
        (DeckMode::ShortDeck, 6) => 5,
        (_, c) => c,
    };

    HandRank { category, kickers }
}

pub fn evaluate(cards: &[Card], mode: DeckMode) -> HandRank {
    assert!((5..=7).contains(&cards.len()), "evaluate expects 5-7 cards");
    if cards.len() == 5 {
        return evaluate_5(cards, mode);
    }
    cards
        .iter()
        .copied()
        .combinations(5)
        .map(|combo| evaluate_5(&combo, mode))
        .max()
        .expect("at least one combination exists")
}

/// Return the actual best 5-card subset of `cards` (used for showdown display).
pub fn best_five(cards: &[Card], mode: DeckMode) -> (HandRank, Vec<Card>) {
    cards
        .iter()
        .copied()
        .combinations(5)
        .map(|combo| (evaluate_5(&combo, mode), combo))
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
}
