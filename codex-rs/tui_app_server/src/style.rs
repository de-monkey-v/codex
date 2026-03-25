use crate::color::blend;
use crate::color::is_light;
use crate::terminal_palette::StdoutColorLevel;
use crate::terminal_palette::best_color;
use crate::terminal_palette::default_bg;
use crate::terminal_palette::indexed_color;
use crate::terminal_palette::rgb_color;
use crate::terminal_palette::stdout_color_level;
use ratatui::style::Color;
use ratatui::style::Style;

const USER_MESSAGE_BG_ALPHA_LIGHT: f32 = 0.08;
const USER_MESSAGE_BG_ALPHA_DARK: f32 = 0.22;
const USER_MESSAGE_FALLBACK_BG_RGB: (u8, u8, u8) = (224, 224, 224);
const USER_MESSAGE_FALLBACK_FG_RGB: (u8, u8, u8) = (24, 24, 24);
const USER_MESSAGE_FALLBACK_BG_ANSI256_LIGHT: u8 = 252;
const USER_MESSAGE_FALLBACK_BG_ANSI256_DARK: u8 = 238;
const USER_MESSAGE_FALLBACK_FG_ANSI256: u8 = 16;

pub fn user_message_style() -> Style {
    user_message_style_for(default_bg())
}

pub fn proposed_plan_style() -> Style {
    proposed_plan_style_for(default_bg())
}

/// Returns the style for a user-authored message using the provided terminal background.
pub fn user_message_style_for(terminal_bg: Option<(u8, u8, u8)>) -> Style {
    match terminal_bg {
        Some(bg) => Style::default().bg(user_message_bg(bg)),
        None => fallback_user_message_style(),
    }
}

pub fn proposed_plan_style_for(terminal_bg: Option<(u8, u8, u8)>) -> Style {
    match terminal_bg {
        Some(bg) => Style::default().bg(proposed_plan_bg(bg)),
        None => fallback_user_message_style(),
    }
}

#[allow(clippy::disallowed_methods)]
pub fn user_message_bg(terminal_bg: (u8, u8, u8)) -> Color {
    let color = best_color(blended_user_message_bg_target(terminal_bg));
    if color == Color::Reset {
        fallback_user_message_bg_for(stdout_color_level(), Some(terminal_bg))
    } else {
        color
    }
}

#[allow(clippy::disallowed_methods)]
pub fn proposed_plan_bg(terminal_bg: (u8, u8, u8)) -> Color {
    user_message_bg(terminal_bg)
}

fn blended_user_message_bg_target(terminal_bg: (u8, u8, u8)) -> (u8, u8, u8) {
    let (top, alpha) = if is_light(terminal_bg) {
        ((0, 0, 0), USER_MESSAGE_BG_ALPHA_LIGHT)
    } else {
        ((255, 255, 255), USER_MESSAGE_BG_ALPHA_DARK)
    };
    blend(top, terminal_bg, alpha)
}

fn fallback_user_message_style() -> Style {
    let color_level = stdout_color_level();
    Style::default()
        .fg(fallback_user_message_fg_for(color_level))
        .bg(fallback_user_message_bg_for(
            color_level,
            /*terminal_bg*/ None,
        ))
}

fn fallback_user_message_fg_for(color_level: StdoutColorLevel) -> Color {
    match color_level {
        StdoutColorLevel::TrueColor => rgb_color(USER_MESSAGE_FALLBACK_FG_RGB),
        StdoutColorLevel::Ansi256 => indexed_color(USER_MESSAGE_FALLBACK_FG_ANSI256),
        StdoutColorLevel::Ansi16 | StdoutColorLevel::Unknown => Color::Black,
    }
}

fn fallback_user_message_bg_for(
    color_level: StdoutColorLevel,
    terminal_bg: Option<(u8, u8, u8)>,
) -> Color {
    let is_light_terminal = terminal_bg.map(is_light).unwrap_or(true);
    match color_level {
        StdoutColorLevel::TrueColor => {
            if let Some(bg) = terminal_bg {
                rgb_color(blended_user_message_bg_target(bg))
            } else {
                rgb_color(USER_MESSAGE_FALLBACK_BG_RGB)
            }
        }
        StdoutColorLevel::Ansi256 => indexed_color(if is_light_terminal {
            USER_MESSAGE_FALLBACK_BG_ANSI256_LIGHT
        } else {
            USER_MESSAGE_FALLBACK_BG_ANSI256_DARK
        }),
        StdoutColorLevel::Ansi16 | StdoutColorLevel::Unknown => {
            if is_light_terminal {
                Color::Gray
            } else {
                Color::DarkGray
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_terminal_user_messages_get_stronger_gray_panel() {
        assert_eq!(blended_user_message_bg_target((0, 0, 0)), (56, 56, 56));
    }

    #[test]
    fn light_terminal_user_messages_get_visible_gray_panel() {
        assert_eq!(
            blended_user_message_bg_target((255, 255, 255)),
            (234, 234, 234)
        );
    }

    #[test]
    fn unknown_terminal_bg_uses_explicit_truecolor_fallback_panel() {
        let style = fallback_user_message_style_for_test(StdoutColorLevel::TrueColor);
        assert_eq!(style.fg, Some(rgb_color(USER_MESSAGE_FALLBACK_FG_RGB)));
        assert_eq!(style.bg, Some(rgb_color(USER_MESSAGE_FALLBACK_BG_RGB)));
    }

    #[test]
    fn ansi256_fallback_uses_gray_indexes() {
        let style = fallback_user_message_style_for_test(StdoutColorLevel::Ansi256);
        assert_eq!(
            style.fg,
            Some(indexed_color(USER_MESSAGE_FALLBACK_FG_ANSI256))
        );
        assert_eq!(
            style.bg,
            Some(indexed_color(USER_MESSAGE_FALLBACK_BG_ANSI256_LIGHT))
        );
    }

    #[test]
    fn ansi16_fallback_uses_named_colors() {
        let style = fallback_user_message_style_for_test(StdoutColorLevel::Ansi16);
        assert_eq!(style.fg, Some(Color::Black));
        assert_eq!(style.bg, Some(Color::Gray));
    }

    fn fallback_user_message_style_for_test(color_level: StdoutColorLevel) -> Style {
        Style::default()
            .fg(fallback_user_message_fg_for(color_level))
            .bg(fallback_user_message_bg_for(color_level, None))
    }
}
