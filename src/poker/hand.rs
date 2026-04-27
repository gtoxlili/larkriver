use super::card::Card;
use itertools::Itertools;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct HandRank {
    pub category: u8,
    pub kickers: [u8; 5],
}

pub fn category_name(cat: u8) -> &'static str {
    match cat {
        0 => "高牌",
        1 => "对子",
        2 => "两对",
        3 => "三条",
        4 => "顺子",
        5 => "同花",
        6 => "葫芦",
        7 => "四条",
        8 => "同花顺",
        9 => "皇家同花顺",
        _ => "?",
    }
}

fn evaluate_5(cards: &[Card]) -> HandRank {
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
    let is_straight_low_ace = sorted_asc == [2u8, 3, 4, 5, 14];
    let is_straight = is_straight_normal || is_straight_low_ace;
    let straight_high = if is_straight_low_ace {
        5
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

    let category;
    let kickers: [u8; 5];

    if is_flush && is_straight {
        category = if straight_high == 14 { 9 } else { 8 };
        kickers = [straight_high, 0, 0, 0, 0];
    } else if count_pairs[0].0 == 4 {
        category = 7;
        let four = count_pairs[0].1;
        let kicker = ranks.iter().find(|&&r| r != four).copied().unwrap_or(0);
        kickers = [four, kicker, 0, 0, 0];
    } else if count_pairs[0].0 == 3
        && count_pairs.get(1).map_or(false, |x| x.0 >= 2)
    {
        category = 6;
        kickers = [count_pairs[0].1, count_pairs[1].1, 0, 0, 0];
    } else if is_flush {
        category = 5;
        kickers = [ranks[0], ranks[1], ranks[2], ranks[3], ranks[4]];
    } else if is_straight {
        category = 4;
        kickers = [straight_high, 0, 0, 0, 0];
    } else if count_pairs[0].0 == 3 {
        category = 3;
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
        category = 2;
        let high = count_pairs[0].1.max(count_pairs[1].1);
        let low = count_pairs[0].1.min(count_pairs[1].1);
        let kicker = ranks
            .iter()
            .find(|&&r| r != high && r != low)
            .copied()
            .unwrap_or(0);
        kickers = [high, low, kicker, 0, 0];
    } else if count_pairs[0].0 == 2 {
        category = 1;
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
        category = 0;
        kickers = [ranks[0], ranks[1], ranks[2], ranks[3], ranks[4]];
    }

    HandRank { category, kickers }
}

pub fn evaluate(cards: &[Card]) -> HandRank {
    assert!((5..=7).contains(&cards.len()), "evaluate expects 5-7 cards");
    if cards.len() == 5 {
        return evaluate_5(cards);
    }
    cards
        .iter()
        .copied()
        .combinations(5)
        .map(|combo| evaluate_5(&combo))
        .max()
        .expect("at least one combination exists")
}

/// Return the actual best 5-card subset of `cards` (used for showdown display).
pub fn best_five(cards: &[Card]) -> (HandRank, Vec<Card>) {
    cards
        .iter()
        .copied()
        .combinations(5)
        .map(|combo| (evaluate_5(&combo), combo))
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
        let r = evaluate(&cards);
        assert_eq!(r.category, 9);
    }

    #[test]
    fn ace_low_straight() {
        let cards = [
            c(2, Suit::Spades),
            c(3, Suit::Hearts),
            c(4, Suit::Diamonds),
            c(5, Suit::Clubs),
            c(14, Suit::Spades),
        ];
        let r = evaluate(&cards);
        assert_eq!(r.category, 4);
        assert_eq!(r.kickers[0], 5);
    }

    #[test]
    fn full_house_beats_flush() {
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
        assert!(evaluate(&fh) > evaluate(&fl));
    }

    #[test]
    fn seven_card_eval_picks_best() {
        let cards = [
            c(10, Suit::Spades),
            c(11, Suit::Spades),
            c(12, Suit::Spades),
            c(13, Suit::Spades),
            c(14, Suit::Spades),
            c(2, Suit::Hearts),
            c(2, Suit::Diamonds),
        ];
        let r = evaluate(&cards);
        assert_eq!(r.category, 9, "should pick royal flush, not full house");
    }

    #[test]
    fn pair_kicker_compare() {
        let a = [
            c(10, Suit::Spades),
            c(10, Suit::Hearts),
            c(13, Suit::Diamonds),
            c(5, Suit::Clubs),
            c(2, Suit::Spades),
        ];
        let b = [
            c(10, Suit::Spades),
            c(10, Suit::Hearts),
            c(12, Suit::Diamonds),
            c(5, Suit::Clubs),
            c(2, Suit::Spades),
        ];
        assert!(evaluate(&a) > evaluate(&b));
    }
}
