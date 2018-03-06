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

extern crate parking_lot;
extern crate fnv;
extern crate futures;
extern crate tokio_core;
extern crate tokio_io;
extern crate tokio_timer;
extern crate multiaddr;
extern crate multiplex;
extern crate ethkey;
extern crate ethcore_network as network;
extern crate libp2p_dns as dns;
extern crate libp2p_kad as kad;
extern crate libp2p_identify as identify;
extern crate libp2p_peerstore as peerstore;
//extern crate libp2p_secio as secio;
extern crate libp2p_swarm as swarm;
extern crate libp2p_tcp_transport as tcp_transport;
extern crate libp2p_websocket as websocket;
extern crate slab;
extern crate bytes;
extern crate varint;

#[macro_use]
extern crate log;

mod service;

pub use service::NetworkService;
