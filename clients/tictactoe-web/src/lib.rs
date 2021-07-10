use anyhow::{bail, Result};
use futures::channel::mpsc::{channel, Receiver, Sender};
use futures::prelude::*;
use js_sys::Error;
use libp2p::gossipsub::{Gossipsub, GossipsubConfig, GossipsubEvent, MessageAuthenticity};
use libp2p::identify::{Identify, IdentifyConfig, IdentifyEvent};
use libp2p::ping::{Ping, PingEvent};
use libp2p::relay::{Relay, RelayConfig};
use libp2p::swarm::{Swarm, SwarmEvent, NetworkBehaviour, ProtocolsHandler, IntoProtocolsHandler};
use libp2p::{
    core, identity, mplex, multiaddr::Protocol, Multiaddr, NetworkBehaviour, PeerId, Transport,
};
use libp2p_noise::{Keypair, NoiseConfig, X25519Spec};
use libp2p_wasm_ext::{ffi, ExtTransport};
use libp2p_yamux::YamuxConfig;
use log::{debug, error, info, warn, Level};
use once_cell::sync::OnceCell;
use std::fmt::Debug;
use std::str::FromStr;
use std::time::Duration;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsValue;

static GLOBAL_SENDER: OnceCell<Sender<Command>> = OnceCell::new();
const BOOTSTRAP_ADDRS : [&'static str; 4] = [
    "/dns6/ipfs.thedisco.zone/tcp/4430/wss/p2p/12D3KooWChhhfGdB9GJy1GbhghAAKCUR99oCymMEVS4eUcEy67nt",
    "/dns4/ipfs.thedisco.zone/tcp/4430/wss/p2p/12D3KooWChhhfGdB9GJy1GbhghAAKCUR99oCymMEVS4eUcEy67nt",
    "/dns6/ipfs.v6.fusetim.tk/tcp/443/wss/p2p/12D3KooWSaji41rvmVofTTKbth4x6VRu8SzJ9AnazsCrEJFHUxh2",
    "/dns6/ipfs.thedisco.zone/tcp/4430/wss/p2p/12D3KooWChhhfGdB9GJy1GbhghAAKCUR99oCymMEVS4eUcEy67nt/p2p-circuit/p2p/12D3KooWSaji41rvmVofTTKbth4x6VRu8SzJ9AnazsCrEJFHUxh2"
];

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen(start)]
pub fn main() {
    let _ = console_log::init_with_level(Level::Debug);
    info!("Initialization...");
    info!("Registering console log and panic hook...");
    ::console_error_panic_hook::set_once();
    info!("WASM Module fully initialized and operational!");
}

#[wasm_bindgen]
pub async fn start() -> Result<(), JsValue> {
    convert_result(inner_start().await)
}

#[wasm_bindgen]
pub async fn stop() -> Result<(), JsValue> {
    async fn inner_stop() -> Result<()> {
        let mut sender = GLOBAL_SENDER
            .get()
            .ok_or(anyhow::Error::msg(
                "Command channel is uninitialized! Please run start() before.",
            ))?
            .clone();
        sender.try_send(Command::Stop)?;
        sender.close_channel();
        Ok(())
    }
    convert_result(inner_stop().await)
}

#[wasm_bindgen]
pub async fn bootstrap() -> Result<(), JsValue> {
    async fn inner_bootstrap() -> Result<()> {
        let mut sender = GLOBAL_SENDER
            .get()
            .ok_or(anyhow::Error::msg(
                "Command channel is uninitialized! Please run start() before.",
            ))?
            .clone();
        sender.try_send(Command::Bootstrap)?;
        Ok(())
    }
    convert_result(inner_bootstrap().await)
}

fn convert_result<T, E: Debug>(result: Result<T, E>) -> Result<T, JsValue> {
    result.map_err(|err| Error::new(&format!("WASM Internal Error: {:?}", err)).into())
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Command {
    Bootstrap,
    Stop,
}

#[derive(NetworkBehaviour)]
#[behaviour(out_event = "Event", event_process = false)]
struct ClientBehavior {
    gossipsub: Gossipsub,
    identify: Identify,
    relay: Relay,
    ping: Ping,
}

#[derive(Debug)]
enum Event {
    Relay(()),
    Ping(PingEvent),
    Identify(IdentifyEvent),
    Gossipsub(GossipsubEvent),
}

impl From<IdentifyEvent> for Event {
    fn from(e: IdentifyEvent) -> Self {
        Event::Identify(e)
    }
}

impl From<PingEvent> for Event {
    fn from(e: PingEvent) -> Self {
        Event::Ping(e)
    }
}

impl From<GossipsubEvent> for Event {
    fn from(e: GossipsubEvent) -> Self {
        Event::Gossipsub(e)
    }
}

impl From<()> for Event {
    fn from(_: ()) -> Self {
        Event::Relay(())
    }
}

async fn inner_start() -> Result<()> {
    info!("Heyy starting...");
    let node_keys = identity::Keypair::generate_secp256k1();
    info!("Key generated!");
    let node_id = PeerId::from(node_keys.public());
    info!("Node peer id: {:?}", node_id);

    // Setting up Network behavior (and gossipsub)
    let gossipsub_config = GossipsubConfig::default();
    let message_authenticity = MessageAuthenticity::Signed(node_keys.clone());
    let gossipsub: Gossipsub = Gossipsub::new(message_authenticity, gossipsub_config).unwrap();

    let (transport, relay_behavior) = setup_transport(&node_keys).await?;

    let behavior = ClientBehavior {
        relay: relay_behavior,
        ping: Ping::default(),
        identify: Identify::new(
            IdentifyConfig::new("ipfs/1.0.0".to_owned(), node_keys.public())
                .with_agent_version("fusetim/tictactoe/0.1.0".to_owned()),
        ),
        gossipsub,
    };

    // Create the final swarm
    let mut swarm = Swarm::new(transport.boxed(), behavior, node_id.clone());

    info!("Listening on /p2p-circuit...");
    // Listen on relayed connections
    let addr = Multiaddr::empty().with(Protocol::P2pCircuit); // Signal to listen via any relay.
    swarm.listen_on(addr)?;

    // Setting up MPSC channel for commands
    let (tx, mut rx) = channel(10);
    if let Some(_) = GLOBAL_SENDER.get() {
        bail!("Already initialized command channel! start() should be run only 1 times!!!")
    };
    GLOBAL_SENDER.set(tx).unwrap();

    // Polling events & commands
    let mut terminate = false;
    while !terminate {
        terminate = futures::select! {
            event = swarm.next_event().fuse() => handle_swarm_event(&mut swarm, event).await?,
            command = rx.next().fuse() => if let Some(cmd) = command {handle_command(&mut swarm, cmd).await? } else { false },
        };
    }
    Ok(())
}


async fn handle_swarm_event(_swarm: &mut Swarm<ClientBehavior>, event: SwarmEvent<<ClientBehavior as NetworkBehaviour>::OutEvent, <<<ClientBehavior as NetworkBehaviour>::ProtocolsHandler as IntoProtocolsHandler>::Handler as ProtocolsHandler>::Error>) -> Result<bool> {
    debug!("Recieved event: {:?}", event);
    Ok(false)
}

async fn handle_command(swarm: &mut Swarm<ClientBehavior>, cmd: Command) -> Result<bool> {
    debug!("Recieved command: {:?}", cmd);
    match cmd {
        Command::Bootstrap => {
            info!("Bootstraping...");
            for addr in &BOOTSTRAP_ADDRS {
                info!("Dialing to {}...", addr);
                match Multiaddr::from_str(addr) {
                    Ok(multiaddr) => match swarm.dial_addr(multiaddr) {
                        Ok(()) => info!("Connected to {}...", addr),
                        Err(err) => error!("Connection to {} failed, error: {:?}", addr, err),
                    },
                    Err(err) => error!("Bad multiaddr, error: {:?}", err),
                }
            }
        }
        Command::Stop => {
            info!("Shuting down...");
            return Ok(true);
        }
    };
    Ok(false)
}

async fn setup_transport(
    keys: &identity::Keypair,
) -> Result<(
    core::transport::Boxed<(PeerId, core::muxing::StreamMuxerBox)>,
    Relay,
)> {
    let dh_keys = Keypair::<X25519Spec>::new().into_authentic(keys)?;
    let noise = NoiseConfig::xx(dh_keys).into_authenticated();

    // Get websocket transport
    let transport = core::transport::timeout::TransportTimeout::new(
        ExtTransport::new(ffi::websocket_transport()),
        std::time::Duration::from_secs(10),
    );
    // Enable relayed connections
    let relay_config = RelayConfig {
        connection_idle_timeout: Duration::from_secs(10 * 60),
        ..Default::default()
    };

    let (relay_wrapped_transport, relay_behavior) =
        libp2p::relay::new_transport_and_behaviour(relay_config, transport.clone());

    // Upgrade transport
    Ok((
        relay_wrapped_transport
            .upgrade(core::upgrade::Version::V1)
            .authenticate(noise.clone())
            .multiplex(core::upgrade::SelectUpgrade::new(
                YamuxConfig::default(),
                mplex::MplexConfig::new(),
            ))
            .map(|(peer, muxer), _| (peer, core::muxing::StreamMuxerBox::new(muxer)))
            .boxed(),
        relay_behavior,
    ))
}
