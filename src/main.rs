//! WD-005 port: virtual screencast source -> PipeWire -> GStreamer appsink.
//! No network connection, encoder, or TV receiver is started by this diagnostic.

use gio::prelude::*;
use glib::{variant::ToVariant, MainLoop, Variant, VariantDict};
use gstreamer as gst;
use gst::prelude::*;
use std::{cell::RefCell, os::fd::{AsRawFd, IntoRawFd}, rc::Rc, time::{Duration, Instant}};

const BUS: &str = "org.freedesktop.portal.Desktop";
const DESKTOP: &str = "/org/freedesktop/portal/desktop";
const SCREENCAST: &str = "org.freedesktop.portal.ScreenCast";

fn token() -> String {
    format!("ruden_{}", uuid::Uuid::new_v4().simple())
}

fn options(pairs: &[(&str, Variant)]) -> Variant {
    let dict = VariantDict::new(None);
    for (key, value) in pairs {
        dict.insert_value(key, value);
    }
    dict.end()
}

struct App {
    bus: gio::DBusConnection,
    main_loop: MainLoop,
    session: Option<String>,
    request: Option<String>,
    stage: &'static str,
    pipeline: Option<gst::Pipeline>,
    pipewire_fd: Option<i32>,
    frames: u64,
    started: Option<Instant>,
    first_frame: Option<Instant>,
    stopped: bool,
}

impl App {
    fn call(&self, name: &str, arguments: Variant) -> Result<String, glib::Error> {
        let reply = self.bus.call_sync(
            Some(BUS), DESKTOP, SCREENCAST, name, Some(&arguments),
            None, gio::DBusCallFlags::NONE, 10_000, gio::Cancellable::NONE,
        )?;
        Ok(reply.child_value(0).str().unwrap_or_default().to_owned())
    }

    fn create(&mut self) -> Result<(), glib::Error> {
        println!("[1/4] Создание ScreenCast-сессии...");
        self.stage = "create";
        self.request = Some(self.call("CreateSession", (options(&[
            ("handle_token", token().to_variant()),
            ("session_handle_token", token().to_variant()),
        ]),).to_variant())?);
        Ok(())
    }

    fn select(&mut self) -> Result<(), glib::Error> {
        println!("[2/4] Запрос виртуального дисплея...");
        self.stage = "select";
        let session = self.session.as_ref().expect("session missing");
        self.request = Some(self.call("SelectSources", (
            session.as_str(),
            options(&[
                ("handle_token", token().to_variant()),
                ("types", 4u32.to_variant()),
                ("cursor_mode", 2u32.to_variant()),
                ("multiple", false.to_variant()),
            ]),
        ).to_variant())?);
        Ok(())
    }

    fn start(&mut self) -> Result<(), glib::Error> {
        println!("[3/4] Запуск виртуального дисплея...");
        println!("[INFO] Подтверди запрос KDE, если он появится.");
        self.stage = "start";
        let session = self.session.as_ref().expect("session missing");
        self.request = Some(self.call("Start", (
            session.as_str(), "", options(&[("handle_token", token().to_variant())]),
        ).to_variant())?);
        Ok(())
    }

    fn close(&mut self) {
        if self.stopped { return; }
        self.stopped = true;
        if let Some(pipeline) = self.pipeline.take() {
            let _ = pipeline.set_state(gst::State::Null);
        }
        if let Some(fd) = self.pipewire_fd.take() {
            // SAFETY: fd belongs to this App after into_raw_fd(), and is closed once.
            unsafe { libc::close(fd); }
        }
        if let Some(session) = self.session.take() {
            let _ = self.bus.call_sync(
                Some(BUS), &session, "org.freedesktop.portal.Session", "Close",
                None, None, gio::DBusCallFlags::NONE, 3_000, gio::Cancellable::NONE,
            );
            println!("[OK] Portal-сессия закрыта.");
        }
        self.main_loop.quit();
    }

    fn report(&mut self) {
        let seconds = self.started.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
        let fps = self.first_frame.map(|t| self.frames as f64 / t.elapsed().as_secs_f64().max(0.001)).unwrap_or(0.0);
        println!("\n========== WD-005 / RUST ==========");
        println!("Продолжительность: {seconds:.2} с");
        println!("Получено кадров: {}", self.frames);
        println!("Средний FPS: {fps:.2}");
        println!("Статус: {}", if self.frames > 0 { "ВИДЕОКАДРЫ ПОЛУЧЕНЫ" } else { "КАДРЫ НЕ ПОЛУЧЕНЫ" });
        println!("===================================");
        self.close();
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    gst::init()?;
    let bus = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE)?;
    let main_loop = MainLoop::new(None, false);
    let app = Rc::new(RefCell::new(App {
        bus: bus.clone(), main_loop: main_loop.clone(), session: None, request: None,
        stage: "", pipeline: None, pipewire_fd: None,
        frames: 0, started: None, first_frame: None, stopped: false,
    }));

    let handler = app.clone();
    bus.signal_subscribe(
        None, "org.freedesktop.portal.Request", "Response", None, None,
        gio::DBusSignalFlags::NONE,
        move |_, _, path, _, _, params| {
            let Some(path) = path else { return; };
            let mut state = handler.borrow_mut();
            if state.request.as_deref() != Some(path.as_str()) { return; }
            let status: u32 = params.child_value(0).get().unwrap_or(u32::MAX);
            let result = params.child_value(1);
            if status != 0 {
                eprintln!("[ERROR] Portal отклонил запрос: {status}");
                state.close();
                return;
            }
            let action = match state.stage {
                "create" => {
                    let session = result.lookup_value("session_handle", None)
                        .and_then(|v| v.str().map(str::to_owned));
                    if let Some(session) = session {
                        state.session = Some(session);
                        println!("[OK] Сессия создана.");
                        state.select().map(|_| ())
                    } else { Err(glib::Error::new(gio::IOErrorEnum::Failed, "session_handle отсутствует")) }
                }
                "select" => {
                    println!("[OK] Виртуальный источник выбран.");
                    state.start().map(|_| ())
                }
                "start" => {
                    println!("[OK] Виртуальный дисплей создан!");
                    let streams = result.lookup_value("streams", None);
                    if let Some(streams) = streams {
                        if streams.n_children() == 0 {
                            Err(glib::Error::new(gio::IOErrorEnum::Failed, "streams пуст"))
                        } else {
                            let stream_id: u32 = streams.child_value(0).child_value(0).get().unwrap_or(0);
                            println!("[INFO] Stream ID: {stream_id}");
                            capture(handler.clone(), &mut state, stream_id)
                        }
                    } else { Err(glib::Error::new(gio::IOErrorEnum::Failed, "streams отсутствует")) }
                }
                _ => Ok(()),
            };
            if let Err(error) = action {
                eprintln!("[ERROR] {error}");
                state.close();
            }
        },
    );

    println!("RuDeNoID Wireless Display — WD-005 / Rust");
    app.borrow_mut().create()?;
    main_loop.run();
    println!("[INFO] Тест завершён.");
    Ok(())
}

fn capture(app: Rc<RefCell<App>>, state: &mut App, stream_id: u32) -> Result<(), glib::Error> {
    println!("[4/4] Подключение к PipeWire...");
    let session = state.session.as_ref().expect("session missing");
    let (reply, fd_list) = state.bus.call_with_unix_fd_list_sync(
        Some(BUS), DESKTOP, SCREENCAST, "OpenPipeWireRemote",
        Some(&(session.as_str(), options(&[])).to_variant()),
        None, gio::DBusCallFlags::NONE, 10_000,
        None::<&gio::UnixFDList>, gio::Cancellable::NONE,
    )?;
    let index: i32 = reply.child_value(0).get().unwrap_or(-1);
    let fd = fd_list.as_ref().ok_or_else(|| glib::Error::new(gio::IOErrorEnum::Failed, "нет fd_list"))?.get(index)?;
    let raw_fd = fd.into_raw_fd();
    println!("[OK] PipeWire FD: {raw_fd}");
    state.pipewire_fd = Some(raw_fd);

    let element = gst::parse::launch(
        "pipewiresrc name=source do-timestamp=true ! videoconvert ! video/x-raw,format=RGB ! appsink name=sink emit-signals=true sync=false max-buffers=2 drop=true"
    ).map_err(|e| glib::Error::new(gio::IOErrorEnum::Failed, &e.to_string()))?;
    let pipeline = element.downcast::<gst::Pipeline>()
        .map_err(|_| glib::Error::new(gio::IOErrorEnum::Failed, "не Pipeline"))?;
    let source = pipeline.by_name("source").expect("pipewiresrc missing");
    source.set_property("fd", raw_fd);
    source.set_property("path", stream_id.to_string());
    let sink = pipeline.by_name("sink").expect("appsink missing");
    let samples = app.clone();
    sink.connect("new-sample", false, move |values| {
        let sink = values[0].get::<gst::Element>().unwrap();
        let sample = sink.emit_by_name::<Option<gst::Sample>>("pull-sample", &[]);
        if let Some(sample) = sample {
            let mut state = samples.borrow_mut();
            state.frames += 1;
            if state.first_frame.is_none() {
                state.first_frame = Some(Instant::now());
                println!("[OK] Первый видеокадр получен!");
                if let Some(caps) = sample.caps() { println!("[VIDEO] Формат: {caps}"); }
            }
        }
        Some(gst::FlowReturn::Ok.to_value())
    });

    let gst_bus = pipeline.bus().expect("GStreamer bus missing");
    let monitor = app.clone();
    gst_bus.add_watch_local(move |_, message| {
        if let gst::MessageView::Error(error) = message.view() {
            eprintln!("[ERROR] GStreamer: {}", error.error());
            monitor.borrow_mut().close();
        }
        glib::ControlFlow::Continue
    }).map_err(|e| glib::Error::new(gio::IOErrorEnum::Failed, &e.to_string()))?;

    state.pipeline = Some(pipeline.clone());
    state.started = Some(Instant::now());
    pipeline.set_state(gst::State::Playing)
        .map_err(|e| glib::Error::new(gio::IOErrorEnum::Failed, &e.to_string()))?;
    println!("[OK] Захват запущен. Продолжительность: 15 секунд");
    let timer = app.clone();
    glib::timeout_add_local_once(Duration::from_secs(15), move || timer.borrow_mut().report());
    Ok(())
}
