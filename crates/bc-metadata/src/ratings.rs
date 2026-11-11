//! Star ratings, color labels, and approve/reject flags (issue #10).
//!
//! Thin, typed layer on top of the structured [`Metadata`] model: values are
//! stored as standard XMP properties (`xmp:Rating`, `photoshop:Label`) plus a
//! BridgeCraft-namespaced flag property, so a single XMP packet persists all
//! three and remains interoperable with other tools.
//!
//! Clean-room: XMP Specification Part 1 (`xmp:Rating`, `photoshop:Label`) and
//! the XMP rules for defining a custom namespace.

use crate::{Metadata, Value, NS_BRIDGECRAFT, NS_PHOTOSHOP, NS_XMP};

/// Star rating, 0–5. `0` means "no rating / cleared".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rating(pub u8);

impl Rating {
    pub fn new(stars: u8) -> Self {
        Self(stars.min(5))
    }

    pub fn stars(&self) -> u8 {
        self.0
    }

    pub fn is_clear(&self) -> bool {
        self.0 == 0
    }
}

/// BridgeCraft's fixed color-label set. Unknown strings from other apps degrade
/// to [`ColorLabel::Custom`] rather than erroring.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ColorLabel {
    #[default]
    None,
    Red,
    Yellow,
    Green,
    Blue,
    Purple,
    Custom(String),
}

impl ColorLabel {
    /// The `photoshop:Label` string for this label, if any.
    pub fn as_label_str(&self) -> Option<&str> {
        match self {
            ColorLabel::None => None,
            ColorLabel::Red => Some("Red"),
            ColorLabel::Yellow => Some("Yellow"),
            ColorLabel::Green => Some("Green"),
            ColorLabel::Blue => Some("Blue"),
            ColorLabel::Purple => Some("Purple"),
            ColorLabel::Custom(s) => Some(s.as_str()),
        }
    }

    pub fn from_label_str(s: &str) -> Self {
        match s {
            "" => ColorLabel::None,
            "Red" => ColorLabel::Red,
            "Yellow" => ColorLabel::Yellow,
            "Green" => ColorLabel::Green,
            "Blue" => ColorLabel::Blue,
            "Purple" => ColorLabel::Purple,
            other => ColorLabel::Custom(other.to_string()),
        }
    }

    pub fn is_custom(&self) -> bool {
        matches!(self, ColorLabel::Custom(_))
    }
}

/// Approve/reject flag. There is no cross-app standard, so BridgeCraft stores
/// this under its own namespace (`NS_BRIDGECRAFT`, property `Flag`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Flag {
    #[default]
    None,
    Approved,
    Rejected,
}

impl Flag {
    pub fn as_str(&self) -> Option<&'static str> {
        match self {
            Flag::None => None,
            Flag::Approved => Some("Approved"),
            Flag::Rejected => Some("Rejected"),
        }
    }
}

impl std::str::FromStr for Flag {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "Approved" => Flag::Approved,
            "Rejected" => Flag::Rejected,
            _ => Flag::None,
        })
    }
}

// ---------------------------------------------------------------------------
// Metadata accessors
// ---------------------------------------------------------------------------

impl Metadata {
    /// Read the star rating from `xmp:Rating`.
    pub fn rating(&self) -> Rating {
        let stars = self
            .get(NS_XMP, "Rating")
            .and_then(|v| match v {
                Value::Integer(n) => Some(*n),
                Value::Real(n) => Some(*n as i64),
                Value::Text(s) => s.trim().parse::<i64>().ok(),
                _ => None,
            })
            .unwrap_or(0)
            .clamp(0, 5) as u8;
        Rating(stars)
    }

    /// Set the star rating, writing `xmp:Rating` (0 clears the property).
    pub fn set_rating(&mut self, rating: Rating) {
        if rating.is_clear() {
            let _ = self.remove(NS_XMP, "Rating");
        } else {
            self.set(NS_XMP, "Rating", Value::Integer(rating.stars() as i64));
        }
    }

    /// Read the color label from `photoshop:Label`.
    pub fn color_label(&self) -> ColorLabel {
        match self.text(NS_PHOTOSHOP, "Label") {
            Some(s) => ColorLabel::from_label_str(s),
            None => ColorLabel::None,
        }
    }

    /// Set the color label, writing `photoshop:Label` (None clears it).
    pub fn set_color_label(&mut self, label: ColorLabel) {
        match label.as_label_str() {
            Some(s) => self.set_text(NS_PHOTOSHOP, "Label", s.to_string()),
            None => {
                let _ = self.remove(NS_PHOTOSHOP, "Label");
            }
        }
    }

    /// Read the approve/reject flag from the BridgeCraft namespace.
    pub fn flag(&self) -> Flag {
        match self.text(NS_BRIDGECRAFT, "Flag") {
            Some(s) => s.parse().unwrap_or_default(),
            None => Flag::None,
        }
    }

    /// Set the approve/reject flag (None clears it).
    pub fn set_flag(&mut self, flag: Flag) {
        match flag.as_str() {
            Some(s) => self.set_text(NS_BRIDGECRAFT, "Flag", s.to_string()),
            None => {
                let _ = self.remove(NS_BRIDGECRAFT, "Flag");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rating_roundtrip() {
        let mut m = Metadata::new();
        assert_eq!(m.rating(), Rating(0));
        m.set_rating(Rating::new(4));
        assert_eq!(m.rating(), Rating(4));
        assert_eq!(m.get(NS_XMP, "Rating").and_then(Value::as_integer), Some(4));
        m.set_rating(Rating(0));
        assert!(m.get(NS_XMP, "Rating").is_none());
    }

    #[test]
    fn rating_clamps_to_five() {
        assert_eq!(Rating::new(9).stars(), 5);
    }

    #[test]
    fn color_label_roundtrip_and_unknown() {
        let mut m = Metadata::new();
        m.set_color_label(ColorLabel::Red);
        assert_eq!(m.color_label(), ColorLabel::Red);
        // unknown degrades to custom
        m.set_text(NS_PHOTOSHOP, "Label", "Magenta".to_string());
        assert_eq!(m.color_label(), ColorLabel::Custom("Magenta".to_string()));
        m.set_color_label(ColorLabel::None);
        assert_eq!(m.color_label(), ColorLabel::None);
        assert!(m.get(NS_PHOTOSHOP, "Label").is_none());
    }

    #[test]
    fn flag_roundtrip() {
        let mut m = Metadata::new();
        assert_eq!(m.flag(), Flag::None);
        m.set_flag(Flag::Approved);
        assert_eq!(m.flag(), Flag::Approved);
        assert_eq!(m.text(NS_BRIDGECRAFT, "Flag"), Some("Approved"));
        m.set_flag(Flag::Rejected);
        assert_eq!(m.flag(), Flag::Rejected);
        m.set_flag(Flag::None);
        assert!(m.get(NS_BRIDGECRAFT, "Flag").is_none());
    }

    #[test]
    fn all_three_in_one_packet() {
        let mut m = Metadata::new();
        m.set_rating(Rating::new(3));
        m.set_color_label(ColorLabel::Green);
        m.set_flag(Flag::Approved);
        // single write path: XMP sidecar roundtrip
        let xml = crate::xmp::write_xmp(&m);
        let parsed = crate::xmp::parse_xmp(xml.as_bytes()).unwrap();
        assert_eq!(parsed.rating(), Rating(3));
        assert_eq!(parsed.color_label(), ColorLabel::Green);
        assert_eq!(parsed.flag(), Flag::Approved);
    }
}
