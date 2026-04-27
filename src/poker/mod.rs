pub mod card;
pub mod equity;
pub mod hand;

pub use card::{Card, Deck, Rank, Suit};
pub use equity::equity;
pub use hand::{best_five, category_name, HandRank};

/// Which Texas Hold'em variant is in play.
///
/// `Standard` is the usual 52-card game.
///
/// `ShortDeck` (a.k.a. 6+ Hold'em) drops 2/3/4/5, plays with 36 cards (6-A).
/// Hand rankings shift because card frequencies change:
///
/// - **Flush > Full house** — only 9 cards per suit, flushes get rarer than
///   full houses.
/// - **Three of a kind > Straight** — straights are more common (consecutive
///   ranks span 5 of 9 instead of 5 of 13).
/// - The low straight uses the ace as the bottom: **A-6-7-8-9** (vs A-2-3-4-5
///   in Standard).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum DeckMode {
    Standard,
    ShortDeck,
}
