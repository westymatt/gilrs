// Copyright 2016-2018 Mateusz Sieczko and other GilRs Developers
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

use ev::{Axis, AxisOrBtn, Button, Code, Event, EventType};
use ev::state::{AxisData, ButtonData, GamepadState};
use ff::Error as FfError;
use ff::server::{self, Message};
use mapping::{Mapping, MappingData, MappingDb, MappingError};
use gilrs_core::{self, Error as PlatformError};

pub use gilrs_core::{PowerInfo, Status};

use uuid::Uuid;

use std::collections::VecDeque;
use std::error;
use std::fmt::{self, Display};
use std::ops::{Index, IndexMut};
use std::sync::mpsc::Sender;
use gilrs_core::EventType as RawEventType;
use gilrs_core::Event as RawEvent;
use gilrs_core::AxisInfo;
use utils;

/// Main object responsible of managing gamepads.
///
/// `Gilrs` owns all gamepads and you can use one of two methods to get reference to specific one.
/// First, you can use `Index` operator. It will always return some gamepad, even if it was
/// disconnected or never observed. All actions on such gamepads are no-op, and state of all
/// elements should be 0. This makes it ideal when you don't care whether gamepad is connected.
///
/// The second method is to use `get()` function. Because it return `Option`, it will return `None`
/// if gamepad is not connected.
///
/// # Event loop
///
/// All interesting actions like button was pressed or new controller was connected are represented
/// by struct [`Event`](struct.Event.html). Use `next_event()` function to retrieve event from
/// queue.
///
/// ```
/// use gilrs::{Gilrs, Event, EventType, Button};
///
/// let mut gilrs = Gilrs::new().unwrap();
///
/// // Event loop
/// loop {
///     while let Some(event) = gilrs.next_event() {
///         match event {
///             Event { id, event: EventType::ButtonPressed(Button::South, _), .. } => {
///                 println!("Player {}: jump!", id + 1)
///             }
///             Event { id, event: EventType::Disconnected, .. } => {
///                 println!("We lost player {}", id + 1)
///             }
///             _ => (),
///         };
///     }
///     # break;
/// }
/// ```
///
/// # Cached gamepad state
///
/// `Gilrs` also menage cached gamepad state. Updating state is done automatically, unless it's
///  disabled by `GilrsBuilder::set_update_state(false)`. However, if you are using custom filters,
/// you still have to update state manually – to do this call `update()` method.
///
/// To access state you can use `Gamepad::state()` function. Gamepad also implement some state
/// related functions directly, see [`Gamepad`](struct.Gamepad.html) for more.
///
/// ## Counter
///
/// `Gilrs` has additional functionality, referred here as *counter*. The idea behind it is simple,
/// each time you end iteration of update loop, you call `Gilrs::inc()` which will increase
/// internal counter by one. When state of one if elements changes, value of counter is saved. When
/// checking state of one of elements you can tell exactly when this event happened. Timestamps are
/// not good solution here because they can tell you when *system* observed event, not when you
/// processed it. On the other hand, they are good when you want to implement key repeat or software
/// debouncing.
///
/// ```
/// use gilrs::{Gilrs, Button};
///
/// let mut gilrs = Gilrs::new().unwrap();
///
/// loop {
///     while let Some(ev) = gilrs.next_event() {
///         gilrs.update(&ev);
///         // Do other things with event
///     }
///
///     match gilrs.gamepad(0) {
///         Some(gamepad) if gamepad.is_pressed(Button::DpadLeft) {
///             // go left
///         }
///         _ => (),
///     }
///
///     if let Some(gamepad) = gilrs.gamepad(0) {
///         match gamepad.button_data(Button::South) {
///             Some(d) if d.is_pressed() && d.counter() == gilrs.counter() => {
///                 // jump
///             }
///             _ => ()
///         }
///     }
///
///     gilrs.inc();
/// #   break;
/// }
///
#[derive(Debug)]
pub struct Gilrs {
    inner: gilrs_core::Gilrs,
    next_id: usize,
    tx: Sender<Message>,
    counter: u64,
    mappings: MappingDb,
    default_filters: bool,
    events: VecDeque<Event>,
    axis_to_btn_pressed: f32,
    axis_to_btn_released: f32,
    update_state: bool,
    gamepads_data: Vec<GamepadData>,
}

impl Gilrs {
    /// Creates new `Gilrs` with default settings. See [`GilrsBuilder`](struct.GilrsBuilder.html)
    /// for more details.
    pub fn new() -> Result<Self, Error> {
        GilrsBuilder::new().build()
    }

    /// Returns next pending event.
    pub fn next_event(&mut self) -> Option<Event> {
        use ev::filter::{axis_dpad_to_button, deadzone, Filter, Jitter};

        let ev = if self.default_filters {
            let jitter_filter = Jitter::new();
            loop {
                let ev = self.next_event_priv()
                    .filter_ev(&axis_dpad_to_button, self)
                    .filter_ev(&jitter_filter, self)
                    .filter_ev(&deadzone, self);

                // Skip all dropped events, there is no reason to return them
                match ev {
                    Some(ev) if ev.is_dropped() => (),
                    _ => break ev,
                }
            }
        } else {
            self.next_event_priv()
        };

        if self.update_state {
            if let Some(ref ev) = ev {
                self.update(ev);
            }
        }

        ev
    }

    /// Returns next pending event.
    fn next_event_priv(&mut self) -> Option<Event> {
        if let Some(ev) = self.events.pop_front() {
            Some(ev)
        } else {
            match self.inner.next_event() {
                Some(RawEvent { id, event, time }) => {
                    trace!("Original event: {:?}", RawEvent { id, event, time });
                    let event = match event {
                        RawEventType::ButtonPressed(nec) => {
                            let nec = Code(nec);
                            match self.gamepad(id).unwrap().axis_or_btn_name(nec) {
                                Some(AxisOrBtn::Btn(b)) => {
                                    self.events.push_back(Event {
                                        id,
                                        time,
                                        event: EventType::ButtonChanged(b, 1.0, nec),
                                    });

                                    EventType::ButtonPressed(b, nec)
                                }
                                Some(AxisOrBtn::Axis(a)) => EventType::AxisChanged(a, 1.0, nec),
                                None => {
                                    self.events.push_back(Event {
                                        id,
                                        time,
                                        event: EventType::ButtonChanged(Button::Unknown, 1.0, nec),
                                    });

                                    EventType::ButtonPressed(Button::Unknown, nec)
                                }
                            }
                        }
                        RawEventType::ButtonReleased(nec) => {
                            let nec = Code(nec);
                            match self.gamepad(id).unwrap().axis_or_btn_name(nec) {
                                Some(AxisOrBtn::Btn(b)) => {
                                    self.events.push_back(Event {
                                        id,
                                        time,
                                        event: EventType::ButtonChanged(b, 0.0, nec),
                                    });

                                    EventType::ButtonReleased(b, nec)
                                }
                                Some(AxisOrBtn::Axis(a)) => EventType::AxisChanged(a, 0.0, nec),
                                None => {
                                    self.events.push_back(Event {
                                        id,
                                        time,
                                        event: EventType::ButtonChanged(Button::Unknown, 0.0, nec),
                                    });

                                    EventType::ButtonReleased(Button::Unknown, nec)
                                }
                            }
                        }
                        RawEventType::AxisValueChanged(val, nec) => {
                            // Let's trust at least our backend code
                            let axis_info = self.gamepad(id).unwrap().inner.axis_info(nec).unwrap().clone();
                            let nec = Code(nec);

                            match self.gamepad(id).unwrap().axis_or_btn_name(nec) {
                                Some(AxisOrBtn::Btn(b)) => {
                                    let val = btn_value(&axis_info, val);

                                    if val >= self.axis_to_btn_pressed
                                        && !self.gamepad(id).unwrap().state().is_pressed(nec)
                                    {
                                        self.events.push_back(Event {
                                            id,
                                            time,
                                            event: EventType::ButtonChanged(b, val, nec),
                                        });

                                        EventType::ButtonPressed(b, nec)
                                    } else if val <= self.axis_to_btn_released
                                        && self.gamepad(id).unwrap().state().is_pressed(nec)
                                    {
                                        self.events.push_back(Event {
                                            id,
                                            time,
                                            event: EventType::ButtonChanged(b, val, nec),
                                        });

                                        EventType::ButtonReleased(b, nec)
                                    } else {
                                        EventType::ButtonChanged(b, val, nec)
                                    }
                                }
                                Some(AxisOrBtn::Axis(a)) => {
                                    EventType::AxisChanged(a, axis_value(&axis_info, val, a), nec)
                                }
                                None => EventType::AxisChanged(
                                    Axis::Unknown,
                                    axis_value(&axis_info, val, Axis::Unknown),
                                    nec,
                                ),
                            }
                        }
                        RawEventType::Connected => {
                            if id == self.gamepads_data.len() {
                                self.gamepads_data.push(GamepadData::new(id, self.tx.clone(), self.inner.gamepad(id), &self.mappings));
                            } else if id < self.gamepads_data.len() {
                                self.gamepads_data[id] = GamepadData::new(id, self.tx.clone(), self.inner.gamepad(id), &self.mappings);
                            } else {
                                error!("Platform implementation error: got Connected event with id {}, when expected id {}", id, self.gamepads_data.len());
                            }

                            EventType::Connected
                        }
                        RawEventType::Disconnected => {
                            let data = &mut self.gamepads_data[id];
                            let _ = self.tx.send(Message::Close { id });

                            EventType::Disconnected
                        }
                    };

                    Some(Event { id, event, time })
                }
                None => None,
            }
        }
    }

    /// Updates internal state according to `event`.
    pub fn update(&mut self, event: &Event) {
        use EventType::*;

        let counter = self.counter;

        let data = match self.gamepads_data.get_mut(event.id) {
            Some(d) => d,
            None => return,
        };

        match event.event {
            ButtonPressed(_, nec) => {
                data
                    .state
                    .set_btn_pressed(nec, true, counter, event.time);
            }
            ButtonReleased(_, nec) => {
                data
                    .state
                    .set_btn_pressed(nec, false, counter, event.time);
            }
            ButtonRepeated(_, nec) => {
                data.state.set_btn_repeating(nec, counter, event.time);
            }
            ButtonChanged(_, value, nec) => {
                data.state.set_btn_value(nec, value, counter, event.time);
            }
            AxisChanged(_, value, nec) => {
                data
                    .state
                    .update_axis(nec, AxisData::new(value, counter, event.time));
            }
            Disconnected | Connected | Dropped => (),
        }
    }

    /// Increases internal counter by one. Counter data is stored with state and can be used to
    /// determine when last event happened. You probably want to use this function in your update
    /// loop after processing events.
    pub fn inc(&mut self) {
        // Counter is 62bit. See `ButtonData`.
        if self.counter == 0x3FFF_FFFF_FFFF_FFFF {
            self.counter = 0;
        } else {
            self.counter += 1;
        }
    }

    /// Returns counter. Counter data is stored with state and can be used to determine when last
    /// event happened.
    pub fn counter(&self) -> u64 {
        self.counter
    }

    /// Sets counter to 0.
    pub fn reset_counter(&mut self) {
        self.counter = 0;
    }

    fn finish_gamepads_creation(&mut self) {
        let tx = self.tx.clone();
        for id in 0..self.inner.last_gamepad_hint() {
            let gamepad = self.inner.gamepad(id);
            self.gamepads_data.push(GamepadData::new(id, tx.clone(), gamepad, &self.mappings))
        }
    }

    /// Returns handle to gamepad with given ID. Unlike `connected_gamepad()`, this function will
    /// also return handle to gamepad that is currently disconnected. `None` is only returned if
    /// gamepad with given ID have never been observed.
    ///
    /// ```
    /// # let mut gilrs = gilrs::Gilrs::new().unwrap();
    /// use gilrs::{Button, EventType};
    ///
    /// loop {
    ///     while let Some(ev) = gilrs.next_event() {
    ///         // unwrap() should never panic because we use id from event
    ///         let is_up_pressed = gilrs.gamepad(ev.id).unwrap().is_pressed(Button::DPadUp);
    ///
    ///         match ev.event_type {
    ///             EventType::ButtonPressed(Button::South, _) if is_up_pressed => {
    ///                 // do something…
    ///             }
    ///             _ => (),
    ///         }
    ///     }
    /// }
    /// ```
    pub fn gamepad<'a>(&'a self, id: usize) -> Option<Gamepad<'a>> {
        if let Some(data) = self.gamepads_data.get(id) {
            Some(Gamepad {
                inner: self.inner.gamepad(id),
                data,
            })
        } else {
            None
        }
    }

    /// Returns a reference to connected gamepad or `None`.
    pub fn connected_gamepad(&self, id: usize) -> Option<Gamepad> {
        match self.gamepad(id) {
            Some(gamepad) if gamepad.is_connected() => Some(gamepad),
            _ => None
        }
    }

    /// Returns iterator over all connected gamepads and their ids.
    ///
    /// ```
    /// # let gilrs = gilrs::Gilrs::new().unwrap();
    /// for (id, gamepad) in gilrs.gamepads() {
    ///     assert!(gamepad.is_connected());
    ///     println!("Gamepad with id {} and name {} is connected",
    ///              id, gamepad.name());
    /// }
    /// ```
    pub fn gamepads(&self) -> ConnectedGamepadsIterator {
        ConnectedGamepadsIterator(self, 0)
    }

    /// Adds `ev` at the end of internal event queue. It can later be retrieved with `next_event()`.
    pub fn insert_event(&mut self, ev: Event) {
        self.events.push_back(ev);
    }

    pub(crate) fn ff_sender(&self) -> &Sender<Message> {
        &self.tx
    }

    pub(crate) fn next_ff_id(&mut self) -> usize {
        // TODO: reuse free ids
        let id = self.next_id;
        self.next_id = match self.next_id.checked_add(1) {
            Some(x) => x,
            None => panic!("Failed to assign ID to new effect"),
        };
        id
    }
}

//impl<'a> Index<usize> for &'a Gilrs {
//    type Output = Gamepad<'a>;
//
//    fn index(&self, idx: usize) -> Gamepad<'a> {
//        self.gamepad(idx)
//    }
//}

/// Allow to create `Gilrs ` with customized behaviour.
pub struct GilrsBuilder {
    mappings: MappingDb,
    default_filters: bool,
    axis_to_btn_pressed: f32,
    axis_to_btn_released: f32,
    update_state: bool,
    env_mappings: bool,
    included_mappings: bool,
}

impl GilrsBuilder {
    /// Create builder with default settings. Use `build()` to create `Gilrs`.
    pub fn new() -> Self {
        GilrsBuilder {
            mappings: MappingDb::new(),
            default_filters: true,
            axis_to_btn_pressed: 0.75,
            axis_to_btn_released: 0.65,
            update_state: true,
            env_mappings: true,
            included_mappings: true,
        }
    }

    /// If `true`, use [`axis_dpad_to_button`](ev/filter/fn.axis_dpad_to_button.html),
    /// [`Jitter`](ev/filter/struct.Jitter.html) and [`deadzone`](ev/filter/fn.deadzone.html)
    /// filters with default parameters. Defaults to `true`.
    pub fn with_default_filters(mut self, default_filters: bool) -> Self {
        self.default_filters = default_filters;

        self
    }

    /// Adds SDL mappings.
    pub fn add_mappings(mut self, mappings: &str) -> Self {
        self.mappings.insert(mappings);

        self
    }

    /// If true, will add SDL mappings from `SDL_GAMECONTROLLERCONFIG` environment variable.
    /// Defaults to true.
    pub fn add_env_mappings(mut self, env_mappings: bool) -> Self {
        self.env_mappings = env_mappings;

        self
    }

    /// If true, will add SDL mappings included from
    /// https://github.com/gabomdq/SDL_GameControllerDB. Defaults to true.
    pub fn add_included_mappings(mut self, included_mappings: bool) -> Self {
        self.included_mappings = included_mappings;

        self
    }

    /// Sets values on which `ButtonPressed` and `ButtonReleased` events will be emitted. `build()`
    /// will return error if `pressed ≤ released` or if one of values is outside [0.0, 1.0].
    ///
    /// Defaults to 0.75 for `pressed` and 0.65 for `released`.
    pub fn set_axis_to_btn(mut self, pressed: f32, released: f32) -> Self {
        self.axis_to_btn_pressed = pressed;
        self.axis_to_btn_released = released;

        self
    }

    /// Disable or enable automatic state updates. You should use this if you use custom filters;
    /// in this case you have to update state manually anyway.
    pub fn set_update_state(mut self, enabled: bool) -> Self {
        self.update_state = enabled;

        self
    }

    /// Creates `Gilrs`.
    pub fn build(mut self) -> Result<Gilrs, Error> {
        if self.env_mappings {
            self.mappings.add_env_mappings();
        }

        if self.included_mappings {
            self.mappings.add_included_mappings();
        }

        if self.axis_to_btn_pressed <= self.axis_to_btn_released || self.axis_to_btn_pressed < 0.0
            || self.axis_to_btn_pressed > 1.0 || self.axis_to_btn_released < 0.0
            || self.axis_to_btn_released > 1.0
        {
            return Err(Error::InvalidAxisToBtn);
        }

        let mut is_dummy = false;
        let inner = match gilrs_core::Gilrs::new() {
            Ok(g) => g,
            Err(PlatformError::NotImplemented(g)) => {
                is_dummy = true;

                g
            }
            Err(PlatformError::Other(e)) => return Err(Error::Other(e)),
        };

        let mut gilrs = Gilrs {
            inner,
            next_id: 0,
            tx: server::init(),
            counter: 0,
            mappings: self.mappings,
            default_filters: self.default_filters,
            events: VecDeque::new(),
            axis_to_btn_pressed: self.axis_to_btn_pressed,
            axis_to_btn_released: self.axis_to_btn_released,
            update_state: self.update_state,
            gamepads_data: Vec::new(),
        };
        gilrs.finish_gamepads_creation();

        if is_dummy {
            Err(Error::NotImplemented(gilrs))
        } else {
            Ok(gilrs)
        }
    }
}

/// Iterator over all connected gamepads.
pub struct ConnectedGamepadsIterator<'a>(&'a Gilrs, usize);

impl<'a> Iterator for ConnectedGamepadsIterator<'a> {
    type Item = (usize, Gamepad<'a>);

    fn next(&mut self) -> Option<(usize, Gamepad<'a>)> {
        loop {
            if self.1 == self.0.inner.last_gamepad_hint() {
                return None;
            }

            if let Some(gp) = self.0.connected_gamepad(self.1) {
                let idx = self.1;
                self.1 += 1;
                return Some((idx, gp));
            }

            self.1 += 1;
        }
    }
}

///// Iterator over all connected gamepads.
//pub struct ConnectedGamepadsMutIterator<'a>(&'a mut Gilrs, usize);
//
//impl<'a> Iterator for ConnectedGamepadsMutIterator<'a> {
//    type Item = (usize, Gamepad<'a>);
//
//    fn next(&mut self) -> Option<(usize, &'a mut Gamepad)> {
//        loop {
//            if self.1 == self.0.inner.last_gamepad_hint() {
//                return None;
//            }
//
//            if let Some(gp) = self.0.get_mut(self.1) {
//                let idx = self.1;
//                self.1 += 1;
//                let gp = unsafe { &mut *(gp as *mut _) };
//                return Some((idx, gp));
//            }
//
//            self.1 += 1;
//        }
//    }
//}

/// Represents handle to game controller.
///
/// Using this struct you can access cached gamepad state, information about gamepad such as name
/// or UUID and manage force feedback effects.
#[derive(Debug, Copy, Clone)]
pub struct Gamepad<'a> {
    data: &'a GamepadData,
    inner: &'a gilrs_core::Gamepad,
}

impl<'a> Gamepad<'a> {
    /// Returns the mapping name if it exists otherwise returns the os provided name.
    /// Warning: May change from os provided name to mapping name after the first call of event_next.
    pub fn name(&self) -> &str {
        if let Some(map_name) = self.map_name() {
            map_name
        } else {
            self.os_name()
        }
    }

    /// if `mapping_source()` is `SdlMappings` returns the name of the mapping used by the gamepad.
    /// Otherwise returns `None`.
    ///
    /// Warning: Mappings are set after event `Connected` is processed therefore this function will
    /// always return `None` before first calls to `Gilrs::next_event()`.
    pub fn map_name(&self) -> Option<&str> {
        self.data.map_name()
    }

    /// Returns the name of the gamepad supplied by the OS.
    pub fn os_name(&self) -> &str {
        self.inner.name()
    }

    /// Returns gamepad's UUID.
    ///
    /// It is recommended to process with the [UUID crate](https://crates.io/crates/uuid).
    /// Use `Uuid::from_bytes` method to create a `Uuid` from the returned bytes.
    pub fn uuid(&self) -> [u8; 16] {
        self.inner.uuid()
    }

    /// Returns gamepad's UUID.
    pub(crate) fn internal_uuid(&self) -> Uuid {
        Uuid::from_bytes(self.inner.uuid())
    }

    /// Returns cached gamepad state.
    pub fn state(&self) -> &GamepadState {
        &self.data.state
    }

    /// Returns current gamepad's status, which can be `Connected`, `Disconnected` or `NotObserved`.
    /// Only connected gamepads generate events. Disconnected gamepads retain their name and UUID.
    /// Cached state of disconnected and not observed gamepads is 0 (false for buttons and 0.0 for
    /// axis) and all actions preformed on such gamepad are no-op.
    pub fn status(&self) -> Status {
        self.inner.status()
    }

    /// Returns true if gamepad is connected.
    pub fn is_connected(&self) -> bool {
        self.inner.is_connected()
    }

    /// Examines cached gamepad state to check if given button is pressed. Panics if `btn` is
    /// `Unknown`.
    ///
    /// If you know `Code` of the element that you want to examine, it's recommended to use methods
    /// directly on `State`, because this version have to check which `Code` is mapped to element of
    /// gamepad.
    pub fn is_pressed(&self, btn: Button) -> bool {
        self.data.is_pressed(btn)
    }

    /// Examines cached gamepad state to check axis's value. Panics if `axis` is `Unknown`.
    ///
    /// If you know `Code` of the element that you want to examine, it's recommended to use methods
    /// directly on `State`, because this version have to check which `Code` is mapped to element of
    /// gamepad.
    pub fn value(&self, axis: Axis) -> f32 {
        self.data.value(axis)
    }

    /// Returns button state and when it changed.
    ///
    /// If you know `Code` of the element that you want to examine, it's recommended to use methods
    /// directly on `State`, because this version have to check which `Code` is mapped to element of
    /// gamepad.
    pub fn button_data(&self, btn: Button) -> Option<&ButtonData> {
        self.data.button_data(btn)
    }

    /// Returns axis state and when it changed.
    ///
    /// If you know `Code` of the element that you want to examine, it's recommended to use methods
    /// directly on `State`, because this version have to check which `Code` is mapped to element of
    /// gamepad.
    pub fn axis_data(&self, axis: Axis) -> Option<&AxisData> {
        self.data.axis_data(axis)
    }

    /// Returns device's power supply state. See [`PowerInfo`](enum.PowerInfo.html) for details.
    pub fn power_info(&self) -> PowerInfo {
        self.inner.power_info()
    }

    /// Returns source of gamepad mapping. Can be used to filter gamepads which do not provide
    /// unified controller layout.
    ///
    /// ```
    /// use gilrs::MappingSource;
    /// # let mut gilrs = gilrs::Gilrs::new().unwrap();
    ///
    /// for (_, gamepad) in gilrs.gamepads().filter(
    ///     |gp| gp.1.mapping_source() != MappingSource::None)
    /// {
    ///     println!("{} is ready to use!", gamepad.name());
    /// }
    /// ```
    pub fn mapping_source(&self) -> MappingSource {
        if self.data.mapping.is_default() {
            // TODO: check if it's Driver or None
            MappingSource::Driver
        } else {
            MappingSource::SdlMappings
        }
    }

//    /// Sets gamepad's mapping and returns SDL2 representation of them. Returned mappings may not be
//    /// compatible with SDL2 - if it is important, use
//    /// [`set_mapping_strict()`](#method.set_mapping_strict).
//    ///
//    /// The `name` argument can be a string slice with custom gamepad name or `None`. If `None`,
//    /// gamepad name reported by driver will be used.
//    ///
//    /// # Errors
//    ///
//    /// This function return error if `name` contains comma, `mapping` have axis and button entry
//    /// for same element (for example `Axis::LetfTrigger` and `Button::LeftTrigger`) or gamepad does
//    /// not have any element with `EvCode` used in mapping. `Button::Unknown` and
//    /// `Axis::Unknown` are not allowd as keys to `mapping` – in this case,
//    /// `MappingError::UnknownElement` is returned.
//    ///
//    /// Error is also returned if this function is not implemented or gamepad is not connected.
//    ///
//    /// # Example
//    ///
//    /// ```
//    /// use gilrs::{Mapping, Button};
//    ///
//    /// # let mut gilrs = gilrs::Gilrs::new().unwrap();
//    /// let mut data = Mapping::new();
//    /// // …
//    ///
//    /// // or `match gilrs[0].set_mapping(&data, None) {`
//    /// match gilrs[0].set_mapping(&data, "Custom name") {
//    ///     Ok(sdl) => println!("SDL2 mapping: {}", sdl),
//    ///     Err(e) => println!("Failed to set mapping: {}", e),
//    /// };
//    /// ```
//    ///
//    /// See also `examples/mapping.rs`.
//    pub fn set_mapping<'a, O: Into<Option<&'a str>>>(
//        &mut self,
//        mapping: &MappingData,
//        name: O,
//    ) -> Result<String, MappingError> {
//        if !self.is_connected() {
//            return Err(MappingError::NotConnected);
//        }
//
//        let name = match name.into() {
//            Some(s) => s,
//            None => self.inner.name(),
//        };
//
//        let (mapping, s) = Mapping::from_data(
//            mapping,
//            self.inner.buttons(),
//            self.inner.axes(),
//            name,
//            self.internal_uuid(),
//        )?;
//        self.mapping = mapping;
//
//        Ok(s)
//    }
//
//    /// Similar to [`set_mapping()`](#method.set_mapping) but returned string should be compatible
//    /// with SDL2.
//    ///
//    /// # Errors
//    ///
//    /// Returns `MappingError::NotSdl2Compatible` if `mapping` have an entry for `Button::{C, Z}`
//    /// or `Axis::{LeftZ, RightZ}`.
//    pub fn set_mapping_strict<'a, O: Into<Option<&'a str>>>(
//        &mut self,
//        mapping: &MappingData,
//        name: O,
//    ) -> Result<String, MappingError> {
//        if mapping.button(Button::C).is_some() || mapping.button(Button::Z).is_some()
//            || mapping.axis(Axis::LeftZ).is_some()
//            || mapping.axis(Axis::RightZ).is_some()
//            {
//                Err(MappingError::NotSdl2Compatible)
//            } else {
//            self.set_mapping(mapping, name)
//        }
//   }

    /// Returns true if force feedback is supported by device.
    pub fn is_ff_supported(&self) -> bool {
        self.inner.is_ff_supported()
    }

    /// Change gamepad position used by force feedback effects.
    pub fn set_listener_position<Vec3: Into<[f32; 3]>>(
        &self,
        position: Vec3,
    ) -> Result<(), FfError> {
        if !self.is_connected() {
            Err(FfError::Disconnected(self.id()))
        } else if !self.is_ff_supported() {
            Err(FfError::FfNotSupported(self.id()))
        } else {
            self.data.tx.send(Message::SetListenerPosition {
                id: self.data.id,
                position: position.into(),
            })?;
            Ok(())
        }
    }

    /// Returns `AxisOrBtn` mapped to `Code`.
    pub fn axis_or_btn_name(&self, ec: Code) -> Option<AxisOrBtn> {
        self.data.axis_or_btn_name(ec)
    }

    /// Returns `Code` associated with `btn`.
    pub fn button_code(&self, btn: Button) -> Option<Code> {
        self.data.button_code(btn)
    }

    /// Returns `Code` associated with `axis`.
    pub fn axis_code(&self, axis: Axis) -> Option<Code> {
        self.data.axis_code(axis)
    }

    /// Returns area in which axis events should be ignored.
    pub fn deadzone(&self, axis: Code) -> Option<f32> {
        self.inner.axis_info(axis.0).map(|i| i.deadzone())
    }

    /// Returns ID of gamepad.
    pub fn id(&self) -> usize {
        self.data.id
    }

    pub(crate) fn mapping(&self) -> &Mapping {
        &self.data.mapping
    }
}

#[derive(Debug)]
struct GamepadData {
    state: GamepadState,
    mapping: Mapping,
    tx: Sender<Message>,
    id: usize,
}

impl GamepadData {
    fn new(id: usize, tx: Sender<Message>, gamepad: &gilrs_core::Gamepad, db: &MappingDb) -> Self {
        let mapping = db
            .get(Uuid::from_bytes(gamepad.uuid()))
            .and_then(|s| {
                Mapping::parse_sdl_mapping(
                    s,
                    gamepad.buttons(),
                    gamepad.axes(),
                ).ok()
            })
            .unwrap_or_default();

        if gamepad.is_ff_supported() {
            if let Some(device) = gamepad.ff_device() {
                let _ = tx.send(Message::Open { id, device });
            }
        }

        GamepadData {
            state: GamepadState::new(),
            mapping,
            tx,
            id,
        }
    }

    /// if `mapping_source()` is `SdlMappings` returns the name of the mapping used by the gamepad.
    /// Otherwise returns `None`.
    ///
    /// Warning: Mappings are set after event `Connected` is processed therefore this function will
    /// always return `None` before first calls to `Gilrs::next_event()`.
    pub fn map_name(&self) -> Option<&str> {
        if self.mapping.is_default() {
            None
        } else {
            Some(&self.mapping.name())
        }
    }

    /// Examines cached gamepad state to check if given button is pressed. Panics if `btn` is
    /// `Unknown`.
    ///
    /// If you know `Code` of the element that you want to examine, it's recommended to use methods
    /// directly on `State`, because this version have to check which `Code` is mapped to element of
    /// gamepad.
    pub fn is_pressed(&self, btn: Button) -> bool {
        assert_ne!(btn, Button::Unknown);

        self.button_code(btn)
            .or_else(|| btn.to_nec())
            .map(|nec| self.state.is_pressed(nec))
            .unwrap_or(false)
    }

    /// Examines cached gamepad state to check axis's value. Panics if `axis` is `Unknown`.
    ///
    /// If you know `Code` of the element that you want to examine, it's recommended to use methods
    /// directly on `State`, because this version have to check which `Code` is mapped to element of
    /// gamepad.
    pub fn value(&self, axis: Axis) -> f32 {
        assert_ne!(axis, Axis::Unknown);

        self.axis_code(axis)
            .map(|nec| self.state.value(nec))
            .unwrap_or(0.0)
    }

    /// Returns button state and when it changed.
    ///
    /// If you know `Code` of the element that you want to examine, it's recommended to use methods
    /// directly on `State`, because this version have to check which `Code` is mapped to element of
    /// gamepad.
    pub fn button_data(&self, btn: Button) -> Option<&ButtonData> {
        self.button_code(btn)
            .and_then(|nec| self.state.button_data(nec))
    }

    /// Returns axis state and when it changed.
    ///
    /// If you know `Code` of the element that you want to examine, it's recommended to use methods
    /// directly on `State`, because this version have to check which `Code` is mapped to element of
    /// gamepad.
    pub fn axis_data(&self, axis: Axis) -> Option<&AxisData> {
        self.axis_code(axis)
            .and_then(|nec| self.state.axis_data(nec))
    }

    /// Returns `AxisOrBtn` mapped to `Code`.
    pub fn axis_or_btn_name(&self, ec: Code) -> Option<AxisOrBtn> {
        self.mapping.map(&ec.0)
    }

    /// Returns `Code` associated with `btn`.
    pub fn button_code(&self, btn: Button) -> Option<Code> {
        self.mapping
            .map_rev(&AxisOrBtn::Btn(btn))
            .map(|nec| Code(nec))
    }

    /// Returns `Code` associated with `axis`.
    pub fn axis_code(&self, axis: Axis) -> Option<Code> {
        self.mapping
            .map_rev(&AxisOrBtn::Axis(axis))
            .map(|nec| Code(nec))
    }
}

/// Source of gamepad mappings.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum MappingSource {
    /// Gamepad uses SDL mappings.
    SdlMappings,
    /// Gamepad does not use any mappings but driver should provide unified controller layout.
    Driver,
    /// Gamepad does not use any mappings and most gamepad events will probably be `Button::Unknown`
    /// or `Axis::Unknown`
    None,
}

fn axis_value(info: &AxisInfo, val: i32, axis: Axis) -> f32 {
    let range = (info.max - info.min) as f32;
    let mut val = (val - info.min) as f32;
    val = val / range * 2.0 - 1.0;

    if gilrs_core::IS_Y_AXIS_REVERSED && (axis == Axis::LeftStickY || axis == Axis::RightStickY) {
        val = -val;
    }

    utils::clamp(val, -1.0, 1.0)
}

fn btn_value(info: &AxisInfo, val: i32) -> f32 {
    let range = (info.max - info.min) as f32;
    let mut val = (val - info.min) as f32;
    val = val / range;

    utils::clamp(val, 0.0, 1.0)
}

/// Error type which can be returned when creating `Gilrs`.
#[derive(Debug)]
pub enum Error {
    /// Gilrs does not support current platform, but you can use dummy context from this error if
    /// gamepad input is not essential.
    NotImplemented(Gilrs),
    /// Either `pressed ≤ released` or one of values is outside [0.0, 1.0] range.
    InvalidAxisToBtn,
    /// Platform specific error.
    Other(Box<error::Error + Send + Sync>),
}

impl Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            &Error::NotImplemented(_) => f.write_str("Gilrs does not support current platform."),
            &Error::InvalidAxisToBtn => f.write_str(
                "Either `pressed ≤ released` or one of values is outside [0.0, 1.0] range.",
            ),
            &Error::Other(ref e) => e.fmt(f),
        }
    }
}

impl error::Error for Error {
    fn description(&self) -> &str {
        match self {
            &Error::NotImplemented(_) => "platform not supported",
            &Error::InvalidAxisToBtn => "values passed to set_axis_to_btn() are invalid",
            &Error::Other(_) => "platform specific error",
        }
    }

    fn cause(&self) -> Option<&error::Error> {
        match self {
            &Error::NotImplemented(_) => None,
            &Error::InvalidAxisToBtn => None,
            &Error::Other(ref e) => Some(&**e),
        }
    }
}

