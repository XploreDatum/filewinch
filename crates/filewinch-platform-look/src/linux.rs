//! Linux: the XDG desktop portal's `org.freedesktop.appearance` `accent-color` (GNOME 47+,
//! KDE Plasma 6), read once and followed through `SettingChanged`, as Rune follows
//! `color-scheme`. No portal, or no accent set, leaves `current()` `None`.

use std::time::Duration;

use futures_lite::{FutureExt, StreamExt};
use zbus::zvariant::{OwnedValue, Value};
use zbus::{Connection, Proxy};

use crate::{Accent, portal_accent, set};

const DEST: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";
const INTERFACE: &str = "org.freedesktop.portal.Settings";
const NAMESPACE: &str = "org.freedesktop.appearance";
const KEY: &str = "accent-color";
/// Connecting, D-Bus activation of the portal and the first read.
const RPC_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) fn run() {
    async_io::block_on(watch());
}

/// The accent in a portal reply. Portals answer `v`, and `Read` (before `ReadOne`) wraps it twice.
pub(crate) fn accent(value: &Value<'_>) -> Option<Accent> {
    match value {
        Value::Value(inner) => accent(inner),
        Value::Structure(s) => match s.fields() {
            [Value::F64(r), Value::F64(g), Value::F64(b)] => {
                portal_accent(*r, *g, *b).map(|base| Accent { base, light2: None, dark1: None })
            }
            _ => None,
        },
        _ => None,
    }
}

async fn watch() {
    let setup = async {
        let conn = Connection::session().await.ok()?;
        let portal = Proxy::new(&conn, DEST, PATH, INTERFACE).await.ok()?;
        // Subscribe first, so a change between the read and the subscription isn't lost.
        let changes = portal.receive_signal("SettingChanged").await.ok()?;
        let read = match portal.call::<_, _, OwnedValue>("ReadOne", &(NAMESPACE, KEY)).await {
            Ok(v) => Some(v),
            Err(_) => portal.call::<_, _, OwnedValue>("Read", &(NAMESPACE, KEY)).await.ok(),
        };
        // An older portal without the key still sends `SettingChanged` once it gains one.
        set(read.as_deref().and_then(accent));
        Some((conn, changes))
    }
    .or(async {
        async_io::Timer::after(RPC_TIMEOUT).await;
        None
    })
    .await;
    let Some((_conn, mut changes)) = setup else { return };
    while let Some(message) = changes.next().await {
        if let Ok((namespace, key, value)) = message.body().deserialize::<(String, String, OwnedValue)>()
            && namespace == NAMESPACE
            && key == KEY
        {
            set(accent(&value));
        }
    }
}
