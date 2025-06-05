use bluer::{Adapter, AdapterEvent, Address, DeviceEvent, DeviceProperty};
use color_eyre::{
    Result,
    eyre::{Context, eyre},
};
use crossterm::event::{Event, EventStream, KeyCode};
use futures::stream::{SelectAll, StreamExt};
use indexmap::IndexMap;
use ratatui::{
    DefaultTerminal,
    prelude::*,
    widgets::{Block, Borders, Row, Table, TableState},
};
use tokio::{
    fs::File,
    select,
    sync::mpsc::{self, UnboundedSender},
    task::JoinHandle,
};
use tracing::{debug, error, instrument, trace};
use tracing_subscriber::EnvFilter;
use tui_popup::Popup;

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    tracing_subscriber::fmt()
        .with_writer(
            File::create("log")
                .await
                .wrap_err("Failed to create log file")?
                .into_std()
                .await,
        )
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let terminal = ratatui::init();
    let result = run(terminal).await;
    ratatui::restore();

    result
}

// const THROBBERS: [&str; 4] = ["│", "╱", "─", "╲"];

#[instrument]
async fn get_adapter() -> Result<bluer::Adapter> {
    trace!("Getting session");
    let session = bluer::Session::new()
        .await
        .wrap_err("Failed to create session")?;
    trace!("Getting default adapter");
    let adapter = session
        .default_adapter()
        .await
        .wrap_err("Failed to get default adapter")?;
    trace!("Turning on adapter");
    adapter
        .set_powered(true)
        .await
        .wrap_err("Failed to turn on adapter")?;
    Ok(adapter)
}

#[instrument(skip_all)]
async fn run(mut terminal: DefaultTerminal) -> Result<()> {
    let mut app = App::default();

    let mut adapter: Option<Adapter> = None;
    let (tx_adapter, mut rx_adapter) = mpsc::channel(1);
    tokio::spawn(async move {
        let result = get_adapter().await;
        if let Err(error) = tx_adapter.send(result).await {
            error!(%error, "Failed to send adapter");
        }
    });

    let mut events = EventStream::new();

    let mut adapter_events_handle: Option<JoinHandle<()>> = None;

    let (tx_additions, mut rx_additions) = mpsc::unbounded_channel();
    let mut changes = SelectAll::new();
    let (tx_errors, mut rx_errors) = mpsc::unbounded_channel();

    loop {
        terminal
            .draw(|frame| {
                trace!("Drawing");
                app.render(frame);
            })
            .wrap_err("Failed to draw")?;

        select! {
            Some(error) = rx_errors.recv() => {
                app.error = Some(error);
            }
            Some(addr) = rx_additions.recv() => {
                let Some(adapter) = &adapter else {
                    continue;
                };
                let device = match adapter.device(addr) {
                    Ok(d) => d,
                    Err(error) => {
                        error!(%addr, %error, "Failed to get device from addr");
                        continue;
                    }
                };
                let paired = match device.is_paired().await {
                    Ok(paired) => paired,
                    Err(error) => {
                        error!(%addr, %error, "Failed to get device paired state");
                        continue;
                    }
                };
                let connected = match device.is_connected().await {
                    Ok(connected) => connected,
                    Err(error) => {
                        error!(%addr, %error, "Failed to get device connected state");
                        continue;
                    }
                };
                let events = match device.events().await {
                    Ok(events) => events,
                    Err(error) => {
                        error!(%addr, %error, "Failed to get device events");
                        continue;
                    }
                };
                changes.push(events.map(move |e| (addr, e)));
                let alias = match device.alias().await {
                    Ok(a) => a,
                    Err(error) => {
                        error!(%addr, %error, "Failed to get device alias");
                        continue;
                    }
                };
                let device = Device {
                    alias,
                    connected,
                };
                if paired {
                    app.paired.insert(addr, device);
                } else {
                    app.unpaired.insert(addr, device);
                }
            }
            Some((addr, DeviceEvent::PropertyChanged(property))) = changes.next() => {
                match property {
                    DeviceProperty::Alias(alias) => {
                        app.paired.entry(addr).and_modify(|d| d.alias = alias);
                    }
                    DeviceProperty::Connected(connected) => {
                        app.paired.entry(addr).and_modify(|d| d.connected = connected);
                    }
                    DeviceProperty::Paired(paired) => {
                        if paired {
                            let Some(device) = app.unpaired.shift_remove(&addr) else {
                                continue;
                            };
                            app.paired.insert(addr, device);
                        } else {
                            let Some(device) = app.paired.shift_remove(&addr) else {
                                continue;
                            };
                            app.unpaired.insert(addr, device);
                        }
                    }
                    _ => {}
                }
            }
            Some(fetched_adapter) = rx_adapter.recv() => {
                let fetched_adapter = fetched_adapter?;
                adapter_events_handle = Some(scan(fetched_adapter.clone(), tx_additions.clone()));
                adapter = Some(fetched_adapter);
            }
            Some(event) = events.next() => {
                let Event::Key(event) = event.wrap_err("Failed to read terminal event")? else {
                    continue;
                };

                debug!(?event, "Terminal keypress");

                if app.error.is_some() {
                    if event.code == KeyCode::Esc {
                        app.error.take();
                    }
                    continue;
                }

                match event.code {
                    KeyCode::Char('q') => {
                        break Ok(());
                    }
                    KeyCode::Down => {
                        let len = match app.selected_list {
                            List::Unpaired => app.unpaired.len(),
                            List::Paired => app.paired.len(),
                        };
                        if app.selected_row < len.checked_sub(1).unwrap_or_default() {
                            app.selected_row += 1;
                        }
                    }
                    KeyCode::Up => {
                        if app.selected_row > 0 {
                            app.selected_row -= 1;
                        }
                    }
                    KeyCode::Left => {
                        if app.selected_list == List::Paired {
                            app.selected_list = List::Unpaired;
                        }
                    }
                    KeyCode::Right => {
                        if app.selected_list == List::Unpaired {
                            app.selected_list = List::Paired;
                        }
                    }
                    KeyCode::Char('s') => {
                        let Some(adapter) = adapter.clone() else {
                            continue;
                        };
                        if let Some(device_events_handle) = &adapter_events_handle {
                            device_events_handle.abort()
                        }
                        app.unpaired.clear();
                        adapter_events_handle = Some(scan(adapter, tx_additions.clone()));
                    }
                    KeyCode::Enter => {
                        let slice = match app.selected_list {
                            List::Unpaired => app.unpaired.as_mut_slice(),
                            List::Paired => app.paired.as_mut_slice(),
                        };
                        let Some(adapter) = &adapter else {
                            continue;
                        };
                        let (addr, Device {alias, ..}) = slice.get_index_mut(app.selected_row).ok_or(eyre!("Attempted to use item that doesn't exist in list. This should be impossible!"))?;
                        let alias = alias.clone();
                        let process = match app.selected_list {
                            List::Paired => format!("connecting to {alias}"),
                            List::Unpaired => format!("pairing with {alias}"),
                        };
                        let device = match adapter.device(*addr) {
                            Ok(d) => d,
                            Err(error) => {
                                app.error = Some(Error {
                                    message: error.to_string(),
                                    process,
                                });
                                continue;
                            }
                        };
                        let tx = tx_errors.clone();
                        match app.selected_list {
                            List::Unpaired => {
                                tokio::spawn(async move {
                                    if let Err(error) = device.pair().await {
                                        if let Err(error) = tx.send(Error {
                                            message: error.to_string(),
                                            process: format!("pairing with {alias}"),
                                        }) {
                                            error!(%error, "Failed to send pair error");
                                        }
                                    }
                                });
                            }
                            List::Paired => {
                                tokio::spawn(async move {
                                    if let Err(error) = device.connect().await {
                                        if let Err(error) = tx.send(Error {
                                            message: error.to_string(),
                                            process,
                                        }) {
                                            error!(%error, "Failed to send connect error");
                                        }
                                    }
                                });
                            }
                        }
                    }
                    KeyCode::Esc => {
                        app.error.take();
                    }
                    _ => {}
                }
            }
        }
    }
}

#[instrument(skip_all)]
fn scan(adapter: Adapter, tx_additions: UnboundedSender<Address>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut events = match adapter.discover_devices().await {
            Ok(events) => events,
            Err(error) => {
                error!(%error, "Failed to discover devices");
                return;
            }
        };
        while let Some(event) = events.next().await {
            let AdapterEvent::DeviceAdded(addr) = event else {
                continue;
            };
            if let Err(error) = tx_additions.send(addr) {
                error!(%addr, %error, "Failed to send device addition event");
            }
        }
    })
}

#[derive(Default, PartialEq, Eq)]
enum List {
    #[default]
    Unpaired,
    Paired,
}

impl std::fmt::Display for List {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            List::Unpaired => write!(f, "Unpaired"),
            List::Paired => write!(f, "Paired"),
        }
    }
}

#[derive(Debug)]
struct Device {
    pub alias: String,
    pub connected: bool,
}

#[derive(Default)]
struct App {
    pub selected_list: List,
    pub selected_row: usize,
    pub unpaired: IndexMap<Address, Device>,
    pub paired: IndexMap<Address, Device>,
    pub error: Option<Error>,
}

#[derive(Clone)]
struct Error {
    pub message: String,
    pub process: String,
}

impl App {
    fn render(&self, frame: &mut Frame) {
        let [top, bottom] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(frame.area());
        let [left, right] = Layout::horizontal([Constraint::Ratio(1, 2); 2]).areas(top);

        let (unpaired, mut unpaired_state) =
            self.table(self.unpaired.values().collect(), List::Unpaired);
        frame.render_stateful_widget(unpaired, left, &mut unpaired_state);
        let (paired, mut paired_state) = self.table(self.paired.values().collect(), List::Paired);
        frame.render_stateful_widget(paired, right, &mut paired_state);

        let legend = Text::raw("q: quit • s: scan")
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
        let block = Block::default()
            .title(list.to_string())
            .borders(Borders::ALL);
        let rows = items.iter().map(|d| {
            let mut cells = vec![d.alias.clone()];
            if let List::Paired = list {
                cells.push(d.connected.to_string());
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

        let selected = if list == self.selected_list && !items.is_empty() {
            Some(self.selected_row)
        } else {
            None
        };

        (table, TableState::default().with_selected(selected))
    }
}
