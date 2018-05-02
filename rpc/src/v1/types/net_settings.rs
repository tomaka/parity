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
// along with Parity. If not, see <http://www.gnu.org/licenses/>.

//! Network Settings data.

/// Values of network settings.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetSettings {
	/// The port devp2p is listening on, or `None` if it is disabled.
	pub devp2p_port: Option<u16>,
	/// The port libp2p is listening on, or `None` if it is disabled.
	pub libp2p_port: Option<u16>,
}
