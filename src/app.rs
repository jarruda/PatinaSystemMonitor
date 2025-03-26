use egui::{Color32, Context};
use egui_extras::{Column, TableBuilder};
use egui_plot::{Line, Plot, PlotBounds, PlotPoints};
use influxdb_line_protocol::{parse_lines, FieldValue, ParsedLine};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc;
use std::sync::mpsc::{Receiver, Sender};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const WINDOW_LENGTH: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq)]
enum MeasurementValue {
    I64(i64),
    U64(u64),
    F64(f64),
    String(String),
    Boolean(bool),
}

impl From<&MeasurementValue> for f64 {
    fn from(value: &MeasurementValue) -> Self {
        match value {
            MeasurementValue::I64(i) => *i as f64,
            MeasurementValue::U64(u) => *u as f64,
            MeasurementValue::F64(f) => *f,
            MeasurementValue::String(_) => 0.0, // Convert strings to 0.0
            MeasurementValue::Boolean(b) => {
                if *b {
                    1.0
                } else {
                    0.0
                }
            } // Convert booleans to 0.0 or 1.0
        }
    }
}

impl<'a> From<&FieldValue<'a>> for MeasurementValue {
    fn from(value: &FieldValue<'a>) -> Self {
        match value {
            FieldValue::I64(i) => MeasurementValue::I64(*i),
            FieldValue::U64(u) => MeasurementValue::U64(*u),
            FieldValue::F64(f) => MeasurementValue::F64(*f),
            FieldValue::String(s) => MeasurementValue::String(s.to_string()),
            FieldValue::Boolean(b) => MeasurementValue::Boolean(*b),
        }
    }
}

/// Stores an owned version of `ParsedLine`
#[derive(Debug, Clone)]
struct OwnedParsedLine {
    measurement: String,
    tags: Vec<(String, String)>,
    fields: Vec<(String, MeasurementValue)>,
    unix_timestamp: Duration,
}
impl<'a> From<&ParsedLine<'a>> for OwnedParsedLine {
    fn from(parsed: &ParsedLine<'a>) -> Self {
        let unix_timestamp = parsed
            .timestamp
            .and_then(|ns| ns.try_into().ok())
            .map(Duration::from_nanos)
            .unwrap_or(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default(),
            );

        OwnedParsedLine {
            measurement: parsed.series.measurement.to_string(),
            tags: parsed
                .series
                .tag_set
                .iter()
                .flat_map(|ts| ts.iter().map(|t| (t.0.to_string(), t.1.to_string())))
                .collect(),
            fields: parsed
                .field_set
                .iter()
                .map(|(k, v)| (k.to_string(), v.into()))
                .collect(),
            unix_timestamp,
        }
    }
}

impl OwnedParsedLine {
    pub fn offset_timestamp_secs_f64(&self, timestamp_relative: Duration) -> f64 {
        self.unix_timestamp.as_secs_f64() - timestamp_relative.as_secs_f64()
    }

    pub fn get_field_as_f64(&self, name: &str, default: f64) -> f64 {
        self.fields
            .iter()
            .find(|(k, _)| k == name)
            .map_or_else(|| default, |(_, v)| v.into())
    }
}

#[derive(Debug)]
struct TimeSeriesDatum<T> {
    unix_timestamp: Duration,
    data: T,
}

//
#[derive(Debug)]
struct TimeSeries<T> {
    data: VecDeque<TimeSeriesDatum<T>>,
}

impl<T> TimeSeries<T> {
    pub fn new() -> TimeSeries<T> {
        TimeSeries {
            data: VecDeque::new(),
        }
    }

    fn push(&mut self, unix_timestamp: Duration, data: T) {
        self.data.push_back(TimeSeriesDatum {
            unix_timestamp,
            data,
        });
    }

    fn trim_older_than(&mut self, unix_timestamp: Duration) {
        while self
            .data
            .front()
            .is_some_and(|d| d.unix_timestamp < unix_timestamp)
        {
            self.data.pop_front();
        }
    }
}

/// We derive Deserialize/Serialize so we can persist app state on shutdown.
#[derive(serde::Deserialize, serde::Serialize)]
#[serde(default)] // if we add new fields, give them default values when deserializing old state
pub struct PatinaSystemMonitor {
    #[serde(skip)]
    metrics_rx: Receiver<(SocketAddr, Vec<OwnedParsedLine>)>,

    #[serde(skip)]
    listen_thread_tx: Sender<()>,

    #[serde(skip)]
    time_series: TimeSeries<OwnedParsedLine>,
}

impl Default for PatinaSystemMonitor {
    fn default() -> Self {
        Self {
            metrics_rx: mpsc::channel().1,
            listen_thread_tx: mpsc::channel().0,
            time_series: TimeSeries::new(),
        }
    }
}

impl PatinaSystemMonitor {
    /// Called once before the first frame.
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // This is also where you can customize the look and feel of egui using
        // `cc.egui_ctx.set_visuals` and `cc.egui_ctx.set_fonts`.

        // Load previous app state (if any).
        // Note that you must enable the `persistence` feature for this to work.
        // if let Some(storage) = cc.storage {
        //     return eframe::get_value(storage, eframe::APP_KEY).unwrap_or_default();
        // }

        let listener = TcpListener::bind("127.0.0.1:8094")
            .expect("Failed to bind TCP listener, unhandled error.");

        println!("Listening on port 8094...");

        let (metrics_tx, metrics_rx) = mpsc::channel::<(SocketAddr, Vec<OwnedParsedLine>)>();
        let (listen_thread_tx, listen_thread_rx) = mpsc::channel::<()>();

        let ctx = cc.egui_ctx.clone();
        thread::spawn(move || {
            while listen_thread_rx.try_recv() != Err(mpsc::TryRecvError::Disconnected) {
                for stream in listener.incoming() {
                    match stream {
                        Ok(stream) => {
                            println!("New connection: {}", stream.peer_addr().unwrap());
                            let tx_clone = metrics_tx.clone();
                            let ctx_clone = ctx.clone();
                            thread::spawn(|| handle_client(stream, tx_clone, ctx_clone));
                        }
                        Err(e) => eprintln!("Connection failed: {}", e),
                    }
                }
            }
            println!("Closing listening port.");
        });

        PatinaSystemMonitor {
            metrics_rx,
            listen_thread_tx,
            ..Default::default()
        }
    }

    fn trim_time_series(&mut self) {
        let now_unix = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        let window_start_unix_timestamp = now_unix - WINDOW_LENGTH.mul_f64(1.1);

        self.time_series
            .trim_older_than(window_start_unix_timestamp);
    }

    fn recv_metrics(&mut self) {
        for x in self.metrics_rx.try_iter() {
            for line in x.1 {
                self.time_series.push(line.unix_timestamp, line);
            }
        }
    }

    fn show_debug_metrics(&self, ctx: &egui::Context) {
        let window = egui::Window::new("Metrics");
        window.show(ctx, |ui| {
            ui.label(format!("Total metrics: {}", self.time_series.data.len()));
            ui.label("All CPU Total Metrics");
            TableBuilder::new(ui)
                .striped(true)
                .resizable(true)
                .column(Column::remainder())
                .body(|mut body| {
                    for data in self
                        .time_series
                        .data
                        .iter()
                        .filter(|d| d.data.measurement == "cpu")
                        .filter(|d| {
                            d.data
                                .tags
                                .iter()
                                .any(|(k, v)| k == "cpu" && v == "cpu-total")
                        })
                    {
                        body.row(18.0, |mut row| {
                            row.col(|ui| {
                                ui.label(format!("{:?}", data));
                            });
                        })
                    }
                });
        });
    }

    fn simple_plot(&self, ui: &mut egui::Ui) {
        let now_unix = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        let plot = Plot::new("cpu_graph").height(300.0);

        plot.show(ui, |plot_ui| {
            plot_ui.set_plot_bounds(PlotBounds::from_min_max(
                [-WINDOW_LENGTH.as_secs_f64(), 0.0],
                [-1.1, 100.0],
            ));

            let plot_points = self
                .time_series
                .data
                .iter()
                .filter(|d| d.data.measurement == "cpu")
                .filter(|d| {
                    d.data
                        .tags
                        .iter()
                        .any(|(k, v)| k == "cpu" && v == "cpu-total")
                })
                .map(|d| {
                    [
                        d.data.offset_timestamp_secs_f64(now_unix),
                        100.0 - d.data.get_field_as_f64("usage_idle", 0.0),
                    ]
                })
                .collect();

            plot_ui.line(Line::new(PlotPoints::new(plot_points)).color(Color32::RED));
        });
    }
}

impl eframe::App for PatinaSystemMonitor {
    /// Called each time the UI needs repainting, which may be many times per second.
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.trim_time_series();
        self.recv_metrics();

        self.show_debug_metrics(ctx);

        // Put your widgets into a `SidePanel`, `TopBottomPanel`, `CentralPanel`, `Window` or `Area`.
        // For inspiration and more examples, go to https://emilk.github.io/egui
        egui::TopBottomPanel::top("top_panel").show(ctx, |ui| {
            // The top panel is often a good place for a menu bar:

            egui::menu::bar(ui, |ui| {
                // NOTE: no File->Quit on web pages!
                let is_web = cfg!(target_arch = "wasm32");
                if !is_web {
                    ui.menu_button("File", |ui| {
                        if ui.button("Quit").clicked() {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                    ui.add_space(16.0);
                }

                egui::widgets::global_theme_preference_buttons(ui);
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            // The central panel the region left after adding TopPanel's and SidePanel's
            ui.heading("CPU");

            self.simple_plot(ui);

            ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                powered_by_egui_and_eframe(ui);
                egui::warn_if_debug_build(ui);
            });
        });

        ctx.request_repaint();
    }

    /// Called by the frame work to save state before shutdown.
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, eframe::APP_KEY, self);
    }
}

fn powered_by_egui_and_eframe(ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.label("Powered by ");
        ui.hyperlink_to("egui", "https://github.com/emilk/egui");
        ui.label(" and ");
        ui.hyperlink_to(
            "eframe",
            "https://github.com/emilk/egui/tree/master/crates/eframe",
        );
        ui.label(".");
    });
}

/// Handles an individual client connection.
fn handle_client(
    stream: TcpStream,
    sender: Sender<(SocketAddr, Vec<OwnedParsedLine>)>,
    ctx: Context,
) {
    let socket_addr = stream.peer_addr().unwrap();
    let reader = BufReader::new(stream);

    for line in reader.lines() {
        match line {
            Ok(ref data) => {
                let lines: Vec<OwnedParsedLine> = parse_lines(data)
                    .filter(|x| x.is_ok())
                    .map(|x| x.as_ref().unwrap().into())
                    .collect();

                if sender.send((socket_addr, lines)).is_err() {
                    eprintln!("Failed to send parsed data to main thread. Exiting stream.");
                    return;
                }
                ctx.request_repaint();
            }
            Err(e) => {
                eprintln!("Error reading from stream: {}", e);
                break;
            }
        }
    }
}
