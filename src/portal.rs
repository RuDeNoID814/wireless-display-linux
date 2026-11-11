use gio::prelude::*;
use glib::{
    variant::{ObjectPath, ToVariant},
    Variant, VariantDict,
};
use std::os::fd::OwnedFd;

const BUS: &str = "org.freedesktop.portal.Desktop";
const DESKTOP: &str = "/org/freedesktop/portal/desktop";
const SCREENCAST: &str = "org.freedesktop.portal.ScreenCast";

pub type Result<T> = std::result::Result<T, String>;

fn token() -> String {
    format!("ruden_{}", uuid::Uuid::new_v4().simple())
}

fn options(entries: &[(&str, Variant)]) -> Variant {
    let dict = VariantDict::new(None);
    for (key, value) in entries {
        dict.insert_value(key, value);
    }
    dict.end()
}

fn args(values: Vec<Variant>) -> Variant {
    Variant::tuple_from_iter(values)
}

fn object_path(value: &str) -> Result<Variant> {
    ObjectPath::try_from(value)
        .map(|path| path.to_variant())
        .map_err(|error| format!("Некорректный D-Bus путь {value:?}: {error}"))
}

pub struct Portal {
    connection: gio::DBusConnection,
    pub session: Option<String>,
}

impl Portal {
    pub fn connect() -> Result<Self> {
        let connection =
            gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).map_err(|error| {
                format!("Не удалось подключиться к пользовательскому D-Bus: {error}")
            })?;
        Ok(Self {
            connection,
            session: None,
        })
    }

    pub fn connection(&self) -> &gio::DBusConnection {
        &self.connection
    }

    fn call_request(&self, method: &str, arguments: Variant) -> Result<String> {
        let reply = self
            .connection
            .call_sync(
                Some(BUS),
                DESKTOP,
                SCREENCAST,
                method,
                Some(&arguments),
                None,
                gio::DBusCallFlags::NONE,
                10_000,
                gio::Cancellable::NONE,
            )
            .map_err(|error| format!("Portal.{method}: D-Bus вызов завершился ошибкой: {error}"))?;

        reply
            .child_value(0)
            .str()
            .map(str::to_owned)
            .filter(|path| glib::Variant::is_object_path(path))
            .ok_or_else(|| format!("Portal.{method}: отсутствует корректный путь Request"))
    }

    pub fn create_session(&self) -> Result<String> {
        self.call_request(
            "CreateSession",
            args(vec![options(&[
                ("handle_token", token().to_variant()),
                ("session_handle_token", token().to_variant()),
            ])]),
        )
    }

    pub fn select_sources(&self) -> Result<String> {
        let session = self
            .session
            .as_deref()
            .ok_or("Portal.SelectSources: сессия ещё не создана")?;
        self.call_request(
            "SelectSources",
            args(vec![
                object_path(session)?,
                options(&[
                    ("handle_token", token().to_variant()),
                    ("types", 4u32.to_variant()),
                    ("cursor_mode", 2u32.to_variant()),
                    ("multiple", false.to_variant()),
                ]),
            ]),
        )
    }

    pub fn start(&self) -> Result<String> {
        let session = self
            .session
            .as_deref()
            .ok_or("Portal.Start: сессия ещё не создана")?;
        self.call_request(
            "Start",
            args(vec![
                object_path(session)?,
                "".to_variant(),
                options(&[("handle_token", token().to_variant())]),
            ]),
        )
    }

    pub fn open_pipewire(&self) -> Result<OwnedFd> {
        let session = self
            .session
            .as_deref()
            .ok_or("Portal.OpenPipeWireRemote: сессия ещё не создана")?;
        let parameters = args(vec![object_path(session)?, options(&[])]);
        let (reply, fds) = self
            .connection
            .call_with_unix_fd_list_sync(
                Some(BUS),
                DESKTOP,
                SCREENCAST,
                "OpenPipeWireRemote",
                Some(&parameters),
                None,
                gio::DBusCallFlags::NONE,
                10_000,
                None::<&gio::UnixFDList>,
                gio::Cancellable::NONE,
            )
            .map_err(|error| format!("Portal.OpenPipeWireRemote: {error}"))?;

        let index = reply
            .child_value(0)
            .get::<glib::variant::Handle>()
            .ok_or_else(|| {
                format!(
                    "Portal.OpenPipeWireRemote: ожидался D-Bus handle (h), получен {}",
                    reply.child_value(0).type_()
                )
            })?
            .0;
        let fds = fds.ok_or("Portal.OpenPipeWireRemote: список FD отсутствует")?;
        fds.get(index)
            .map_err(|error| format!("Portal.OpenPipeWireRemote: FD #{index}: {error}"))
    }

    pub fn close(&mut self) {
        if let Some(session) = self.session.take() {
            if let Err(error) = self.connection.call_sync(
                Some(BUS),
                &session,
                "org.freedesktop.portal.Session",
                "Close",
                None,
                None,
                gio::DBusCallFlags::NONE,
                3_000,
                gio::Cancellable::NONE,
            ) {
                eprintln!("[WARN] Не удалось закрыть Portal-сессию: {error}");
            }
        }
    }
}

pub fn parse_response(parameters: &Variant) -> Result<(u32, VariantDict)> {
    if parameters.n_children() != 2 {
        return Err(format!(
            "Portal.Response: ожидались 2 аргумента, получено {}",
            parameters.n_children()
        ));
    }
    let code: u32 = parameters
        .child_value(0)
        .get()
        .ok_or("Portal.Response: неверный код ответа")?;
    let result = parameters.child_value(1);
    if result.type_().as_str() != "a{sv}" {
        return Err(format!(
            "Portal.Response: неверный тип результатов: {}",
            result.type_()
        ));
    }
    Ok((code, VariantDict::new(Some(&result))))
}

pub fn session_from_response(dict: &VariantDict) -> Result<String> {
    let session = dict
        .lookup_value("session_handle", None)
        .and_then(|value| value.str().map(str::to_owned))
        .ok_or("Portal.CreateSession: нет session_handle")?;
    if !Variant::is_object_path(&session) {
        return Err(format!(
            "Portal.CreateSession: неверный путь сессии: {session}"
        ));
    }
    Ok(session)
}

pub fn stream_from_response(dict: &VariantDict) -> Result<(u32, Variant)> {
    let streams = dict
        .lookup_value("streams", None)
        .ok_or("Portal.Start: отсутствует streams")?;
    if streams.n_children() == 0 {
        return Err("Portal.Start: список потоков пуст".into());
    }
    let first = streams.child_value(0);
    let node: u32 = first
        .child_value(0)
        .get()
        .ok_or("Portal.Start: неверный PipeWire node ID")?;
    Ok((node, first))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_session_signature() {
        let input = args(vec![options(&[("handle_token", "test".to_variant())])]);
        assert_eq!(input.type_().as_str(), "(a{sv})");
    }

    #[test]
    fn select_sources_signature() {
        let input = args(vec![object_path("/example/session").unwrap(), options(&[])]);
        assert_eq!(input.type_().as_str(), "(oa{sv})");
    }

    #[test]
    fn invalid_session_path_is_rejected() {
        assert!(object_path("not/a/path").is_err());
    }
}
