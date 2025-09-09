// Jackson Coxson
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // hide console window on Windows in release

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use egui::{ColorImage, ComboBox, RichText, TextureHandle};
use idevice::IdeviceService;
use idevice::core_device_proxy::CoreDeviceProxy;
use idevice::dvt::remote_server::RemoteServerClient;
use idevice::rsd::RsdHandshake;
use idevice::tcp::stream::AdapterStream;
use log::{error, info};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;

use idevice::{
    IdeviceError,
    dvt::screenshot::ScreenshotClient,
    lockdown::LockdownClient,
    usbmuxd::{UsbmuxdAddr, UsbmuxdConnection, UsbmuxdDevice},
};

fn main() {
    println!("Startup");
    egui_logger::builder().init().unwrap();
    let (gui_sender, gui_recv) = unbounded_channel();
    let (idevice_sender, mut idevice_receiver) = unbounded_channel();
    idevice_sender.send(IdeviceCommands::GetDevices).unwrap();

    let app = MyApp {
        devices: None,
        devices_placeholder: "Loading...".to_string(),
        selected_device: "".to_string(),
        device_info: None,
        gui_recv,
        idevice_sender: idevice_sender.clone(),
        show_logs: false,
        is_streaming: false,
        screenshot_texture: None,
    // Stats
    frame_times: VecDeque::with_capacity(120),
    fps: 0.0,
    fps_window: Duration::from_secs(2),
    };

    let d = eframe::icon_data::from_png_bytes(include_bytes!("../icon.png"))
        .expect("The icon data must be valid");
    let mut options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1000.0, 800.0]),
        ..Default::default()
    };
    options.viewport.icon = Some(std::sync::Arc::new(d));

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.spawn(async move {
        let mut screenshot_task: Option<JoinHandle<()>> = None;
        while let Some(command) = idevice_receiver.recv().await {
            match command {
                IdeviceCommands::GetDevices => {
                    // This logic is mostly unchanged
                    let mut uc = match UsbmuxdConnection::default().await {
                        Ok(u) => u,
                        Err(e) => {
                            gui_sender.send(GuiCommands::NoUsbmuxd(e)).unwrap();
                            continue;
                        }
                    };
                    match uc.get_devices().await {
                        Ok(devs) => {
                            let mut selections = HashMap::new();
                            for dev in devs {
                                let p = dev.to_provider(UsbmuxdAddr::default(), "idevice_pair");
                                let mut lc = match LockdownClient::connect(&p).await {
                                    Ok(l) => l,
                                    Err(e) => {
                                        error!("Failed to connect to lockdown: {e:?}");
                                        continue;
                                    }
                                };
                                let values = match lc.get_value(None, None).await {
                                    Ok(v) => v,
                                    Err(e) => {
                                        error!("Failed to get lockdown values: {e:?}");
                                        continue;
                                    }
                                };
                                let device_name = match values
                                    .as_dictionary()
                                    .and_then(|x| x.get("DeviceName"))
                                {
                                    Some(plist::Value::String(n)) => n.clone(),
                                    _ => continue,
                                };
                                selections.insert(device_name, dev);
                            }
                            gui_sender.send(GuiCommands::Devices(selections)).unwrap();
                        }
                        Err(e) => {
                            gui_sender.send(GuiCommands::GetDevicesFailure(e)).unwrap();
                        }
                    }
                }
                IdeviceCommands::GetDeviceInfo(dev) => {
                    // This logic is unchanged
                    let p = dev.to_provider(UsbmuxdAddr::default(), "idevice_pair");
                    let mut lc = match LockdownClient::connect(&p).await {
                        Ok(l) => l,
                        Err(e) => {
                            error!("Failed to connect to lockdown: {e:?}");
                            continue;
                        }
                    };
                    let values = match lc.get_value(None, None).await {
                        Ok(v) => v,
                        Err(e) => {
                            error!("Failed to get lockdown values: {e:?}");
                            continue;
                        }
                    };
                    let mut device_info = Vec::with_capacity(5);
                    let fields = [
                        ("Device Name", "DeviceName"),
                        ("Model", "ProductType"),
                        ("iOS Version", "ProductVersion"),
                        ("Build Number", "BuildVersion"),
                        ("UDID", "UniqueDeviceID"),
                    ];
                    for (display_name, key) in fields.iter() {
                        if let Some(plist::Value::String(value)) =
                            values.as_dictionary().and_then(|x| x.get(key))
                        {
                            device_info.push((display_name.to_string(), value.clone()));
                        }
                    }
                    gui_sender
                        .send(GuiCommands::DeviceInfo(device_info))
                        .unwrap();
                }
                IdeviceCommands::StartScreenshotStream((dev, ctx)) => {
                    info!("Starting screenshot stream");
                    let gui_sender_clone = gui_sender.clone();
                    // Spawn a new task for the screenshot loop
                    screenshot_task = Some(tokio::spawn(async move {
                        let p = dev.to_provider(UsbmuxdAddr::default(), "screenshot_service");
                        let cdp = match CoreDeviceProxy::connect(&p).await {
                            Ok(sc) => sc,
                            Err(e) => {
                                error!("Failed to connect to core device proxy: {e:?}");
                                return;
                            }
                        };

                        let rsd_port = cdp.handshake.server_rsd_port;

                        let mut adapter = cdp.create_software_tunnel().expect("no software tunnel");
                        let stream = AdapterStream::connect(&mut adapter, rsd_port)
                            .await
                            .expect("no rsd connect");

                        // Make the connection to RemoteXPC
                        let handshake = RsdHandshake::new(stream).await.unwrap();
                        let instruments_service =
                            match handshake.services.get("com.apple.instruments.dtservicehub") {
                                Some(i) => i,
                                None => {
                                    error!("Instruments service not found, DDI not mounted?");
                                    return;
                                }
                            };
                        let ts_client_stream =
                            match AdapterStream::connect(&mut adapter, instruments_service.port)
                                .await
                            {
                                Ok(sc) => sc,
                                Err(e) => {
                                    error!("Failed to connect to remote server: {e:?}");
                                    return;
                                }
                            };
                        let mut ts_client = RemoteServerClient::new(ts_client_stream);

                        ts_client.read_message(0).await.expect("no read??");

                        let mut sc = match ScreenshotClient::new(&mut ts_client).await {
                            Ok(sc) => sc,
                            Err(e) => {
                                error!("Failed to connect to ScreenshotClient: {e:?}");
                                return;
                            }
                        };
                        loop {
                            println!("Taking screenshot...");
                            match sc.take_screenshot().await {
                                Ok(png_data) => {
                                    println!("Screenshot taken!");
                                    // Decode the PNG and convert to egui::ColorImage
                                    match image::load_from_memory_with_format(
                                        &png_data,
                                        image::ImageFormat::Png,
                                    ) {
                                        Ok(dynamic_image) => {
                                            let image_buffer = dynamic_image.to_rgba8();
                                            let size = [
                                                image_buffer.width() as _,
                                                image_buffer.height() as _,
                                            ];
                                            let pixels = image_buffer.into_flat_samples();
                                            let color_image = ColorImage::from_rgba_unmultiplied(
                                                size,
                                                pixels.as_slice(),
                                            );
                                            if gui_sender_clone
                                                .send(GuiCommands::NewScreenshot(color_image))
                                                .is_err()
                                            {
                                                // GUI has closed, stop the loop
                                                break;
                                            }
                                            ctx.request_repaint();
                                            println!("Image sent");
                                        }
                                        Err(e) => error!("Failed to decode image: {e:?}"),
                                    }
                                }
                                Err(e) => {
                                    error!("Failed to take screenshot: {e:?}");
                                    // Break the loop if the device disconnects or another error occurs
                                    break;
                                }
                            }
                        }
                    }));
                }
                IdeviceCommands::StopScreenshotStream => {
                    info!("Stopping screenshot stream");
                    if let Some(task) = screenshot_task.take() {
                        task.abort();
                    }
                }
            };
        }
        eprintln!("Exited idevice loop!!");
    });

    eframe::run_native("idevice mirror", options, Box::new(|_| Ok(Box::new(app)))).unwrap();
}

enum GuiCommands {
    NoUsbmuxd(IdeviceError),
    GetDevicesFailure(IdeviceError),
    Devices(HashMap<String, UsbmuxdDevice>),
    DeviceInfo(Vec<(String, String)>),
    NewScreenshot(ColorImage),
}

enum IdeviceCommands {
    GetDevices,
    GetDeviceInfo(UsbmuxdDevice),
    StartScreenshotStream((UsbmuxdDevice, egui::Context)),
    StopScreenshotStream,
}

struct MyApp {
    // Selector
    devices: Option<HashMap<String, UsbmuxdDevice>>,
    devices_placeholder: String,
    selected_device: String,
    device_info: Option<Vec<(String, String)>>,
    // Screenshot state
    is_streaming: bool,
    screenshot_texture: Option<TextureHandle>,
    // Stats
    frame_times: VecDeque<Instant>,
    fps: f32,
    fps_window: Duration,
    // Channel
    gui_recv: UnboundedReceiver<GuiCommands>,
    idevice_sender: UnboundedSender<IdeviceCommands>,
    show_logs: bool,
}

impl eframe::App for MyApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        match self.gui_recv.try_recv() {
            Ok(msg) => match msg {
                GuiCommands::NoUsbmuxd(idevice_error) => {
                    self.devices_placeholder =
                        format!("Failed to connect to usbmuxd!\n\n{idevice_error:#?}");
                }
                GuiCommands::Devices(vec) => self.devices = Some(vec),
                GuiCommands::DeviceInfo(info) => self.device_info = Some(info),
                GuiCommands::GetDevicesFailure(idevice_error) => {
                    self.devices_placeholder =
                        format!("Failed to get devices!\n\n{idevice_error:?}");
                }
                GuiCommands::NewScreenshot(color_image) => {
                    // This is the hot path, executed for every frame
                    let texture = self.screenshot_texture.get_or_insert_with(|| {
                        // Allocate a new texture if we don't have one
                        ctx.load_texture("screenshot", color_image.clone(), Default::default())
                    });
                    // Update the existing texture with new data, which is faster
                    texture.set(color_image, Default::default());
                    // Request a repaint to show the new frame
                    ctx.request_repaint();

                    // Update FPS stats based on arrival of frames
                    let now = Instant::now();
                    self.frame_times.push_back(now);
                    // Retain only the timestamps within the window
                    let cutoff = now - self.fps_window;
                    while let Some(&front) = self.frame_times.front() {
                        if front < cutoff {
                            self.frame_times.pop_front();
                        } else {
                            break;
                        }
                    }
                    // Compute FPS as count over window seconds
                    let window_secs = self.fps_window.as_secs_f32().max(0.000_001);
                    self.fps = (self.frame_times.len() as f32) / window_secs;
                }
            },
            Err(e) => match e {
                tokio::sync::mpsc::error::TryRecvError::Empty => {}
                tokio::sync::mpsc::error::TryRecvError::Disconnected => {
                    panic!("idevice crashed");
                }
            },
        }

        if self.show_logs {
            egui::Window::new("logs")
                .open(&mut self.show_logs)
                .show(ctx, |ui| {
                    egui_logger::logger_ui().show(ui);
                });
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("idevice mirror");
                ui.separator();
                ui.toggle_value(&mut self.show_logs, "logs");
                if self.is_streaming {
                    ui.separator();
                    ui.label(format!("FPS: {:.1}", self.fps));
                }
            });

            ui.separator();

            match &self.devices {
                Some(devs) => {
                    ui.horizontal(|ui| {
                        // Device selection UI (unchanged)
                        ui.vertical(|ui| {
                            ui.label("Choose a device");
                            ComboBox::from_label("")
                                .selected_text(&self.selected_device)
                                .show_ui(ui, |ui| {
                                    for (dev_name, dev) in devs {
                                        if ui
                                            .selectable_value(
                                                &mut self.selected_device,
                                                dev_name.clone(),
                                                dev_name.clone(),
                                            )
                                            .clicked()
                                        {
                                            self.device_info = None;
                                            self.idevice_sender
                                                .send(IdeviceCommands::GetDeviceInfo(dev.clone()))
                                                .unwrap();
                                        };
                                    }
                                });
                        });
                        ui.separator();
                        if let Some(info) = &self.device_info {
                            ui.vertical(|ui| {
                                for (key, value) in info {
                                    ui.horizontal(|ui| {
                                        ui.label(RichText::new(format!("{key}: ")).strong());
                                        ui.label(value);
                                    });
                                }
                            });
                        }
                    });

                    if ui.button("Refresh Devices").clicked() {
                        self.idevice_sender
                            .send(IdeviceCommands::GetDevices)
                            .unwrap();
                    }
                }
                None => {
                    ui.label(&self.devices_placeholder);
                }
            }

            ui.separator();

            if let Some(dev) = self
                .devices
                .as_ref()
                .and_then(|x| x.get(&self.selected_device))
            {
                ui.horizontal(|ui| {
                    if !self.is_streaming {
                        if ui.button("Start Streaming").clicked() {
                            self.idevice_sender
                                .send(IdeviceCommands::StartScreenshotStream((
                                    dev.clone(),
                                    ctx.clone(),
                                )))
                                .unwrap();
                            self.is_streaming = true;
                            // Reset stats for a fresh session
                            self.frame_times.clear();
                            self.fps = 0.0;
                        }
                    } else if ui.button("Stop Streaming").clicked() {
                        self.is_streaming = false;
                        self.screenshot_texture = None; // Clear the texture
                        self.idevice_sender
                            .send(IdeviceCommands::StopScreenshotStream)
                            .unwrap();
                        // Clear stats when stopping
                        self.frame_times.clear();
                        self.fps = 0.0;
                    }
                });

                ui.separator();

                // Display the screenshot
                if let Some(texture) = &self.screenshot_texture {
                    // The size calculation logic is still correct.
                    let available_size = ui.available_size();
                    let image_width = texture.size()[0] as f32;
                    let image_height = texture.size()[1] as f32;
                    let image_aspect_ratio = image_width / image_height;

                    let mut desired_width = available_size.x;
                    let mut desired_height = desired_width / image_aspect_ratio;

                    if desired_height > available_size.y {
                        desired_height = available_size.y;
                        desired_width = desired_height * image_aspect_ratio;
                    }

                    let image_size = egui::vec2(desired_width, desired_height);

                    // Create an Image widget with a maximum size.
                    // This scales the image down to fit, preserving aspect ratio.
                    let image_widget = egui::Image::new(texture).max_size(image_size);

                    // Center the widget and add it to the UI.
                    ui.centered_and_justified(|ui| {
                        ui.add(image_widget);
                    });
                }
            }
        });
    }
}
