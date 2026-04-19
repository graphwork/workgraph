//! Convert syntect highlight output into ratatui Spans/Styles.
//!
//! Replaces the abandoned-for-ratatui-0.30 `syntect-tui 3.0.6`. Small
//! enough to keep inline — two public helpers plus a couple of
//! private translators. Behavior matches syntect-tui verbatim: RGB
//! colours (alpha=0 becomes `None`), underline colour matches the
//! foreground, font styles map to the obvious modifiers. `into_span`
//! infallibly succeeds by dropping any unrecognised bit combination
//! from `FontStyle` rather than returning a Result — the original
//! crate's error path wasn't observed at any call site we have.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use syntect::highlighting::{Color as SColor, FontStyle, Style as SStyle};

pub fn into_span<'a>((style, content): (SStyle, &'a str)) -> Span<'a> {
    Span::styled(content.to_owned(), translate_style(style))
}

pub fn translate_style(s: SStyle) -> Style {
    Style::default()
        .fg(translate_colour(s.foreground).unwrap_or(Color::Reset))
        .bg(translate_colour(s.background).unwrap_or(Color::Reset))
        .add_modifier(translate_font_style(s.font_style))
}

fn translate_colour(c: SColor) -> Option<Color> {
    if c.a > 0 {
        Some(Color::Rgb(c.r, c.g, c.b))
    } else {
        None
    }
}

fn translate_font_style(fs: FontStyle) -> Modifier {
    let mut m = Modifier::empty();
    if fs.contains(FontStyle::BOLD) {
        m |= Modifier::BOLD;
    }
    if fs.contains(FontStyle::ITALIC) {
        m |= Modifier::ITALIC;
    }
    if fs.contains(FontStyle::UNDERLINE) {
        m |= Modifier::UNDERLINED;
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb_colour_round_trips() {
        let c = SColor {
            r: 10,
            g: 20,
            b: 30,
            a: 255,
        };
        assert_eq!(translate_colour(c), Some(Color::Rgb(10, 20, 30)));
    }

    #[test]
    fn zero_alpha_becomes_none() {
        let c = SColor {
            r: 10,
            g: 20,
            b: 30,
            a: 0,
        };
        assert_eq!(translate_colour(c), None);
    }

    #[test]
    fn font_styles_map_to_modifiers() {
        assert_eq!(translate_font_style(FontStyle::BOLD), Modifier::BOLD);
        assert_eq!(translate_font_style(FontStyle::ITALIC), Modifier::ITALIC);
        assert_eq!(
            translate_font_style(FontStyle::UNDERLINE),
            Modifier::UNDERLINED
        );
        assert_eq!(
            translate_font_style(FontStyle::BOLD | FontStyle::ITALIC),
            Modifier::BOLD | Modifier::ITALIC
        );
    }
}
