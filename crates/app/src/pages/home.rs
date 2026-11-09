//! Home: the greeting, then Spotify's sections (Jump back in, Made For you, …), each a
//! row of cards.

use eframe::egui::Ui;
use engine_ipc::{Page, PageReply};

use super::{PageCx, cards, other_page, scrolled, title};

pub fn show(ui: &mut Ui, reply: &PageReply, cx: &mut PageCx) {
    let Page::Home(home) = &reply.page else {
        return other_page(ui);
    };
    scrolled(ui, cx, |ui, cx| {
        title(ui, &home.greeting);
        ui.add_space(16.0);
        for (i, section) in home.sections.iter().enumerate() {
            cards::section(ui, &section.title, &section.cards, ("home", i), cx);
        }
    });
}
