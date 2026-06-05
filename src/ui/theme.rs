use ratatui::style::Color;

#[derive(Clone, Copy)]
pub struct Theme {
    pub bg: Color,
    pub surface: Color,
    pub border_idle: Color,
    pub border_focus: Color,
    pub text: Color,
    pub text_muted: Color,
    pub accent: Color,
    pub usd: Color,
    pub danger: Color,
    pub warn: Color,
    pub selection_bg: Color,
    pub master: Color,
}

impl Theme {
    pub fn dark() -> Self {
        Theme {
            bg: Color::Rgb(10, 14, 26),
            surface: Color::Rgb(18, 24, 38),
            border_idle: Color::Rgb(41, 52, 74),
            border_focus: Color::Rgb(76, 141, 255),
            text: Color::Rgb(230, 237, 243),
            text_muted: Color::Rgb(133, 147, 171),
            accent: Color::Rgb(94, 174, 255),
            usd: Color::Rgb(74, 222, 128),
            danger: Color::Rgb(248, 113, 113),
            warn: Color::Rgb(251, 191, 36),
            selection_bg: Color::Rgb(27, 40, 64),
            master: Color::Rgb(251, 191, 36),
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Theme::dark()
    }
}
