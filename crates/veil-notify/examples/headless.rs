//! Headless notification daemon for poking at step 3 without any UI.
//!
//!     cargo run -p veil-notify --example headless
//!     notify-send -A yes=Yes -A no=No --wait "hello" "<b>body</b>"
//!
//! Prints every notification it receives. A notification with actions has its
//! first button "clicked" after 300 ms (so `notify-send --wait` prints the key);
//! anything else just "expires" after 1 s. If another daemon owns the name
//! (DMS, swaync, mako) this one queues and says so, which is the nested-testing
//! behaviour. To leave your real session alone, run it on a private bus:
//!
//!     dbus-run-session -- sh -c 'cargo run -p veil-notify --example headless & sleep 2; notify-send hi'

use std::sync::Arc;
use std::time::Duration;

use veil_notify::dbus::{CloseReason, Event, NameState, Service, ServiceConfig};
use veil_notify::ids::IdGen;

fn main() {
    let svc = Arc::new(Service::spawn(ServiceConfig::default(), IdGen::new(1)).expect("spawn worker"));
    while let Ok(ev) = svc.events.recv() {
        match ev {
            Event::Name(NameState::Owner) => eprintln!("owning org.freedesktop.Notifications"),
            Event::Name(NameState::Queued) => eprintln!("another daemon owns the name; queued behind it"),
            Event::Unavailable(why) => {
                eprintln!("no D-Bus: {why}");
                return;
            }
            Event::Close { id } => {
                eprintln!("#{id}: app asked to close it");
                svc.closed(id, CloseReason::Closed);
            }
            Event::Notify { notification: n, replaced } => {
                let buttons: Vec<_> = n.buttons().map(|a| a.key.as_str()).collect();
                eprintln!(
                    "#{} {} from {:?}: {:?} / {:?} urgency={:?} buttons={:?} default={} image={}",
                    n.id,
                    if replaced { "(replaces)" } else { "" },
                    n.app_name,
                    n.summary,
                    n.body,
                    n.urgency,
                    buttons,
                    n.default_action().is_some(),
                    n.image.is_some()
                );
                let (id, svc) = (n.id, Arc::clone(&svc));
                let click = n.buttons().next().map(|a| a.key.clone());
                std::thread::spawn(move || match click {
                    Some(key) => {
                        std::thread::sleep(Duration::from_millis(300));
                        svc.action(id, &key);
                        svc.closed(id, CloseReason::Dismissed);
                    }
                    None => {
                        std::thread::sleep(Duration::from_secs(1));
                        svc.closed(id, CloseReason::Expired);
                    }
                });
            }
        }
    }
}
