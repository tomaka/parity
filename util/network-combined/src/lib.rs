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

extern crate ethcore_io as io;
extern crate ethcore_network as network;
extern crate ethcore_network_devp2p as devp2p;
extern crate ethcore_network_libp2p as libp2p;
extern crate ethkey;

mod service;

pub use devp2p::validate_node_url as validate_devp2p_node_url;
pub use libp2p::validate_node_url as validate_libp2p_node_url;
pub use service::NetworkService;
