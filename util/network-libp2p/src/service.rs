// Copyright 2015-2017 Parity Technologies (UK) Ltd.
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

use bytes::{Bytes, BytesMut};
use network::{Error, NetworkConfiguration, NetworkProtocolHandler, NonReservedPeerMode};
use network::{NetworkContext, PeerId, ProtocolId};
use ethkey::Secret;
use slab::Slab;
use fnv::FnvHashMap;
use parking_lot::RwLock;
use multiaddr::{AddrComponent, Multiaddr};
use multiplex::BufferedMultiplexConfig;
use network::{NodeId, PacketId, HostInfo, SessionInfo, ConnectionFilter, TimerToken};
use std::io::{Error as IoError, ErrorKind as IoErrorKind};
use std::iter;
use std::net::{IpAddr, Ipv4Addr};
use std::ops::Deref;
use std::sync::Arc;
use std::sync::mpsc::channel;
use std::thread;
use std::time::{Duration, Instant};
use std::vec::IntoIter as VecIntoIter;
use futures::{future, Future, Stream, Sink};
use futures::sync::{mpsc, oneshot};
use dns::DnsConfig;
use kad::{KademliaConfig, KademliaControllerPrototype, KademliaUpgrade, KademliaProcessingFuture};
use identify::{IdentifyInfo, IdentifyOutput, IdentifyProtocolConfig, IdentifyTransport};
use peerstore::{PeerId as PeerstorePeerId, Peerstore, PeerAccess};
use peerstore::memory_peerstore::MemoryPeerstore;
use tokio_core::reactor::{Core, Timeout};
use tokio_io::{AsyncRead, AsyncWrite};
use swarm::{self, Transport, ConnectionUpgrade, Endpoint};
use tcp_transport::TcpConfig;
use tokio_timer;
use varint::VarintCodec;
use websocket::WsConfig;

/// IO Service with networking
/// `Message` defines a notification data type.
pub struct NetworkService {
	shared: Arc<Shared>,
	config: NetworkConfiguration,
	// Sending a message on the channel will trigger the end of the background thread. We can
	// then wait on the join handle.
	bg_thread: Option<(oneshot::Sender<()>, thread::JoinHandle<()>)>,
}

// Common struct shared throughout all the components of the service.
struct Shared {
	// List of protocols available on the network. It is a logic error to remote protocols from
	// this list, and the code may assume that protocols stay at the same index forever.
	protocols: RwLock<Vec<RegisteredProtocol>>,

	// For each node ID (node ID = hash of public key), the ID of the peer.
	peer_by_nodeid: RwLock<FnvHashMap<Vec<u8>, usize>>,
	// For each peer ID, a list of senders per protocol and the protocol version.
	sender_by_peer: RwLock<Slab<Vec<(ProtocolId, mpsc::UnboundedSender<Message>, u8)>>>,

	// Filter for incoming connections, as passed to the `new` function.
	filter: Option<Arc<ConnectionFilter>>,

	// Use this channel to send a timeout request to the background thread's events loop.
	// After the timeout, elapsed, it will call `timeout` on the `NetworkProtocolHandler`.
	timeouts_register_tx: mpsc::UnboundedSender<(Instant, Arc<NetworkProtocolHandler + Send + Sync>, ProtocolId, TimerToken)>,
}

#[derive(Clone)]
struct RegisteredProtocol {
	// Id of the protocol for API purposes.
	id: ProtocolId,
	// Base name of the protocol as advertised on the network.
	// Ends with `/` so that we can append a version number behind.
	base_name: Bytes,
	// List of protocol versions that we support. Ordered in descending order so that the best
	// comes first.
	supported_versions: Vec<u8>,
	// Number of different packet IDs. Used to filter invalid messages.
	packet_count: u8,
	handler: Arc<NetworkProtocolHandler + Send + Sync>,
}

impl NetworkService {
	/// Starts IO event loop
	pub fn new(config: NetworkConfiguration, filter: Option<Arc<ConnectionFilter>>) -> Result<NetworkService, Error> {
		// Channel that will be used to send timeouts to the background thread so that they
		// are registered by the events loop.
		let (timeouts_register_tx, timeouts_register_rx) = mpsc::unbounded();

		let shared = Arc::new(Shared {
			protocols: RwLock::new(Vec::new()),
			peer_by_nodeid: RwLock::new(FnvHashMap::default()),		// TODO: capacity = max peers
			sender_by_peer: RwLock::new(Slab::new()),				// TODO: capacity = max peers
			filter,
			timeouts_register_tx,
		});

		let bg_thread = {
			// channel we use to signal success or failure of the bg thread initialization process
			let (init_tx, init_rx) = channel();
			// channel the main thread uses to signal the bg thread that it should stop
			let (close_tx, close_rx) = oneshot::channel();
			let shared = shared.clone();
			let config = config.clone();
			let join_handle = thread::spawn(move || {
				let mut core = match Core::new() {
					Ok(c) => c,
					Err(err) => { let _ = init_tx.send(Err(err)); return; }
				};

				let local_peer_id = PeerstorePeerId::from_public_key(&[]);

				// Build the storage for peers.
				let peerstore = Arc::new(MemoryPeerstore::empty());
				for bootnode in config.boot_nodes.iter() {
					let mut addr: Multiaddr = bootnode.parse().expect("wrong libp2p bootnode format");		// TODO: don't unwrap
					let p2p_component = addr.pop().expect("bootnode multiaddr is empty");		// TODO: don't unwrap
					let peer_id = match p2p_component {
						AddrComponent::P2P(key) | AddrComponent::IPFS(key) => {
							PeerstorePeerId::from_bytes(key).expect("invalid peer id")
						}
						_ => panic!("bootnode multiaddr didn't end with /p2p/"),
					};

					// Registering the bootstrap node with a TTL of 100000 years
					peerstore
						.peer_or_create(&peer_id)
						.add_addr(addr, Duration::from_secs(100000 * 365 * 24 * 3600));
				}

				// Configuration for Kademlia DHT.
				let kad_config = KademliaControllerPrototype::new(KademliaConfig {
					parallelism: 3,
					record_store: (),
					peer_store: peerstore.clone(),
					local_peer_id: local_peer_id.clone(),
					timeout: Duration::from_secs(10),
				});

				let kad_upgrade = KademliaUpgrade::from_prototype(&kad_config);

				// Build the transport layer.
				let transport = TcpConfig::new(core.handle())
					.or_transport(WsConfig::new(TcpConfig::new(core.handle())));
				let transport = DnsConfig::new(transport);
				let transport = transport
					.with_upgrade({
						// TODO:
						let plain_text = swarm::PlainTextConfig;
						/*let secio = {
							let private_key = include_bytes!("test-private-key.pk8");		// TODO:
							let public_key = include_bytes!("test-public-key.der").to_vec();	// TODO:
							secio::SecioConfig {
								key: secio::SecioKeyPair::rsa_from_pkcs8(private_key, public_key).unwrap(),
							}
						};*/

						plain_text//.or_upgrade(secio)
					})
					.with_upgrade(BufferedMultiplexConfig::<[u8; 256]>::new())
					.into_connection_reuse();
				let transport = IdentifyTransport::new(transport, peerstore);

				let upgrade = ConnectionUpgrader {
					kad: kad_upgrade,
					identify: IdentifyProtocolConfig,
					custom: CustomProtosConnectionUpgrade(shared.clone()),
				};

				let (swarm_controller, swarm_future) = {
					let local_peer_id = local_peer_id.clone();
					let upgrade2 = upgrade.clone();
					swarm::swarm(
						transport,
						upgrade,
						move |upgrade, client_addr| {
							// TODO: call on_connect?
							match upgrade {
								FinalUpgrade::Kad(future) => Box::new(future),
								FinalUpgrade::Identify(IdentifyOutput::Sender { sender, .. }) => {
									sender.send(
										IdentifyInfo {
											public_key: local_peer_id.clone().into_bytes(),
											protocol_version: "parity/1.11.0".to_owned(),	// TODO: version?
											agent_version: "rust-libp2p/1.0.0".to_owned(),
											listen_addrs: vec![],// TODO: listened_addrs_clone.read().unwrap().to_vec(),
											protocols: ConnectionUpgrade::<<TcpConfig as Transport>::RawConn>::protocol_names(&upgrade2)
												.filter_map(|(name, _)| String::from_utf8(name.to_vec()).ok())
												.collect(),
										},
										&client_addr,
									)
								},
								FinalUpgrade::Identify(IdentifyOutput::RemoteInfo { .. }) => {
									unreachable!("We are never dialing with the identify protocol")
								},
								FinalUpgrade::Custom(future) => future,
							}
						},
					)
				};

				{
					let listen_addr: Multiaddr = if let Some(addr) = config.listen_address {
						let ip = match addr.ip() {
							IpAddr::V4(addr) => AddrComponent::IP4(addr),
							IpAddr::V6(addr) => AddrComponent::IP6(addr),
						};
						iter::once(ip).chain(iter::once(AddrComponent::TCP(addr.port()))).collect()
					} else {
						let host = AddrComponent::IP4(Ipv4Addr::new(0, 0, 0, 0));
						let port = AddrComponent::TCP(0);
						iter::once(host).chain(iter::once(port)).collect()
					};

					info!(target: "eth-libp2p", "Libp2p listening on {}", listen_addr);		// TODO: no info!
					if let Err(err) = swarm_controller.listen_on(listen_addr.clone()) {		// TODO: don't clone
						warn!("Failed to listen on {}: {:?}", listen_addr, err);	// TODO: temporary, remove
						//let _ = init_tx.send(Err(err));		// TODO:
					}
				}

				let (kad_controller, kad_future) = kad_config.start(swarm_controller.clone());

				let timeouts = {
					let handle = core.handle();
					let shared = shared.clone();
					timeouts_register_rx
						.map_err(|()| -> IoError { unreachable!() })
						.and_then(move |(at, handler, protocol_id, timer_token)| {
							let future = Timeout::new_at(at, &handle)?
								.map(move |()| (handler, protocol_id, timer_token));
							Ok(future)
						})
						.and_then(|f| f)
						.for_each(move |(handler, protocol_id, timer_token)| {
							let ctxt = NetworkContextImpl {
								inner: shared.clone(),
								protocol: protocol_id,
								current_peer: None,
							};
							handler.timeout(&ctxt, timer_token);
							Ok(())
						})
				};

				// initialization success!
				let _ = init_tx.send(Ok(()));

				let discovery = tokio_timer::wheel()
					.build()
					.interval_at(Instant::now(), Duration::from_secs(30))
					.map_err(|_| -> IoError { unreachable!() })
					.and_then(move |()| kad_controller.find_node(local_peer_id.clone()))
					.for_each(move |results| {
						for discovered_peer in results {
							if shared.peer_by_nodeid.read().contains_key(discovered_peer.as_bytes()) {
								continue;
							}

							let addr: Multiaddr = AddrComponent::P2P(discovered_peer.into_bytes()).into();
							for proto_num in 0 .. shared.protocols.read().len() {
								let upgr = CustomProtoConnectionUpgrade(shared.clone(), proto_num);
								let _ = swarm_controller.dial_to_handler(addr.clone(), upgr);	
							}
						}

						Ok(())
					});

				let thread_future = swarm_future
					.select(kad_future).map_err(|(err, _)| err).and_then(|(_, rest)| rest)
					.select(discovery).map_err(|(err, _)| err).and_then(|(_, rest)| rest)
					.select(timeouts).map_err(|(err, _)| err).and_then(|(_, rest)| rest)
					.select(close_rx.then(|_| Ok(()))).map(|_| ()).map_err(|(err, _)| err);

				match core.run(thread_future) {
					Ok(()) => {
						info!("libp2p future finished")		// TODO: don't use info!
					},
					Err(err) => {
						error!("error while running libp2p: {:?}", err)
					},
				}
			});

			init_rx.recv().expect("libp2p background thread panicked")?;
			Some((close_tx, join_handle))
		};

		Ok(NetworkService { shared, config, bg_thread })
	}

	/// Register a new protocol handler with the event loop.
	pub fn register_protocol(&self, handler: Arc<NetworkProtocolHandler + Send + Sync>, protocol: ProtocolId, packet_count: u8, versions: &[u8]) -> Result<(), Error> {
		let mut proto_name = Bytes::from_static(b"/parity/");
		proto_name.extend_from_slice(&protocol);
		proto_name.extend_from_slice(b"/");

		if !self.shared.peer_by_nodeid.read().is_empty() {
			// TODO: figure out if that's correct
			warn!(target: "eth-libp2p", "a new network protocol was registered while the service \
										was already active ; this is a programmer error");
		}

		self.shared.protocols.write().push(RegisteredProtocol {
			base_name: proto_name,
			id: protocol,
			supported_versions: {
				let mut tmp: Vec<_> = versions.iter().rev().cloned().collect();
				tmp.sort_unstable_by(|a, b| b.cmp(a));
				tmp
			},
			handler: handler.clone(),
			packet_count,
		});

		handler.initialize(&NetworkContextImpl {
			inner: self.shared.clone(),
			protocol: protocol.clone(),
			current_peer: None,
		}, self);

		Ok(())
	}

	/// Returns network configuration.
	pub fn config(&self) -> &NetworkConfiguration {
		&self.config
	}

	/// Start network IO
	pub fn start(&self) -> Result<(), Error> {
		// TODO:
		Ok(())
	}

	/// Stop network IO
	pub fn stop(&self) -> Result<(), Error> {
		// TODO:
		Ok(())
	}

	/// Get a list of all connected peers by id.
	pub fn connected_peers(&self) -> Vec<PeerId> {
		self.shared.peer_by_nodeid.read().values().cloned().collect()
	}

	/// Try to add a reserved peer.
	pub fn add_reserved_peer(&self, peer: &str) -> Result<(), Error> {
		unimplemented!()
	}

	/// Try to remove a reserved peer.
	pub fn remove_reserved_peer(&self, peer: &str) -> Result<(), Error> {
		unimplemented!()
	}

	/// Set the non-reserved peer mode.
	pub fn set_non_reserved_mode(&self, mode: NonReservedPeerMode) {
		unimplemented!()
	}

	/// Executes action in the network context
	pub fn with_context<F>(&self, protocol: ProtocolId, action: F) where F: FnOnce(&NetworkContext) {
		self.with_context_eval(protocol, action);
	}

	/// Evaluates function in the network context
	pub fn with_context_eval<F, T>(&self, protocol: ProtocolId, action: F) -> Option<T> where F: FnOnce(&NetworkContext) -> T {
		if self.shared.protocols.read().iter().find(|p| p.id == protocol).is_none() {
			return None;
		};

		let ctxt = NetworkContextImpl {
			inner: self.shared.clone(),
			protocol: protocol.clone(),
			current_peer: None,
		};

		Some(action(&ctxt))
	}
}

impl Drop for NetworkService {
	fn drop(&mut self) {
		let (close_tx, join) = self.bg_thread.take()
			.expect("Option extracted only in destructor");
		let _ = close_tx.send(());
		if let Err(e) = join.join() {
			warn!(target: "eth-libp2p", "error while waiting on libp2p background thread: {:?}", e);
		}
	}
}

impl HostInfo for NetworkService {
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

#[derive(Clone)]
struct NetworkContextImpl {
	inner: Arc<Shared>,
	protocol: ProtocolId,
	current_peer: Option<PeerId>,
}

impl NetworkContext for NetworkContextImpl {
	fn send(&self, peer: PeerId, packet_id: PacketId, data: Vec<u8>) -> Result<(), Error> {
		self.send_protocol(self.protocol, peer, packet_id, data)
	}

	fn send_protocol(&self, protocol: ProtocolId, peer: PeerId, packet_id: PacketId, data: Vec<u8>) -> Result<(), Error> {
		debug_assert!(self.inner.protocols.read().iter().find(|p| p.id == protocol).is_some(),
					  "invalid protocol id requested in the API of the libp2p networking");
		debug_assert!(packet_id < self.inner.protocols.read().iter().find(|p| p.id == protocol).unwrap().packet_count,
					  "invalid packet id requested in the API of the libp2p networking");

		if let Some(peer) = self.inner.sender_by_peer.read().get(peer) {
			if let Some(sender) = peer.iter().find(|elem| elem.0 == protocol).map(|e| &e.1) {
				let mut message = Bytes::with_capacity(1 + data.len());
				message.extend_from_slice(&[packet_id]);
				message.extend_from_slice(&data);
				sender.unbounded_send(Message::SendReq(message));		// TODO: report errors
				Ok(())
			} else {
				panic!()		// TODO: proper error if no open connection with this proto
			}
		} else {
			panic!()		// TODO: proper error if no open connection with this peer
		}
	}

	fn respond(&self, packet_id: PacketId, data: Vec<u8>) -> Result<(), Error> {
		if let Some(peer) = self.current_peer {
			self.send_protocol(self.protocol, peer, packet_id, data)
		} else {
			panic!("respond() called outside of a received message");
		}
	}

	fn disable_peer(&self, peer: PeerId) {

	}

	fn disconnect_peer(&self, peer: PeerId) {

	}

	fn is_expired(&self) -> bool {
		let peer = match self.current_peer.as_ref() {
			Some(&p) => p,
			None => return false
		};

		self.inner.sender_by_peer
			.read()
			.get(peer)
			.is_none()
	}

	fn register_timer(&self, token: usize, ms: u64) -> Result<(), Error> {
		// TODO: don't unwrap
		let handler = self.inner.protocols.read().iter().find(|p| p.id == self.protocol).unwrap().handler.clone();
		let at = Instant::now() + Duration::from_millis(ms);
		self.inner.timeouts_register_tx.unbounded_send((at, handler, self.protocol, token));
		Ok(())		// TODO: error handling
	}

	fn peer_client_version(&self, peer: PeerId) -> String {
		String::new() // TODO: implement
	}

	fn session_info(&self, peer: PeerId) -> Option<SessionInfo> {
		/*let node_id = self.inner.peer_by_nodeid.read().iter()
			.find(|&(_, &p)| p == peer)
			.map(|n| n.clone());*/

		// FIXME: wrong!
		Some(SessionInfo {
			id: None,
			client_version: String::new(),
			protocol_version: 1,
			capabilities: Vec::new(),
			peer_capabilities: Vec::new(),
			ping_ms: None,
			originated: false,
			remote_address: String::new(),
			local_address: String::new(),
		})
	}

	fn protocol_version(&self, protocol: ProtocolId, peer: PeerId) -> Option<u8> {
		self.inner.sender_by_peer
			.read()
			.get(peer)
			.and_then(|list| list.iter().find(|p| p.0 == protocol).map(|p| p.2))
	}

	fn subprotocol_name(&self) -> ProtocolId {
		self.protocol.clone()
	}
}

enum FinalUpgrade<C> {
    Kad(KademliaProcessingFuture),
    Identify(IdentifyOutput<C>),
	Custom(Box<Future<Item = (), Error = IoError>>),
}

// Master connection updater, which handles all the registered protocols but also what is necessary
// for the infrastructure (kademlia, identify, ...)
// TODO: use a custom derive or something
#[derive(Clone)]
struct ConnectionUpgrader<P, R> {
    kad: KademliaUpgrade<P, R>,
    identify: IdentifyProtocolConfig,
	custom: CustomProtosConnectionUpgrade,
}

impl<C, P, R, Pc> ConnectionUpgrade<C> for ConnectionUpgrader<P, R>
where
    C: AsyncRead + AsyncWrite + 'static,     // TODO: 'static :-/
    P: Deref<Target = Pc> + Clone + 'static, // TODO: 'static :-/
    for<'r> &'r Pc: Peerstore,
    R: 'static, // TODO: 'static :-/
{
    type NamesIter = VecIntoIter<(Bytes, (usize, u8))>;		// TODO: use a chain of iters
    type UpgradeIdentifier = (usize, u8);

    #[inline]
    fn protocol_names(&self) -> Self::NamesIter {
		let kad = iter::once((Bytes::from("/ipfs/kad/1.0.0"), (0, 0)));
		let id = iter::once((Bytes::from("/ipfs/id/1.0.0"), (1, 0)));
		kad.chain(id)
			.chain(ConnectionUpgrade::<C>::protocol_names(&self.custom).map(|(n, (id, v))| (n, (id + 2, v))))
			.collect::<Vec<_>>().into_iter()
    }

    type Output = FinalUpgrade<C>;
    type Future = Box<Future<Item = FinalUpgrade<C>, Error = IoError>>;

    fn upgrade(
        self,
        socket: C,
        (id, v): Self::UpgradeIdentifier,
        ty: Endpoint,
        remote_addr: &Multiaddr,
    ) -> Self::Future {
        match id {
            0 => Box::new(
                self.kad
                    .upgrade(socket, (), ty, remote_addr)
                    .map(|upg| upg.into()),
            ),
            1 => Box::new(
                self.identify
                    .upgrade(socket, (), ty, remote_addr)
                    .map(|upg| upg.into()),
            ),
			n => Box::new(self.custom.upgrade(socket, (n - 2, v), ty, remote_addr)
				.map(FinalUpgrade::Custom)),
        }
    }
}

impl<C> From<KademliaProcessingFuture> for FinalUpgrade<C> {
    #[inline]
    fn from(upgr: KademliaProcessingFuture) -> Self {
        FinalUpgrade::Kad(upgr)
    }
}

impl<C> From<IdentifyOutput<C>> for FinalUpgrade<C> {
    #[inline]
    fn from(upgr: IdentifyOutput<C>) -> Self {
        FinalUpgrade::Identify(upgr)
    }
}

impl<C> From<Box<Future<Item = (), Error = IoError>>> for FinalUpgrade<C> {
    #[inline]
    fn from(upgr: Box<Future<Item = (), Error = IoError>>) -> Self {
        FinalUpgrade::Custom(upgr)
    }
}

// Connection upgrade for a single protocol.
//
// Note that "a single protocol" refers to `eth` or `pip` for example. However each protocol can
// have multiple different versions for networking purposes.
//
// The protocol that is handled is the one at the given index with the list of protocols in
// the `Shared`.
#[derive(Clone)]
struct CustomProtoConnectionUpgrade(Arc<Shared>, usize);

impl<C> ConnectionUpgrade<C> for CustomProtoConnectionUpgrade
	where C: AsyncRead + AsyncWrite + 'static		// TODO: 'static :-/
{
	type NamesIter = VecIntoIter<(Bytes, Self::UpgradeIdentifier)>;
	type UpgradeIdentifier = u8;		// Protocol version

	#[inline]
	fn protocol_names(&self) -> Self::NamesIter {
		if let Some(proto) = self.0.protocols.read().get(self.1) {
			// Report each version as an individual protocol.
			proto.supported_versions.iter().map(|&ver| {
				let mut num = ver.to_string();
				let mut name = proto.base_name.clone();
				name.extend_from_slice(num.as_bytes());
				(name, ver)
			}).collect::<Vec<_>>().into_iter()
		} else {
			error!(target: "eth-libp2p", "trying to negotiate a protocol that is no longer registered");
			Vec::new().into_iter()
		}
	}

	type Output = Box<Future<Item = (), Error = IoError>>;
	type Future = future::FutureResult<Self::Output, IoError>;

	fn upgrade(self, socket: C, protocol_version: Self::UpgradeIdentifier, _: Endpoint,
			   remote_addr: &Multiaddr) -> Self::Future
	{
		// Grab the protocol that `self` indicates.
		let (registered_protocol, num_registered_protocols) = {
			let lock = self.0.protocols.read();
			let num_registered_protocols = lock.len();
			match lock.get(self.1).cloned() {
				Some(p) => (p, num_registered_protocols),
				None => {
					error!(target: "eth-libp2p", "trying to upgrade to a protocol that is no longer registered");
					return future::err(IoError::new(IoErrorKind::Other, "protocol is no longer registered".to_owned()));
				}
			}
		};

		// Because we're using the `IdentifyTransport` layer, all the multiaddresses received here
		// should be of the format `/p2p/<node_id>`.
		let node_id = {
			let mut iter = remote_addr.iter();
			let first = iter.next();
			let second = iter.next();
			match (first, second) {
				(Some(AddrComponent::P2P(node_id)), None) => node_id,
				_ => {
					warn!(target: "eth-libp2p", "Ignoring node because of invalid \
												multiaddress {:?}", remote_addr);
					return future::ok(Box::new(future::ok(())) as Box<_>);
				}
			}
		};

		// This channel is used to send instructions to the handler for this open substream.
		let (msg_tx, msg_rx) = mpsc::unbounded();

		let peer_id = {
			// TODO: double locking :-/ is there something we can do?
			let mut sender_by_peer = self.0.sender_by_peer.write();
			let peer_id = *self.0.peer_by_nodeid.write().entry(node_id.clone()).or_insert_with(|| {
				sender_by_peer.insert(Vec::with_capacity(num_registered_protocols))
			});

			// TODO: check for existing entry
			sender_by_peer[peer_id].push((registered_protocol.id.clone(), msg_tx, protocol_version));
			peer_id
		};

		registered_protocol.handler.connected(&NetworkContextImpl {
			inner: self.0.clone(),
			protocol: registered_protocol.id,
			current_peer: Some(peer_id),
		}, &peer_id);

		// Build the sink for outgoing network bytes, and the stream for incoming instructions.
		// `stream` implements `Stream<Item = Message>`.
		let (sink, stream) = {
			let framed = AsyncRead::framed(socket, VarintCodec::default());
			let (sink, stream) = framed.split();
			let stream = msg_rx.map_err(|()| unreachable!()).select(stream.map(Message::RecvSocket));
			(sink, stream)
		};

		let shared = self.0.clone();
		let future = future::loop_fn((stream, sink), move |(stream, sink)| {
			let registered_protocol = registered_protocol.clone();
			let shared = shared.clone();

			stream
				.into_future()
				.map_err(|(err, _)| err)
				.and_then(move |(message, stream)| {
					// Grabbed next message from `stream`.
					match message {
						Some(Message::RecvSocket(mut data)) => {
							// The `data` should be prefixed by the packet ID, therefore an empty
							// packet is invalid.
							if data.is_empty() {
								debug!(target: "eth-libp2p", "ignoring incoming packet because \
										it was empty");
								let f = future::ok(future::Loop::Continue((stream, sink)));
								return future::Either::A(f);
							}

							let packet_id = data[0];
							if packet_id >= registered_protocol.packet_count {
								debug!(target: "eth-libp2p", "ignoring incoming packet because \
										packet_id {} is too large", packet_id);
							}

							registered_protocol.handler.read(&NetworkContextImpl {
								inner: shared,
								protocol: registered_protocol.id,
								current_peer: Some(peer_id.clone()),
							}, &peer_id, packet_id, &data.split_off(1));

							let fut = future::ok(future::Loop::Continue((stream, sink)));
							future::Either::A(fut)
						},

						Some(Message::SendReq(data)) => {
							let fut = sink.send(data)
								.map(|sink| future::Loop::Continue((stream, sink)));
							future::Either::B(fut)
						},

						Some(Message::Disconnect) | None => {
							let fut = future::ok(future::Loop::Break(()));
							future::Either::A(fut)
						},
					}
				})
		});

		future::ok(Box::new(future) as Box<_>)
	}
}

// Connection upgrade for all the protocols registered within the `Shared`.
#[derive(Clone)]
struct CustomProtosConnectionUpgrade(Arc<Shared>);

impl<C> ConnectionUpgrade<C> for CustomProtosConnectionUpgrade
	where C: AsyncRead + AsyncWrite + 'static		// TODO: 'static :-/
{
	type NamesIter = VecIntoIter<(Bytes, Self::UpgradeIdentifier)>;
	type UpgradeIdentifier = (usize, <CustomProtoConnectionUpgrade as ConnectionUpgrade<C>>::UpgradeIdentifier);

	fn protocol_names(&self) -> Self::NamesIter {
		// We concat the lists of `CustomProtoConnectionUpgrade::protocol_names` for each protocol.
		let num_protos = self.0.protocols.read().len();
		(0 .. num_protos).flat_map(|n| {
			let interm = CustomProtoConnectionUpgrade(self.0.clone(), n);
			ConnectionUpgrade::<C>::protocol_names(&interm)
				.map(move |(name, id)| (name, (n, id)))
		}).collect::<Vec<_>>().into_iter()
	}

	type Output = <CustomProtoConnectionUpgrade as ConnectionUpgrade<C>>::Output;
	type Future = <CustomProtoConnectionUpgrade as ConnectionUpgrade<C>>::Future;

	#[inline]
	fn upgrade(self, socket: C, (protocol_index, inner_proto_id): Self::UpgradeIdentifier,
				endpoint: Endpoint, remote_addr: &Multiaddr) -> Self::Future
	{
		let interm = CustomProtoConnectionUpgrade(self.0.clone(), protocol_index);
		interm.upgrade(socket, inner_proto_id, endpoint, remote_addr)
	}
}

// Message dispatched to the handler of an open socket with a remote.
enum Message {
	// Received a packet from the remote.
	RecvSocket(BytesMut),
	// Want to send a packet to the remote. Note that the packet_id has to be inside the `Bytes`.
	SendReq(Bytes),
	// Disconnect from the remote.
	Disconnect,
}

#[cfg(test)]
mod tests {
	use super::NetworkService;

	#[test]
	fn builds_and_finishes_in_finite_time() {
		let _service = NetworkService::new(Default::default(), None).unwrap();
	}
}
