// Copyright (C) 2019-2021 Aleo Systems Inc.
// This file is part of the snarkOS library.

// The snarkOS library is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// The snarkOS library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with the snarkOS library. If not, see <https://www.gnu.org/licenses/>.

use crate::{message::*, ConnReader, ConnWriter, NetworkError, Node, Version};
use snarkvm_objects::Storage;

use std::{
    net::SocketAddr,
    sync::{atomic::Ordering, Arc},
    time::Duration,
};

use parking_lot::Mutex;
use rand::seq::IteratorRandom;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    task,
};

impl<S: Storage> Node<S> {
    /// Obtain a list of addresses of currently connected peers.
    pub(crate) fn connected_addrs(&self) -> Vec<SocketAddr> {
        self.peer_book.read().connected_peers().keys().copied().collect()
    }
}

impl<S: Storage + Send + Sync + 'static> Node<S> {
    ///
    /// Broadcasts updates with connected peers and maintains a permitted number of connected peers.
    ///
    pub(crate) async fn update_peers(&self) -> Result<(), NetworkError> {
        // Fetch the number of connected peers.
        let number_of_connected_peers = self.peer_book.read().number_of_connected_peers() as usize;
        trace!(
            "network peers peers update_peers():  Connected to {} peer{}",
            number_of_connected_peers,
            if number_of_connected_peers == 1 { "" } else { "s" }
        );

        // Drop peers whose RTT is too high or have too many failures.
        for (addr, peer_quality) in self
            .peer_book
            .read()
            .connected_peers()
            .iter()
            .map(|(addr, info)| (*addr, Arc::clone(&info.quality)))
        {
            if peer_quality.rtt_ms.load(Ordering::Relaxed) > 1000 || peer_quality.failures.load(Ordering::Relaxed) > 10
            {
                warn!("network peers peers update_peers():  Peer {} has a low quality score; disconnecting.", addr);
                let _ = self.disconnect_from_peer(addr);
            }
        }

        // Check that this node is not a bootnode.
        if !self.config.is_bootnode() {
            // Check if this node server is below the permitted number of connected peers.
            let min_peers = self.config.minimum_number_of_connected_peers() as usize;
            if number_of_connected_peers < min_peers {
                // Attempt to connect to the default bootnodes of the network.
                self.connect_to_bootnodes().await;

                // Attempt to connect to each disconnected peer saved in the peer book.
                self.connect_to_disconnected_peers(min_peers - number_of_connected_peers)
                    .await;

                // Broadcast a `GetPeers` message to request for more peers.
                self.broadcast_getpeers_requests().await;
            }
        }

        // Check if this node server is above the permitted number of connected peers.
        let max_peers = self.config.maximum_number_of_connected_peers() as usize;
        if number_of_connected_peers > max_peers {
            let number_to_disconnect = number_of_connected_peers - max_peers;
            trace!(
                "network peers peers update_peers():  Disconnecting from the most recent {} peers to maintain their permitted number",
                number_to_disconnect
            );

            let mut connected = self
                .peer_book
                .read()
                .connected_peers()
                .iter()
                .map(|(addr, info)| (*addr, info.last_connected()))
                .collect::<Vec<_>>();
            connected.sort_unstable_by_key(|(_, info)| *info);

            for _ in 0..number_to_disconnect {
                if let Some((addr, _)) = connected.pop() {
                    let _ = self.disconnect_from_peer(addr);
                }
            }
        }

        // disconnect from peers after a while, even if they haven't sent a GetPeers
        let now = chrono::Utc::now();

        #[allow(clippy::needless_collect)] // clippy reports a false positive here
        if self.config.is_bootnode() {
            let peers = self
                .peer_book
                .read()
                .connected_peers()
                .iter()
                .map(|(addr, info)| (*addr, info.last_connected().unwrap()))
                .collect::<Vec<_>>();

            for (peer_addr, last_connected) in peers.into_iter() {
                if (now - last_connected).num_seconds() > 10 {
                    let _ = self.disconnect_from_peer(peer_addr);
                }
            }
        }

        if number_of_connected_peers != 0 {
            if !self.config.is_bootnode() {
                // Send a `Ping` to every connected peer.
                self.broadcast_pings().await;
            }

            // Store the peer book to storage.
            self.save_peer_book_to_storage()?;
        }

        Ok(())
    }

    async fn initiate_connection(&self, remote_address: SocketAddr) -> Result<(), NetworkError> {
        let own_address = self.local_address().unwrap(); // must be known by now
        if !self.can_connect() {
            // Don't connect if max number of connections has been reached.
            return Err(NetworkError::TooManyConnections);
        }
        if remote_address == own_address
            || ((remote_address.ip().is_unspecified() || remote_address.ip().is_loopback())
                && remote_address.port() == own_address.port())
        {
            return Err(NetworkError::SelfConnectAttempt);
        }
        if self.peer_book.read().is_connecting(remote_address) {
            return Err(NetworkError::PeerAlreadyConnecting);
        }
        if self.peer_book.read().is_connected(remote_address) {
            return Err(NetworkError::PeerAlreadyConnected);
        }

        self.peer_book.write().set_connecting(remote_address)?;

        // spawn a task that will be subject to a deadline
        let node = self.clone();
        let handshake_task = task::spawn(async move {
            // open the connection
            let stream = TcpStream::connect(remote_address).await?;
            let (mut reader, mut writer) = stream.into_split();

            let builder = snow::Builder::with_resolver(
                crate::HANDSHAKE_PATTERN
                    .parse()
                    .expect("Invalid noise handshake pattern!"),
                Box::new(snow::resolvers::SodiumResolver),
            );
            let static_key = builder.generate_keypair()?.private;
            let noise_builder = builder.local_private_key(&static_key).psk(3, crate::HANDSHAKE_PSK);
            let mut noise = noise_builder.build_initiator()?;
            let mut buffer: Box<[u8]> = vec![0u8; crate::MAX_MESSAGE_SIZE].into();
            let mut buf = [0u8; crate::NOISE_BUF_LEN]; // a temporary intermediate buffer to decrypt from

            // -> e
            let len = noise.write_message(&[], &mut buffer)?;
            writer.write_all(&[len as u8]).await?;
            writer.write_all(&buffer[..len]).await?;
            trace!("network peers peers initiate_connection():  sent e (XX handshake part 1/3) to {}", remote_address);

            // <- e, ee, s, es
            reader.read_exact(&mut buf[..1]).await?;
            let len = buf[0] as usize;
            if len == 0 {
                return Err(NetworkError::InvalidHandshake);
            }
            let len = reader.read_exact(&mut buf[..len]).await?;
            let len = noise.read_message(&buf[..len], &mut buffer)?;
            let _peer_version = Version::deserialize(&buffer[..len])?;
            trace!("network peers peers initiate_connection():  received e, ee, s, es (XX handshake part 2/3) from {}", remote_address);

            // -> s, se, psk
            let own_version = Version::serialize(&Version::new(1u64, own_address.port())).unwrap();
            let len = noise.write_message(&own_version, &mut buffer)?;
            writer.write_all(&[len as u8]).await?;
            writer.write_all(&buffer[..len]).await?;
            trace!("network peers peers initiate_connection():  sent s, se, psk (XX handshake part 3/3) to {}", remote_address);

            let noise = Arc::new(Mutex::new(noise.into_transport_mode()?));
            let writer = ConnWriter::new(remote_address, writer, buffer.clone(), Arc::clone(&noise));
            let mut reader = ConnReader::new(remote_address, reader, buffer, noise);

            // save the outbound channel
            node.outbound.channels.write().insert(remote_address, Arc::new(writer));

            node.peer_book.write().set_connected(remote_address, None)?;

            // spawn the inbound loop
            let node_clone = node.clone();
            let conn_listening_task = tokio::spawn(async move {
                node_clone.listen_for_messages(&mut reader).await;
            });

            if let Ok(ref peer) = node.peer_book.read().get_peer(remote_address) {
                peer.register_task(conn_listening_task);
            } else {
                // if the related peer is not found, it means it's already been dropped
                conn_listening_task.abort();
            }

            Ok(())
        });

        // check if the handshake doesn't time out
        match tokio::time::timeout(
            Duration::from_secs(crate::HANDSHAKE_TIME_LIMIT_SECS as u64),
            handshake_task,
        )
        .await
        {
            // the Result layers are: <timeout result>(<task join result>(<block result>)); JoinHandleError is returned
            // as NetworkError::InvalidHandshake, since there's not much to salvage there
            Ok(Ok(Ok(_))) => {}
            Ok(Ok(e)) => {
                return e;
            }
            Ok(Err(_)) => {
                return Err(NetworkError::InvalidHandshake);
            }
            Err(_) => {
                return Err(NetworkError::HandshakeTimeout);
            }
        }

        trace!("network peers peers initiate_connection():  Connected to {}", remote_address);

        Ok(())
    }

    ///
    /// Broadcasts a connection request to all default bootnodes of the network.
    ///
    /// This function attempts to reconnect this node server with any bootnode peer
    /// that this node may have failed to connect to.
    ///
    /// This function filters out any bootnode peers the node server is already connected to.
    ///
    async fn connect_to_bootnodes(&self) {
        trace!("network peers peers connect_to_bootnodes():  Connecting to default bootnodes");

        // Fetch the current connected peers of this node.
        let connected_peers = self.connected_addrs();

        // Iterate through each bootnode address and attempt a connection request.
        for bootnode_address in self
            .config
            .bootnodes()
            .iter()
            .filter(|addr| !connected_peers.contains(addr))
            .copied()
        {
            let node = self.clone();
            task::spawn(async move {
                if let Err(e) = node.initiate_connection(bootnode_address).await {
                    warn!("network peers peers connect_to_bootnodes():  Couldn't connect to bootnode {}: {}", bootnode_address, e);
                    let _ = node.disconnect_from_peer(bootnode_address);
                }
            });
        }
    }

    /// Broadcasts a connection request to all disconnected peers.
    async fn connect_to_disconnected_peers(&self, count: usize) {
        trace!("network peers peers connect_to_disconnected_peers():  Connecting to disconnected peers");

        // Iterate through a selection of random peers and attempt to connect.
        let random_peers = self
            .peer_book
            .read()
            .disconnected_peers()
            .iter()
            .map(|(k, _)| k)
            .copied()
            .choose_multiple(&mut rand::thread_rng(), count);

        for remote_address in random_peers {
            let node = self.clone();
            task::spawn(async move {
                if let Err(e) = node.initiate_connection(remote_address).await {
                    trace!("network peers peers connect_to_disconnected_peers():  Couldn't connect to the disconnected peer {}: {}", remote_address, e);
                    let _ = node.disconnect_from_peer(remote_address);
                }
            });
        }
    }

    /// Broadcasts a `Ping` message to all connected peers.
    async fn broadcast_pings(&self) {
        trace!("network peers peers broadcast_pings():  Broadcasting Ping messages");

        // consider peering tests that don't use the consensus layer
        let current_block_height = if let Some(ref consensus) = self.consensus() {
            consensus.current_block_height()
        } else {
            0
        };

        for remote_address in self.connected_addrs() {
            self.peer_book.read().sending_ping(remote_address);

            self.outbound
                .send_request(Message::new(
                    Direction::Outbound(remote_address),
                    Payload::Ping(current_block_height),
                ))
                .await;
        }
    }

    /// Broadcasts a `GetPeers` message to all connected peers to request for more peers.
    async fn broadcast_getpeers_requests(&self) {
        trace!("network peers peers broadcast_getpeers_requests():  Sending GetPeers requests to connected peers");

        for remote_address in self.connected_addrs() {
            self.outbound
                .send_request(Message::new(Direction::Outbound(remote_address), Payload::GetPeers))
                .await;

            // // Fetch the connection channel.
            // if let Some(channel) = self.get_channel(&remote_address) {
            //     // Broadcast the message over the channel.
            //     if let Err(_) = channel.write(&GetPeers).await {
            //         // Disconnect from the peer if the message fails to send.
            //         self.disconnect_from_peer(&remote_address).await?;
            //     }
            // } else {
            //     // Disconnect from the peer if the channel is not active.
            //     self.disconnect_from_peer(&remote_address).await?;
            // }
        }
    }

    /// TODO (howardwu): Implement manual serializers and deserializers to prevent forward breakage
    ///  when the PeerBook or PeerInfo struct fields change.
    ///
    /// Stores the current peer book to the given storage object.
    ///
    /// This function checks that this node is not connected to itself,
    /// and proceeds to serialize the peer book into a byte vector for storage.
    ///
    #[inline]
    fn save_peer_book_to_storage(&self) -> Result<(), NetworkError> {
        // Serialize the peer book.
        let serialized_peer_book = bincode::serialize(&*self.peer_book.read())?;

        // TODO: the peer book should be stored outside of consensus
        if let Some(ref consensus) = self.consensus() {
            // Save the serialized peer book to storage.
            consensus.storage().save_peer_book_to_storage(serialized_peer_book)?;
        }

        Ok(())
    }

    /// TODO (howardwu): Add logic to remove the active channels
    ///  and handshakes of the peer from this struct.
    /// Sets the given remote address in the peer book as disconnected from this node server.
    ///
    #[inline]
    pub(crate) fn disconnect_from_peer(&self, remote_address: SocketAddr) -> Result<(), NetworkError> {
        debug!("network peers peers diconnect_from_peer():  Disconnecting from {}", remote_address);

        if let Some(ref consensus) = self.consensus() {
            if self.peer_book.read().is_syncing_blocks(remote_address) {
                consensus.finished_syncing_blocks();
            }
        }

        self.outbound.channels.write().remove(&remote_address);

        self.peer_book.write().set_disconnected(remote_address)
        // TODO (howardwu): Attempt to blindly send disconnect message to peer.
    }

    pub(crate) async fn send_peers(&self, remote_address: SocketAddr) {
        // TODO (howardwu): Simplify this and parallelize this with Rayon.
        // Broadcast the sanitized list of connected peers back to requesting peer.
        let peers = if !self.config.is_bootnode() {
            self.peer_book
                .read()
                .connected_peers()
                .iter()
                .map(|(k, _)| k)
                .filter(|&addr| *addr != remote_address)
                .copied()
                .choose_multiple(&mut rand::thread_rng(), crate::SHARED_PEER_COUNT)
        } else {
            self.peer_book
                .read()
                .disconnected_peers()
                .iter()
                .map(|(k, _)| k)
                .filter(|&addr| *addr != remote_address)
                .copied()
                .choose_multiple(&mut rand::thread_rng(), crate::SHARED_PEER_COUNT)
        };

        self.outbound
            .send_request(Message::new(Direction::Outbound(remote_address), Payload::Peers(peers)))
            .await;

        // the bootstrapper's job is finished once it's sent its peer a list of peers
        if self.config.is_bootnode() {
            let _ = self.disconnect_from_peer(remote_address);
        }
    }

    /// A miner has sent their list of peer addresses.
    /// Add all new/updated addresses to our disconnected.
    /// The connection handler will be responsible for sending out handshake requests to them.
    pub(crate) fn process_inbound_peers(&self, peers: Vec<SocketAddr>) {
        // TODO (howardwu): Simplify this and parallelize this with Rayon.
        // Process all of the peers sent in the message,
        // by informing the peer book of that we found peers.
        let local_address = self.local_address().unwrap(); // the address must be known by now

        let number_of_connected_peers = self.peer_book.read().number_of_connected_peers();
        let number_to_connect = self
            .config
            .maximum_number_of_connected_peers()
            .saturating_sub(number_of_connected_peers);

        for peer_address in peers
            .iter()
            .take(number_to_connect as usize)
            .filter(|&peer_addr| *peer_addr != local_address)
            .copied()
        {
            // Inform the peer book that we found a peer.
            // The peer book will determine if we have seen the peer before,
            // and include the peer if it is new.
            self.peer_book.write().add_peer(peer_address);
        }
    }

    fn can_connect(&self) -> bool {
        let peer_book = self.peer_book.read();
        let num_connected = peer_book.number_of_connected_peers() as usize;
        let num_connecting = peer_book.number_of_connecting_peers() as usize;
        drop(peer_book);

        let max_peers = self.config.maximum_number_of_connected_peers() as usize;

        if num_connected >= max_peers || num_connected + num_connecting >= max_peers {
            warn!("network peers peers can_connect():  Max number of connections ({}) reached", max_peers);
            false
        } else {
            true
        }
    }
}
