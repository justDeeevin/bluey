use bluer::Address;
use indexmap::IndexMap;
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Row, Table, TableState},
};
use tui_popup::Popup;

use crate::{Device, Error, List};

const THROBBERS: [&str; 4] = ["│", "╱", "─", "╲"];

#[derive(Default)]
pub struct App {
    pub selected_list: List,
    pub selected_row: usize,
    pub unpaired: IndexMap<Address, Device>,
    pub paired: IndexMap<Address, Device>,
    pub error: Option<Error>,
}

impl App {
    pub fn render(&self, frame: &mut Frame) {
        let [top, bottom] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(frame.area());
        let [left, right] = Layout::horizontal([Constraint::Ratio(1, 2); 2]).areas(top);

        let (unpaired, mut unpaired_state) =
            self.table(self.unpaired.values().collect(), List::Unpaired);
        frame.render_stateful_widget(unpaired, left, &mut unpaired_state);
        let (paired, mut paired_state) = self.table(self.paired.values().collect(), List::Paired);
        frame.render_stateful_widget(paired, right, &mut paired_state);

        let legend_items: &[(&str, &str)] = if self.error.is_some() {
            &[("esc", "close")]
        } else {
            &[
                ("◀▼▲▶", "navigate"),
                ("q", "quit"),
                ("s", "scan"),
                (
                    "↵",
                    match self.selected_list {
                        List::Unpaired => "pair",
                        List::Paired => "connect",
                    },
                ),
            ]
        };

        let legend = Text::raw(
            legend_items
                .iter()
                .map(|(key, action)| format!("{key}: {action}"))
                .collect::<Vec<_>>()
                .join(" • "),
        )
        .alignment(Alignment::Center)
        .style(Style::default().bold());
        frame.render_widget(legend, bottom);

        let Some(Error { process, message }) = self.error.clone() else {
            return;
        };
        let text = Text::from(vec![
            Line::from(format!("Error while {process}")).style(Style::default().bold()),
            Line::from(message),
        ]);
        let popup = Popup::new(text)
            .title("Error")
            .border_style(Style::default().red());
        frame.render_widget(&popup, frame.area());
    }

    fn table(&self, items: Vec<&Device>, list: List) -> (Table, TableState) {
        let active = list == self.selected_list;
        let mut block = Block::default()
            .title(list.to_string())
            .borders(Borders::ALL);
        if active {
            block = block
                .title_style(Style::default().bold())
                .border_set(symbols::border::THICK);
        }
        let rows = items.iter().map(|device| {
            let mut cells = vec![device.alias.clone()];
            if let List::Paired = list {
                cells.push(device.connected.to_string());
            }
            if let Some(index) = device.loading {
                cells.push(THROBBERS[index % 4].into());
            }
            Row::new(cells)
        });
        let widths = [Constraint::Ratio(1, 3); 3];
        let header = match list {
            List::Unpaired => Row::new(["Alias"]),
            List::Paired => Row::new(["Alias", "Connected"]),
        };
        let table = Table::new(rows, widths)
            .header(header.style(Style::default().bold()).bottom_margin(1))
            .row_highlight_style(Style::default().reversed())
            .block(block);

        let selected = if active && !items.is_empty() {
            Some(self.selected_row)
        } else {
            None
        };

        (table, TableState::default().with_selected(selected))
    }
}
