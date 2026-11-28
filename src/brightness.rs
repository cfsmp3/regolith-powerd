//! `org.gnome.Shell.Brightness` and automatic screen brightness for the
//! Wayland session.
//!
//! GNOME Shell (and gnome-flashback in the X11 session) serves this
//! interface. Settings › Power shows Dim Screen only while it says a
//! backlight can be controlled, and Automatic Screen Brightness only when the
//! ambient-light sensor service (iio-sensor-proxy, `net.hadess.SensorProxy`)
//! also reports a light sensor. In the sway session nothing served it, and
//! nothing followed the light sensor either.
//!
//! This module serves the interface and keeps one state for the screen
//! backlight, shared by:
//!
//! - dimming (`SetDimming`, which swayidle's idle dim calls too): the
//!   brightness is capped at `idle-brightness` and comes back afterwards;
//! - targets set by other programs (`SetAutoBrightnessTarget`), as GNOME
//!   Shell applies them, while this daemon's own automatic brightness is off;
//! - automatic brightness: while `ambient-enabled` is on, a backlight and a
//!   light sensor are present, the screen isn't dimmed and an output is on,
//!   the light sensor is claimed and the brightness follows the light level
//!   proportionally, from a reference pair (brightness, light level) taken
//!   when the period starts and whenever someone else changes the brightness.
//!
//! The backlight is read and written through `light`, as the idle dim and the
//! brightness keys of the session do. A brightness that differs from what
//! this daemon last set counts as a change made by someone else.

use crate::PowerSettings;
use glib::g_log;
use std::cell::Cell;
use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use zbus::blocking::{Connection, ConnectionBuilder};
use zbus::fdo::RequestNameFlags;
use zbus::zvariant::OwnedValue;
use zbus::{dbus_interface, fdo};

const NAME: &str = "org.gnome.Shell.Brightness";
const PATH: &str = "/org/gnome/Shell/Brightness";

const SENSOR_NAME: &str = "net.hadess.SensorProxy";
const SENSOR_PATH: &str = "/net/hadess/SensorProxy";
const SENSOR_IFACE: &str = "net.hadess.SensorProxy";

const BACKLIGHT_CLASS: &str = "/sys/class/backlight";

/// How often the backlight, the outputs and the settings are looked at.
const POLL: Duration = Duration::from_secs(1);
/// How often a failed connection to the system bus is retried.
const SYSTEM_RETRY: Duration = Duration::from_secs(5);
/// A brightness further than this (percentage points) from the one this
/// daemon last set was changed by someone else.
const EXTERNAL_CHANGE: f64 = 2.0;
/// Light-level changes within this fraction of the level the brightness was
/// last computed for are ignored.
const DEAD_BAND: f64 = 0.10;
/// Changes of at least this many points are made in [`RAMP_STEPS`] steps,
/// [`RAMP_STEP`] apart; smaller ones in one.
const RAMP_MIN: f64 = 3.0;
const RAMP_STEPS: u32 = 10;
const RAMP_STEP: Duration = Duration::from_millis(200);

/// The swayidle commands of the idle dim: going idle dims, activity undims.
pub fn idle_dim_command(enable: bool) -> String {
    format!(
        "gdbus call --session --dest {NAME} --object-path {PATH} \
         --method {NAME}.SetDimming {enable} >/dev/null"
    )
}

/// The automatic brightness, in percent, for light level `level` when the
/// reference pair is brightness `b0` at light level `l0`.
fn auto_target(b0: f64, l0: f64, level: f64) -> f64 {
    (b0 * level / l0).clamp(1.0, 100.0)
}

/// Whether `level` is far enough from `basis`, the light level the
/// brightness was last computed for, to change the brightness.
fn beyond_dead_band(basis: f64, level: f64) -> bool {
    (level - basis).abs() > DEAD_BAND * basis
}

/// The backlight level for an external target in [0, 1].
fn target_level(target: f64) -> f64 {
    (100.0 * target).round().max(1.0)
}

/// The values written, in order, to go from `from` to `to`.
fn ramp_values(from: f64, to: f64) -> Vec<f64> {
    let steps = if (to - from).abs() >= RAMP_MIN {
        RAMP_STEPS
    } else {
        1
    };
    (1..=steps)
        .map(|i| from + (to - from) * f64::from(i) / f64::from(steps))
        .collect()
}

fn backlight_present() -> bool {
    std::fs::read_dir(BACKLIGHT_CLASS)
        .map(|mut entries| entries.any(|e| e.is_ok()))
        .unwrap_or(false)
}

/// The brightness in percent, as `light` reports it.
fn read_brightness() -> Option<f64> {
    let output = Command::new("light")
        .arg("-G")
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

fn write_brightness(value: f64) {
    let value = value.clamp(1.0, 100.0);
    match Command::new("light")
        .arg("-S")
        .arg(format!("{value:.2}"))
        .stdout(Stdio::null())
        .status()
    {
        Ok(status) if status.success() => {}
        Ok(status) => g_log!(glib::LogLevel::Warning, "light -S {value:.2}: {status}"),
        Err(e) => g_log!(glib::LogLevel::Warning, "Cannot run light: {e}"),
    }
}

/// What a D-Bus caller asked for.
enum Request {
    SetDimming(bool),
    AutoTarget(f64),
}

enum Event {
    Request(Request, async_channel::Sender<()>),
    /// The unique name now owning the sensor service's name, if any.
    SensorOwner(Option<String>),
    /// Changed sensor properties, with the sender that announced them.
    SensorProperties(String, HashMap<String, OwnedValue>),
}

struct Brightness {
    has_control: Arc<AtomicBool>,
    events: Mutex<mpsc::Sender<Event>>,
}

impl Brightness {
    async fn request(&self, request: Request) {
        let (done, wait) = async_channel::bounded(1);
        let sent = self
            .events
            .lock()
            .unwrap()
            .send(Event::Request(request, done));
        if sent.is_ok() {
            // An error means the state is gone; there is nothing to wait for.
            let _ = wait.recv().await;
        }
    }
}

#[dbus_interface(name = "org.gnome.Shell.Brightness")]
impl Brightness {
    async fn set_dimming(&self, enable: bool) {
        self.request(Request::SetDimming(enable)).await
    }

    async fn set_auto_brightness_target(&self, target: f64) -> fdo::Result<()> {
        if target.is_nan() || target > 1.0 {
            return Err(fdo::Error::InvalidArgs(format!(
                "Invalid brightness target {target}"
            )));
        }
        self.request(Request::AutoTarget(target)).await;
        Ok(())
    }

    #[dbus_interface(property)]
    fn has_brightness_control(&self) -> bool {
        self.has_control.load(Ordering::SeqCst)
    }
}

/// An automatic-brightness period: the reference pair and the light level
/// the brightness was last computed for.
struct Period {
    b0: f64,
    l0: Option<f64>,
    basis: Option<f64>,
}

/// A gradual change in progress.
struct Ramp {
    values: Vec<f64>,
    next: usize,
    due: Instant,
}

/// The run conditions of automatic brightness, to retry a failed claim only
/// once they change.
#[derive(Clone, PartialEq)]
struct Conditions {
    enabled: bool,
    has_control: bool,
    /// The unique name of the sensor service, if it is on the bus.
    sensor: Option<String>,
    has_light: bool,
    dimmed: bool,
    outputs_on: bool,
}

impl Conditions {
    fn met(&self) -> bool {
        self.enabled
            && self.has_control
            && self.sensor.is_some()
            && self.has_light
            && !self.dimmed
            && self.outputs_on
    }
}

struct State {
    session: Connection,
    has_control_flag: Arc<AtomicBool>,
    settings: PowerSettings,
    system: Option<Connection>,
    system_retry: Instant,
    events: mpsc::Sender<Event>,
    sway: Option<swayipc::Connection>,

    has_control: bool,
    outputs_on: bool,
    sensor_owner: Option<String>,
    has_light: bool,
    /// The last light reading above 0.
    level: Option<f64>,

    /// The brightness this daemon last set, or found when it last looked.
    known: Option<f64>,
    /// The brightness to come back to after dimming or an external target.
    natural: f64,
    dimmed: bool,
    external: Option<f64>,

    /// Whether the light sensor is claimed: automatic brightness runs.
    claimed: bool,
    /// The conditions under which the last claim failed.
    failed_claim: Option<Conditions>,
    period: Option<Period>,
    ramp: Option<Ramp>,
}

impl State {
    fn conditions(&self) -> Conditions {
        Conditions {
            enabled: self.settings.ambient_enabled(),
            has_control: self.has_control,
            sensor: self.sensor_owner.clone(),
            has_light: self.has_light,
            dimmed: self.dimmed,
            outputs_on: self.outputs_on,
        }
    }

    fn sensor_call(&self, method: &str) -> zbus::Result<()> {
        let system = self
            .system
            .as_ref()
            .ok_or_else(|| zbus::Error::Failure("no system bus".into()))?;
        system.call_method(
            Some(SENSOR_NAME),
            SENSOR_PATH,
            Some(SENSOR_IFACE),
            method,
            &(),
        )?;
        Ok(())
    }

    fn sensor_property(&self, name: &str) -> Option<OwnedValue> {
        let system = self.system.as_ref()?;
        let reply = system
            .call_method(
                Some(SENSOR_NAME),
                SENSOR_PATH,
                Some("org.freedesktop.DBus.Properties"),
                "Get",
                &(SENSOR_IFACE, name),
            )
            .ok()?;
        let value: zbus::zvariant::Value = reply.body().ok()?;
        Some(OwnedValue::from(value))
    }

    fn update_has_light(&mut self) {
        self.has_light = self
            .sensor_property("HasAmbientLight")
            .and_then(|v| bool::try_from(v).ok())
            .unwrap_or(false);
    }

    fn connect_system(&mut self) {
        if self.system.is_some() || Instant::now() < self.system_retry {
            return;
        }
        self.system_retry = Instant::now() + SYSTEM_RETRY;
        match Connection::system() {
            Ok(system) => match watch_sensor(&system, self.events.clone()) {
                Ok(unique) => {
                    self.system = Some(system);
                    self.sensor_owner_changed(unique);
                }
                Err(e) => g_log!(
                    glib::LogLevel::Warning,
                    "Cannot follow the light sensor: {e}"
                ),
            },
            Err(e) => g_log!(
                glib::LogLevel::Info,
                "No system bus for the light sensor: {e}"
            ),
        }
    }

    fn poll_outputs(&mut self) {
        if self.sway.is_none() {
            self.sway = swayipc::Connection::new().ok();
        }
        let outputs = self.sway.as_mut().map(|sway| sway.get_outputs());
        self.outputs_on = match outputs {
            Some(Ok(outputs)) => outputs.iter().any(|o| o.active && o.dpms),
            Some(Err(_)) => {
                self.sway = None;
                false
            }
            None => false,
        };
    }

    fn set_has_control(&mut self, has_control: bool) {
        if has_control == self.has_control {
            return;
        }
        self.has_control = has_control;
        self.has_control_flag.store(has_control, Ordering::SeqCst);
        self.dimmed = false;
        self.external = None;
        self.ramp = None;
        self.known = if has_control { read_brightness() } else { None };
        if let Some(known) = self.known {
            self.natural = known;
        }
        let announced = self
            .session
            .object_server()
            .interface::<_, Brightness>(PATH)
            .and_then(|iface| {
                zbus::block_on(
                    iface
                        .get()
                        .has_brightness_control_changed(iface.signal_context()),
                )
            });
        if let Err(e) = announced {
            g_log!(
                glib::LogLevel::Warning,
                "Cannot announce HasBrightnessControl: {e}"
            );
        }
    }

    /// Whether everything automatic brightness needs is there, apart from a
    /// powered output.
    fn auto_possible(&self) -> bool {
        self.settings.ambient_enabled()
            && self.has_control
            && self.sensor_owner.is_some()
            && self.has_light
            && !self.dimmed
    }

    /// Whether a change made by someone else matters now: it ends dimming
    /// and an external target, stops a ramp and re-anchors automatic
    /// brightness. Otherwise the brightness is only read once one of them
    /// starts, so that `light` doesn't run every second for nothing.
    fn tracking(&self) -> bool {
        self.dimmed || self.external.is_some() || self.claimed || self.ramp.is_some()
    }

    fn poll(&mut self) {
        self.connect_system();
        self.set_has_control(backlight_present());
        if self.auto_possible() {
            self.poll_outputs();
        } else {
            // Looked at again before automatic brightness can start.
            self.outputs_on = false;
        }
        if self.tracking() {
            self.check_external_change();
        }
    }

    /// Notice a brightness change made by someone else, and adopt it: it
    /// ends dimming and an external target, and becomes the new reference.
    /// Returns the current brightness.
    fn check_external_change(&mut self) -> Option<f64> {
        if !self.has_control {
            return None;
        }
        let current = read_brightness()?;
        let Some(known) = self.known else {
            self.known = Some(current);
            return Some(current);
        };
        if (current - known).abs() <= EXTERNAL_CHANGE {
            return Some(current);
        }
        self.known = Some(current);
        self.natural = current;
        self.dimmed = false;
        self.external = None;
        self.ramp = None;
        if let Some(period) = self.period.as_mut() {
            period.b0 = current;
            period.l0 = self.level;
            period.basis = self.level;
        }
        Some(current)
    }

    fn current(&mut self) -> Option<f64> {
        self.check_external_change().or(self.known)
    }

    fn set_now(&mut self, value: f64) {
        self.ramp = None;
        write_brightness(value);
        self.known = read_brightness().or(Some(value));
    }

    fn start_ramp(&mut self, to: f64) {
        let Some(from) = self.known else { return };
        if (to - from).abs() < 0.05 {
            self.ramp = None;
            return;
        }
        self.ramp = Some(Ramp {
            values: ramp_values(from, to),
            next: 0,
            due: Instant::now() + RAMP_STEP,
        });
    }

    fn ramp_step(&mut self) {
        let due = matches!(&self.ramp, Some(r) if Instant::now() >= r.due);
        if !due {
            return;
        }
        // A change by someone else stops the ramp.
        self.check_external_change();
        let Some(ramp) = self.ramp.as_mut() else {
            return;
        };
        let value = ramp.values[ramp.next];
        ramp.next += 1;
        ramp.due = Instant::now() + RAMP_STEP;
        let last = ramp.next == ramp.values.len();
        write_brightness(value);
        if last {
            self.ramp = None;
            self.known = read_brightness().or(Some(value));
        } else {
            self.known = Some(value);
        }
        if self.external.is_none() && !self.dimmed {
            self.natural = self.known.unwrap_or(value);
        }
    }

    /// The brightness dimming and an external target leave.
    fn wanted(&self) -> f64 {
        let base = self.external.unwrap_or(self.natural);
        if self.dimmed {
            base.min(f64::from(self.settings.idle_brightness()))
        } else {
            base
        }
    }

    fn set_dimming(&mut self, enable: bool) {
        // Without a backlight there is nothing to read, and nothing to do.
        let Some(current) = self.current() else {
            return;
        };
        if enable == self.dimmed {
            return;
        }
        if enable {
            if self.external.is_none() {
                self.natural = current;
            }
            self.dimmed = true;
            // Stop automatic brightness first, so that the dim is the last
            // write.
            self.sync_auto();
            self.ramp = None;
            let wanted = self.wanted();
            if wanted < current - 0.05 {
                self.set_now(wanted);
            }
        } else {
            self.dimmed = false;
            let wanted = self.wanted();
            if (wanted - current).abs() >= 0.05 {
                self.set_now(wanted);
            }
        }
    }

    fn external_target(&mut self, target: f64) {
        let Some(current) = self.current() else {
            return;
        };
        if self.claimed {
            return;
        }
        if target < 0.0 {
            if self.external.take().is_none() {
                return;
            }
        } else {
            if self.external.is_none() && !self.dimmed {
                self.natural = current;
            }
            self.external = Some(target_level(target));
        }
        let wanted = self.wanted();
        self.start_ramp(wanted);
    }

    fn sensor_owner_changed(&mut self, unique: Option<String>) {
        if unique == self.sensor_owner {
            return;
        }
        // A claim belongs to the connection that took it: it is gone with
        // the previous owner.
        self.sensor_owner = unique;
        self.end_period();
        self.level = None;
        if self.sensor_owner.is_some() {
            self.update_has_light();
        } else {
            self.has_light = false;
        }
    }

    fn sensor_properties(&mut self, sender: &str, changed: HashMap<String, OwnedValue>) {
        if self.sensor_owner.as_deref() != Some(sender) {
            return;
        }
        if let Some(has) = changed
            .get("HasAmbientLight")
            .and_then(|v| bool::try_from(v.clone()).ok())
        {
            self.has_light = has;
            self.sync_auto();
        }
        if let Some(level) = changed
            .get("LightLevel")
            .and_then(|v| f64::try_from(v.clone()).ok())
        {
            self.light_reading(level);
        }
    }

    fn light_reading(&mut self, level: f64) {
        // NaN and infinities are not readings either.
        if !level.is_finite() || level <= 0.0 {
            return;
        }
        self.level = Some(level);
        if !self.claimed {
            return;
        }
        // A change by someone else since the last look sets the reference
        // pair before this reading is used.
        self.check_external_change();
        let Some(period) = self.period.as_mut() else {
            return;
        };
        let (Some(l0), Some(basis)) = (period.l0, period.basis) else {
            period.l0 = Some(level);
            period.basis = Some(level);
            return;
        };
        if !beyond_dead_band(basis, level) {
            return;
        }
        period.basis = Some(level);
        let target = auto_target(period.b0, l0, level);
        self.start_ramp(target);
    }

    /// Forget the current period without releasing the claim (the sensor
    /// service went away or restarted).
    fn end_period(&mut self) {
        self.claimed = false;
        self.period = None;
        if self.external.is_none() {
            self.ramp = None;
        }
        self.failed_claim = None;
    }

    /// Start or stop automatic brightness to match its conditions.
    fn sync_auto(&mut self) {
        let conditions = self.conditions();
        let met = conditions.met();
        if met && !self.claimed {
            if self.failed_claim.as_ref() == Some(&conditions) {
                return;
            }
            if let Err(e) = self.sensor_call("ClaimLight") {
                g_log!(
                    glib::LogLevel::Warning,
                    "Cannot claim the light sensor: {e}"
                );
                self.failed_claim = Some(conditions);
                return;
            }
            self.failed_claim = None;
            self.claimed = true;
            // An external target in force ends here; the brightness stays.
            self.external = None;
            self.ramp = None;
            let b0 = self.current().unwrap_or(self.natural);
            self.natural = b0;
            self.period = Some(Period {
                b0,
                l0: None,
                basis: None,
            });
            if let Some(level) = self
                .sensor_property("LightLevel")
                .and_then(|v| f64::try_from(v).ok())
            {
                self.light_reading(level);
            }
        } else if !met && self.claimed {
            if self.sensor_owner.is_some() {
                if let Err(e) = self.sensor_call("ReleaseLight") {
                    g_log!(
                        glib::LogLevel::Warning,
                        "Cannot release the light sensor: {e}"
                    );
                }
            }
            self.end_period();
            // The brightness stays where it is.
            if let Some(known) = self.known {
                if !self.dimmed {
                    self.natural = known;
                }
            }
        } else if !met {
            self.failed_claim = None;
        }
    }

    fn handle(&mut self, event: Event) {
        match event {
            Event::Request(request, done) => {
                match request {
                    Request::SetDimming(enable) => self.set_dimming(enable),
                    Request::AutoTarget(target) => self.external_target(target),
                }
                let _ = done.try_send(());
            }
            Event::SensorOwner(unique) => self.sensor_owner_changed(unique),
            Event::SensorProperties(sender, changed) => self.sensor_properties(&sender, changed),
        }
    }

    fn run(mut self, events: mpsc::Receiver<Event>, settings_changed: Rc<Cell<bool>>) {
        let context = glib::MainContext::ref_thread_default();
        let mut next_poll = Instant::now();
        loop {
            let now = Instant::now();
            if now >= next_poll {
                self.poll();
                next_poll = now + POLL;
            }
            self.ramp_step();
            self.sync_auto();
            let wake = match &self.ramp {
                Some(ramp) => ramp.due.min(next_poll),
                None => next_poll,
            };
            let timeout = wake.saturating_duration_since(Instant::now());
            match events.recv_timeout(timeout) {
                Ok(event) => {
                    self.handle(event);
                    while let Ok(event) = events.try_recv() {
                        self.handle(event);
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
            // Settings change notifications are delivered here.
            while context.iteration(false) {}
            if settings_changed.replace(false) {
                self.sync_auto();
            }
        }
    }
}

/// Follow the sensor service on `system`: its owner and its property
/// changes go to `events`. Returns the current owner.
fn watch_sensor(system: &Connection, events: mpsc::Sender<Event>) -> zbus::Result<Option<String>> {
    let dbus = zbus::blocking::fdo::DBusProxy::new(system)?;
    let owners = dbus.receive_name_owner_changed_with_args(&[(0, SENSOR_NAME)])?;
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::MessageType::Signal)
        .interface("org.freedesktop.DBus.Properties")?
        .member("PropertiesChanged")?
        .path(SENSOR_PATH)?
        .build();
    let changes = zbus::blocking::MessageIterator::for_match_rule(rule, system, None)?;
    let owner_events = events.clone();
    thread::Builder::new()
        .name("sensor-owner".into())
        .spawn(move || {
            for signal in owners {
                let Ok(args) = signal.args() else { continue };
                let unique = args.new_owner().as_ref().map(|n| n.to_string());
                if owner_events.send(Event::SensorOwner(unique)).is_err() {
                    return;
                }
            }
        })
        .map_err(|e| zbus::Error::Failure(e.to_string()))?;
    thread::Builder::new()
        .name("sensor-properties".into())
        .spawn(move || {
            for message in changes {
                let Ok(message) = message else { continue };
                let Ok(header) = message.header() else {
                    continue;
                };
                let Ok(Some(sender)) = header.sender() else {
                    continue;
                };
                let sender = sender.to_string();
                let Ok((iface, changed, _)) =
                    message.body::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
                else {
                    continue;
                };
                if iface != SENSOR_IFACE {
                    continue;
                }
                if events
                    .send(Event::SensorProperties(sender, changed))
                    .is_err()
                {
                    return;
                }
            }
        })
        .map_err(|e| zbus::Error::Failure(e.to_string()))?;
    let unique = dbus
        .get_name_owner(SENSOR_NAME.try_into()?)
        .ok()
        .map(|n| n.to_string());
    Ok(unique)
}

fn serve() -> zbus::Result<()> {
    let (events, receiver) = mpsc::channel();
    let has_control = backlight_present();
    let has_control_flag = Arc::new(AtomicBool::new(has_control));
    let session = ConnectionBuilder::session()?
        .serve_at(
            PATH,
            Brightness {
                has_control: has_control_flag.clone(),
                events: Mutex::new(events.clone()),
            },
        )?
        .build()?;
    // Nothing else owns this name in the sway session. Should something that
    // does turn up, it takes over, as with org.gnome.Shell itself.
    session.request_name_with_flags(NAME, RequestNameFlags::AllowReplacement.into())?;

    // Settings change notifications go to this thread's own main context.
    let context = glib::MainContext::new();
    context
        .with_thread_default(|| {
            let settings = PowerSettings::new();
            let settings_changed = Rc::new(Cell::new(false));
            let flag = settings_changed.clone();
            settings.connect_ambient_enabled_changed(move |_| flag.set(true));
            let known = if has_control { read_brightness() } else { None };
            let state = State {
                session,
                has_control_flag,
                settings,
                system: None,
                system_retry: Instant::now(),
                events,
                sway: None,
                has_control,
                outputs_on: false,
                sensor_owner: None,
                has_light: false,
                level: None,
                known,
                natural: known.unwrap_or(100.0),
                dimmed: false,
                external: None,
                claimed: false,
                failed_claim: None,
                period: None,
                ramp: None,
            };
            state.run(receiver, settings_changed);
        })
        .map_err(|e| zbus::Error::Failure(e.to_string()))
}

/// Serve `org.gnome.Shell.Brightness` on the session bus for the lifetime of
/// the daemon, and run automatic brightness, from a thread of its own.
pub fn start() {
    thread::spawn(|| {
        if let Err(e) = serve() {
            g_log!(
                glib::LogLevel::Warning,
                "org.gnome.Shell.Brightness unavailable: {e}"
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_target_is_proportional_to_the_light_level_and_clamped() {
        assert_eq!(auto_target(40.0, 100.0, 200.0), 80.0);
        assert_eq!(auto_target(20.0, 100.0, 50.0), 10.0);
        assert_eq!(auto_target(40.0, 100.0, 1000.0), 100.0);
        assert_eq!(auto_target(40.0, 100.0, 1.0), 1.0);
    }

    #[test]
    fn light_within_ten_percent_is_ignored() {
        assert!(!beyond_dead_band(200.0, 210.0));
        assert!(!beyond_dead_band(200.0, 190.0));
        assert!(!beyond_dead_band(200.0, 220.0));
        assert!(beyond_dead_band(200.0, 221.0));
        assert!(beyond_dead_band(200.0, 179.0));
    }

    #[test]
    fn external_targets_map_to_whole_percents_of_at_least_one() {
        assert_eq!(target_level(0.7), 70.0);
        assert_eq!(target_level(0.0), 1.0);
        assert_eq!(target_level(0.004), 1.0);
        assert_eq!(target_level(0.555), 56.0);
        assert_eq!(target_level(1.0), 100.0);
    }

    #[test]
    fn large_changes_are_gradual_and_end_on_the_target() {
        let values = ramp_values(40.0, 80.0);
        assert_eq!(values.len() as u32, RAMP_STEPS);
        assert_eq!(*values.last().unwrap(), 80.0);
        assert!(values.windows(2).all(|w| w[0] < w[1]));
        let span = RAMP_STEP * (RAMP_STEPS - 1);
        assert!(span >= Duration::from_secs(1), "{span:?}");
        assert_eq!(ramp_values(40.0, 41.0), [41.0]);
    }

    #[test]
    fn the_idle_dim_commands_call_set_dimming() {
        assert!(idle_dim_command(true).ends_with("SetDimming true >/dev/null"));
        assert!(idle_dim_command(false).contains("--dest org.gnome.Shell.Brightness"));
        assert!(!idle_dim_command(false).contains('\n'));
    }
}
