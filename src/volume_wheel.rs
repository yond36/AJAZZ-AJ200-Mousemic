//! Hold a configured side button and turn the vertical wheel to change volume.
//!
//! `forward` is XBUTTON2; `backward` is XBUTTON1. WH_MOUSE_LL is global and
//! cannot identify a physical device: this applies to **all mice**, including
//! a button held on one mouse while another mouse supplies wheel events.
//! Each 120 wheel units sends one VK_VOLUME_UP/DOWN tap (the Windows default
//! volume step, usually 2%). Horizontal wheels and unconfigured buttons pass
//! through. A configured button's ordinary click is replayed on release, so
//! its timing/target and drag behavior differ from an unmodified side button.
//! Injected mouse events bypass this feature. SendInput is subject to UIPI;
//! elevated applications may reject replayed clicks. Windows can also silently
//! remove a low-level hook that exceeds its timeout. Use one active instance.

use std::{
    cell::RefCell,
    mem::size_of,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc, Arc,
    },
    thread::{self, JoinHandle},
};

use anyhow::{anyhow, Context, Result};
use windows::Win32::{
    Foundation::{HINSTANCE, LPARAM, LRESULT, WAIT_FAILED, WPARAM},
    System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
    UI::{
        Input::KeyboardAndMouse::{
            SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP,
            MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, VK_VOLUME_DOWN, VK_VOLUME_UP,
        },
        WindowsAndMessaging::{
            CallNextHookEx, DispatchMessageW, MsgWaitForMultipleObjectsEx, PeekMessageW,
            PostThreadMessageW, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx,
            HC_ACTION, HHOOK, MSG, MSLLHOOKSTRUCT, MWMO_INPUTAVAILABLE, PM_NOREMOVE, PM_REMOVE,
            QS_ALLINPUT, WH_MOUSE_LL, WM_APP, WM_MOUSEWHEEL, WM_QUIT, WM_XBUTTONDOWN, WM_XBUTTONUP,
            XBUTTON1, XBUTTON2,
        },
    },
};

use state::{Action, Button, Event, State};

const MSG_VOLUME: u32 = WM_APP + 0x351;
const MSG_CLICK: u32 = WM_APP + 0x352;
const MSG_STOP: u32 = WM_APP + 0x353;
const MSG_RESET: u32 = WM_APP + 0x354;

/// Owns a global mouse hook and its dedicated message thread.
/// Dropping this value stops interception, discards pending gestures/clicks,
/// and joins the thread. Construction waits until hook installation succeeds.
pub struct VolumeWheel {
    stop: Arc<AtomicBool>,
    generation: Arc<AtomicUsize>,
    thread_id: u32,
    thread: Option<JoinHandle<()>>,
}

impl VolumeWheel {
    pub fn start(forward: bool, backward: bool) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let generation = Arc::new(AtomicUsize::new(0));
        if !forward && !backward {
            return Ok(Self {
                stop,
                generation,
                thread_id: 0,
                thread: None,
            });
        }
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let thread_stop = Arc::clone(&stop);
        let thread_generation = Arc::clone(&generation);
        let thread = thread::Builder::new()
            .name("volume-wheel".into())
            .spawn(move || {
                // PeekMessage creates the thread's queue before readiness is sent.
                let mut msg = MSG::default();
                let _ = unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE) };
                let thread_id = unsafe { GetCurrentThreadId() };
                HOOK_STATE.with(|slot| {
                    *slot.borrow_mut() = Some(HookState {
                        state: State::new(forward, backward),
                        thread_id,
                        stop: Arc::clone(&thread_stop),
                        generation: Arc::clone(&thread_generation),
                        observed_generation: 0,
                        post_failed: false,
                    });
                });
                let installed = (|| -> Result<HookGuard> {
                    let module = unsafe { GetModuleHandleW(None) }
                        .context("volume wheel: GetModuleHandleW failed")?;
                    let hook = unsafe {
                        SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), HINSTANCE(module.0), 0)
                    }
                    .context("volume wheel: SetWindowsHookExW failed")?;
                    Ok(HookGuard(hook))
                })();
                match installed {
                    Ok(hook) => {
                        if ready_tx.send(Ok(thread_id)).is_ok() {
                            message_loop(&thread_stop, &thread_generation);
                        }
                        // Do not replay any held buttons during shutdown.
                        drop(hook);
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                    }
                }
                HOOK_STATE.with(|slot| *slot.borrow_mut() = None);
            })
            .context("volume wheel: could not spawn hook thread")?;

        match ready_rx.recv() {
            Ok(Ok(thread_id)) => Ok(Self {
                stop,
                generation,
                thread_id,
                thread: Some(thread),
            }),
            result => {
                stop.store(true, Ordering::Release);
                let _ = thread.join();
                match result {
                    Ok(Err(error)) => Err(error),
                    _ => Err(anyhow!("volume wheel: hook thread exited before readiness")),
                }
            }
        }
    }
}

impl VolumeWheel {
    /// Asynchronously clears held buttons and wheel remainder without replaying
    /// clicks. Call on bridge disconnect/reconnect. Queued actions from before
    /// this call are invalidated immediately; an already executing SendInput
    /// cannot be recalled. A failed wake-up is covered by the loop's timeout.
    pub fn reset(&self) {
        if self.thread.is_some() {
            self.generation.fetch_add(1, Ordering::AcqRel);
            let _ = unsafe { PostThreadMessageW(self.thread_id, MSG_RESET, WPARAM(0), LPARAM(0)) };
        }
    }
}

impl Drop for VolumeWheel {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            // A full/destroyed message queue must not prevent shutdown: the
            // loop also observes `stop` at least every 100ms while idle.
            let _ = unsafe { PostThreadMessageW(self.thread_id, MSG_STOP, WPARAM(0), LPARAM(0)) };
            if thread.join().is_err() {
                log::warn!("volume wheel: hook thread panicked");
            }
        }
    }
}

struct HookGuard(HHOOK);
impl Drop for HookGuard {
    fn drop(&mut self) {
        if let Err(error) = unsafe { UnhookWindowsHookEx(self.0) } {
            log::warn!("volume wheel: could not remove hook: {error}");
        }
    }
}

struct HookState {
    state: State,
    thread_id: u32,
    stop: Arc<AtomicBool>,
    generation: Arc<AtomicUsize>,
    observed_generation: usize,
    post_failed: bool,
}

impl HookState {
    fn apply_reset(&mut self) {
        let generation = self.generation.load(Ordering::Acquire);
        if generation != self.observed_generation {
            self.state.reset();
            self.observed_generation = generation;
        }
    }
}

fn apply_pending_reset() {
    HOOK_STATE.with(|slot| {
        if let Some(context) = slot.borrow_mut().as_mut() {
            context.apply_reset();
        }
    });
}

thread_local! {
    static HOOK_STATE: RefCell<Option<HookState>> = const { RefCell::new(None) };
}

unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && lparam.0 != 0 {
        // Windows owns this structure for the duration of the callback.
        let data = &*(lparam.0 as *const MSLLHOOKSTRUCT);
        if !state::is_injected(data.flags) {
            let high_word = (data.mouseData >> 16) as u16;
            let event = match wparam.0 as u32 {
                WM_XBUTTONDOWN | WM_XBUTTONUP => {
                    let button = match high_word {
                        XBUTTON1 => Some(Button::Backward),
                        XBUTTON2 => Some(Button::Forward),
                        _ => None,
                    };
                    button.map(|button| {
                        if wparam.0 as u32 == WM_XBUTTONDOWN {
                            Event::Down(button)
                        } else {
                            Event::Up(button)
                        }
                    })
                }
                WM_MOUSEWHEEL => Some(Event::Wheel(high_word as i16)),
                _ => None,
            };
            if let Some(event) = event {
                let swallow = HOOK_STATE.with(|slot| {
                    // No blocking locks, allocation, logging, SendInput or channel
                    // sends in the callback. Reentrant calls fail open.
                    let Ok(mut slot) = slot.try_borrow_mut() else {
                        return false;
                    };
                    let Some(context) = slot.as_mut() else {
                        return false;
                    };
                    if context.stop.load(Ordering::Acquire) {
                        return false;
                    }
                    context.apply_reset();
                    let outcome = context.state.handle(event);
                    if let Some(action) = outcome.action {
                        let (message, value) = match action {
                            Action::Volume(steps) => (MSG_VOLUME, steps as isize),
                            Action::Click(button) => (MSG_CLICK, button.mask() as isize),
                        };
                        if PostThreadMessageW(
                            context.thread_id,
                            message,
                            WPARAM(context.observed_generation),
                            LPARAM(value),
                        )
                        .is_err()
                        {
                            context.post_failed = true;
                        }
                    }
                    outcome.swallow
                });
                if swallow {
                    return LRESULT(1);
                }
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

fn message_loop(stop: &AtomicBool, generation: &AtomicUsize) {
    let mut msg = MSG::default();
    while !stop.load(Ordering::Acquire) {
        apply_pending_reset();
        // PeekMessage also dispatches the system's low-level hook callbacks.
        while !stop.load(Ordering::Acquire)
            && unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool()
        {
            // A callback can have run while retrieving the message.
            if stop.load(Ordering::Acquire) {
                return;
            }
            apply_pending_reset();
            // Generation tags invalidate stale messages even when they precede
            // MSG_RESET in the queue. Draining them cannot discard fresh input.
            if matches!(msg.message, MSG_VOLUME | MSG_CLICK)
                && msg.wParam.0 != generation.load(Ordering::Acquire)
            {
                continue;
            }
            match msg.message {
                MSG_RESET => {}
                MSG_STOP | WM_QUIT => return,
                MSG_CLICK => {
                    if let Some(button) = match msg.lParam.0 {
                        1 => Some(Button::Backward),
                        2 => Some(Button::Forward),
                        _ => None,
                    } {
                        send_click(button);
                    }
                }
                MSG_VOLUME => {
                    let steps = msg.lParam.0;
                    // Each actual wheel event is an i16, so no valid message
                    // needs more than 274 steps. Bound even malformed messages.
                    for _ in 0..steps.unsigned_abs().min(274) {
                        if stop.load(Ordering::Acquire) {
                            return;
                        }
                        if msg.wParam.0 != generation.load(Ordering::Acquire) {
                            break;
                        }
                        send_volume(steps > 0);
                    }
                }
                _ => unsafe {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                },
            }
        }
        let failed = HOOK_STATE.with(|slot| {
            slot.borrow_mut()
                .as_mut()
                .is_some_and(|s| std::mem::take(&mut s.post_failed))
        });
        if failed {
            log::warn!("volume wheel: message queue full; input action dropped");
        }
        if stop.load(Ordering::Acquire) {
            break;
        }
        if unsafe { MsgWaitForMultipleObjectsEx(None, 100, QS_ALLINPUT, MWMO_INPUTAVAILABLE) }
            == WAIT_FAILED
        {
            log::warn!("volume wheel: waiting for mouse messages failed");
            break;
        }
    }
}

fn send_click(button: Button) {
    let input = |flags| INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                mouseData: button.mask() as u32,
                dwFlags: flags,
                ..Default::default()
            },
        },
    };
    send_pair([input(MOUSEEVENTF_XDOWN), input(MOUSEEVENTF_XUP)]);
}

fn send_volume(up: bool) {
    let input = |flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: if up { VK_VOLUME_UP } else { VK_VOLUME_DOWN },
                dwFlags: flags,
                ..Default::default()
            },
        },
    };
    send_pair([input(Default::default()), input(KEYEVENTF_KEYUP)]);
}

fn send_pair(inputs: [INPUT; 2]) {
    let sent = unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) };
    if sent != 2 {
        // Best-effort release if Windows accepted only the down event.
        if sent == 1 {
            unsafe { SendInput(&inputs[1..], size_of::<INPUT>() as i32) };
        }
        log::warn!("volume wheel: SendInput accepted {sent}/2 events (possibly blocked by UIPI)");
    }
}

// Kept independent of Win32 so all gesture rules can be tested without a hook.
mod state {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum Button {
        Backward,
        Forward,
    }
    impl Button {
        pub(super) fn mask(self) -> u8 {
            match self {
                Self::Backward => 1,
                Self::Forward => 2,
            }
        }
    }
    #[derive(Clone, Copy, Debug)]
    pub(super) enum Event {
        Down(Button),
        Up(Button),
        Wheel(i16),
    }
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum Action {
        Click(Button),
        Volume(i32),
    }
    #[derive(Debug, Default, PartialEq, Eq)]
    pub(super) struct Outcome {
        pub(super) swallow: bool,
        pub(super) action: Option<Action>,
    }
    pub(super) struct State {
        configured: u8,
        held: u8,
        gestured: u8,
        remainder: i32,
    }
    // MSLLHOOKSTRUCT.flags: LLMHF_INJECTED | LLMHF_LOWER_IL_INJECTED.
    // Other flag bits and dwExtraInfo are not evidence of injection.
    pub(super) fn is_injected(flags: u32) -> bool {
        flags & 0x3 != 0
    }

    impl State {
        pub(super) fn new(forward: bool, backward: bool) -> Self {
            Self {
                configured: (if forward { 2 } else { 0 }) | (if backward { 1 } else { 0 }),
                held: 0,
                gestured: 0,
                remainder: 0,
            }
        }
        pub(super) fn reset(&mut self) {
            self.held = 0;
            self.gestured = 0;
            self.remainder = 0;
        }

        pub(super) fn handle(&mut self, event: Event) -> Outcome {
            let action = match event {
                Event::Down(button) => {
                    let bit = button.mask();
                    if self.configured & bit == 0 {
                        return Outcome::default();
                    }
                    if self.held & bit == 0 {
                        // Joining an ongoing volume chord must not navigate.
                        if self.gestured & self.held != 0 {
                            self.gestured |= bit;
                        }
                        self.held |= bit;
                    }
                    None
                }
                Event::Up(button) => {
                    let bit = button.mask();
                    // Pass an orphan release: its down may predate installation.
                    if self.held & bit == 0 {
                        return Outcome::default();
                    }
                    let click = self.gestured & bit == 0;
                    self.held &= !bit;
                    self.gestured &= !bit;
                    if self.held == 0 {
                        self.remainder = 0;
                    }
                    if click {
                        Some(Action::Click(button))
                    } else {
                        None
                    }
                }
                Event::Wheel(delta) => {
                    if self.held == 0 {
                        return Outcome::default();
                    }
                    // Even zero/sub-notch wheel messages cancel navigation.
                    self.gestured |= self.held;
                    self.remainder += i32::from(delta);
                    let steps = self.remainder / 120;
                    self.remainder %= 120;
                    if steps != 0 {
                        Some(Action::Volume(steps))
                    } else {
                        None
                    }
                }
            };
            Outcome {
                swallow: true,
                action,
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use Button::{Backward as B, Forward as F};
        use Event::{Down, Up, Wheel};

        fn swallowed(action: Option<Action>) -> Outcome {
            Outcome {
                swallow: true,
                action,
            }
        }

        #[test]
        fn reset_clears_chord_and_remainder_without_navigation() {
            let mut s = State::new(true, true);
            s.handle(Down(F));
            s.handle(Down(B));
            s.handle(Wheel(60));
            s.reset();
            assert_eq!((s.held, s.gestured, s.remainder), (0, 0, 0));
            assert_eq!(s.handle(Wheel(120)), Outcome::default());
            assert_eq!(s.handle(Up(F)), Outcome::default());
            assert_eq!(s.handle(Up(B)), Outcome::default());
            s.handle(Down(F));
            assert_eq!(s.handle(Wheel(60)), swallowed(None));
            assert_eq!(s.handle(Up(F)), swallowed(None));
            s.handle(Down(B));
            assert_eq!(s.handle(Up(B)), swallowed(Some(Action::Click(B))));
        }

        #[test]
        fn reset_is_idempotent_and_keeps_configuration() {
            for (forward, backward) in [(false, false), (true, false), (false, true), (true, true)]
            {
                let mut s = State::new(forward, backward);
                s.handle(Down(F));
                s.handle(Down(B));
                s.reset();
                s.reset();
                assert_eq!(s.handle(Up(F)), Outcome::default());
                assert_eq!(s.handle(Up(B)), Outcome::default());
                for (button, enabled) in [(F, forward), (B, backward)] {
                    assert_eq!(s.handle(Down(button)).swallow, enabled);
                    assert_eq!(
                        s.handle(Up(button)).action,
                        if enabled {
                            Some(Action::Click(button))
                        } else {
                            None
                        }
                    );
                }
            }
        }

        #[test]
        fn configuration_combinations_and_ordinary_clicks() {
            for forward in [false, true] {
                for backward in [false, true] {
                    let mut s = State::new(forward, backward);
                    assert_eq!(s.handle(Wheel(120)), Outcome::default());
                    for (button, enabled) in [(F, forward), (B, backward)] {
                        assert_eq!(
                            s.handle(Down(button)),
                            if enabled {
                                swallowed(None)
                            } else {
                                Outcome::default()
                            }
                        );
                        assert_eq!(
                            s.handle(Up(button)),
                            if enabled {
                                swallowed(Some(Action::Click(button)))
                            } else {
                                Outcome::default()
                            }
                        );
                    }
                }
            }
        }

        #[test]
        fn unconfigured_button_does_not_modify_gesture() {
            let mut s = State::new(true, false);
            s.handle(Down(F));
            assert_eq!(s.handle(Down(B)), Outcome::default());
            assert_eq!(s.handle(Wheel(120)), swallowed(Some(Action::Volume(1))));
            assert_eq!(s.handle(Up(B)), Outcome::default());
            assert_eq!(s.handle(Wheel(-120)), swallowed(Some(Action::Volume(-1))));
            assert_eq!(s.handle(Up(F)), swallowed(None));
        }

        #[test]
        fn either_button_can_continue_interleaved_chord() {
            for (first, second) in [(F, B), (B, F)] {
                let mut s = State::new(true, true);
                s.handle(Down(first));
                s.handle(Wheel(60));
                s.handle(Down(second));
                assert_eq!(s.handle(Up(first)), swallowed(None));
                assert_eq!(s.handle(Wheel(60)), swallowed(Some(Action::Volume(1))));
                assert_eq!(s.handle(Up(second)), swallowed(None));
                assert_eq!(s.handle(Wheel(120)), Outcome::default());
            }
        }

        #[test]
        fn simultaneous_buttons_do_not_double_volume() {
            let mut s = State::new(true, true);
            s.handle(Down(F));
            s.handle(Down(B));
            assert_eq!(s.handle(Wheel(240)), swallowed(Some(Action::Volume(2))));
            assert_eq!(s.handle(Up(B)), swallowed(None));
            assert_eq!(s.handle(Wheel(-120)), swallowed(Some(Action::Volume(-1))));
            assert_eq!(s.handle(Up(F)), swallowed(None));
        }

        #[test]
        fn joining_used_chord_does_not_replay_click() {
            let mut s = State::new(true, true);
            s.handle(Down(F));
            s.handle(Wheel(1));
            s.handle(Down(B));
            assert_eq!(s.handle(Up(B)), swallowed(None));
            assert_eq!(s.handle(Up(F)), swallowed(None));
        }

        #[test]
        fn two_buttons_without_wheel_replay_independently() {
            let mut s = State::new(true, true);
            s.handle(Down(F));
            s.handle(Down(B));
            assert_eq!(s.handle(Up(F)), swallowed(Some(Action::Click(F))));
            assert_eq!(s.handle(Up(B)), swallowed(Some(Action::Click(B))));
        }

        #[test]
        fn half_notches_accumulate_in_both_directions() {
            for delta in [60, -60] {
                let mut s = State::new(true, false);
                s.handle(Down(F));
                assert_eq!(s.handle(Wheel(delta)), swallowed(None));
                assert_eq!(
                    s.handle(Wheel(delta)),
                    swallowed(Some(Action::Volume(i32::from(delta) / 60)))
                );
                assert_eq!(s.handle(Up(F)), swallowed(None));
            }
        }

        #[test]
        fn reversals_cancel_remainder() {
            let mut s = State::new(true, false);
            s.handle(Down(F));
            s.handle(Wheel(90));
            assert_eq!(s.handle(Wheel(-60)), swallowed(None));
            assert_eq!(s.handle(Wheel(-150)), swallowed(Some(Action::Volume(-1))));
            assert_eq!(s.remainder, 0);
        }

        #[test]
        fn final_release_clears_remainder_and_gesture() {
            let mut s = State::new(true, false);
            s.handle(Down(F));
            s.handle(Wheel(60));
            assert_eq!(s.handle(Up(F)), swallowed(None));
            assert_eq!(s.remainder, 0);
            s.handle(Down(F));
            assert_eq!(s.handle(Wheel(60)), swallowed(None));
            s.handle(Up(F));
            s.handle(Down(F));
            assert_eq!(s.handle(Up(F)), swallowed(Some(Action::Click(F))));
        }

        #[test]
        fn repeated_down_does_not_reset_gesture_or_remainder() {
            let mut s = State::new(true, false);
            s.handle(Down(F));
            s.handle(Wheel(60));
            assert_eq!(s.handle(Down(F)), swallowed(None));
            assert_eq!(s.handle(Wheel(60)), swallowed(Some(Action::Volume(1))));
            assert_eq!(s.handle(Up(F)), swallowed(None));
            assert_eq!(s.handle(Up(F)), Outcome::default());
        }

        #[test]
        fn maximum_wheel_delta_is_safe() {
            let mut s = State::new(true, false);
            s.handle(Down(F));
            s.handle(Wheel(119));
            assert_eq!(
                s.handle(Wheel(i16::MAX)),
                swallowed(Some(Action::Volume(274)))
            );
            assert!(s.remainder.abs() < 120);
            s.handle(Wheel(i16::MIN));
            assert!(s.remainder.abs() < 120);
        }

        #[test]
        fn injected_flags_are_filtered_without_hiding_unrelated_flags() {
            for flags in [1, 2, 3, 0x81, 0xffff_ffff] {
                assert!(is_injected(flags));
            }
            for flags in [0, 4, 0x80, 0xffff_fffc] {
                assert!(!is_injected(flags));
            }
            let mut s = State::new(true, false);
            s.handle(Down(F));
            // Same gate as mouse_hook: injected wheel/release cannot alter
            // physical state, and a replayed down/up cannot recurse.
            for event in [Wheel(120), Up(F), Down(F)] {
                if !is_injected(1) {
                    s.handle(event);
                }
            }
            assert_eq!(s.handle(Up(F)), swallowed(Some(Action::Click(F))));
        }
    }
}
