use rand::seq::SliceRandom;
use rand::thread_rng;
use serde::{Deserialize, Serialize};

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

    pub fn color_md(self) -> &'static str {
        // lark_md supports inline colors via <font color=...>
        match self {
            Suit::Hearts | Suit::Diamonds => "red",
            _ => "grey",
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

    pub fn label_md(self) -> String {
        format!("`{}{}`", self.rank.label(), self.suit.symbol())
    }
}

pub struct Deck {
    cards: Vec<Card>,
}

impl Deck {
    pub fn shuffled() -> Self {
        let mut cards = Vec::with_capacity(52);
        for r in 2..=14u8 {
            for s in [Suit::Spades, Suit::Hearts, Suit::Diamonds, Suit::Clubs] {
                cards.push(Card { rank: Rank(r), suit: s });
            }
        }
        cards.shuffle(&mut thread_rng());
        Self { cards }
    }

    pub fn draw(&mut self) -> Option<Card> {
        self.cards.pop()
    }

    pub fn draw_n(&mut self, n: usize) -> Vec<Card> {
        (0..n).filter_map(|_| self.draw()).collect()
    }
}
