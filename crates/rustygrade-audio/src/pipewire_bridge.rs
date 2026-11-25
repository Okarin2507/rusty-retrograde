/// `node.name` of the virtual sink the bridge creates. mpv lists it as an output
/// (`pipewire/rustygrade-direct-bridge`); it is Rustygrade's own volume relay, never a place to play to.
pub const BRIDGE_NODE_NAME: &str = "rustygrade-direct-bridge";

/// Events emitted by [`PipeWireBridge`] back to the application/player coordinator.
#[derive(Debug, Clone, PartialEq)]
pub enum BridgeEvent {
    /// Virtual bridge sink was bound in PipeWire with global object ID `node_id`.
    Ready { node_id: u32, description: String },
    /// External client (Noctalia slider, wpctl, pavucontrol, media keys) changed sink volume (`0.0..=1.0`).
    VolumeChanged(f64),
    /// External client toggled sink mute state.
    MuteChanged(bool),
    /// PipeWire bridge disconnected or stopped.
    Disconnected(String),
}

/// Converts a linear UI/mixer slider position (`0.0..=1.0`) to PipeWire's cubic volume scale (`level^3`).
pub fn linear_to_pw_cubic(level: f64) -> f32 {
    if !level.is_finite() {
        return 0.0;
    }
    let clamped = level.clamp(0.0, 1.0);
    (clamped * clamped * clamped) as f32
}

/// Converts a PipeWire cubic volume factor (`channelVolumes[i]`) back to a linear slider position (`cbrt(pw)`).
pub fn pw_cubic_to_linear(pw: f32) -> f64 {
    if !pw.is_finite() || pw <= 0.0 {
        return 0.0;
    }
    (pw as f64).cbrt().clamp(0.0, 1.0)
}

#[cfg(feature = "pipewire-bridge")]
mod imp {
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::io::Cursor;
    use std::rc::Rc;
    use std::sync::mpsc as std_mpsc;
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    use libspa::param::ParamType;
    use libspa::pod::deserialize::PodDeserializer;
    use libspa::pod::serialize::PodSerializer;
    use libspa::pod::{Object, Pod, Property, Value, ValueArray};
    use libspa::utils::SpaTypes;
    use pipewire::channel;
    use pipewire::context::ContextRc;
    use pipewire::main_loop::MainLoopRc;
    use pipewire::node::{Node, NodeListener};
    use pipewire::properties::properties;
    use pipewire::proxy::ProxyT;
    use tokio::sync::mpsc;
    use tracing::{debug, warn};

    use super::{BridgeEvent, linear_to_pw_cubic, pw_cubic_to_linear};

    const NODE_NAME: &str = super::BRIDGE_NODE_NAME;
    const SELF_ECHO_WINDOW: Duration = Duration::from_millis(600);
    const SELF_ECHO_EPSILON: f64 = 0.005;
    const ECHO_HISTORY_CAP: usize = 16;

    enum BridgeCommand {
        SetVolume(f64),
        SetMute(bool),
        Shutdown,
    }

    struct BridgeState {
        node: Option<Node>,
        channels: usize,
        known_volume: f64,
        known_muted: bool,
        recent_volumes: VecDeque<(f64, Instant)>,
        recent_mutes: VecDeque<(bool, Instant)>,
        ready_sent: bool,
    }

    impl BridgeState {
        fn record_sent_volume(&mut self, level: f64) {
            if self.recent_volumes.len() >= ECHO_HISTORY_CAP {
                self.recent_volumes.pop_front();
            }
            self.recent_volumes.push_back((level, Instant::now()));
        }

        fn record_sent_mute(&mut self, muted: bool) {
            if self.recent_mutes.len() >= ECHO_HISTORY_CAP {
                self.recent_mutes.pop_front();
            }
            self.recent_mutes.push_back((muted, Instant::now()));
        }

        fn is_recent_volume_echo(&mut self, v: f64) -> bool {
            let now = Instant::now();
            while self
                .recent_volumes
                .front()
                .is_some_and(|(_, at)| now.duration_since(*at) >= SELF_ECHO_WINDOW)
            {
                self.recent_volumes.pop_front();
            }
            self.recent_volumes
                .iter()
                .any(|&(sent, _)| (v - sent).abs() < SELF_ECHO_EPSILON)
        }

        fn is_recent_mute_echo(&mut self, m: bool) -> bool {
            let now = Instant::now();
            while self
                .recent_mutes
                .front()
                .is_some_and(|(_, at)| now.duration_since(*at) >= SELF_ECHO_WINDOW)
            {
                self.recent_mutes.pop_front();
            }
            self.recent_mutes.iter().any(|&(sent, _)| sent == m)
        }
    }

    /// Crash-safe temporary PipeWire Virtual Null Sink (`rustygrade-direct-bridge`) that
    /// mirrors external OS/shell volume sliders (Noctalia, WirePlumber, media keys) 2-way
    /// with ALSA hardware mixers while audio itself bypasses PipeWire over direct `alsa/hw:`.
    ///
    /// Because `object.linger` is intentionally omitted, PipeWire automatically removes this
    /// node if the process exits unexpectedly or crashes.
    pub struct PipeWireBridge {
        cmd_tx: channel::Sender<BridgeCommand>,
        thread: Option<JoinHandle<()>>,
    }

    impl PipeWireBridge {
        pub fn connect(
            description: impl Into<String>,
            initial_volume: f64,
            initial_muted: bool,
        ) -> Result<(Self, mpsc::UnboundedReceiver<BridgeEvent>), String> {
            let description = description.into();
            let initial_volume = if initial_volume.is_finite() {
                initial_volume.clamp(0.0, 1.0)
            } else {
                0.0
            };
            let (event_tx, event_rx) = mpsc::unbounded_channel();
            let (cmd_tx, cmd_rx) = channel::channel::<BridgeCommand>();
            let (init_tx, init_rx) = std_mpsc::sync_channel::<Result<(), String>>(1);

            let thread = thread::Builder::new()
                .name("rustygrade-pw-bridge".to_owned())
                .spawn(move || {
                    pipewire::init();

                    let mainloop = match MainLoopRc::new(None) {
                        Ok(ml) => ml,
                        Err(e) => {
                            let _ = init_tx.send(Err(format!("PipeWire MainLoop failed: {e}")));
                            return;
                        }
                    };
                    let context = match ContextRc::new(&mainloop, None) {
                        Ok(ctx) => ctx,
                        Err(e) => {
                            let _ = init_tx.send(Err(format!("PipeWire Context failed: {e}")));
                            return;
                        }
                    };
                    let core = match context.connect_rc(None) {
                        Ok(core) => core,
                        Err(e) => {
                            let _ = init_tx
                                .send(Err(format!("PipeWire connect failed (not running?): {e}")));
                            return;
                        }
                    };

                    let props = properties! {
                        "factory.name" => "support.null-audio-sink",
                        "node.name" => NODE_NAME,
                        "node.description" => description.as_str(),
                        "node.nick" => description.as_str(),
                        "media.class" => "Audio/Sink",
                        "audio.channels" => "2",
                        "audio.position" => "[ FL FR ]",
                        "monitor.channel-volumes" => "true",
                        "monitor.passthrough" => "true",
                        "priority.session" => "2000",
                        "device.icon-name" => "audio-card",
                        "application.name" => "Rustygrade",
                    };

                    let node = match core.create_object::<Node>("adapter", &props) {
                        Ok(node) => node,
                        Err(e) => {
                            let _ = init_tx.send(Err(format!(
                                "PipeWire null-audio-sink creation failed: {e}"
                            )));
                            return;
                        }
                    };

                    let state = Rc::new(RefCell::new({
                        let mut st = BridgeState {
                            node: None,
                            channels: 2,
                            known_volume: initial_volume,
                            known_muted: initial_muted,
                            recent_volumes: VecDeque::with_capacity(ECHO_HISTORY_CAP),
                            recent_mutes: VecDeque::with_capacity(ECHO_HISTORY_CAP),
                            ready_sent: false,
                        };
                        st.record_sent_volume(initial_volume);
                        st.record_sent_mute(initial_muted);
                        st
                    }));

                    node.subscribe_params(&[ParamType::Props]);
                    node.enum_params(0, Some(ParamType::Props), 0, u32::MAX);
                    if let Some(buf) = build_props_pod(Some(initial_volume), Some(initial_muted), 2)
                        && let Some(pod) = Pod::from_bytes(&buf)
                    {
                        node.set_param(ParamType::Props, 0, pod);
                    }

                    let _node_listener: NodeListener = register_node_listener(
                        &node,
                        Rc::clone(&state),
                        description.clone(),
                        event_tx.clone(),
                    );

                    state.borrow_mut().node = Some(node);

                    let ml_quit_err = mainloop.clone();
                    let event_tx_err = event_tx.clone();
                    let disconnect_sent = Rc::new(Cell::new(false));
                    let disconnect_sent_err = Rc::clone(&disconnect_sent);
                    let _core_listener = core
                        .add_listener_local()
                        .error(move |id, _seq, res, message| {
                            warn!(
                                id,
                                res, message, "PipeWire core error on rustygrade-direct-bridge"
                            );
                            if id == 0 && !disconnect_sent_err.replace(true) {
                                let _ = event_tx_err.send(BridgeEvent::Disconnected(format!(
                                    "PipeWire error {res}: {message}"
                                )));
                                ml_quit_err.quit();
                            }
                        })
                        .register();

                    let ml_cmd = mainloop.clone();
                    let core_cmd = core.clone();
                    let state_cmd = Rc::clone(&state);
                    let _attached_rx = cmd_rx.attach(mainloop.loop_(), move |cmd| match cmd {
                        BridgeCommand::SetVolume(level) => {
                            if !level.is_finite() {
                                return;
                            }
                            let level = level.clamp(0.0, 1.0);
                            let mut st = state_cmd.borrow_mut();
                            st.known_volume = level;
                            st.record_sent_volume(level);
                            let channels = st.channels;
                            if let Some(node) = st.node.as_ref()
                                && let Some(buf) = build_props_pod(Some(level), None, channels)
                                && let Some(pod) = Pod::from_bytes(&buf)
                            {
                                node.set_param(ParamType::Props, 0, pod);
                            }
                        }
                        BridgeCommand::SetMute(muted) => {
                            let mut st = state_cmd.borrow_mut();
                            st.known_muted = muted;
                            st.record_sent_mute(muted);
                            let channels = st.channels;
                            if let Some(node) = st.node.as_ref()
                                && let Some(buf) = build_props_pod(None, Some(muted), channels)
                                && let Some(pod) = Pod::from_bytes(&buf)
                            {
                                node.set_param(ParamType::Props, 0, pod);
                            }
                        }
                        BridgeCommand::Shutdown => {
                            if let Some(node) = state_cmd.borrow_mut().node.take() {
                                let _ = core_cmd.destroy_object(node);
                            }
                            ml_cmd.quit();
                        }
                    });

                    let _ = init_tx.send(Ok(()));
                    mainloop.run();

                    // Ensure remote object is gone if the loop exited unexpectedly.
                    if let Some(node) = state.borrow_mut().node.take() {
                        let _ = core.destroy_object(node);
                    }
                    drop(_node_listener);
                    drop(_core_listener);
                    drop(_attached_rx);
                    drop(core);
                    drop(context);
                    drop(mainloop);
                    if !disconnect_sent.get() {
                        let _ = event_tx.send(BridgeEvent::Disconnected("Stopped".to_owned()));
                    }
                })
                .map_err(|e| format!("Failed to spawn PipeWire bridge thread: {e}"))?;

            match init_rx.recv() {
                Ok(Ok(())) => Ok((
                    Self {
                        cmd_tx,
                        thread: Some(thread),
                    },
                    event_rx,
                )),
                Ok(Err(err)) => {
                    let _ = thread.join();
                    Err(err)
                }
                Err(_) => {
                    let _ = thread.join();
                    Err("PipeWire bridge thread exited during initialization".to_owned())
                }
            }
        }

        pub fn set_volume(&self, level: f64) {
            let _ = self.cmd_tx.send(BridgeCommand::SetVolume(level));
        }

        pub fn set_mute(&self, muted: bool) {
            let _ = self.cmd_tx.send(BridgeCommand::SetMute(muted));
        }

        pub fn shutdown(&mut self) {
            let _ = self.cmd_tx.send(BridgeCommand::Shutdown);
            if let Some(handle) = self.thread.take() {
                let _ = handle.join();
            }
        }
    }

    impl Drop for PipeWireBridge {
        fn drop(&mut self) {
            self.shutdown();
        }
    }

    fn register_node_listener(
        node: &Node,
        state: Rc<RefCell<BridgeState>>,
        description: String,
        event_tx: mpsc::UnboundedSender<BridgeEvent>,
    ) -> NodeListener {
        let state_info = Rc::clone(&state);
        let event_tx_info = event_tx.clone();
        let node_proxy_id = node.upcast_ref().id();

        node.add_listener_local()
            .info(move |info| {
                let bound_id = info.id();
                if bound_id != 0 && bound_id != u32::MAX {
                    let mut st = state_info.borrow_mut();
                    if !st.ready_sent {
                        st.ready_sent = true;
                        debug!(
                            proxy_id = node_proxy_id,
                            global_id = bound_id,
                            description,
                            "PipeWire virtual bridge sink bound"
                        );
                        let _ = event_tx_info.send(BridgeEvent::Ready {
                            node_id: bound_id,
                            description: description.clone(),
                        });
                    }
                }
            })
            .param(move |_seq, param_type, _index, _next, pod| {
                if param_type != ParamType::Props {
                    return;
                }
                let Some(pod) = pod else {
                    return;
                };
                let (vol, muted, ch) = parse_props_pod(pod);
                let mut st = state.borrow_mut();
                if let Some(ch) = ch {
                    st.channels = ch.max(1);
                }
                if let Some(v) = vol {
                    let suppressed = st.is_recent_volume_echo(v);
                    if !suppressed && (v - st.known_volume).abs() >= SELF_ECHO_EPSILON {
                        st.known_volume = v;
                        let _ = event_tx.send(BridgeEvent::VolumeChanged(v));
                    }
                }
                if let Some(m) = muted {
                    let suppressed = st.is_recent_mute_echo(m);
                    if !suppressed && m != st.known_muted {
                        st.known_muted = m;
                        let _ = event_tx.send(BridgeEvent::MuteChanged(m));
                    }
                }
            })
            .register()
    }

    fn build_props_pod(
        volume: Option<f64>,
        mute: Option<bool>,
        channels: usize,
    ) -> Option<Vec<u8>> {
        let mut properties = Vec::new();
        if let Some(level) = volume {
            let pw = linear_to_pw_cubic(level);
            let ch = channels.clamp(1, 8);
            properties.push(Property::new(
                libspa_sys::SPA_PROP_channelVolumes,
                Value::ValueArray(ValueArray::Float(vec![pw; ch])),
            ));
        }
        if let Some(muted) = mute {
            properties.push(Property::new(libspa_sys::SPA_PROP_mute, Value::Bool(muted)));
        }
        if properties.is_empty() {
            return None;
        }
        let obj = Value::Object(Object {
            type_: SpaTypes::ObjectParamProps.as_raw(),
            id: ParamType::Props.as_raw(),
            properties,
        });
        let mut buf = Vec::with_capacity(128);
        PodSerializer::serialize(Cursor::new(&mut buf), &obj).ok()?;
        Some(buf)
    }

    fn parse_props_pod(pod: &Pod) -> (Option<f64>, Option<bool>, Option<usize>) {
        let Ok((_, Value::Object(obj))) = PodDeserializer::deserialize_any_from(pod.as_bytes())
        else {
            return (None, None, None);
        };
        let mut channel_vol: Option<(f64, usize)> = None;
        let mut scalar_vol: Option<f64> = None;
        let mut muted: Option<bool> = None;

        for prop in obj.properties {
            if prop.key == libspa_sys::SPA_PROP_channelVolumes {
                if let Value::ValueArray(ValueArray::Float(vols)) = prop.value
                    && !vols.is_empty()
                {
                    let sum: f64 = vols.iter().map(|&v| pw_cubic_to_linear(v)).sum();
                    channel_vol = Some((sum / vols.len() as f64, vols.len()));
                }
            } else if prop.key == libspa_sys::SPA_PROP_volume {
                if let Value::Float(v) = prop.value {
                    scalar_vol = Some(pw_cubic_to_linear(v));
                }
            } else if prop.key == libspa_sys::SPA_PROP_mute
                && let Value::Bool(m) = prop.value
            {
                muted = Some(m);
            }
        }

        let (vol, ch) = match channel_vol {
            Some((v, c)) => (Some(v), Some(c)),
            None => (scalar_vol, None),
        };
        (vol, muted, ch)
    }
}

#[cfg(not(feature = "pipewire-bridge"))]
mod imp {
    use tokio::sync::mpsc;

    use super::BridgeEvent;

    /// No-op stub compiled when `--no-default-features` disables `pipewire-bridge`
    /// (for minimal PulseAudio-only, pure-ALSA, or headless builds without `libpipewire-0.3`).
    pub struct PipeWireBridge;

    impl PipeWireBridge {
        pub fn connect(
            _description: impl Into<String>,
            _initial_volume: f64,
            _initial_muted: bool,
        ) -> Result<(Self, mpsc::UnboundedReceiver<BridgeEvent>), String> {
            Err("PipeWire bridge support was disabled at compile time".to_owned())
        }

        pub fn set_volume(&self, _level: f64) {}

        pub fn set_mute(&self, _muted: bool) {}

        pub fn shutdown(&mut self) {}
    }
}

pub use imp::PipeWireBridge;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cubic_volume_roundtrips_and_matches_pw_math() {
        let pw = linear_to_pw_cubic(0.35);
        assert!((pw - 0.042875).abs() < 1e-6);
        assert!((pw_cubic_to_linear(pw) - 0.35).abs() < 1e-6);

        let pw70 = linear_to_pw_cubic(0.70);
        assert!((pw70 - 0.343).abs() < 1e-6);
        assert!((pw_cubic_to_linear(pw70) - 0.70).abs() < 1e-6);

        assert_eq!(linear_to_pw_cubic(-0.5), 0.0);
        assert_eq!(linear_to_pw_cubic(1.5), 1.0);
        assert_eq!(linear_to_pw_cubic(f64::NAN), 0.0);
        assert_eq!(linear_to_pw_cubic(f64::INFINITY), 0.0);
        assert_eq!(pw_cubic_to_linear(f32::NAN), 0.0);
        assert_eq!(pw_cubic_to_linear(-0.1), 0.0);
    }
}
