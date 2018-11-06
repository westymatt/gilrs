#[cfg(target_os = "linux")]
extern crate libc;
#[cfg(target_os = "linux")]
extern crate libudev_sys;
#[cfg(target_os = "linux")]
#[macro_use]
extern crate nix;
#[cfg(target_os = "linux")]
extern crate vec_map;

#[cfg(target_os = "macos")]
extern crate core_foundation;
#[cfg(target_os = "macos")]
extern crate io_kit_sys;
#[cfg(target_os = "macos")]
extern crate vec_map;

#[cfg(target_os = "windows")]
extern crate winapi;

extern crate uuid;
#[macro_use]
extern crate log;

use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::mem;

use std::error;
use std::time::Duration;
use std::time::SystemTime;

mod platform;
mod utils;

/// True, if Y axis of sticks commonly points downwards.
pub const IS_Y_AXIS_REVERSED: bool = platform::IS_Y_AXIS_REVERSED;

/// Allow control of gamepad's force feedback.
#[derive(Debug)]
pub struct FfDevice {
    inner: platform::FfDevice,
}

impl FfDevice {
    /// Sets magnitude for strong and weak ff motors.
    pub fn set_ff_state(&mut self, strong: u16, weak: u16, min_duration: Duration) {
        self.inner.set_ff_state(strong, weak, min_duration)
    }
}

/// Holds information about gamepad event.
#[derive(Copy, Clone, PartialEq, Debug)]
pub struct Event {
    /// Id of gamepad.
    pub id: usize,
    /// Event's data.
    pub event: EventType,
    /// Time when event was emitted.
    pub time: SystemTime,
}

impl Event {
    /// Creates new event with current time.
    pub fn new(id: usize, event: EventType) -> Self {
        Event {
            id,
            event,
            time: SystemTime::now(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
/// Gamepad event.
pub enum EventType {
    ButtonPressed(EvCode),
    ButtonReleased(EvCode),
    AxisValueChanged(i32, EvCode),
    Connected,
    Disconnected,
}

/// Holds information about expected axis range and deadzone.
#[derive(Copy, Clone, Debug)]
pub struct AxisInfo {
    pub min: i32,
    pub max: i32,
    pub deadzone: u32,
}

impl AxisInfo {
    pub fn deadzone(&self) -> f32 {
        let range = self.max as f32 - self.min as f32;

        if range == 0.0 {
            0.0
        } else {
            self.deadzone as f32 / range * 2.0
        }
    }
}

/// State of device's power supply.
///
/// Battery level is reported as integer between 0 and 100.
///
/// ## Example
///
/// ```
/// use gilrs_core::PowerInfo;
/// # let gilrs = gilrs_core::Gilrs::new().unwrap();
///
/// match gilrs.gamepad(0).map(|g| g.power_info()) {
///     Some(PowerInfo::Discharging(lvl)) if lvl <= 10 => println!("Low battery level, you should \
///                                                           plug your gamepad"),
///     _ => (),
/// };
/// ```
#[derive(Debug, Copy, Clone, PartialEq)]
pub enum PowerInfo {
    /// Failed to determine power status.
    Unknown,
    /// Device doesn't have battery.
    Wired,
    /// Device is running on the battery.
    Discharging(u8),
    /// Battery is charging.
    Charging(u8),
    /// Battery is charged.
    Charged,
}

/// Struct used to manage gamepads and retrieve events.
#[derive(Debug)]
pub struct Gilrs {
    inner: platform::Gilrs,
}

impl Gilrs {
    pub fn new() -> Result<Self, Error> {
        let inner = platform::Gilrs::new().map_err(|e| match e {
            PlatformError::NotImplemented(inner) => Error::NotImplemented(Gilrs { inner }),
            PlatformError::Other(e) => Error::Other(e),
        })?;

        Ok(Gilrs { inner })
    }

    /// Returns oldest event or `None` if all events were processed.
    pub fn next_event(&mut self) -> Option<Event> {
        self.inner.next_event()
    }

    /// Borrows `Gamrpad` or return `None` if index is invalid. Returned gamepad may be disconnected.
    pub fn gamepad(&self, id: usize) -> Option<&Gamepad> {
        unsafe {
            let gp: Option<&platform::Gamepad> = self.inner.gamepad(id);

            gp.map(|gp| mem::transmute(gp))
        }
    }

    /// Returns id greater than id of last connected gamepad. The returned value is only hint
    /// and may be much larger than number of observed gamepads. For example, it may return maximum
    /// number of connected gamepads on platforms when this limit is small.
    ///
    /// `gamepad(id)` should return `Some` if using id that is smaller than value returned from this
    /// function.
    pub fn last_gamepad_hint(&self) -> usize {
        self.inner.last_gamepad_hint()
    }
}

/// Provides information about gamepad.
#[derive(Debug)]
pub struct Gamepad {
    inner: platform::Gamepad,
    // Do not add any fields to this struct, it must have same memory layout as platform::Gamepad.
}

impl Gamepad {
    /// Returns name of gamepad.
    pub fn name(&self) -> &str {
        self.inner.name()
    }

    /// Returns true if gamepad is connected.
    pub fn is_connected(&self) -> bool {
        self.inner.is_connected()
    }

    /// Returns UUID that represents gamepad model.
    ///
    /// Returned UUID should be the same as SLD2 uses. If platform does not provide any method to
    /// distinguish between gamepad models, nil UUID is returned.
    ///
    /// It is recommended to process with the [UUID crate](https://crates.io/crates/uuid).
    /// Use `Uuid::from_bytes` method to create a `Uuid` from the returned bytes.
    pub fn uuid(&self) -> [u8; 16] {
        self.inner.uuid().as_bytes().clone()
    }

    /// Returns device's power supply state.
    pub fn power_info(&self) -> PowerInfo {
        self.inner.power_info()
    }

    /// Returns true if force feedback is supported by device,
    pub fn is_ff_supported(&self) -> bool {
        self.inner.is_ff_supported()
    }

    /// Creates `FfDevice` corresponding to this gamepad.
    pub fn ff_device(&self) -> Option<FfDevice> {
        self.inner.ff_device().map(|inner| FfDevice { inner })
    }

    /// Returns slice with EvCodes that may appear in button related events.
    pub fn buttons(&self) -> &[EvCode] {
        unsafe {
            let bt: &[platform::EvCode] = self.inner.buttons();

            mem::transmute(bt)
        }
    }

    /// Returns slice with EvCodes that may appear in axis related events.
    pub fn axes(&self) -> &[EvCode] {
        unsafe {
            let ax: &[platform::EvCode] = self.inner.axes();

            mem::transmute(ax)
        }
    }

    /// Returns information about specific axis or `None` if device doesn't have axis with provided
    /// `EvCode`.
    pub fn axis_info(&self, nec: EvCode) -> Option<&AxisInfo> {
        self.inner.axis_info(nec.0)
    }
}

/// Platform specific representation of axis or button.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct EvCode(platform::EvCode);

impl EvCode {
    pub fn into_u32(self) -> u32 {
        self.0.into_u32()
    }
}

impl Display for EvCode {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Error type which can be returned when creating `Gilrs`.
#[derive(Debug)]
enum PlatformError {
    /// Gilrs does not support current platform, but you can use dummy context from this error if
    /// gamepad input is not essential.
    #[allow(dead_code)]
    NotImplemented(platform::Gilrs),
    /// Platform specific error.
    #[allow(dead_code)]
    Other(Box<error::Error + Send + Sync>),
}

impl Display for PlatformError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            &PlatformError::NotImplemented(_) => {
                f.write_str("Gilrs does not support current platform.")
            }
            &PlatformError::Other(ref e) => e.fmt(f),
        }
    }
}

impl error::Error for PlatformError {
    fn description(&self) -> &str {
        match self {
            &PlatformError::NotImplemented(_) => "platform not supported",
            &PlatformError::Other(_) => "platform specific error",
        }
    }

    fn cause(&self) -> Option<&error::Error> {
        match self {
            &PlatformError::NotImplemented(_) => None,
            &PlatformError::Other(ref e) => Some(&**e),
        }
    }
}

/// Error type which can be returned when creating `Gilrs`.
#[derive(Debug)]
pub enum Error {
    /// Gilrs does not support current platform, but you can use dummy context from this error if
    /// gamepad input is not essential.
    #[allow(dead_code)]
    NotImplemented(Gilrs),
    /// Platform specific error.
    #[allow(dead_code)]
    Other(Box<error::Error + Send + Sync>),
}

impl Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            &Error::NotImplemented(_) => f.write_str("Gilrs does not support current platform."),
            &Error::Other(ref e) => e.fmt(f),
        }
    }
}

impl error::Error for Error {
    fn description(&self) -> &str {
        match self {
            &Error::NotImplemented(_) => "platform not supported",
            &Error::Other(_) => "platform specific error",
        }
    }

    fn cause(&self) -> Option<&error::Error> {
        match self {
            &Error::NotImplemented(_) => None,
            &Error::Other(ref e) => Some(&**e),
        }
    }
}

/// Provides the most common mappings of physical location of gamepad elements to their EvCodes.
/// Some (or most) gamepads may use different mappings.
pub mod native_ev_codes {
    use super::EvCode;
    use platform::native_ev_codes as nec;

    pub const AXIS_LSTICKX: EvCode = EvCode(nec::AXIS_LSTICKX);
    pub const AXIS_LSTICKY: EvCode = EvCode(nec::AXIS_LSTICKY);
    pub const AXIS_LEFTZ: EvCode = EvCode(nec::AXIS_LEFTZ);
    pub const AXIS_RSTICKX: EvCode = EvCode(nec::AXIS_RSTICKX);
    pub const AXIS_RSTICKY: EvCode = EvCode(nec::AXIS_RSTICKY);
    pub const AXIS_RIGHTZ: EvCode = EvCode(nec::AXIS_RIGHTZ);
    pub const AXIS_DPADX: EvCode = EvCode(nec::AXIS_DPADX);
    pub const AXIS_DPADY: EvCode = EvCode(nec::AXIS_DPADY);
    pub const AXIS_RT: EvCode = EvCode(nec::AXIS_RT);
    pub const AXIS_LT: EvCode = EvCode(nec::AXIS_LT);
    pub const AXIS_RT2: EvCode = EvCode(nec::AXIS_RT2);
    pub const AXIS_LT2: EvCode = EvCode(nec::AXIS_LT2);

    pub const BTN_SOUTH: EvCode = EvCode(nec::BTN_SOUTH);
    pub const BTN_EAST: EvCode = EvCode(nec::BTN_EAST);
    pub const BTN_C: EvCode = EvCode(nec::BTN_C);
    pub const BTN_NORTH: EvCode = EvCode(nec::BTN_NORTH);
    pub const BTN_WEST: EvCode = EvCode(nec::BTN_WEST);
    pub const BTN_Z: EvCode = EvCode(nec::BTN_Z);
    pub const BTN_LT: EvCode = EvCode(nec::BTN_LT);
    pub const BTN_RT: EvCode = EvCode(nec::BTN_RT);
    pub const BTN_LT2: EvCode = EvCode(nec::BTN_LT2);
    pub const BTN_RT2: EvCode = EvCode(nec::BTN_RT2);
    pub const BTN_SELECT: EvCode = EvCode(nec::BTN_SELECT);
    pub const BTN_START: EvCode = EvCode(nec::BTN_START);
    pub const BTN_MODE: EvCode = EvCode(nec::BTN_MODE);
    pub const BTN_LTHUMB: EvCode = EvCode(nec::BTN_LTHUMB);
    pub const BTN_RTHUMB: EvCode = EvCode(nec::BTN_RTHUMB);

    pub const BTN_DPAD_UP: EvCode = EvCode(nec::BTN_DPAD_UP);
    pub const BTN_DPAD_DOWN: EvCode = EvCode(nec::BTN_DPAD_DOWN);
    pub const BTN_DPAD_LEFT: EvCode = EvCode(nec::BTN_DPAD_LEFT);
    pub const BTN_DPAD_RIGHT: EvCode = EvCode(nec::BTN_DPAD_RIGHT);
}
