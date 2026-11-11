//! WD-005.2: KDE virtual ScreenCast monitor -> PipeWire -> GStreamer.
//! This is a diagnostic capture host. No TV transport or encoder yet.

mod capture;
mod portal;

use capture::{Capture, Stats};
use gst::prelude::*;
use gstreamer as gst;
use std::{
    cell::RefCell,
    rc::Rc,
    sync::{mpsc, Arc},
    time::{Duration, Instant},
};

const CAPTURE_SECONDS: u64 = 15;
const CONNECTION_TIMEOUT_SECONDS: u64 = 45;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Create,
    Select,
    Start,
    Capturing,
    Finished,
}

struct App {
    portal: portal::Portal,
    loop_: glib::MainLoop,
    request: Option<String>,
    stage: Stage,
    capture: Option<Capture>,
    stats: Arc<Stats>,
    capture_started: Option<Instant>,
}

impl App {
    fn fail(&mut self, context: &str, error: impl std::fmt::Display) {
        eprintln!("[ERROR] {context}: {error}");
        self.finish();
    }

    fn finish(&mut self) {
        if self.stage == Stage::Finished {
            return;
        }
        self.stage = Stage::Finished;
        self.request = None;
        if let Some(capture) = self.capture.take() {
            capture.stop();
        }
        self.portal.close();
        self.stats.report();
        self.loop_.quit();
    }

    fn next(&mut self, stage: Stage, request: portal::Result<String>) {
        match request {
            Ok(path) => {
                self.stage = stage;
                self.request = Some(path);
            }
            Err(error) => self.fail("Portal request", error),
        }
    }

    fn handle_response(
        &mut self,
        path: &str,
        parameters: &glib::Variant,
    ) -> portal::Result<Option<u32>> {
        if self.request.as_deref() != Some(path) || self.stage == Stage::Finished {
            return Ok(None);
        }
        let (status, result) = portal::parse_response(parameters)?;
        if status != 0 {
            return Err(format!(
                "Portal отклонил этап {:?}, код {status} (1 — отмена)",
                self.stage
            ));
        }
        match self.stage {
            Stage::Create => {
                self.portal.session = Some(portal::session_from_response(&result)?);
                println!("[1/4] [OK] ScreenCast-сессия создана");
                let request = self.portal.select_sources();
                self.next(Stage::Select, request);
                Ok(None)
            }
            Stage::Select => {
                println!("[2/4] [OK] Виртуальный источник выбран");
                let request = self.portal.start();
                self.next(Stage::Start, request);
                Ok(None)
            }
            Stage::Start => {
                let (node_id, data) = portal::stream_from_response(&result)?;
                println!("[3/4] [OK] Виртуальный монитор: {data}");
                self.stage = Stage::Capturing;
                self.request = None;
                Ok(Some(node_id))
            }
            _ => Ok(None),
        }
    }
}

fn begin_capture(app: Rc<RefCell<App>>, node_id: u32) -> Result<(), String> {
    let (remote, stats) = {
        let state = app.borrow();
        (state.portal.open_pipewire()?, state.stats.clone())
    };
    println!("[4/4] PipeWire remote открыт; node ID = {node_id}");

    let mut capture = Capture::new(remote, node_id, stats)?;
    let on_error = app.clone();
    capture.watch_errors(move |error| {
        // Events arrive on the GLib thread. Close only once.
        on_error.borrow_mut().fail("GStreamer bus", error);
    })?;
    let pipeline = capture.pipeline.clone();

    {
        let mut state = app.borrow_mut();
        state.capture_started = Some(Instant::now());
        state.capture = Some(capture);
    }

    // GstPipeWireSrc can block inside set_state(Playing). Never call it from
    // a D-Bus signal handler with Rc<RefCell<App>> borrowed.
    let (sender, receiver) = mpsc::channel();
    std::thread::Builder::new()
        .name("gstreamer-start".into())
        .spawn(move || {
            let status = pipeline
                .set_state(gst::State::Playing)
                .map(|value| format!("{value:?}"))
                .map_err(|error| format!("GStreamer.set_state(Playing): {error}"));
            let _ = sender.send(status);
        })
        .map_err(|error| format!("Не удалось создать поток запуска GStreamer: {error}"))?;

    let poll_app = app.clone();
    let mut response_received = false;
    let mut capture_timer_scheduled = false;
    glib::timeout_add_local(Duration::from_millis(100), move || {
        if poll_app.borrow().stage == Stage::Finished {
            return glib::ControlFlow::Break;
        }
        if !response_received {
            match receiver.try_recv() {
                Ok(Ok(status)) => {
                    println!("[STATE] set_state(Playing) вернул {status}");
                    response_received = true;
                }
                Ok(Err(error)) => {
                    poll_app.borrow_mut().fail("Запуск видеопотока", error);
                    return glib::ControlFlow::Break;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    poll_app.borrow_mut().fail(
                        "Запуск видеопотока",
                        "Поток GStreamer завершился без результата",
                    );
                    return glib::ControlFlow::Break;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if !capture_timer_scheduled && poll_app.borrow().stats.frames() > 0 {
            capture_timer_scheduled = true;
            let report_app = poll_app.clone();
            println!("[INFO] Захват активен; измеряем FPS {CAPTURE_SECONDS} секунд");
            glib::timeout_add_local_once(Duration::from_secs(CAPTURE_SECONDS), move || {
                if report_app.borrow().stage != Stage::Finished {
                    report_app.borrow_mut().finish();
                }
            });
        }
        let elapsed = poll_app
            .borrow()
            .capture_started
            .map(|time| time.elapsed())
            .unwrap_or_default();
        if !capture_timer_scheduled && elapsed >= Duration::from_secs(CONNECTION_TIMEOUT_SECONDS) {
            poll_app
                .borrow_mut()
                .fail("Тайм-аут", "GStreamer не завершил запуск за 45 секунд");
            return glib::ControlFlow::Break;
        }
        glib::ControlFlow::Continue
    });

    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    gst::init().map_err(|error| format!("GStreamer.init: {error}"))?;
    let portal = portal::Portal::connect()?;
    let main_loop = glib::MainLoop::new(None, false);
    let app = Rc::new(RefCell::new(App {
        portal,
        loop_: main_loop.clone(),
        request: None,
        stage: Stage::Create,
        capture: None,
        stats: Stats::new(),
        capture_started: None,
    }));

    let connection = app.borrow().portal.connection().clone();
    let response_app = app.clone();
    let _responses = connection.subscribe_to_signal(
        None,
        Some("org.freedesktop.portal.Request"),
        Some("Response"),
        None,
        None,
        gio::DBusSignalFlags::NONE,
        move |signal| {
            let result = response_app
                .borrow_mut()
                .handle_response(signal.object_path, signal.parameters);
            match result {
                Ok(Some(node_id)) => {
                    if let Err(error) = begin_capture(response_app.clone(), node_id) {
                        response_app
                            .borrow_mut()
                            .fail("Подключение PipeWire", error);
                    }
                }
                Ok(None) => {}
                Err(error) => response_app.borrow_mut().fail("Ответ Portal", error),
            }
        },
    );

    let closed_app = app.clone();
    let _closed = connection.subscribe_to_signal(
        None,
        Some("org.freedesktop.portal.Session"),
        Some("Closed"),
        None,
        None,
        gio::DBusSignalFlags::NONE,
        move |signal| {
            let own_session =
                closed_app.borrow().portal.session.as_deref() == Some(signal.object_path);
            if own_session && closed_app.borrow().stage != Stage::Finished {
                closed_app
                    .borrow_mut()
                    .fail("Portal", "Сессия закрыта окружением рабочего стола");
            }
        },
    );

    println!("RuDeNoID Wireless Display — WD-005.2 / Rust");
    let request = app.borrow().portal.create_session()?;
    app.borrow_mut().request = Some(request);
    main_loop.run();
    println!("[INFO] Программа завершена");
    Ok(())
}
