//! Port hotplug notifications, from the MIDI 1.0 port watcher that keeps the backend's port list.
//!
//! The watcher updates its list before it raises an event, so `ports()` and `port_name()` in a
//! callback already see the new state. The watcher starts when ports are first listed or when the
//! first `PortWatcher` is created; either way, creation waits for the first enumeration, so the
//! ports that are already there are not reported as additions.
//!
//! Callbacks run on the threads that raise the events, as with the other midir backends. Nothing
//! here may panic across the WinRT boundary.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex, MutexGuard};

use log::{error, warn};
use windows_core::Ref;

use super::bindings::Windows::Devices::Midi2::Enumeration::Legacy::{
    MidiLegacyPortDeviceInformation, MidiLegacyPortDeviceInformationAddedEventArgs,
    MidiLegacyPortDeviceInformationRemovedEventArgs, MidiLegacyPortDeviceWatcher,
};
use super::bindings::Windows::Devices::Midi2::Enumeration::Midi1PortFlow;
use super::bindings::Windows::Foundation::TypedEventHandler;
use super::watcher;
use super::{ensure_available, MidiInputPort, MidiOutputPort};
use crate::common::PortEvent;
use crate::errors::InitError;

type AddedEvent =
    TypedEventHandler<MidiLegacyPortDeviceWatcher, MidiLegacyPortDeviceInformationAddedEventArgs>;
type RemovedEvent =
    TypedEventHandler<MidiLegacyPortDeviceWatcher, MidiLegacyPortDeviceInformationRemovedEventArgs>;
type PortCallback<T> = Box<dyn FnMut(PortEvent, &mut T) + Send>;

struct CallbackData<T> {
    callback: PortCallback<T>,
    data: Option<T>,
    /// Cleared before the handlers are removed, so that events which are already on their way
    /// don't call the user callback any more.
    active: bool,
}

fn lock<T>(shared: &Mutex<T>) -> MutexGuard<'_, T> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

/// Converts a port from the watcher into a midir port event that can be passed to the callback.
fn port_event(port: &MidiLegacyPortDeviceInformation, added: bool) -> Option<PortEvent> {
    let (_, info) = super::port_info(port)?;
    let input = || MidiInputPort {
        port_device_id: info.port_device_id.clone(),
        endpoint_device_id: info.endpoint_device_id.clone(),
        group: info.group,
    };
    let output = || MidiOutputPort {
        port_device_id: info.port_device_id.clone(),
        endpoint_device_id: info.endpoint_device_id.clone(),
        group: info.group,
    };
    let flow = port.Flow();
    let event = if flow == Midi1PortFlow::MidiMessageSource {
        let port = crate::common::MidiInputPort { imp: input() };
        if added {
            PortEvent::InputAdded(port)
        } else {
            PortEvent::InputRemoved(port)
        }
    } else if flow == Midi1PortFlow::MidiMessageDestination {
        let port = crate::common::MidiOutputPort { imp: output() };
        if added {
            PortEvent::OutputAdded(port)
        } else {
            PortEvent::OutputRemoved(port)
        }
    } else {
        return None;
    };
    Some(event)
}

fn dispatch<T>(
    shared: &Mutex<CallbackData<T>>,
    port: &MidiLegacyPortDeviceInformation,
    added: bool,
) {
    let Some(event) = port_event(port, added) else {
        return;
    };

    let mut guard = lock(shared);
    if !guard.active {
        return;
    }
    let CallbackData { callback, data, .. } = &mut *guard;
    let Some(data) = data.as_mut() else {
        return;
    };
    if catch_unwind(AssertUnwindSafe(|| callback(event, data))).is_err() {
        error!("A MIDI port watcher callback panicked");
    }
}

fn handle_added<T>(
    args: &MidiLegacyPortDeviceInformationAddedEventArgs,
    shared: &Mutex<CallbackData<T>>,
) {
    if let Some(port) = args.AddedDevice() {
        dispatch(shared, &port, true);
    }
}

fn handle_removed<T>(
    args: &MidiLegacyPortDeviceInformationRemovedEventArgs,
    shared: &Mutex<CallbackData<T>>,
) {
    if let Some(port) = args.RemovedDevice() {
        dispatch(shared, &port, false);
    }
}

/// Watches for MIDI ports being added to and removed from the system.
///
/// This is the `winmidi2` implementation of `midir::PortWatcher`. It listens to the same
/// `MidiLegacyPortDeviceWatcher` that keeps the backend's port list up to date, so the port list
/// has already changed by the time a callback runs.
pub struct PortWatcher<T: 'static> {
    watcher: Option<MidiLegacyPortDeviceWatcher>,
    added: Option<i64>,
    removed: Option<i64>,
    shared: Arc<Mutex<CallbackData<T>>>,
}

impl<T: Send> PortWatcher<T> {
    /// Creates a watcher and calls `callback` for every port that is added or removed afterwards.
    pub fn new<F>(_client_name: &str, callback: F, data: T) -> Result<Self, InitError>
    where
        F: FnMut(PortEvent, &mut T) + Send + 'static,
    {
        ensure_available()?;
        // Starting the watcher, if needed, waits for its first enumeration. Subscribing after
        // that means the initial `Added` events are not reported as changes.
        let watcher = watcher::current().ok_or(InitError)?;

        let shared = Arc::new(Mutex::new(CallbackData {
            callback: Box::new(callback),
            data: Some(data),
            active: true,
        }));

        let added = {
            let shared = shared.clone();
            let handler = AddedEvent::new(
                move |_: Ref<'_, MidiLegacyPortDeviceWatcher>,
                      args: Ref<'_, MidiLegacyPortDeviceInformationAddedEventArgs>| {
                    if let Some(args) = args.as_ref() {
                        let handled =
                            catch_unwind(AssertUnwindSafe(|| handle_added(args, &shared)));
                        if handled.is_err() {
                            error!("Handling a MIDI port being added panicked");
                        }
                    }
                    Ok(())
                },
            );
            match watcher.Added(&handler) {
                Ok(token) => token,
                Err(e) => {
                    warn!("Could not listen for MIDI ports being added: {e:?}");
                    return Err(InitError);
                }
            }
        };

        let removed = {
            let shared = shared.clone();
            let handler = RemovedEvent::new(
                move |_: Ref<'_, MidiLegacyPortDeviceWatcher>,
                      args: Ref<'_, MidiLegacyPortDeviceInformationRemovedEventArgs>| {
                    if let Some(args) = args.as_ref() {
                        let handled =
                            catch_unwind(AssertUnwindSafe(|| handle_removed(args, &shared)));
                        if handled.is_err() {
                            error!("Handling a MIDI port being removed panicked");
                        }
                    }
                    Ok(())
                },
            );
            match watcher.Removed(&handler) {
                Ok(token) => token,
                Err(e) => {
                    warn!("Could not listen for MIDI ports being removed: {e:?}");
                    let _ = watcher.RemoveAdded(added);
                    return Err(InitError);
                }
            }
        };

        Ok(PortWatcher {
            watcher: Some(watcher),
            added: Some(added),
            removed: Some(removed),
            shared,
        })
    }
}

impl<T: 'static> PortWatcher<T> {
    /// Stops the watcher and returns the user data.
    pub fn stop(mut self) -> T {
        self.shutdown()
            .expect("port watcher user data is only taken by stop")
    }

    fn shutdown(&mut self) -> Option<T> {
        // Wait for a callback that is running right now, and make sure that no handler which is
        // already on its way can call the user callback after this point.
        let data = {
            let mut guard = lock(&self.shared);
            guard.active = false;
            guard.data.take()
        };

        // The watcher is shared with `ports()` and the other `PortWatcher`s: only remove this
        // watcher's handlers, don't stop it.
        if let Some(watcher) = self.watcher.take() {
            if let Some(token) = self.added.take() {
                if let Err(e) = watcher.RemoveAdded(token) {
                    warn!("Removing a MIDI port added handler failed: {e:?}");
                }
            }
            if let Some(token) = self.removed.take() {
                if let Err(e) = watcher.RemoveRemoved(token) {
                    warn!("Removing a MIDI port removed handler failed: {e:?}");
                }
            }
        }

        data
    }
}

impl<T: 'static> Drop for PortWatcher<T> {
    fn drop(&mut self) {
        self.shutdown();
    }
}
