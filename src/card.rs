use egui::Ui;

pub trait Card {
    fn ui(&mut self, ui: &mut egui::Ui);
}

pub struct GraphCard {
    pub title: String,
}

impl Default for GraphCard {
    fn default() -> GraphCard {
        GraphCard {
            title: "New Graph Card".to_string(),
        }
    }
}

impl Card for GraphCard {
    fn ui(&mut self, ui: &mut Ui) {
        ui.heading(&self.title);
    }
}
