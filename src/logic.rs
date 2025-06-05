use std::{collections::HashMap, time::Duration};

use crate::{Device, Error, List, ui::App};
use bluer::{Adapter, AdapterEvent, Address, DeviceEvent, DeviceProperty};
use color_eyre::{
    Result,
    eyre::{Context, eyre},
};
use crossterm::event::{Event, EventStream, KeyCode};
use futures::stream::{SelectAll, StreamExt};
use ratatui::DefaultTerminal;
use tokio::{
    select,
    sync::mpsc::{self, UnboundedSender},
    task::JoinHandle,
    time::sleep,
};
use tracing::{debug, error, instrument, trace};

const SPINNER_TICK: Duration = Duration::from_millis(100);

#[instrument(skip_all)]
pub async fn run(mut terminal: DefaultTerminal) -> Result<()> {
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
    let (tx_spinner_tick, mut rx_spinner_tick) = mpsc::unbounded_channel();
    let (tx_complete, mut rx_complete) = mpsc::unbounded_channel();
    let mut spinners: HashMap<Address, JoinHandle<_>> = HashMap::new();

    loop {
        terminal
            .draw(|frame| {
                trace!("Drawing");
                app.render(frame);
            })
            .wrap_err("Failed to draw")?;

        select! {
            Some(addr) = rx_spinner_tick.recv() => {
                let Some(Some(index)) = app.paired.get_mut(&addr).or_else(|| app.unpaired.get_mut(&addr)).map(|d| &mut d.loading) else {
                    continue;
                };
                *index = index.wrapping_add(1);
            }
            Some(addr) = rx_complete.recv() => {
                let Some(loading) = app.paired.get_mut(&addr).or_else(|| app.unpaired.get_mut(&addr)).map(|d| &mut d.loading) else {
                    continue;
                };
                *loading = None;
                if let Some(h) = spinners.remove(&addr) {
                    h.abort();
                }
            }
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
                    loading: None,
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
                        app.paired.clear();
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
                        let (
                            addr,
                            Device {alias, loading, ..}
                        ) = slice
                            .get_index_mut(app.selected_row)
                            .ok_or(eyre!("Attempted to use item that doesn't exist in list. This should be impossible!"))?;
                        if loading.is_some() {
                            continue;
                        } else {
                            *loading = Some(0);
                        }
                        let addr = *addr;
                        let alias = alias.clone();
                        let process = match app.selected_list {
                            List::Paired => format!("connecting to {alias}"),
                            List::Unpaired => format!("pairing with {alias}"),
                        };
                        let device = match adapter.device(addr) {
                            Ok(d) => d,
                            Err(error) => {
                                app.error = Some(Error {
                                    message: error.to_string(),
                                    process,
                                });
                                continue;
                            }
                        };
                        let tx_errors = tx_errors.clone();
                        let tx_spinner_tick = tx_spinner_tick.clone();
                        let tx_complete = tx_complete.clone();
                        match app.selected_list {
                            List::Unpaired => {
                                tokio::spawn(async move {
                                    let res = device.pair().await;
                                    if let Err(error) = tx_complete.send(addr) {
                                        error!(%addr, %error, "Failed to send pair complete");
                                    }
                                    if let Err(error) = res {
                                        if let Err(error) = tx_errors.send(Error {
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
                                    let res = device.connect().await;
                                    if let Err(error) = tx_complete.send(addr) {
                                        error!(%addr, %error, "Failed to send connect complete");
                                    }
                                    if let Err(error) = res {
                                        if let Err(error) = tx_errors.send(Error {
                                            message: error.to_string(),
                                            process,
                                        }) {
                                            error!(%error, "Failed to send connect error");
                                        }
                                    }
                                });
                            }
                        }
                        spinners.insert(addr, tokio::spawn(async move {
                            loop {
                                sleep(SPINNER_TICK).await;
                                if let Err(error) = tx_spinner_tick.send(addr) {
                                    error!(%addr, %error, "Failed to send spinner tick");
                                }
                            }
                        }));
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
