//! Wireless Display for Linux
//! WD-005: XDG Desktop Portal -> PipeWire -> GStreamer
//!
//! Diagnostic prototype:
//! - Creates a ScreenCast session.
//! - Requests a virtual monitor.
//! - Connects to PipeWire.
//! - Captures frames for 15 seconds.
//! - Reports the number of captured frames.
//!
//! Network streaming and video encoding are not implemented yet.

use gio::prelude::*;
use glib::{
    variant::{ObjectPath, ToVariant},
    MainLoop, Variant, VariantDict,
};
use gstreamer as gst;
use gst::prelude::*;

use std::{
    cell::RefCell,
    os::fd::IntoRawFd,
    rc::Rc,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

const BUS: &str = "org.freedesktop.portal.Desktop";
const DESKTOP: &str = "/org/freedesktop/portal/desktop";
const SCREENCAST: &str = "org.freedesktop.portal.ScreenCast";

const TEST_DURATION: u64 = 15;

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

// Preserve the actual GVariant types of tuple elements.
// For example: (a{sv}), (oa{sv}) and (osa{sv}).
fn arguments(values: Vec<Variant>) -> Variant {
    Variant::tuple_from_iter(values)
}

fn session_path(path: &str) -> Result<Variant, glib::Error> {
    let path = ObjectPath::try_from(path).map_err(|_| {
        glib::Error::new(
            gio::IOErrorEnum::InvalidArgument,
            "Неверный D-Bus путь Portal-сессии",
        )
    })?;

    Ok(path.to_variant())
}

struct App {
    bus: gio::DBusConnection,
    main_loop: MainLoop,

    session: Option<String>,
    request: Option<String>,
    stage: &'static str,

    pipeline: Option<gst::Pipeline>,
    bus_watch: Option<gst::bus::BusWatchGuard>,
    pipewire_fd: Option<i32>,

    frames: Arc<AtomicU64>,
    started: Option<Instant>,
    first_frame: Arc<Mutex<Option<Instant>>>,

    stopped: bool,
}

impl App {
    fn call(
        &self,
        name: &str,
        arguments: Variant,
    ) -> Result<String, glib::Error> {
        let reply = self.bus.call_sync(
            Some(BUS),
                                       DESKTOP,
                                       SCREENCAST,
                                       name,
                                       Some(&arguments),
                                       None,
                                       gio::DBusCallFlags::NONE,
                                       10_000,
                                       gio::Cancellable::NONE,
        )?;

        Ok(reply
        .child_value(0)
        .str()
        .unwrap_or_default()
        .to_owned())
    }

    fn create(&mut self) -> Result<(), glib::Error> {
        println!("[1/4] Создание ScreenCast-сессии...");

        self.stage = "create";

        let params = arguments(vec![
            options(&[
                ("handle_token", token().to_variant()),
                    ("session_handle_token", token().to_variant()),
            ]),
        ]);

        self.request = Some(self.call("CreateSession", params)?);

        Ok(())
    }

    fn select(&mut self) -> Result<(), glib::Error> {
        println!("[2/4] Запрос виртуального дисплея...");

        self.stage = "select";

        let session = self
        .session
        .as_ref()
        .expect("Portal session missing");

        let params = arguments(vec![
            session_path(session)?,
                               options(&[
                                   ("handle_token", token().to_variant()),
                                       ("types", 4u32.to_variant()),
                                       ("cursor_mode", 2u32.to_variant()),
                                       ("multiple", false.to_variant()),
                               ]),
        ]);

        self.request = Some(self.call("SelectSources", params)?);

        Ok(())
    }

    fn start(&mut self) -> Result<(), glib::Error> {
        println!("[3/4] Запуск виртуального дисплея...");
        println!("[INFO] Подтверди запрос KDE, если появится окно.");

        self.stage = "start";

        let session = self
        .session
        .as_ref()
        .expect("Portal session missing");

        let params = arguments(vec![
            session_path(session)?,
                               "".to_variant(),
                               options(&[
                                   ("handle_token", token().to_variant()),
                               ]),
        ]);

        self.request = Some(self.call("Start", params)?);

        Ok(())
    }

    fn close(&mut self) {
        if self.stopped {
            return;
        }

        self.stopped = true;

        if let Some(pipeline) = self.pipeline.take() {
            let _ = pipeline.set_state(gst::State::Null);
        }

        // Keep the GStreamer bus watch alive until shutdown.
        self.bus_watch.take();

        if let Some(fd) = self.pipewire_fd.take() {
            // SAFETY: This file descriptor is owned by App
            // after into_raw_fd() and is closed only once.
            unsafe {
                libc::close(fd);
            }
        }

        if let Some(session) = self.session.take() {
            let _ = self.bus.call_sync(
                Some(BUS),
                                       &session,
                                       "org.freedesktop.portal.Session",
                                       "Close",
                                       None,
                                       None,
                                       gio::DBusCallFlags::NONE,
                                       3_000,
                                       gio::Cancellable::NONE,
            );

            println!("[OK] Portal-сессия закрыта.");
        }

        self.main_loop.quit();
    }

    fn report(&mut self) {
        let seconds = self
        .started
        .map(|time| time.elapsed().as_secs_f64())
        .unwrap_or(0.0);

        let frames = self.frames.load(Ordering::Relaxed);

        let first_frame = *self
        .first_frame
        .lock()
        .expect("First frame mutex poisoned");

        let fps = first_frame
        .map(|time| {
            frames as f64 / time.elapsed().as_secs_f64().max(0.001)
        })
        .unwrap_or(0.0);

        println!();
        println!("========== WD-005 / RUST ==========");
        println!("Продолжительность: {seconds:.2} с");
        println!("Получено кадров: {frames}");
        println!("Средний FPS: {fps:.2}");

        if frames > 0 {
            println!("Статус: ВИДЕОКАДРЫ ПОЛУЧЕНЫ");
        } else {
            println!("Статус: КАДРЫ НЕ ПОЛУЧЕНЫ");
        }

        println!("===================================");

        self.close();
    }
}

fn capture(
    app: Rc<RefCell<App>>,
    state: &mut App,
    stream_id: u32,
) -> Result<(), glib::Error> {
    println!("[4/4] Подключение к PipeWire...");

    let session = state
    .session
    .as_ref()
    .expect("Portal session missing");

    let params = arguments(vec![
        session_path(session)?,
                           options(&[]),
    ]);

    let (reply, fd_list) = state.bus.call_with_unix_fd_list_sync(
        Some(BUS),
                                                                 DESKTOP,
                                                                 SCREENCAST,
                                                                 "OpenPipeWireRemote",
                                                                 Some(&params),
                                                                 None,
                                                                 gio::DBusCallFlags::NONE,
                                                                 10_000,
                                                                 None::<&gio::UnixFDList>,
                                                                 gio::Cancellable::NONE,
    )?;

    let index: i32 = reply
    .child_value(0)
    .get()
    .unwrap_or(-1);

    let fd = fd_list
    .as_ref()
    .ok_or_else(|| {
        glib::Error::new(
            gio::IOErrorEnum::Failed,
            "Portal не вернул список файловых дескрипторов",
        )
    })?
    .get(index)?;

    let raw_fd = fd.into_raw_fd();

    println!("[OK] PipeWire FD: {raw_fd}");

    state.pipewire_fd = Some(raw_fd);

    let pipeline_description = concat!(
        "pipewiresrc name=source do-timestamp=true ",
        "! videoconvert ",
        "! video/x-raw,format=RGB ",
        "! appsink name=sink ",
        "emit-signals=true ",
        "sync=false ",
        "max-buffers=2 ",
        "drop=true"
    );

    let element = gst::parse::launch(pipeline_description)
    .map_err(|error| {
        glib::Error::new(
            gio::IOErrorEnum::Failed,
            &error.to_string(),
        )
    })?;

    let pipeline = element
    .downcast::<gst::Pipeline>()
    .map_err(|_| {
        glib::Error::new(
            gio::IOErrorEnum::Failed,
            "Не удалось создать GStreamer Pipeline",
        )
    })?;

    let source = pipeline
    .by_name("source")
    .expect("pipewiresrc element missing");

    source.set_property("fd", raw_fd);
    source.set_property("path", stream_id.to_string());

    let sink = pipeline
    .by_name("sink")
    .expect("appsink element missing");

    let frames = state.frames.clone();
    let first_frame = state.first_frame.clone();

    sink.connect(
        "new-sample",
        false,
        move |values| {
            let sink = values[0]
            .get::<gst::Element>()
            .expect("Invalid appsink element");

            let sample = sink.emit_by_name::<Option<gst::Sample>>(
                "pull-sample",
                &[],
            );

            if let Some(sample) = sample {
                frames.fetch_add(1, Ordering::Relaxed);

                let mut first = first_frame
                .lock()
                .expect("First frame mutex poisoned");

                if first.is_none() {
                    *first = Some(Instant::now());

                    println!("[OK] Первый видеокадр получен!");

                    if let Some(caps) = sample.caps() {
                        println!("[VIDEO] Формат: {caps}");
                    }
                }
            }

            Some(gst::FlowReturn::Ok.to_value())
        },
    );

    let gst_bus = pipeline
    .bus()
    .expect("GStreamer bus missing");

    let monitor = app.clone();

    let watch = gst_bus
    .add_watch_local(move |_, message| {
        if let gst::MessageView::Error(error) = message.view() {
            eprintln!(
                "[ERROR] GStreamer: {}",
                error.error()
            );

            monitor.borrow_mut().close();
        }

        glib::ControlFlow::Continue
    })
    .map_err(|error| {
        glib::Error::new(
            gio::IOErrorEnum::Failed,
            &error.to_string(),
        )
    })?;

    state.bus_watch = Some(watch);
    state.pipeline = Some(pipeline.clone());
    state.started = Some(Instant::now());

    pipeline
    .set_state(gst::State::Playing)
    .map_err(|error| {
        glib::Error::new(
            gio::IOErrorEnum::Failed,
            &error.to_string(),
        )
    })?;

    println!(
        "[OK] Захват запущен. Продолжительность: {TEST_DURATION} секунд"
    );

    let timer = app.clone();

    glib::timeout_add_local_once(
        Duration::from_secs(TEST_DURATION),
                                 move || {
                                     timer.borrow_mut().report();
                                 },
    );

    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    gst::init()?;

    let bus = gio::bus_get_sync(
        gio::BusType::Session,
        gio::Cancellable::NONE,
    )?;

    let main_loop = MainLoop::new(None, false);

    let app = Rc::new(RefCell::new(App {
        bus: bus.clone(),
                                   main_loop: main_loop.clone(),

                                   session: None,
                                   request: None,
                                   stage: "",

                                   pipeline: None,
                                   bus_watch: None,
                                   pipewire_fd: None,

                                   frames: Arc::new(AtomicU64::new(0)),
                                   started: None,
                                   first_frame: Arc::new(Mutex::new(None)),

                                   stopped: false,
    }));

    let handler = app.clone();

    bus.signal_subscribe(
        None,
        Some("org.freedesktop.portal.Request"),
                         Some("Response"),
                         None,
                         None,
                         gio::DBusSignalFlags::NONE,
                         move |_, _, path, _, _, params| {
                             let mut state = handler.borrow_mut();

                             if state.request.as_deref() != Some(path) {
                                 return;
                             }

                             let status: u32 = params
                             .child_value(0)
                             .get()
                             .unwrap_or(u32::MAX);

                             let result = params.child_value(1);
                             let dict = VariantDict::new(Some(&result));

                             if status != 0 {
                                 eprintln!(
                                     "[ERROR] Portal отклонил запрос: {status}"
                                 );

                                 state.close();
                                 return;
                             }

                             let action = match state.stage {
                                 "create" => {
                                     let session = dict
                                     .lookup_value("session_handle", None)
                                     .and_then(|value| {
                                         value.str().map(str::to_owned)
                                     });

                                     if let Some(session) = session {
                                         state.session = Some(session);

                                         println!("[OK] Сессия создана.");

                                         state.select()
                                     } else {
                                         Err(glib::Error::new(
                                             gio::IOErrorEnum::Failed,
                                             "session_handle отсутствует",
                                         ))
                                     }
                                 }

                                 "select" => {
                                     println!(
                                         "[OK] Виртуальный источник выбран."
                                     );

                                     state.start()
                                 }

                                 "start" => {
                                     println!(
                                         "[OK] ScreenCast успешно запущен!"
                                     );

                                     let streams = dict.lookup_value(
                                         "streams",
                                         None,
                                     );

                                     if let Some(streams) = streams {
                                         if streams.n_children() == 0 {
                                             Err(glib::Error::new(
                                                 gio::IOErrorEnum::Failed,
                                                 "Portal вернул пустой streams",
                                             ))
                                         } else {
                                             let stream_id: u32 = streams
                                             .child_value(0)
                                             .child_value(0)
                                             .get()
                                             .unwrap_or(0);

                                             println!(
                                                 "[DEBUG] Данные потока: {}",
                                                 streams.child_value(0).print(true)
                                             );

                                             capture(
                                                 handler.clone(),
                                                     &mut state,
                                                     stream_id,
                                             )
                                         }
                                     } else {
                                         Err(glib::Error::new(
                                             gio::IOErrorEnum::Failed,
                                             "streams отсутствует в ответе Portal",
                                         ))
                                     }
                                 }

                                 _ => Ok(()),
                             };

                             if let Err(error) = action {
                                 eprintln!("[ERROR] {error}");

                                 state.close();
                             }
                         },
    );

    println!(
        "RuDeNoID Wireless Display — WD-005 / Rust"
    );

    app.borrow_mut().create()?;

    main_loop.run();

    println!("[INFO] Тест завершён.");

    Ok(())
}
