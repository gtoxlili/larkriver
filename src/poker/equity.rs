//! Monte-Carlo equity (showdown win rate) for the actor's own ephemeral card.
//!
//! Given the actor's hole cards, the revealed community cards, and the count
//! of opponents still in the hand, simulate `iterations` random complete deals
//! and return the actor's win probability. Ties are scored as half-wins
//! (split-pot approximation, good enough for chat-poker UX).
//!
//! Cost is roughly `iterations * (n_opponents + 1) * 21 * O(evaluate_5)` —
//! ~50–250 ms at 2000 iterations on a modern CPU. Run this off the lock.

use rand::seq::SliceRandom;
use std::collections::HashSet;

use super::card::{Card, Rank, Suit};
use super::hand::evaluate;
use super::DeckMode;

fn full_deck(mode: DeckMode) -> Vec<Card> {
    let low = match mode {
        DeckMode::Standard => 2u8,
        DeckMode::ShortDeck => 6u8,
    };
    let mut cards = Vec::with_capacity(52);
    for r in low..=14u8 {
        for s in [Suit::Spades, Suit::Hearts, Suit::Diamonds, Suit::Clubs] {
            cards.push(Card {
                rank: Rank(r),
                suit: s,
            });
        }
    }
    cards
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

    let used: HashSet<Card> = hole.iter().chain(community.iter()).copied().collect();
    let mut remaining: Vec<Card> = full_deck(mode).into_iter().filter(|c| !used.contains(c)).collect();
    if remaining.len() < need_total {
        return 0.0;
    }

    let mut rng = rand::rng();
    let mut score = 0.0;
    // Reusable scratch buffers — keep allocations out of the hot loop.
    let mut full_community: Vec<Card> = Vec::with_capacity(5);
    let mut seven: Vec<Card> = Vec::with_capacity(7);

    for _ in 0..iterations {
        remaining.shuffle(&mut rng);

        // Slice plan inside `remaining`:
        //   [0 .. 2*n_opponents)                    opponent hole cards
        //   [2*n_opponents .. 2*n_opp + need_comm)  fill-in community

        full_community.clear();
        full_community.extend_from_slice(community);
        for j in 0..need_community {
            full_community.push(remaining[2 * n_opponents + j]);
        }

        // Hero
        seven.clear();
        seven.extend_from_slice(hole);
        seven.extend_from_slice(&full_community);
        let hero_rank = evaluate(&seven, mode);

        // Best opponent
        let mut max_opp = None;
        for i in 0..n_opponents {
            seven.clear();
            seven.push(remaining[2 * i]);
            seven.push(remaining[2 * i + 1]);
            seven.extend_from_slice(&full_community);
            let r = evaluate(&seven, mode);
            max_opp = match max_opp {
                Some(m) if m >= r => Some(m),
                _ => Some(r),
            };
        }
        let max_opp = max_opp.unwrap();

        if hero_rank > max_opp {
            score += 1.0;
        } else if hero_rank == max_opp {
            score += 0.5;
        }
    }

    score / iterations as f64
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
