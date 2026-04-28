use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use super::DeckMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Suit {
    Spades,
    Hearts,
    Diamonds,
    Clubs,
}

impl Suit {
    pub fn symbol(self) -> &'static str {
        match self {
            Suit::Spades => "♠",
            Suit::Hearts => "♥",
            Suit::Diamonds => "♦",
            Suit::Clubs => "♣",
        }
    }

    /// 0..=3 packed index — matches the encoding used by [`Card::packed`].
    #[inline(always)]
    pub fn index(self) -> u8 {
        match self {
            Suit::Spades => 0,
            Suit::Hearts => 1,
            Suit::Diamonds => 2,
            Suit::Clubs => 3,
        }
    }

    #[inline(always)]
    pub fn from_index(i: u8) -> Self {
        match i & 0b11 {
            0 => Suit::Spades,
            1 => Suit::Hearts,
            2 => Suit::Diamonds,
            _ => Suit::Clubs,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Rank(pub u8); // 2..=14, 14 == Ace

impl Rank {
    pub fn label(self) -> String {
        match self.0 {
            14 => "A".into(),
            13 => "K".into(),
            12 => "Q".into(),
            11 => "J".into(),
            10 => "10".into(),
            n => n.to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Card {
    pub rank: Rank,
    pub suit: Suit,
}

impl Card {
    pub fn label(self) -> String {
        format!("{}{}", self.rank.label(), self.suit.symbol())
    }

    /// Pack a card into a single 0..52 index: `rank_idx * 4 + suit_idx`,
    /// where `rank_idx = rank - 2` (0..=12). Used for the `u64` bitset
    /// representation that the hot eval / equity paths operate on.
    #[inline(always)]
    pub fn packed(self) -> u8 {
        (self.rank.0 - 2) * 4 + self.suit.index()
    }

    /// Inverse of [`Card::packed`].
    #[inline(always)]
    pub fn from_packed(p: u8) -> Self {
        debug_assert!(p < 52);
        let suit = Suit::from_index(p & 0b11);
        let rank = Rank(2 + (p >> 2));
        Card { rank, suit }
    }
}

/// In-memory deck used to draw hole / community cards during a hand.
///
/// Stores the cards as a stack (top = last) — `draw()` pops in O(1) and the
/// shuffle uses [`fastrand`] (single-threaded thread-local PRNG, dramatically
/// faster than `rand::rng()` for non-crypto needs).
///
/// Serialised as a `Vec<Card>` so existing redb dumps load unchanged.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Deck {
    cards: Vec<Card>,
}

impl Deck {
    pub fn shuffled(mode: DeckMode) -> Self {
        let low = match mode {
            DeckMode::Standard => 2u8,
            DeckMode::ShortDeck => 6u8,
        };
        let cap = (15 - low as usize) * 4; // 52 or 36
        let mut cards: SmallVec<Card, 52> = SmallVec::with_capacity(cap);
        for r in low..=14u8 {
            for s in [Suit::Spades, Suit::Hearts, Suit::Diamonds, Suit::Clubs] {
                cards.push(Card { rank: Rank(r), suit: s });
            }
        }
        // Fisher–Yates with fastrand. Avoids the rand::seq machinery + thread_rng
        // lookup. Inlined entirely.
        let n = cards.len();
        for i in (1..n).rev() {
            let j = fastrand::usize(0..=i);
            cards.swap(i, j);
        }
        Self { cards: cards.into_vec() }
    }

    pub fn draw(&mut self) -> Option<Card> {
        self.cards.pop()
    }

    pub fn draw_n(&mut self, n: usize) -> Vec<Card> {
        // Match the original pop-order semantics so community-card display
        // and any deck-order-sensitive logic stay byte-identical.
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            match self.cards.pop() {
                Some(c) => out.push(c),
                None => break,
            }
        }
        out
    }
}
