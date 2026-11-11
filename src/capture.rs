use gst::prelude::*;
use gstreamer as gst;
use std::{
    os::fd::{AsRawFd, OwnedFd},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};

pub type Result<T> = std::result::Result<T, String>;

pub struct Stats {
    frames: AtomicU64,
    first_frame: Mutex<Option<Instant>>,
}

impl Stats {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            frames: AtomicU64::new(0),
            first_frame: Mutex::new(None),
        })
    }

    pub fn frames(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }

    pub fn report(&self) {
        let frames = self.frames.load(Ordering::Relaxed);
        let first = match self.first_frame.lock() {
            Ok(guard) => *guard,
            Err(error) => {
                eprintln!("[ERROR] Статистика: {error}");
                None
            }
        };
        let fps = first
            .map(|time| frames as f64 / time.elapsed().as_secs_f64().max(0.001))
            .unwrap_or(0.0);
        println!("[RESULT] Кадров: {frames}; средний FPS: {fps:.2}");
        println!(
            "[RESULT] {}",
            if frames > 0 {
                "Видеокадры получены"
            } else {
                "Видеокадры НЕ получены"
            }
        );
    }
}

pub struct Capture {
    pub pipeline: gst::Pipeline,
    pub watch: Option<gst::bus::BusWatchGuard>,
    // Keep the portal-supplied descriptor alive for the entire capture session.
    _remote: OwnedFd,
}

impl Capture {
    pub fn new(remote: OwnedFd, node_id: u32, stats: Arc<Stats>) -> Result<Self> {
        let description = concat!(
            "pipewiresrc name=source do-timestamp=true ",
            "! videoconvert ! video/x-raw,format=RGB ",
            "! appsink name=sink emit-signals=true sync=false max-buffers=2 drop=true"
        );
        let element =
            gst::parse::launch(description).map_err(|error| format!("GStreamer.parse: {error}"))?;
        let pipeline = element
            .downcast::<gst::Pipeline>()
            .map_err(|_| "GStreamer.parse: элемент не является Pipeline".to_string())?;
        let source = pipeline
            .by_name("source")
            .ok_or("GStreamer.pipeline: отсутствует pipewiresrc")?;
        let sink = pipeline
            .by_name("sink")
            .ok_or("GStreamer.pipeline: отсутствует appsink")?;

        source.set_property("fd", remote.as_raw_fd());
        source.set_property("path", node_id.to_string());

        sink.connect("new-sample", false, move |values| {
            let Some(object) = values.first() else {
                return Some(gst::FlowReturn::Error.to_value());
            };
            let Ok(sink) = object.get::<gst::Element>() else {
                return Some(gst::FlowReturn::Error.to_value());
            };
            let sample = sink.emit_by_name::<Option<gst::Sample>>("pull-sample", &[]);
            if let Some(sample) = sample {
                stats.frames.fetch_add(1, Ordering::Relaxed);
                if let Ok(mut first) = stats.first_frame.lock() {
                    if first.is_none() {
                        *first = Some(Instant::now());
                        println!("[OK] Получен первый кадр");
                        if let Some(caps) = sample.caps() {
                            println!("[VIDEO] {caps}");
                        }
                    }
                }
            }
            Some(gst::FlowReturn::Ok.to_value())
        });

        Ok(Self {
            pipeline,
            watch: None,
            _remote: remote,
        })
    }

    pub fn watch_errors(&mut self, on_error: impl Fn(String) + 'static) -> Result<()> {
        let bus = self
            .pipeline
            .bus()
            .ok_or("GStreamer.pipeline: отсутствует message bus")?;
        self.watch = Some(
            bus.add_watch_local(move |_, message| {
                match message.view() {
                    gst::MessageView::Error(error) => {
                        let origin = message
                            .src()
                            .map(|src| src.path_string().to_string())
                            .unwrap_or_else(|| "unknown".to_string());
                        on_error(format!(
                            "GStreamer [{origin}]: {} | debug: {:?}",
                            error.error(),
                            error.debug()
                        ));
                    }
                    gst::MessageView::Warning(warning) => {
                        eprintln!(
                            "[WARN] GStreamer: {} | debug: {:?}",
                            warning.error(),
                            warning.debug()
                        );
                    }
                    gst::MessageView::StateChanged(change) => {
                        if message
                            .src()
                            .is_some_and(|src| src.type_() == gst::Pipeline::static_type())
                        {
                            println!(
                                "[STATE] Pipeline: {:?} -> {:?}",
                                change.old(),
                                change.current()
                            );
                        }
                    }
                    _ => {}
                }
                glib::ControlFlow::Continue
            })
            .map_err(|error| format!("GStreamer.bus: не удалось подписаться: {error}"))?,
        );
        Ok(())
    }

    pub fn stop(mut self) {
        self.watch.take();
        // A PipeWire state transition may block. Do not freeze the GLib loop
        // while tearing down the diagnostic.
        let pipeline = self.pipeline;
        let remote = self._remote;
        if let Err(error) = std::thread::Builder::new()
            .name("gstreamer-stop".into())
            .spawn(move || {
                if let Err(error) = pipeline.set_state(gst::State::Null) {
                    eprintln!("[WARN] GStreamer.stop: {error}");
                }
                drop(remote);
            })
        {
            eprintln!("[WARN] Не удалось запустить поток остановки GStreamer: {error}");
        }
    }
}
