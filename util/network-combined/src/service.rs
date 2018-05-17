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

use std::net::SocketAddr;
use std::ops::Range;
use std::sync::{Arc, Weak};
use std::time::Duration;
use devp2p::NetworkService as Devp2pNetworkService;
use libp2p::NetworkService as Libp2pNetworkService;
use network::{NetworkProtocolHandler, NetworkContext, PeerId, ProtocolId, PacketId,
	NetworkConfiguration, NonReservedPeerMode, Error, ErrorKind, ConnectionFilter, SessionInfo};
use io::TimerToken;

/// Unification of devp2p and libp2p.
pub struct NetworkService {
	inner: Arc<NetworkServiceInner>,
	num_peers_range: Range<u32>,
}

enum NetworkServiceInner {
	Devp2p(Devp2pNetworkService),
	Libp2p(Libp2pNetworkService),
	Both(Devp2pNetworkService, Libp2pNetworkService),
}

impl NetworkServiceInner {
	// Returns the devp2p service, if it's enabled.
	#[inline]
	fn devp2p(&self) -> Option<&Devp2pNetworkService> {
		match *self {
			NetworkServiceInner::Devp2p(ref devp2p) => Some(devp2p),
			NetworkServiceInner::Libp2p(_) => None,
			NetworkServiceInner::Both(ref devp2p, _) => Some(devp2p),
		}
	}

	// Returns the libp2p service, if it's enabled.
	#[inline]
	fn libp2p(&self) -> Option<&Libp2pNetworkService> {
		match *self {
			NetworkServiceInner::Devp2p(_) => None,
			NetworkServiceInner::Libp2p(ref libp2p) => Some(libp2p),
			NetworkServiceInner::Both(_, ref libp2p) => Some(libp2p),
		}
	}
}

impl NetworkService {
	/// Starts IO event loop
	pub fn new(net_config: NetworkConfiguration, connection_filter: Option<Arc<ConnectionFilter>>) -> Result<NetworkService, Error> {
		let inner = match (net_config.enabled_devp2p, net_config.enabled_libp2p) {
			(true, true) => {
				// TODO: adjust the filter to handle max_peers
				let devp2p = Devp2pNetworkService::new(net_config.clone(), connection_filter.clone())?;
				let libp2p = Libp2pNetworkService::new(net_config, connection_filter.clone())?;
				NetworkServiceInner::Both(devp2p, libp2p)
			},
			(true, false) => {
				let devp2p = Devp2pNetworkService::new(net_config, connection_filter)?;
				NetworkServiceInner::Devp2p(devp2p)
			},
			(false, true) => {
				let libp2p = Libp2pNetworkService::new(net_config, connection_filter)?;
				NetworkServiceInner::Libp2p(libp2p)
			},
			(false, false) => {
				// TODO:
				panic!()
			},
		};

		Ok(NetworkService {
			inner: Arc::new(inner),
			num_peers_range: 0 .. 1,		// FIXME:
		})
	}

	/// Register a new protocol handler with the event loop.
	pub fn register_protocol(&self, handler: Arc<NetworkProtocolHandler + Send + Sync>, protocol: ProtocolId, versions: &[(u8, u8)]) -> Result<(), Error> {
		match *self.inner {
			NetworkServiceInner::Devp2p(ref net) => {
				net.register_protocol(handler, protocol, versions)
			},
			NetworkServiceInner::Libp2p(ref net) => {
				net.register_protocol(handler, protocol, versions);
				Ok(())
			},
			NetworkServiceInner::Both(ref devp2p, ref libp2p) => {
				// If both devp2p and libp2p are enabled, we don't directly pass the handler to
				// devp2p and libp2p. Instead we put the handler in a small wrapper whose job is to
				// convert the peer IDs between the outside and the inside.

				devp2p.register_protocol(Arc::new(Devp2pHandlerWrapper(handler.clone(), Arc::downgrade(&self.inner))), protocol, versions)?;
				// Note that `libp2p.register_protocol` can never error, so there's no need to
				// handle rolling back the devp2p registration.
				libp2p.register_protocol(Arc::new(Libp2pHandlerWrapper(handler.clone(), Arc::downgrade(&self.inner))), protocol, versions);

				// When we register a protocol to either devp2p or libp2p, they call the
				// `initialize()` method on the handler.
				// However, because we don't want `initialize()` to be called twice the wrappers
				// filter out the call to `initialize` and we call it manually here.
				handler.initialize(&MergedNetworkContext {
					inner: &self.inner,
					protocol: protocol,
					context: MergedNetworkContextInner::None,
				});

				Ok(())
			},
		}
	}

	/// Start network IO
	pub fn start(&self) -> Result<(), (Error, Option<SocketAddr>)> {
		match *self.inner {
			NetworkServiceInner::Devp2p(ref net) => net.start(),
			NetworkServiceInner::Libp2p(ref net) => net.start(),
			NetworkServiceInner::Both(ref devp2p, ref libp2p) => {
				devp2p.start()?;
				match libp2p.start() {
					Ok(()) => Ok(()),
					Err(err) => {
						devp2p.stop();
						Err(err)
					},
				}
			},
		}
	}

	/// Stop network IO
	pub fn stop(&self) {
		match *self.inner {
			NetworkServiceInner::Devp2p(ref net) => net.stop(),
			NetworkServiceInner::Libp2p(ref net) => net.stop(),
			NetworkServiceInner::Both(ref devp2p, ref libp2p) => {
				devp2p.stop();
				libp2p.stop();
			},
		}
	}

	/// Get a list of all connected peers by id.
	pub fn connected_peers(&self) -> Vec<PeerId> {
		match *self.inner {
			NetworkServiceInner::Devp2p(ref net) => net.connected_peers(),
			NetworkServiceInner::Libp2p(ref net) => net.connected_peers(),
			NetworkServiceInner::Both(ref devp2p, ref libp2p) => {
				let devp2p = devp2p.connected_peers().into_iter()
					.map(|p| peer_id_int_to_ext(InternalPeerId::Devp2p(p)));
				let libp2p = libp2p.connected_peers().into_iter()
					.map(|p| peer_id_int_to_ext(InternalPeerId::Libp2p(p)));
				devp2p.chain(libp2p).collect()
			},
		}
	}

	/// Try to add a reserved peer to devp2p.
	/// The format of the parameter is specific to devp2p.
	pub fn add_reserved_peer_devp2p(&self, peer: &str) -> Result<(), Error> {
		match *self.inner {
			NetworkServiceInner::Devp2p(ref net) => net.add_reserved_peer(peer),
			NetworkServiceInner::Libp2p(_) => Ok(()),
			NetworkServiceInner::Both(ref devp2p, _) => {
				devp2p.add_reserved_peer(peer)
			},
		}
	}

	/// Try to add a reserved peer to libp2p.
	/// The format of the parameter is specific to libp2p.
	// TODO: consider other parameter type
	pub fn add_reserved_peer_libp2p(&self, peer: &str) -> Result<(), Error> {
		match *self.inner {
			NetworkServiceInner::Devp2p(_) => Ok(()),
			NetworkServiceInner::Libp2p(ref net) => net.add_reserved_peer(peer),
			NetworkServiceInner::Both(_, ref libp2p) => {
				libp2p.add_reserved_peer(peer)
			},
		}
	}

	/// Try to remove a reserved peer from devp2p.
	/// The format of the parameter is specific to devp2p.
	pub fn remove_reserved_peer_devp2p(&self, peer: &str) -> Result<(), Error> {
		match *self.inner {
			NetworkServiceInner::Devp2p(ref net) => net.remove_reserved_peer(peer),
			NetworkServiceInner::Libp2p(_) => Ok(()),
			NetworkServiceInner::Both(ref devp2p, _) => {
				devp2p.remove_reserved_peer(peer)
			},
		}
	}

	/// Try to remove a reserved peer.
	/// The format of the parameter is specific to libp2p.
	// TODO: consider another format for the parameter
	pub fn remove_reserved_peer_libp2p(&self, peer: &str) -> Result<(), Error> {
		match *self.inner {
			NetworkServiceInner::Devp2p(_) => Ok(()),
			NetworkServiceInner::Libp2p(ref net) => net.remove_reserved_peer(peer),
			NetworkServiceInner::Both(_, ref libp2p) => {
				libp2p.remove_reserved_peer(peer)
			},
		}
	}

	/// Set the non-reserved peer mode.
	pub fn set_non_reserved_mode(&self, mode: NonReservedPeerMode) {
		match *self.inner {
			NetworkServiceInner::Devp2p(ref net) => net.set_non_reserved_mode(mode),
			NetworkServiceInner::Libp2p(ref net) => net.set_non_reserved_mode(mode),
			NetworkServiceInner::Both(ref devp2p, ref libp2p) => {
				devp2p.set_non_reserved_mode(mode.clone());
				libp2p.set_non_reserved_mode(mode);
			},
		}
	}

	/// Returns the external url of devp2p if available.
	pub fn external_url_devp2p(&self) -> Option<String> {
		match *self.inner {
			NetworkServiceInner::Devp2p(ref net) => net.external_url(),
			NetworkServiceInner::Libp2p(_) => None,
			NetworkServiceInner::Both(ref devp2p, _) => {
				devp2p.external_url()
			},
		}
	}

	/// Returns the number of peers allowed.
	///
	/// Keep in mind that `range.end` is *exclusive*.
	pub fn num_peers_range(&self) -> Range<u32> {
		self.num_peers_range.clone()
	}

	/// Executes action in the network context
	pub fn with_context<F>(&self, protocol: ProtocolId, action: F) where F: FnOnce(&NetworkContext) {
		self.with_context_eval(protocol, action);
	}

	/// Evaluates function in the network context
	pub fn with_context_eval<F, T>(&self, protocol: ProtocolId, action: F) -> Option<T> where F: FnOnce(&NetworkContext) -> T {
		Some(action(&MergedNetworkContext {
			inner: &self.inner,
			protocol: protocol,
			context: MergedNetworkContextInner::None,
		}))
	}
}

/// Implementation of `NetworkContext` for the merge of devp2p and libp2p.
pub struct MergedNetworkContext<'a> {
	inner: &'a NetworkServiceInner,
	protocol: ProtocolId,
	context: MergedNetworkContextInner<'a>,
}

enum MergedNetworkContextInner<'a> {
	Devp2p(&'a NetworkContext),
	Libp2p(&'a NetworkContext),
	None,
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
				if let Some(ref net) = self.inner.devp2p() {
					net.with_context_eval(self.subprotocol_name(), |ctxt| ctxt.send(peer, packet_id, data))
						.expect("err")		// TODO: error
				} else {
					return Err(ErrorKind::InvalidNodeId.into());
				}
			},
			(InternalPeerId::Libp2p(peer), _) => {
				if let Some(ref net) = self.inner.libp2p() {
					net.with_context_eval(self.subprotocol_name(), |ctxt| ctxt.send(peer, packet_id, data))
						.expect("err")		// TODO: error
				} else {
					return Err(ErrorKind::InvalidNodeId.into());
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
				if let Some(ref net) = self.inner.devp2p() {
					net.with_context_eval(self.subprotocol_name(), |ctxt| {
						ctxt.send_protocol(protocol, peer, packet_id, data)
					}).unwrap_or(Err(ErrorKind::InvalidNodeId.into()))
				} else {
					return Err(ErrorKind::InvalidNodeId.into());
				}
			},
			(InternalPeerId::Libp2p(peer), _) => {
				if let Some(ref net) = self.inner.libp2p() {
					net.with_context_eval(self.subprotocol_name(), |ctxt| {
						ctxt.send_protocol(protocol, peer, packet_id, data)
					}).unwrap_or(Err(ErrorKind::InvalidNodeId.into()))
				} else {
					return Err(ErrorKind::InvalidNodeId.into());
				}
			},
		}
	}

	fn respond(&self, packet_id: PacketId, data: Vec<u8>) -> Result<(), Error> {
		match self.context {
			MergedNetworkContextInner::Devp2p(ref net) => net.respond(packet_id, data),
			MergedNetworkContextInner::Libp2p(ref net) => net.respond(packet_id, data),
			MergedNetworkContextInner::None => panic!("respond called outside of a session"),
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
				if let Some(ref net) = self.inner.devp2p() {
					net.with_context(self.subprotocol_name(), |ctxt| ctxt.disable_peer(peer));
				}
			},
			(InternalPeerId::Libp2p(peer), _) => {
				if let Some(ref net) = self.inner.libp2p() {
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
				if let Some(ref net) = self.inner.devp2p() {
					net.with_context(self.subprotocol_name(), |ctxt| ctxt.disconnect_peer(peer));
				}
			},
			(InternalPeerId::Libp2p(peer), _) => {
				if let Some(ref net) = self.inner.libp2p() {
					net.with_context(self.subprotocol_name(), |ctxt| ctxt.disconnect_peer(peer));
				}
			},
		}
	}

	fn is_expired(&self) -> bool {
		match self.context {
			MergedNetworkContextInner::Devp2p(ref net) => net.is_expired(),
			MergedNetworkContextInner::Libp2p(ref net) => net.is_expired(),
			MergedNetworkContextInner::None => panic!("is_expired called outside of a session"),
		}
	}

	fn register_timer(&self, token: TimerToken, duration: Duration) -> Result<(), Error> {
		match self.inner {
			NetworkServiceInner::Libp2p(ref libp2p) => {
				libp2p.with_context_eval(self.protocol, |ctxt| {
					ctxt.register_timer(token, duration)
				}).expect("we are inside a net callback ; therefore the backend is running")
			}
			NetworkServiceInner::Devp2p(ref devp2p) => {
				devp2p.with_context_eval(self.protocol, |ctxt| {
					ctxt.register_timer(token, duration)
				}).expect("we are inside a net callback ; therefore the backend is running")
			}
			NetworkServiceInner::Both(_, ref libp2p) => {
				// If both devp2p and libp2p are enabled, we use only one backend (libp2p) so that
				// we don't split the timers between the two backends.
				libp2p.with_context_eval(self.protocol, |ctxt| {
					ctxt.register_timer(token, duration)
				}).expect("we are inside a net callback ; therefore the backend is running")
			}
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
				// Devp2p returns "unknown" on unknown peer ID, so we do the same.
				if let Some(ref net) = self.inner.devp2p() {
					net.with_context_eval(self.subprotocol_name(), |ctxt| ctxt.peer_client_version(peer))
						.unwrap_or_else(|| "unknown".to_owned())
				} else {
					"unknown".to_owned()
				}
			},
			(InternalPeerId::Libp2p(peer), _) => {
				// Devp2p returns "unknown" on unknown peer ID, so we do the same.
				if let Some(ref net) = self.inner.libp2p() {
					net.with_context_eval(self.subprotocol_name(), |ctxt| ctxt.peer_client_version(peer))
						.unwrap_or_else(|| "unknown".to_owned())
				} else {
					"unknown".to_owned()
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
				if let Some(ref net) = self.inner.devp2p() {
					net.with_context_eval(self.subprotocol_name(), |ctxt| ctxt.session_info(peer))
						.and_then(|v| v)
				} else {
					None
				}
			},
			(InternalPeerId::Libp2p(peer), _) => {
				if let Some(ref net) = self.inner.libp2p() {
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
				if let Some(ref net) = self.inner.devp2p() {
					net.with_context_eval(self.subprotocol_name(), |ctxt| ctxt.protocol_version(protocol, peer))
						.and_then(|v| v)
				} else {
					None
				}
			},
			(InternalPeerId::Libp2p(peer), _) => {
				if let Some(ref net) = self.inner.libp2p() {
					net.with_context_eval(self.subprotocol_name(), |ctxt| ctxt.protocol_version(protocol, peer))
						.and_then(|v| v)
				} else {
					None
				}
			},
		}
	}

	#[inline]
	fn subprotocol_name(&self) -> ProtocolId {
		self.protocol
	}

	fn is_reserved_peer(&self, peer: PeerId) -> bool {
		match (peer_id_ext_to_int(peer), &self.context) {
			(InternalPeerId::Devp2p(peer), &MergedNetworkContextInner::Devp2p(ref ctxt)) => {
				ctxt.is_reserved_peer(peer)
			},
			(InternalPeerId::Libp2p(peer), &MergedNetworkContextInner::Libp2p(ref ctxt)) => {
				ctxt.is_reserved_peer(peer)
			},
			(InternalPeerId::Devp2p(peer), _) => {
				if let Some(ref net) = self.inner.devp2p() {
					net.with_context_eval(self.subprotocol_name(), |ctxt| ctxt.is_reserved_peer(peer))
						.unwrap_or(false)
				} else {
					false
				}
			},
			(InternalPeerId::Libp2p(peer), _) => {
				if let Some(ref net) = self.inner.libp2p() {
					net.with_context_eval(self.subprotocol_name(), |ctxt| ctxt.is_reserved_peer(peer))
						.unwrap_or(false)
				} else {
					false
				}
			},
		}
	}
}

// Wraps around a `NetworkProtocolHandler`. We do this before attaching any protocol to devp2p.
struct Devp2pHandlerWrapper(Arc<NetworkProtocolHandler + Send + Sync>, Weak<NetworkServiceInner>);

impl NetworkProtocolHandler for Devp2pHandlerWrapper {
	fn initialize(&self, _io: &NetworkContext) {
		// Since the protocol is registered once for devp2p and once for libp2p, calling
		// `self.0.initialize()` here would result in `initialize()` being called twice, which we
		// don't want.
		// Instead we call `initialize()` manually when registering the protocol.
	}

	fn read(&self, io: &NetworkContext, peer: &PeerId, packet_id: u8, data: &[u8]) {
		let inner = self.1.upgrade().expect("we are in callback, therefore services are running");
		let io = MergedNetworkContext {
			inner: &inner,
			protocol: io.subprotocol_name(),
			context: MergedNetworkContextInner::Devp2p(io),
		};
		self.0.read(&io, &peer_id_int_to_ext(InternalPeerId::Devp2p(*peer)), packet_id, data);
	}

	fn connected(&self, io: &NetworkContext, peer: &PeerId) {
		let inner = self.1.upgrade().expect("we are in callback, therefore services are running");
		let io = MergedNetworkContext {
			inner: &inner,
			protocol: io.subprotocol_name(),
			context: MergedNetworkContextInner::Devp2p(io),
		};
		self.0.connected(&io, &peer_id_int_to_ext(InternalPeerId::Devp2p(*peer)));
	}

	fn disconnected(&self, io: &NetworkContext, peer: &PeerId) {
		let inner = self.1.upgrade().expect("we are in callback, therefore services are running");
		let io = MergedNetworkContext {
			inner: &inner,
			protocol: io.subprotocol_name(),
			context: MergedNetworkContextInner::Devp2p(io),
		};
		self.0.disconnected(&io, &peer_id_int_to_ext(InternalPeerId::Devp2p(*peer)));
	}

	fn timeout(&self, io: &NetworkContext, timer: TimerToken) {
		let inner = self.1.upgrade().expect("we are in callback, therefore services are running");
		let io = MergedNetworkContext {
			inner: &inner,
			protocol: io.subprotocol_name(),
			context: MergedNetworkContextInner::Devp2p(io),
		};
		self.0.timeout(&io, timer);
	}
}

// Wraps around a `NetworkProtocolHandler`. We do this before attaching any protocol to libp2p.
struct Libp2pHandlerWrapper(Arc<NetworkProtocolHandler + Send + Sync>, Weak<NetworkServiceInner>);

impl NetworkProtocolHandler for Libp2pHandlerWrapper {
	fn initialize(&self, _io: &NetworkContext) {
		// Since the protocol is registered once for devp2p and once for libp2p, calling
		// `self.0.initialize()` here would result in `initialize()` being called twice, which we
		// don't want.
		// Instead we call `initialize()` manually when registering the protocol.
	}

	fn read(&self, io: &NetworkContext, peer: &PeerId, packet_id: u8, data: &[u8]) {
		let inner = self.1.upgrade().expect("we are in callback, therefore services are running");
		let io = MergedNetworkContext {
			inner: &inner,
			protocol: io.subprotocol_name(),
			context: MergedNetworkContextInner::Libp2p(io),
		};
		self.0.read(&io, &peer_id_int_to_ext(InternalPeerId::Libp2p(*peer)), packet_id, data);
	}

	fn connected(&self, io: &NetworkContext, peer: &PeerId) {
		let inner = self.1.upgrade().expect("we are in callback, therefore services are running");
		let io = MergedNetworkContext {
			inner: &inner,
			protocol: io.subprotocol_name(),
			context: MergedNetworkContextInner::Libp2p(io),
		};
		self.0.connected(&io, &peer_id_int_to_ext(InternalPeerId::Libp2p(*peer)));
	}

	fn disconnected(&self, io: &NetworkContext, peer: &PeerId) {
		let inner = self.1.upgrade().expect("we are in callback, therefore services are running");
		let io = MergedNetworkContext {
			inner: &inner,
			protocol: io.subprotocol_name(),
			context: MergedNetworkContextInner::Libp2p(io),
		};
		self.0.disconnected(&io, &peer_id_int_to_ext(InternalPeerId::Libp2p(*peer)));
	}

	fn timeout(&self, io: &NetworkContext, timer: TimerToken) {
		let inner = self.1.upgrade().expect("we are in callback, therefore services are running");
		let io = MergedNetworkContext {
			inner: &inner,
			protocol: io.subprotocol_name(),
			context: MergedNetworkContextInner::Libp2p(io),
		};
		self.0.timeout(&io, timer);
	}
}


// When the user of our API passes in a `PeerId`, we turn it into an `InternalPeerId` by calling
// `peer_id_ext_to_int`. If it is a `InternalPeerId::Devp2p` then the content is relevant to
// devp2p, and if it is a `InternalPeerId::Libp2p` then the content is relevant to libp2p.
//
// When we receive a `PeerId` from devp2p or libp2p and we want to pass it to the user of our API,
// we use `peer_id_int_to_ext`.

#[derive(Debug, Copy, Clone)]
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
