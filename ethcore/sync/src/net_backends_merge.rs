// Copyright 2018 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

//! This module merges devp2p and libp2p into one struct.
//!
//! The way it works is that PeerIds between `0` and `max_value() / 2` belong to devp2p, and
//! PeerIds between `max_value() / 2` and `max_value()` belong to libp2p.
//!
//! In order to convert between a libp2p ID and an exposed ID, we simply invert the bits.

use std::sync::{Arc, Weak};
use std::collections::HashMap;
use devp2p::NetworkService as Devp2pNetworkService;
use libp2p::NetworkService as Libp2pNetworkService;
use network::{NetworkProtocolHandler, NetworkContext, HostInfo, PeerId, ProtocolId, PacketId,
	NetworkConfiguration as BasicNetworkConfiguration, NonReservedPeerMode, Error, ErrorKind,
	ConnectionFilter, NodeId, SessionInfo};
use io::{TimerToken};
use ethcore::ethstore::ethkey::Secret;
use std::net::{SocketAddr, AddrParseError};
use std::str::FromStr;
use network::IpFilter;
use api::WARP_SYNC_PROTOCOL_ID;

#[derive(Debug, Clone, PartialEq, Eq)]
/// Network service configuration
pub struct NetworkConfiguration {
	/// Directory path to store general network configuration. None means nothing will be saved
	pub config_path: Option<String>,
	/// Directory path to store network-specific configuration. None means nothing will be saved
	pub net_config_path: Option<String>,
	/// IP address to listen for incoming connections. Listen to all connections by default
	pub listen_address: Option<String>,
	/// IP address to advertise. Detected automatically if none.
	pub public_address: Option<String>,
	/// Port for UDP connections, same as TCP by default
	pub udp_port: Option<u16>,
	/// Enable NAT configuration
	pub nat_enabled: bool,
	/// Enable discovery
	pub discovery_enabled: bool,
	/// List of initial node addresses
	pub boot_nodes: Vec<String>,
	/// Use provided node key instead of default
	pub use_secret: Option<Secret>,
	/// Max number of connected peers to maintain
	pub max_peers: u32,
	/// Min number of connected peers to maintain
	pub min_peers: u32,
	/// Max pending peers.
	pub max_pending_peers: u32,
	/// Reserved snapshot sync peers.
	pub snapshot_peers: u32,
	/// List of reserved node addresses.
	pub reserved_nodes: Vec<String>,
	/// The non-reserved peer mode.
	pub allow_non_reserved: bool,
	/// IP Filtering
	pub ip_filter: IpFilter,
	/// Client version string
	pub client_version: String,
}

impl NetworkConfiguration {
	/// Create a new default config.
	pub fn new() -> Self {
		From::from(BasicNetworkConfiguration::new())
	}

	/// Create a new local config.
	pub fn new_local() -> Self {
		From::from(BasicNetworkConfiguration::new_local())
	}

	/// Attempt to convert this config into a BasicNetworkConfiguration.
	pub fn into_basic(self) -> Result<BasicNetworkConfiguration, AddrParseError> {
		Ok(BasicNetworkConfiguration {
			config_path: self.config_path,
			net_config_path: self.net_config_path,
			listen_address: match self.listen_address { None => None, Some(addr) => Some(SocketAddr::from_str(&addr)?) },
			public_address:  match self.public_address { None => None, Some(addr) => Some(SocketAddr::from_str(&addr)?) },
			udp_port: self.udp_port,
			nat_enabled: self.nat_enabled,
			discovery_enabled: self.discovery_enabled,
			boot_nodes: self.boot_nodes,
			use_secret: self.use_secret,
			max_peers: self.max_peers,
			min_peers: self.min_peers,
			max_handshakes: self.max_pending_peers,
			reserved_protocols: hash_map![WARP_SYNC_PROTOCOL_ID => self.snapshot_peers],
			reserved_nodes: self.reserved_nodes,
			ip_filter: self.ip_filter,
			non_reserved_mode: if self.allow_non_reserved { NonReservedPeerMode::Accept } else { NonReservedPeerMode::Deny },
			client_version: self.client_version,
		})
	}
}

impl From<BasicNetworkConfiguration> for NetworkConfiguration {
	fn from(other: BasicNetworkConfiguration) -> Self {
		NetworkConfiguration {
			config_path: other.config_path,
			net_config_path: other.net_config_path,
			listen_address: other.listen_address.and_then(|addr| Some(format!("{}", addr))),
			public_address: other.public_address.and_then(|addr| Some(format!("{}", addr))),
			udp_port: other.udp_port,
			nat_enabled: other.nat_enabled,
			discovery_enabled: other.discovery_enabled,
			boot_nodes: other.boot_nodes,
			use_secret: other.use_secret,
			max_peers: other.max_peers,
			min_peers: other.min_peers,
			max_pending_peers: other.max_handshakes,
			snapshot_peers: *other.reserved_protocols.get(&WARP_SYNC_PROTOCOL_ID).unwrap_or(&0),
			reserved_nodes: other.reserved_nodes,
			ip_filter: other.ip_filter,
			allow_non_reserved: match other.non_reserved_mode { NonReservedPeerMode::Accept => true, _ => false } ,
			client_version: other.client_version,
		}
	}
}

/// Unification of devp2p and libp2p.
pub struct NetBackend {
	inner: Arc<NetBackendInner>,
}

enum NetBackendInner {
	Devp2p(Devp2pNetworkService),
	Libp2p(Libp2pNetworkService),
	Both(Devp2pNetworkService, Libp2pNetworkService),
}

impl NetBackendInner {
	#[inline]
	fn devp2p(&self) -> Option<&Devp2pNetworkService> {
		match *self {
			NetBackendInner::Devp2p(ref devp2p) => Some(devp2p),
			NetBackendInner::Libp2p(_) => None,
			NetBackendInner::Both(ref devp2p, _) => Some(devp2p),
		}
	}

	#[inline]
	fn libp2p(&self) -> Option<&Libp2pNetworkService> {
		match *self {
			NetBackendInner::Devp2p(_) => None,
			NetBackendInner::Libp2p(ref libp2p) => Some(libp2p),
			NetBackendInner::Both(_, ref libp2p) => Some(libp2p),
		}
	}
}

impl NetBackend {
	/// Starts IO event loop
	pub fn new(devp2p_config: Option<NetworkConfiguration>, libp2p_config: Option<NetworkConfiguration>, connection_filter: Option<Arc<ConnectionFilter>>) -> Result<NetBackend, Error> {
		let inner = match (devp2p_config, libp2p_config) {
			(Some(devp2p), Some(libp2p)) => {
				let devp2p = Devp2pNetworkService::new(devp2p.into_basic()?, connection_filter.clone())?;
				let libp2p = Libp2pNetworkService::new(libp2p.into_basic()?, connection_filter.clone())?;
				NetBackendInner::Both(devp2p, libp2p)
			},
			(Some(devp2p), None) => {
				let devp2p = Devp2pNetworkService::new(devp2p.into_basic()?, connection_filter)?;
				NetBackendInner::Devp2p(devp2p)
			},
			(None, Some(libp2p)) => {
				let libp2p = Libp2pNetworkService::new(libp2p.into_basic()?, connection_filter)?;
				NetBackendInner::Libp2p(libp2p)
			},
			(None, None) => {
				// TODO:
				panic!()
			},
		};

		Ok(NetBackend { inner: Arc::new(inner) })
	}

	/// Register a new protocol handler with the event loop.
	pub fn register_protocol(&self, handler: Arc<NetworkProtocolHandler + Send + Sync>, protocol: ProtocolId, packet_count: u8, versions: &[u8]) -> Result<(), Error> {
		match *self.inner {
			NetBackendInner::Devp2p(ref net) => {
				net.register_protocol(Arc::new(Devp2pHandlerWrapper(handler, Arc::downgrade(&self.inner))), protocol, packet_count, versions)
			},
			NetBackendInner::Libp2p(ref net) => {
				net.register_protocol(Arc::new(Libp2pHandlerWrapper(handler, Arc::downgrade(&self.inner))), protocol, packet_count, versions)
			},
			NetBackendInner::Both(ref devp2p, ref libp2p) => {
				// TODO: what to do in case of error?
				// TODO: initialize called twice?
				devp2p.register_protocol(Arc::new(Devp2pHandlerWrapper(handler.clone(), Arc::downgrade(&self.inner))), protocol, packet_count, versions)?;
				libp2p.register_protocol(Arc::new(Libp2pHandlerWrapper(handler, Arc::downgrade(&self.inner))), protocol, packet_count, versions)?;
				Ok(())
			},
		}
	}

	/// Returns network configuration.
	pub fn config(&self) -> &BasicNetworkConfiguration {
		match *self.inner {
			NetBackendInner::Devp2p(ref net) => net.config(),
			NetBackendInner::Libp2p(ref net) => net.config(),
			NetBackendInner::Both(ref devp2p, _) => {
				devp2p.config()		// TODO: ?
			},
		}
	}

	/// Start network IO
	pub fn start(&self) -> Result<(), Error> {
		match *self.inner {
			NetBackendInner::Devp2p(ref net) => net.start(),
			NetBackendInner::Libp2p(ref net) => net.start(),
			NetBackendInner::Both(ref devp2p, ref libp2p) => {
				devp2p.start()?;
				match libp2p.start() {
					Ok(()) => Ok(()),
					Err(err) => {
						devp2p.stop();  // TODO: what to do in case of error?
						Err(err)
					},
				}
			},
		}
	}

	/// Stop network IO
	pub fn stop(&self) -> Result<(), Error> {
		match *self.inner {
			NetBackendInner::Devp2p(ref net) => net.stop(),
			NetBackendInner::Libp2p(ref net) => net.stop(),
			NetBackendInner::Both(ref devp2p, ref libp2p) => {
				// TODO: revert stopping? what do we do here? can be just remove the `Result` entirely?
				devp2p.stop()?;
				libp2p.stop()
			},
		}
	}

	/// Get a list of all connected peers by id.
	pub fn connected_peers(&self) -> Vec<PeerId> {
		let devp2p = self.inner.devp2p().map(|n| n.connected_peers()).unwrap_or(Vec::new()).into_iter()
			.map(|p| peer_id_int_to_ext(InternalPeerId::Devp2p(p)));
		let libp2p = self.inner.libp2p().map(|n| n.connected_peers()).unwrap_or(Vec::new()).into_iter()
			.map(|p| peer_id_int_to_ext(InternalPeerId::Libp2p(p)));
		devp2p.chain(libp2p).collect()
	}

	/// Try to add a reserved peer.
	pub fn add_reserved_peer(&self, peer: &str) -> Result<(), Error> {
		match *self.inner {
			NetBackendInner::Devp2p(ref net) => net.add_reserved_peer(peer),
			NetBackendInner::Libp2p(ref net) => net.add_reserved_peer(peer),
			NetBackendInner::Both(ref devp2p, ref libp2p) => {
				// TODO: revert in case of error?
				devp2p.add_reserved_peer(peer)?;
				libp2p.add_reserved_peer(peer)?;
				Ok(())
			},
		}
	}

	/// Try to remove a reserved peer.
	pub fn remove_reserved_peer(&self, peer: &str) -> Result<(), Error> {
		match *self.inner {
			NetBackendInner::Devp2p(ref net) => net.remove_reserved_peer(peer),
			NetBackendInner::Libp2p(ref net) => net.remove_reserved_peer(peer),
			NetBackendInner::Both(ref devp2p, ref libp2p) => {
				// TODO: revert in case of error?
				devp2p.remove_reserved_peer(peer)?;
				libp2p.remove_reserved_peer(peer)?;
				Ok(())
			},
		}
	}

	/// Set the non-reserved peer mode.
	pub fn set_non_reserved_mode(&self, mode: NonReservedPeerMode) {
		match *self.inner {
			NetBackendInner::Devp2p(ref net) => net.set_non_reserved_mode(mode),
			NetBackendInner::Libp2p(ref net) => net.set_non_reserved_mode(mode),
			NetBackendInner::Both(ref devp2p, ref libp2p) => {
				devp2p.set_non_reserved_mode(mode.clone());
				libp2p.set_non_reserved_mode(mode);
			},
		}
	}

	/// Returns external url if available.
	pub fn external_url(&self) -> Option<String> {
		match *self.inner {
			NetBackendInner::Devp2p(ref net) => net.external_url(),
			NetBackendInner::Libp2p(ref net) => None,// TODO: net.external_url(),
			NetBackendInner::Both(ref devp2p, _) => {
				devp2p.external_url()		// TODO: ?
			},
		}
	}

	/// Executes action in the network context
	pub fn with_context<F>(&self, protocol: ProtocolId, action: F) where F: FnOnce(&NetworkContext) {
		self.with_context_eval(protocol, action);
	}

	/// Evaluates function in the network context
	pub fn with_context_eval<F, T>(&self, protocol: ProtocolId, action: F) -> Option<T> where F: FnOnce(&NetworkContext) -> T {
		unimplemented!()
	}
}

struct DummyHostInfo;
impl HostInfo for DummyHostInfo {
	fn id(&self) -> &NodeId {
		unimplemented!()
	}

	fn secret(&self) -> &Secret {
		unimplemented!()
	}

	fn client_version(&self) -> &str {
		unimplemented!()
	}
}

pub struct MergedNetworkContext<'a> {
	devp2p: Option<&'a Devp2pNetworkService>,
	libp2p: Option<&'a Libp2pNetworkService>,
	context: MergedNetworkContextInner<'a>,
}

enum MergedNetworkContextInner<'a> {
	Devp2p(&'a NetworkContext),
	Libp2p(&'a NetworkContext),
}

impl<'a> NetworkContext for MergedNetworkContext<'a> {
	fn send(&self, peer: PeerId, packet_id: PacketId, data: Vec<u8>) -> Result<(), Error> {
		match (peer_id_ext_to_int(peer), &self.context) {
			(InternalPeerId::Devp2p(peer), &MergedNetworkContextInner::Devp2p(ref ctxt)) => {
				ctxt.send(peer, packet_id, data)
			},
			(InternalPeerId::Libp2p(peer), &MergedNetworkContextInner::Libp2p(ref ctxt)) => {
				ctxt.send(peer, packet_id, data)
			},
			(InternalPeerId::Devp2p(peer), _) => {
				if let Some(ref net) = self.devp2p {
					net.with_context_eval(self.subprotocol_name(), |ctxt| ctxt.send(peer, packet_id, data))
						.expect("err")		// TODO: error
				} else {
					panic!()		// TODO: error
				}
			},
			(InternalPeerId::Libp2p(peer), _) => {
				if let Some(ref net) = self.libp2p {
					net.with_context_eval(self.subprotocol_name(), |ctxt| ctxt.send(peer, packet_id, data))
						.expect("err")		// TODO: error
				} else {
					panic!()		// TODO: error
				}
			},
		}
	}

	fn send_protocol(&self, protocol: ProtocolId, peer: PeerId, packet_id: PacketId, data: Vec<u8>) -> Result<(), Error> {
		match (peer_id_ext_to_int(peer), &self.context) {
			(InternalPeerId::Devp2p(peer), &MergedNetworkContextInner::Devp2p(ctxt)) => {
				ctxt.send_protocol(protocol, peer, packet_id, data)
			},
			(InternalPeerId::Libp2p(peer), &MergedNetworkContextInner::Libp2p(ctxt)) => {
				ctxt.send_protocol(protocol, peer, packet_id, data)
			},
			(InternalPeerId::Devp2p(peer), _) => {
				if let Some(ref net) = self.devp2p {
					net.with_context_eval(self.subprotocol_name(), |ctxt| ctxt.send_protocol(protocol, peer, packet_id, data))
						.expect("err2")		// TODO: error
				} else {
					panic!()		// TODO: error
				}
			},
			(InternalPeerId::Libp2p(peer), _) => {
				if let Some(ref net) = self.libp2p {
					net.with_context_eval(self.subprotocol_name(), |ctxt| ctxt.send_protocol(protocol, peer, packet_id, data))
						.expect("err2")		// TODO: error
				} else {
					panic!()		// TODO: error
				}
			},
		}
	}

	fn respond(&self, packet_id: PacketId, data: Vec<u8>) -> Result<(), Error> {
		match self.context {
			MergedNetworkContextInner::Devp2p(ref net) => net.respond(packet_id, data),
			MergedNetworkContextInner::Libp2p(ref net) => net.respond(packet_id, data),
		}
	}

	fn disable_peer(&self, peer: PeerId) {
		match (peer_id_ext_to_int(peer), &self.context) {
			(InternalPeerId::Devp2p(peer), &MergedNetworkContextInner::Devp2p(ref ctxt)) => {
				ctxt.disable_peer(peer)
			},
			(InternalPeerId::Libp2p(peer), &MergedNetworkContextInner::Libp2p(ref ctxt)) => {
				ctxt.disable_peer(peer)
			},
			(InternalPeerId::Devp2p(peer), _) => {
				if let Some(ref net) = self.devp2p {
					net.with_context(self.subprotocol_name(), |ctxt| ctxt.disable_peer(peer));
				}
			},
			(InternalPeerId::Libp2p(peer), _) => {
				if let Some(ref net) = self.libp2p {
					net.with_context(self.subprotocol_name(), |ctxt| ctxt.disable_peer(peer));
				}
			},
		}
	}

	fn disconnect_peer(&self, peer: PeerId) {
		match (peer_id_ext_to_int(peer), &self.context) {
			(InternalPeerId::Devp2p(peer), &MergedNetworkContextInner::Devp2p(ref ctxt)) => {
				ctxt.disconnect_peer(peer)
			},
			(InternalPeerId::Libp2p(peer), &MergedNetworkContextInner::Libp2p(ref ctxt)) => {
				ctxt.disconnect_peer(peer)
			},
			(InternalPeerId::Devp2p(peer), _) => {
				if let Some(ref net) = self.devp2p {
					net.with_context(self.subprotocol_name(), |ctxt| ctxt.disconnect_peer(peer));
				}
			},
			(InternalPeerId::Libp2p(peer), _) => {
				if let Some(ref net) = self.libp2p {
					net.with_context(self.subprotocol_name(), |ctxt| ctxt.disconnect_peer(peer));
				}
			},
		}
	}

	fn is_expired(&self) -> bool {
		match self.context {
			MergedNetworkContextInner::Devp2p(ref net) => net.is_expired(),
			MergedNetworkContextInner::Libp2p(ref net) => net.is_expired(),
		}
	}

	fn register_timer(&self, token: TimerToken, ms: u64) -> Result<(), Error> {
		match self.context {
			MergedNetworkContextInner::Devp2p(ref net) => net.register_timer(token, ms),
			MergedNetworkContextInner::Libp2p(ref net) => net.register_timer(token, ms),
		}
	}

	fn peer_client_version(&self, peer: PeerId) -> String {
		match (peer_id_ext_to_int(peer), &self.context) {
			(InternalPeerId::Devp2p(peer), &MergedNetworkContextInner::Devp2p(ref ctxt)) => {
				ctxt.peer_client_version(peer)
			},
			(InternalPeerId::Libp2p(peer), &MergedNetworkContextInner::Libp2p(ref ctxt)) => {
				ctxt.peer_client_version(peer)
			},
			(InternalPeerId::Devp2p(peer), _) => {
				if let Some(ref net) = self.devp2p {
					net.with_context_eval(self.subprotocol_name(), |ctxt| ctxt.peer_client_version(peer))
						.unwrap_or_default()		// TODO: is that correct?
				} else {
					String::new()		// TODO: is that correct?
				}
			},
			(InternalPeerId::Libp2p(peer), _) => {
				if let Some(ref net) = self.libp2p {
					net.with_context_eval(self.subprotocol_name(), |ctxt| ctxt.peer_client_version(peer))
						.unwrap_or_default()		// TODO: is that correct?
				} else {
					String::new()		// TODO: is that correct?
				}
			},
		}
	}

	fn session_info(&self, peer: PeerId) -> Option<SessionInfo> {
		match (peer_id_ext_to_int(peer), &self.context) {
			(InternalPeerId::Devp2p(peer), &MergedNetworkContextInner::Devp2p(ref ctxt)) => {
				ctxt.session_info(peer)
			},
			(InternalPeerId::Libp2p(peer), &MergedNetworkContextInner::Libp2p(ref ctxt)) => {
				ctxt.session_info(peer)
			},
			(InternalPeerId::Devp2p(peer), _) => {
				if let Some(ref net) = self.devp2p {
					net.with_context_eval(self.subprotocol_name(), |ctxt| ctxt.session_info(peer))
						.and_then(|v| v)
				} else {
					None
				}
			},
			(InternalPeerId::Libp2p(peer), _) => {
				if let Some(ref net) = self.libp2p {
					net.with_context_eval(self.subprotocol_name(), |ctxt| ctxt.session_info(peer))
						.and_then(|v| v)
				} else {
					None
				}
			},
		}
	}

	fn protocol_version(&self, protocol: ProtocolId, peer: PeerId) -> Option<u8> {
		match (peer_id_ext_to_int(peer), &self.context) {
			(InternalPeerId::Devp2p(peer), &MergedNetworkContextInner::Devp2p(ctxt)) => {
				ctxt.protocol_version(protocol, peer)
			},
			(InternalPeerId::Libp2p(peer), &MergedNetworkContextInner::Libp2p(ctxt)) => {
				ctxt.protocol_version(protocol, peer)
			},
			(InternalPeerId::Devp2p(peer), _) => {
				if let Some(ref net) = self.devp2p {
					net.with_context_eval(self.subprotocol_name(), |ctxt| ctxt.protocol_version(protocol, peer))
						.and_then(|v| v)
				} else {
					None
				}
			},
			(InternalPeerId::Libp2p(peer), _) => {
				if let Some(ref net) = self.libp2p {
					net.with_context_eval(self.subprotocol_name(), |ctxt| ctxt.protocol_version(protocol, peer))
						.and_then(|v| v)
				} else {
					None
				}
			},
		}
	}

	fn subprotocol_name(&self) -> ProtocolId {
		match self.context {
			MergedNetworkContextInner::Devp2p(ref net) => net.subprotocol_name(),
			MergedNetworkContextInner::Libp2p(ref net) => net.subprotocol_name(),
		}
	}
}

// Wraps around a `NetworkProtocolHandler`. We do this before attaching any protocol to devp2p.
struct Devp2pHandlerWrapper(Arc<NetworkProtocolHandler + Send + Sync>, Weak<NetBackendInner>);

impl NetworkProtocolHandler for Devp2pHandlerWrapper {
	fn initialize(&self, io: &NetworkContext, host_info: &HostInfo) {
		// TODO: initialized twice in case we have both devp2p and libp2p	
		/*let inner = self.1.upgrade().unwrap();
		let io = MergedNetworkContext {
			devp2p: inner.devp2p(),
			libp2p: inner.libp2p(),
			context: MergedNetworkContextInner::Devp2p(io),
		};
		self.0.initialize(&io, &DummyHostInfo)*/
	}

	fn read(&self, io: &NetworkContext, peer: &PeerId, packet_id: u8, data: &[u8]) {
		let inner = self.1.upgrade().unwrap();
		let io = MergedNetworkContext {
			devp2p: inner.devp2p(),
			libp2p: inner.libp2p(),
			context: MergedNetworkContextInner::Devp2p(io),
		};
		self.0.read(&io, &peer_id_int_to_ext(InternalPeerId::Devp2p(*peer)), packet_id, data);
	}

	fn connected(&self, io: &NetworkContext, peer: &PeerId) {
		let inner = self.1.upgrade().unwrap();
		let io = MergedNetworkContext {
			devp2p: inner.devp2p(),
			libp2p: inner.libp2p(),
			context: MergedNetworkContextInner::Devp2p(io),
		};
		self.0.connected(&io, &peer_id_int_to_ext(InternalPeerId::Devp2p(*peer)));
	}

	fn disconnected(&self, io: &NetworkContext, peer: &PeerId) {
		let inner = self.1.upgrade().unwrap();
		let io = MergedNetworkContext {
			devp2p: inner.devp2p(),
			libp2p: inner.libp2p(),
			context: MergedNetworkContextInner::Devp2p(io),
		};
		self.0.disconnected(&io, &peer_id_int_to_ext(InternalPeerId::Devp2p(*peer)));
	}

	fn timeout(&self, io: &NetworkContext, timer: TimerToken) {
		let inner = self.1.upgrade().unwrap();
		let io = MergedNetworkContext {
			devp2p: inner.devp2p(),
			libp2p: inner.libp2p(),
			context: MergedNetworkContextInner::Devp2p(io),
		};
		self.0.timeout(&io, timer);
	}
}

// Wraps around a `NetworkProtocolHandler`. We do this before attaching any protocol to libp2p.
struct Libp2pHandlerWrapper(Arc<NetworkProtocolHandler + Send + Sync>, Weak<NetBackendInner>);

impl NetworkProtocolHandler for Libp2pHandlerWrapper {
	fn initialize(&self, io: &NetworkContext, host_info: &HostInfo) {
		let inner = self.1.upgrade().unwrap();
		let io = MergedNetworkContext {
			devp2p: inner.devp2p(),
			libp2p: inner.libp2p(),
			context: MergedNetworkContextInner::Libp2p(io),
		};
		self.0.initialize(&io, &DummyHostInfo)
	}

	fn read(&self, io: &NetworkContext, peer: &PeerId, packet_id: u8, data: &[u8]) {
		let inner = self.1.upgrade().unwrap();
		let io = MergedNetworkContext {
			devp2p: inner.devp2p(),
			libp2p: inner.libp2p(),
			context: MergedNetworkContextInner::Libp2p(io),
		};
		self.0.read(&io, &peer_id_int_to_ext(InternalPeerId::Libp2p(*peer)), packet_id, data);
	}

	fn connected(&self, io: &NetworkContext, peer: &PeerId) {
		let inner = self.1.upgrade().unwrap();
		let io = MergedNetworkContext {
			devp2p: inner.devp2p(),
			libp2p: inner.libp2p(),
			context: MergedNetworkContextInner::Libp2p(io),
		};
		self.0.connected(&io, &peer_id_int_to_ext(InternalPeerId::Libp2p(*peer)));
	}

	fn disconnected(&self, io: &NetworkContext, peer: &PeerId) {
		let inner = self.1.upgrade().unwrap();
		let io = MergedNetworkContext {
			devp2p: inner.devp2p(),
			libp2p: inner.libp2p(),
			context: MergedNetworkContextInner::Libp2p(io),
		};
		self.0.disconnected(&io, &peer_id_int_to_ext(InternalPeerId::Libp2p(*peer)));
	}

	fn timeout(&self, io: &NetworkContext, timer: TimerToken) {
		let inner = self.1.upgrade().unwrap();
		let io = MergedNetworkContext {
			devp2p: inner.devp2p(),
			libp2p: inner.libp2p(),
			context: MergedNetworkContextInner::Libp2p(io),
		};
		self.0.timeout(&io, timer);
	}
}

enum InternalPeerId {
	Devp2p(PeerId),
	Libp2p(PeerId),
}

fn peer_id_ext_to_int(peer_id: PeerId) -> InternalPeerId {
	if peer_id >= usize::max_value() / 2 {
		InternalPeerId::Libp2p(!peer_id)
	} else {
		InternalPeerId::Devp2p(peer_id)
	}
}

fn peer_id_int_to_ext(peer_id: InternalPeerId) -> PeerId {
	match peer_id {
		InternalPeerId::Devp2p(peer_id) => {
			assert!(peer_id < usize::max_value() / 2,
					"programmer error: peer id returned by devp2p too large");
			peer_id
		},
		InternalPeerId::Libp2p(peer_id) => {
			let peer_id = !peer_id;
			assert!(peer_id >= usize::max_value() / 2,
					"programmer error: peer id returned by libp2p too large");
			peer_id
		},
	}
}
